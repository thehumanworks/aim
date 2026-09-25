//! Scripted session boundary and real Git worktree integration for board workers.
#![expect(clippy::unwrap_used, reason = "isolated temporary repository assertions")]
#![expect(clippy::expect_used, reason = "isolated temporary repository assertions")]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aim::board::{Board, integration::IntegrationState};
use aim::host::{BoxFuture, SessionClient, UpdateStream};
use aim::workers::{Integrator, Runner, RunnerOptions};
use aim_proto::board::{FailureCleanup, JobSnapshot, JobSpec, PostParams, ReviewParams, WorkPolicy};
use aim_proto::conversation::{Part, StopReason};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionState,
    SessionSummary, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::SessionMeta;
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum Script {
    Separate,
    Conflict,
    Paused,
}

struct Session {
    summary: SessionSummary,
    updates: tokio::sync::broadcast::Sender<SessionUpdate>,
    attachments: usize,
}

struct Scripted {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    script: Script,
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::NotFound, "scripted session missing")
}

impl SessionClient for Scripted {
    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            let id = uuid::Uuid::new_v4().to_string();
            let summary = SessionSummary {
                meta: SessionMeta {
                    id: id.clone(),
                    created_ms: 1,
                    workspace: spec.workspace,
                    location: "local".into(),
                    provider: spec.provider,
                    model: spec.model.unwrap_or_else(|| "scripted".into()),
                    title: None,
                    parent: None,
                    agent: None,
                },
                state: SessionState::Idle,
                persistence: Persistence::Persistent,
                last_activity_ms: 1,
                turns: 0,
            };
            let (updates, _) = tokio::sync::broadcast::channel(64);
            sessions.lock().unwrap().insert(id, Session { summary: summary.clone(), updates, attachments: 0 });
            Ok(summary)
        })
    }
    fn list(&self, _params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move { Ok(sessions.lock().unwrap().values().map(|s| s.summary.clone()).collect()) })
    }
    fn attach(&self, id: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            let mut map = sessions.lock().unwrap();
            let session = map.get_mut(&id).ok_or_else(unavailable)?;
            session.attachments += 1;
            let summary = session.summary.clone();
            let mut receiver = session.updates.subscribe();
            drop(map);
            let updates: UpdateStream = Box::pin(async_stream::stream! {
                while let Ok(update) = receiver.recv().await { yield update; }
            });
            Ok((SessionAttachResult { summary, transcript: vec![] }, updates))
        })
    }
    fn prompt(&self, id: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
        let sessions = Arc::clone(&self.sessions);
        let script = self.script;
        Box::pin(async move {
            let text = parts.iter().find_map(|part| if let Part::Text { text } = part { Some(text.as_str()) } else { None }).unwrap_or("");
            let (workspace, updates) = {
                let mut map = sessions.lock().unwrap();
                let session = map.get_mut(&id).ok_or_else(unavailable)?;
                session.summary.state = SessionState::Running;
                session.summary.turns = 1;
                (session.summary.meta.workspace.clone(), session.updates.clone())
            };
            let is_a = text.contains("Title: A");
            if !matches!(script, Script::Paused) {
                let name = match script {
                    Script::Separate => {
                        if is_a {
                            "a.txt"
                        } else {
                            "b.txt"
                        }
                    }
                    Script::Conflict => "shared.txt",
                    Script::Paused => "a.txt",
                };
                std::fs::write(Path::new(&workspace).join(name), if is_a { "A\n" } else { "B\n" })
                    .map_err(|err| ProtoError::new(ErrorCode::Unavailable, err.to_string()))?;
                let _ignored = updates.send(SessionUpdate::TurnEnded { stop: StopReason::EndTurn });
            }
            Ok(PromptOutcome::Started { turn: 1 })
        })
    }
    fn cancel(&self, _id: String) -> BoxFuture<Result<(), ProtoError>> {
        Box::pin(async { Ok(()) })
    }
    fn set_config(&self, _params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
        Box::pin(async { Ok(()) })
    }
    fn close(&self, id: String) -> BoxFuture<Result<(), ProtoError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            sessions.lock().unwrap().get_mut(&id).ok_or_else(unavailable)?.summary.state = SessionState::Closed;
            Ok(())
        })
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").args(args).current_dir(repo).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn setup(script: Script) -> (TempDir, PathBuf, PathBuf, Board, Arc<Scripted>) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let home = dir.path().join("home");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "Board Test"]);
    git(&repo, &["config", "user.email", "board@example.invalid"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    if matches!(script, Script::Conflict) {
        std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "base"]);
    let board = Board::open(&home.join("aim.db")).unwrap();
    let sessions = Arc::new(Scripted { sessions: Arc::new(Mutex::new(HashMap::new())), script });
    (dir, repo, home, board, sessions)
}

fn aimx() -> PathBuf {
    let path = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("aimx");
    assert!(path.exists(), "build aimx before running the worker integration test: {}", path.display());
    path
}

fn worker(home: &Path, board: Board, sessions: Arc<Scripted>) -> Runner {
    Runner::new(
        board,
        sessions,
        RunnerOptions {
            home: home.into(),
            aimx: aimx(),
            provider: "scripted".into(),
            model: None,
            effort: None,
            agent: None,
            worker: "worker".into(),
            concurrency: 2,
            turn_timeout: Duration::from_secs(10),
        },
    )
    .unwrap()
}

async fn post(board: &Board, repo: &Path, title: &str, depends_on: Vec<String>, script: Script) -> JobSnapshot {
    let check = match script {
        Script::Separate | Script::Paused => format!("test -f {}.txt", title.to_ascii_lowercase()),
        Script::Conflict => "test -f shared.txt".into(),
    };
    board
        .post(PostParams {
            run_id: if let Some(first) = depends_on.first() { Some(board.show(first.clone()).await.unwrap().run_id) } else { None },
            spec: JobSpec {
                title: title.into(),
                deliverable: format!("Write {title} output"),
                acceptance: vec!["File exists".into()],
                depends_on,
                max_retries: 1,
                workspace: Some(repo.to_string_lossy().to_string()),
                work: Some(WorkPolicy {
                    location: Location::Local,
                    target_branch: "main".into(),
                    check_command: Some(check),
                    failure_cleanup: FailureCleanup::Keep,
                }),
            },
            idempotency_key: format!("post-{title}"),
        })
        .await
        .unwrap()
        .job
}

async fn accept(board: &Board, job_id: &str) {
    let job = board.show(job_id.to_owned()).await.unwrap();
    let attempt = job.attempt.as_ref().expect("attempt");
    assert!(attempt.cleanup_confirmed);
    let evidence = job.artifacts.iter().find(|artifact| artifact.media_type == "application/vnd.aim.board-contribution+json").unwrap();
    board
        .review(ReviewParams {
            job_id: job_id.into(),
            attempt_id: attempt.id.clone(),
            reviewer: "lead".into(),
            accepted: true,
            evidence: vec![evidence.id.clone()],
            expected_version: job.version,
            now_ms: 0,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn scripted_two_job_dag_runs_in_worktrees_then_integrates() {
    let (_dir, repo, home, board, sessions) = setup(Script::Separate);
    let a = post(&board, &repo, "A", vec![], Script::Separate).await;
    let b = post(&board, &repo, "B", vec![a.id.clone()], Script::Separate).await;
    let runner = worker(&home, board.clone(), sessions);
    let first = runner.run_once().await.unwrap();
    assert_eq!((first.succeeded, first.failed), (1, 0));
    accept(&board, &a.id).await;
    let second = runner.run_once().await.unwrap();
    assert_eq!((second.succeeded, second.failed), (1, 0));
    accept(&board, &b.id).await;
    let integrator = Integrator::new(board.clone(), &home, aimx()).unwrap();
    assert_eq!(integrator.integrate(a.id).await.unwrap().state, IntegrationState::Integrated);
    assert_eq!(integrator.integrate(b.id).await.unwrap().state, IntegrationState::Integrated);
    assert_eq!(git(&repo, &["show", "main:a.txt"]), "A");
    assert_eq!(git(&repo, &["show", "main:b.txt"]), "B");
}

#[tokio::test]
async fn conflicting_integrations_leave_conflict_files() {
    let (_dir, repo, home, board, sessions) = setup(Script::Conflict);
    let a = post(&board, &repo, "A", vec![], Script::Conflict).await;
    let b = board
        .post(PostParams {
            run_id: Some(a.run_id.clone()),
            spec: JobSpec {
                title: "B".into(),
                deliverable: "Write B".into(),
                acceptance: vec!["File exists".into()],
                depends_on: vec![],
                max_retries: 1,
                workspace: Some(repo.to_string_lossy().to_string()),
                work: Some(WorkPolicy {
                    location: Location::Local,
                    target_branch: "main".into(),
                    check_command: Some("test -f shared.txt".into()),
                    failure_cleanup: FailureCleanup::Keep,
                }),
            },
            idempotency_key: "post-B".into(),
        })
        .await
        .unwrap()
        .job;
    let runner = worker(&home, board.clone(), sessions);
    assert_eq!(runner.run_once().await.unwrap().succeeded, 2);
    accept(&board, &a.id).await;
    accept(&board, &b.id).await;
    let integrator = Integrator::new(board, &home, aimx()).unwrap();
    assert_eq!(integrator.integrate(a.id).await.unwrap().state, IntegrationState::Integrated);
    let conflict = integrator.integrate(b.id).await.unwrap();
    assert_eq!(conflict.state, IntegrationState::Conflict);
    assert_eq!(conflict.conflict_files, vec!["shared.txt"]);
    assert!(std::fs::read_to_string(repo.join("shared.txt")).unwrap().contains("<<<<<<<"));
}

#[tokio::test]
async fn runner_restart_resumes_a_live_session_from_its_private_receipt() {
    let (_dir, repo, home, board, sessions) = setup(Script::Paused);
    let job = post(&board, &repo, "A", vec![], Script::Paused).await;
    let runner = worker(&home, board.clone(), Arc::clone(&sessions));
    let task = tokio::spawn({
        let runner = runner.clone();
        async move { runner.run_once().await }
    });
    for _ in 0..100 {
        if sessions.sessions.lock().unwrap().values().any(|s| s.summary.state == SessionState::Running) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(sessions.sessions.lock().unwrap().values().any(|s| s.summary.state == SessionState::Running));
    task.abort();
    let _ignored = task.await;
    drop(runner);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let resumed = worker(&home, board.clone(), Arc::clone(&sessions));
    let task = tokio::spawn(async move { resumed.run_once().await });
    for _ in 0..100 {
        if sessions.sessions.lock().unwrap().values().any(|s| s.attachments >= 2) {
            break;
        }
        if task.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        sessions.sessions.lock().unwrap().values().any(|s| s.attachments >= 2),
        "restart result: {:?}",
        if task.is_finished() { Some(task.await) } else { None }
    );
    {
        let map = sessions.sessions.lock().unwrap();
        let session = map.values().next().unwrap();
        std::fs::write(Path::new(&session.summary.meta.workspace).join("a.txt"), "A\n").unwrap();
        let _ignored = session.updates.send(SessionUpdate::TurnEnded { stop: StopReason::EndTurn });
    }
    let summary = task.await.unwrap().unwrap();
    assert_eq!(summary.succeeded, 1);
    assert!(board.show(job.id).await.unwrap().attempt.unwrap().cleanup_confirmed);
}

/// Real Codex session through the binary worker, independent review, and Git integration.
#[tokio::test]
#[ignore = "live: needs Codex credentials, network, and built aim/aimx binaries"]
async fn live_board_worker_codex_end_to_end() {
    let (_dir, repo, home, board, _sessions) = setup(Script::Separate);
    std::fs::write(repo.join(".gitignore"), "__pycache__/\n").unwrap();
    std::fs::write(repo.join("maths.py"), "def add(a, b):\n    raise NotImplementedError\n").unwrap();
    std::fs::write(repo.join("test_maths.py"),
        "import unittest\nfrom maths import add\n\nclass MathsTest(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "seed live board job"]);
    let job = board
        .post(PostParams {
            run_id: None,
            spec: JobSpec {
                title: "Implement add".into(),
                deliverable: "Replace the stub add(a, b) with correct addition and keep test_maths.py passing".into(),
                acceptance: vec!["python3 -m unittest -q passes".into()],
                depends_on: vec![],
                max_retries: 1,
                workspace: Some(repo.to_string_lossy().to_string()),
                work: Some(WorkPolicy {
                    location: Location::Local,
                    target_branch: "main".into(),
                    check_command: Some("python3 -m unittest -q".into()),
                    failure_cleanup: FailureCleanup::Keep,
                }),
            },
            idempotency_key: "live-add".into(),
        })
        .await
        .unwrap()
        .job;
    let binary = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("aim");
    let began = Instant::now();
    let output = tokio::process::Command::new(&binary)
        .env("AIM_HOME", &home)
        .args(["board", "work", "--provider", "codex", "--effort", "low", "--once", "--timeout-seconds", "180", "--aimx"])
        .arg(aimx())
        .arg("--json")
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["succeeded"], 1);
    let worked = began.elapsed();
    accept(&board, &job.id).await;
    let record = Integrator::new(board, &home, aimx()).unwrap().integrate(job.id).await.unwrap();
    assert_eq!(record.state, IntegrationState::Integrated);
    assert!(git(&repo, &["show", "main:maths.py"]).contains("return a + b"));
    eprintln!(
        "live board worker: work={worked:?}, total={:?}, input_tokens={}, output_tokens={}, cost_reported={}",
        began.elapsed(),
        summary["input_tokens"],
        summary["output_tokens"],
        summary["cost_reported"]
    );
    let stopped = tokio::process::Command::new(binary).env("AIM_HOME", &home).args(["daemon", "stop"]).output().await.unwrap();
    assert!(stopped.status.success(), "{}", String::from_utf8_lossy(&stopped.stderr));
}
