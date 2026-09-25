//! Board-backed execution records for declarative workflows.
//!
//! Every manifest step is a board job. The workflow store keeps scheduling metadata; board job
//! and attempt states remain authoritative. Claim tokens live only in private, synced receipts.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use aim_proto::board::{
    ArtifactInput, ClaimParams, CleanupReceipt, CompleteParams, FailParams, HeartbeatParams, JobSnapshot, JobSpec, JobState, PostParams,
    RegisterWorkerParams, RetryParams, ReviewParams, ReviewState,
};
use aim_proto::content::Base64Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::board::Board;

use super::manifest::{StepKind, WorkflowManifest};
use super::store::{Error as StoreError, WorkflowStore};

const RESULT_MEDIA_TYPE: &str = "application/vnd.aim.workflow-result+json";
const LEASE_MS: u64 = 300_000;

/// An error while connecting workflow steps to the board ledger.
#[derive(Debug)]
pub enum Error {
    /// Workflow projection failed.
    Store(StoreError),
    /// Authoritative board operation failed.
    Board(crate::board::Error),
    /// The persisted manifest or step state is inconsistent.
    Invalid(String),
    /// Receipt storage failed.
    Receipt(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "workflow store: {err}"),
            Self::Board(err) => write!(f, "workflow board: {err}"),
            Self::Invalid(err) => write!(f, "workflow board input: {err}"),
            Self::Receipt(err) => write!(f, "workflow receipt: {err}"),
        }
    }
}

impl core::error::Error for Error {}

impl From<StoreError> for Error {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<crate::board::Error> for Error {
    fn from(err: crate::board::Error) -> Self {
        Self::Board(err)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimReceipt {
    run_id: String,
    step_name: String,
    job_id: String,
    attempt_key: String,
    attempt_id: String,
    token: String,
    worker: String,
}

/// A board claim with a secret held privately for fenced completion.
///
/// The claim token is intentionally omitted from `Debug` and never stored in `aim.db`.
#[derive(Clone)]
pub struct ClaimHandle {
    /// Authoritative board job snapshot at claim time.
    pub job: JobSnapshot,
    /// Stable attempt ID.
    pub attempt_id: String,
    /// Nonsecret idempotency key for this workflow attempt.
    pub attempt_key: String,
    receipt: ClaimReceipt,
}

impl core::fmt::Debug for ClaimHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClaimHandle")
            .field("job_id", &self.job.id)
            .field("attempt_id", &self.attempt_id)
            .field("attempt_key", &self.attempt_key)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct ReceiptStore {
    dir: PathBuf,
}

#[cfg(unix)]
fn private_dir(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.file_type().is_dir()
                && meta.uid() == nix::unistd::Uid::current().as_raw()
                && meta.permissions().mode().trailing_zeros() >= 6 =>
        {
            Ok(())
        }
        Ok(_) => Err(Error::Receipt("claim directory is not owned and private".into())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path).map_err(|cause| Error::Receipt(cause.to_string()))?;
            File::open(path.parent().ok_or_else(|| Error::Receipt("claim directory has no parent".into()))?)
                .and_then(|parent| parent.sync_all())
                .map_err(|cause| Error::Receipt(cause.to_string()))
        }
        Err(err) => Err(Error::Receipt(err.to_string())),
    }
}

#[cfg(not(unix))]
fn private_dir(_path: &Path) -> Result<(), Error> {
    Err(Error::Receipt("private workflow claims require Unix permissions".into()))
}

impl ReceiptStore {
    fn open(home: &Path) -> Result<Self, Error> {
        private_dir(home)?;
        let run = home.join("run");
        private_dir(&run)?;
        let dir = run.join("workflow-claims");
        private_dir(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, key: &str) -> Result<PathBuf, Error> {
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Receipt("invalid workflow attempt key".into()));
        }
        Ok(self.dir.join(format!("{key}.json")))
    }

    fn load(&self, key: &str) -> Result<Option<ClaimReceipt>, Error> {
        let path = self.path(key)?;
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Receipt(err.to_string())),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            if !meta.file_type().is_file()
                || meta.uid() != nix::unistd::Uid::current().as_raw()
                || meta.permissions().mode().trailing_zeros() < 6
            {
                return Err(Error::Receipt("claim receipt is not owned and private".into()));
            }
        }
        if meta.len() > 4096 {
            return Err(Error::Receipt("claim receipt exceeds size limit".into()));
        }
        let bytes = fs::read(path).map_err(|err| Error::Receipt(err.to_string()))?;
        serde_json::from_slice(&bytes).map(Some).map_err(|err| Error::Receipt(err.to_string()))
    }

    fn save_new(&self, receipt: &ClaimReceipt) -> Result<ClaimReceipt, Error> {
        if let Some(existing) = self.load(&receipt.attempt_key)? {
            return Ok(existing);
        }
        let final_path = self.path(&receipt.attempt_key)?;
        let temp_path = self.dir.join(format!(".{}.tmp", Uuid::new_v4()));
        let bytes = serde_json::to_vec(receipt).map_err(|err| Error::Receipt(err.to_string()))?;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            OpenOptions::new().create_new(true).write(true).mode(0o600).open(&temp_path)
        }
        .map_err(|err| Error::Receipt(err.to_string()))?;
        #[cfg(not(unix))]
        let mut file = OpenOptions::new().create_new(true).write(true).open(&temp_path).map_err(|err| Error::Receipt(err.to_string()))?;
        file.write_all(&bytes).map_err(|err| Error::Receipt(err.to_string()))?;
        file.sync_all().map_err(|err| Error::Receipt(err.to_string()))?;
        let linked = match fs::hard_link(&temp_path, &final_path) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(err) => return Err(Error::Receipt(err.to_string())),
        };
        fs::remove_file(&temp_path).map_err(|err| Error::Receipt(err.to_string()))?;
        File::open(&self.dir).and_then(|dir| dir.sync_all()).map_err(|err| Error::Receipt(err.to_string()))?;
        if linked {
            Ok(receipt.clone())
        } else {
            self.load(&receipt.attempt_key)?.ok_or_else(|| Error::Receipt("claim receipt disappeared".into()))
        }
    }
}

/// A workflow's board adapter and private claim receipt store.
#[derive(Debug)]
pub struct WorkflowBoard {
    board: Board,
    receipts: ReceiptStore,
    worker: String,
    reviewer: String,
}

impl WorkflowBoard {
    /// Opens private claim receipts under `private_home/run/workflow-claims`.
    ///
    /// # Errors
    /// Returns an error if the home or receipt directory is not owned and private.
    pub fn open(board: Board, private_home: &Path, worker: String, reviewer: String) -> Result<Self, Error> {
        if worker.is_empty() || reviewer.is_empty() || worker == reviewer || worker.len() > 256 || reviewer.len() > 256 {
            return Err(Error::Invalid("worker and reviewer must be distinct bounded names".into()));
        }
        Ok(Self { board, receipts: ReceiptStore::open(private_home)?, worker, reviewer })
    }

    /// Posts one ready step as a board job, preserving the same job across retries and restarts.
    /// `title` and `description` are rendered input only for a `job` step.
    ///
    /// # Errors
    /// Returns an error for an unposted dependency, changed post contract, or board failure.
    pub async fn post_step(
        &self,
        store: &WorkflowStore,
        run_id: &str,
        name: &str,
        title: Option<&str>,
        description: Option<&str>,
    ) -> Result<JobSnapshot, Error> {
        let run = store.get_run(run_id)?.ok_or_else(|| Error::Invalid("run does not exist".into()))?;
        let manifest = WorkflowManifest::parse(&run.manifest_text).map_err(Error::Invalid)?;
        let step = manifest.steps.iter().find(|step| step.id == name).ok_or_else(|| Error::Invalid("step not in manifest".into()))?;
        let stored = store.get_step(run_id, name)?.ok_or_else(|| Error::Invalid("step not in run".into()))?;
        let mut depends_on = Vec::with_capacity(step.depends_on.len());
        for dependency in &step.depends_on {
            let prior = store.get_step(run_id, dependency)?.ok_or_else(|| Error::Invalid("dependency not in run".into()))?;
            let job_id = prior.board_job_id.ok_or_else(|| Error::Invalid("dependency has not been posted".into()))?;
            depends_on.push(job_id);
        }
        let (job_title, deliverable) = match &step.kind {
            StepKind::Tool { tool, .. } => {
                (format!("workflow {}/{}", run.name, step.id), format!("Execute dispatcher tool {tool} once and record its JSON result"))
            }
            StepKind::Agent { agent, .. } => {
                (format!("workflow {}/{}", run.name, step.id), format!("Run one turn of agent {agent} and record its JSON result"))
            }
            StepKind::Job { .. } => (
                title.ok_or_else(|| Error::Invalid("rendered job title is required".into()))?.to_owned(),
                description.ok_or_else(|| Error::Invalid("rendered job description is required".into()))?.to_owned(),
            ),
        };
        let spec = JobSpec {
            title: job_title,
            deliverable,
            acceptance: vec!["Result is recorded as an immutable artifact and independently accepted".into()],
            depends_on,
            max_retries: step.retries,
            workspace: Some(run.workspace_root.clone()),
            work: None,
        };
        // The run ID is also the board namespace. Board.post creates it for the first root step.
        let job = self
            .board
            .post(PostParams { run_id: Some(run.id.clone()), spec, idempotency_key: format!("workflow:post:{}:{}", run.id, stored.id) })
            .await?
            .job;
        if job.run_id != run.id {
            return Err(Error::Invalid("posted job belongs to another board run".into()));
        }
        store.set_board_run_id(run_id, &job.run_id, i64::try_from(job.updated_ms).map_err(|err| Error::Invalid(err.to_string()))?)?;
        store.set_board_job_id(run_id, name, &job.id, i64::try_from(job.updated_ms).map_err(|err| Error::Invalid(err.to_string()))?)?;
        Ok(job)
    }

    /// Reads a step's authoritative board snapshot.
    ///
    /// # Errors
    /// Returns an error if the step has not been posted or the board read fails.
    pub async fn show_step(&self, store: &WorkflowStore, run_id: &str, name: &str) -> Result<JobSnapshot, Error> {
        let step = store.get_step(run_id, name)?.ok_or_else(|| Error::Invalid("step does not exist".into()))?;
        let job_id = step.board_job_id.ok_or_else(|| Error::Invalid("step has not been posted".into()))?;
        Ok(self.board.show(job_id).await?)
    }

    /// Claims a tool or agent step after its board dependencies are accepted.
    /// The receipt is durable before `board.claim` runs. Call `WorkflowStore::start_step` after
    /// this method returns and before external execution.
    ///
    /// # Errors
    /// Returns an error for a missing board job, rejected claim, or receipt failure.
    pub async fn claim_step(&self, store: &WorkflowStore, run_id: &str, name: &str) -> Result<ClaimHandle, Error> {
        let key = store.planned_attempt_key(run_id, name)?;
        let job = self.show_step(store, run_id, name).await?;
        let run = store.get_run(run_id)?.ok_or_else(|| Error::Invalid("run does not exist".into()))?;
        let manifest = WorkflowManifest::parse(&run.manifest_text).map_err(Error::Invalid)?;
        let step = manifest.steps.iter().find(|step| step.id == name).ok_or_else(|| Error::Invalid("step not in manifest".into()))?;
        if matches!(&step.kind, StepKind::Job { .. }) {
            return Err(Error::Invalid("job steps are claimed by board workers".into()));
        }
        self.board.register_worker(RegisterWorkerParams { worker: self.worker.clone(), capacity: 1 }).await?;
        let receipt = self.receipts.save_new(&ClaimReceipt {
            run_id: run_id.to_owned(),
            step_name: name.to_owned(),
            job_id: job.id.clone(),
            attempt_key: key.clone(),
            attempt_id: Uuid::new_v4().to_string(),
            token: Uuid::new_v4().to_string(),
            worker: self.worker.clone(),
        })?;
        if receipt.run_id != run_id
            || receipt.step_name != name
            || receipt.job_id != job.id
            || receipt.attempt_key != key
            || receipt.worker != self.worker
        {
            return Err(Error::Receipt("claim receipt identity changed".into()));
        }
        let token_sha256 = format!("{:x}", Sha256::digest(receipt.token.as_bytes()));
        let claimed = self
            .board
            .claim(ClaimParams {
                job_id: job.id,
                worker: self.worker.clone(),
                capacity: 1,
                attempt_id: receipt.attempt_id.clone(),
                token_sha256,
                idempotency_key: key.clone(),
                now_ms: 0,
                lease_ms: LEASE_MS,
                expected_version: None,
            })
            .await?;
        Ok(ClaimHandle { job: claimed.job, attempt_id: claimed.attempt.id, attempt_key: key, receipt })
    }

    /// Loads a private claim receipt and its current board state for crash reconciliation.
    ///
    /// # Errors
    /// Returns an error for an invalid receipt or a board read failure.
    pub async fn load_claim(&self, key: &str) -> Result<Option<ClaimHandle>, Error> {
        let Some(receipt) = self.receipts.load(key)? else { return Ok(None) };
        if receipt.attempt_key != key || receipt.worker != self.worker {
            return Err(Error::Receipt("claim receipt identity changed".into()));
        }
        let job = self.board.show(receipt.job_id.clone()).await?;
        if job.run_id != receipt.run_id || job.attempt.as_ref().is_none_or(|attempt| attempt.id != receipt.attempt_id) {
            return Err(Error::Invalid("board attempt does not match private receipt".into()));
        }
        Ok(Some(ClaimHandle { job, attempt_id: receipt.attempt_id.clone(), attempt_key: key.to_owned(), receipt }))
    }

    /// Renews a live attempt lease with a strictly increasing sequence.
    ///
    /// # Errors
    /// Returns an error when the claim is stale or the sequence did not increase.
    pub async fn heartbeat(&self, claim: &ClaimHandle, sequence: u64) -> Result<(), Error> {
        self.board
            .heartbeat(HeartbeatParams {
                job_id: claim.job.id.clone(),
                attempt_id: claim.attempt_id.clone(),
                claim_token: claim.receipt.token.clone(),
                sequence,
                now_ms: 0,
                lease_ms: LEASE_MS,
            })
            .await?;
        Ok(())
    }

    /// Completes a local step with JSON evidence, confirms stopped effects, and accepts it under
    /// a distinct reviewer. The caller must provide truthful cleanup evidence after shutdown.
    ///
    /// # Errors
    /// Returns an error when the claim is stale, evidence differs on replay, or review fails.
    pub async fn complete_step(&self, claim: &ClaimHandle, result: &Value, cleanup: CleanupReceipt) -> Result<JobSnapshot, Error> {
        if !cleanup.session_stopped || !cleanup.harness_stopped || cleanup.worktree.is_empty() {
            return Err(Error::Invalid("external effects must be stopped before completion".into()));
        }
        let data = serde_json::to_vec(result).map_err(|err| Error::Invalid(err.to_string()))?;
        if data.len() > 1_048_576 {
            return Err(Error::Invalid("result artifact exceeds 1 MiB".into()));
        }
        let expected_hash = format!("{:x}", Sha256::digest(&data));
        let job = match self
            .board
            .complete(CompleteParams {
                job_id: claim.job.id.clone(),
                attempt_id: claim.attempt_id.clone(),
                claim_token: claim.receipt.token.clone(),
                artifacts: vec![ArtifactInput { media_type: RESULT_MEDIA_TYPE.into(), data: Base64Bytes(data) }],
                now_ms: 0,
            })
            .await
        {
            Ok(job) => job,
            Err(err) => {
                let job = self.board.show(claim.job.id.clone()).await?;
                if job.state != JobState::Succeeded || job.attempt.as_ref().is_none_or(|attempt| attempt.id != claim.attempt_id) {
                    return Err(Error::Board(err));
                }
                job
            }
        };
        let artifact = job
            .artifacts
            .iter()
            .find(|artifact| artifact.media_type == RESULT_MEDIA_TYPE && artifact.sha256 == expected_hash)
            .ok_or_else(|| Error::Invalid("board result artifact differs from completed value".into()))?;
        let artifact_id = artifact.id.clone();
        self.board.record_cleanup(claim.job.id.clone(), claim.attempt_id.clone(), claim.receipt.token.clone(), cleanup).await?;
        let current = self.board.show(claim.job.id.clone()).await?;
        if current.review == ReviewState::Accepted {
            return Ok(current);
        }
        if current.review != ReviewState::Pending {
            return Err(Error::Invalid("board review rejected the result".into()));
        }
        Ok(self
            .board
            .review(ReviewParams {
                job_id: claim.job.id.clone(),
                attempt_id: claim.attempt_id.clone(),
                reviewer: self.reviewer.clone(),
                accepted: true,
                evidence: vec![artifact_id],
                expected_version: current.version,
                now_ms: 0,
            })
            .await?)
    }

    /// Fails a local attempt after the caller has stopped its effects. A repeated call after a
    /// partial crash resumes cleanup confirmation for the same fenced attempt.
    ///
    /// # Errors
    /// Returns an error for an invalid cleanup attestation, stale claim, or board failure.
    pub async fn fail_step(&self, claim: &ClaimHandle, reason: &str, cleanup: CleanupReceipt) -> Result<JobSnapshot, Error> {
        if !cleanup.session_stopped || !cleanup.harness_stopped || cleanup.worktree.is_empty() {
            return Err(Error::Invalid("external effects must be stopped before failure completion".into()));
        }
        let job = match self
            .board
            .fail(FailParams {
                job_id: claim.job.id.clone(),
                attempt_id: claim.attempt_id.clone(),
                claim_token: claim.receipt.token.clone(),
                reason: reason.to_owned(),
                now_ms: 0,
            })
            .await
        {
            Ok(job) => job,
            Err(err) => {
                let job = self.board.show(claim.job.id.clone()).await?;
                if job.state != JobState::Failed || job.attempt.as_ref().is_none_or(|attempt| attempt.id != claim.attempt_id) {
                    return Err(Error::Board(err));
                }
                job
            }
        };
        self.board.record_cleanup(job.id.clone(), claim.attempt_id.clone(), claim.receipt.token.clone(), cleanup).await?;
        Ok(self.board.show(job.id).await?)
    }

    /// Opens the next board attempt only after a failed or rejected attempt was cleaned up.
    ///
    /// # Errors
    /// Returns a conflict for an unfinished attempt, stale version, or exhausted retry budget.
    pub async fn retry_step(&self, job: &JobSnapshot) -> Result<JobSnapshot, Error> {
        if !matches!(job.state, JobState::Failed | JobState::Cancelled) && job.review != ReviewState::Rejected {
            return Err(Error::Invalid("board job is not retryable".into()));
        }
        if job.attempt.as_ref().is_none_or(|attempt| !attempt.cleanup_confirmed) {
            return Err(Error::Invalid("board attempt cleanup is not confirmed".into()));
        }
        Ok(self.board.retry(RetryParams { job_id: job.id.clone(), expected_version: job.version, now_ms: 0 }).await?)
    }

    /// Reads an accepted workflow JSON result artifact, if the board job carries one.
    ///
    /// # Errors
    /// Returns an error for duplicate, malformed, or unreadable result evidence.
    pub async fn accepted_result(&self, job: &JobSnapshot) -> Result<Option<Value>, Error> {
        if job.state != JobState::Succeeded || job.review != ReviewState::Accepted {
            return Ok(None);
        }
        let mut artifacts = job.artifacts.iter().filter(|artifact| artifact.media_type == RESULT_MEDIA_TYPE);
        let Some(artifact) = artifacts.next() else { return Ok(None) };
        if artifacts.next().is_some() {
            return Err(Error::Invalid("accepted step has multiple result artifacts".into()));
        }
        let bytes = self.board.artifact_bytes(artifact.id.clone()).await?;
        let value = serde_json::from_slice(&bytes).map_err(|err| Error::Invalid(err.to_string()))?;
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{NewRun, NewStep, StepState};
    use super::*;
    use serde_json::json;

    #[test]
    fn claim_receipt_is_private_and_first_write_wins() {
        let home = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = ReceiptStore::open(home.path()).unwrap();
        let receipt = ClaimReceipt {
            run_id: "run".into(),
            step_name: "step".into(),
            job_id: "job".into(),
            attempt_key: "a".repeat(64),
            attempt_id: Uuid::new_v4().to_string(),
            token: Uuid::new_v4().to_string(),
            worker: "worker".into(),
        };
        let first = store.save_new(&receipt).unwrap();
        let mut replacement = receipt.clone();
        replacement.token = Uuid::new_v4().to_string();
        let replayed = store.save_new(&replacement).unwrap();
        assert_eq!(first.token, replayed.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(store.path(&receipt.attempt_key).unwrap()).unwrap().permissions().mode();
            assert!(mode.trailing_zeros() >= 6);
        }
    }

    #[tokio::test]
    async fn board_result_and_workflow_step_survive_reopen() {
        let home = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = home.path().join("aim.db");
        let store = WorkflowStore::open(&path).unwrap();
        let board = Board::open(&path).unwrap();
        let manifest = r#"
name = "one"
version = "1"
trigger = "manual"
params = '{"type":"object"}'
results = '{"type":"object"}'
[ceiling]
roots = ["."]
ops = ["read"]
[budget]
max_steps = 1
max_tokens = 100
timeout_seconds = 30
[[steps]]
id = "check"
kind = "tool"
tool = "fs.read"
arguments = '{"path":"README.md"}'
retries = 0
"#;
        let run = store
            .start_run(&NewRun {
                name: "one".into(),
                manifest_hash: "test-hash".into(),
                manifest_text: manifest.into(),
                manifest_version: "1".into(),
                params: json!({}),
                steps: vec![NewStep { name: "check".into(), max_retries: 0, board_job_id: None }],
                board_run_id: None,
                workspace_root: home.path().to_string_lossy().into_owned(),
                now_ms: 1,
            })
            .unwrap();
        let adapter = WorkflowBoard::open(board, home.path(), "workflow-test".into(), "workflow-reviewer".into()).unwrap();
        let posted = adapter.post_step(&store, &run.id, "check", None, None).await.unwrap();
        assert_eq!(posted.state, JobState::Posted);
        let claim = adapter.claim_step(&store, &run.id, "check").await.unwrap();
        let step = store.start_step(&run.id, "check", 2).unwrap();
        assert_eq!(step.attempt_key.as_deref(), Some(claim.attempt_key.as_str()));
        adapter.heartbeat(&claim, 1).await.unwrap();
        let result = json!({"ok":true});
        let accepted = adapter
            .complete_step(
                &claim,
                &result,
                CleanupReceipt {
                    session_id: None,
                    worktree: home.path().to_string_lossy().into_owned(),
                    session_stopped: true,
                    harness_stopped: true,
                    disposition: "kept".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(accepted.review, ReviewState::Accepted);
        store.succeed_step(&run.id, "check", &claim.attempt_key, &result, 3).unwrap();
        assert_eq!(adapter.accepted_result(&accepted).await.unwrap(), Some(result.clone()));
        drop(adapter);
        drop(store);

        let reopened = WorkflowStore::open(&path).unwrap();
        let step = reopened.get_step(&run.id, "check").unwrap().unwrap();
        assert_eq!(step.state, StepState::Succeeded);
        assert_eq!(step.result, Some(result));
        assert!(reopened.list_in_flight(&run.id).unwrap().is_empty());
        assert!(reopened.start_step(&run.id, "check", 4).is_err());
    }
}
