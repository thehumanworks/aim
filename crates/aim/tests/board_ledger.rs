//! Board ledger persistence and transaction boundary tests.
//!
//! Exercises the exported `aim::board` service against real SQLite files.

use aim::board::{Board, CleanupReceipt, Error};
use aim::store::{SessionStore as _, SqliteStore};
use aim_proto::board::{
    ArtifactInput, ClaimParams, CompleteParams, JobSpec, JobState, PostParams, RegisterWorkerParams, RetryParams, ReviewParams,
};
use aim_proto::content::Base64Bytes;
use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent, SessionMeta};
use sha2::{Digest as _, Sha256};

fn spec(title: &str, depends_on: Vec<String>, max_retries: u32) -> JobSpec {
    JobSpec {
        title: title.into(),
        deliverable: format!("Deliver {title}"),
        acceptance: vec!["Evidence exists".into()],
        depends_on,
        max_retries,
        workspace: None,
        work: None,
    }
}

fn claim(job_id: &str, worker: &str, capacity: u32) -> ClaimParams {
    let token_sha256 = Sha256::digest(token(job_id, worker));
    let attempt_id = uuid::Uuid::new_v4().to_string();
    ClaimParams {
        job_id: job_id.into(),
        worker: worker.into(),
        capacity,
        attempt_id: attempt_id.clone(),
        token_sha256: format!("{token_sha256:x}"),
        idempotency_key: attempt_id,
        now_ms: 0,
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

#[tokio::test]
async fn accepted_evidence_opens_dependency_gate_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".aim/aim.db");
    let board = Board::open(&path).unwrap();
    for worker in ["w1", "w2"] {
        board.register_worker(RegisterWorkerParams { worker: worker.into(), capacity: 1 }).await.unwrap();
    }
    let first = board.post(PostParams { run_id: None, spec: spec("a", vec![], 1), idempotency_key: "post-a".into() }).await.unwrap().job;
    let second = board
        .post(PostParams {
            run_id: Some(first.run_id.clone()),
            spec: spec("b", vec![first.id.clone()], 0),
            idempotency_key: "post-b".into(),
        })
        .await
        .unwrap()
        .job;
    assert!(matches!(board.claim(claim(&second.id, "w2", 1)).await, Err(Error::Conflict(_))));
    let receipt = board.claim(claim(&first.id, "w1", 1)).await.unwrap();
    let completed = board
        .complete(CompleteParams {
            job_id: first.id.clone(),
            attempt_id: receipt.attempt.id.clone(),
            claim_token: token(&first.id, "w1"),
            artifacts: vec![ArtifactInput { media_type: "text/plain".into(), data: Base64Bytes(b"evidence".to_vec()) }],
            now_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(completed.state, JobState::Succeeded);
    board.record_cleanup(first.id.clone(), receipt.attempt.id.clone(), token(&first.id, "w1"), cleanup()).await.unwrap();
    let cleaned = board.show(first.id.clone()).await.unwrap();
    let direct = rusqlite::Connection::open(&path).unwrap();
    assert!(direct.execute("UPDATE board_artifacts SET data=x'00' WHERE job_id=?1", rusqlite::params![first.id]).is_err());
    assert!(direct.execute("DELETE FROM board_artifacts WHERE job_id=?1", rusqlite::params![first.id]).is_err());
    assert!(matches!(board.claim(claim(&second.id, "w2", 1)).await, Err(Error::Conflict(_))));
    let artifact_id = completed.artifacts.first().unwrap().id.clone();
    assert!(matches!(
        board
            .review(ReviewParams {
                job_id: first.id.clone(),
                attempt_id: receipt.attempt.id.clone(),
                reviewer: "w1".into(),
                accepted: true,
                evidence: vec![artifact_id.clone()],
                expected_version: cleaned.version,
                now_ms: 0
            })
            .await,
        Err(Error::Conflict(_))
    ));
    let reviewed = board
        .review(ReviewParams {
            job_id: first.id.clone(),
            attempt_id: receipt.attempt.id.clone(),
            reviewer: "lead".into(),
            accepted: true,
            evidence: vec![artifact_id],
            expected_version: cleaned.version,
            now_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(reviewed.review, aim_proto::board::ReviewState::Accepted);
    assert!(board.claim(claim(&second.id, "w2", 1)).await.is_ok());
    drop(board);
    let reopened = Board::open(&path).unwrap();
    assert_eq!(reopened.show(first.id.clone()).await.unwrap().review, aim_proto::board::ReviewState::Accepted);
    let polled =
        reopened.poll(aim_proto::board::PollParams { run_id: first.run_id, after_seq: 0, limit: 100, after_job: None }).await.unwrap();
    assert_eq!(polled.events.len(), 7);
    assert!(polled.events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

#[tokio::test]
async fn concurrent_claims_have_one_winner_and_token_is_not_in_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let board = Board::open(&dir.path().join(".aim/aim.db")).unwrap();
    for worker in ["left", "right"] {
        board.register_worker(RegisterWorkerParams { worker: worker.into(), capacity: 1 }).await.unwrap();
    }
    let job = board.post(PostParams { run_id: None, spec: spec("one", vec![], 0), idempotency_key: "one".into() }).await.unwrap().job;
    let (left, right) = tokio::join!(board.claim(claim(&job.id, "left", 1)), board.claim(claim(&job.id, "right", 1)));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let _winner = left.or(right).unwrap();
    let serialized = serde_json::to_string(&board.show(job.id).await.unwrap()).unwrap();
    assert!(!serialized.contains("test-token:"));
}

#[tokio::test]
async fn claim_replay_keeps_the_same_attempt_and_capacity_is_registered() {
    let dir = tempfile::tempdir().unwrap();
    let board = Board::open(&dir.path().join(".aim/aim.db")).unwrap();
    board.register_worker(RegisterWorkerParams { worker: "worker".into(), capacity: 1 }).await.unwrap();
    let a = board.post(PostParams { run_id: None, spec: spec("a", vec![], 0), idempotency_key: "a".into() }).await.unwrap().job;
    let b = board
        .post(PostParams { run_id: Some(a.run_id.clone()), spec: spec("b", vec![], 0), idempotency_key: "b".into() })
        .await
        .unwrap()
        .job;
    let request = claim(&a.id, "worker", 1);
    let first = board.claim(request.clone()).await.unwrap();
    let replay = board.claim(request).await.unwrap();
    assert_eq!(first.attempt.id, replay.attempt.id);
    assert!(matches!(board.claim(claim(&b.id, "worker", 1)).await, Err(Error::Conflict(_))));
    assert!(matches!(board.claim(claim(&b.id, "worker", 99)).await, Err(Error::Conflict(_))));
    assert!(matches!(board.register_worker(RegisterWorkerParams { worker: "worker".into(), capacity: 99 }).await, Err(Error::Invalid(_))));
    assert!(matches!(board.register_worker(RegisterWorkerParams { worker: "worker".into(), capacity: 2 }).await, Err(Error::Conflict(_))));
}

#[tokio::test]
async fn list_and_poll_page_all_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let board = Board::open(&dir.path().join(".aim/aim.db")).unwrap();
    let first = board.post(PostParams { run_id: None, spec: spec("first", vec![], 0), idempotency_key: "first".into() }).await.unwrap().job;
    for i in 0..249 {
        board
            .post(PostParams { run_id: Some(first.run_id.clone()), spec: spec("paged", vec![], 0), idempotency_key: format!("page-{i}") })
            .await
            .unwrap();
    }
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = board
            .poll(aim_proto::board::PollParams { run_id: first.run_id.clone(), after_seq: 0, limit: 0, after_job: cursor })
            .await
            .unwrap();
        for job in page.jobs {
            assert!(seen.insert(job.id));
        }
        if !page.truncated {
            break;
        }
        cursor = page.next_job;
    }
    assert_eq!(seen.len(), 250);
}

#[tokio::test]
async fn expired_token_cannot_mutate_after_retry() {
    let dir = tempfile::tempdir().unwrap();
    let board = Board::open(&dir.path().join(".aim/aim.db")).unwrap();
    board.register_worker(RegisterWorkerParams { worker: "worker".into(), capacity: 1 }).await.unwrap();
    let job = board.post(PostParams { run_id: None, spec: spec("one", vec![], 1), idempotency_key: "one".into() }).await.unwrap().job;
    let mut request = claim(&job.id, "worker", 1);
    request.lease_ms = 1;
    let old = board.claim(request).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let expired = board.expire(job.id.clone(), old.job.version).await.unwrap();
    assert!(board.retry(RetryParams { job_id: job.id.clone(), expected_version: expired.version, now_ms: 0 }).await.is_err());
    board.record_cleanup(job.id.clone(), old.attempt.id.clone(), token(&job.id, "worker"), cleanup()).await.unwrap();
    let confirmed = board.show(job.id.clone()).await.unwrap();
    let reopened = board.retry(RetryParams { job_id: job.id.clone(), expected_version: confirmed.version, now_ms: 0 }).await.unwrap();
    assert_eq!(reopened.generation, 1);
    let newer = board.claim(claim(&job.id, "worker", 1)).await.unwrap();
    let stale = board
        .complete(CompleteParams {
            job_id: job.id.clone(),
            attempt_id: old.attempt.id,
            claim_token: token(&job.id, "worker"),
            artifacts: vec![],
            now_ms: 0,
        })
        .await;
    assert_eq!(stale, Err(Error::StaleClaim));
    assert_eq!(newer.attempt.generation, 1);
}

#[tokio::test]
async fn board_and_session_actors_share_one_wal_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".aim/aim.db");
    let board = Board::open(&path).unwrap();
    let sessions = SqliteStore::open(&path).unwrap();
    sessions
        .create(SessionMeta {
            id: "session".into(),
            created_ms: 1,
            workspace: "/w".into(),
            location: "local".into(),
            provider: "codex".into(),
            model: "model".into(),
            title: None,
            parent: None,
            agent: None,
        })
        .await
        .unwrap();
    let post = async {
        for index in 0..20 {
            board
                .post(PostParams { run_id: None, spec: spec("parallel", vec![], 0), idempotency_key: format!("parallel-{index}") })
                .await
                .unwrap();
        }
    };
    let append = async {
        for seq in 1..=20 {
            sessions
                .append(
                    "session".into(),
                    vec![SessionEvent {
                        schema: EVENT_SCHEMA,
                        seq,
                        turn: seq,
                        ts_ms: i64::try_from(seq).unwrap(),
                        body: EventBody::TurnStarted,
                    }],
                )
                .await
                .unwrap();
        }
    };
    tokio::join!(post, append);
    assert_eq!(board.list(aim_proto::board::ListParams { run_id: None, limit: Some(50), after_job: None }).await.unwrap().jobs.len(), 20);
    assert_eq!(sessions.load("session".into()).await.unwrap().1.len(), 20);
}

#[test]
fn crash_child_transaction() {
    let Ok(path) = std::env::var("AIM_CRASH_DB") else {
        return;
    };
    let marker = std::env::var("AIM_CRASH_MARKER").unwrap();
    let mut connection = rusqlite::Connection::open(path).unwrap();
    let tx = connection.transaction().unwrap();
    tx.execute(
        "INSERT INTO board_jobs (id,run_id,title,contract_json,post_key,state,created_ms,updated_ms) VALUES ('interrupted','run','interrupted','{}','interrupted','posted',1,1)",
        [],
    ).unwrap();
    tx.execute(
        "INSERT INTO board_outbox (event_id,run_id,job_id,job_version,kind,payload_json,created_ms) VALUES ('interrupted-event','run','interrupted',1,'job.posted','{}',1)",
        [],
    ).unwrap();
    std::fs::write(marker, b"inside transaction").unwrap();
    std::thread::park();
}

#[tokio::test]
async fn killed_mid_transaction_reopens_without_half_state_or_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".aim/aim.db");
    let marker = dir.path().join("ready");
    let board = Board::open(&path).unwrap();
    let posted =
        board.post(PostParams { run_id: Some("run".into()), spec: spec("root", vec![], 0), idempotency_key: "root".into() }).await.unwrap();
    assert_eq!(posted.job.run_id, "run");
    let executable = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(executable)
        .arg("--exact")
        .arg("crash_child_transaction")
        .env("AIM_CRASH_DB", &path)
        .env("AIM_CRASH_MARKER", &marker)
        .spawn()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if marker.exists() {
            ready = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(ready, "child never reached the transaction barrier");
    child.kill().unwrap();
    child.wait().unwrap();
    drop(board);
    let reopened = Board::open(&path).unwrap();
    assert_eq!(reopened.show("interrupted".into()).await, Err(Error::NotFound));
    let polled =
        reopened.poll(aim_proto::board::PollParams { run_id: "run".into(), after_seq: 0, limit: 10, after_job: None }).await.unwrap();
    assert_eq!(polled.events.len(), 1);
    assert_eq!(polled.jobs.len(), 1);
}
