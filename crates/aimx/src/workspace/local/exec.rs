//! Processes of the local backend.
//!
//! Every process runs in its own process group (pipes: `setpgid(0, 0)`; pty: `setsid`, so the
//! group id is the child's pid), so signals, timeouts and release reach its whole tree.
//!
//! The group outlives its leader: the leader's exit is observed with `waitid(WNOWAIT)`, which
//! leaves it a zombie, and a zombie's pid cannot be reused, so the group id stays reserved. Only
//! `release` (or the workspace closing) kills the whole group and then reaps the leader, so
//! `exec.signal`, `exec.release`, `KillShell` and session expiry reach every descendant a leader
//! left behind, and nothing is ever signalled once the id could be reused. Output
//! goes into a per-process [`OutputRing`] with one strictly increasing `seq` across stdout,
//! stderr and the pty. The exit status is recorded only after the output readers have drained (or
//! a short grace period has passed, for a child that left a background process holding the pipe),
//! so a reader that sees `exit` has seen all retained output.

use std::collections::HashMap;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{Command, ExecReadResult, ExitStatus, OutputChunk, OutputStream, PtySize, Signal};
use aim_proto::ids::ProcId;
use rustix::fs::{Mode, OFlags};
use rustix::process::{WaitId, WaitIdOptions, WaitIdStatus};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::ChildStdin;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinHandle;

use super::walk::{self, Follow};
use super::{Authority, Base, blocking, io_error, path_string};
use crate::id::random_hex;
use crate::ring::OutputRing;
use crate::workspace::{BoxFuture, Exec, Outcome, SpawnSpec};

const READ_BLOCK: usize = 32 * 1024;
/// The smallest output chunk a scoped spawn splits its output into (ADR 0067): a smaller
/// `max_output_bytes` would multiply per-chunk bookkeeping in the output ring.
const MIN_CHUNK: usize = 1024;

/// How a process's output is kept: the ring's byte budget, and the largest chunk (the spawning
/// call's `max_output_bytes`, at least [`MIN_CHUNK`]), so every `exec.read` answer and pushed
/// `exec.output` stays within that limit without ever splitting a sequence number.
#[derive(Clone, Copy, Debug)]
struct OutputBounds {
    ring_bytes: usize,
    max_chunk: usize,
}

/// Serialises everything that creates inheritable descriptors and forks. On macOS the standard
/// library creates pipes with `pipe` + `fcntl(FD_CLOEXEC)`, so a child forked by another thread in
/// between inherits the pipe's write end — and the first process then never sees end of file on
/// its stdin (observed in this crate's conformance suite under load). Spawning is short; this lock
/// only orders spawns against each other.
///
/// It also orders the working-directory switch of [`in_dir`].
static SPAWN: Mutex<()> = Mutex::new(());
/// How long output readers may keep draining after the process exited.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `spawn` (under the spawn lock) with this process's working directory switched to the
/// held directory `dir` and restored afterwards, so a child inherits a directory the walk opened
/// rather than one re-resolved by path: a swap after the walk cannot move a process out of the
/// root (REV4-A finding 3). The standard library can only set a child's directory by path
/// (`/dev/fd/N` is refused on macOS), and `fchdir` in a `pre_exec` hook would need `unsafe`.
/// Every other path aimx uses is absolute, so the brief switch affects nothing else.
fn in_dir<T>(dir: &OwnedFd, spawn: impl FnOnce() -> T) -> io::Result<T> {
    static ORIGINAL: OnceLock<Option<OwnedFd>> = OnceLock::new();
    let _spawning = lock(&SPAWN);
    let original = ORIGINAL
        .get_or_init(|| rustix::fs::openat(rustix::fs::CWD, ".", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).ok());
    rustix::process::fchdir(dir)?;
    let spawned = spawn();
    let restored = match original {
        Some(original) => rustix::process::fchdir(original),
        None => rustix::process::chdir("/"),
    };
    if let Err(err) = restored {
        tracing::error!(%err, "could not restore the working directory after a spawn");
    }
    Ok(spawned)
}

/// This host's processes, for one workspace.
pub(super) struct LocalExec {
    base: Arc<Base>,
    ring_bytes: usize,
    ptys: Option<Arc<Semaphore>>,
    procs: Arc<ProcRegistry>,
}

/// All scoped views of a workspace own one registry; processes are released when its final
/// owner goes away, not when an individual request ends.
struct ProcRegistry(Mutex<HashMap<ProcId, Registered>>);

/// A registered process and the real cwd its spawn resolved (the process keeps that directory
/// whatever later happens to its path), which a scoped view checks on every control call.
struct Registered {
    proc: Arc<Proc>,
    cwd: String,
}

impl Drop for ProcRegistry {
    fn drop(&mut self) {
        for registered in lock(&self.0).values() {
            registered.proc.release();
        }
    }
}

impl std::fmt::Debug for LocalExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalExec").field("procs", &lock(&self.procs.0).len()).finish_non_exhaustive()
    }
}

type Master = Box<dyn portable_pty::MasterPty + Send>;

/// The PTY permit refusal happens before a process is started and may be retried with the same key.
pub(super) const PTY_CAPACITY_MESSAGE: &str = "as many pty processes run as may; end one first";
type PtyWriter = Box<dyn io::Write + Send>;

struct Proc {
    pgid: Option<i32>,
    max_chunk: usize,
    state: Mutex<ProcState>,
    version: watch::Sender<u64>,
    input: tokio::sync::Mutex<Input>,
    master: Option<Mutex<Master>>,
    timed_out: AtomicBool,
    /// The group leader has exited (it stays unreaped, reserving the group id, until release).
    leader_exited: AtomicBool,
    last_signal: AtomicI32,
    /// Set by release: the group has been killed and the leader may be reaped, after which the
    /// group id may be reused, so nothing is signalled any more. Signals are sent under this lock.
    released: Mutex<bool>,
    /// Wakes the supervisor to reap the leader once released.
    reap: Notify,
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
    fn new(pgid: Option<i32>, output: OutputBounds, input: Input, master: Option<Master>) -> Self {
        Self {
            pgid,
            max_chunk: output.max_chunk.max(1),
            state: Mutex::new(ProcState { ring: OutputRing::new(output.ring_bytes), exit: None }),
            version: watch::channel(0).0,
            input: tokio::sync::Mutex::new(input),
            master: master.map(Mutex::new),
            timed_out: AtomicBool::new(false),
            leader_exited: AtomicBool::new(false),
            last_signal: AtomicI32::new(0),
            released: Mutex::new(false),
            reap: Notify::new(),
        }
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Appends output, in chunks of at most `max_chunk` bytes; ignored once the exit is recorded
    /// (a straggler holding the pipe).
    fn push(&self, stream: OutputStream, data: &[u8]) {
        let pushed = {
            let mut state = lock(&self.state);
            if state.exit.is_some() || data.is_empty() {
                false
            } else {
                for piece in data.chunks(self.max_chunk) {
                    state.ring.push(stream, piece.to_vec());
                }
                true
            }
        };
        if pushed {
            self.bump();
        }
    }

    fn finish(&self, exit: ExitStatus) {
        lock(&self.state).exit = Some(exit);
        self.bump();
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

    /// Signals the whole process group, including what an exited leader left behind (its
    /// unreaped zombie keeps the group id reserved). A no-op once released.
    fn signal(&self, signal: rustix::process::Signal) -> Outcome<()> {
        let released = lock(&self.released);
        if *released {
            return Ok(());
        }
        let Some(pgid) = self.pgid else {
            return Err(ProtoError::new(ErrorCode::Unavailable, "the process has no process group"));
        };
        let already_killed = self.last_signal.swap(signal.as_raw(), Ordering::SeqCst) == rustix::process::Signal::KILL.as_raw();
        match kill_group(pgid, signal) {
            Ok(()) => Ok(()),
            // Nothing is left in the group.
            Err(err) if err.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => Ok(()),
            // macOS answers EPERM when the group holds a zombie (an exited leader, or members
            // killed but not yet reaped) and still delivers the signal to the live members;
            // otherwise a member changed its credentials (setuid) and cannot be signalled.
            Err(err) if err.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()) => {
                if already_killed || self.leader_exited.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(ProtoError::new(ErrorCode::Denied, format!("not permitted to signal process group {pgid}")))
                }
            }
            Err(err) => Err(ProtoError::new(ErrorCode::Internal, format!("signalling process group {pgid}: {err}"))),
        }
    }

    /// Kills the whole group while the unreaped leader still reserves its id, then lets the
    /// supervisor reap the leader. Idempotent.
    fn release(&self) {
        {
            let mut released = lock(&self.released);
            if !*released {
                if let Some(pgid) = self.pgid
                    && let Err(err) = kill_group(pgid, rustix::process::Signal::KILL)
                    && err.raw_os_error() != Some(rustix::io::Errno::SRCH.raw_os_error())
                {
                    tracing::debug!(%err, pgid, "killing a released process group");
                }
                *released = true;
            }
        }
        self.reap.notify_one();
    }
}

fn kill_group(pgid: i32, signal: rustix::process::Signal) -> io::Result<()> {
    let group = rustix::process::Pid::from_raw(pgid).ok_or_else(|| io::Error::other("invalid process group id"))?;
    rustix::process::kill_process_group(group, signal).map_err(io::Error::from)
}

/// How often a waiting supervisor re-checks for its leader's exit when no `SIGCHLD` arrives.
const EXIT_POLL: Duration = Duration::from_millis(100);

fn exit_from_waitid(status: &WaitIdStatus) -> ExitStatus {
    if status.exited() {
        ExitStatus::Exited { code: status.exit_status().unwrap_or(-1) }
    } else if status.killed() || status.dumped() {
        ExitStatus::Signaled { signal: status.terminating_signal().unwrap_or(0) }
    } else {
        ExitStatus::Exited { code: -1 }
    }
}

/// Waits for `pid` to exit **without reaping it** (`waitid(WEXITED | WNOHANG | WNOWAIT)`), woken
/// by `SIGCHLD` and re-checked every [`EXIT_POLL`] in case a signal is missed.
async fn leader_exit(pid: rustix::process::Pid) -> io::Result<ExitStatus> {
    // Subscribe before the first check, so an exit in between still wakes us.
    let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok();
    loop {
        match rustix::process::waitid(WaitId::Pid(pid), WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT) {
            Ok(Some(status)) => return Ok(exit_from_waitid(&status)),
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
        match sigchld.as_mut() {
            Some(signals) => {
                if let Ok(None) = tokio::time::timeout(EXIT_POLL, signals.recv()).await {
                    sigchld = None;
                }
            }
            None => tokio::time::sleep(EXIT_POLL).await,
        }
    }
}

/// Blocks until `pid` exits, without reaping it (for the pty's waiter thread).
fn leader_exit_blocking(pid: rustix::process::Pid) -> io::Result<ExitStatus> {
    loop {
        match rustix::process::waitid(WaitId::Pid(pid), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Ok(Some(status)) => return Ok(exit_from_waitid(&status)),
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
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
            Ok(n) => proc.push(stream, block.get(..n).unwrap_or_default()),
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
    if let Err(err) = proc.signal(rustix::process::Signal::KILL) {
        tracing::debug!(%err, "killing a timed-out process group failed");
    }
}

fn exit_of(proc: &Proc, status: &io::Result<ExitStatus>) -> ExitStatus {
    if proc.timed_out.load(Ordering::SeqCst) {
        ExitStatus::TimedOut
    } else {
        status.as_ref().map_or(ExitStatus::Exited { code: -1 }, |status| *status)
    }
}

async fn supervise_pipes(proc: Arc<Proc>, mut child: tokio::process::Child, readers: Vec<JoinHandle<()>>, timeout: Option<Duration>) {
    let pid = child.id().and_then(|id| i32::try_from(id).ok()).and_then(rustix::process::Pid::from_raw);
    let exited = async {
        match pid {
            Some(pid) => leader_exit(pid).await,
            None => Err(io::Error::other("the child has no pid")),
        }
    };
    let status = match timeout {
        Some(limit) => {
            tokio::pin!(exited);
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
    drain(readers).await;
    proc.finish(exit_of(&proc, &status));
    // The leader stays a zombie (reserving the group id) until the process is released.
    proc.reap.notified().await;
    if let Err(err) = child.wait().await {
        tracing::debug!(%err, "reaping a released process");
    }
}

async fn supervise_pty(
    proc: Arc<Proc>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    exited: oneshot::Receiver<io::Result<ExitStatus>>,
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
    let status = status.unwrap_or_else(|_| Err(io::Error::other("the pty waiter stopped")));
    proc.leader_exited.store(true, Ordering::SeqCst);
    if tokio::time::timeout(DRAIN_GRACE, drained).await.is_err() {
        tracing::debug!("pty output still open after exit; later output is dropped");
    }
    proc.finish(exit_of(&proc, &status));
    // The leader stays a zombie (reserving the group id) until the process is released.
    proc.reap.notified().await;
    if let Err(err) = tokio::task::spawn_blocking(move || child.wait()).await {
        tracing::debug!(%err, "reaping a released pty process");
    }
}

fn spawn_pipes(cwd: &OwnedFd, spec: &SpawnSpec<'_>, output: OutputBounds) -> Outcome<Arc<Proc>> {
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
        .envs(spec.env)
        .stdin(if spec.stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let spawned = in_dir(cwd, || command.spawn()).map_err(|err| io_error(&err, spec.cwd))?;
    let mut child = spawned.map_err(|err| spawn_error(&err, &program))?;
    let pgid = child.id().and_then(|id| i32::try_from(id).ok());
    let input = child.stdin.take().map_or(Input::Closed, Input::Pipe);
    let proc = Arc::new(Proc::new(pgid, output, input, None));
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
    cwd: &OwnedFd,
    spec: &SpawnSpec<'_>,
    size: PtySize,
    output: OutputBounds,
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
    // The child keeps the working directory it inherits from [`in_dir`].
    builder.cwd(".");
    if !spec.env.contains_key("TERM") {
        builder.env("TERM", "xterm-256color");
    }
    for (key, value) in spec.env {
        builder.env(key, value);
    }
    let spawned = in_dir(cwd, || -> Outcome<_> {
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
        Ok((child, pair.master, reader, writer))
    });
    let (child, master, mut reader, writer) = spawned.map_err(|err| io_error(&err, spec.cwd))??;
    let pgid = child.process_id().and_then(|id| i32::try_from(id).ok());
    let proc = Arc::new(Proc::new(pgid, output, Input::Pty(Arc::new(Mutex::new(writer))), Some(master)));

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
                    Ok(n) => reading.push(OutputStream::Pty, block.get(..n).unwrap_or_default()),
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
    let leader = pgid.and_then(rustix::process::Pid::from_raw);
    std::thread::Builder::new()
        .name("aimx-pty-wait".to_owned())
        .spawn(move || {
            let _permit = permit;
            let status = match leader {
                Some(leader) => leader_exit_blocking(leader),
                None => Err(io::Error::other("the pty child has no pid")),
            };
            if exited_tx.send(status).is_err() {
                tracing::debug!("pty supervisor gone before the child exited");
            }
        })
        .map_err(|err| ProtoError::new(ErrorCode::Internal, format!("starting a pty waiter: {err}")))?;

    tokio::spawn(supervise_pty(Arc::clone(&proc), child, exited_rx, drained_rx, spec.timeout));
    Ok(proc)
}

impl LocalExec {
    pub(super) fn new(base: Arc<Base>, ring_bytes: usize, ptys: Option<Arc<Semaphore>>) -> Self {
        Self { base, ring_bytes, ptys, procs: Arc::new(ProcRegistry(Mutex::new(HashMap::new()))) }
    }

    pub(super) fn scoped(&self, base: Arc<Base>) -> Self {
        Self { base, ring_bytes: self.ring_bytes, ptys: self.ptys.clone(), procs: Arc::clone(&self.procs) }
    }

    /// The process, once this view's grant (if any) permits execution at the cwd its spawn
    /// resolved: a control call is judged against the resolved target, never a lexical path
    /// (ADR 0046, 0067).
    fn get(&self, proc: &ProcId) -> Outcome<Arc<Proc>> {
        let (found, cwd) = lock(&self.procs.0)
            .get(proc)
            .map(|registered| (Arc::clone(&registered.proc), registered.cwd.clone()))
            .ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("unknown process `{proc}`")))?;
        if let Some(grant) = &self.base.grant {
            grant.exec_path(&cwd)?;
        }
        Ok(found)
    }
}

impl Exec for LocalExec {
    fn spawn<'a>(&'a self, spec: SpawnSpec<'a>) -> BoxFuture<'a, Outcome<ProcId>> {
        Box::pin(async move {
            let base = Arc::clone(&self.base);
            let cwd_path = spec.cwd.to_owned();
            let (cwd, real_cwd) = blocking(move || {
                let loc = base.resolve(&cwd_path, Follow::Final, Authority::Exec)?;
                if let Some(dir) = loc.target_dir() {
                    let real = path_string(&base.real(&loc))?;
                    return Ok((dir.try_clone().map_err(|err| io_error(&err, &cwd_path))?, real));
                }
                let exists = match (loc.dir(), &loc.name) {
                    (Ok(dir), Some(name)) if loc.missing.is_empty() => walk::stat_entry(dir, name).is_ok(),
                    _ => false,
                };
                if exists {
                    Err(ProtoError::new(ErrorCode::Conflict, format!("`{cwd_path}` is not a directory")))
                } else {
                    Err(ProtoError::new(ErrorCode::NotFound, format!("`{cwd_path}` does not exist")))
                }
            })
            .await?;
            let max_chunk =
                self.base.grant.as_ref().and_then(crate::authz::Grant::limits).map_or(READ_BLOCK, |limits| {
                    usize::try_from(limits.max_output_bytes).unwrap_or(usize::MAX).clamp(MIN_CHUNK, READ_BLOCK)
                });
            let output = OutputBounds { ring_bytes: self.ring_bytes, max_chunk };
            let proc = match spec.pty {
                Some(size) => {
                    // The permit lives as long as the pty's threads (it is released when both end).
                    let permit = match &self.ptys {
                        Some(ptys) => Some(Arc::new(
                            Arc::clone(ptys)
                                .try_acquire_owned()
                                .map_err(|_| ProtoError::new(ErrorCode::LimitExceeded, PTY_CAPACITY_MESSAGE))?,
                        )),
                        None => None,
                    };
                    spawn_pty(&cwd, &spec, size, output, permit)?
                }
                None => spawn_pipes(&cwd, &spec, output)?,
            };
            let id = ProcId::new(format!("p{}", random_hex()));
            lock(&self.procs.0).insert(id.clone(), Registered { proc, cwd: real_cwd });
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
            self.get(proc)?;
            let removed = lock(&self.procs.0).remove(proc);
            let Some(registered) = removed else {
                return Err(ProtoError::new(ErrorCode::NotFound, format!("unknown process `{proc}`")));
            };
            // Kills the whole group (whatever the leader left behind too); forgotten either way.
            registered.proc.release();
            Ok(())
        })
    }
}
