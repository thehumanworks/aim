//! Durable integration intent and result around external Git work (ADR 0048).

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use aim_proto::board::{BoardEvent, JobState, ReviewState};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::service::{insert_event, load_job, sql};
use super::{Board, Error};

/// Recorded outcome of an accepted attempt's integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    /// Review accepted; waiting for the serialized integrator.
    Queued,
    /// The target HEAD was pinned before Git side effects.
    Integrating,
    /// The merge and configured check passed.
    Integrated,
    /// Git left conflict files for manual resolution.
    Conflict,
    /// Git merge or the configured check failed without a conflict.
    Failed,
}

impl IntegrationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Integrating => "integrating",
            Self::Integrated => "integrated",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
        }
    }
}

/// Authoritative integration row and immutable result evidence ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationRecord {
    /// Job whose accepted attempt is being integrated.
    pub job_id: String,
    /// Accepted attempt.
    pub attempt_id: String,
    /// Attempt branch.
    pub source_branch: String,
    /// Target branch.
    pub target_branch: String,
    /// Current integration state.
    pub state: IntegrationState,
    /// Target commit pinned before merge.
    pub target_head: Option<String>,
    /// Accepted attempt commit pinned before merge.
    pub source_commit: Option<String>,
    /// Detached scratch worktree recorded before it is created.
    pub scratch_path: Option<String>,
    /// Checked merge result recorded before any target ref may move.
    pub result_commit: Option<String>,
    /// Owned ref retaining an unapplied result across scratch cleanup.
    pub rescue_ref: Option<String>,
    /// Conflicted paths; never resolved by the integrator.
    pub conflict_files: Vec<String>,
    /// Content-hashed integration result artifact.
    pub artifact_id: Option<String>,
}

type IntegrationRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
);

fn now_ms() -> Result<i64, Error> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).map_err(sql)?;
    i64::try_from(duration.as_millis()).map_err(sql)
}

fn decode_state(value: &str) -> Result<IntegrationState, Error> {
    match value {
        "queued" => Ok(IntegrationState::Queued),
        "integrating" => Ok(IntegrationState::Integrating),
        "integrated" => Ok(IntegrationState::Integrated),
        "conflict" => Ok(IntegrationState::Conflict),
        "failed" => Ok(IntegrationState::Failed),
        _ => Err(Error::Storage("unknown integration state".into())),
    }
}

fn row(tx: &rusqlite::Transaction<'_>, job_id: &str) -> Result<IntegrationRecord, Error> {
    let raw: Option<IntegrationRow> = tx
        .query_row(
            "SELECT job_id,attempt_id,source_branch,target_branch,state,target_head,source_commit,scratch_path,result_commit,rescue_ref,conflicts_json,artifact_id FROM board_integrations WHERE job_id=?1",
            params![job_id],
            |record| Ok((record.get(0)?,record.get(1)?,record.get(2)?,record.get(3)?,record.get(4)?,record.get(5)?,record.get(6)?,record.get(7)?,record.get(8)?,record.get(9)?,record.get(10)?,record.get(11)?)),
        )
        .optional().map_err(sql)?;
    let (
        job_id,
        attempt_id,
        source_branch,
        target_branch,
        state,
        target_head,
        source_commit,
        scratch_path,
        result_commit,
        rescue_ref,
        conflicts_json,
        artifact_id,
    ) = raw.ok_or(Error::NotFound)?;
    Ok(IntegrationRecord {
        job_id,
        attempt_id,
        source_branch,
        target_branch,
        state: decode_state(&state)?,
        target_head,
        source_commit,
        scratch_path,
        result_commit,
        rescue_ref,
        conflict_files: serde_json::from_str(&conflicts_json).map_err(sql)?,
        artifact_id,
    })
}

fn store_result_evidence(
    tx: &rusqlite::Transaction<'_>,
    job_id: &str,
    attempt_id: &str,
    evidence: &[u8],
    now: i64,
) -> Result<String, Error> {
    let hash = Sha256::digest(evidence);
    let mut hex = String::with_capacity(64);
    for byte in &hash {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    let artifact_id = format!("{attempt_id}:{hex}");
    tx.execute(
        "INSERT OR IGNORE INTO board_artifacts (id,job_id,attempt_id,uri,media_type,byte_size,sha256,data,created_ms) \
         VALUES (?1,?2,?3,?4,'application/vnd.aim.board-integration+json',?5,?6,?7,?8)",
        params![
            artifact_id,
            job_id,
            attempt_id,
            format!("sha256:{hex}"),
            i64::try_from(evidence.len()).map_err(sql)?,
            hash.as_slice(),
            evidence,
            now
        ],
    )
    .map_err(sql)?;
    Ok(artifact_id)
}

impl Board {
    /// Lists accepted integration jobs waiting for the serialized integrator.
    ///
    /// # Errors
    /// Returns a ledger error.
    pub async fn queued_integrations(&self) -> Result<Vec<String>, Error> {
        self.ledger
            .transact(|tx| {
                let mut query = tx
                    .prepare("SELECT job_id FROM board_integrations WHERE state='queued' ORDER BY created_ms,job_id LIMIT 200")
                    .map_err(sql)?;
                query.query_map([], |row| row.get::<_, String>(0)).map_err(sql)?.collect::<Result<Vec<_>, _>>().map_err(sql)
            })
            .await
    }

    /// Reads the durable integration state of one job.
    ///
    /// # Errors
    /// Returns not-found or a ledger error.
    pub async fn integration(&self, job_id: String) -> Result<IntegrationRecord, Error> {
        self.ledger.transact(move |tx| row(tx, &job_id)).await
    }

    /// Lists intents and completed results whose owned scratch or rescue ref may need cleanup.
    ///
    /// # Errors
    /// Returns a ledger read error.
    pub async fn recoverable_integrations(&self) -> Result<Vec<IntegrationRecord>, Error> {
        self.ledger
            .transact(|tx| {
                let mut query = tx
                    .prepare(
                        "SELECT job_id FROM board_integrations WHERE state='integrating' OR scratch_path IS NOT NULL \
                         OR rescue_ref IS NOT NULL ORDER BY created_ms,job_id",
                    )
                    .map_err(sql)?;
                let jobs =
                    query.query_map([], |entry| entry.get::<_, String>(0)).map_err(sql)?.collect::<Result<Vec<_>, _>>().map_err(sql)?;
                jobs.iter().map(|id| row(tx, id)).collect()
            })
            .await
    }

    /// Lists failed merge results retained for an explicit user fast-forward.
    ///
    /// # Errors
    /// Returns a ledger read error.
    pub async fn pending_apply_integrations(&self) -> Result<Vec<IntegrationRecord>, Error> {
        self.ledger
            .transact(|tx| {
                let mut query = tx
                    .prepare(
                        "SELECT job_id FROM board_integrations WHERE state='failed' AND result_commit IS NOT NULL \
                         AND rescue_ref IS NOT NULL ORDER BY created_ms,job_id",
                    )
                    .map_err(sql)?;
                let jobs =
                    query.query_map([], |entry| entry.get::<_, String>(0)).map_err(sql)?.collect::<Result<Vec<_>, _>>().map_err(sql)?;
                jobs.iter().map(|id| row(tx, id)).collect()
            })
            .await
    }

    /// Pins the two commits and records intent before Git changes the target checkout.
    ///
    /// # Errors
    /// Refuses unaccepted, stale, or already-running integration.
    pub async fn begin_integration(
        &self,
        job_id: String,
        target_head: String,
        source_commit: String,
        scratch_path: String,
    ) -> Result<IntegrationRecord, Error> {
        if target_head.len() != 40
            || source_commit.len() != 40
            || !target_head.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !source_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Error::Invalid("Git commits must be 40 hexadecimal characters".into()));
        }
        if !scratch_path.starts_with('/') || scratch_path.len() > 4096 {
            return Err(Error::Invalid("integration scratch path must be absolute and bounded".into()));
        }
        let now = now_ms()?;
        let (record, _event) = self.ledger.transact(move |tx| {
            let job = load_job(tx, &job_id)?;
            if job.state != JobState::Succeeded || job.review != ReviewState::Accepted {
                return Err(Error::Conflict("attempt has not been accepted".into()));
            }
            let prior = row(tx, &job_id)?;
            if prior.state != IntegrationState::Queued || job.attempt.as_ref().is_none_or(|attempt| attempt.id != prior.attempt_id) {
                return Err(Error::Conflict("integration is not queued for this attempt".into()));
            }
            tx.execute(
                "UPDATE board_integrations SET state='integrating',target_head=?1,source_commit=?2,scratch_path=?3,result_commit=NULL,rescue_ref=NULL,updated_ms=?4 WHERE job_id=?5 AND state='queued'",
                params![target_head,source_commit,scratch_path,now,job_id],
            ).map_err(sql)?;
            let event = insert_event(tx,&job.run_id,&job.id,job.version,"integration.started",u64::try_from(now).map_err(sql)?)?;
            Ok((row(tx,&job_id)?,event))
        }).await?;

        Ok(record)
    }

    /// Saves a checked merge result and its intended rescue ref before either Git ref moves.
    ///
    /// # Errors
    /// Refuses mismatched or unpinned result intent.
    pub async fn record_integration_result(
        &self,
        job_id: String,
        result_commit: String,
        rescue_ref: String,
    ) -> Result<IntegrationRecord, Error> {
        if result_commit.len() != 40
            || !result_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            || rescue_ref != format!("refs/aim/rescue/{job_id}")
        {
            return Err(Error::Invalid("invalid result commit or rescue ref".into()));
        }
        let now = now_ms()?;
        self.ledger
            .transact(move |tx| {
                let prior = row(tx, &job_id)?;
                if prior.state != IntegrationState::Integrating {
                    return Err(Error::Conflict("integration is not running".into()));
                }
                if prior.result_commit.as_deref() == Some(result_commit.as_str())
                    && prior.rescue_ref.as_deref() == Some(rescue_ref.as_str())
                {
                    return Ok(prior);
                }
                if prior.result_commit.is_some() || prior.rescue_ref.is_some() {
                    return Err(Error::Conflict("integration result changed".into()));
                }
                tx.execute(
                    "UPDATE board_integrations SET result_commit=?1,rescue_ref=?2,updated_ms=?3 \
                     WHERE job_id=?4 AND state='integrating' AND result_commit IS NULL",
                    params![result_commit, rescue_ref, now, job_id],
                )
                .map_err(sql)?;
                row(tx, &job_id)
            })
            .await
    }

    /// Requeues an incomplete scratch operation when no result or target ref move was recorded.
    ///
    /// # Errors
    /// Refuses an already computed result or a terminal integration.
    pub async fn requeue_integration(&self, job_id: String) -> Result<IntegrationRecord, Error> {
        let now = now_ms()?;
        self.ledger
            .transact(move |tx| {
                let prior = row(tx, &job_id)?;
                if prior.state != IntegrationState::Integrating || prior.result_commit.is_some() {
                    return Err(Error::Conflict("integration cannot be retried".into()));
                }
                tx.execute(
                    "UPDATE board_integrations SET state='queued',target_head=NULL,source_commit=NULL,scratch_path=NULL,updated_ms=?1 \
                     WHERE job_id=?2 AND state='integrating' AND result_commit IS NULL",
                    params![now, job_id],
                )
                .map_err(sql)?;
                row(tx, &job_id)
            })
            .await
    }

    /// Clears the recorded scratch path after the owned Git worktree is removed or found absent.
    ///
    /// # Errors
    /// Returns a ledger write error.
    pub async fn clear_integration_scratch(&self, job_id: String) -> Result<IntegrationRecord, Error> {
        let now = now_ms()?;
        self.ledger
            .transact(move |tx| {
                tx.execute("UPDATE board_integrations SET scratch_path=NULL,updated_ms=?1 WHERE job_id=?2", params![now, job_id])
                    .map_err(sql)?;
                row(tx, &job_id)
            })
            .await
    }

    /// Clears an owned rescue-ref name after its Git ref was deleted on integration or disposal.
    ///
    /// # Errors
    /// Refuses a still-running integration or a missing row.
    pub async fn clear_integration_rescue(&self, job_id: String) -> Result<IntegrationRecord, Error> {
        let now = now_ms()?;
        self.ledger
            .transact(move |tx| {
                let prior = row(tx, &job_id)?;
                if !matches!(prior.state, IntegrationState::Integrated | IntegrationState::Failed) {
                    return Err(Error::Conflict("integration rescue is still active".into()));
                }
                tx.execute("UPDATE board_integrations SET rescue_ref=NULL,updated_ms=?1 WHERE job_id=?2", params![now, job_id])
                    .map_err(sql)?;
                row(tx, &job_id)
            })
            .await
    }

    /// Records a merge/check outcome and its immutable evidence in the same transaction.
    ///
    /// # Errors
    /// Refuses nonterminal results, a stale attempt, or oversized evidence.
    pub async fn finish_integration(
        &self,
        job_id: String,
        state: IntegrationState,
        conflict_files: Vec<String>,
        evidence: Vec<u8>,
    ) -> Result<IntegrationRecord, Error> {
        if !matches!(state, IntegrationState::Integrated | IntegrationState::Conflict | IntegrationState::Failed)
            || evidence.len() > 1_048_576
            || conflict_files.len() > 256
        {
            return Err(Error::Invalid("invalid integration outcome or evidence bound".into()));
        }
        let now = now_ms()?;
        let (record,_event): (IntegrationRecord,BoardEvent) = self.ledger.transact(move |tx| {
            let job = load_job(tx,&job_id)?;
            let prior = row(tx,&job_id)?;
            if prior.state != IntegrationState::Integrating || job.attempt.as_ref().is_none_or(|attempt| attempt.id != prior.attempt_id) {
                return Err(Error::Conflict("integration intent is stale".into()));
            }
            if state == IntegrationState::Integrated && prior.result_commit.is_none() {
                return Err(Error::Conflict("integration result was not persisted".into()));
            }
            let artifact_id = store_result_evidence(tx, &job.id, &prior.attempt_id, &evidence, now)?;
            tx.execute(
                "UPDATE board_integrations SET state=?1,conflicts_json=?2,artifact_id=?3,updated_ms=?4 WHERE job_id=?5 AND state='integrating'",
                params![state.as_str(),serde_json::to_string(&conflict_files).map_err(sql)?,artifact_id,now,job_id],
            ).map_err(sql)?;
            let kind = match state { IntegrationState::Integrated => "integration.integrated", IntegrationState::Conflict => "integration.conflict", IntegrationState::Failed => "integration.failed", _ => return Err(Error::Invalid("integration result is not terminal".into())) };
            let event = insert_event(tx,&job.run_id,&job.id,job.version,kind,u64::try_from(now).map_err(sql)?)?;
            Ok((row(tx,&job_id)?,event))
        }).await?;

        Ok(record)
    }

    /// Records a user-requested fast-forward after the observed target equals the saved result.
    ///
    /// # Errors
    /// Refuses an unaccepted or changed result, missing rescue, or oversized evidence.
    pub async fn mark_integration_applied(
        &self,
        job_id: String,
        observed_target: String,
        evidence: Vec<u8>,
    ) -> Result<IntegrationRecord, Error> {
        if evidence.len() > 1_048_576 || observed_target.len() != 40 {
            return Err(Error::Invalid("invalid applied result or evidence bound".into()));
        }
        let now = now_ms()?;
        let (record, _event): (IntegrationRecord, BoardEvent) = self
            .ledger
            .transact(move |tx| {
                let job = load_job(tx, &job_id)?;
                let prior = row(tx, &job_id)?;
                if prior.state != IntegrationState::Failed
                    || prior.result_commit.as_deref() != Some(observed_target.as_str())
                    || prior.rescue_ref.is_none()
                    || job.attempt.as_ref().is_none_or(|attempt| attempt.id != prior.attempt_id)
                {
                    return Err(Error::Conflict("applied integration does not match retained result".into()));
                }
                let artifact_id = store_result_evidence(tx, &job.id, &prior.attempt_id, &evidence, now)?;
                tx.execute(
                    "UPDATE board_integrations SET state='integrated',artifact_id=?1,updated_ms=?2 \
                     WHERE job_id=?3 AND state='failed' AND result_commit=?4",
                    params![artifact_id, now, job_id, observed_target],
                )
                .map_err(sql)?;
                let event = insert_event(tx, &job.run_id, &job.id, job.version, "integration.applied", u64::try_from(now).map_err(sql)?)?;
                Ok((row(tx, &job_id)?, event))
            })
            .await?;
        Ok(record)
    }
}
