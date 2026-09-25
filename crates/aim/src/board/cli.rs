//! Human and JSON board commands over the local daemon.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aim_proto::board::{
    ArtifactInput, BoardEvent, ClaimParams, CompleteParams, JobRef, JobSnapshot, JobSpec, ListParams, PollParams, PostParams, ReviewParams,
    WatchParams,
};
use aim_proto::content::Base64Bytes;
use clap::{Subcommand, ValueEnum};
use serde::Serialize;

use crate::daemon::spawn::connect_or_spawn;

/// `aim board` commands. Claim secrets are stored under `AIM_HOME/run/board-claims` and never printed.
#[derive(Subcommand)]
pub enum BoardAction {
    /// Post a job contract to a new or existing run.
    Post {
        /// Job title.
        title: String,
        /// Concrete deliverable.
        #[arg(long)]
        deliverable: String,
        /// Acceptance criterion (repeatable).
        #[arg(long)]
        acceptance: Vec<String>,
        /// Existing run ID; absent creates a run.
        #[arg(long)]
        run: Option<String>,
        /// Accepted predecessor job ID (repeatable).
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        /// Additional attempt budget.
        #[arg(long, default_value_t = 1)]
        max_retries: u32,
        /// Emit JSON rather than a short human summary.
        #[arg(long)]
        json: bool,
    },
    /// List recent jobs or a run's jobs.
    List {
        /// Restrict to this run.
        #[arg(long)]
        run: Option<String>,
        /// Maximum jobs.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one job with current artifact IDs.
    Show {
        /// Job ID.
        job_id: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Claim a ready job; saves the one-time token privately.
    Claim {
        /// Job ID.
        job_id: String,
        /// Worker identity.
        #[arg(long)]
        worker: String,
        /// Declared concurrent capacity.
        #[arg(long, default_value_t = 1)]
        capacity: u32,
        /// Requested lease in seconds.
        #[arg(long, default_value_t = 60)]
        lease_seconds: u64,
        /// Emit JSON without the claim token.
        #[arg(long)]
        json: bool,
    },
    /// Complete a claimed attempt with content-hashed evidence.
    Complete {
        /// Job ID.
        job_id: String,
        /// Attempt ID returned by claim.
        attempt_id: String,
        /// Evidence file (repeatable).
        #[arg(long)]
        artifact: Vec<PathBuf>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Accept or reject a completed attempt after reviewing its evidence.
    Review {
        /// Job ID.
        job_id: String,
        /// Attempt ID being reviewed.
        attempt_id: String,
        /// Reviewer identity.
        #[arg(long)]
        reviewer: String,
        /// Review decision.
        #[arg(long, value_enum)]
        decision: ReviewDecision,
        /// Artifact ID from `board show` (repeatable).
        #[arg(long)]
        evidence: Vec<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Watch a run's board events; reconcile on lag.
    Watch {
        /// Run ID.
        #[arg(long)]
        run: String,
        /// Last sequence already observed.
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// Emit JSON lines.
        #[arg(long)]
        json: bool,
    },
}

/// Separate review outcome.
#[derive(Clone, Copy, ValueEnum)]
pub enum ReviewDecision {
    /// Accept the evidence.
    Accepted,
    /// Reject the contribution.
    Rejected,
}

fn now_ms() -> Result<u64, String> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|err| format!("system clock before epoch: {err}"))?;
    u64::try_from(elapsed.as_millis()).map_err(|err| format!("system clock out of range: {err}"))
}

fn print_json<T: Serialize>(value: &T) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value).map_err(|err| format!("serializing board output: {err}"))?;
    out.write_all(b"\n").map_err(|err| format!("writing board output: {err}"))
}

fn print_job(job: &JobSnapshot, json: bool) -> Result<(), String> {
    if json {
        return print_json(job);
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}  {:?}  review={:?}  version={}  {}", job.id, job.state, job.review, job.version, job.spec.title)
        .map_err(|err| err.to_string())?;
    for artifact in &job.artifacts {
        writeln!(out, "  artifact {}  {}  {} bytes", artifact.id, artifact.media_type, artifact.size).map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn print_event(event: &BoardEvent, json: bool) -> Result<(), String> {
    if json {
        return print_json(event);
    }
    writeln!(std::io::stdout().lock(), "{}  {}  {}  v{}", event.seq, event.kind, event.job_id, event.version).map_err(|err| err.to_string())
}

fn claim_file(home: &Path, attempt_id: &str) -> Result<PathBuf, String> {
    if attempt_id.is_empty() || !attempt_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return Err("invalid attempt id for local claim storage".to_owned());
    }
    Ok(home.join("run/board-claims").join(attempt_id))
}

fn ensure_claim_dir(home: &Path) -> Result<(), String> {
    for dir in [home.join("run"), home.join("run/board-claims")] {
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) => {
                if !meta.file_type().is_dir()
                    || meta.uid() != nix::unistd::Uid::current().as_raw()
                    || meta.permissions().mode() & 0o077 != 0
                {
                    return Err("claim directory must be an owned private directory".to_owned());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&dir)
                    .map_err(|cause| format!("creating private claim directory: {cause}"))?;
            }
            Err(err) => return Err(format!("inspecting private claim directory: {err}")),
        }
    }
    Ok(())
}

fn save_claim(home: &Path, attempt_id: &str, token: &str) -> Result<(), String> {
    let path = claim_file(home, attempt_id)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("saving private claim: {err}"))?;
    file.write_all(token.as_bytes()).map_err(|err| format!("writing private claim: {err}"))?;
    file.sync_all().map_err(|err| format!("syncing private claim: {err}"))
}

fn load_claim(home: &Path, attempt_id: &str) -> Result<String, String> {
    let path = claim_file(home, attempt_id)?;
    let meta = std::fs::symlink_metadata(&path).map_err(|err| format!("reading private claim metadata: {err}"))?;
    if !meta.file_type().is_file() || meta.uid() != nix::unistd::Uid::current().as_raw() || meta.permissions().mode() & 0o077 != 0 {
        return Err("private claim file is not a regular 0600 file".to_owned());
    }
    std::fs::read_to_string(path).map_err(|err| format!("reading private claim: {err}"))
}

/// Runs one board command through the local daemon, starting it when absent.
///
/// # Errors
/// Returns a transport, authorization, storage, or local-output error.
#[expect(clippy::too_many_lines, reason = "each CLI subcommand is a short typed daemon call")]
pub async fn run(home: &Path, action: BoardAction) -> Result<i32, String> {
    let client = connect_or_spawn(home).await.map_err(|err| err.to_string())?;
    match action {
        BoardAction::Post { title, deliverable, acceptance, run, depends_on, max_retries, json } => {
            let result = client
                .board_post(PostParams {
                    run_id: run,
                    spec: JobSpec { title, deliverable, acceptance, depends_on, max_retries, workspace: None },
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                })
                .await
                .map_err(|err| err.to_string())?;
            print_job(&result.job, json)?;
        }
        BoardAction::List { run, limit, json } => {
            let result = client.board_list(ListParams { run_id: run, limit: Some(limit) }).await.map_err(|err| err.to_string())?;
            if json {
                print_json(&result)?;
            } else {
                for job in &result.jobs {
                    print_job(job, false)?;
                }
            }
        }
        BoardAction::Show { job_id, json } => {
            let job = client.board_show(JobRef { job_id }).await.map_err(|err| err.to_string())?;
            print_job(&job, json)?;
        }
        BoardAction::Claim { job_id, worker, capacity, lease_seconds, json } => {
            ensure_claim_dir(home)?;
            let result = client
                .board_claim(ClaimParams {
                    job_id,
                    worker,
                    capacity,
                    now_ms: now_ms()?,
                    lease_ms: lease_seconds.saturating_mul(1_000),
                    expected_version: None,
                })
                .await
                .map_err(|err| err.to_string())?;
            save_claim(home, &result.attempt.id, &result.claim_token)?;
            if json {
                print_json(&serde_json::json!({"job": result.job, "attempt": result.attempt, "claim_saved": true}))?;
            } else {
                print_job(&result.job, false)?;
                writeln!(std::io::stdout().lock(), "  attempt {}  claim saved privately", result.attempt.id)
                    .map_err(|err| err.to_string())?;
            }
        }
        BoardAction::Complete { job_id, attempt_id, artifact, json } => {
            let mut artifacts = Vec::with_capacity(artifact.len());
            for path in artifact {
                let bytes = std::fs::read(&path).map_err(|err| format!("reading artifact {}: {err}", path.display()))?;
                if bytes.len() > 1_048_576 {
                    return Err("CLI artifact exceeds the 1 MiB board request limit".to_owned());
                }
                artifacts.push(ArtifactInput { media_type: "application/octet-stream".into(), data: Base64Bytes(bytes) });
            }
            let job = client
                .board_complete(CompleteParams {
                    job_id,
                    attempt_id: attempt_id.clone(),
                    claim_token: load_claim(home, &attempt_id)?,
                    artifacts,
                    now_ms: now_ms()?,
                })
                .await
                .map_err(|err| err.to_string())?;
            print_job(&job, json)?;
        }
        BoardAction::Review { job_id, attempt_id, reviewer, decision, evidence, json } => {
            let current = client.board_show(JobRef { job_id: job_id.clone() }).await.map_err(|err| err.to_string())?;
            let job = client
                .board_review(ReviewParams {
                    job_id,
                    attempt_id,
                    reviewer,
                    accepted: matches!(decision, ReviewDecision::Accepted),
                    evidence,
                    expected_version: current.version,
                    now_ms: now_ms()?,
                })
                .await
                .map_err(|err| err.to_string())?;
            print_job(&job, json)?;
        }
        BoardAction::Watch { run, after, json } => {
            let (snapshot, mut events) =
                client.board_watch(WatchParams { run_id: run.clone(), after_seq: after }).await.map_err(|err| err.to_string())?;
            if json {
                print_json(&snapshot)?;
            } else {
                for job in &snapshot.jobs {
                    print_job(job, false)?;
                }
            }
            let mut cursor = snapshot.next_seq;
            let mut reconcile = tokio::time::interval(std::time::Duration::from_secs(5));
            reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => {
                            if event.seq > cursor {
                                cursor = event.seq;
                                print_event(&event, json)?;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let snapshot = client.board_poll(PollParams { run_id: run.clone(), after_seq: cursor, limit: 256 })
                                .await.map_err(|err| err.to_string())?;
                            for event in &snapshot.events { print_event(event, json)?; }
                            cursor = snapshot.next_seq;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err("board event stream closed; reconcile with board show or list".to_owned());
                        }
                    },
                    _ = reconcile.tick() => {
                        let snapshot = client.board_poll(PollParams { run_id: run.clone(), after_seq: cursor, limit: 256 })
                            .await.map_err(|err| err.to_string())?;
                        for event in &snapshot.events { print_event(event, json)?; }
                        cursor = snapshot.next_seq;
                    }
                    signal = tokio::signal::ctrl_c() => {
                        signal.map_err(|err| format!("waiting for interrupt: {err}"))?;
                        break;
                    }
                }
            }
        }
    }
    Ok(0)
}
