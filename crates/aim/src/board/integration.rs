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
    /// Conflicted paths; never resolved by the integrator.
    pub conflict_files: Vec<String>,
    /// Content-hashed integration result artifact.
    pub artifact_id: Option<String>,
}

type IntegrationRow = (String, String, String, String, String, Option<String>, Option<String>, String, Option<String>);

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
            "SELECT job_id,attempt_id,source_branch,target_branch,state,target_head,source_commit,conflicts_json,artifact_id FROM board_integrations WHERE job_id=?1",
            params![job_id],
            |record| Ok((record.get(0)?,record.get(1)?,record.get(2)?,record.get(3)?,record.get(4)?,record.get(5)?,record.get(6)?,record.get(7)?,record.get(8)?)),
        )
        .optional().map_err(sql)?;
    let (job_id, attempt_id, source_branch, target_branch, state, target_head, source_commit, conflicts_json, artifact_id) =
        raw.ok_or(Error::NotFound)?;
    Ok(IntegrationRecord {
        job_id,
        attempt_id,
        source_branch,
        target_branch,
        state: decode_state(&state)?,
        target_head,
        source_commit,
        conflict_files: serde_json::from_str(&conflicts_json).map_err(sql)?,
        artifact_id,
    })
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

    /// Pins the two commits and records intent before Git changes the target checkout.
    ///
    /// # Errors
    /// Refuses unaccepted, stale, or already-running integration.
    pub async fn begin_integration(&self, job_id: String, target_head: String, source_commit: String) -> Result<IntegrationRecord, Error> {
        if target_head.len() != 40 || source_commit.len() != 40 {
            return Err(Error::Invalid("Git commits must be 40 hexadecimal characters".into()));
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
                "UPDATE board_integrations SET state='integrating',target_head=?1,source_commit=?2,updated_ms=?3 WHERE job_id=?4 AND state='queued'",
                params![target_head,source_commit,now,job_id],
            ).map_err(sql)?;
            let event = insert_event(tx,&job.run_id,&job.id,job.version,"integration.started",u64::try_from(now).map_err(sql)?)?;
            Ok((row(tx,&job_id)?,event))
        }).await?;

        Ok(record)
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
            let hash = Sha256::digest(&evidence);
            let mut hex = String::with_capacity(64);
            for byte in &hash { let _ = write!(&mut hex,"{byte:02x}"); }
            let artifact_id = format!("{}:{hex}",prior.attempt_id);
            tx.execute(
                "INSERT OR IGNORE INTO board_artifacts (id,job_id,attempt_id,uri,media_type,byte_size,sha256,data,created_ms) VALUES (?1,?2,?3,?4,'application/vnd.aim.board-integration+json',?5,?6,?7,?8)",
                params![artifact_id,job.id,prior.attempt_id,format!("sha256:{hex}"),i64::try_from(evidence.len()).map_err(sql)?,hash.as_slice(),evidence,now],
            ).map_err(sql)?;
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
}
