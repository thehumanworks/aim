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
        let result = self.integrate_detached(&source, root, &work.location, &job_id, &queued, &contribution).await;
        source.shutdown().await;
        result
    }

    /// Merges and checks in a fresh detached worktree of the pinned target head, never in a
    /// checkout someone works in (review finding N3), then advances the target branch only by a
    /// fast-forward of a clean checkout still at the pinned head, or a compare-and-swap ref update
    /// when the branch is not checked out. The detached worktree is removed either way.
    async fn integrate_detached(
        &self,
        source: &GitHarness,
        root: &str,
        location: &aim_proto::daemon::Location,
        job_id: &str,
        queued: &IntegrationRecord,
        contribution: &Contribution,
    ) -> Result<IntegrationRecord, String> {
        let target_ref = format!("refs/heads/{}", queued.target_branch);
        let pinned = valid_git(
            source
                .argv(vec!["git".into(), "rev-parse".into(), "--verify".into(), target_ref.clone()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?,
            "pin target head",
        )?;
        let scratch = format!("{}-board-integration-{}", root.trim_end_matches('/'), Uuid::new_v4());
        valid_git(
            source
                .argv(
                    vec!["git".into(), "worktree".into(), "add".into(), "--detach".into(), scratch.clone(), pinned.clone()],
                    GIT_TIMEOUT_MS,
                )
                .await
                .map_err(|err| err.to_string())?,
            "create detached integration worktree",
        )?;
        let merged = match GitHarness::connect(&self.aimx, &scratch, location).await {
            Ok(target) => {
                let merged = self.integrate_on_host(&target, source, location, job_id, queued, contribution, &pinned).await;
                target.shutdown().await;
                merged
            }
            Err(err) => Err(err.to_string()),
        };
        let removed =
            source.argv(vec!["git".into(), "worktree".into(), "remove".into(), "--force".into(), scratch.clone()], GIT_TIMEOUT_MS).await;
        if !removed.is_ok_and(|out| out.success()) {
            tracing::warn!(path = %scratch, "could not remove the detached integration worktree");
        }
        merged
    }

    /// Moves the target branch from `pinned` to `result`, touching a checkout only when it is
    /// clean and still at `pinned`. Returns why it could not.
    async fn advance_target(
        &self,
        source: &GitHarness,
        target_branch: &str,
        pinned: &str,
        result: &str,
        location: &aim_proto::daemon::Location,
    ) -> Result<(), String> {
        let target_ref = format!("refs/heads/{target_branch}");
        let listed = valid_git(
            source
                .argv(vec!["git".into(), "worktree".into(), "list".into(), "--porcelain".into()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?,
            "list worktrees",
        )?;
        let mut path = None;
        let mut checked_out = None;
        for line in listed.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(value.to_owned());
            } else if line == format!("branch {target_ref}") {
                checked_out.clone_from(&path);
            }
        }
        let Some(checkout) = checked_out else {
            // Not checked out anywhere: a compare-and-swap ref update.
            let moved = source
                .argv(vec!["git".into(), "update-ref".into(), target_ref, result.to_owned(), pinned.to_owned()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?;
            return if moved.success() { Ok(()) } else { Err("the target branch moved during integration".into()) };
        };
        let target = GitHarness::connect(&self.aimx, &checkout, location).await.map_err(|err| err.to_string())?;
        let advanced = async {
            let status = valid_git(
                target
                    .argv(vec!["git".into(), "status".into(), "--porcelain".into()], GIT_TIMEOUT_MS)
                    .await
                    .map_err(|err| err.to_string())?,
                "read target status",
            )?;
            let head = valid_git(
                target.argv(vec!["git".into(), "rev-parse".into(), "HEAD".into()], GIT_TIMEOUT_MS).await.map_err(|err| err.to_string())?,
                "read target head",
            )?;
            if !status.is_empty() || head != pinned {
                return Err(format!(
                    "the target checkout {checkout} is dirty or moved; the merged commit {result} is left for you to fast-forward"
                ));
            }
            let forwarded = target
                .argv(vec!["git".into(), "merge".into(), "--ff-only".into(), result.to_owned()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?;
            if forwarded.success() {
                Ok(())
            } else {
                Err(format!("could not fast-forward {checkout}; the merged commit {result} is left for you"))
            }
        }
        .await;
        target.shutdown().await;
        advanced
    }

    #[expect(clippy::too_many_lines, reason = "one serialized merge and check transaction with explicit outcome evidence")]
    #[expect(clippy::too_many_arguments, reason = "one integration's inputs, kept explicit")]
    async fn integrate_on_host(
        &self,
        harness: &GitHarness,
        repo: &GitHarness,
        location: &aim_proto::daemon::Location,
        job_id: &str,
        queued: &IntegrationRecord,
        contribution: &Contribution,
        pinned: &str,
    ) -> Result<IntegrationRecord, String> {
        let status = valid_git(
            harness.argv(vec!["git".into(), "status".into(), "--porcelain".into()], GIT_TIMEOUT_MS).await.map_err(|err| err.to_string())?,
            "read target status",
        )?;
        if !status.is_empty() {
            return Err("target checkout is dirty or has an unfinished merge".into());
        }
        let target = pinned.to_owned();
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
                        let commit = valid_git(
                            harness
                                .argv(vec!["git".into(), "rev-parse".into(), "HEAD".into()], GIT_TIMEOUT_MS)
                                .await
                                .map_err(|err| err.to_string())?,
                            "read merge commit",
                        )?;
                        // Only now does anything outside the detached worktree change.
                        if let Err(why) = self.advance_target(repo, &queued.target_branch, pinned, &commit, location).await {
                            state = IntegrationState::Failed;
                            check_log = Some(format!("{}\nnot integrated: {why}", check_log.unwrap_or_default()));
                        }
                        result_commit = Some(commit);
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
