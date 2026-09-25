//! Human and JSON board commands over the local daemon.

use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use aim_proto::board::{
    ArtifactInput, BoardEvent, ClaimParams, CleanupReceipt, CompleteParams, ConfirmCleanupParams, FailureCleanup, JobRef, JobSnapshot,
    JobSpec, ListParams, PollParams, PostParams, RegisterWorkerParams, ReviewParams, WatchParams, WorkPolicy,
};
use aim_proto::content::Base64Bytes;
use aim_proto::daemon::Location;
use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::daemon::client::DaemonClient;
use crate::daemon::spawn::connect_or_spawn;
use crate::workers::{Integrator, Runner, RunnerOptions};
use crate::{board::Board, cli as aim_cli};

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
        /// Absolute repository path for an editing worker.
        #[arg(long)]
        workspace: Option<String>,
        /// SSH destination for a remote repository path.
        #[arg(long)]
        ssh: Option<String>,
        /// Branch that accepted work integrates into.
        #[arg(long, default_value = "main")]
        target_branch: String,
        /// Check command in the attempt and integration worktrees.
        #[arg(long)]
        check: Option<String>,
        /// Keep the failed worktree for inspection or remove it after stopped processes.
        #[arg(long, value_enum, default_value_t = CleanupArg::Keep)]
        cleanup: CleanupArg,
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
        /// Continue after a job ID from the previous page.
        #[arg(long)]
        after_job: Option<String>,
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
        /// Retry an uncertain claim with its already-saved attempt token.
        #[arg(long)]
        attempt_id: Option<String>,
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
    /// Record stopped effects for a completed, failed, or cancelled attempt.
    ConfirmCleanup {
        /// Job ID.
        job_id: String,
        /// Attempt ID whose private token is held locally.
        attempt_id: String,
        /// Inert worktree retained or removed after all processes stopped.
        #[arg(long)]
        worktree: String,
        /// The worktree was removed after shutdown.
        #[arg(long)]
        removed: bool,
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
    /// Claim eligible jobs and run agent sessions in isolated Git worktrees.
    Work {
        /// Provider for worker sessions.
        #[arg(short, long, default_value = "codex")]
        provider: String,
        /// Model override.
        #[arg(short, long)]
        model: Option<String>,
        /// Effort override.
        #[arg(short, long)]
        effort: Option<String>,
        /// Named agent definition.
        #[arg(long)]
        agent: Option<String>,
        /// Stable worker identity for capacity and crash recovery.
        #[arg(long, default_value = "board-worker")]
        worker: String,
        /// Maximum concurrent attempts.
        #[arg(long, default_value_t = 1)]
        concurrency: u32,
        /// Maximum seconds for one agent turn.
        #[arg(long, default_value_t = 3_600)]
        timeout_seconds: u64,
        /// Process one ledger scan and exit.
        #[arg(long)]
        once: bool,
        /// Integrate accepted jobs from the serialized queue after each scan.
        #[arg(long)]
        auto_integrate: bool,
        /// Explicit aimx executable.
        #[arg(long)]
        aimx: Option<PathBuf>,
        /// Emit a JSON summary per scan.
        #[arg(long)]
        json: bool,
    },
    /// Merge an accepted attempt into its configured target branch.
    Integrate {
        /// Accepted job ID.
        job_id: String,
        /// Explicit aimx executable.
        #[arg(long)]
        aimx: Option<PathBuf>,
        /// Emit JSON integration state.
        #[arg(long)]
        json: bool,
    },
}

/// Failure-worktree choice when posting an editing job.
#[derive(Clone, Copy, ValueEnum)]
pub enum CleanupArg {
    /// Preserve it for inspection.
    Keep,
    /// Remove it after runner-owned processes stop.
    Remove,
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

async fn reconcile_events(client: &DaemonClient, run: &str, cursor: &mut u64, json: bool) -> Result<(), String> {
    loop {
        let snapshot = client
            .board_poll(PollParams { run_id: run.to_owned(), after_seq: *cursor, limit: 256, after_job: None })
            .await
            .map_err(|err| err.to_string())?;
        let count = snapshot.events.len();
        for event in &snapshot.events {
            print_event(event, json)?;
        }
        *cursor = snapshot.next_seq;
        if count < 256 {
            return Ok(());
        }
    }
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
    file.sync_all().map_err(|err| format!("syncing private claim: {err}"))?;
    std::fs::File::open(home.join("run/board-claims"))
        .and_then(|dir| dir.sync_all())
        .map_err(|err| format!("syncing private claim directory: {err}"))
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
        BoardAction::Post {
            title,
            deliverable,
            acceptance,
            run,
            depends_on,
            max_retries,
            workspace,
            ssh,
            target_branch,
            check,
            cleanup,
            json,
        } => {
            if workspace.is_none() && (ssh.is_some() || check.is_some() || target_branch != "main" || matches!(cleanup, CleanupArg::Remove))
            {
                return Err("worker policy requires --workspace".into());
            }
            let work = workspace.as_ref().map(|_| WorkPolicy {
                location: ssh.map_or(Location::Local, |destination| Location::Ssh { destination }),
                target_branch,
                check_command: check,
                failure_cleanup: if matches!(cleanup, CleanupArg::Remove) { FailureCleanup::Remove } else { FailureCleanup::Keep },
            });
            let result = client
                .board_post(PostParams {
                    run_id: run,
                    spec: JobSpec { title, deliverable, acceptance, depends_on, max_retries, workspace, work },
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                })
                .await
                .map_err(|err| err.to_string())?;
            print_job(&result.job, json)?;
        }
        BoardAction::List { run, limit, after_job, json } => {
            let result =
                client.board_list(ListParams { run_id: run, limit: Some(limit), after_job }).await.map_err(|err| err.to_string())?;
            if json {
                print_json(&result)?;
            } else {
                for job in &result.jobs {
                    print_job(job, false)?;
                }
                if let Some(next) = result.next_job {
                    writeln!(std::io::stdout().lock(), "  next page: --after-job {next}").map_err(|err| err.to_string())?;
                }
            }
        }
        BoardAction::Show { job_id, json } => {
            let job = client.board_show(JobRef { job_id }).await.map_err(|err| err.to_string())?;
            print_job(&job, json)?;
        }
        BoardAction::Claim { job_id, worker, attempt_id, capacity, lease_seconds, json } => {
            client.board_register_worker(RegisterWorkerParams { worker: worker.clone(), capacity }).await.map_err(|err| err.to_string())?;
            ensure_claim_dir(home)?;
            let (attempt_id, token) = if let Some(id) = attempt_id {
                let token = load_claim(home, &id)?;
                (id, token)
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
                save_claim(home, &id, &token)?;
                (id, token)
            };
            let mut token_sha256 = String::with_capacity(64);
            for byte in Sha256::digest(token.as_bytes()) {
                let _ = write!(&mut token_sha256, "{byte:02x}");
            }
            let result = client
                .board_claim(ClaimParams {
                    job_id,
                    worker,
                    capacity,
                    attempt_id: attempt_id.clone(),
                    token_sha256,
                    idempotency_key: attempt_id.clone(),
                    now_ms: now_ms()?,
                    lease_ms: lease_seconds.saturating_mul(1_000),
                    expected_version: None,
                })
                .await
                .map_err(|err| format!("{err} (retry with --attempt-id {attempt_id})"))?;
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
        BoardAction::ConfirmCleanup { job_id, attempt_id, worktree, removed, json } => {
            let job = client
                .board_confirm_cleanup(ConfirmCleanupParams {
                    job_id,
                    attempt_id: attempt_id.clone(),
                    claim_token: load_claim(home, &attempt_id)?,
                    receipt: CleanupReceipt {
                        session_id: None,
                        worktree,
                        session_stopped: true,
                        harness_stopped: true,
                        disposition: if removed { "removed" } else { "kept" }.into(),
                    },
                })
                .await
                .map_err(|err| err.to_string())?;
            std::fs::remove_file(claim_file(home, &attempt_id)?).map_err(|err| format!("removing confirmed claim file: {err}"))?;
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
            let mut next_job = snapshot.next_job;
            while let Some(after_job) = next_job {
                let page = client
                    .board_poll(PollParams { run_id: run.clone(), after_seq: snapshot.next_seq, limit: 0, after_job: Some(after_job) })
                    .await
                    .map_err(|err| err.to_string())?;
                for job in &page.jobs {
                    print_job(job, json)?;
                }
                next_job = page.next_job;
            }
            let mut cursor = snapshot.next_seq;
            let mut reconcile = tokio::time::interval(Duration::from_secs(5));
            reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) =>
                            reconcile_events(&client,&run,&mut cursor,json).await?,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err("board event stream closed; reconcile with board show or list".to_owned());
                        }
                    },
                    _ = reconcile.tick() => {
                        reconcile_events(&client,&run,&mut cursor,json).await?;
                    }
                    signal = tokio::signal::ctrl_c() => {
                        signal.map_err(|err| format!("waiting for interrupt: {err}"))?;
                        break;
                    }
                }
            }
        }
        BoardAction::Work { provider, model, effort, agent, worker, concurrency, timeout_seconds, once, auto_integrate, aimx, json } => {
            let board = Board::open(&home.join("aim.db")).map_err(|err| err.to_string())?;
            let aimx = aim_cli::find_aimx(aimx.as_deref());
            let runner = Runner::new(
                board.clone(),
                Arc::new(client),
                RunnerOptions {
                    home: home.to_path_buf(),
                    aimx: aimx.clone(),
                    provider,
                    model,
                    effort,
                    agent,
                    worker,
                    concurrency,
                    turn_timeout: Duration::from_secs(timeout_seconds),
                },
            )?;
            loop {
                let summary = runner.run_once().await?;
                if json {
                    print_json(&summary)?;
                } else if summary.started > 0 {
                    writeln!(
                        std::io::stdout().lock(),
                        "board worker: {} started, {} succeeded, {} failed",
                        summary.started,
                        summary.succeeded,
                        summary.failed
                    )
                    .map_err(|err| err.to_string())?;
                }
                if auto_integrate {
                    let integrator = Integrator::new(board.clone(), home, aimx.clone())?;
                    let integrated = integrator.integrate_queued().await?;
                    for record in integrated {
                        if json {
                            print_json(&record)?;
                        } else {
                            writeln!(std::io::stdout().lock(), "integration {} {:?}", record.job_id, record.state)
                                .map_err(|err| err.to_string())?;
                        }
                    }
                }
                if once {
                    break;
                }
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(1)) => {},
                    signal = tokio::signal::ctrl_c() => { signal.map_err(|err| err.to_string())?; break; }
                }
            }
        }
        BoardAction::Integrate { job_id, aimx, json } => {
            let board = Board::open(&home.join("aim.db")).map_err(|err| err.to_string())?;
            let integrator = Integrator::new(board, home, aim_cli::find_aimx(aimx.as_deref()))?;
            let record = integrator.integrate(job_id).await?;
            if json {
                print_json(&record)?;
            } else {
                writeln!(std::io::stdout().lock(), "integration {} {:?}", record.job_id, record.state).map_err(|err| err.to_string())?;
            }
        }
    }
    Ok(0)
}
