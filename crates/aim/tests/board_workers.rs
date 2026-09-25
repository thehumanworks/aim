//! Scripted session boundary and real Git worktree integration for board workers.
#![expect(clippy::unwrap_used, reason = "isolated temporary repository assertions")]
#![expect(clippy::expect_used, reason = "isolated temporary repository assertions")]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aim::agent::ToolHost;
use aim::board::{Board, integration::IntegrationState};
use aim::daemon::{client::DaemonClient, socket_path};
use aim::harness::HarnessClient;
use aim::host::{BoxFuture, SessionClient, UpdateStream};
use aim::resources::{self, ResourceConfig, tools::AllowedTools};
use aim::workers::{Integrator, Runner, RunnerOptions};
use aim_proto::board::{FailureCleanup, JobSnapshot, JobSpec, JobState, PostParams, ReviewParams, WorkPolicy};
use aim_proto::conversation::{Part, StopReason};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionState,
    SessionSummary, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{SessionAgent, SessionMeta};
use aim_proto::harness::{FsList, FsListParams, FsRead, FsReadParams};
use aim_proto::ids::IdempotencyKey;
use serde_json::json;
use tempfile::TempDir;

/// The live test owns its daemon, including when an assertion or provider call fails.
struct ChildGuard(Child);

impl ChildGuard {
    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status.success();
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            drop(self.0.kill());
            drop(self.0.wait());
        }
    }
}

#[derive(Clone, Copy)]
enum Script {
    Separate,
    Conflict,
    Paused,
    Revision,
}

struct Session {
    summary: SessionSummary,
    updates: tokio::sync::broadcast::Sender<SessionUpdate>,
    attachments: usize,
}

struct Scripted {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    prompts: Arc<Mutex<Vec<String>>>,
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
                    agent: spec.agent.map(|name| SessionAgent {
                        name,
                        allow: Some(["Edit", "Glob", "Grep", "LS", "Read", "Write"].into_iter().map(str::to_owned).collect()),
                        deny: Vec::new(),
                    }),
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
        let prompts = Arc::clone(&self.prompts);
        let script = self.script;
        Box::pin(async move {
            let text = parts.iter().find_map(|part| if let Part::Text { text } = part { Some(text.as_str()) } else { None }).unwrap_or("");
            prompts.lock().unwrap().push(text.to_owned());
            let (workspace, updates) = {
                let mut map = sessions.lock().unwrap();
                let session = map.get_mut(&id).ok_or_else(unavailable)?;
                session.summary.state = SessionState::Running;
                session.summary.turns += 1;
                (session.summary.meta.workspace.clone(), session.updates.clone())
            };
            let is_a = text.contains("Title: A") || matches!(script, Script::Revision);
            if !matches!(script, Script::Paused) {
                let name = match script {
                    Script::Separate | Script::Revision => {
                        if is_a {
                            "a.txt"
                        } else {
                            "b.txt"
                        }
                    }
                    Script::Conflict => "shared.txt",
                    Script::Paused => "a.txt",
                };
                let content = if matches!(script, Script::Revision) && !text.contains("runner's check failed") {
                    "wrong\n"
                } else if is_a {
                    "A\n"
                } else {
                    "B\n"
                };
                std::fs::write(Path::new(&workspace).join(name), content)
                    .map_err(|err| ProtoError::new(ErrorCode::Unavailable, err.to_string()))?;
                let _ignored = updates.send(SessionUpdate::TurnEnded { stop: StopReason::EndTurn });
                sessions.lock().unwrap().get_mut(&id).ok_or_else(unavailable)?.summary.state = SessionState::Idle;
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

fn git_ref_exists(repo: &Path, name: &str) -> bool {
    std::process::Command::new("git").args(["show-ref", "--verify", "--quiet", name]).current_dir(repo).status().unwrap().success()
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
    let sessions = Arc::new(Scripted { sessions: Arc::new(Mutex::new(HashMap::new())), prompts: Arc::new(Mutex::new(Vec::new())), script });
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
        Script::Revision => "grep -qx 'A' a.txt || awk 'BEGIN { for(i=0;i<1000;i++) print \"failed line\"; exit 1 }'".into(),
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
    // The disposable checkout releases main so the integrator can own its checkout while
    // advancing the branch. A user checkout of main takes the explicit apply path below.
    git(&repo, &["switch", "--detach"]);
    let integrator = Integrator::new(board.clone(), &home, aimx()).unwrap();
    assert_eq!(integrator.integrate(a.id).await.unwrap().state, IntegrationState::Integrated);
    assert_eq!(integrator.integrate(b.id).await.unwrap().state, IntegrationState::Integrated);
    assert_eq!(git(&repo, &["show", "main:a.txt"]), "A");
    assert_eq!(git(&repo, &["show", "main:b.txt"]), "B");
}

#[tokio::test]
async fn concurrent_calls_on_one_integrator_advance_both_jobs() {
    let (_dir, repo, home, board, sessions) = setup(Script::Separate);
    let a = post(&board, &repo, "A", vec![], Script::Separate).await;
    let b = post(&board, &repo, "B", vec![], Script::Separate).await;
    assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 2);
    accept(&board, &a.id).await;
    accept(&board, &b.id).await;
    git(&repo, &["switch", "--detach"]);
    let integrator = Integrator::new(board, &home, aimx()).unwrap();
    let (first, second) = tokio::join!(integrator.integrate(a.id), integrator.integrate(b.id));
    assert_eq!(first.unwrap().state, IntegrationState::Integrated);
    assert_eq!(second.unwrap().state, IntegrationState::Integrated);
    assert_eq!(git(&repo, &["show", "main:a.txt"]), "A");
    assert_eq!(git(&repo, &["show", "main:b.txt"]), "B");
}

/// A branch checked out by the user cannot be advanced by the integrator. The saved result
/// remains reachable until the user explicitly applies it in that checkout.
#[tokio::test]
async fn checked_out_target_keeps_rescue_until_explicit_apply() {
    let (_dir, repo, home, board, sessions) = setup(Script::Separate);
    let job = post(&board, &repo, "A", vec![], Script::Separate).await;
    assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 1);
    accept(&board, &job.id).await;
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let integrator = Integrator::new(board.clone(), &home, aimx()).unwrap();
    let result = integrator.integrate(job.id.clone()).await.unwrap();
    assert_eq!(result.state, IntegrationState::Failed);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
    assert!(!repo.join("a.txt").exists(), "the user checkout was not updated");
    let rescue = result.rescue_ref.as_deref().expect("saved rescue ref");
    assert!(git_ref_exists(&repo, rescue));
    assert_eq!(git(&repo, &["rev-parse", rescue]), result.result_commit.unwrap());

    // Git refuses a fast-forward that would replace an untracked user file.
    std::fs::write(repo.join("a.txt"), "user work\n").unwrap();
    assert!(integrator.apply(job.id.clone()).await.is_err());
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "user work\n");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
    assert!(git_ref_exists(&repo, rescue));

    std::fs::remove_file(repo.join("a.txt")).unwrap();
    let applied = integrator.apply(job.id).await.unwrap();
    assert_eq!(applied.state, IntegrationState::Integrated);
    assert_eq!(applied.rescue_ref, None);
    assert!(!git_ref_exists(&repo, rescue));
    assert_eq!(git(&repo, &["show", "main:a.txt"]), "A");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "A\n");
}

#[tokio::test]
async fn explicit_disposal_removes_unapplied_rescue() {
    let (_dir, repo, home, board, sessions) = setup(Script::Separate);
    let job = post(&board, &repo, "A", vec![], Script::Separate).await;
    assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 1);
    accept(&board, &job.id).await;
    let before = git(&repo, &["rev-parse", "main"]);
    let integrator = Integrator::new(board, &home, aimx()).unwrap();
    let failed = integrator.integrate(job.id.clone()).await.unwrap();
    assert_eq!(failed.state, IntegrationState::Failed);
    let rescue = failed.rescue_ref.unwrap();
    assert!(git_ref_exists(&repo, &rescue));
    let disposed = integrator.discard_rescue(job.id).await.unwrap();
    assert_eq!(disposed.state, IntegrationState::Failed);
    assert_eq!(disposed.rescue_ref, None);
    assert!(!git_ref_exists(&repo, &rescue));
    assert_eq!(git(&repo, &["rev-parse", "main"]), before);
}

/// Child processes stop at each durable boundary: before a result, with a saved result, and
/// after the target moved. A fresh integrator must reconcile OIDs and prune the old scratch.
#[tokio::test]
async fn integration_recovers_after_process_faults() {
    if let Some(repo) = std::env::var_os("AIM_BOARD_TEST_REPO") {
        let repo = PathBuf::from(repo);
        let home = PathBuf::from(std::env::var_os("AIM_BOARD_TEST_HOME").unwrap());
        let job_id = std::env::var("AIM_BOARD_TEST_JOB").unwrap();
        let stage = std::env::var("AIM_BOARD_TEST_STAGE").unwrap();
        let board = Board::open(&home.join("aim.db")).unwrap();
        let integrator = Integrator::new(board.clone(), &home, aimx()).unwrap();
        assert!(integrator.integrate(job_id.clone()).await.is_err(), "the injected fault must interrupt integration");
        let intent = board.integration(job_id).await.unwrap();
        assert_eq!(intent.state, IntegrationState::Integrating);
        assert!(intent.scratch_path.is_some(), "startup recovery has a recorded scratch path");
        if stage == "after_intent" || stage == "after_commit" {
            assert!(intent.result_commit.is_none());
        } else {
            assert!(intent.result_commit.is_some(), "the result was durably recorded");
        }
        let observed = git(&repo, &["rev-parse", "main"]);
        if stage == "after_target" {
            assert_eq!(Some(observed.as_str()), intent.result_commit.as_deref());
        } else {
            assert_eq!(Some(observed.as_str()), intent.target_head.as_deref());
        }
        return;
    }

    for stage in ["after_intent", "after_result", "after_target"] {
        let (_dir, repo, home, board, sessions) = setup(Script::Separate);
        let job = post(&board, &repo, "A", vec![], Script::Separate).await;
        assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 1);
        accept(&board, &job.id).await;
        git(&repo, &["switch", "--detach"]);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "integration_recovers_after_process_faults", "--nocapture"])
            .env("AIM_BOARD_TEST_REPO", &repo)
            .env("AIM_BOARD_TEST_HOME", &home)
            .env("AIM_BOARD_TEST_JOB", &job.id)
            .env("AIM_BOARD_TEST_STAGE", stage)
            .env("AIM_BOARD_TEST_FAULT", format!("{}:{stage}", job.id))
            .output()
            .unwrap();
        assert!(child.status.success(), "{stage}: {}", String::from_utf8_lossy(&child.stderr));

        let interrupted = board.integration(job.id.clone()).await.unwrap();
        let abandoned = PathBuf::from(interrupted.scratch_path.unwrap());
        let integrator = Integrator::new(board.clone(), &home, aimx()).unwrap();
        let recovered = integrator.integrate(job.id).await.unwrap();
        assert_eq!(recovered.state, IntegrationState::Integrated, "{stage}");
        assert_eq!(Some(git(&repo, &["rev-parse", "main"])), recovered.result_commit, "{stage}");
        assert_eq!(recovered.scratch_path, None, "{stage}");
        assert_eq!(recovered.rescue_ref, None, "{stage}");
        assert!(!abandoned.exists(), "{stage}");
    }
}

/// A merge commit made before its ledger result is saved is recovered and rescued even if the
/// source branch moves before the next integrator starts.
#[tokio::test]
async fn committed_scratch_result_is_rescued_after_source_moves() {
    let (dir, repo, home, board, sessions) = setup(Script::Separate);
    let job = post(&board, &repo, "A", vec![], Script::Separate).await;
    assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 1);
    accept(&board, &job.id).await;
    git(&repo, &["switch", "--detach"]);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "integration_recovers_after_process_faults", "--nocapture"])
        .env("AIM_BOARD_TEST_REPO", &repo)
        .env("AIM_BOARD_TEST_HOME", &home)
        .env("AIM_BOARD_TEST_JOB", &job.id)
        .env("AIM_BOARD_TEST_STAGE", "after_commit")
        .env("AIM_BOARD_TEST_FAULT", format!("{}:after_commit", job.id))
        .output()
        .unwrap();
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
    let before = board.integration(job.id.clone()).await.unwrap();
    assert_eq!(before.state, IntegrationState::Integrating);
    assert_eq!(before.result_commit, None);
    let scratch = PathBuf::from(before.scratch_path.unwrap());
    let source_ref = format!("refs/heads/{}", before.source_branch);
    let accepted = git(&repo, &["rev-parse", &source_ref]);
    let tamper = dir.path().join("unreviewed-after-commit");
    git(&repo, &["worktree", "add", "--detach", tamper.to_str().unwrap(), &accepted]);
    std::fs::write(tamper.join("unreviewed.txt"), "unreviewed\n").unwrap();
    git(&tamper, &["add", "unreviewed.txt"]);
    git(&tamper, &["commit", "-m", "unreviewed"]);
    let unreviewed = git(&tamper, &["rev-parse", "HEAD"]);
    git(&repo, &["update-ref", &source_ref, &unreviewed, &accepted]);
    git(&repo, &["worktree", "remove", "--force", tamper.to_str().unwrap()]);

    let recovered = Integrator::new(board, &home, aimx()).unwrap().integrate(job.id).await.unwrap();
    assert_eq!(recovered.state, IntegrationState::Failed);
    let result = recovered.result_commit.expect("recovered merge result");
    let rescue = recovered.rescue_ref.expect("retained rescue ref");
    assert_eq!(git(&repo, &["rev-parse", &rescue]), result);
    assert_eq!(git(&repo, &["show", &format!("{result}:a.txt")]), "A");
    assert!(!git(&repo, &["ls-tree", "-r", "--name-only", &result]).lines().any(|path| path == "unreviewed.txt"));
    assert!(!scratch.exists());
}

/// The source ref moves after the accepted OID is checked. Git must merge the pinned OID,
/// not resolve the branch name again after the interleaving.
#[tokio::test]
async fn source_ref_move_cannot_add_unreviewed_commit_to_merge() {
    if let Some(repo) = std::env::var_os("AIM_BOARD_RACE_REPO") {
        let _repo = PathBuf::from(repo);
        let home = PathBuf::from(std::env::var_os("AIM_BOARD_RACE_HOME").unwrap());
        let job_id = std::env::var("AIM_BOARD_RACE_JOB").unwrap();
        let board = Board::open(&home.join("aim.db")).unwrap();
        let record = Integrator::new(board, &home, aimx()).unwrap().integrate(job_id).await.unwrap();
        assert_eq!(record.state, IntegrationState::Failed);
        assert!(record.result_commit.is_some(), "the pinned source was merged before the stale-ref check");
        assert!(record.rescue_ref.is_some(), "the saved result remains reachable");
        return;
    }

    let (dir, repo, home, board, sessions) = setup(Script::Separate);
    let job = post(&board, &repo, "A", vec![], Script::Separate).await;
    assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 1);
    accept(&board, &job.id).await;
    git(&repo, &["switch", "--detach"]);
    let source_branch = board.integration(job.id.clone()).await.unwrap().source_branch;
    let source_ref = format!("refs/heads/{source_branch}");
    let accepted = git(&repo, &["rev-parse", &source_ref]);
    let pause = dir.path().join("pause");
    std::fs::create_dir(&pause).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "source_ref_move_cannot_add_unreviewed_commit_to_merge", "--nocapture"])
        .env("AIM_BOARD_RACE_REPO", &repo)
        .env("AIM_BOARD_RACE_HOME", &home)
        .env("AIM_BOARD_RACE_JOB", &job.id)
        .env("AIM_BOARD_TEST_PAUSE_AFTER_SOURCE_CHECK", &pause)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !pause.join("ready").exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!("integrator exited before source-check pause: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !pause.join("ready").exists() {
        if child.try_wait().unwrap().is_none() {
            drop(child.kill());
        }
        let output = child.wait_with_output().unwrap();
        panic!("integrator did not reach source-check pause: {}", String::from_utf8_lossy(&output.stderr));
    }

    let tamper = dir.path().join("unreviewed");
    git(&repo, &["worktree", "add", "--detach", tamper.to_str().unwrap(), &accepted]);
    std::fs::write(tamper.join("unreviewed.txt"), "never accepted\n").unwrap();
    git(&tamper, &["add", "unreviewed.txt"]);
    git(&tamper, &["commit", "-m", "unreviewed"]);
    let unreviewed = git(&tamper, &["rev-parse", "HEAD"]);
    git(&repo, &["update-ref", &source_ref, &unreviewed, &accepted]);
    git(&repo, &["worktree", "remove", "--force", tamper.to_str().unwrap()]);
    std::fs::write(pause.join("release"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let failed = board.integration(job.id).await.unwrap();
    assert_eq!(failed.state, IntegrationState::Failed);
    let result = failed.result_commit.unwrap();
    assert_eq!(git(&repo, &["show", &format!("{result}:a.txt")]), "A");
    assert!(!git(&repo, &["ls-tree", "-r", "--name-only", &result]).lines().any(|path| path == "unreviewed.txt"));
    assert!(git_ref_exists(&repo, failed.rescue_ref.as_deref().unwrap()));
    assert_eq!(git(&repo, &["rev-parse", &source_ref]), unreviewed, "the ref really moved during integration");
}

/// A user can check out main while the merge is being prepared. The integrator must discover
/// that Git will not grant its scratch worktree ownership and retain the result for apply.
#[tokio::test]
async fn target_checked_out_during_integration_stays_untouched() {
    if let Some(home) = std::env::var_os("AIM_BOARD_TARGET_RACE_HOME") {
        let home = PathBuf::from(home);
        let job_id = std::env::var("AIM_BOARD_TARGET_RACE_JOB").unwrap();
        let board = Board::open(&home.join("aim.db")).unwrap();
        let failed = Integrator::new(board, &home, aimx()).unwrap().integrate(job_id).await.unwrap();
        assert_eq!(failed.state, IntegrationState::Failed);
        assert!(failed.result_commit.is_some());
        assert!(failed.rescue_ref.is_some());
        return;
    }

    let (dir, repo, home, board, sessions) = setup(Script::Separate);
    let job = post(&board, &repo, "A", vec![], Script::Separate).await;
    assert_eq!(worker(&home, board.clone(), sessions).run_once().await.unwrap().succeeded, 1);
    accept(&board, &job.id).await;
    let before = git(&repo, &["rev-parse", "main"]);
    git(&repo, &["switch", "--detach"]);
    let pause = dir.path().join("pause");
    std::fs::create_dir(&pause).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "target_checked_out_during_integration_stays_untouched", "--nocapture"])
        .env("AIM_BOARD_TARGET_RACE_HOME", &home)
        .env("AIM_BOARD_TARGET_RACE_JOB", &job.id)
        .env("AIM_BOARD_TEST_PAUSE_AFTER_SOURCE_CHECK", &pause)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !pause.join("ready").exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!("integrator exited before source-check pause: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !pause.join("ready").exists() {
        if child.try_wait().unwrap().is_none() {
            drop(child.kill());
        }
        let output = child.wait_with_output().unwrap();
        panic!("integrator did not reach source-check pause: {}", String::from_utf8_lossy(&output.stderr));
    }

    git(&repo, &["switch", "main"]);
    std::fs::write(pause.join("release"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let failed = board.integration(job.id).await.unwrap();
    assert_eq!(failed.state, IntegrationState::Failed);
    assert!(git_ref_exists(&repo, failed.rescue_ref.as_deref().unwrap()));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
    assert!(!repo.join("a.txt").exists());
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());
}

#[tokio::test]
async fn failing_runner_check_returns_bounded_feedback_for_a_file_only_revision() {
    let (_dir, repo, home, board, sessions) = setup(Script::Revision);
    let job = post(&board, &repo, "A", vec![], Script::Revision).await;
    let runner = worker(&home, board.clone(), Arc::clone(&sessions));
    let summary = runner.run_once().await.unwrap();
    assert_eq!((summary.succeeded, summary.failed), (1, 0));
    {
        let prompts = sessions.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[1].contains("runner's check failed"));
        assert!(prompts[1].contains("failed line"));
        assert!(prompts[1].chars().count() < 8_500, "failure feedback must stay bounded");
    }
    assert!(board.show(job.id).await.unwrap().attempt.unwrap().cleanup_confirmed);
}

#[tokio::test]
async fn worktree_inside_aim_home_is_refused_before_claim() {
    let (dir, repo, _home, board, sessions) = setup(Script::Separate);
    let job = post(&board, &repo, "A", vec![], Script::Separate).await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = worker(dir.path(), board.clone(), sessions);
    let err = runner.run_once().await.unwrap_err();
    assert!(err.contains("overlaps AIM_HOME"), "{err}");
    assert_eq!(board.show(job.id).await.unwrap().state, JobState::Posted);
    assert_eq!(std::fs::read_dir(dir.path().join("run/board-workers")).unwrap().count(), 1, "only the worker lock exists");
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
    git(&repo, &["switch", "--detach"]);
    let integrator = Integrator::new(board, &home, aimx()).unwrap();
    assert_eq!(integrator.integrate(a.id).await.unwrap().state, IntegrationState::Integrated);
    let conflict = integrator.integrate(b.id).await.unwrap();
    assert_eq!(conflict.state, IntegrationState::Conflict);
    assert_eq!(conflict.conflict_files, vec!["shared.txt"]);
    // The conflict stays in the removed detached worktree: the user's checkout is untouched,
    // clean, and not mid-merge (review finding N3).
    assert!(!std::fs::read_to_string(repo.join("shared.txt")).unwrap().contains("<<<<<<<"));
    let status = std::process::Command::new("git").args(["status", "--porcelain"]).current_dir(&repo).output().unwrap();
    assert!(status.stdout.is_empty(), "{}", String::from_utf8_lossy(&status.stdout));
    assert!(!repo.join(".git/MERGE_HEAD").exists(), "no half-done merge");
}

/// Review finding N3: a check that fails on the merged result never leaves the user's checkout mid-merge,
/// and the branch is not moved.
#[tokio::test]
async fn a_failed_check_leaves_the_users_checkout_untouched() {
    let (_dir, repo, home, board, sessions) = setup(Script::Separate);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let job = board
        .post(PostParams {
            run_id: None,
            spec: JobSpec {
                title: "A".into(),
                deliverable: "Write A output".into(),
                acceptance: vec!["File exists".into()],
                depends_on: Vec::new(),
                max_retries: 1,
                workspace: Some(repo.to_string_lossy().to_string()),
                work: Some(WorkPolicy {
                    location: Location::Local,
                    target_branch: "main".into(),
                    // Passes in the attempt; fails once main gains blocker.txt below.
                    check_command: Some("test ! -f blocker.txt".into()),
                    failure_cleanup: FailureCleanup::Keep,
                }),
            },
            idempotency_key: "post-failing-check".into(),
        })
        .await
        .unwrap()
        .job;
    let runner = worker(&home, board.clone(), sessions);
    assert_eq!(runner.run_once().await.unwrap().succeeded, 1);
    accept(&board, &job.id).await;
    std::fs::write(repo.join("blocker.txt"), "x").unwrap();
    git(&["add", "blocker.txt"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "blocker"]);
    let before = git(&["rev-parse", "HEAD"]);
    let integrator = Integrator::new(board, &home, aimx()).unwrap();
    assert_eq!(integrator.integrate(job.id).await.unwrap().state, IntegrationState::Failed);
    assert_eq!(git(&["rev-parse", "HEAD"]), before, "the branch did not move");
    assert!(git(&["status", "--porcelain"]).is_empty(), "the checkout is clean");
    assert!(!repo.join(".git/MERGE_HEAD").exists(), "no half-done merge");
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one restart regression also checks the worker's exact read, write, and command boundary")]
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
    let attempt = board.show(job.id.clone()).await.unwrap().attempt.unwrap();
    let agent_name = format!("board-worker-{}", attempt.id);
    let catalog = resources::discover(&ResourceConfig::user(&home), None, "local", "").await;
    let policy = catalog.agent(&agent_name).expect("private board worker agent").tools.clone();
    assert_eq!(
        policy.allow.as_ref().unwrap().iter().map(String::as_str).collect::<Vec<_>>(),
        ["Edit", "Glob", "Grep", "LS", "Read", "Write"]
    );
    let workspace = sessions.sessions.lock().unwrap().values().next().unwrap().summary.meta.workspace.clone();
    let harness = HarnessClient::spawn_stdio(aimx().to_str().unwrap(), &workspace).await.unwrap();
    let workspace_id = harness.workspace().id.clone();
    let receipt = home.join("run/board-workers").join(format!("{}.json", attempt.id));
    for path in [&receipt, &receipt.canonicalize().unwrap()] {
        let denied = harness
            .peer()
            .call::<FsRead>(FsReadParams {
                workspace: workspace_id.clone(),
                path: path.to_string_lossy().into_owned(),
                range: None,
                scope: None,
                hash: false,
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::Denied, "fs.read cannot leave the attempt worktree");
    }
    assert_eq!(
        harness
            .peer()
            .call::<FsList>(FsListParams {
                workspace: workspace_id,
                path: home.to_string_lossy().into_owned(),
                limit: None,
                page_token: None,
                include_hidden: true,
                scope: None,
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied,
        "fs.list cannot enumerate AIM_HOME"
    );
    let tools = AllowedTools::new(Arc::new(harness), policy, agent_name.clone());
    assert!(!tools.specs().iter().any(|tool| tool.name == "Bash" || tool.name == "run_code"));
    let key = || IdempotencyKey::new(uuid::Uuid::new_v4().to_string());
    let blocked = tools
        .call("Bash".into(), json!({"command":"aim board review job attempt --reviewer lead --decision accepted"}), key())
        .await
        .unwrap_err();
    assert_eq!(blocked.code, ErrorCode::Denied, "the model cannot open a board owner connection via Bash");
    assert_eq!(
        tools.call("Read".into(), json!({"file_path": receipt}), key()).await.unwrap_err().code,
        ErrorCode::Denied,
        "the model cannot read its own token receipt"
    );
    let other = home.join("run/board-workers/other.json");
    std::fs::write(&other, b"other attempt token").unwrap();
    assert_eq!(
        tools.call("Read".into(), json!({"file_path": other}), key()).await.unwrap_err().code,
        ErrorCode::Denied,
        "the model cannot read another attempt's token"
    );
    let alias = Path::new(&workspace).join("receipt-link");
    std::os::unix::fs::symlink(&receipt, &alias).unwrap();
    assert_eq!(
        tools.call("Read".into(), json!({"file_path": alias}), key()).await.unwrap_err().code,
        ErrorCode::Denied,
        "a worktree symlink cannot cross the root"
    );
    assert_eq!(
        tools.call("Write".into(), json!({"file_path": home.join("control"), "content":"x"}), key()).await.unwrap_err().code,
        ErrorCode::Denied,
        "the model cannot write controller state"
    );
    assert_eq!(
        tools
            .call(
                "Write".into(),
                json!({"file_path": home.join("agents").join(format!("{agent_name}.md")), "content":"tools: [Bash]"}),
                key()
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied,
        "the model cannot modify its private agent definition"
    );
    std::fs::remove_file(alias).unwrap();
    std::fs::remove_file(other).unwrap();
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
    let mut daemon = ChildGuard(
        std::process::Command::new(&binary)
            .arg("daemon")
            .env("AIM_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let startup = Instant::now();
    loop {
        if let Ok(client) = DaemonClient::connect(&socket_path(&home)).await {
            client.disconnect();
            break;
        }
        assert!(daemon.0.try_wait().unwrap().is_none(), "live test daemon exited before becoming ready");
        assert!(startup.elapsed() < Duration::from_secs(30), "live test daemon did not become ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let began = Instant::now();
    let output = tokio::process::Command::new(&binary)
        .env("AIM_HOME", &home)
        .args(["board", "work", "--provider", "codex", "--effort", "low", "--once", "--timeout-seconds", "180", "--aimx"])
        .arg(aimx())
        .arg("--json")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["succeeded"], 1);
    let worked = began.elapsed();
    accept(&board, &job.id).await;
    git(&repo, &["switch", "--detach"]);
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
    assert!(daemon.wait_for_exit(Duration::from_secs(10)), "live test daemon did not exit cleanly");
}
