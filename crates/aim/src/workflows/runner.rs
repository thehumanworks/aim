//! Daemon workflow driver. Board jobs remain the execution authority (ADR 0071).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use aim_kernel::workflow::{self as kernel, Step, StepStatus};
use aim_proto::board::{CancelParams, CleanupReceipt, JobSnapshot, JobState, ReviewState};
use serde_json::Value;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::board::Board;

use super::agent;
use super::board::WorkflowBoard;
use super::manifest::{StepKind, WorkflowManifest};
use super::store::{RunState, StepState, WorkflowRun, WorkflowStep, WorkflowStore};
use super::template;

fn now_ms() -> Result<i64, String> {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?;
    i64::try_from(elapsed.as_millis()).map_err(|error| error.to_string())
}

fn now_u64() -> Result<u64, String> {
    u64::try_from(now_ms()?).map_err(|error| error.to_string())
}

fn state(state: StepState) -> StepStatus {
    match state {
        StepState::Pending => StepStatus::Pending,
        StepState::InFlight => StepStatus::Running,
        StepState::Succeeded => StepStatus::Succeeded,
        StepState::Failed => StepStatus::Failed,
        StepState::Cancelled => StepStatus::Cancelled,
    }
}

fn planned(manifest: &WorkflowManifest) -> Result<(Vec<usize>, Vec<Step>), String> {
    let order = manifest.topological_order()?;
    let mut positions = HashMap::new();
    for (position, index) in order.iter().copied().enumerate() {
        let step = manifest.steps.get(index).ok_or("invalid workflow step order")?;
        positions.insert(step.id.as_str(), position);
    }
    let steps = order
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let step = manifest.steps.get(*index).ok_or("invalid workflow step order")?;
            let depends_on = step.depends_on.iter().map(|name| positions.get(name.as_str()).copied().ok_or("unknown dependency")).collect::<Result<Vec<_>,_>>()?;
            Ok(Step { id: u64::try_from(position).map_err(|error| error.to_string())?, depends_on, max_retries: step.retries })
        })
        .collect::<Result<Vec<_>, String>>()?;
    kernel::validate(&steps).map_err(|error| format!("verified workflow admission: {error:?}"))?;
    Ok((order, steps))
}

fn snapshot(store: &WorkflowStore, run: &WorkflowRun, order: &[usize], manifest: &WorkflowManifest) -> Result<Vec<WorkflowStep>, String> {
    order.iter().map(|index| {
        let step = manifest.steps.get(*index).ok_or("invalid workflow step index")?;
        store.get_step(&run.id, &step.id).map_err(|error| error.to_string())?.ok_or_else(|| format!("missing durable step {}", step.id))
    }).collect()
}

fn known_results(steps: &[WorkflowStep]) -> BTreeMap<String, Value> {
    steps.iter().filter_map(|step| step.result.as_ref().map(|result| (step.name.clone(), result.clone()))).collect()
}

fn tokens_used(steps: &[WorkflowStep]) -> u64 {
    steps.iter().filter_map(|step| step.result.as_ref()?.get("tokens")?.as_u64()).fold(0, u64::saturating_add)
}

fn remaining(run: &WorkflowRun, manifest: &WorkflowManifest, step_timeout: Option<u64>) -> Result<Duration, String> {
    let total_ms = i64::try_from(manifest.budget.timeout_seconds).map_err(|error| error.to_string())?.saturating_mul(1000);
    let left_ms = run.created_ms.saturating_add(total_ms).saturating_sub(now_ms()?);
    if left_ms <= 0 {
        return Err("workflow wall-clock budget exhausted".into());
    }
    let cap_ms = step_timeout.unwrap_or(manifest.budget.timeout_seconds).saturating_mul(1000);
    Ok(Duration::from_millis(u64::try_from(left_ms).unwrap_or(0).min(cap_ms)))
}

fn cleaned(root: &str, session_id: Option<String>) -> CleanupReceipt {
    CleanupReceipt { session_id, worktree: root.to_owned(), session_stopped: true, harness_stopped: true, disposition: "kept".into() }
}

async fn cancel_board_job(board: &Board, job: &JobSnapshot) -> Result<(), String> {
    if matches!(job.state, JobState::Posted | JobState::Claimed | JobState::Running) {
        board.cancel(CancelParams { job_id: job.id.clone(), expected_version: job.version, now_ms: now_u64()? })
            .await.map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn cancel_run(store: &WorkflowStore, workflow_board: &WorkflowBoard, board: &Board, run: &WorkflowRun) -> Result<(), String> {
    for step in store.list_steps(&run.id).map_err(|error| error.to_string())? {
        if step.board_job_id.is_some() {
            let job = workflow_board.show_step(store, &run.id, &step.name).await.map_err(|error| error.to_string())?;
            cancel_board_job(board, &job).await?;
        }
        if step.state == StepState::InFlight {
            let _cancelled = store.cancel_step(&run.id, &step.name, step.attempt_key.as_deref(), now_ms()?)
                .map_err(|error| error.to_string())?;
        }
    }
    let _closed = store.complete_cancel(&run.id, now_ms()?).map_err(|error| error.to_string())?;
    Ok(())
}

async fn accepted(store: &WorkflowStore, workflow_board: &WorkflowBoard, run: &WorkflowRun, step: &WorkflowStep) -> Result<Option<Value>, String> {
    let job = workflow_board.show_step(store, &run.id, &step.name).await.map_err(|error| error.to_string())?;
    if job.state != JobState::Succeeded || job.review != ReviewState::Accepted {
        return Ok(None);
    }
    if let Some(value) = workflow_board.accepted_result(&job).await.map_err(|error| error.to_string())? {
        return Ok(Some(value));
    }
    serde_json::to_value(job).map(Some).map_err(|error| error.to_string())
}

#[expect(clippy::too_many_arguments, reason = "workflow execution keeps durable, authority and budget inputs explicit")]
async fn execute_step(
    store: &WorkflowStore,
    workflow_board: &WorkflowBoard,
    run: &WorkflowRun,
    manifest: &WorkflowManifest,
    step: &WorkflowStep,
    definition: &super::manifest::WorkflowStep,
    results: &BTreeMap<String, Value>,
    aimx: &Path,
    db_path: &Path,
    token_budget: u64,
) -> Result<(), String> {
    let (title, description) = match &definition.kind {
        StepKind::Job { title, description } => (
            Some(template::render_text(title, &run.params, results)?),
            Some(template::render_text(description, &run.params, results)?),
        ),
        _ => (None, None),
    };
    let job = workflow_board.post_step(store, &run.id, &step.name, title.as_deref(), description.as_deref())
        .await.map_err(|error| error.to_string())?;
    if matches!(&definition.kind, StepKind::Job { .. }) {
        let _started = store.start_step(&run.id, &step.name, now_ms()?).map_err(|error| error.to_string())?;
        return Ok(());
    }
    if !matches!(job.state, JobState::Posted | JobState::Claimed) {
        return Err("workflow-owned board job is not claimable".into());
    }
    let claim = workflow_board.claim_step(store, &run.id, &step.name).await.map_err(|error| error.to_string())?;
    let started = store.start_step(&run.id, &step.name, now_ms()?).map_err(|error| error.to_string())?;
    if started.attempt_key.as_deref() != Some(claim.attempt_key.as_str()) {
        return Err("workflow and board attempt keys differ".into());
    }
    let timeout = remaining(run, manifest, definition.timeout_seconds)?;
    let cancel = CancellationToken::new();
    let effect = async {
        match &definition.kind {
            StepKind::Tool { tool, arguments } => {
                let args = template::render_value(arguments, &run.params, results)?;
                tokio::select! {
                    result = tokio::time::timeout(timeout, agent::run_tool(aimx, &run.workspace_root, &manifest.ceiling, tool, args, &claim.attempt_key)) =>
                        result.map_err(|_| "workflow tool step timed out".to_owned())?,
                    () = cancel.cancelled() => Err("workflow step cancelled".into()),
                }
            }
            StepKind::Agent { agent: name, provider, model, prompt } => {
                let rendered = template::render_text(prompt, &run.params, results)?;
                let on_session = |session: &str| store.set_agent_session_id(&run.id, &step.name, session, now_ms()?).map_err(|error| error.to_string());
                agent::run_agent(
                    db_path, aimx, &run.workspace_root, &manifest.ceiling, name,
                    provider.as_deref(), model.as_deref(), &rendered, token_budget, timeout, cancel.clone(), on_session,
                ).await.map(|(result, _tokens)| result)
            }
            StepKind::Job { .. } => Err("job step cannot be locally executed".into()),
        }
    };
    tokio::pin!(effect);
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut sequence = 0u64;
    let mut aborted = None;
    let outcome = loop {
        tokio::select! {
            result = &mut effect => break result,
            _ = ticker.tick() => {
                let latest = store.get_run(&run.id).map_err(|error| error.to_string())?.ok_or("workflow run disappeared")?;
                if latest.cancel_requested && !cancel.is_cancelled() {
                    aborted = Some("workflow step cancelled".to_owned());
                    cancel.cancel();
                }
                if sequence.is_multiple_of(60) {
                    let next = sequence.saturating_add(1);
                    if let Err(error) = workflow_board.heartbeat(&claim, next).await {
                        aborted = Some(format!("workflow board lease could not be renewed: {error}"));
                        cancel.cancel();
                    }
                }
                sequence = sequence.saturating_add(1);
            }
        }
    };
    let outcome = aborted.map_or(outcome, Err);
    match outcome {
        Ok(result) => {
            let session = store.get_step(&run.id, &step.name).map_err(|error| error.to_string())?.and_then(|step| step.agent_session_id);
            let cleanup = cleaned(&run.workspace_root, session);
            let _board_job = workflow_board.complete_step(&claim, &result, cleanup).await.map_err(|error| error.to_string())?;
            let _finished = store.succeed_step(&run.id, &step.name, &claim.attempt_key, &result, now_ms()?)
                .map_err(|error| error.to_string())?;
        }
        Err(reason) => {
            let session = store.get_step(&run.id, &step.name).map_err(|error| error.to_string())?.and_then(|step| step.agent_session_id);
            let cleanup = cleaned(&run.workspace_root, session);
            let _board_job = workflow_board.fail_step(&claim, "step_failed", cleanup).await.map_err(|error| error.to_string())?;
            let _failed = store.fail_step(&run.id, &step.name, &claim.attempt_key, &reason, now_ms()?)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

async fn run_one(home: PathBuf, aimx: PathBuf, board: Board, run_id: String) -> Result<(), String> {
    let store = WorkflowStore::open(&home.join("aim.db")).map_err(|error| error.to_string())?;
    let workflow_board = WorkflowBoard::open(board.clone(), &home, format!("workflow:{run_id}"), "workflow:reviewer".into())
        .map_err(|error| error.to_string())?;
    loop {
        let run = store.get_run(&run_id).map_err(|error| error.to_string())?.ok_or("workflow run disappeared")?;
        if run.state != RunState::Running {
            return Ok(());
        }
        let manifest = WorkflowManifest::parse(&run.manifest_text)?;
        let (order, plan) = planned(&manifest)?;
        let steps = snapshot(&store, &run, &order, &manifest)?;
        if run.cancel_requested || remaining(&run, &manifest, None).is_err() {
            if !run.cancel_requested {
                let _requested = store.request_cancel(&run.id, now_ms()?).map_err(|error| error.to_string())?;
            }
            return cancel_run(&store, &workflow_board, &board, &run).await;
        }
        if steps.iter().all(|step| step.state == StepState::Succeeded) {
            let last = steps.last().and_then(|step| step.result.as_ref()).ok_or("last workflow result missing")?;
            if let Err(error) = manifest.validate_result(last) {
                let _failed = store.fail_run(&run.id, now_ms()?).map_err(|failure| failure.to_string())?;
                return Err(format!("workflow result does not match declared schema: {error}"));
            }
            let _finished = store.finish_run(&run.id, last, now_ms()?).map_err(|error| error.to_string())?;
            return Ok(());
        }
        let statuses = steps.iter().map(|step| state(step.state)).collect::<Vec<_>>();
        let mut progressed = false;
        for (position, step) in steps.iter().enumerate() {
            let index = *order.get(position).ok_or("workflow order mismatch")?;
            let definition = manifest.steps.get(index).ok_or("workflow definition missing")?;
            if step.state == StepState::InFlight {
                if let Some(result) = accepted(&store, &workflow_board, &run, step).await? {
                    let key = step.attempt_key.as_deref().ok_or("in-flight attempt key missing")?;
                    let _done = store.succeed_step(&run.id, &step.name, key, &result, now_ms()?).map_err(|error| error.to_string())?;
                    progressed = true;
                    break;
                }
                if matches!(definition.kind, StepKind::Job { .. }) {
                    let job = workflow_board.show_step(&store, &run.id, &step.name).await.map_err(|error| error.to_string())?;
                    if matches!(job.state, JobState::Failed | JobState::Cancelled) || job.review == ReviewState::Rejected {
                        let key = step.attempt_key.as_deref().ok_or("job attempt key missing")?;
                        let _failed = store.fail_step(&run.id, &step.name, key, "board job failed or review rejected", now_ms()?)
                            .map_err(|error| error.to_string())?;
                        progressed = true;
                        break;
                    }
                    continue;
                }
                let key = step.attempt_key.as_deref().ok_or("in-flight attempt key missing")?;
                let _failed = store.fail_step(&run.id, &step.name, key, "uncertain external outcome after restart", now_ms()?)
                    .map_err(|error| error.to_string())?;
                let _run = store.fail_run(&run.id, now_ms()?).map_err(|error| error.to_string())?;
                return Err("workflow stopped at an uncertain in-flight effect; board claim retained for reconciliation".into());
            }
            if step.state == StepState::Failed {
                let used = step.attempt.saturating_sub(1);
                if kernel::retry(StepStatus::Failed, used, step.max_retries).is_err() {
                    let _run = store.fail_run(&run.id, now_ms()?).map_err(|error| error.to_string())?;
                    return Ok(());
                }
                let job = workflow_board.show_step(&store, &run.id, &step.name).await.map_err(|error| error.to_string())?;
                if job.state != JobState::Posted {
                    let _retried = workflow_board.retry_step(&job).await.map_err(|error| error.to_string())?;
                }
                let _retry = store.retry_step(&run.id, &step.name, now_ms()?).map_err(|error| error.to_string())?;
                progressed = true;
                break;
            }
            if step.state == StepState::Pending && kernel::is_ready(&plan, &statuses, position).map_err(|error| format!("verified ready decision: {error:?}"))? {
                let remaining_tokens = manifest.budget.max_tokens.saturating_sub(tokens_used(&steps));
                if remaining_tokens == 0 && matches!(definition.kind, StepKind::Agent { .. }) {
                    let _run = store.fail_run(&run.id, now_ms()?).map_err(|error| error.to_string())?;
                    return Err("workflow token budget exhausted".into());
                }
                let results = known_results(&steps);
                execute_step(&store, &workflow_board, &run, &manifest, step, definition, &results, &aimx, &home.join("aim.db"), remaining_tokens).await?;
                progressed = true;
                break;
            }
        }
        if !progressed {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Serve active workflows, including those left running by a prior daemon process.
///
/// This loop owns no global scheduler state: every decision is re-read from SQLite and the board.
pub async fn serve(home: PathBuf, aimx: PathBuf) {
    let path = home.join("aim.db");
    let Ok(store) = WorkflowStore::open(&path) else { return };
    let Ok(board) = Board::open(&path) else { return };
    let mut active = HashSet::new();
    let mut tasks = JoinSet::new();
    #[expect(clippy::infinite_loop, reason = "the daemon owns this workflow driver until process shutdown")]
    loop {
        while let Some(result) = tasks.try_join_next() {
            match result {
                Ok((run_id, outcome)) => {
                    active.remove(&run_id);
                    if let Err(error) = outcome {
                        tracing::warn!(%run_id, %error, "workflow run stopped");
                    }
                }
                Err(error) => tracing::warn!(%error, "workflow task stopped"),
            }
        }
        match store.list_active_runs() {
            Ok(runs) => {
                for run in runs {
                    if active.insert(run.id.clone()) {
                        let (home, aimx, board, run_id) = (home.clone(), aimx.clone(), board.clone(), run.id);
                        tasks.spawn(async move { let outcome = run_one(home, aimx, board, run_id.clone()).await; (run_id, outcome) });
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "workflow discovery failed"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod live_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    use aim_proto::board::{ArtifactInput, ClaimParams, CompleteParams, RegisterWorkerParams, ReviewParams};
    use aim_proto::content::Base64Bytes;
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;

    struct LiveFixture {
        _temp: TempDir,
        root: PathBuf,
        home: PathBuf,
        aim: PathBuf,
        aimx: PathBuf,
        daemon: Option<Child>,
    }

    impl LiveFixture {
        fn new() -> Result<Self, String> {
            let aim = PathBuf::from(std::env::var("AIM_TEST_AIM_BIN").map_err(|_| "set AIM_TEST_AIM_BIN to the built aim binary")?);
            let aimx = PathBuf::from(std::env::var("AIM_TEST_AIMX_BIN").map_err(|_| "set AIM_TEST_AIMX_BIN to the built aimx binary")?);
            let temp = tempfile::tempdir().map_err(|error| error.to_string())?;
            let root = temp.path().join("project");
            let home = temp.path().join("home");
            fs::create_dir_all(&root).map_err(|error| error.to_string())?;
            fs::create_dir_all(&home).map_err(|error| error.to_string())?;
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
            let init = Command::new("git").args(["init", "-q"]).current_dir(&root).status().map_err(|error| error.to_string())?;
            if !init.success() {
                return Err("cannot initialize temporary Git repository".into());
            }
            Ok(Self { _temp: temp, root, home, aim, aimx, daemon: None })
        }

        fn write_workflow(&self, name: &str, source: &str) -> Result<(), String> {
            let dir = self.root.join(".agents/workflows").join(name);
            fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            fs::write(dir.join("workflow.toml"), source).map_err(|error| error.to_string())
        }

        async fn start_daemon(&mut self) -> Result<(), String> {
            let child = Command::new(&self.aim)
                .arg("daemon")
                .env("AIM_HOME", &self.home)
                .env("AIM_AIMX", &self.aimx)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| error.to_string())?;
            self.daemon = Some(child);
            let start = Instant::now();
            while !self.home.join("run/daemon.sock").exists() {
                if start.elapsed() > Duration::from_secs(20) {
                    return Err("test daemon did not bind its socket".into());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(())
        }

        fn kill_daemon(&mut self) -> Result<(), String> {
            if let Some(mut child) = self.daemon.take() {
                child.kill().map_err(|error| error.to_string())?;
                let _status = child.wait().map_err(|error| error.to_string())?;
            }
            Ok(())
        }

        fn cli(&self, args: &[&str]) -> Result<String, String> {
            let output = Command::new(&self.aim)
                .args(args)
                .env("AIM_HOME", &self.home)
                .env("AIM_AIMX", &self.aimx)
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
            }
            String::from_utf8(output.stdout).map_err(|error| error.to_string())
        }

        fn queue(&self, name: &str) -> Result<String, String> {
            let root = self.root.to_str().ok_or("test path is not UTF-8")?;
            let _trust = self.cli(&["workflow", "trust", name, "-C", root])?;
            let run = self.cli(&["workflow", "run", name, "-C", root])?;
            Ok(run.trim().to_owned())
        }

        async fn wait_for(&self, run_id: &str, timeout: Duration, ready: impl Fn(&WorkflowStore) -> bool) -> Result<(), String> {
            let store = WorkflowStore::open(&self.home.join("aim.db")).map_err(|error| error.to_string())?;
            let start = Instant::now();
            loop {
                if ready(&store) {
                    return Ok(());
                }
                if start.elapsed() >= timeout {
                    let state = store.get_run(run_id).map_err(|error| error.to_string())?.map(|run| run.state);
                    return Err(format!("workflow did not reach expected state; current: {state:?}"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    impl Drop for LiveFixture {
        fn drop(&mut self) {
            let _stopped = self.kill_daemon();
        }
    }

    #[tokio::test]
    #[ignore = "requires built aim/aimx binaries and a live daemon process"]
    async fn live_workflow_crash_resume_without_rerunning_finished_tool() {
        let mut fixture = LiveFixture::new().expect("test binaries and temporary repository");
        let manifest = r#"
name = "resume"
version = "1"
trigger = "manual"
params = '{"type":"object"}'
results = '{"type":"object"}'
[ceiling]
roots = ["."]
ops = ["read", "write", "exec"]
[budget]
max_steps = 3
max_tokens = 1000
timeout_seconds = 120
[[steps]]
id = "write"
kind = "tool"
tool = "Bash"
arguments = '{"command":"printf once\\n >> once.log"}'
[[steps]]
id = "gate"
kind = "job"
title = "Gate the resumed run"
description = "An external worker accepts this barrier"
depends_on = ["write"]
[[steps]]
id = "verify"
kind = "tool"
tool = "Bash"
arguments = '{"command":"test $(wc -l < once.log) -eq 1"}'
depends_on = ["gate"]
"#;
        fixture.write_workflow("resume", manifest).expect("workflow manifest");
        fixture.start_daemon().await.expect("first daemon");
        let run_id = fixture.queue("resume").expect("trusted run");
        fixture.wait_for(&run_id, Duration::from_secs(30), |store| {
            store.get_step(&run_id, "write").ok().flatten().is_some_and(|step| step.state == StepState::Succeeded)
                && store.get_step(&run_id, "gate").ok().flatten().is_some_and(|step| step.state == StepState::InFlight)
        }).await.expect("first step complete and gate waiting");
        fixture.kill_daemon().expect("kill first daemon between steps");
        fixture.start_daemon().await.expect("restarted daemon");
        let store = WorkflowStore::open(&fixture.home.join("aim.db")).expect("workflow store");
        let gate = store.get_step(&run_id, "gate").expect("gate lookup").expect("gate step");
        let job_id = gate.board_job_id.expect("posted gate job");
        let board = Board::open(&fixture.home.join("aim.db")).expect("board");
        board.register_worker(RegisterWorkerParams { worker: "test-gate".into(), capacity: 1 }).await.expect("register worker");
        let token = Uuid::new_v4().to_string();
        let attempt = Uuid::new_v4().to_string();
        let claim = board.claim(ClaimParams {
            job_id: job_id.clone(), worker: "test-gate".into(), capacity: 1, attempt_id: attempt.clone(),
            token_sha256: format!("{:x}", Sha256::digest(token.as_bytes())), idempotency_key: Uuid::new_v4().to_string(),
            now_ms: 0, lease_ms: 300_000, expected_version: None,
        }).await.expect("claim barrier");
        let complete = board.complete(CompleteParams {
            job_id: job_id.clone(), attempt_id: claim.attempt.id.clone(), claim_token: token.clone(),
            artifacts: vec![ArtifactInput { media_type: "application/json".into(), data: Base64Bytes(b"{\"ok\":true}".to_vec()) }], now_ms: 0,
        }).await.expect("complete barrier");
        board.record_cleanup(job_id.clone(), claim.attempt.id.clone(), token, cleaned(&fixture.root.to_string_lossy(), None))
            .await.expect("confirm barrier cleanup");
        let snapshot = board.show(job_id.clone()).await.expect("barrier snapshot");
        let evidence = complete.artifacts.first().expect("evidence artifact").id.clone();
        let _reviewed = board.review(ReviewParams {
            job_id, attempt_id: claim.attempt.id, reviewer: "test-reviewer".into(), accepted: true,
            evidence: vec![evidence], expected_version: snapshot.version, now_ms: 0,
        }).await.expect("accept barrier evidence");
        fixture.wait_for(&run_id, Duration::from_secs(30), |store| {
            store.get_run(&run_id).ok().flatten().is_some_and(|run| run.state == RunState::Succeeded)
        }).await.expect("resumed run completed");
        let first = store.get_step(&run_id, "write").expect("first lookup").expect("first step");
        assert_eq!(first.attempt, 1, "finished step was never retried");
        assert_eq!(fs::read_to_string(fixture.root.join("once.log")).expect("tool output"), "once\n");
    }

    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY, built aim/aimx binaries, and a live provider"]
    async fn live_workflow_tool_agent_tool_openrouter() {
        assert!(std::env::var_os("OPENROUTER_API_KEY").is_some(), "OPENROUTER_API_KEY is required for this live test");
        let mut fixture = LiveFixture::new().expect("test binaries and temporary repository");
        let agents = fixture.root.join(".agents/agents");
        fs::create_dir_all(&agents).expect("agent directory");
        fs::write(agents.join("editor.md"), "---\nschema: aim.agent/v1\nname: editor\ndescription: Edits a small file.\nprovider: openrouter\nmodel: openai/gpt-4.1-mini\ntools: [Read, Edit]\n---\nRead the named file, make only the requested edit, and report completion.\n")
            .expect("agent definition");
        let manifest = r#"
name = "edit"
version = "1"
trigger = "manual"
params = '{"type":"object"}'
results = '{"type":"object","properties":{"is_error":{"type":"boolean"}},"required":["is_error"]}'
[ceiling]
roots = ["."]
ops = ["read", "write", "exec"]
[budget]
max_steps = 3
max_tokens = 12000
timeout_seconds = 240
[[steps]]
id = "write"
kind = "tool"
tool = "Write"
arguments = '{"file_path":"story.txt","content":"alpha\\n"}'
[[steps]]
id = "edit"
kind = "agent"
agent = "editor"
provider = "openrouter"
model = "openai/gpt-4.1-mini"
prompt = "Read story.txt and use Edit to change alpha to beta. Do not alter any other files."
depends_on = ["write"]
[[steps]]
id = "verify"
kind = "tool"
tool = "Bash"
arguments = '{"command":"grep -q beta story.txt"}'
depends_on = ["edit"]
"#;
        fixture.write_workflow("edit", manifest).expect("workflow manifest");
        fixture.start_daemon().await.expect("daemon");
        let run_id = fixture.queue("edit").expect("trusted run");
        fixture.wait_for(&run_id, Duration::from_secs(230), |store| {
            store.get_run(&run_id).ok().flatten().is_some_and(|run| run.state != RunState::Running)
        }).await.expect("live workflow finished");
        let store = WorkflowStore::open(&fixture.home.join("aim.db")).expect("workflow store");
        let run = store.get_run(&run_id).expect("run lookup").expect("run record");
        assert_eq!(run.state, RunState::Succeeded, "three-step workflow failed");
        assert!(fs::read_to_string(fixture.root.join("story.txt")).expect("edited file").contains("beta"));
        assert_eq!(store.list_steps(&run_id).expect("steps").len(), 3);
    }
}
