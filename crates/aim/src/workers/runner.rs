//! Fenced board attempts run as sessions in harness-created Git worktrees.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aim_proto::board::{
    ArtifactInput, ClaimParams, CompleteParams, FailParams, HeartbeatParams, JobSnapshot, JobState, ListParams, RegisterWorkerParams,
};
use aim_proto::content::Base64Bytes;
use aim_proto::conversation::{Part, StopReason};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionSummary, SessionUpdate};
use futures_util::StreamExt as _;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::board::{Board, CleanupReceipt, Error as BoardError};
use crate::host::SessionClient;

use super::harness::{CommandOutput, GitHarness};
use super::receipt::{Receipt, ReceiptStore, WORKER_TOOLS, agent_name};

pub(super) const GIT_TIMEOUT_MS: u64 = 60_000;
const LEASE_MS: u64 = 300_000;
const MAX_CHECK_REVISIONS: u8 = 2;
const MAX_CHECK_FEEDBACK_CHARS: usize = 8_000;

/// Configuration of one stable local worker identity and its session backend.
#[derive(Clone)]
pub struct RunnerOptions {
    /// Private aim home for ownership receipts and the single-runner lock.
    pub home: PathBuf,
    /// Local aimx binary; its SSH transport reaches remote workspaces.
    pub aimx: PathBuf,
    /// Session provider (`openrouter`, `codex`, `acp:claude`, …).
    pub provider: String,
    /// Optional model override.
    pub model: Option<String>,
    /// Optional effort override.
    pub effort: Option<String>,
    /// Optional named agent definition.
    pub agent: Option<String>,
    /// Stable worker name, reused for crash reconciliation.
    pub worker: String,
    /// Most concurrent attempts, also declared to the ledger.
    pub concurrency: u32,
    /// Maximum wall time of one agent turn.
    pub turn_timeout: Duration,
}

/// One worker scan's results and provider-reported cost.
#[derive(Default, Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkSummary {
    /// Claimed or resumed attempts.
    pub started: u32,
    /// Attempts completed with commit evidence.
    pub succeeded: u32,
    /// Attempts that failed after cleanup or remained held for inspection.
    pub failed: u32,
    /// Provider-reported input tokens; zero means no usage was reported.
    pub input_tokens: u64,
    /// Provider-reported output tokens.
    pub output_tokens: u64,
    /// Provider-reported cost, when available, in millionths of USD.
    pub cost_micro_usd: u64,
    /// Whether any provider supplied cost data.
    pub cost_reported: bool,
}

#[derive(Default)]
struct AttemptOutcome {
    success: bool,
    input_tokens: u64,
    output_tokens: u64,
    cost_micro_usd: u64,
    cost_reported: bool,
}

#[derive(Serialize)]
struct Packet<'a> {
    branch: &'a str,
    base: &'a str,
    commit: &'a str,
    diff_stat: &'a str,
    check_log: Option<&'a str>,
    input_tokens: u64,
    output_tokens: u64,
    cost_micro_usd: Option<u64>,
}

/// A runner uses the board for admission and only [`SessionClient`] to drive an agent.
#[derive(Clone)]
pub struct Runner {
    board: Board,
    sessions: Arc<dyn SessionClient>,
    options: RunnerOptions,
    receipts: Arc<ReceiptStore>,
}

fn error(err: impl core::fmt::Display) -> String {
    err.to_string()
}

pub(super) fn valid_git(output: CommandOutput, action: &str) -> Result<String, String> {
    if !output.success() || output.truncated {
        return Err(format!("{action} failed or its output was truncated"));
    }
    let text = output.text;
    Ok(text.trim().to_owned())
}

pub(super) fn safe_log(text: &str) -> String {
    text.lines()
        .take(400)
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if ["token", "secret", "password", "authorization", "api_key", "bearer"].iter().any(|word| lower.contains(word)) {
                "[redacted]".to_owned()
            } else {
                line.chars().take(500).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prompt(job: &JobSnapshot, messages: &[String]) -> String {
    let mut text = format!(
        "Work on board job {} in this isolated Git worktree.\nTitle: {}\nDeliverable: {}\nAcceptance criteria:\n",
        job.id, job.spec.title, job.spec.deliverable,
    );
    for criterion in &job.spec.acceptance {
        text.push_str("- ");
        text.push_str(criterion);
        text.push('\n');
    }
    if !messages.is_empty() {
        text.push_str("Committed board messages:\n");
        for message in messages {
            text.push_str("- ");
            text.push_str(message);
            text.push('\n');
        }
    }
    text.push_str("Make the requested changes and tests. Do not commit; the board runner will check and commit your work.\n");
    text
}

fn worktree_path(source: &str, job_id: &str, attempt_id: &str) -> String {
    format!("{}-board-{job_id}-{attempt_id}", source.trim_end_matches('/'))
}

fn worktree_overlaps_home(worktree: &str, home: &Path) -> Result<bool, String> {
    let home = home.canonicalize().map_err(error)?;
    let raw = Path::new(worktree);
    let canonical = raw.canonicalize().or_else(|_| {
        raw.parent()
            .ok_or_else(|| std::io::Error::other("worktree has no parent"))?
            .canonicalize()
            .map(|parent| parent.join(raw.file_name().unwrap_or_default()))
    });
    let worktree = canonical.as_deref().unwrap_or(raw);
    Ok(worktree.starts_with(&home) || home.starts_with(worktree))
}

fn worker_session_is_confined(summary: &SessionSummary, receipt: &Receipt) -> bool {
    let Some(agent) = &summary.meta.agent else { return false };
    summary.meta.workspace == receipt.worktree
        && agent.name == agent_name(&receipt.attempt_id)
        && agent.allow.as_ref().is_some_and(|allowed| allowed.iter().map(String::as_str).eq(WORKER_TOOLS))
        && agent.deny.is_empty()
}

impl Runner {
    /// Opens private receipts and locks a worker identity to one process.
    ///
    /// # Errors
    /// Returns an invalid capacity or unsafe receipt directory/lock error.
    pub fn new(board: Board, sessions: Arc<dyn SessionClient>, options: RunnerOptions) -> Result<Self, String> {
        if options.concurrency == 0 || options.concurrency > 64 || options.provider.is_empty() || options.turn_timeout.is_zero() {
            return Err("invalid worker capacity, provider, or timeout".into());
        }
        if options.agent.is_some() || options.provider.starts_with("acp:") {
            return Err("board workers require a native provider and the private file-only agent".into());
        }
        let receipts = Arc::new(ReceiptStore::open(&options.home, &options.worker)?);
        Ok(Self { board, sessions, options, receipts })
    }

    /// Reconciles owned attempts and claims up to the declared capacity once.
    ///
    /// # Errors
    /// Returns a board read, receipt, or task failure.
    #[expect(clippy::too_many_lines, reason = "one bounded worker scan coordinates recovery, admission, and concurrent attempts")]
    pub async fn run_once(&self) -> Result<WorkSummary, String> {
        self.board
            .register_worker(RegisterWorkerParams { worker: self.options.worker.clone(), capacity: self.options.concurrency })
            .await
            .map_err(error)?;
        let mut jobs = Vec::new();
        let mut after_job = None;
        loop {
            let page = self.board.list(ListParams { run_id: None, limit: Some(200), after_job }).await.map_err(error)?;
            jobs.extend(page.jobs);
            if !page.truncated {
                break;
            }
            after_job = page.next_job;
        }
        let mut active = Vec::new();
        for job in &jobs {
            if let Some(attempt) = &job.attempt
                && attempt.worker == self.options.worker
                && (matches!(job.state, JobState::Claimed | JobState::Running)
                    || (job.state == JobState::Succeeded && !attempt.cleanup_confirmed))
                && let Some(receipt) = self.receipts.load(&attempt.id)?
            {
                if receipt.job_id != job.id || receipt.worker != self.options.worker {
                    return Err("worker receipt disagrees with ledger".into());
                }
                active.push(receipt);
            }
        }
        let capacity = usize::try_from(self.options.concurrency).map_err(error)?;
        for job in jobs.iter().filter(|job| job.state == JobState::Posted && job.spec.work.is_some()) {
            if active.len() >= capacity {
                break;
            }
            if job.assigned_worker.as_deref().is_some_and(|assigned| assigned != self.options.worker) {
                continue;
            }
            let attempt_id = Uuid::new_v4().to_string();
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let mut token_sha256 = String::with_capacity(64);
            for byte in Sha256::digest(token.as_bytes()) {
                let _ = write!(&mut token_sha256, "{byte:02x}");
            }
            let workspace = job.spec.workspace.clone().ok_or("editing job has no workspace")?;
            let location = job.spec.work.as_ref().ok_or("editing job has no work policy")?.location.clone();
            let workspace = if matches!(location, Location::Local) {
                std::fs::canonicalize(&workspace).map_err(error)?.to_string_lossy().to_string()
            } else {
                workspace
            };
            let branch = format!("board/{}/{}", job.id, attempt_id);
            let worktree = worktree_path(&workspace, &job.id, &attempt_id);
            if worktree_overlaps_home(&worktree, &self.options.home)? {
                return Err("worker worktree overlaps AIM_HOME; receipt reads would be possible".into());
            }
            let receipt = Receipt {
                job_id: job.id.clone(),
                run_id: job.run_id.clone(),
                attempt_id: attempt_id.clone(),
                token,
                worker: self.options.worker.clone(),
                worktree,
                workspace,
                location,
                branch,
                base_commit: None,
                session_id: None,
                turn_succeeded: false,
                session_closed: false,
                file_only_session: false,
                check_revisions: 0,
                check_feedback: None,
                check_passed: false,
                check_log: None,
            };
            self.receipts.save(&receipt)?;
            let claim = self
                .board
                .claim(ClaimParams {
                    job_id: job.id.clone(),
                    worker: self.options.worker.clone(),
                    capacity: self.options.concurrency,
                    attempt_id: attempt_id.clone(),
                    token_sha256,
                    idempotency_key: attempt_id.clone(),
                    now_ms: 0,
                    lease_ms: LEASE_MS,
                    expected_version: Some(job.version),
                })
                .await;
            let claim = match claim {
                Ok(claim) => claim,
                Err(BoardError::Conflict(_)) => {
                    self.receipts.remove(&attempt_id)?;
                    continue;
                }
                Err(err) => return Err(error(err)),
            };
            if claim.attempt.id != attempt_id {
                return Err("claim receipt returned a different attempt".into());
            }
            active.push(receipt);
        }
        let mut tasks = JoinSet::new();
        let mut summary = WorkSummary { started: u32::try_from(active.len()).map_err(error)?, ..WorkSummary::default() };
        for receipt in active {
            let runner = self.clone();
            tasks.spawn(async move { runner.run_attempt(receipt).await });
        }
        while let Some(joined) = tasks.join_next().await {
            let outcome = joined.map_err(error)??;
            if outcome.success {
                summary.succeeded += 1;
            } else {
                summary.failed += 1;
            }
            summary.input_tokens = summary.input_tokens.saturating_add(outcome.input_tokens);
            summary.output_tokens = summary.output_tokens.saturating_add(outcome.output_tokens);
            summary.cost_micro_usd = summary.cost_micro_usd.saturating_add(outcome.cost_micro_usd);
            summary.cost_reported |= outcome.cost_reported;
        }
        Ok(summary)
    }

    async fn run_attempt(&self, mut receipt: Receipt) -> Result<AttemptOutcome, String> {
        let job = self.board.show(receipt.job_id.clone()).await.map_err(error)?;
        let attempt = job.attempt.as_ref().ok_or("claimed attempt disappeared")?;
        if attempt.id != receipt.attempt_id {
            return Err("attempt generation changed before execution".into());
        }
        if job.state == JobState::Succeeded && !attempt.cleanup_confirmed {
            if !receipt.session_closed {
                return Err("succeeded attempt still has an unclosed session".into());
            }
            let source = GitHarness::connect(&self.options.aimx, &receipt.workspace, &receipt.location).await.map_err(error)?;
            source.shutdown().await;
            self.board
                .record_cleanup(
                    job.id.clone(),
                    receipt.attempt_id.clone(),
                    receipt.token.clone(),
                    CleanupReceipt {
                        session_id: receipt.session_id.clone(),
                        worktree: receipt.worktree.clone(),
                        session_stopped: true,
                        harness_stopped: true,
                        disposition: "kept".into(),
                    },
                )
                .await
                .map_err(error)?;
            self.receipts.remove(&receipt.attempt_id)?;
            return Ok(AttemptOutcome { success: true, ..AttemptOutcome::default() });
        }
        if attempt.lease_until_ms <= u64::try_from(crate::session::now_ms()).map_err(error)? {
            if let Ok(source) = GitHarness::connect(&self.options.aimx, &receipt.workspace, &receipt.location).await {
                let cleaned = self.cleanup_with_source(&mut receipt, &job, &source).await;
                source.shutdown().await;
                self.cleanup_and_fail(&receipt, &job, cleaned).await?;
            } else {
                self.cleanup_and_fail(&receipt, &job, false).await?;
            }
            return Ok(AttemptOutcome::default());
        }
        let stop = CancellationToken::new();
        let _heartbeat_guard = stop.clone().drop_guard();
        let (heart_error_tx, heart_error_rx) = tokio::sync::watch::channel::<Option<String>>(None);
        let heartbeat = self.spawn_heartbeat(&receipt, attempt.heartbeat_seq, stop.clone(), heart_error_tx);
        let source = GitHarness::connect(&self.options.aimx, &receipt.workspace, &receipt.location).await.map_err(error);
        let source = match source {
            Ok(source) => source,
            Err(_err) => {
                stop.cancel();
                let _ignored = heartbeat.await;
                self.cleanup_and_fail(&receipt, &job, false).await?;
                tracing::warn!(job = %job.id, "worker could not connect to its workspace");
                return Ok(AttemptOutcome::default());
            }
        };
        let work = self.execute(&mut receipt, &job, &source, heart_error_rx).await;
        stop.cancel();
        let _ignored = heartbeat.await;
        let cleaned = match &work {
            Ok(_) => false,
            Err(_err) => {
                tracing::warn!(job = %job.id, "board worker attempt failed");
                self.cleanup_with_source(&mut receipt, &job, &source).await
            }
        };
        source.shutdown().await;
        if let Ok(outcome) = work {
            self.board
                .record_cleanup(
                    job.id.clone(),
                    receipt.attempt_id.clone(),
                    receipt.token.clone(),
                    CleanupReceipt {
                        session_id: receipt.session_id.clone(),
                        worktree: receipt.worktree.clone(),
                        session_stopped: receipt.session_closed,
                        harness_stopped: true,
                        disposition: "kept".into(),
                    },
                )
                .await
                .map_err(error)?;
            self.receipts.remove(&receipt.attempt_id)?;
            Ok(outcome)
        } else {
            self.cleanup_and_fail(&receipt, &job, cleaned).await?;
            Ok(AttemptOutcome::default())
        }
    }

    fn spawn_heartbeat(
        &self,
        receipt: &Receipt,
        sequence: u64,
        stop: CancellationToken,
        failed: tokio::sync::watch::Sender<Option<String>>,
    ) -> tokio::task::JoinHandle<()> {
        let board = self.board.clone();
        let job_id = receipt.job_id.clone();
        let attempt_id = receipt.attempt_id.clone();
        let token = receipt.token.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(30));
            let mut seq = sequence;
            loop {
                tokio::select! {
                    () = stop.cancelled() => break,
                    _ = every.tick() => {
                        seq = seq.saturating_add(1);
                        if let Err(err) = board.heartbeat(HeartbeatParams {
                            job_id:job_id.clone(),attempt_id:attempt_id.clone(),claim_token:token.clone(),
                            sequence:seq,now_ms:0,lease_ms:LEASE_MS,
                        }).await {
                            let _ignored = failed.send(Some(err.to_string()));
                            break;
                        }
                    }
                }
            }
        })
    }

    async fn execute(
        &self,
        receipt: &mut Receipt,
        job: &JobSnapshot,
        source: &GitHarness,
        mut heart_error: tokio::sync::watch::Receiver<Option<String>>,
    ) -> Result<AttemptOutcome, String> {
        let policy = job.spec.work.as_ref().ok_or("job lacks work policy")?;
        if receipt.base_commit.is_none() {
            let base = valid_git(
                source
                    .argv(
                        vec!["git".into(), "rev-parse".into(), "--verify".into(), format!("refs/heads/{}", policy.target_branch)],
                        GIT_TIMEOUT_MS,
                    )
                    .await
                    .map_err(error)?,
                "resolve target",
            )?;
            receipt.base_commit = Some(base.clone());
            self.receipts.save(receipt)?;
        }
        let worktrees = valid_git(
            source.argv(vec!["git".into(), "worktree".into(), "list".into(), "--porcelain".into()], GIT_TIMEOUT_MS).await.map_err(error)?,
            "list worktrees",
        )?;
        let exists = worktrees.lines().any(|line| line == format!("worktree {}", receipt.worktree));
        if !exists {
            let base = receipt.base_commit.clone().ok_or("missing pinned base commit")?;
            valid_git(
                source
                    .argv(
                        vec![
                            "git".into(),
                            "worktree".into(),
                            "add".into(),
                            "-b".into(),
                            receipt.branch.clone(),
                            receipt.worktree.clone(),
                            base,
                        ],
                        GIT_TIMEOUT_MS,
                    )
                    .await
                    .map_err(error)?,
                "create attempt worktree",
            )?;
        }
        let worktree = GitHarness::connect(&self.options.aimx, &receipt.worktree, &receipt.location).await.map_err(error)?;
        let result = self.execute_in_worktree(receipt, job, &worktree, &mut heart_error).await;
        worktree.shutdown().await;
        result
    }

    async fn run_worker_turn(
        &self,
        receipt: &mut Receipt,
        job: &JobSnapshot,
        heart_error: &mut tokio::sync::watch::Receiver<Option<String>>,
        outcome: &mut AttemptOutcome,
    ) -> Result<(), String> {
        let confined_agent = self.receipts.ensure_agent(&receipt.attempt_id)?;
        let session_id = if let Some(id) = &receipt.session_id {
            id.clone()
        } else {
            let summary = self
                .sessions
                .create(SessionSpec {
                    workspace: receipt.worktree.clone(),
                    location: receipt.location.clone(),
                    provider: self.options.provider.clone(),
                    model: self.options.model.clone(),
                    effort: self.options.effort.clone(),
                    agent: Some(confined_agent),
                    persistence: Persistence::Persistent,
                    code_mode: None,
                })
                .await
                .map_err(error)?;
            if !worker_session_is_confined(&summary, receipt) {
                let _ignored = self.sessions.close(summary.meta.id.clone()).await;
                return Err("worker session did not retain its file-only worktree ceiling".into());
            }
            receipt.session_id = Some(summary.meta.id.clone());
            receipt.file_only_session = true;
            self.receipts.save(receipt)?;
            summary.meta.id
        };
        let (attached, mut updates) = self.sessions.attach(session_id.clone()).await.map_err(error)?;
        if !worker_session_is_confined(&attached.summary, receipt) {
            let _ignored = self.sessions.close(session_id).await;
            return Err("worker session lost its file-only worktree ceiling".into());
        }
        receipt.file_only_session = true;
        if receipt.turn_succeeded {
            return Ok(());
        }
        if attached.summary.state == SessionState::Idle && attached.summary.turns == u64::from(receipt.check_revisions) {
            let text = if receipt.check_revisions == 0 {
                let messages = self.board.messages(job.id.clone()).await.map_err(error)?;
                prompt(job, &messages)
            } else {
                receipt.check_feedback.clone().ok_or("missing check feedback for worker revision")?
            };
            self.sessions.prompt(session_id.clone(), vec![Part::Text { text }]).await.map_err(error)?;
        } else if attached.summary.state != SessionState::Running {
            return Err("session ended while runner was absent; result needs inspection".into());
        }
        let deadline = tokio::time::Instant::now() + self.options.turn_timeout;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    let _ignored = self.sessions.cancel(session_id.clone()).await;
                    return Err("worker session timed out".into());
                }
                changed = heart_error.changed() => {
                    let reason = heart_error.borrow().clone();
                    if changed.is_ok() && let Some(reason) = reason {
                        let _ignored = self.sessions.cancel(session_id.clone()).await;
                        return Err(format!("worker lease was lost: {reason}"));
                    }
                }
                update = updates.next() => match update {
                    Some(SessionUpdate::Usage {usage}) => {
                        outcome.input_tokens = outcome.input_tokens.saturating_add(usage.input_tokens);
                        outcome.output_tokens = outcome.output_tokens.saturating_add(usage.output_tokens);
                        if let Some(cost) = usage.cost_micro_usd {
                            outcome.cost_reported = true;
                            outcome.cost_micro_usd = outcome.cost_micro_usd.saturating_add(cost);
                        }
                    }
                    Some(SessionUpdate::TurnEnded {stop:StopReason::EndTurn}) => break,
                    Some(SessionUpdate::TurnEnded {stop}) => return Err(format!("worker turn ended without completion: {stop:?}")),
                    Some(SessionUpdate::TurnFailed {message}) => return Err(format!("worker turn failed: {message}")),
                    None => return Err("worker update stream ended before the turn".into()),
                    _ => {}
                }
            }
        }
        receipt.turn_succeeded = true;
        self.receipts.save(receipt)
    }

    #[expect(clippy::too_many_lines, reason = "session result, Git check, commit, and artifact form one attempt outcome")]
    async fn execute_in_worktree(
        &self,
        receipt: &mut Receipt,
        job: &JobSnapshot,
        worktree: &GitHarness,
        heart_error: &mut tokio::sync::watch::Receiver<Option<String>>,
    ) -> Result<AttemptOutcome, String> {
        let branch = valid_git(
            worktree.argv(vec!["git".into(), "branch".into(), "--show-current".into()], GIT_TIMEOUT_MS).await.map_err(error)?,
            "verify attempt branch",
        )?;
        if branch != receipt.branch {
            return Err("attempt worktree is on an unexpected branch".into());
        }
        let mut outcome = AttemptOutcome::default();
        if !receipt.session_closed && !receipt.check_passed {
            loop {
                self.run_worker_turn(receipt, job, heart_error, &mut outcome).await?;
                let status = valid_git(
                    worktree.argv(vec!["git".into(), "status".into(), "--porcelain".into()], GIT_TIMEOUT_MS).await.map_err(error)?,
                    "inspect worktree",
                )?;
                if status.is_empty() {
                    return Err("worker produced no tracked or untracked changes".into());
                }
                if let Some(command) = &job.spec.work.as_ref().ok_or("missing work policy")?.check_command {
                    let check = worktree
                        .shell(command.clone(), u64::try_from(self.options.turn_timeout.as_millis()).unwrap_or(u64::MAX))
                        .await
                        .map_err(error)?;
                    let safe = safe_log(&check.text);
                    let passed = check.success() && !check.truncated;
                    receipt.check_log = Some(format!("command: {}\npassed: {passed}\n{safe}", safe_log(command)));
                    if !passed {
                        if receipt.check_revisions >= MAX_CHECK_REVISIONS {
                            return Err("job check command failed after bounded worker revisions".into());
                        }
                        let output = safe.chars().take(MAX_CHECK_FEEDBACK_CHARS).collect::<String>();
                        receipt.check_feedback = Some(format!(
                            "The runner's check failed (or its output was truncated). Fix the files and finish another turn.\nCheck: {}\nOutput (bounded):\n{output}",
                            safe_log(command)
                        ));
                        receipt.check_revisions += 1;
                        receipt.turn_succeeded = false;
                        self.receipts.save(receipt)?;
                        continue;
                    }
                }
                receipt.check_passed = true;
                self.receipts.save(receipt)?;
                break;
            }
        }
        if !receipt.file_only_session || !receipt.check_passed {
            return Err("attempt lacks a verified file-only session and passing check".into());
        }
        if !receipt.session_closed {
            let id = receipt.session_id.as_ref().ok_or("worker session id is missing")?;
            self.sessions.close(id.clone()).await.map_err(error)?;
            receipt.session_closed = true;
            self.receipts.save(receipt)?;
        }
        valid_git(
            worktree.argv(vec!["git".into(), "add".into(), "-A".into()], GIT_TIMEOUT_MS).await.map_err(error)?,
            "stage contribution",
        )?;
        valid_git(
            worktree
                .argv(vec!["git".into(), "commit".into(), "-m".into(), format!("board: {}", job.spec.title)], GIT_TIMEOUT_MS)
                .await
                .map_err(error)?,
            "commit contribution",
        )?;
        let commit = valid_git(
            worktree.argv(vec!["git".into(), "rev-parse".into(), "HEAD".into()], GIT_TIMEOUT_MS).await.map_err(error)?,
            "read contribution commit",
        )?;
        let base = receipt.base_commit.as_deref().ok_or("missing pinned base commit")?;
        let diff_stat = valid_git(
            worktree
                .argv(vec!["git".into(), "diff".into(), "--stat".into(), format!("{base}..{commit}")], GIT_TIMEOUT_MS)
                .await
                .map_err(error)?,
            "read contribution diff",
        )?;
        let packet = serde_json::to_vec(&Packet {
            branch: &receipt.branch,
            base,
            commit: &commit,
            diff_stat: &diff_stat,
            check_log: receipt.check_log.as_deref(),
            input_tokens: outcome.input_tokens,
            output_tokens: outcome.output_tokens,
            cost_micro_usd: outcome.cost_reported.then_some(outcome.cost_micro_usd),
        })
        .map_err(error)?;
        let completed = self
            .board
            .complete(CompleteParams {
                job_id: job.id.clone(),
                attempt_id: receipt.attempt_id.clone(),
                claim_token: receipt.token.clone(),
                artifacts: vec![ArtifactInput {
                    media_type: "application/vnd.aim.board-contribution+json".into(),
                    data: Base64Bytes(packet),
                }],
                now_ms: 0,
            })
            .await
            .map_err(error)?;
        if completed.state != JobState::Succeeded {
            return Err("board did not record success".into());
        }
        outcome.success = true;
        Ok(outcome)
    }

    async fn cleanup_with_source(&self, receipt: &mut Receipt, job: &JobSnapshot, source: &GitHarness) -> bool {
        if let Some(session_id) = &receipt.session_id
            && !receipt.session_closed
        {
            if self.sessions.close(session_id.clone()).await.is_err() {
                return false;
            }
            receipt.session_closed = true;
            if self.receipts.save(receipt).is_err() {
                return false;
            }
        }
        let remove = job.spec.work.as_ref().is_some_and(|work| work.failure_cleanup == aim_proto::board::FailureCleanup::Remove);
        if remove {
            let worktrees = source.argv(vec!["git".into(), "worktree".into(), "list".into(), "--porcelain".into()], GIT_TIMEOUT_MS).await;
            let Ok(worktrees) = worktrees else {
                return false;
            };
            let exists = worktrees.text.lines().any(|line| line == format!("worktree {}", receipt.worktree));
            if exists {
                let removed = source
                    .argv(
                        vec!["git".into(), "worktree".into(), "remove".into(), "--force".into(), receipt.worktree.clone()],
                        GIT_TIMEOUT_MS,
                    )
                    .await;
                if removed.is_err() || removed.is_ok_and(|output| !output.success()) {
                    return false;
                }
            }
        }
        true
    }

    async fn cleanup_and_fail(&self, receipt: &Receipt, job: &JobSnapshot, confirmed: bool) -> Result<(), String> {
        let current = self.board.show(job.id.clone()).await.map_err(error)?;
        let attempt = current.attempt.as_ref().ok_or("attempt disappeared during failure")?;
        let current = if matches!(current.state, JobState::Claimed | JobState::Running)
            && attempt.lease_until_ms <= u64::try_from(crate::session::now_ms()).map_err(error)?
        {
            self.board.expire(job.id.clone(), current.version).await.map_err(error)?
        } else {
            current
        };
        if confirmed {
            let disposition = if job.spec.work.as_ref().is_some_and(|work| work.failure_cleanup == aim_proto::board::FailureCleanup::Remove)
            {
                "removed"
            } else {
                "kept"
            };
            self.board
                .record_cleanup(
                    job.id.clone(),
                    receipt.attempt_id.clone(),
                    receipt.token.clone(),
                    CleanupReceipt {
                        session_id: receipt.session_id.clone(),
                        worktree: receipt.worktree.clone(),
                        session_stopped: true,
                        harness_stopped: true,
                        disposition: disposition.into(),
                    },
                )
                .await
                .map_err(error)?;
        }
        if matches!(current.state, JobState::Claimed | JobState::Running) {
            self.board
                .fail(FailParams {
                    job_id: job.id.clone(),
                    attempt_id: receipt.attempt_id.clone(),
                    claim_token: receipt.token.clone(),
                    reason: "worker_execution_failed".into(),
                    now_ms: 0,
                })
                .await
                .map_err(error)?;
        }
        if confirmed {
            self.receipts.remove(&receipt.attempt_id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::worktree_overlaps_home;

    #[test]
    fn home_inside_worktree_is_also_refused() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("attempt");
        let home = worktree.join("state");
        std::fs::create_dir_all(&home).unwrap();
        assert!(worktree_overlaps_home(&worktree.to_string_lossy(), &home).unwrap());
        let separate = root.path().join("separate");
        std::fs::create_dir(&separate).unwrap();
        assert!(!worktree_overlaps_home(&worktree.to_string_lossy(), &separate).unwrap());
    }
}
