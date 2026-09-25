//! REV7 daemon idle, signal and failed-store regressions.
#![expect(clippy::unwrap_used, reason = "isolated child-process test setup")]
#![expect(clippy::panic, reason = "test helper reports a failed child deadline")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aim::daemon::{server, socket_path};
use aim::host::{BoxFuture, SessionClient, UpdateStream};
use aim_proto::conversation::Part;
use aim_proto::daemon::{PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionSummary};
use aim_proto::error::{ErrorCode, ProtoError};

#[derive(Clone, Copy)]
enum Mode {
    Fast,
    SlowOk,
    SlowError,
}

struct ProbeHost {
    mode: Mode,
    marker: PathBuf,
    lists: Arc<AtomicUsize>,
}

/// Reap a test daemon even when an assertion fails before the explicit stop.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _killed = self.0.kill();
            let _reaped = self.0.wait();
        }
    }
}

fn unavailable<T: Send + 'static>() -> BoxFuture<Result<T, ProtoError>> {
    Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "unused probe route")) })
}

impl SessionClient for ProbeHost {
    fn create(&self, _spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        unavailable()
    }

    fn list(&self, _params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        let mode = self.mode;
        let marker = self.marker.clone();
        let lists = Arc::clone(&self.lists);
        Box::pin(async move {
            lists.fetch_add(1, Ordering::SeqCst);
            if matches!(mode, Mode::SlowOk | Mode::SlowError) {
                let _written = std::fs::write(marker, "in list");
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            if matches!(mode, Mode::SlowError) { Err(ProtoError::new(ErrorCode::Internal, "bad store row")) } else { Ok(Vec::new()) }
        })
    }

    fn live_summaries(&self) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        if matches!(self.mode, Mode::Fast) { Box::pin(async { Ok(Vec::new()) }) } else { self.list(SessionListParams::default()) }
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
        Box::pin(async { Ok(()) })
    }
}

fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

async fn wait_for(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected child marker at {}", path.display());
}

async fn wait_child(child: &mut Child) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > Duration::from_secs(3) {
            let _killed = child.kill();
            let _reaped = child.wait();
            panic!("daemon did not exit after SIGTERM");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn signal_case(mode: &str) {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rev7_daemon_signal_child", "--nocapture"])
            .env("REV7_CHILD_MODE", mode)
            .env("REV7_CHILD_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(&socket).await;
    wait_for(&dir.path().join("inside-list")).await;
    let signaled = Command::new("kill").args(["-TERM", &child.0.id().to_string()]).status().unwrap();
    assert!(signaled.success());
    let status = wait_child(&mut child.0).await;
    assert!(status.success(), "daemon child exited with failure");
    assert!(dir.path().join("shutdown-ran").exists(), "shutdown was skipped");
    assert!(!socket.exists(), "daemon socket survived shutdown");
}

/// Subprocess target: signals are process-wide and must not affect the rest of the test suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_daemon_signal_child() {
    let Ok(mode) = std::env::var("REV7_CHILD_MODE") else { return };
    let home = PathBuf::from(std::env::var("REV7_CHILD_HOME").unwrap());
    let host: Arc<dyn SessionClient> = Arc::new(ProbeHost {
        mode: match mode.as_str() {
            "slow_error" => Mode::SlowError,
            "fd" => Mode::Fast,
            _ => Mode::SlowOk,
        },
        marker: home.join("inside-list"),
        lists: Arc::new(AtomicUsize::new(0)),
    });
    let marked = home.join("shutdown-ran");
    let idle_exit = if mode == "fd" { None } else { Some(Duration::from_secs(3600)) };
    let result = server::serve_with_shutdown(&home, &socket_path(&home), idle_exit, host, async move {
        std::fs::write(marked, "done").map_err(|e| ProtoError::new(ErrorCode::Internal, format!("marking shutdown: {e}")))?;
        Ok(())
    })
    .await;
    assert!(result.is_ok(), "daemon child returned {result:?}");
}

/// A signal delivered during a slow idle query must remain pending until the select resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_signal_during_idle_check_stops_daemon() {
    signal_case("slow_ok").await;
}

/// Bad stored data must neither kill the listener nor skip shutdown after a signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_bad_list_keeps_daemon_and_runs_shutdown() {
    signal_case("slow_error").await;
}

/// Idle checks must inspect live actor state without reading the durable session index.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_idle_tick_never_lists_stored_sessions() {
    let dir = private_tempdir();
    let lists = Arc::new(AtomicUsize::new(0));
    let host: Arc<dyn SessionClient> =
        Arc::new(ProbeHost { mode: Mode::Fast, marker: dir.path().join("unused"), lists: Arc::clone(&lists) });
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        server::serve(dir.path(), &socket_path(dir.path()), Some(Duration::from_millis(450)), host),
    )
    .await;
    assert!(matches!(result, Ok(Ok(()))), "idle daemon did not exit cleanly");
    assert_eq!(lists.load(Ordering::SeqCst), 0, "idle tick read the stored session index");
}

/// A daemon that accepts a socket but never answers `initialize` must not hang callers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_handshake_has_a_deadline() {
    let dir = private_tempdir();
    let socket = dir.path().join("silent.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let stalled = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let started = Instant::now();
    let result = aim::daemon::client::DaemonClient::connect(&socket).await;
    assert!(matches!(result, Err(ProtoError { code: ErrorCode::Timeout, .. })));
    assert!(started.elapsed() < Duration::from_secs(3));
    stalled.abort();
}

/// An fd shortage is transient: releasing held clients lets the same daemon accept again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live subprocess with a lowered file-descriptor limit"]
async fn live_rev7_emfile_accept_recovers() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let mut child = ChildGuard(
        Command::new("sh")
            .arg("-c")
            .arg("ulimit -n 48; exec \"$REV7_TEST_EXE\" --exact rev7_daemon_signal_child --nocapture")
            .env("REV7_TEST_EXE", std::env::current_exe().unwrap())
            .env("REV7_CHILD_MODE", "fd")
            .env("REV7_CHILD_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(&socket).await;
    let mut sockets = Vec::new();
    for _ in 0..80 {
        if let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_millis(200), tokio::net::UnixStream::connect(&socket)).await {
            sockets.push(stream);
        } else {
            break;
        }
    }
    assert!(sockets.len() >= 25, "could not pressure the daemon's fd limit");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(child.0.try_wait().unwrap().is_none(), "an accept error killed the daemon");
    drop(sockets);
    let connected = tokio::time::timeout(Duration::from_secs(2), aim::daemon::client::DaemonClient::connect(&socket)).await;
    assert!(matches!(connected, Ok(Ok(_))), "daemon did not recover after fd release");
    let signaled = Command::new("kill").args(["-TERM", &child.0.id().to_string()]).status().unwrap();
    assert!(signaled.success());
    assert!(wait_child(&mut child.0).await.success());
    assert!(dir.path().join("shutdown-ran").exists());
}
