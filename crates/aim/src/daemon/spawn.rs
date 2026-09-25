//! Detached startup and bounded connection wait.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aim_proto::error::{ErrorCode, ProtoError};

use super::client::DaemonClient;
use super::socket_path;

/// Owns a daemon until startup succeeds. A failed or cancelled connection attempt must not
/// leave the process running after its caller has given up.
struct PendingDaemon(Option<Child>);

impl Drop for PendingDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.kill());
            drop(child.wait());
        }
    }
}

/// Connects to a running daemon, starting this binary as a detached daemon when needed.
///
/// # Errors
/// Returns a clear timeout or process-start error if no daemon becomes available.
pub async fn connect_or_spawn(home: &Path) -> Result<DaemonClient, ProtoError> {
    let exe = std::env::current_exe().map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("finding aim executable: {e}")))?;
    connect_or_spawn_executable(home, &exe).await
}

/// Connects or starts a selected aim executable; useful to launch a pinned binary.
///
/// # Errors
/// Returns a timeout or process-start error if no daemon becomes available.
pub async fn connect_or_spawn_executable(home: &Path, exe: &Path) -> Result<DaemonClient, ProtoError> {
    let socket = socket_path(home);
    if let Ok(client) = DaemonClient::connect(&socket).await {
        return Ok(client);
    }
    let logs = home.join("logs");
    fs::create_dir_all(&logs).map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("creating daemon logs: {e}")))?;
    fs::set_permissions(&logs, fs::Permissions::from_mode(0o700))
        .map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("securing daemon logs: {e}")))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("daemon.log"))
        .map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("opening daemon log: {e}")))?;
    log.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("securing daemon log: {e}")))?;
    // What this attempt's daemon writes starts here; it explains a failed start.
    let offset = log.metadata().map_or(0, |meta| meta.len());
    let stderr = log.try_clone().map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("cloning daemon log: {e}")))?;
    let mut command = Command::new(exe);
    command.arg("daemon").env("AIM_HOME", home).stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(stderr));
    command.process_group(0);
    let mut pending =
        PendingDaemon(Some(command.spawn().map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("spawning daemon: {e}")))?));
    let start = Instant::now();
    let mut delay = Duration::from_millis(20);
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(client) = DaemonClient::connect(&socket).await {
            if let Some(mut child) = pending.0.take() {
                if client.initialize_result().pid == child.id() {
                    let _reaper = std::thread::Builder::new().name("aim-daemon-reaper".into()).spawn(move || {
                        let _status = child.wait();
                    });
                } else {
                    // This candidate lost the race. It may still be migrating and could
                    // otherwise bind after the winner stops, resurrecting a test daemon.
                    drop(child.kill());
                    drop(child.wait());
                }
            }
            return Ok(client);
        }
        // A daemon that exited will never answer: say why now instead of after the full wait.
        if let Some(status) = pending.0.as_mut().and_then(|child| child.try_wait().ok().flatten()) {
            pending.0 = None;
            // It may have exited because another candidate won the socket.
            if let Ok(client) = DaemonClient::connect(&socket).await {
                return Ok(client);
            }
            return Err(ProtoError::new(ErrorCode::Unavailable, explain(&format!("the daemon exited ({status})"), &logs, offset)));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(250));
    }
    Err(ProtoError::new(ErrorCode::Timeout, explain("the daemon did not become ready within five seconds", &logs, offset)))
}

/// `failure`, followed by the last line this attempt's daemon logged, or a pointer to the log.
fn explain(failure: &str, logs: &Path, offset: u64) -> String {
    let path = logs.join("daemon.log");
    match last_line_since(&path, offset) {
        Some(line) => format!("{failure}: {line}"),
        None => format!("{failure}; see {}", path.display()),
    }
}

/// The last non-empty line written to `path` after `offset`, read from at most its last 4 KiB
/// and cut to 300 characters.
fn last_line_since(path: &Path, offset: u64) -> Option<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = offset.max(len.saturating_sub(4096));
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.take(4096).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    Some(line.chars().take(300).collect())
}
