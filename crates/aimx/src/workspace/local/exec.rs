//! Processes of the local backend.
//!
//! Every process runs in its own process group (pipes: `setpgid(0, 0)`; pty: `setsid`, so the
//! group id is the child's pid), so signals, timeouts and release reach its whole tree. Output
//! goes into a per-process [`OutputRing`] with one strictly increasing `seq` across stdout,
//! stderr and the pty. The exit status is recorded only after the output readers have drained (or
//! a short grace period has passed, for a child that left a background process holding the pipe),
//! so a reader that sees `exit` has seen all retained output.

use std::collections::HashMap;
use std::io::{self, Read as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{Command, ExecReadResult, ExitStatus, OutputChunk, OutputStream, PtySize, Signal};
use aim_proto::ids::ProcId;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::ChildStdin;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinHandle;

use super::{Base, Follow, blocking, io_error};
use crate::id::random_hex;
use crate::ring::OutputRing;
use crate::workspace::{BoxFuture, Exec, Outcome, SpawnSpec};

const READ_BLOCK: usize = 32 * 1024;

/// Serialises everything that creates inheritable descriptors and forks. On macOS the standard
/// library creates pipes with `pipe` + `fcntl(FD_CLOEXEC)`, so a child forked by another thread in
/// between inherits the pipe's write end — and the first process then never sees end of file on
/// its stdin (observed in this crate's conformance suite under load). Spawning is short; this lock
/// only orders spawns against each other.
static SPAWN: Mutex<()> = Mutex::new(());
/// How long output readers may keep draining after the process exited.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// This host's processes, for one workspace.
pub(super) struct LocalExec {
    base: Arc<Base>,
    ring_bytes: usize,
    ptys: Option<Arc<Semaphore>>,
    procs: Mutex<HashMap<ProcId, Arc<Proc>>>,
}

impl std::fmt::Debug for LocalExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalExec").field("procs", &lock(&self.procs).len()).finish_non_exhaustive()
    }
}

type Master = Box<dyn portable_pty::MasterPty + Send>;
type PtyWriter = Box<dyn io::Write + Send>;

struct Proc {
    pgid: Option<i32>,
    state: Mutex<ProcState>,
    version: watch::Sender<u64>,
    input: tokio::sync::Mutex<Input>,
    master: Option<Mutex<Master>>,
    timed_out: AtomicBool,
    /// The group leader has been reaped (set before the output drains and the exit is recorded):
    /// from here on the group id may be reused, so nothing is signalled.
    leader_exited: AtomicBool,
    last_signal: AtomicI32,
}

struct ProcState {
    ring: OutputRing,
    exit: Option<ExitStatus>,
}

enum Input {
    Closed,
    Pipe(ChildStdin),
    Pty(Arc<Mutex<PtyWriter>>),
}

impl Proc {
    fn new(pgid: Option<i32>, ring_bytes: usize, input: Input, master: Option<Master>) -> Self {
        Self {
            pgid,
            state: Mutex::new(ProcState { ring: OutputRing::new(ring_bytes), exit: None }),
            version: watch::channel(0).0,
            input: tokio::sync::Mutex::new(input),
            master: master.map(Mutex::new),
            timed_out: AtomicBool::new(false),
            leader_exited: AtomicBool::new(false),
            last_signal: AtomicI32::new(0),
        }
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Appends output; ignored once the exit is recorded (a straggler holding the pipe).
    fn push(&self, stream: OutputStream, data: Vec<u8>) {
        let pushed = {
            let mut state = lock(&self.state);
            if state.exit.is_some() { None } else { state.ring.push(stream, data) }
        };
        if pushed.is_some() {
            self.bump();
        }
    }

    fn finish(&self, exit: ExitStatus) {
        lock(&self.state).exit = Some(exit);
        self.bump();
    }

    fn finished(&self) -> bool {
        lock(&self.state).exit.is_some()
    }

    fn snapshot(&self, after_seq: u64, max_bytes: usize) -> ExecReadResult {
        let state = lock(&self.state);
        let slice = state.ring.read(after_seq, max_bytes);
        let exit = if slice.complete { state.exit } else { None };
        ExecReadResult {
            chunks: slice
                .chunks
                .into_iter()
                .map(|chunk| OutputChunk { seq: chunk.seq, stream: chunk.stream, data: Content::from_bytes(chunk.data) })
                .collect(),
            dropped_before: slice.dropped_before,
            exit,
        }
    }

    /// Signals the process group while the process runs (after it ended, the group id may be
    /// reused, so nothing is sent).
    fn signal(&self, signal: rustix::process::Signal) -> Outcome<()> {
        if self.leader_exited.load(Ordering::SeqCst) || self.finished() {
            return Ok(());
        }
        let Some(pgid) = self.pgid else {
            return Err(ProtoError::new(ErrorCode::Unavailable, "the process has no process group"));
        };
        let already_killed = self.last_signal.swap(signal.as_raw(), Ordering::SeqCst) == rustix::process::Signal::KILL.as_raw();
        match kill_group(pgid, signal) {
            Ok(()) => Ok(()),
            // The group is gone.
            Err(err) if err.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => Ok(()),
            // macOS answers EPERM for a group whose members are all zombies (killed, not yet
            // reaped); otherwise a member changed its credentials (setuid) and cannot be signalled.
            Err(err) if err.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()) => {
                if already_killed {
                    Ok(())
                } else {
                    Err(ProtoError::new(ErrorCode::Denied, format!("not permitted to signal process group {pgid}")))
                }
            }
            Err(err) => Err(ProtoError::new(ErrorCode::Internal, format!("signalling process group {pgid}: {err}"))),
        }
    }
}

fn kill_group(pgid: i32, signal: rustix::process::Signal) -> io::Result<()> {
    let group = rustix::process::Pid::from_raw(pgid).ok_or_else(|| io::Error::other("invalid process group id"))?;
    rustix::process::kill_process_group(group, signal).map_err(io::Error::from)
}

fn exit_from_std(status: std::process::ExitStatus) -> ExitStatus {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitStatus::Exited { code },
        (None, Some(signal)) => ExitStatus::Signaled { signal },
        (None, None) => ExitStatus::Exited { code: -1 },
    }
}

/// The number of a signal from `strsignal` text (`Terminated: 15` on macOS, `Terminated` on
/// Linux, `Signal 34` for unnamed ones).
fn signal_number(name: &str) -> Option<i32> {
    let digits: String = name.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    if !digits.is_empty() {
        return digits.parse().ok();
    }
    let table = [
        ("Hangup", 1),
        ("Interrupt", 2),
        ("Quit", 3),
        ("Illegal instruction", 4),
        ("Trace/breakpoint trap", 5),
        ("Aborted", 6),
        ("Abort trap", 6),
        ("Killed", 9),
        ("Segmentation fault", 11),
        ("Broken pipe", 13),
        ("Alarm clock", 14),
        ("Terminated", 15),
    ];
    table.iter().find(|(text, _)| name.starts_with(text)).map(|(_, number)| *number)
}

fn spawn_error(err: &io::Error, program: &str) -> ProtoError {
    match err.kind() {
        io::ErrorKind::NotFound => ProtoError::new(ErrorCode::NotFound, format!("program `{program}` not found")),
        io::ErrorKind::PermissionDenied => ProtoError::new(ErrorCode::Denied, format!("program `{program}` is not executable")),
        _ => io_error(err, program),
    }
}

/// portable-pty reports `anyhow::Error`s; only their text is kept.
fn pty_error(err: &dyn std::fmt::Display) -> ProtoError {
    ProtoError::new(ErrorCode::Internal, format!("pty: {err}"))
}

async fn pump<R: AsyncRead + Unpin>(proc: Arc<Proc>, mut reader: R, stream: OutputStream) {
    let mut block = vec![0u8; READ_BLOCK];
    loop {
        match reader.read(&mut block).await {
            Ok(0) | Err(_) => break,
            Ok(n) => proc.push(stream, block.get(..n).unwrap_or_default().to_vec()),
        }
    }
}

async fn drain(readers: Vec<JoinHandle<()>>) {
    let deadline = tokio::time::Instant::now() + DRAIN_GRACE;
    for mut reader in readers {
        if tokio::time::timeout_at(deadline, &mut reader).await.is_err() {
            reader.abort();
        }
    }
}

fn kill_on_timeout(proc: &Proc) {
    proc.timed_out.store(true, Ordering::SeqCst);
    if let Some(pgid) = proc.pgid
        && let Err(err) = kill_group(pgid, rustix::process::Signal::KILL)
    {
        tracing::debug!(%err, pgid, "killing a timed-out process group failed");
    }
}

async fn supervise_pipes(proc: Arc<Proc>, mut child: tokio::process::Child, readers: Vec<JoinHandle<()>>, timeout: Option<Duration>) {
    let status = match timeout {
        Some(limit) => {
            tokio::select! {
                status = child.wait() => status,
                () = tokio::time::sleep(limit) => {
                    kill_on_timeout(&proc);
                    if let Err(err) = child.start_kill() {
                        tracing::debug!(%err, "killing a timed-out child failed");
                    }
                    child.wait().await
                }
            }
        }
        None => child.wait().await,
    };
    proc.leader_exited.store(true, Ordering::SeqCst);
    drain(readers).await;
    let exit = if proc.timed_out.load(Ordering::SeqCst) {
        ExitStatus::TimedOut
    } else {
        status.map_or(ExitStatus::Exited { code: -1 }, exit_from_std)
    };
    proc.finish(exit);
}

async fn supervise_pty(
    proc: Arc<Proc>,
    exited: oneshot::Receiver<io::Result<portable_pty::ExitStatus>>,
    drained: oneshot::Receiver<()>,
    timeout: Option<Duration>,
) {
    let mut exited = exited;
    let status = match timeout {
        Some(limit) => {
            tokio::select! {
                status = &mut exited => status,
                () = tokio::time::sleep(limit) => {
                    kill_on_timeout(&proc);
                    exited.await
                }
            }
        }
        None => exited.await,
    };
    proc.leader_exited.store(true, Ordering::SeqCst);
    if tokio::time::timeout(DRAIN_GRACE, drained).await.is_err() {
        tracing::debug!("pty output still open after exit; later output is dropped");
    }
    let exit = if proc.timed_out.load(Ordering::SeqCst) {
        ExitStatus::TimedOut
    } else {
        match status {
            Ok(Ok(status)) => match status.signal() {
                Some(name) => {
                    ExitStatus::Signaled { signal: signal_number(name).unwrap_or_else(|| proc.last_signal.load(Ordering::SeqCst)) }
                }
                None => ExitStatus::Exited { code: i32::try_from(status.exit_code()).unwrap_or(-1) },
            },
            Ok(Err(_)) | Err(_) => ExitStatus::Exited { code: -1 },
        }
    };
    proc.finish(exit);
}

fn spawn_pipes(cwd: PathBuf, spec: &SpawnSpec<'_>, ring_bytes: usize) -> Outcome<Arc<Proc>> {
    let (mut command, program) = match spec.command {
        Command::Argv { argv } => {
            let Some((program, args)) = argv.split_first() else {
                return Err(ProtoError::new(ErrorCode::InvalidParams, "argv must not be empty"));
            };
            let mut command = tokio::process::Command::new(program);
            command.args(args);
            (command, program.clone())
        }
        Command::Shell { script } => {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.arg("-c").arg(script);
            (command, "/bin/sh".to_owned())
        }
    };
    command
        .current_dir(cwd)
        .envs(spec.env)
        .stdin(if spec.stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let spawned = {
        let _spawning = lock(&SPAWN);
        command.spawn()
    };
    let mut child = spawned.map_err(|err| spawn_error(&err, &program))?;
    let pgid = child.id().and_then(|id| i32::try_from(id).ok());
    let input = child.stdin.take().map_or(Input::Closed, Input::Pipe);
    let proc = Arc::new(Proc::new(pgid, ring_bytes, input, None));
    let mut readers = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        readers.push(tokio::spawn(pump(Arc::clone(&proc), stdout, OutputStream::Stdout)));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(tokio::spawn(pump(Arc::clone(&proc), stderr, OutputStream::Stderr)));
    }
    tokio::spawn(supervise_pipes(Arc::clone(&proc), child, readers, spec.timeout));
    Ok(proc)
}

fn spawn_pty(
    cwd: PathBuf,
    spec: &SpawnSpec<'_>,
    size: PtySize,
    ring_bytes: usize,
    permit: Option<Arc<OwnedSemaphorePermit>>,
) -> Outcome<Arc<Proc>> {
    let mut builder = match spec.command {
        Command::Argv { argv } => {
            if argv.is_empty() {
                return Err(ProtoError::new(ErrorCode::InvalidParams, "argv must not be empty"));
            }
            portable_pty::CommandBuilder::from_argv(argv.iter().map(Into::into).collect())
        }
        Command::Shell { script } => {
            let mut builder = portable_pty::CommandBuilder::new("/bin/sh");
            builder.arg("-c");
            builder.arg(script);
            builder
        }
    };
    builder.cwd(cwd);
    if !spec.env.contains_key("TERM") {
        builder.env("TERM", "xterm-256color");
    }
    for (key, value) in spec.env {
        builder.env(key, value);
    }
    let (mut child, master, mut reader, writer) = {
        let _spawning = lock(&SPAWN);
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize { rows: size.rows, cols: size.cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|err| pty_error(&err))?;
        let child = pair.slave.spawn_command(builder).map_err(|err| {
            let message = err.to_string();
            if message.contains("No such file") {
                ProtoError::new(ErrorCode::NotFound, format!("program not found: {message}"))
            } else {
                pty_error(&err)
            }
        })?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|err| pty_error(&err))?;
        let writer = pair.master.take_writer().map_err(|err| pty_error(&err))?;
        (child, pair.master, reader, writer)
    };
    let pgid = child.process_id().and_then(|id| i32::try_from(id).ok());
    let proc = Arc::new(Proc::new(pgid, ring_bytes, Input::Pty(Arc::new(Mutex::new(writer))), Some(master)));

    let (drained_tx, drained_rx) = oneshot::channel();
    let reading = Arc::clone(&proc);
    let reader_permit = permit.clone();
    std::thread::Builder::new()
        .name("aimx-pty-read".to_owned())
        .spawn(move || {
            let _permit = reader_permit;
            let mut block = vec![0u8; READ_BLOCK];
            loop {
                match reader.read(&mut block) {
                    Ok(0) => break,
                    Ok(n) => reading.push(OutputStream::Pty, block.get(..n).unwrap_or_default().to_vec()),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    // EIO once the slave side is closed (Linux) is the pty's end of file.
                    Err(_) => break,
                }
            }
            if drained_tx.send(()).is_err() {
                tracing::debug!("pty supervisor gone before output drained");
            }
        })
        .map_err(|err| ProtoError::new(ErrorCode::Internal, format!("starting a pty reader: {err}")))?;

    let (exited_tx, exited_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("aimx-pty-wait".to_owned())
        .spawn(move || {
            let _permit = permit;
            if exited_tx.send(child.wait()).is_err() {
                tracing::debug!("pty supervisor gone before the child exited");
            }
        })
        .map_err(|err| ProtoError::new(ErrorCode::Internal, format!("starting a pty waiter: {err}")))?;

    tokio::spawn(supervise_pty(Arc::clone(&proc), exited_rx, drained_rx, spec.timeout));
    Ok(proc)
}

impl LocalExec {
    pub(super) fn new(base: Arc<Base>, ring_bytes: usize, ptys: Option<Arc<Semaphore>>) -> Self {
        Self { base, ring_bytes, ptys, procs: Mutex::new(HashMap::new()) }
    }

    fn get(&self, proc: &ProcId) -> Outcome<Arc<Proc>> {
        lock(&self.procs).get(proc).cloned().ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("unknown process `{proc}`")))
    }
}

impl Drop for LocalExec {
    fn drop(&mut self) {
        for proc in lock(&self.procs).values() {
            if let Err(err) = proc.signal(rustix::process::Signal::KILL) {
                tracing::debug!(%err, "killing a process on workspace close failed");
            }
        }
    }
}

impl Exec for LocalExec {
    fn spawn<'a>(&'a self, spec: SpawnSpec<'a>) -> BoxFuture<'a, Outcome<ProcId>> {
        Box::pin(async move {
            let base = Arc::clone(&self.base);
            let cwd_path = spec.cwd.to_owned();
            let cwd = blocking(move || {
                let real = base.resolve(&cwd_path, Follow::Final)?;
                let meta = std::fs::metadata(&real).map_err(|err| io_error(&err, &cwd_path))?;
                if meta.is_dir() { Ok(real) } else { Err(ProtoError::new(ErrorCode::Conflict, format!("`{cwd_path}` is not a directory"))) }
            })
            .await?;
            let proc = match spec.pty {
                Some(size) => {
                    // The permit lives as long as the pty's threads (it is released when both end).
                    let permit =
                        match &self.ptys {
                            Some(ptys) => Some(Arc::new(Arc::clone(ptys).try_acquire_owned().map_err(|_| {
                                ProtoError::new(ErrorCode::LimitExceeded, "as many pty processes run as may; end one first")
                            })?)),
                            None => None,
                        };
                    spawn_pty(cwd, &spec, size, self.ring_bytes, permit)?
                }
                None => spawn_pipes(cwd, &spec, self.ring_bytes)?,
            };
            let id = ProcId::new(format!("p{}", random_hex()));
            lock(&self.procs).insert(id.clone(), proc);
            Ok(id)
        })
    }

    fn read<'a>(&'a self, proc: &'a ProcId, after_seq: u64, max_bytes: u64, wait: Duration) -> BoxFuture<'a, Outcome<ExecReadResult>> {
        Box::pin(async move {
            let proc = self.get(proc)?;
            let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
            let deadline = tokio::time::Instant::now() + wait;
            let mut changes = proc.version.subscribe();
            loop {
                changes.mark_unchanged();
                let result = proc.snapshot(after_seq, max_bytes);
                if !result.chunks.is_empty() || result.exit.is_some() || tokio::time::Instant::now() >= deadline {
                    return Ok(result);
                }
                match tokio::time::timeout_at(deadline, changes.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => return Ok(proc.snapshot(after_seq, max_bytes)),
                }
            }
        })
    }

    fn write_stdin<'a>(&'a self, proc: &'a ProcId, data: &'a [u8], eof: bool) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let proc = self.get(proc)?;
            let mut input = proc.input.lock().await;
            let closed = || ProtoError::new(ErrorCode::Conflict, "the process's input is closed");
            match &mut *input {
                Input::Closed => return Err(closed()),
                Input::Pipe(pipe) => {
                    let written = async {
                        pipe.write_all(data).await?;
                        pipe.flush().await
                    };
                    if let Err(err) = written.await {
                        *input = Input::Closed;
                        return Err(if err.kind() == io::ErrorKind::BrokenPipe { closed() } else { io_error(&err, "stdin") });
                    }
                }
                Input::Pty(writer) => {
                    let writer = Arc::clone(writer);
                    let data = data.to_vec();
                    blocking(move || {
                        let mut writer = lock(&writer);
                        writer.write_all(&data).and_then(|()| writer.flush()).map_err(|err| io_error(&err, "pty"))
                    })
                    .await?;
                }
            }
            if eof {
                // Dropping a pipe closes it; dropping portable-pty's writer sends the pty's EOF.
                *input = Input::Closed;
            }
            Ok(())
        })
    }

    fn resize<'a>(&'a self, proc: &'a ProcId, size: PtySize) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let proc = self.get(proc)?;
            let Some(master) = &proc.master else {
                return Err(ProtoError::new(ErrorCode::Conflict, "the process has no pty"));
            };
            lock(master)
                .resize(portable_pty::PtySize { rows: size.rows, cols: size.cols, pixel_width: 0, pixel_height: 0 })
                .map_err(|err| pty_error(&err))
        })
    }

    fn signal<'a>(&'a self, proc: &'a ProcId, signal: Signal) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let proc = self.get(proc)?;
            let signal = match signal {
                Signal::Interrupt => rustix::process::Signal::INT,
                Signal::Terminate => rustix::process::Signal::TERM,
                Signal::Kill => rustix::process::Signal::KILL,
            };
            proc.signal(signal)
        })
    }

    fn release<'a>(&'a self, proc: &'a ProcId) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let removed = lock(&self.procs).remove(proc);
            let Some(proc) = removed else {
                return Err(ProtoError::new(ErrorCode::NotFound, format!("unknown process `{proc}`")));
            };
            // Best effort: the process is forgotten either way.
            if let Err(err) = proc.signal(rustix::process::Signal::KILL) {
                tracing::debug!(%err, "killing a released process failed");
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::signal_number;

    #[test]
    fn signal_names() {
        assert_eq!(signal_number("Terminated: 15"), Some(15));
        assert_eq!(signal_number("Terminated"), Some(15));
        assert_eq!(signal_number("Killed"), Some(9));
        assert_eq!(signal_number("Signal 34"), Some(34));
        assert_eq!(signal_number("mystery"), None);
    }
}
