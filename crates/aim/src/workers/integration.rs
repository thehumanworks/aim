//! Serialized Git integration with pinned heads and recoverable board evidence.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aim_kernel::integration::{Decision, Failure, GitObservation, Intent, Phase, decide_cleanup, decide_next, decide_reconcile};
use aim_proto::board::{JobState, ReviewState};
use aim_proto::daemon::Location;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
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

fn phase(state: IntegrationState) -> Phase {
    match state {
        IntegrationState::Queued => Phase::Queued,
        IntegrationState::Integrating => Phase::Integrating,
        IntegrationState::Integrated => Phase::Integrated,
        IntegrationState::Failed => Phase::Failed,
        IntegrationState::Conflict => Phase::Conflict,
    }
}

fn intent(record: &IntegrationRecord) -> Intent {
    Intent {
        phase: phase(record.state),
        target: record.target_head.as_deref().unwrap_or_default().as_bytes().to_vec(),
        source: record.source_commit.as_deref().unwrap_or_default().as_bytes().to_vec(),
        scratch: record.scratch_path.as_deref().unwrap_or_default().as_bytes().to_vec(),
        result: record.result_commit.as_ref().map(|oid| oid.as_bytes().to_vec()),
        rescue_expected: record.rescue_ref.is_some(),
    }
}

fn oid(text: &str) -> Result<String, String> {
    if text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(text.to_owned())
    } else {
        Err("Git returned an invalid SHA-1 commit OID".into())
    }
}

#[cfg(any(test, debug_assertions))]
fn fault(job_id: &str, point: &str) -> Result<(), String> {
    if std::env::var("AIM_BOARD_TEST_FAULT").ok().as_deref() == Some(format!("{job_id}:{point}").as_str()) {
        return Err(format!("injected board integration fault after {point}"));
    }
    Ok(())
}

#[cfg(not(any(test, debug_assertions)))]
fn fault(_job_id: &str, _point: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(any(test, debug_assertions))]
async fn pause_after_source_check() -> Result<(), String> {
    use std::time::{Duration, Instant};
    let Ok(dir) = std::env::var("AIM_BOARD_TEST_PAUSE_AFTER_SOURCE_CHECK") else { return Ok(()) };
    let dir = Path::new(&dir);
    if !dir.is_absolute() || !dir.is_dir() {
        return Err("invalid source-check pause directory".into());
    }
    std::fs::write(dir.join("ready"), b"").map_err(|err| err.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.join("release").exists() {
        if Instant::now() >= deadline {
            return Err("source-check pause timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

#[cfg(not(any(test, debug_assertions)))]
async fn pause_after_source_check() -> Result<(), String> {
    Ok(())
}

fn evidence(record: &IntegrationRecord, state: IntegrationState, conflicts: &[String], log: Option<&str>) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&ResultEvidence {
        job_id: &record.job_id,
        attempt_id: &record.attempt_id,
        target_head: record.target_head.as_deref().unwrap_or_default(),
        source_commit: record.source_commit.as_deref().unwrap_or_default(),
        result_commit: record.result_commit.as_deref(),
        state,
        conflict_files: conflicts,
        check_log: log,
    })
    .map_err(|err| err.to_string())
}

fn rescue_name(job_id: &str) -> String {
    format!("refs/aim/rescue/{job_id}")
}

/// Holds a repository-wide process lock and serializes all operations on this instance.
pub struct Integrator {
    board: Board,
    aimx: PathBuf,
    operation: Arc<Mutex<()>>,
    lock: Arc<File>,
}

/// Schedules conservative owned-scratch recovery if an integration future is cancelled or unwinds.
struct ScratchGuard {
    board: Board,
    aimx: PathBuf,
    operation: Arc<Mutex<()>>,
    lock: Arc<File>,
    job_id: String,
    armed: bool,
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(any(test, debug_assertions))]
        if std::env::var("AIM_BOARD_TEST_FAULT").is_ok_and(|fault| fault.starts_with(&format!("{}:", self.job_id))) {
            // The fault hook models a process crash, where Drop never runs.
            return;
        }
        let (board, aimx, job_id, operation, lock) =
            (self.board.clone(), self.aimx.clone(), self.job_id.clone(), Arc::clone(&self.operation), Arc::clone(&self.lock));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _lock = lock;
                let _operation = operation.lock().await;
                if let Err(err) = Integrator::prune_one(&board, &aimx, &job_id).await {
                    tracing::warn!(job_id, %err, "deferred integration scratch cleanup failed");
                }
            });
        }
    }
}

impl Integrator {
    /// Opens the private integration lock; only one process may integrate through this home.
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
        Ok(Self { board, aimx, operation: Arc::new(Mutex::new(())), lock: Arc::new(lock) })
    }

    async fn command(harness: &GitHarness, args: Vec<String>, label: &str) -> Result<String, String> {
        valid_git(harness.argv(args, GIT_TIMEOUT_MS).await.map_err(|err| err.to_string())?, label)
    }

    async fn optional_ref(harness: &GitHarness, reference: &str) -> Result<Option<String>, String> {
        let output = harness
            .argv(vec!["git".into(), "rev-parse".into(), "--verify".into(), reference.into()], GIT_TIMEOUT_MS)
            .await
            .map_err(|err| err.to_string())?;
        if output.truncated {
            return Err("Git ref output was truncated".into());
        }
        if output.success() { Ok(Some(oid(output.text.trim())?)) } else { Ok(None) }
    }

    async fn committed_scratch_result(worktree: &GitHarness, record: &IntegrationRecord) -> Result<Option<String>, String> {
        let target = record.target_head.as_deref().ok_or("missing pinned target")?;
        let source = record.source_commit.as_deref().ok_or("missing pinned source")?;
        let head = Self::command(worktree, vec!["git".into(), "rev-parse".into(), "HEAD".into()], "read scratch head").await?;
        if head == target {
            return Ok(None);
        }
        let parents = Self::command(
            worktree,
            vec!["git".into(), "rev-list".into(), "--parents".into(), "-n".into(), "1".into(), head.clone()],
            "read merge parents",
        )
        .await?;
        let words: Vec<_> = parents.split_whitespace().collect();
        if words.len() != 3 || words.first() != Some(&head.as_str()) || words.get(1) != Some(&target) || words.get(2) != Some(&source) {
            return Err("owned scratch head changed unexpectedly".into());
        }
        let message =
            Self::command(worktree, vec!["git".into(), "log".into(), "-1".into(), "--format=%B".into()], "read merge marker").await?;
        if message.lines().next() != Some(format!("aim board integration {}", record.job_id).as_str()) {
            return Err("owned scratch merge marker is missing".into());
        }
        Ok(Some(head))
    }

    async fn worktrees(harness: &GitHarness) -> Result<Vec<(String, Option<String>)>, String> {
        let text =
            Self::command(harness, vec!["git".into(), "worktree".into(), "list".into(), "--porcelain".into()], "list worktrees").await?;
        let mut rows = Vec::new();
        let mut path = None;
        let mut branch = None;
        for line in text.lines().chain(std::iter::once("")) {
            if line.is_empty() {
                if let Some(path) = path.take() {
                    rows.push((path, branch.take()));
                }
            } else if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_owned());
            }
        }
        Ok(rows)
    }

    async fn observe(
        harness: &GitHarness,
        _root: &str,
        record: &IntegrationRecord,
        failure: Failure,
        checkout_attempt_failed: bool,
        apply_requested: bool,
    ) -> Result<GitObservation, String> {
        let scratch = record.scratch_path.as_deref();
        let worktrees = Self::worktrees(harness).await?;
        let canonical_root =
            Self::command(harness, vec!["git".into(), "rev-parse".into(), "--show-toplevel".into()], "locate repository root").await?;
        let owned = scratch.is_some_and(|path| worktrees.iter().any(|(found, _)| found == path))
            && scratch.is_some_and(|path| path.starts_with(&format!("{}-board-integration-", canonical_root.trim_end_matches('/'))));
        let target_ref = format!("refs/heads/{}", record.target_branch);
        let default_rescue = rescue_name(&record.job_id);
        let target_checkout_owned = owned
            && worktrees.iter().any(|(path, branch)| Some(path.as_str()) == scratch && branch.as_deref() == Some(target_ref.as_str()));
        Ok(GitObservation {
            target: Self::optional_ref(harness, &target_ref).await?.map(String::into_bytes),
            source: Self::optional_ref(harness, &format!("refs/heads/{}", record.source_branch)).await?.map(String::into_bytes),
            scratch: if owned { scratch.map(|path| path.as_bytes().to_vec()) } else { None },
            scratch_owned: owned,
            target_checkout_owned,
            checkout_attempt_failed,
            rescue: Self::optional_ref(harness, record.rescue_ref.as_deref().unwrap_or(&default_rescue)).await?.map(String::into_bytes),
            failure,
            apply_requested,
        })
    }

    async fn ensure_rescue(harness: &GitHarness, record: &IntegrationRecord) -> Result<(), String> {
        let result = record.result_commit.as_deref().ok_or("missing checked result")?;
        let reference = record.rescue_ref.as_deref().ok_or("missing rescue ref name")?;
        match Self::optional_ref(harness, reference).await? {
            Some(existing) if existing == result => Ok(()),
            Some(_) => Err("rescue ref names a different commit".into()),
            None => {
                let zero = "0".repeat(40);
                Self::command(harness, vec!["git".into(), "update-ref".into(), reference.into(), result.into(), zero], "create rescue ref")
                    .await?;
                Ok(())
            }
        }
    }

    async fn cleanup_scratch(
        board: &Board,
        aimx: &Path,
        harness: &GitHarness,
        location: &Location,
        record: &IntegrationRecord,
        root: &str,
    ) -> Result<(), String> {
        let Some(path) = &record.scratch_path else { return Ok(()) };
        let observed = Self::observe(harness, root, record, Failure::None, false, false).await?;
        if observed.scratch.is_none() {
            board.clear_integration_scratch(record.job_id.clone()).await.map_err(|err| err.to_string())?;
            return Ok(());
        }
        // A committed but not yet recorded merge result must survive cancellation. Recovery
        // recognizes its two parents and persists it before any cleanup.
        if record.result_commit.is_none() {
            let worktree = GitHarness::connect(aimx, path, location).await.map_err(|err| err.to_string())?;
            let head = Self::command(&worktree, vec!["git".into(), "rev-parse".into(), "HEAD".into()], "read owned scratch head").await;
            worktree.shutdown().await;
            if head? != record.target_head.as_deref().unwrap_or_default() {
                return Ok(());
            }
        }
        if !decide_cleanup(&intent(record), &observed) {
            return Ok(());
        }
        let removed = harness
            .argv(vec!["git".into(), "worktree".into(), "remove".into(), "--force".into(), path.clone()], GIT_TIMEOUT_MS)
            .await
            .map_err(|err| err.to_string())?;
        if !removed.success() || removed.truncated {
            return Err("remove owned integration worktree failed".into());
        }
        board.clear_integration_scratch(record.job_id.clone()).await.map_err(|err| err.to_string())?;
        Ok(())
    }

    async fn prune_one(board: &Board, aimx: &Path, job_id: &str) -> Result<(), String> {
        let record = board.integration(job_id.to_owned()).await.map_err(|err| err.to_string())?;
        let job = board.show(job_id.to_owned()).await.map_err(|err| err.to_string())?;
        let work = job.spec.work.as_ref().ok_or("job has no integration policy")?;
        let root = job.spec.workspace.as_deref().ok_or("job has no workspace")?;
        let harness = GitHarness::connect(aimx, root, &work.location).await.map_err(|err| err.to_string())?;
        let result = async {
            let observed = Self::observe(&harness, root, &record, Failure::None, false, false).await?;
            if decide_reconcile(&intent(&record), &observed) == Decision::EnsureRescue {
                Self::ensure_rescue(&harness, &record).await?;
            }
            Self::cleanup_scratch(board, aimx, &harness, &work.location, &record, root).await?;
            if record.state == IntegrationState::Integrated {
                Self::cleanup_rescue(board, &harness, &record).await?;
            }
            Ok(())
        }
        .await;
        harness.shutdown().await;
        result
    }

    async fn cleanup_rescue(board: &Board, harness: &GitHarness, record: &IntegrationRecord) -> Result<(), String> {
        let Some(reference) = &record.rescue_ref else { return Ok(()) };
        let Some(result) = &record.result_commit else { return Err("rescue has no result".into()) };
        match Self::optional_ref(harness, reference).await? {
            Some(found) if found == *result => {
                Self::command(
                    harness,
                    vec!["git".into(), "update-ref".into(), "-d".into(), reference.clone(), result.clone()],
                    "remove owned rescue ref",
                )
                .await?;
            }
            Some(_) => return Err("rescue ref changed; refusing to remove it".into()),
            None => {}
        }
        board.clear_integration_rescue(record.job_id.clone()).await.map_err(|err| err.to_string())?;
        Ok(())
    }

    /// Integrates one accepted job, reconciling unfinished jobs first.
    ///
    /// # Errors
    /// Refuses stale or missing evidence and reports Git or ledger failures.
    pub async fn integrate(&self, job_id: String) -> Result<IntegrationRecord, String> {
        let _operation = self.operation.lock().await;
        Box::pin(self.reconcile_all()).await?;
        let record = self.board.integration(job_id.clone()).await.map_err(|err| err.to_string())?;
        if matches!(record.state, IntegrationState::Integrated | IntegrationState::Failed | IntegrationState::Conflict) {
            return Ok(record);
        }
        Box::pin(self.integrate_locked(job_id)).await
    }

    async fn reconcile_all(&self) -> Result<(), String> {
        for record in self.board.recoverable_integrations().await.map_err(|err| err.to_string())? {
            if record.state == IntegrationState::Integrating {
                Box::pin(self.reconcile_one(&record.job_id)).await?;
            } else {
                Self::prune_one(&self.board, &self.aimx, &record.job_id).await?;
            }
        }
        Ok(())
    }

    async fn integrate_locked(&self, job_id: String) -> Result<IntegrationRecord, String> {
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
            .find(|item| item.media_type == "application/vnd.aim.board-contribution+json")
            .ok_or("accepted attempt has no contribution artifact")?;
        let contribution: Contribution =
            serde_json::from_slice(&self.board.artifact_bytes(artifact.id.clone()).await.map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())?;
        if contribution.branch != queued.source_branch {
            return Err("contribution packet disagrees with queued branch".into());
        }
        let accepted = oid(&contribution.commit)?;
        let harness = GitHarness::connect(&self.aimx, root, &work.location).await.map_err(|err| err.to_string())?;
        let start = Box::pin(async {
            let target =
                Self::optional_ref(&harness, &format!("refs/heads/{}", queued.target_branch)).await?.ok_or("target branch is missing")?;
            let source =
                Self::optional_ref(&harness, &format!("refs/heads/{}", queued.source_branch)).await?.ok_or("attempt branch is missing")?;
            if source != accepted {
                return Err("attempt branch moved since accepted evidence".into());
            }
            let canonical_root =
                Self::command(&harness, vec!["git".into(), "rev-parse".into(), "--show-toplevel".into()], "locate repository root").await?;
            let scratch = format!("{}-board-integration-{}", canonical_root.trim_end_matches('/'), Uuid::new_v4());
            let proposed = Intent {
                phase: Phase::Queued,
                target: target.as_bytes().to_vec(),
                source: source.as_bytes().to_vec(),
                scratch: scratch.as_bytes().to_vec(),
                result: None,
                rescue_expected: false,
            };
            let observation = GitObservation {
                target: Some(target.clone().into_bytes()),
                source: Some(source.clone().into_bytes()),
                scratch: None,
                scratch_owned: false,
                target_checkout_owned: false,
                checkout_attempt_failed: false,
                rescue: Self::optional_ref(&harness, &rescue_name(&job_id)).await?.map(String::into_bytes),
                failure: Failure::None,
                apply_requested: false,
            };
            if decide_reconcile(&proposed, &observation) != Decision::Begin
                || decide_next(&proposed, &observation, Decision::Begin) != Some(Phase::Integrating)
            {
                return Err("kernel refused integration intent".into());
            }
            self.board.begin_integration(job_id.clone(), target.clone(), source, scratch.clone()).await.map_err(|err| err.to_string())?;
            let mut guard = ScratchGuard {
                board: self.board.clone(),
                aimx: self.aimx.clone(),
                operation: Arc::clone(&self.operation),
                lock: Arc::clone(&self.lock),
                job_id: job_id.clone(),
                armed: true,
            };
            fault(&job_id, "after_intent")?;
            Self::command(
                &harness,
                vec!["git".into(), "worktree".into(), "add".into(), "--detach".into(), scratch, target],
                "create detached integration worktree",
            )
            .await?;
            fault(&job_id, "after_scratch")?;
            let result = Box::pin(self.reconcile_one(&job_id)).await;
            if result.is_ok() {
                guard.armed = false;
            }
            result
        })
        .await;
        harness.shutdown().await;
        start
    }

    async fn reconcile_one(&self, job_id: &str) -> Result<IntegrationRecord, String> {
        let job = self.board.show(job_id.to_owned()).await.map_err(|err| err.to_string())?;
        let work = job.spec.work.as_ref().ok_or("job has no integration policy")?;
        let root = job.spec.workspace.as_deref().ok_or("job has no workspace")?;
        let harness = GitHarness::connect(&self.aimx, root, &work.location).await.map_err(|err| err.to_string())?;
        let result = Box::pin(self.reconcile_with_harness(&harness, root, &work.location, job_id, work.check_command.as_ref())).await;
        harness.shutdown().await;
        result
    }

    #[expect(clippy::too_many_lines, reason = "kernel-directed recovery keeps the effects and terminal outcome in one ordered loop")]
    async fn reconcile_with_harness(
        &self,
        harness: &GitHarness,
        root: &str,
        location: &Location,
        job_id: &str,
        check: Option<&String>,
    ) -> Result<IntegrationRecord, String> {
        let mut failure = Failure::None;
        let mut checkout_attempt_failed = false;
        let mut conflicts = Vec::new();
        let mut check_log = None;
        for _ in 0..16 {
            let record = self.board.integration(job_id.to_owned()).await.map_err(|err| err.to_string())?;
            if record.state != IntegrationState::Integrating {
                Self::prune_one(&self.board, &self.aimx, job_id).await?;
                return self.board.integration(job_id.to_owned()).await.map_err(|err| err.to_string());
            }
            let observed = Self::observe(harness, root, &record, failure, checkout_attempt_failed, false).await?;
            if record.result_commit.is_none() && observed.scratch_owned {
                let scratch = record.scratch_path.as_deref().ok_or("missing scratch")?;
                let worktree = GitHarness::connect(&self.aimx, scratch, location).await.map_err(|err| err.to_string())?;
                let committed = Self::committed_scratch_result(&worktree, &record).await;
                worktree.shutdown().await;
                if let Some(result) = committed? {
                    self.board
                        .record_integration_result(job_id.to_owned(), result, rescue_name(job_id))
                        .await
                        .map_err(|err| err.to_string())?;
                    continue;
                }
            }
            let decision = decide_reconcile(&intent(&record), &observed);
            match decision {
                Decision::ResumeScratch => {
                    let scratch = record.scratch_path.as_deref().ok_or("missing scratch")?;
                    let worktree = GitHarness::connect(&self.aimx, scratch, location).await.map_err(|err| err.to_string())?;
                    let outcome = self.merge_in_scratch(&worktree, job_id, &record, check).await;
                    worktree.shutdown().await;
                    match outcome? {
                        MergeOutcome::Result(result) => {
                            self.board
                                .record_integration_result(job_id.to_owned(), result, rescue_name(job_id))
                                .await
                                .map_err(|err| err.to_string())?;
                            fault(job_id, "after_result")?;
                        }
                        MergeOutcome::Conflict(files) => {
                            conflicts = files;
                            failure = Failure::Conflict;
                        }
                        MergeOutcome::Failed(log) => {
                            check_log = Some(log);
                            failure = Failure::Failed;
                        }
                    }
                }
                Decision::EnsureRescue => {
                    Self::ensure_rescue(harness, &record).await?;
                    fault(job_id, "after_rescue")?;
                }
                Decision::AcquireOwnedCheckout => {
                    let scratch = record.scratch_path.as_deref().ok_or("missing scratch")?;
                    let worktree = GitHarness::connect(&self.aimx, scratch, location).await.map_err(|err| err.to_string())?;
                    let switched = worktree
                        .argv(vec!["git".into(), "switch".into(), "--no-guess".into(), record.target_branch.clone()], GIT_TIMEOUT_MS)
                        .await;
                    worktree.shutdown().await;
                    checkout_attempt_failed = !switched.is_ok_and(|output| output.success() && !output.truncated);
                }
                Decision::AdvanceOwned => {
                    let scratch = record.scratch_path.as_deref().ok_or("missing scratch")?;
                    let result = record.result_commit.as_deref().ok_or("missing result")?;
                    let worktree = GitHarness::connect(&self.aimx, scratch, location).await.map_err(|err| err.to_string())?;
                    let advanced =
                        worktree.argv(vec!["git".into(), "merge".into(), "--ff-only".into(), result.into()], GIT_TIMEOUT_MS).await;
                    worktree.shutdown().await;
                    if advanced.is_ok_and(|output| output.success() && !output.truncated) {
                        fault(job_id, "after_target")?;
                    } else {
                        Self::cleanup_scratch(&self.board, &self.aimx, harness, location, &record, root).await?;
                        checkout_attempt_failed = true;
                    }
                }
                Decision::FinalizeIntegrated | Decision::FinalizeFailed | Decision::FinalizeConflict => {
                    let target_state = match decision {
                        Decision::FinalizeIntegrated => IntegrationState::Integrated,
                        Decision::FinalizeFailed => IntegrationState::Failed,
                        _ => IntegrationState::Conflict,
                    };
                    if decide_next(&intent(&record), &observed, decision) != Some(phase(target_state)) {
                        return Err("kernel refused terminal integration".into());
                    }
                    let data = evidence(&record, target_state, &conflicts, check_log.as_deref())?;
                    self.board.finish_integration(job_id.to_owned(), target_state, conflicts, data).await.map_err(|err| err.to_string())?;
                    Self::prune_one(&self.board, &self.aimx, job_id).await?;
                    return self.board.integration(job_id.to_owned()).await.map_err(|err| err.to_string());
                }
                Decision::RetryQueued => {
                    if decide_next(&intent(&record), &observed, decision) != Some(Phase::Queued) {
                        return Err("kernel refused retry".into());
                    }
                    return self.board.requeue_integration(job_id.to_owned()).await.map_err(|err| err.to_string());
                }
                other => return Err(format!("integration reconciliation stopped at {other:?}")),
            }
        }
        Err("integration reconciliation did not converge".into())
    }

    async fn merge_in_scratch(
        &self,
        worktree: &GitHarness,
        job_id: &str,
        record: &IntegrationRecord,
        check: Option<&String>,
    ) -> Result<MergeOutcome, String> {
        let target = record.target_head.as_deref().ok_or("missing pinned target")?;
        let source = record.source_commit.as_deref().ok_or("missing pinned source")?;
        if let Some(result) = Self::committed_scratch_result(worktree, record).await? {
            return Ok(MergeOutcome::Result(result));
        }
        let unmerged = Self::command(
            worktree,
            vec!["git".into(), "diff".into(), "--name-only".into(), "--diff-filter=U".into()],
            "inspect unmerged paths",
        )
        .await?;
        if !unmerged.is_empty() {
            return Ok(MergeOutcome::Conflict(unmerged.lines().take(256).map(str::to_owned).collect()));
        }
        // Reset only this owned scratch, allowing a cancelled uncommitted merge to resume safely.
        drop(worktree.argv(vec!["git".into(), "merge".into(), "--abort".into()], GIT_TIMEOUT_MS).await);
        Self::command(worktree, vec!["git".into(), "reset".into(), "--hard".into(), target.into()], "reset owned scratch").await?;
        Self::command(worktree, vec!["git".into(), "clean".into(), "-fd".into()], "clean owned scratch").await?;
        let branch = Self::optional_ref(worktree, &format!("refs/heads/{}", record.source_branch)).await?;
        if branch.as_deref() != Some(source) {
            return Ok(MergeOutcome::Failed("accepted source branch moved".into()));
        }
        pause_after_source_check().await?;
        let merged = worktree
            .argv(
                vec!["git".into(), "merge".into(), "--no-commit".into(), "--no-ff".into(), "--no-edit".into(), source.into()],
                GIT_TIMEOUT_MS,
            )
            .await
            .map_err(|err| err.to_string())?;
        if !merged.success() || merged.truncated {
            let files = Self::command(
                worktree,
                vec!["git".into(), "diff".into(), "--name-only".into(), "--diff-filter=U".into()],
                "inspect conflicts",
            )
            .await?;
            return if files.is_empty() {
                Ok(MergeOutcome::Failed("merge failed".into()))
            } else {
                Ok(MergeOutcome::Conflict(files.lines().take(256).map(str::to_owned).collect()))
            };
        }
        if let Some(check) = check {
            let checked = worktree.shell(check.clone(), 300_000).await.map_err(|err| err.to_string())?;
            if !checked.success() || checked.truncated {
                return Ok(MergeOutcome::Failed(format!("command: {}\n{}", safe_log(check), safe_log(&checked.text))));
            }
        }
        let committed = worktree
            .argv(vec!["git".into(), "commit".into(), "-m".into(), format!("aim board integration {job_id}")], GIT_TIMEOUT_MS)
            .await
            .map_err(|err| err.to_string())?;
        if !committed.success() || committed.truncated {
            return Ok(MergeOutcome::Failed("merge commit failed".into()));
        }
        let result = oid(&Self::command(worktree, vec!["git".into(), "rev-parse".into(), "HEAD".into()], "read merge result").await?)?;
        fault(job_id, "after_commit")?;
        Ok(MergeOutcome::Result(result))
    }

    /// Processes queued integrations serially, stopping at the first non-integrated result.
    ///
    /// # Errors
    /// Returns the first reconciliation or integration error.
    pub async fn integrate_queued(&self) -> Result<Vec<IntegrationRecord>, String> {
        let _operation = self.operation.lock().await;
        Box::pin(self.reconcile_all()).await?;
        let mut done = Vec::new();
        for job_id in self.board.queued_integrations().await.map_err(|err| err.to_string())? {
            let record = Box::pin(self.integrate_locked(job_id)).await?;
            let stop = record.state != IntegrationState::Integrated;
            done.push(record);
            if stop {
                break;
            }
        }
        Ok(done)
    }

    /// Explicitly fast-forwards the configured user checkout to a retained checked result.
    ///
    /// # Errors
    /// Refuses another branch, a missing rescue, or Git's fast-forward safety check.
    pub async fn apply(&self, job_id: String) -> Result<IntegrationRecord, String> {
        let _operation = self.operation.lock().await;
        Box::pin(self.reconcile_all()).await?;
        let record = self.board.integration(job_id.clone()).await.map_err(|err| err.to_string())?;
        if record.state != IntegrationState::Failed {
            return Err("integration is not awaiting apply".into());
        }
        let result = record.result_commit.as_deref().ok_or("integration has no checked result")?;
        let rescue = record.rescue_ref.as_deref().ok_or("integration has no rescue ref")?;
        let job = self.board.show(job_id.clone()).await.map_err(|err| err.to_string())?;
        let work = job.spec.work.as_ref().ok_or("job has no integration policy")?;
        let root = job.spec.workspace.as_deref().ok_or("job has no workspace")?;
        let harness = GitHarness::connect(&self.aimx, root, &work.location).await.map_err(|err| err.to_string())?;
        let applied = async {
            let branch =
                Self::command(&harness, vec!["git".into(), "symbolic-ref".into(), "HEAD".into()], "read configured checkout branch")
                    .await?;
            if branch != format!("refs/heads/{}", record.target_branch) {
                return Err("configured checkout is not on target branch".into());
            }
            let observed = Self::observe(&harness, root, &record, Failure::None, false, true).await?;
            if observed.rescue.as_deref() != Some(result.as_bytes()) {
                return Err("saved rescue ref changed or is missing".into());
            }
            let merged = harness
                .argv(vec!["git".into(), "merge".into(), "--ff-only".into(), rescue.into()], GIT_TIMEOUT_MS)
                .await
                .map_err(|err| err.to_string())?;
            if !merged.success() || merged.truncated {
                return Err("the user checkout could not fast-forward to the saved result".into());
            }
            let after = Self::observe(&harness, root, &record, Failure::None, false, true).await?;
            if decide_reconcile(&intent(&record), &after) != Decision::MarkApplied
                || decide_next(&intent(&record), &after, Decision::MarkApplied) != Some(Phase::Integrated)
            {
                return Err("kernel refused applied result".into());
            }
            let data = evidence(&record, IntegrationState::Integrated, &[], Some("explicit user apply"))?;
            let applied =
                self.board.mark_integration_applied(job_id.clone(), result.to_owned(), data).await.map_err(|err| err.to_string())?;
            Self::cleanup_rescue(&self.board, &harness, &applied).await?;
            self.board.integration(job_id).await.map_err(|err| err.to_string())
        }
        .await;
        harness.shutdown().await;
        applied
    }

    /// Explicitly discards a retained failed result's owned rescue ref.
    ///
    /// # Errors
    /// Refuses active integrations or a rescue ref naming an unexpected commit.
    pub async fn discard_rescue(&self, job_id: String) -> Result<IntegrationRecord, String> {
        let _operation = self.operation.lock().await;
        Box::pin(self.reconcile_all()).await?;
        let record = self.board.integration(job_id.clone()).await.map_err(|err| err.to_string())?;
        if record.state != IntegrationState::Failed {
            return Err("only failed integrations can discard a rescue".into());
        }
        let job = self.board.show(job_id.clone()).await.map_err(|err| err.to_string())?;
        let work = job.spec.work.as_ref().ok_or("job has no integration policy")?;
        let root = job.spec.workspace.as_deref().ok_or("job has no workspace")?;
        let harness = GitHarness::connect(&self.aimx, root, &work.location).await.map_err(|err| err.to_string())?;
        let result = async {
            Self::cleanup_rescue(&self.board, &harness, &record).await?;
            self.board.integration(job_id).await.map_err(|err| err.to_string())
        }
        .await;
        harness.shutdown().await;
        result
    }
}

enum MergeOutcome {
    Result(String),
    Conflict(Vec<String>),
    Failed(String),
}
