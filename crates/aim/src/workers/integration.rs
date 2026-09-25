//! Serialized Git integration with pinned heads and immutable board evidence.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use aim_proto::board::{JobState, ReviewState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::board::Board;
use crate::board::integration::{IntegrationRecord, IntegrationState};

use super::harness::GitHarness;
use super::receipt::owned_private_dir;
use super::runner::{GIT_TIMEOUT_MS, safe_log, valid_git};

#[derive(Deserialize)]
struct Contribution {
    branch: String,
    commit: String,
}

#[derive(Serialize)]
struct ResultEvidence<'a> {
    job_id: &'a str,
    attempt_id: &'a str,
    target_head: &'a str,
    source_commit: &'a str,
    result_commit: Option<&'a str>,
    state: IntegrationState,
    conflict_files: &'a [String],
    check_log: Option<&'a str>,
}

/// Holds the repository-wide integration lock while merging accepted attempts.
pub struct Integrator {
    board: Board,
    aimx: PathBuf,
    _lock: File,
}

impl Integrator {
    /// Opens the private integration lock; only one integrator may mutate a target checkout.
    ///
    /// # Errors
    /// Returns unsafe home or a currently held integration lock.
    pub fn new(board: Board, home: &Path, aimx: PathBuf) -> Result<Self, String> {
        owned_private_dir(home)?;
        let run = home.join("run");
        owned_private_dir(&run)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(run.join("board-integration.lock"))
            .map_err(|err| err.to_string())?;
        lock.try_lock().map_err(|err| format!("another board integration owns the lock: {err}"))?;
        Ok(Self { board, aimx, _lock: lock })
    }

    /// Integrates one accepted job into its clean configured target checkout.
    ///
    /// # Errors
    /// Refuses unaccepted, dirty, wrong-branch, stale, or missing contribution evidence.
    pub async fn integrate(&self, job_id: String) -> Result<IntegrationRecord, String> {
        let job = self.board.show(job_id.clone()).await.map_err(|err| err.to_string())?;
        if job.state != JobState::Succeeded || job.review != ReviewState::Accepted {
            return Err("job is not accepted".into());
        }
        let work = job.spec.work.as_ref().ok_or("job has no integration policy")?;
        let root = job.spec.workspace.as_deref().ok_or("job has no workspace")?;
        let queued = self.board.integration(job_id.clone()).await.map_err(|err| err.to_string())?;
        if queued.state != IntegrationState::Queued {
            return Err("integration is not queued".into());
        }
        let artifact = job
            .artifacts
            .iter()
            .find(|artifact| artifact.media_type == "application/vnd.aim.board-contribution+json")
            .ok_or("accepted attempt has no contribution artifact")?;
        let contribution: Contribution =
            serde_json::from_slice(&self.board.artifact_bytes(artifact.id.clone()).await.map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())?;
        if contribution.branch != queued.source_branch || contribution.commit.len() != 40 {
            return Err("contribution packet disagrees with the queued branch".into());
        }
        let source = GitHarness::connect(&self.aimx, root, &work.location).await.map_err(|err| err.to_string())?;
        let target_path = self.target_worktree(&source, root, &queued.target_branch).await;
        let result = match target_path {
            Ok(path) if path == root => self.integrate_on_host(&source, &job_id, &queued, &contribution).await,
            Ok(path) => {
                let target = GitHarness::connect(&self.aimx, &path, &work.location).await.map_err(|err| err.to_string())?;
                let result = self.integrate_on_host(&target, &job_id, &queued, &contribution).await;
                target.shutdown().await;
                result
            }
            Err(err) => Err(err),
        };
        source.shutdown().await;
        result
    }

    async fn target_worktree(&self, source: &GitHarness, root: &str, target_branch: &str) -> Result<String, String> {
        let listed = valid_git(
            source
                .argv(vec!["git".into(), "worktree".into(), "list".into(), "--porcelain".into()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?,
            "list target worktrees",
        )?;
        let mut path = None;
        for line in listed.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(value.to_owned());
            } else if line == format!("branch refs/heads/{target_branch}")
                && let Some(path) = path
            {
                return Ok(path);
            }
        }
        let dedicated = format!("{}-board-integration-{}", root.trim_end_matches('/'), Uuid::new_v4());
        valid_git(
            source
                .argv(vec!["git".into(), "worktree".into(), "add".into(), dedicated.clone(), target_branch.to_owned()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?,
            "create dedicated integration worktree",
        )?;
        Ok(dedicated)
    }

    #[expect(clippy::too_many_lines, reason = "one serialized merge and check transaction with explicit outcome evidence")]
    async fn integrate_on_host(
        &self,
        harness: &GitHarness,
        job_id: &str,
        queued: &IntegrationRecord,
        contribution: &Contribution,
    ) -> Result<IntegrationRecord, String> {
        let branch = valid_git(
            harness
                .argv(vec!["git".into(), "branch".into(), "--show-current".into()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?,
            "read target branch",
        )?;
        if branch != queued.target_branch {
            return Err("target checkout is on a different branch".into());
        }
        let status = valid_git(
            harness.argv(vec!["git".into(), "status".into(), "--porcelain".into()], GIT_TIMEOUT_MS).await.map_err(|err| err.to_string())?,
            "read target status",
        )?;
        if !status.is_empty() {
            return Err("target checkout is dirty or has an unfinished merge".into());
        }
        let target = valid_git(
            harness.argv(vec!["git".into(), "rev-parse".into(), "HEAD".into()], GIT_TIMEOUT_MS).await.map_err(|err| err.to_string())?,
            "pin target head",
        )?;
        let source = valid_git(
            harness
                .argv(
                    vec!["git".into(), "rev-parse".into(), "--verify".into(), format!("refs/heads/{}", queued.source_branch)],
                    GIT_TIMEOUT_MS,
                )
                .await
                .map_err(|err| err.to_string())?,
            "pin attempt head",
        )?;
        if source != contribution.commit {
            return Err("attempt branch moved since the accepted evidence".into());
        }
        let intent =
            self.board.begin_integration(job_id.to_owned(), target.clone(), source.clone()).await.map_err(|err| err.to_string())?;
        let merged = harness
            .argv(
                vec![
                    "git".into(),
                    "merge".into(),
                    "--no-commit".into(),
                    "--no-ff".into(),
                    "--no-edit".into(),
                    queued.source_branch.clone(),
                ],
                GIT_TIMEOUT_MS,
            )
            .await;
        let mut state = IntegrationState::Failed;
        let mut conflicts = Vec::new();
        let mut check_log = None;
        let mut result_commit = None;
        if let Ok(merged) = merged {
            if merged.success() && !merged.truncated {
                let job = self.board.show(job_id.to_owned()).await.map_err(|err| err.to_string())?;
                if let Some(check) = job.spec.work.as_ref().and_then(|work| work.check_command.as_ref()) {
                    let checked = harness.shell(check.clone(), 300_000).await.map_err(|err| err.to_string())?;
                    let passed = checked.success() && !checked.truncated;
                    check_log = Some(format!("command: {}\npassed: {passed}\n{}", safe_log(check), safe_log(&checked.text)));
                    if passed {
                        state = IntegrationState::Integrated;
                    }
                } else {
                    state = IntegrationState::Integrated;
                }
                if state == IntegrationState::Integrated {
                    let committed = harness
                        .argv(vec!["git".into(), "commit".into(), "--no-edit".into()], GIT_TIMEOUT_MS)
                        .await
                        .map_err(|err| err.to_string())?;
                    if committed.success() {
                        result_commit = Some(valid_git(
                            harness
                                .argv(vec!["git".into(), "rev-parse".into(), "HEAD".into()], GIT_TIMEOUT_MS)
                                .await
                                .map_err(|err| err.to_string())?,
                            "read merge commit",
                        )?);
                    } else {
                        state = IntegrationState::Failed;
                    }
                }
            } else {
                let files = harness
                    .argv(vec!["git".into(), "diff".into(), "--name-only".into(), "--diff-filter=U".into()], GIT_TIMEOUT_MS)
                    .await
                    .map_err(|err| err.to_string())?;
                if files.success() {
                    conflicts = files.text.lines().take(256).map(str::to_owned).collect();
                }
                if !conflicts.is_empty() {
                    state = IntegrationState::Conflict;
                }
            }
        }
        let evidence = serde_json::to_vec(&ResultEvidence {
            job_id,
            attempt_id: &intent.attempt_id,
            target_head: &target,
            source_commit: &source,
            result_commit: result_commit.as_deref(),
            state,
            conflict_files: &conflicts,
            check_log: check_log.as_deref(),
        })
        .map_err(|err| err.to_string())?;
        self.board.finish_integration(job_id.to_owned(), state, conflicts, evidence).await.map_err(|err| err.to_string())
    }

    /// Processes queued integrations sequentially, stopping after a conflict or failed check.
    ///
    /// # Errors
    /// Returns the first refused or failed integration.
    pub async fn integrate_queued(&self) -> Result<Vec<IntegrationRecord>, String> {
        let mut done = Vec::new();
        for job_id in self.board.queued_integrations().await.map_err(|err| err.to_string())? {
            let record = self.integrate(job_id).await?;
            let stop = record.state != IntegrationState::Integrated;
            done.push(record);
            if stop {
                break;
            }
        }
        Ok(done)
    }
}
