//! Typed `aim-daemon/1` blackboard methods and notifications (docs/adr/0019).
//!
//! Job and attempt state is authoritative in the daemon ledger. `board.event` is a bounded
//! notification hint; clients reconcile with `board.poll` or `board.show` after reconnecting.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::content::Base64Bytes;
use crate::{method, notification};

/// A job's immutable posted contract.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct JobSpec {
    /// Short human-readable title.
    pub title: String,
    /// Deliverable the worker must produce.
    pub deliverable: String,
    /// Review criteria supplied by the poster.
    pub acceptance: Vec<String>,
    /// Job IDs whose accepted evidence is required before this job can be claimed.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Maximum new attempts after the initial attempt.
    pub max_retries: u32,
    /// Workspace selected by the lead, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Execution state; review acceptance is recorded separately.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for an eligible claim and accepted dependencies.
    Posted,
    /// A fenced attempt has a claimant but is not running yet.
    Claimed,
    /// The current attempt is running.
    Running,
    /// The attempt completed; review has not accepted it merely because it finished.
    Succeeded,
    /// The attempt failed or its lease expired.
    Failed,
    /// The job was cancelled.
    Cancelled,
}

/// Separate review state for a finished job.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    /// No reviewer has decided.
    Pending,
    /// Evidence was reviewed and accepted.
    Accepted,
    /// Review rejected the contribution.
    Rejected,
}

/// Nonsecret summary of a current or historical attempt.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AttemptSummary {
    /// Stable attempt ID.
    pub id: String,
    /// Fenced generation within its job.
    pub generation: u32,
    /// Claimant identity.
    pub worker: String,
    /// Current attempt state.
    pub state: JobState,
    /// Lease deadline, Unix milliseconds.
    pub lease_until_ms: u64,
    /// Last accepted heartbeat sequence.
    pub heartbeat_seq: u64,
    /// Whether owned external effects have been reconciled before retry.
    pub cleanup_confirmed: bool,
}

/// Immutable artifact reference; bytes are not returned in board snapshots.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactSummary {
    /// Stable artifact ID.
    pub id: String,
    /// Attempt that produced it.
    pub attempt_id: String,
    /// Media type of the bytes.
    pub media_type: String,
    /// SHA-256 digest in lowercase hexadecimal.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// Authoritative job snapshot without claim secrets.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct JobSnapshot {
    /// Stable job ID.
    pub id: String,
    /// Run namespace.
    pub run_id: String,
    /// Posted contract.
    pub spec: JobSpec,
    /// Execution state.
    pub state: JobState,
    /// Independent review state.
    pub review: ReviewState,
    /// Current generation, starting at zero.
    pub generation: u32,
    /// Durable optimistic-concurrency version.
    pub version: u64,
    /// Assigned worker, when the lead assigned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_worker: Option<String>,
    /// Current attempt, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<AttemptSummary>,
    /// Number of artifacts on the current successful attempt.
    pub artifact_count: u32,
    /// Creation timestamp, Unix milliseconds.
    pub created_ms: u64,
    /// Last committed transition timestamp, Unix milliseconds.
    pub updated_ms: u64,
}

/// An ordered committed board change. Delivery can repeat; the ID and sequence are stable.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardEvent {
    /// Stable event ID.
    pub id: String,
    /// Run namespace.
    pub run_id: String,
    /// Affected job.
    pub job_id: String,
    /// Monotonic run sequence.
    pub seq: u64,
    /// Job version committed by the event.
    pub version: u64,
    /// Stable event kind, such as `job.claimed` or `review.accepted`.
    pub kind: String,
    /// Commit time, Unix milliseconds.
    pub ts_ms: u64,
}

/// Input bytes for an immutable, content-hashed artifact.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactInput {
    /// Media type of the content.
    pub media_type: String,
    /// Exact bytes to hash and store.
    pub data: Base64Bytes,
}

/// Post one job, creating a run when `run_id` is absent.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PostParams {
    /// Existing run namespace, or absent to create one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Immutable job contract.
    pub spec: JobSpec,
    /// Stable key for exact retry of this post.
    pub idempotency_key: String,
}

/// Result of posting a job.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PostResult {
    /// Committed job.
    pub job: JobSnapshot,
}
method!(
    /// `board.post` — durably post a contract and its event.
    BoardPost = "board.post" (PostParams) -> PostResult
);

/// List jobs in a run, or recent jobs across runs.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Restrict to this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Maximum rows; the service applies a cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A bounded list of authoritative job snapshots.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListResult {
    /// Jobs in stable order.
    pub jobs: Vec<JobSnapshot>,
}
method!(
    /// `board.list` — read the current board.
    BoardList = "board.list" (ListParams) -> ListResult
);

/// Identify one job.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct JobRef {
    /// Stable job ID.
    pub job_id: String,
}
method!(
    /// `board.show` — read one authoritative job.
    BoardShow = "board.show" (JobRef) -> JobSnapshot
);

/// Lead assignment of a worker to a posted job; assignment does not launch it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AssignParams {
    /// Target job.
    pub job_id: String,
    /// Worker identity.
    pub worker: String,
    /// Job version observed by the caller.
    pub expected_version: u64,
}
method!(
    /// `board.assign` — assign a worker without starting an attempt.
    BoardAssign = "board.assign" (AssignParams) -> JobSnapshot
);

/// Claim a dependency-ready job with a declared worker capacity.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClaimParams {
    /// Target job.
    pub job_id: String,
    /// Worker identity.
    pub worker: String,
    /// Maximum active and cleanup-pending claims for this worker.
    pub capacity: u32,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
    /// Requested lease length in milliseconds (the service caps it).
    pub lease_ms: u64,
    /// Job version observed by the caller, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
}

/// One-time claim receipt. Store `claim_token` privately; snapshots and events never repeat it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClaimResult {
    /// Committed job.
    pub job: JobSnapshot,
    /// Fenced attempt.
    pub attempt: AttemptSummary,
    /// One-time bearer secret for this attempt.
    pub claim_token: String,
}
method!(
    /// `board.claim` — atomically reserve a fenced attempt.
    BoardClaim = "board.claim" (ClaimParams) -> ClaimResult
);

/// Fenced attempt heartbeat; stale tokens and non-increasing sequences fail.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatParams {
    /// Target job.
    pub job_id: String,
    /// Attempt ID.
    pub attempt_id: String,
    /// One-time bearer secret returned by claim.
    pub claim_token: String,
    /// Monotonic heartbeat sequence.
    pub sequence: u64,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
    /// Requested new lease duration.
    pub lease_ms: u64,
}
method!(
    /// `board.heartbeat` — renew a live fenced attempt.
    BoardHeartbeat = "board.heartbeat" (HeartbeatParams) -> JobSnapshot
);

/// Send a bounded typed message under an active attempt.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MessageParams {
    /// Target job.
    pub job_id: String,
    /// Sending attempt.
    pub attempt_id: String,
    /// Claim secret for the attempt.
    pub claim_token: String,
    /// Recipient identity or thread.
    pub recipient: String,
    /// Message kind.
    pub kind: String,
    /// Bounded UTF-8 body.
    pub body: String,
    /// Stable key for exact retry.
    pub idempotency_key: String,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
}

/// Durable message receipt, without the body.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MessageResult {
    /// Stable message ID.
    pub id: String,
    /// Run sequence committed with it.
    pub seq: u64,
}
method!(
    /// `board.message` — persist a bounded typed message.
    BoardMessage = "board.message" (MessageParams) -> MessageResult
);

/// Complete an attempt with immutable artifact content.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CompleteParams {
    /// Target job.
    pub job_id: String,
    /// Completing attempt.
    pub attempt_id: String,
    /// Claim secret for the attempt.
    pub claim_token: String,
    /// Evidence bytes to hash and retain immutably.
    pub artifacts: Vec<ArtifactInput>,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
}
method!(
    /// `board.complete` — finish execution without accepting it.
    BoardComplete = "board.complete" (CompleteParams) -> JobSnapshot
);

/// Fail an attempt; cleanup can remain uncertain.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FailParams {
    /// Target job.
    pub job_id: String,
    /// Failing attempt.
    pub attempt_id: String,
    /// Claim secret for the attempt.
    pub claim_token: String,
    /// Safe diagnostic class (no credentials or raw model output).
    pub reason: String,
    /// Whether external effects have been reconciled and stopped.
    pub cleanup_confirmed: bool,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
}
method!(
    /// `board.fail` — record an attempt failure and cleanup status.
    BoardFail = "board.fail" (FailParams) -> JobSnapshot
);

/// Cancel a job at an observed version.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CancelParams {
    /// Target job.
    pub job_id: String,
    /// Job version observed by the caller.
    pub expected_version: u64,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
}
method!(
    /// `board.cancel` — persist cancellation before external cleanup.
    BoardCancel = "board.cancel" (CancelParams) -> JobSnapshot
);

/// Retry after an eligible terminal outcome and confirmed cleanup.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RetryParams {
    /// Target job.
    pub job_id: String,
    /// Job version observed by the caller.
    pub expected_version: u64,
    /// Caller asserts external cleanup was confirmed.
    pub cleanup_confirmed: bool,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
}
method!(
    /// `board.retry` — fence the prior attempt and open a new generation.
    BoardRetry = "board.retry" (RetryParams) -> JobSnapshot
);

/// Record a separate review decision against a completed attempt.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReviewParams {
    /// Target job.
    pub job_id: String,
    /// Attempt whose evidence was reviewed.
    pub attempt_id: String,
    /// Reviewer identity.
    pub reviewer: String,
    /// Whether the evidence met the acceptance contract.
    pub accepted: bool,
    /// Artifact IDs used as reviewed evidence.
    pub evidence: Vec<String>,
    /// Job version observed by the reviewer.
    pub expected_version: u64,
    /// Observed current time, Unix milliseconds.
    pub now_ms: u64,
}
method!(
    /// `board.review` — record acceptance or rejection separately from execution.
    BoardReview = "board.review" (ReviewParams) -> JobSnapshot
);

/// Reconciliation read for a run after a durable event sequence.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PollParams {
    /// Run namespace.
    pub run_id: String,
    /// Return events with a sequence greater than this value.
    pub after_seq: u64,
    /// Maximum number of events.
    pub limit: u32,
}

/// Authoritative snapshots and ordered committed events.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PollResult {
    /// Current job snapshots in the run.
    pub jobs: Vec<JobSnapshot>,
    /// Events after the requested cursor, in sequence order.
    pub events: Vec<BoardEvent>,
    /// Highest event sequence returned (or the incoming cursor if none).
    pub next_seq: u64,
}
method!(
    /// `board.poll` — reconcile after a notification or reconnect.
    BoardPoll = "board.poll" (PollParams) -> PollResult
);

/// Subscribe to board events on this daemon connection, starting after a cursor.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchParams {
    /// Run namespace.
    pub run_id: String,
    /// Last event sequence already seen.
    pub after_seq: u64,
}
method!(
    /// `board.watch` — return a snapshot and start `board.event` hints.
    BoardWatch = "board.watch" (WatchParams) -> PollResult
);

/// One board event hint sent to a watcher.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardEventParams {
    /// Committed event; reconcile by `run_id` and `seq`.
    pub event: BoardEvent,
}
notification!(
    /// `board.event` — ordered best-effort notification of a committed ledger event.
    BoardEventNotification = "board.event" (BoardEventParams)
);
