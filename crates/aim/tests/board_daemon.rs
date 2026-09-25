//! Board daemon transport and real-binary CLI checks.
#![expect(clippy::unwrap_used, reason = "isolated integration test setup and assertions")]
#![expect(clippy::panic, reason = "test-only hard failures")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aim::daemon::{client::DaemonClient, server, socket_path};
use aim::host::{BoxFuture, SessionClient, UpdateStream};
use aim_proto::board::{
    ArtifactInput, CancelParams, ClaimParams, CleanupReceipt, CompleteParams, ConfirmCleanupParams, JobRef, JobSnapshot, JobSpec,
    ListParams, PollParams, PostParams, RegisterWorkerParams, ReviewParams, WatchParams,
};
use aim_proto::content::Base64Bytes;
use aim_proto::conversation::Part;
use aim_proto::daemon::{PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionSummary};
use aim_proto::error::{ErrorCode, ProtoError};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt as _;

struct NoSessions;

fn unavailable<T: Send + 'static>() -> BoxFuture<Result<T, ProtoError>> {
    Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "no sessions in board test")) })
}

impl SessionClient for NoSessions {
    fn create(&self, _spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        unavailable()
    }
    fn list(&self, _params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        unavailable()
    }
    fn attach(&self, _session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        unavailable()
    }
    fn prompt(&self, _session: String, _parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
        unavailable()
    }
    fn cancel(&self, _session: String) -> BoxFuture<Result<(), ProtoError>> {
        unavailable()
    }
    fn set_config(&self, _params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
        unavailable()
    }
    fn close(&self, _session: String) -> BoxFuture<Result<(), ProtoError>> {
        unavailable()
    }
}

async fn started() -> (TempDir, tokio::task::JoinHandle<Result<(), ProtoError>>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = dir.path().to_path_buf();
    let socket = socket_path(&home);
    let task = tokio::spawn(async move { server::serve(&home, &socket, None, Arc::new(NoSessions)).await });
    for _ in 0..100 {
        if socket_path(dir.path()).exists() {
            return (dir, task);
        }
        assert!(!task.is_finished(), "board daemon exited before binding");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("board daemon socket did not appear");
}

fn post(title: &str, run_id: Option<String>, depends_on: Vec<String>) -> PostParams {
    PostParams {
        run_id,
        spec: JobSpec {
            title: title.into(),
            deliverable: format!("deliver {title}"),
            acceptance: vec!["review the artifact".into()],
            depends_on,
            max_retries: 1,
            workspace: None,
            work: None,
        },
        idempotency_key: format!("post-{title}"),
    }
}

fn now_ms() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn claim(job_id: String, worker: &str) -> ClaimParams {
    let token_sha256 = Sha256::digest(token(&job_id, worker));
    let attempt_id = uuid::Uuid::new_v4().to_string();
    ClaimParams {
        job_id,
        worker: worker.into(),
        capacity: 1,
        attempt_id: attempt_id.clone(),
        token_sha256: format!("{token_sha256:x}"),
        idempotency_key: attempt_id,
        now_ms: now_ms(),
        lease_ms: 60_000,
        expected_version: None,
    }
}

fn token(job_id: &str, worker: &str) -> String {
    format!("test-token:{job_id}:{worker}")
}

fn cleanup() -> CleanupReceipt {
    CleanupReceipt {
        session_id: None,
        worktree: "/tmp/test-inert-worktree".into(),
        session_stopped: true,
        harness_stopped: true,
        disposition: "kept".into(),
    }
}

async fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_aim")).env("AIM_HOME", home).args(args).output().await.unwrap()
}

#[tokio::test]
async fn daemon_board_round_trip_and_watch_hint() {
    let (dir, task) = started().await;
    let first = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let watcher = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let a = first.board_post(post("a", None, vec![])).await.unwrap().job;
    assert_eq!(first.board_show(JobRef { job_id: a.id.clone() }).await.unwrap().id, a.id);
    let (snapshot, mut events) = watcher.board_watch(WatchParams { run_id: a.run_id.clone(), after_seq: 0 }).await.unwrap();
    assert!(snapshot.jobs.iter().any(|job| job.id == a.id));
    let b = first.board_post(post("b", Some(a.run_id.clone()), vec![a.id.clone()])).await.unwrap().job;
    let hint = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
    assert_eq!(hint.job_id, b.id);
    let readback = watcher.board_poll(PollParams { run_id: a.run_id, after_seq: 0, limit: 20, after_job: None }).await.unwrap();
    assert!(readback.events.iter().any(|event| event.id == hint.id));
    task.abort();
}

#[tokio::test]
async fn daemon_binds_worker_to_connection_and_denies_owner_actions() {
    let (dir, task) = started().await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let a = client.board_post(post("role-a", None, vec![])).await.unwrap().job;
    let b = client.board_post(post("role-b", Some(a.run_id.clone()), vec![])).await.unwrap().job;
    for worker in ["w", "alias"] {
        client.board_register_worker(RegisterWorkerParams { worker: worker.into(), capacity: 1 }).await.unwrap();
    }
    let claimed = client.board_claim(claim(a.id.clone(), "w")).await.unwrap();
    assert_eq!(client.board_claim(claim(b.id.clone(), "alias")).await.unwrap_err().code, ErrorCode::Denied);
    assert_eq!(
        client.board_cancel(CancelParams { job_id: b.id, expected_version: b.version, now_ms: now_ms() }).await.unwrap_err().code,
        ErrorCode::Denied
    );
    assert_eq!(
        client.board_register_worker(RegisterWorkerParams { worker: "new".into(), capacity: 1 }).await.unwrap_err().code,
        ErrorCode::Denied
    );
    assert_eq!(claimed.attempt.worker, "w");
    task.abort();
}

#[tokio::test]
async fn oversized_artifact_batch_is_a_typed_error_before_frame_limit() {
    let (dir, task) = started().await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let job = client.board_post(post("large", None, vec![])).await.unwrap().job;
    client.board_register_worker(RegisterWorkerParams { worker: "large-worker".into(), capacity: 1 }).await.unwrap();
    let claim = client.board_claim(claim(job.id.clone(), "large-worker")).await.unwrap();
    let artifacts =
        (0..25).map(|_| ArtifactInput { media_type: "application/octet-stream".into(), data: Base64Bytes(vec![0; 1_048_576]) }).collect();
    let error = client
        .board_complete(CompleteParams {
            job_id: job.id.clone(),
            attempt_id: claim.attempt.id,
            claim_token: token(&job.id, "large-worker"),
            artifacts,
            now_ms: now_ms(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    task.abort();
}

#[tokio::test]
async fn cli_post_list_show_uses_real_binary_and_temp_home() {
    let (dir, task) = started().await;
    let result = cli(dir.path(), &["board", "post", "hello", "--deliverable", "a result", "--json"]).await;
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let job: JobSnapshot = serde_json::from_slice(&result.stdout).unwrap();
    let listed = cli(dir.path(), &["board", "list", "--run", &job.run_id, "--json"]).await;
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let list: aim_proto::board::ListResult = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(list.jobs.iter().any(|entry| entry.id == job.id));
    let shown = cli(dir.path(), &["board", "show", &job.id, "--json"]).await;
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    let from_cli: JobSnapshot = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(from_cli.id, job.id);

    let claimed = cli(dir.path(), &["board", "claim", &job.id, "--worker", "cli-worker", "--json"]).await;
    assert!(claimed.status.success(), "{}", String::from_utf8_lossy(&claimed.stderr));
    assert!(!String::from_utf8_lossy(&claimed.stdout).contains("claim_token"));
    let claimed: serde_json::Value = serde_json::from_slice(&claimed.stdout).unwrap();
    let attempt = claimed["attempt"]["id"].as_str().unwrap();
    let secret_path = dir.path().join("run/board-claims").join(attempt);
    assert_eq!(std::fs::metadata(&secret_path).unwrap().permissions().mode() & 0o777, 0o600);
    let evidence_file = dir.path().join("evidence.txt");
    std::fs::write(&evidence_file, b"reviewable evidence").unwrap();
    let completed =
        cli(dir.path(), &["board", "complete", &job.id, attempt, "--artifact", evidence_file.to_str().unwrap(), "--json"]).await;
    assert!(completed.status.success(), "{}", String::from_utf8_lossy(&completed.stderr));
    let completed: JobSnapshot = serde_json::from_slice(&completed.stdout).unwrap();
    let cleaned =
        cli(dir.path(), &["board", "confirm-cleanup", &job.id, attempt, "--worktree", "/tmp/test-inert-worktree", "--json"]).await;
    assert!(cleaned.status.success(), "{}", String::from_utf8_lossy(&cleaned.stderr));
    let evidence = completed.artifacts.first().expect("artifact summary").id.as_str();
    let reviewed = cli(
        dir.path(),
        &["board", "review", &job.id, attempt, "--reviewer", "cli-reviewer", "--decision", "accepted", "--evidence", evidence, "--json"],
    )
    .await;
    assert!(reviewed.status.success(), "{}", String::from_utf8_lossy(&reviewed.stderr));
    let reviewed: JobSnapshot = serde_json::from_slice(&reviewed.stdout).unwrap();
    assert_eq!(reviewed.review, aim_proto::board::ReviewState::Accepted);
    task.abort();
}

#[tokio::test]
async fn cli_watch_receives_committed_event_hint() {
    let (dir, task) = started().await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let a = client.board_post(post("watch-a", None, vec![])).await.unwrap().job;
    let mut watch = tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
        .env("AIM_HOME", dir.path())
        .args(["board", "watch", "--run", &a.run_id, "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = watch.stdout.take().expect("watch stdout");
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let initial = tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await.unwrap().unwrap().unwrap();
    let snapshot: aim_proto::board::PollResult = serde_json::from_str(&initial).unwrap();
    assert!(snapshot.jobs.iter().any(|job| job.id == a.id));
    let b = client.board_post(post("watch-b", Some(a.run_id), vec![])).await.unwrap().job;
    let next = tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await.unwrap().unwrap().unwrap();
    let event: aim_proto::board::BoardEvent = serde_json::from_str(&next).unwrap();
    assert_eq!(event.job_id, b.id);
    watch.kill().await.unwrap();
    task.abort();
}

/// Live local daemon + real CLI + two independent scripted clients. No provider credentials needed.
#[tokio::test]
#[ignore = "live local two-job DAG through the real CLI and daemon"]
async fn live_board_two_job_dag_over_daemon() {
    let began = Instant::now();
    let (dir, task) = started().await;
    let a_post = cli(dir.path(), &["board", "post", "A", "--deliverable", "A evidence", "--json"]).await;
    assert!(a_post.status.success(), "{}", String::from_utf8_lossy(&a_post.stderr));
    let a: JobSnapshot = serde_json::from_slice(&a_post.stdout).unwrap();
    let b_post =
        cli(dir.path(), &["board", "post", "B", "--deliverable", "B evidence", "--run", &a.run_id, "--depends-on", &a.id, "--json"]).await;
    assert!(b_post.status.success(), "{}", String::from_utf8_lossy(&b_post.stderr));
    let b: JobSnapshot = serde_json::from_slice(&b_post.stdout).unwrap();
    let posted = began.elapsed();

    let one = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let two = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    one.board_register_worker(RegisterWorkerParams { worker: "worker-one".into(), capacity: 1 }).await.unwrap();
    one.board_register_worker(RegisterWorkerParams { worker: "worker-two".into(), capacity: 1 }).await.unwrap();
    assert!(two.board_claim(claim(b.id.clone(), "worker-two")).await.is_err(), "B must wait for accepted A evidence");
    let a_claim = one.board_claim(claim(a.id.clone(), "worker-one")).await.unwrap();
    let a_done = one
        .board_complete(CompleteParams {
            job_id: a.id.clone(),
            attempt_id: a_claim.attempt.id.clone(),
            claim_token: token(&a.id, "worker-one"),
            artifacts: vec![ArtifactInput { media_type: "text/plain".into(), data: Base64Bytes(b"A evidence".to_vec()) }],
            now_ms: now_ms(),
        })
        .await
        .unwrap();
    assert!(two.board_claim(claim(b.id.clone(), "worker-two")).await.is_err(), "execution success is not review acceptance");
    one.board_confirm_cleanup(ConfirmCleanupParams {
        job_id: a.id.clone(),
        attempt_id: a_claim.attempt.id.clone(),
        claim_token: token(&a.id, "worker-one"),
        receipt: cleanup(),
    })
    .await
    .unwrap();
    let a_clean = one.board_show(JobRef { job_id: a.id.clone() }).await.unwrap();
    let evidence = a_done.artifacts.first().expect("A artifact").id.clone();
    let reviewer = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let a_accepted = reviewer
        .board_review(ReviewParams {
            job_id: a.id,
            attempt_id: a_claim.attempt.id,
            reviewer: "reviewer".into(),
            accepted: true,
            evidence: vec![evidence],
            expected_version: a_clean.version,
            now_ms: now_ms(),
        })
        .await
        .unwrap();
    assert_eq!(a_accepted.review, aim_proto::board::ReviewState::Accepted);
    let accepted = began.elapsed();

    let b_claim = two.board_claim(claim(b.id.clone(), "worker-two")).await.unwrap();
    let b_done = two
        .board_complete(CompleteParams {
            job_id: b.id.clone(),
            attempt_id: b_claim.attempt.id,
            claim_token: token(&b.id, "worker-two"),
            artifacts: vec![ArtifactInput { media_type: "text/plain".into(), data: Base64Bytes(b"B evidence".to_vec()) }],
            now_ms: now_ms(),
        })
        .await
        .unwrap();
    assert_eq!(b_done.state, aim_proto::board::JobState::Succeeded);
    let listed = two.board_list(ListParams { run_id: Some(b.run_id), limit: Some(10), after_job: None }).await.unwrap();
    assert_eq!(listed.jobs.len(), 2);
    eprintln!("live board DAG: posted={posted:?}, A accepted={accepted:?}, B completed={:?}", began.elapsed());
    task.abort();
}
