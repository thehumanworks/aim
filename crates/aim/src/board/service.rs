//! Typed board operations over the transactional ledger.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use aim_kernel::job::{
    Claim as KernelClaim, CleanupState as KernelCleanup, Event as KernelEvent, Job as KernelJob, JobState as KernelState, LifecycleError,
    ReviewState as KernelReview,
};
use aim_proto::board::{
    ArtifactSummary, AssignParams, AttemptSummary, BoardEvent, CancelParams, ClaimParams, ClaimResult, CompleteParams, FailParams,
    HeartbeatParams, JobSnapshot, JobSpec, JobState, ListParams, ListResult, MessageParams, MessageResult, PollParams, PollResult,
    PostParams, PostResult, RegisterWorkerParams, RetryParams, ReviewParams, ReviewState,
};
use rusqlite::{OptionalExtension as _, Transaction, params};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::{Board, CleanupReceipt, Error};

type JobRow = (String, String, String, String, i64, i64, Option<String>, i64, i64);
type ClaimDbRow = (i64, i64, String, i64, i64, String, String, Vec<u8>);

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

pub(super) fn sql(err: impl core::fmt::Display) -> Error {
    Error::Storage(err.to_string())
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String, Error> {
    serde_json::to_string(value).map_err(sql)
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, Error> {
    serde_json::from_str(value).map_err(sql)
}

fn now_ms() -> Result<u64, Error> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).map_err(sql)?;
    u64::try_from(duration.as_millis()).map_err(sql)
}

fn as_i64(value: u64) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::Invalid("integer exceeds SQLite range".into()))
}

fn as_u64(value: i64) -> Result<u64, Error> {
    u64::try_from(value).map_err(sql)
}

fn as_u32(value: i64) -> Result<u32, Error> {
    u32::try_from(value).map_err(sql)
}

fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Posted => "posted",
        JobState::Claimed => "claimed",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn parse_state(value: &str) -> Result<JobState, Error> {
    match value {
        "posted" => Ok(JobState::Posted),
        "claimed" => Ok(JobState::Claimed),
        "running" => Ok(JobState::Running),
        "succeeded" => Ok(JobState::Succeeded),
        "failed" => Ok(JobState::Failed),
        "cancelled" => Ok(JobState::Cancelled),
        _ => Err(Error::Storage(format!("unknown board job state {value}"))),
    }
}

fn review_name(review: ReviewState) -> &'static str {
    match review {
        ReviewState::Pending => "pending",
        ReviewState::Accepted => "accepted",
        ReviewState::Rejected => "rejected",
    }
}

fn kernel_state(state: JobState) -> KernelState {
    match state {
        JobState::Posted => KernelState::Posted,
        JobState::Claimed => KernelState::Claimed,
        JobState::Running => KernelState::Running,
        JobState::Succeeded => KernelState::Succeeded,
        JobState::Failed => KernelState::Failed,
        JobState::Cancelled => KernelState::Cancelled,
    }
}

fn proto_state(state: KernelState) -> JobState {
    match state {
        KernelState::Posted => JobState::Posted,
        KernelState::Claimed => JobState::Claimed,
        KernelState::Running => JobState::Running,
        KernelState::Succeeded => JobState::Succeeded,
        KernelState::Failed => JobState::Failed,
        KernelState::Cancelled => JobState::Cancelled,
    }
}

fn kernel_review(review: ReviewState) -> KernelReview {
    match review {
        ReviewState::Pending => KernelReview::Pending,
        ReviewState::Accepted => KernelReview::Accepted,
        ReviewState::Rejected => KernelReview::Rejected,
    }
}

fn proto_review(review: KernelReview) -> ReviewState {
    match review {
        KernelReview::Pending => ReviewState::Pending,
        KernelReview::Accepted => ReviewState::Accepted,
        KernelReview::Rejected => ReviewState::Rejected,
    }
}

fn kernel_error(err: LifecycleError) -> Error {
    match err {
        LifecycleError::StaleClaim | LifecycleError::InvalidLease => Error::StaleClaim,
        LifecycleError::InvalidSnapshot => Error::Storage("persisted job violates kernel invariant".into()),
        _ => Error::Conflict(format!("kernel rejected transition: {err:?}")),
    }
}

fn kernel_job(tx: &Transaction<'_>, snapshot: &JobSnapshot) -> Result<KernelJob, Error> {
    let claim = if let Some(attempt) = &snapshot.attempt {
        let claim_id: i64 =
            tx.query_row("SELECT claim_id FROM board_attempts WHERE id=?1", params![attempt.id], |row| row.get(0)).map_err(sql)?;
        Some(KernelClaim { worker: worker_id(&attempt.worker), claim_id: as_u64(claim_id)?, lease_until: attempt.lease_until_ms })
    } else {
        None
    };
    let cleanup = match (&snapshot.attempt, snapshot.state) {
        (None, _) | (_, JobState::Posted) => KernelCleanup::Confirmed,
        (Some(_), JobState::Claimed | JobState::Running) => KernelCleanup::Active,
        (Some(attempt), JobState::Succeeded | JobState::Failed | JobState::Cancelled) => {
            if attempt.cleanup_confirmed {
                KernelCleanup::Confirmed
            } else {
                KernelCleanup::Pending
            }
        }
    };
    // Review verifies every referenced immutable artifact before accepting it. The artifact
    // table refuses updates and deletes, so a later restore needs only the review's evidence
    // references; re-reading and hashing the bytes here makes every claim scale with history.
    let evidence_present = if snapshot.review == ReviewState::Pending {
        false
    } else {
        let evidence: Option<String> = tx
            .query_row(
                "SELECT evidence_json FROM board_reviews WHERE job_id=?1 AND attempt_id=?2 ORDER BY created_ms DESC,id DESC LIMIT 1",
                params![snapshot.id, snapshot.attempt.as_ref().map(|attempt| attempt.id.as_str())],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        let ids = evidence.as_deref().map(decode::<Vec<String>>).transpose()?.unwrap_or_default();
        for id in &ids {
            let present: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM board_artifacts WHERE id=?1 AND attempt_id=?2)",
                    params![id, snapshot.attempt.as_ref().map(|attempt| attempt.id.as_str())],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if !present {
                return Err(Error::Storage("review references missing attempt artifact".into()));
            }
        }
        !ids.is_empty()
    };
    KernelJob::restore(
        kernel_state(snapshot.state),
        kernel_review(snapshot.review),
        snapshot.generation,
        claim,
        cleanup,
        snapshot.spec.max_retries,
        evidence_present,
    )
    .map_err(kernel_error)
}

fn update_from_kernel(tx: &Transaction<'_>, current: &JobSnapshot, kernel: &KernelJob, now: u64) -> Result<JobSnapshot, Error> {
    update_job(tx, current, proto_state(kernel.state()), proto_review(kernel.review()), kernel.generation(), now)
}

fn parse_review(value: &str) -> Result<ReviewState, Error> {
    match value {
        "pending" => Ok(ReviewState::Pending),
        "accepted" => Ok(ReviewState::Accepted),
        "rejected" => Ok(ReviewState::Rejected),
        _ => Err(Error::Storage(format!("unknown board review state {value}"))),
    }
}

pub(super) fn load_job(tx: &Transaction<'_>, id: &str) -> Result<JobSnapshot, Error> {
    let row: Option<JobRow> = tx
        .query_row(
            "SELECT run_id, contract_json, state, review_state, generation, version, assignee, created_ms, updated_ms FROM board_jobs WHERE id=?1",
            params![id],
            |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?))
            },
        )
        .optional()
        .map_err(sql)?;
    let (run_id, spec, state, review, generation, version, assigned_worker, created_ms, updated_ms) = row.ok_or(Error::NotFound)?;
    let attempt_row: Option<(String, i64, String, String, i64, i64, String)> = tx
        .query_row(
            "SELECT id, generation, assignee, state, lease_until_ms, heartbeat_seq, cleanup_state FROM board_attempts WHERE job_id=?1 AND generation=?2",
            params![id, generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .optional()
        .map_err(sql)?;
    let attempt = attempt_row
        .map(|(id, generation, worker, state, lease_until_ms, heartbeat_seq, cleanup_state)| {
            Ok(AttemptSummary {
                id,
                generation: as_u32(generation)?,
                worker,
                state: parse_state(&state)?,
                lease_until_ms: as_u64(lease_until_ms)?,
                heartbeat_seq: as_u64(heartbeat_seq)?,
                cleanup_confirmed: cleanup_state == "confirmed",
            })
        })
        .transpose()?;
    let mut artifact_query = tx.prepare_cached(
        "SELECT a.id,a.attempt_id,a.media_type,a.sha256,a.byte_size FROM board_artifacts a JOIN board_attempts t ON a.attempt_id=t.id WHERE a.job_id=?1 AND t.generation=?2 ORDER BY a.created_ms,a.id",
    ).map_err(sql)?;
    let artifacts = artifact_query
        .query_map(params![id, generation], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(sql)?
        .map(|row| {
            let (id, attempt_id, media_type, sha256, size) = row.map_err(sql)?;
            Ok(ArtifactSummary { id, attempt_id, media_type, sha256: hex_bytes(&sha256), size: as_u64(size)? })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(JobSnapshot {
        id: id.into(),
        run_id,
        spec: decode(&spec)?,
        state: parse_state(&state)?,
        review: parse_review(&review)?,
        generation: as_u32(generation)?,
        version: as_u64(version)?,
        assigned_worker,
        attempt,
        artifact_count: u32::try_from(artifacts.len()).map_err(sql)?,
        artifacts,
        created_ms: as_u64(created_ms)?,
        updated_ms: as_u64(updated_ms)?,
    })
}

fn list_jobs(
    tx: &Transaction<'_>,
    run_id: Option<&str>,
    limit: u32,
    after_job: Option<&str>,
) -> Result<(Vec<JobSnapshot>, Option<String>, bool), Error> {
    let anchor: Option<(i64, String)> = after_job
        .map(|id| {
            tx.query_row("SELECT created_ms,id FROM board_jobs WHERE id=?1 AND (?2 IS NULL OR run_id=?2)", params![id, run_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .map_err(sql)?
            .ok_or(Error::Invalid("unknown job page cursor".into()))
        })
        .transpose()?;
    let mut statement = tx
        .prepare_cached(
            "SELECT id FROM board_jobs WHERE (?1 IS NULL OR run_id=?1) AND (?2 IS NULL OR (created_ms,id) < (?2,?3))
         ORDER BY created_ms DESC, id DESC LIMIT ?4",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map(params![run_id, anchor.as_ref().map(|a| a.0), anchor.as_ref().map(|a| a.1.as_str()), limit.min(200) + 1], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql)?;
    let ids = rows.collect::<Result<Vec<_>, _>>().map_err(sql)?;
    let mut jobs = Vec::new();
    let mut bytes = 0usize;
    for id in &ids {
        if jobs.len() >= usize::try_from(limit.min(200)).map_err(sql)? {
            break;
        }
        let job = load_job(tx, id)?;
        let size = serde_json::to_vec(&job).map_err(sql)?.len();
        if !jobs.is_empty() && bytes.saturating_add(size) > 8 * 1024 * 1024 {
            break;
        }
        bytes = bytes.saturating_add(size);
        jobs.push(job);
    }
    let truncated = jobs.len() < ids.len();
    let next_job = if truncated { jobs.last().map(|job| job.id.clone()) } else { None };
    Ok((jobs, next_job, truncated))
}

pub(super) fn insert_event(
    tx: &Transaction<'_>,
    run_id: &str,
    job_id: &str,
    version: u64,
    kind: &str,
    now: u64,
) -> Result<BoardEvent, Error> {
    let id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO board_outbox (event_id,run_id,job_id,job_version,kind,payload_json,created_ms) VALUES (?1,?2,?3,?4,?5,'{}',?6)",
        params![id, run_id, job_id, as_i64(version)?, kind, as_i64(now)?],
    )
    .map_err(sql)?;
    Ok(BoardEvent {
        id,
        run_id: run_id.into(),
        job_id: job_id.into(),
        seq: as_u64(tx.last_insert_rowid())?,
        version,
        kind: kind.into(),
        ts_ms: now,
    })
}

pub(super) fn update_job(
    tx: &Transaction<'_>,
    job: &JobSnapshot,
    state: JobState,
    review: ReviewState,
    generation: u32,
    now: u64,
) -> Result<JobSnapshot, Error> {
    let updated = tx
        .execute(
            "UPDATE board_jobs SET state=?1, review_state=?2, generation=?3, version=version+1, updated_ms=?4 WHERE id=?5 AND version=?6",
            params![state_name(state), review_name(review), generation, as_i64(now)?, job.id, as_i64(job.version)?],
        )
        .map_err(sql)?;
    if updated != 1 {
        return Err(Error::Conflict("job version changed during update".into()));
    }
    load_job(tx, &job.id)
}

fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn parse_hash(hex: &str) -> Result<[u8; 32], Error> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
        return Err(Error::Invalid("claim token hash must be 64 lowercase hex digits".into()));
    }
    let mut hash = [0_u8; 32];
    for (index, slot) in hash.iter_mut().enumerate() {
        let start = index.saturating_mul(2);
        *slot = u8::from_str_radix(hex.get(start..start + 2).ok_or_else(|| Error::Invalid("claim hash is incomplete".into()))?, 16)
            .map_err(|_| Error::Invalid("claim hash is invalid".into()))?;
    }
    Ok(hash)
}

fn same_hash(a: &[u8], b: &[u8; 32]) -> bool {
    if a.len() != 32 {
        return false;
    }
    let difference = a.iter().zip(b).fold(0_u8, |difference, (left, right)| difference | (left ^ right));
    difference == 0
}

fn cleanup_recorded(tx: &Transaction<'_>, attempt_id: &str) -> Result<bool, Error> {
    tx.query_row("SELECT EXISTS(SELECT 1 FROM board_cleanup_receipts WHERE attempt_id=?1)", params![attempt_id], |row| row.get(0))
        .map_err(sql)
}

struct ClaimRow {
    generation: u32,
    claim_id: u64,
    worker: String,
    lease_until_ms: u64,
    heartbeat_seq: u64,
    state: JobState,
}

fn claim_row(tx: &Transaction<'_>, job: &JobSnapshot, attempt_id: &str, token: &str, now: u64) -> Result<ClaimRow, Error> {
    let row: Option<ClaimDbRow> = tx.query_row(
        "SELECT generation,claim_id,assignee,lease_until_ms,heartbeat_seq,state,cleanup_state,token_hash FROM board_attempts WHERE id=?1 AND job_id=?2",
        params![attempt_id, job.id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?)),
    ).optional().map_err(sql)?;
    let (generation, claim_id, worker, lease_until_ms, heartbeat_seq, state, _cleanup, stored_hash) = row.ok_or(Error::StaleClaim)?;
    let generation = as_u32(generation)?;
    let lease_until_ms = as_u64(lease_until_ms)?;
    if generation != job.generation || !same_hash(&stored_hash, &hash_token(token)) || now >= lease_until_ms {
        return Err(Error::StaleClaim);
    }
    Ok(ClaimRow {
        generation,
        claim_id: as_u64(claim_id)?,
        worker,
        lease_until_ms,
        heartbeat_seq: as_u64(heartbeat_seq)?,
        state: parse_state(&state)?,
    })
}

fn worker_id(worker: &str) -> u64 {
    let digest = Sha256::digest(worker.as_bytes());
    let mut bytes = [0_u8; 8];
    for (slot, value) in bytes.iter_mut().zip(digest.iter()) {
        *slot = *value;
    }
    u64::from_be_bytes(bytes)
}

fn valid_spec(spec: &JobSpec) -> Result<(), Error> {
    if spec.title.is_empty() || spec.title.len() > 256 || spec.deliverable.is_empty() || spec.deliverable.len() > 8192 {
        return Err(Error::Invalid("title or deliverable is empty or too long".into()));
    }
    if spec.depends_on.len() > 64 || spec.acceptance.len() > 64 || spec.max_retries > 32 {
        return Err(Error::Invalid("job limit exceeded".into()));
    }
    if spec.acceptance.iter().any(|item| item.is_empty() || item.len() > 8192)
        || spec.depends_on.iter().any(|id| id.is_empty() || id.len() > 256)
        || spec.workspace.as_deref().is_some_and(|workspace| workspace.len() > 4096)
    {
        return Err(Error::Invalid("job contract field exceeds its bound".into()));
    }
    let mut dependencies = HashSet::with_capacity(spec.depends_on.len());
    if spec.depends_on.iter().any(|id| !dependencies.insert(id)) {
        return Err(Error::Invalid("duplicate dependency".into()));
    }
    if let Some(work) = &spec.work
        && (spec.workspace.as_deref().is_none_or(|path| path.is_empty() || !path.starts_with('/'))
            || work.target_branch.is_empty()
            || work.target_branch.len() > 256
            || work.target_branch.starts_with('-')
            || work.check_command.as_deref().is_some_and(|command| command.is_empty() || command.len() > 4096))
    {
        return Err(Error::Invalid("work policy needs an absolute workspace, target branch, and bounded check".into()));
    }
    Ok(())
}

impl Board {
    /// Reads one immutable artifact after checking its stored hash.
    ///
    /// # Errors
    /// Returns not-found, hash mismatch, or storage failure.
    pub async fn artifact_bytes(&self, artifact_id: String) -> Result<Vec<u8>, Error> {
        self.ledger
            .transact(move |tx| {
                let found: Option<(Vec<u8>, Vec<u8>)> = tx
                    .query_row("SELECT sha256,data FROM board_artifacts WHERE id=?1", params![artifact_id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .optional()
                    .map_err(sql)?;
                let (hash, bytes) = found.ok_or(Error::NotFound)?;
                if !same_hash(&hash, &Sha256::digest(&bytes).into()) {
                    return Err(Error::Storage("artifact hash mismatch".into()));
                }
                Ok(bytes)
            })
            .await
    }

    /// Persists a runner-produced cleanup receipt under the fenced attempt token.
    ///
    /// # Errors
    /// Refuses an invalid token, incomplete receipt, or storage failure.
    pub async fn record_cleanup(&self, job_id: String, attempt_id: String, token: String, receipt: CleanupReceipt) -> Result<(), Error> {
        if !receipt.session_stopped
            || !receipt.harness_stopped
            || receipt.worktree.is_empty()
            || !matches!(receipt.disposition.as_str(), "kept" | "removed")
        {
            return Err(Error::Invalid("cleanup receipt does not attest stopped effects".into()));
        }
        let now = now_ms()?;
        let _event = self
            .ledger
            .transact(move |tx| {
                let job = load_job(tx, &job_id)?;
                if job.attempt.as_ref().is_none_or(|attempt| attempt.id != attempt_id) {
                    return Err(Error::StaleClaim);
                }
                let (claim_id, stored): (i64, Vec<u8>) = tx
                    .query_row(
                        "SELECT claim_id,token_hash FROM board_attempts WHERE id=?1 AND job_id=?2 AND generation=?3",
                        params![attempt_id, job_id, job.generation],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(sql)?;
                if !same_hash(&stored, &hash_token(&token)) {
                    return Err(Error::StaleClaim);
                }
                let encoded = encode(&receipt)?;
                let prior: Option<String> = tx
                    .query_row("SELECT receipt_json FROM board_cleanup_receipts WHERE attempt_id=?1", params![attempt_id], |row| row.get(0))
                    .optional()
                    .map_err(sql)?;
                if let Some(prior) = prior {
                    if prior != encoded {
                        return Err(Error::Conflict("cleanup receipt changed on retry".into()));
                    }
                } else {
                    tx.execute(
                        "INSERT INTO board_cleanup_receipts (attempt_id,job_id,receipt_json,created_ms) VALUES (?1,?2,?3,?4)",
                        params![attempt_id, job_id, encoded, as_i64(now)?],
                    )
                    .map_err(sql)?;
                }
                let current = job.attempt.as_ref().ok_or(Error::StaleClaim)?;
                if !current.cleanup_confirmed && matches!(job.state, JobState::Succeeded | JobState::Failed | JobState::Cancelled) {
                    let mut kernel = kernel_job(tx, &job)?;
                    kernel
                        .transition(KernelEvent::ConfirmCleanup { generation: job.generation, claim_id: as_u64(claim_id)? }, now)
                        .map_err(kernel_error)?;
                    tx.execute("UPDATE board_attempts SET cleanup_state='confirmed' WHERE id=?1", params![attempt_id]).map_err(sql)?;
                    let job = update_from_kernel(tx, &job, &kernel, now)?;
                    insert_event(tx, &job.run_id, &job.id, job.version, "attempt.cleanup_confirmed", now).map(Some)
                } else {
                    Ok(None)
                }
            })
            .await?;

        Ok(())
    }

    /// Records an expired attempt as failed with a pending cleanup hold.
    ///
    /// # Errors
    /// Refuses a live lease, wrong version, or storage failure.
    pub async fn expire(&self, job_id: String, expected_version: u64) -> Result<JobSnapshot, Error> {
        let now = now_ms()?;
        let (job, _event) = self
            .ledger
            .transact(move |tx| {
                let current = load_job(tx, &job_id)?;
                let attempt = current.attempt.as_ref().ok_or(Error::StaleClaim)?;
                if current.version != expected_version
                    || now < attempt.lease_until_ms
                    || !matches!(current.state, JobState::Claimed | JobState::Running)
                {
                    return Err(Error::Conflict("attempt is not expired and active".into()));
                }
                let mut kernel = kernel_job(tx, &current)?;
                kernel.transition(KernelEvent::Expire { generation: current.generation }, now).map_err(kernel_error)?;
                tx.execute(
                    "UPDATE board_attempts SET state='failed',cleanup_state='pending',ended_ms=?1 WHERE id=?2",
                    params![as_i64(now)?, attempt.id],
                )
                .map_err(sql)?;
                let job = update_from_kernel(tx, &current, &kernel, now)?;
                let event = insert_event(tx, &job.run_id, &job.id, job.version, "attempt.expired", now)?;
                Ok((job, event))
            })
            .await?;

        Ok(job)
    }

    /// Reads bounded committed messages addressed to a job or written on that job.
    ///
    /// # Errors
    /// Returns a missing-job or ledger error.
    pub async fn messages(&self, job_id: String) -> Result<Vec<String>, Error> {
        self.ledger.transact(move |tx| {
            let job = load_job(tx,&job_id)?;
            let mut statement = tx.prepare(
                "SELECT sender,kind,body FROM board_messages WHERE run_id=?1 AND (job_id=?2 OR recipient=?2) ORDER BY created_ms,id LIMIT 100",
            ).map_err(sql)?;
            statement.query_map(params![job.run_id,job_id], |row| {
                Ok(format!("{} [{}]: {}",row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?))
            }).map_err(sql)?.collect::<Result<Vec<_>,_>>().map_err(sql)
        }).await
    }

    /// Post a job and its outbox event atomically. A stable key returns the original job.
    ///
    /// # Errors
    /// The contract, dependency run, or database rejected the post.
    pub async fn post(&self, params: PostParams) -> Result<PostResult, Error> {
        valid_spec(&params.spec)?;
        if params.idempotency_key.is_empty() || params.idempotency_key.len() > 256 {
            return Err(Error::Invalid("idempotency key is empty or too long".into()));
        }
        let now = now_ms()?;
        let (result, _event) = self.ledger.transact(move |tx| {
            let _fresh = KernelJob::new(params.spec.max_retries);
            let spec_json = encode(&params.spec)?;
            let prior: Option<(String, String)> = tx
                .query_row("SELECT id,contract_json FROM board_jobs WHERE post_key=?1", params![params.idempotency_key], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional().map_err(sql)?;
            if let Some((id, old_spec)) = prior {
                if old_spec != spec_json {
                    return Err(Error::Conflict("idempotency key reused for a different contract".into()));
                }
                let job = load_job(tx, &id)?;
                if params.run_id.as_deref().is_some_and(|run| run != job.run_id) {
                    return Err(Error::Conflict("idempotency key reused in a different run".into()));
                }
                return Ok((PostResult { job }, None));
            }
            let run_id = params.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let run_exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM board_runs WHERE id=?1)", params![run_id], |row| row.get(0)).map_err(sql)?;
            if !run_exists {
                if params.spec.depends_on.is_empty() {
                    tx.execute(
                        "INSERT INTO board_runs (id,owner_session,workspace,created_ms) VALUES (?1,'local',?2,?3)",
                        params![run_id, params.spec.workspace.as_deref().unwrap_or(""), as_i64(now)?],
                    ).map_err(sql)?;
                } else {
                    return Err(Error::Invalid("dependencies require an existing run".into()));
                }
            }
            let id = Uuid::new_v4().to_string();
            for dependency in &params.spec.depends_on {
                let dependency_run: Option<String> = tx.query_row("SELECT run_id FROM board_jobs WHERE id=?1", params![dependency], |row| row.get(0)).optional().map_err(sql)?;
                if dependency_run.as_deref() != Some(run_id.as_str()) {
                    return Err(Error::Invalid("dependency is missing or belongs to another run".into()));
                }
            }
            tx.execute(
                "INSERT INTO board_jobs (id,run_id,title,contract_json,post_key,state,max_retries,created_ms,updated_ms) VALUES (?1,?2,?3,?4,?5,'posted',?6,?7,?7)",
                params![id, run_id, params.spec.title, spec_json, params.idempotency_key, params.spec.max_retries, as_i64(now)?],
            ).map_err(sql)?;
            for dependency in &params.spec.depends_on {
                tx.execute("INSERT INTO board_dependencies (job_id,dependency_id) VALUES (?1,?2)", params![id, dependency]).map_err(sql)?;
            }
            let job = load_job(tx, &id)?;
            let event = insert_event(tx, &run_id, &id, job.version, "job.posted", now)?;
            Ok((PostResult { job }, Some(event)))
        }).await?;

        Ok(result)
    }

    /// Assign a posted job to one worker without launching an attempt.
    ///
    /// # Errors
    /// A missing job, stale version, or invalid worker is rejected.
    pub async fn assign(&self, params: AssignParams) -> Result<JobSnapshot, Error> {
        if params.worker.is_empty() || params.worker.len() > 256 {
            return Err(Error::Invalid("worker name is empty or too long".into()));
        }
        let now = now_ms()?;
        let (job, _event) = self
            .ledger
            .transact(move |tx| {
                let current = load_job(tx, &params.job_id)?;
                if current.version != params.expected_version || current.state != JobState::Posted {
                    return Err(Error::Conflict("job is not posted at the expected version".into()));
                }
                let _verified = kernel_job(tx, &current)?;
                tx.execute(
                    "UPDATE board_jobs SET assignee=?1,version=version+1,updated_ms=?2 WHERE id=?3",
                    params![params.worker, as_i64(now)?, params.job_id],
                )
                .map_err(sql)?;
                let job = load_job(tx, &params.job_id)?;
                let event = insert_event(tx, &job.run_id, &job.id, job.version, "job.assigned", now)?;
                Ok((job, event))
            })
            .await?;

        Ok(job)
    }

    /// Register a stable worker capacity before it makes claims.
    ///
    /// # Errors
    /// Rejects an invalid worker or an attempt to change its registered capacity.
    pub async fn register_worker(&self, params: RegisterWorkerParams) -> Result<(), Error> {
        if params.worker.is_empty() || params.worker.len() > 256 || !(1..=64).contains(&params.capacity) {
            return Err(Error::Invalid("invalid worker registration".into()));
        }
        self.ledger
            .transact(move |tx| {
                let prior: Option<u32> = tx
                    .query_row("SELECT capacity FROM board_workers WHERE worker=?1", params![params.worker], |row| row.get(0))
                    .optional()
                    .map_err(sql)?;
                if let Some(prior) = prior {
                    if prior != params.capacity {
                        return Err(Error::Conflict("worker capacity is already registered".into()));
                    }
                } else {
                    tx.execute("INSERT INTO board_workers (worker,capacity) VALUES (?1,?2)", params![params.worker, params.capacity])
                        .map_err(sql)?;
                }
                Ok(())
            })
            .await
    }

    /// Claim a posted and dependency-ready job with its pre-registered capacity.
    ///
    /// # Errors
    /// An ineligible worker, capacity limit, dependency gate, or concurrent claim is rejected.
    pub async fn claim(&self, params: ClaimParams) -> Result<ClaimResult, Error> {
        if params.worker.is_empty()
            || params.worker.len() > 256
            || params.capacity == 0
            || params.lease_ms == 0
            || params.idempotency_key.is_empty()
            || params.idempotency_key.len() > 256
            || Uuid::parse_str(&params.attempt_id).is_err()
        {
            return Err(Error::Invalid("invalid worker, capacity, or lease".into()));
        }
        let hash = parse_hash(&params.token_sha256)?;
        let now = now_ms()?;
        let lease_until = now.checked_add(params.lease_ms.min(300_000)).ok_or_else(|| Error::Invalid("lease overflow".into()))?;
        let (job, attempt, _event) = self.ledger.transact(move |tx| {
            let capacity: Option<u32> = tx.query_row("SELECT capacity FROM board_workers WHERE worker=?1",params![params.worker],
                |row|row.get(0)).optional().map_err(sql)?;
            if capacity != Some(params.capacity) { return Err(Error::Conflict("worker capacity is not registered".into())); }
            let prior: Option<(String,String,String,Vec<u8>)> = tx.query_row(
                "SELECT id,job_id,assignee,token_hash FROM board_attempts WHERE claim_key=?1",
                params![params.idempotency_key],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            ).optional().map_err(sql)?;
            if let Some((attempt_id,job_id,worker,stored)) = prior {
                if attempt_id != params.attempt_id || job_id != params.job_id || worker != params.worker || !same_hash(&stored,&hash) {
                    return Err(Error::Conflict("claim key was reused with different identity".into()));
                }
                let job = load_job(tx,&job_id)?;
                let attempt = job.attempt.clone().filter(|attempt| attempt.id == attempt_id)
                    .ok_or_else(|| Error::Conflict("claim was already retired".into()))?;
                return Ok((job,attempt,None));
            }
            let current = load_job(tx, &params.job_id)?;
            if current.state != JobState::Posted || params.expected_version.is_some_and(|version| version != current.version) {
                return Err(Error::Conflict("job is no longer claimable".into()));
            }
            if current.assigned_worker.as_deref().is_some_and(|worker| worker != params.worker) {
                return Err(Error::Conflict("job is assigned to another worker".into()));
            }
            let dependency_ids = {
                let mut query = tx.prepare("SELECT dependency_id FROM board_dependencies WHERE job_id=?1 ORDER BY dependency_id").map_err(sql)?;
                query.query_map(params![params.job_id], |row| row.get::<_,String>(0)).map_err(sql)?
                    .collect::<Result<Vec<_>,_>>().map_err(sql)?
            };
            let deps = dependency_ids.iter().map(|id| kernel_job(tx,&load_job(tx,id)?)).collect::<Result<Vec<_>,_>>()?;
            // The kernel counts only this worker's current Active/Pending attempts. An indexed
            // projection selects that complete set without restoring unrelated ledger history.
            let held_job_ids = {
                let mut query = tx.prepare(
                    "SELECT j.id FROM board_attempts a JOIN board_jobs j ON j.id=a.job_id AND j.generation=a.generation \
                     WHERE a.assignee=?1 AND a.cleanup_state!='confirmed'",
                ).map_err(sql)?;
                query.query_map(params![params.worker],|row|row.get::<_,String>(0)).map_err(sql)?
                    .collect::<Result<Vec<_>,_>>().map_err(sql)?
            };
            let others = held_job_ids.iter().map(|id| kernel_job(tx,&load_job(tx,id)?)).collect::<Result<Vec<_>,_>>()?;
            let generation = current.generation;
            let claim_id = u64::from(generation).saturating_add(1);
            let mut kernel = kernel_job(tx,&current)?;
            aim_kernel::board::claim_admitted(
                &mut kernel,&deps,&others,worker_id(&params.worker),usize::try_from(capacity.ok_or(Error::Conflict("worker not registered".into()))?).map_err(sql)?,
                claim_id,lease_until,now,
            ).map_err(|err| Error::Conflict(format!("claim admission: {err:?}")))?;
            let attempt_id = params.attempt_id.clone();
            tx.execute(
                "INSERT INTO board_attempts (id,job_id,generation,assignee,claim_id,token_hash,claim_key,lease_until_ms,state,launch_intent) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'claimed',1)",
                params![attempt_id, current.id, generation, params.worker, as_i64(claim_id)?, hash.as_slice(),params.idempotency_key, as_i64(lease_until)?],
            ).map_err(sql)?;
            let job = update_from_kernel(tx, &current, &kernel, now)?;
            let attempt = job.attempt.clone().ok_or_else(|| Error::Storage("claimed attempt missing".into()))?;
            let event = insert_event(tx, &job.run_id, &job.id, job.version, "job.claimed", now)?;
            Ok((job,attempt,Some(event)))
        }).await?;

        Ok(ClaimResult { job, attempt })
    }

    /// Start or renew an attempt. Heartbeat sequence must increase.
    ///
    /// # Errors
    /// Stale tokens, expired leases, and non-increasing sequences are rejected.
    pub async fn heartbeat(&self, params: HeartbeatParams) -> Result<JobSnapshot, Error> {
        if params.lease_ms == 0 {
            return Err(Error::Invalid("lease must be positive".into()));
        }
        let now = now_ms()?;
        let (job,_event) = self.ledger.transact(move |tx| {
            let current = load_job(tx, &params.job_id)?;
            let claim = claim_row(tx, &current, &params.attempt_id, &params.claim_token, now)?;
            if !matches!(claim.state, JobState::Claimed | JobState::Running) || params.sequence <= claim.heartbeat_seq {
                return Err(Error::Conflict("attempt cannot accept heartbeat sequence".into()));
            }
            let lease_until = now.checked_add(params.lease_ms.min(300_000)).ok_or_else(|| Error::Invalid("lease overflow".into()))?
                .max(claim.lease_until_ms);
            let mut kernel = kernel_job(tx,&current)?;
            if claim.state == JobState::Claimed {
                kernel.transition(KernelEvent::Start {generation:claim.generation,claim_id:claim.claim_id},now).map_err(kernel_error)?;
            }
            kernel.transition(KernelEvent::Heartbeat {generation:claim.generation,claim_id:claim.claim_id,lease_until},now).map_err(kernel_error)?;
            tx.execute(
                "UPDATE board_attempts SET heartbeat_seq=?1,lease_until_ms=?2,state='running',started_ms=COALESCE(started_ms,?3) WHERE id=?4",
                params![as_i64(params.sequence)?,as_i64(lease_until)?,as_i64(now)?,params.attempt_id],
            ).map_err(sql)?;
            let job = update_from_kernel(tx,&current,&kernel,now)?;
            let event = insert_event(tx,&job.run_id,&job.id,job.version,"attempt.heartbeat",now)?;
            Ok((job,event))
        }).await?;

        Ok(job)
    }

    /// Persist a bounded message under a live fenced attempt.
    ///
    /// # Errors
    /// Invalid body, stale token, or conflicting idempotency key is rejected.
    pub async fn message(&self, params: MessageParams) -> Result<MessageResult, Error> {
        if params.body.is_empty()
            || params.body.len() > 65_536
            || params.kind.is_empty()
            || params.kind.len() > 64
            || params.recipient.is_empty()
            || params.recipient.len() > 256
            || params.idempotency_key.is_empty()
            || params.idempotency_key.len() > 256
        {
            return Err(Error::Invalid("message field is empty or exceeds its bound".into()));
        }
        let now = now_ms()?;
        let (receipt,_event) = self.ledger.transact(move |tx| {
            let current = load_job(tx,&params.job_id)?;
            let claim = claim_row(tx,&current,&params.attempt_id,&params.claim_token,now)?;
            if !matches!(claim.state,JobState::Claimed | JobState::Running) { return Err(Error::Conflict("attempt cannot send messages".into())); }
            if !kernel_job(tx,&current)?.authorizes_attempt(claim.generation,claim.claim_id,now) {
                return Err(Error::StaleClaim);
            }
            let prior: Option<(String,String,String,String)> = tx.query_row(
                "SELECT id,recipient,kind,body FROM board_messages WHERE job_id=?1 AND idempotency_key=?2",
                params![params.job_id,params.idempotency_key],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            ).optional().map_err(sql)?;
            if let Some((id,recipient,kind,body)) = prior {
                if recipient != params.recipient || kind != params.kind || body != params.body { return Err(Error::Conflict("message key reused with different bytes".into())); }
                let seq: i64 = tx.query_row("SELECT seq FROM board_outbox WHERE kind='message.posted' AND payload_json=?1",params![id],|row|row.get(0)).map_err(sql)?;
                return Ok((MessageResult {id,seq:as_u64(seq)?},None));
            }
            let id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO board_messages (id,run_id,job_id,attempt_id,sender,recipient,kind,body,idempotency_key,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![id,current.run_id,current.id,params.attempt_id,claim.worker,params.recipient,params.kind,params.body,params.idempotency_key,as_i64(now)?],
            ).map_err(sql)?;
            let event = insert_event(tx,&current.run_id,&current.id,current.version,"message.posted",now)?;
            tx.execute("UPDATE board_outbox SET payload_json=?1 WHERE event_id=?2",params![id,event.id]).map_err(sql)?;
            Ok((MessageResult{id,seq:event.seq},Some(event)))
        }).await?;

        Ok(receipt)
    }

    /// Finish execution and retain immutable content-hashed artifacts; review remains pending.
    ///
    /// # Errors
    /// Invalid or expired claim, artifact size, or execution state is rejected.
    pub async fn complete(&self, params: CompleteParams) -> Result<JobSnapshot, Error> {
        if params.artifacts.len() > 32
            || params.artifacts.iter().map(|artifact| artifact.data.0.len()).sum::<usize>() > 24 * 1024 * 1024
            || params.artifacts.iter().any(|artifact| artifact.data.0.len() > 1_048_576 || artifact.media_type.len() > 128)
        {
            return Err(Error::Invalid("artifact limit exceeded".into()));
        }
        let now = now_ms()?;
        let (job,_event) = self.ledger.transact(move |tx| {
            let current = load_job(tx,&params.job_id)?;
            let claim = claim_row(tx,&current,&params.attempt_id,&params.claim_token,now)?;
            if !matches!(claim.state,JobState::Claimed | JobState::Running) { return Err(Error::Conflict("attempt is not live".into())); }
            let mut kernel = kernel_job(tx,&current)?;
            if claim.state == JobState::Claimed {
                kernel.transition(KernelEvent::Start {generation:claim.generation,claim_id:claim.claim_id},now).map_err(kernel_error)?;
            }
            kernel.transition(KernelEvent::Complete {generation:claim.generation,claim_id:claim.claim_id},now).map_err(kernel_error)?;
            let cleanup_confirmed = cleanup_recorded(tx, &params.attempt_id)?;
            if cleanup_confirmed {
                kernel.transition(KernelEvent::ConfirmCleanup {generation:claim.generation,claim_id:claim.claim_id},now)
                    .map_err(kernel_error)?;
            }
            for artifact in &params.artifacts {
                let digest = Sha256::digest(&artifact.data.0);
                let hex = hex_bytes(&digest);
                let id = format!("{}:{hex}",params.attempt_id);
                tx.execute(
                    "INSERT OR IGNORE INTO board_artifacts (id,job_id,attempt_id,uri,media_type,byte_size,sha256,data,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![id,current.id,params.attempt_id,format!("sha256:{hex}"),artifact.media_type,as_i64(u64::try_from(artifact.data.0.len()).map_err(sql)?)?,digest.as_slice(),artifact.data.0,as_i64(now)?],
                ).map_err(sql)?;
            }
            tx.execute(
                "UPDATE board_attempts SET state='succeeded',cleanup_state=?1,ended_ms=?2 WHERE id=?3",
                params![if cleanup_confirmed { "confirmed" } else { "pending" },as_i64(now)?,params.attempt_id],
            ).map_err(sql)?;
            let job = update_from_kernel(tx,&current,&kernel,now)?;
            let event = insert_event(tx,&job.run_id,&job.id,job.version,"job.succeeded",now)?;
            Ok((job,event))
        }).await?;

        Ok(job)
    }

    /// Fail an attempt and record whether external cleanup was confirmed.
    ///
    /// # Errors
    /// A stale token or a non-live attempt is rejected.
    pub async fn fail(&self, params: FailParams) -> Result<JobSnapshot, Error> {
        if params.reason.is_empty() || params.reason.len() > 256 {
            return Err(Error::Invalid("failure class is empty or too long".into()));
        }
        let now = now_ms()?;
        let (job, _event) = self
            .ledger
            .transact(move |tx| {
                let current = load_job(tx, &params.job_id)?;
                let claim = claim_row(tx, &current, &params.attempt_id, &params.claim_token, now)?;
                let cleanup_confirmed = cleanup_recorded(tx, &params.attempt_id)?;
                if !matches!(claim.state, JobState::Claimed | JobState::Running) {
                    return Err(Error::Conflict("attempt is not live".into()));
                }
                let mut kernel = kernel_job(tx, &current)?;
                if claim.state == JobState::Claimed {
                    kernel
                        .transition(KernelEvent::Start { generation: claim.generation, claim_id: claim.claim_id }, now)
                        .map_err(kernel_error)?;
                }
                kernel
                    .transition(KernelEvent::Fail { generation: claim.generation, claim_id: claim.claim_id, cleanup_confirmed }, now)
                    .map_err(kernel_error)?;
                let cleanup = if cleanup_confirmed { "confirmed" } else { "pending" };
                tx.execute(
                    "UPDATE board_attempts SET state='failed',cleanup_state=?1,ended_ms=?2,failure_reason=?3 WHERE id=?4",
                    params![cleanup, as_i64(now)?, params.reason, params.attempt_id],
                )
                .map_err(sql)?;
                let job = update_from_kernel(tx, &current, &kernel, now)?;
                let event = insert_event(tx, &job.run_id, &job.id, job.version, "job.failed", now)?;
                Ok((job, event))
            })
            .await?;

        Ok(job)
    }

    /// Cancel a job and persist cleanup intent before any external stop action.
    ///
    /// # Errors
    /// A stale version or terminal job is rejected.
    pub async fn cancel(&self, params: CancelParams) -> Result<JobSnapshot, Error> {
        let now = now_ms()?;
        let (job, _event) = self
            .ledger
            .transact(move |tx| {
                let current = load_job(tx, &params.job_id)?;
                if current.version != params.expected_version
                    || matches!(current.state, JobState::Succeeded | JobState::Failed | JobState::Cancelled)
                {
                    return Err(Error::Conflict("job is terminal or version changed".into()));
                }
                let mut kernel = kernel_job(tx, &current)?;
                kernel.transition(KernelEvent::Cancel, now).map_err(kernel_error)?;
                if let Some(attempt) = &current.attempt {
                    tx.execute(
                        "UPDATE board_attempts SET state='cancelled',cleanup_state='pending',ended_ms=?1 WHERE id=?2",
                        params![as_i64(now)?, attempt.id],
                    )
                    .map_err(sql)?;
                }
                let job = update_from_kernel(tx, &current, &kernel, now)?;
                let event = insert_event(tx, &job.run_id, &job.id, job.version, "job.cancelled", now)?;
                Ok((job, event))
            })
            .await?;

        Ok(job)
    }

    /// Reopen a failed or rejected job after a bounded retry and confirmed cleanup.
    ///
    /// # Errors
    /// Stale version, unconfirmed cleanup, or exhausted budget is rejected.
    pub async fn retry(&self, params: RetryParams) -> Result<JobSnapshot, Error> {
        let now = now_ms()?;
        let (job, _event) = self
            .ledger
            .transact(move |tx| {
                let current = load_job(tx, &params.job_id)?;
                if current.version != params.expected_version {
                    return Err(Error::Conflict("job cannot retry at this version".into()));
                }
                let mut kernel = kernel_job(tx, &current)?;
                if !matches!(current.state, JobState::Failed | JobState::Cancelled) && current.review != ReviewState::Rejected {
                    return Err(Error::Conflict("job is not failed, cancelled, or rejected".into()));
                }
                kernel.transition(KernelEvent::Retry, now).map_err(kernel_error)?;
                let job = update_from_kernel(tx, &current, &kernel, now)?;
                let event = insert_event(tx, &job.run_id, &job.id, job.version, "job.retried", now)?;
                Ok((job, event))
            })
            .await?;

        Ok(job)
    }

    /// Review a successful attempt against its immutable evidence.
    ///
    /// # Errors
    /// A stale version, wrong attempt, or unknown evidence reference is rejected.
    pub async fn review(&self, params: ReviewParams) -> Result<JobSnapshot, Error> {
        if params.reviewer.is_empty() || params.reviewer.len() > 256 || params.evidence.len() > 64 {
            return Err(Error::Invalid("reviewer or evidence limit invalid".into()));
        }
        let now = now_ms()?;
        let (job,_event) = self.ledger.transact(move |tx| {
            let current = load_job(tx,&params.job_id)?;
            if current.version != params.expected_version || current.state != JobState::Succeeded || current.review != ReviewState::Pending
                || current.attempt.as_ref().is_none_or(|attempt| attempt.id != params.attempt_id) {
                return Err(Error::Conflict("job is not reviewable at this version".into()));
            }
            if current.attempt.as_ref().is_some_and(|attempt| attempt.worker.trim().to_lowercase() == params.reviewer.trim().to_lowercase()) {
                return Err(Error::Conflict("worker cannot review its own attempt".into()));
            }
            if params.accepted && params.evidence.is_empty() {
                return Err(Error::Invalid("acceptance requires artifact evidence".into()));
            }
            for evidence in &params.evidence {
                if !current.artifacts.iter().any(|artifact| artifact.id == *evidence) {
                    return Err(Error::Invalid("review evidence is not on this attempt".into()));
                }
                let (stored_hash,data): (Vec<u8>,Vec<u8>) = tx.query_row(
                    "SELECT sha256,data FROM board_artifacts WHERE id=?1 AND attempt_id=?2",
                    params![evidence,params.attempt_id],|row|Ok((row.get(0)?,row.get(1)?)),
                ).map_err(sql)?;
                if !same_hash(&stored_hash,&Sha256::digest(&data).into()) {
                    return Err(Error::Storage("review artifact hash does not match stored bytes".into()));
                }
            }
            let mut kernel = kernel_job(tx,&current)?;
            kernel.transition(KernelEvent::Review {accepted:params.accepted,evidence_present:!params.evidence.is_empty()},now).map_err(kernel_error)?;
            let decision = if params.accepted {ReviewState::Accepted} else {ReviewState::Rejected};
            let review_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO board_reviews (id,job_id,attempt_id,reviewer,decision,evidence_json,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![review_id,current.id,params.attempt_id,params.reviewer,review_name(decision),encode(&params.evidence)?,as_i64(now)?],
            ).map_err(sql)?;
            if params.accepted && let Some(work) = &current.spec.work {
                let branch = format!("board/{}/{}", current.id, params.attempt_id);
                tx.execute(
                    "INSERT INTO board_integrations (job_id,attempt_id,source_branch,target_branch,state,created_ms,updated_ms) VALUES (?1,?2,?3,?4,'queued',?5,?5)",
                    params![current.id,params.attempt_id,branch,work.target_branch,as_i64(now)?],
                ).map_err(sql)?;
            }
            let job = update_from_kernel(tx,&current,&kernel,now)?;
            let event = insert_event(tx,&job.run_id,&job.id,job.version,if params.accepted {"review.accepted"} else {"review.rejected"},now)?;
            Ok((job,event))
        }).await?;

        Ok(job)
    }

    /// List current authoritative snapshots with a stable page cursor.
    ///
    /// # Errors
    /// The database read failed.
    pub async fn list(&self, params: ListParams) -> Result<ListResult, Error> {
        let (jobs, next_job, truncated) = self
            .ledger
            .transact(move |tx| {
                list_jobs(tx, params.run_id.as_deref(), params.limit.unwrap_or(50).clamp(1, 200), params.after_job.as_deref())
            })
            .await?;
        Ok(ListResult { jobs, next_job, truncated })
    }

    /// Read one authoritative job snapshot.
    ///
    /// # Errors
    /// The job was not found or the database read failed.
    pub async fn show(&self, job_id: String) -> Result<JobSnapshot, Error> {
        self.ledger.transact(move |tx| load_job(tx, &job_id)).await
    }

    /// Reconcile a run with durable events after a cursor and current snapshots.
    ///
    /// # Errors
    /// The run was not found or the database read failed.
    pub async fn poll(&self, params: PollParams) -> Result<PollResult, Error> {
        self.ledger
            .transact(move |tx| {
                let exists: bool = tx
                    .query_row("SELECT EXISTS(SELECT 1 FROM board_runs WHERE id=?1)", params![params.run_id], |row| row.get(0))
                    .map_err(sql)?;
                if !exists {
                    return Err(Error::NotFound);
                }
                let (jobs, next_job, truncated) = list_jobs(tx, Some(&params.run_id), 200, params.after_job.as_deref())?;
                let mut statement = tx.prepare_cached(
                "SELECT seq,event_id,job_id,job_version,kind,created_ms FROM board_outbox WHERE run_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
            ).map_err(sql)?;
                let rows = statement
                    .query_map(params![params.run_id, as_i64(params.after_seq)?, params.limit.min(500)], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    })
                    .map_err(sql)?;
                let events = rows
                    .map(|row| {
                        let (seq, id, job_id, version, kind, ts_ms) = row.map_err(sql)?;
                        Ok(BoardEvent {
                            id,
                            run_id: params.run_id.clone(),
                            job_id,
                            seq: as_u64(seq)?,
                            version: as_u64(version)?,
                            kind,
                            ts_ms: as_u64(ts_ms)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                let next_seq = events.last().map_or(params.after_seq, |event| event.seq);
                Ok(PollResult { jobs, events, next_seq, next_job, truncated })
            })
            .await
    }

    /// Read a snapshot and cursor before consuming event hints from [`Self::subscribe`].
    ///
    /// # Errors
    /// The run was not found or the database read failed.
    pub async fn watch(&self, run_id: String, after_seq: u64) -> Result<PollResult, Error> {
        self.poll(PollParams { run_id, after_seq, limit: 500, after_job: None }).await
    }
}
