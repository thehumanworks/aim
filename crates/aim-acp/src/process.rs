//! The agent subprocess: spawning, the bounded NDJSON transport, stderr capture and teardown.
//!
//! The agent runs as the leader of its own process group. `claude-agent-acp` is a node script that
//! starts a native `claude` child; killing only the direct child orphans that child (adapter
//! issue #1011), so teardown signals the whole group.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::{SinkExt, StreamExt as _};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec, LinesCodecError};

use crate::config::AcpAgentConfig;
use crate::error::AcpError;

/// Largest single JSON-RPC line accepted from the agent (session replays can carry images).
pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
/// Bytes of agent stderr kept for diagnostics.
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Direction of a line on the wire, as seen by aim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireDirection {
    /// aim → agent.
    Outgoing,
    /// agent → aim.
    Incoming,
}

/// Observes every JSON-RPC line (fixture capture, debugging). Lines can contain the user's data
/// and account details: a tap must not log them unredacted.
pub type WireTap = Arc<dyn Fn(WireDirection, &str) + Send + Sync>;

/// Locks a mutex, recovering the data if a panicking thread poisoned it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A bounded tail of the agent's stderr.
#[derive(Default)]
pub struct StderrTail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl core::fmt::Debug for StderrTail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StderrTail").field("bytes", &self.bytes.len()).field("truncated", &self.truncated).finish()
    }
}

impl StderrTail {
    /// Appends bytes, dropping the oldest beyond [`STDERR_TAIL_BYTES`].
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes.iter().copied());
        let overflow = self.bytes.len().saturating_sub(STDERR_TAIL_BYTES);
        if overflow > 0 {
            self.truncated = true;
            drop(self.bytes.drain(..overflow));
        }
    }

    /// A safe diagnostic summary. Free-form stderr can contain names or secrets with no
    /// recognizable syntax, so it must not leave this process as text.
    pub fn text(&self) -> String {
        if self.bytes.is_empty() {
            String::new()
        } else {
            format!("agent stderr omitted ({} bytes captured, truncated: {})", self.bytes.len(), self.truncated)
        }
    }
}

/// How the agent process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exit {
    /// Exit code; `None` when a signal killed it.
    pub code: Option<i32>,
}

/// Process state shared between the client and the background tasks.
#[derive(Debug)]
pub struct ProcessState {
    /// Process id (also the process-group id).
    pub pid: Option<u32>,
    /// Captured stderr.
    pub stderr: Mutex<StderrTail>,
    /// Set once the direct child has been reaped.
    pub exited: AtomicBool,
    /// Set once the process has exited.
    pub exit: watch::Sender<Option<Exit>>,
}

impl ProcessState {
    /// The redacted stderr tail.
    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr).text()
    }

    /// The error describing the agent's exit (or a closed stream while it still runs).
    pub fn exited_error(&self) -> AcpError {
        let code = self.exit.borrow().and_then(|exit| exit.code);
        AcpError::AgentExited { code, stderr_tail: self.stderr_tail() }
    }

    /// Kills the agent's process group (the adapter and everything it started).
    pub fn kill_group(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()).and_then(rustix::process::Pid::from_raw) {
            // ESRCH just means the group is already gone.
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

/// Kills the agent's process group when the client is dropped, unless it already exited.
#[derive(Debug)]
pub struct ProcessGuard(pub Arc<ProcessState>);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if !self.0.exited.load(Ordering::Acquire) {
            self.0.kill_group();
        }
    }
}

/// A spawned agent: its pipes and shared state. The caller turns the pipes into a transport.
pub struct Spawned {
    /// The agent's stdin.
    pub stdin: ChildStdin,
    /// The agent's stdout.
    pub stdout: ChildStdout,
    /// Shared state; the reaper and stderr tasks are already running.
    pub state: Arc<ProcessState>,
}

/// Spawns the agent described by `config` and starts its stderr and reaper tasks.
///
/// # Errors
///
/// [`AcpError::AgentNotFound`] when the executable does not exist, [`AcpError::Spawn`] when the OS
/// refuses to start it.
pub fn spawn(config: &AcpAgentConfig) -> Result<Spawned, AcpError> {
    let program = config.resolve_command()?;
    let mut command = Command::new(&program);
    command
        .args(&config.args)
        .envs(&config.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AcpError::AgentNotFound {
            command: "<redacted>".to_owned(),
            hint: "the executable or its interpreter (e.g. `node` for claude-agent-acp) is missing; run aim under mise".to_owned(),
        },
        _ => AcpError::Spawn { command: "<redacted>".to_owned(), message: format!("{:?}", error.kind()) },
    })?;
    let missing = |what: &str| AcpError::Spawn { command: "<redacted>".to_owned(), message: format!("no {what} pipe") };
    let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
    let (exit, _) = watch::channel(None);
    let state = Arc::new(ProcessState { pid: child.id(), stderr: Mutex::new(StderrTail::default()), exited: AtomicBool::new(false), exit });
    tokio::spawn(drain_stderr(stderr, Arc::clone(&state)));
    tokio::spawn(reap(child, Arc::clone(&state)));
    Ok(Spawned { stdin, stdout, state })
}

async fn drain_stderr(mut stderr: ChildStderr, state: Arc<ProcessState>) {
    let mut buffer = vec![0_u8; 8 * 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Some(bytes) = buffer.get(..read) {
                    lock(&state.stderr).push(bytes);
                }
            }
        }
    }
}

async fn reap(mut child: Child, state: Arc<ProcessState>) {
    let code = child.wait().await.ok().and_then(|status| status.code());
    state.exited.store(true, Ordering::Release);
    // The adapter may leave children behind in its group; stop them while the group id is still
    // certainly ours.
    state.kill_group();
    state.exit.send_replace(Some(Exit { code }));
}

/// Builds the NDJSON transport over any byte pipes: lines are capped at [`MAX_LINE_BYTES`] and
/// every line passes through `tap`.
pub fn line_transport<R, W>(
    reader: R,
    writer: W,
    tap: Option<WireTap>,
) -> agent_client_protocol::Lines<
    impl futures::Sink<String, Error = std::io::Error> + Send + 'static,
    impl futures::Stream<Item = std::io::Result<String>> + Send + 'static,
>
where
    R: tokio::io::AsyncRead + Send + 'static,
    W: tokio::io::AsyncWrite + Send + 'static,
{
    let incoming_tap = tap.clone();
    let incoming = FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_LINE_BYTES)).map(move |line| {
        let line = line.map_err(|error| match error {
            LinesCodecError::MaxLineLengthExceeded => {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("agent sent a line over {MAX_LINE_BYTES} bytes"))
            }
            LinesCodecError::Io(error) => error,
        })?;
        if let Some(tap) = &incoming_tap {
            tap(WireDirection::Incoming, &line);
        }
        Ok(line)
    });
    let outgoing = SinkExt::<String>::sink_map_err(FramedWrite::new(writer, LinesCodec::new()), |error| match error {
        LinesCodecError::Io(error) => error,
        LinesCodecError::MaxLineLengthExceeded => std::io::Error::other("outgoing line too long"),
    })
    .with(move |line: String| {
        if let Some(tap) = &tap {
            tap(WireDirection::Outgoing, &line);
        }
        futures::future::ready(Ok::<_, std::io::Error>(line))
    });
    agent_client_protocol::Lines::new(outgoing, Box::pin(incoming))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_is_bounded_and_redacted() {
        let mut tail = StderrTail::default();
        tail.push(b"token=abc\n");
        assert!(tail.text().contains("agent stderr omitted"));
        tail.push(&vec![b'x'; STDERR_TAIL_BYTES + 10]);
        let text = tail.text();
        assert!(text.contains("truncated: true"));
        assert!(!text.contains("token"));
        assert!(text.len() <= STDERR_TAIL_BYTES + "[…] ".len());
    }

    #[test]
    fn stderr_diagnostics_omit_unstructured_organization_names() {
        let mut tail = StderrTail::default();
        tail.push(b"account alice@example.com from Acme Corp token=secret");
        let shown = tail.text();
        assert!(!shown.contains("alice@example.com"));
        assert!(!shown.contains("Acme Corp"));
        assert!(!shown.contains("secret"));
    }
}
