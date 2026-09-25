//! Agentless workspace operations over a multiplexed OpenSSH connection.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    ByteRange, Caps, CaseMode, Command, ContentHash, DirEntry, EditOutcome, EntryKind, ExecReadResult, ExitStatus, FsListResult,
    FsReadResult, GlobResult, GrepMatch, GrepResult, Meta, Precondition, PtySize, Signal, WriteOutcome,
};
use aim_proto::ids::{IdempotencyKey, ProcId};
use globset::{Glob, GlobSetBuilder};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

use super::conn::Connection;
use super::quote;
use crate::workspace::{
    BoxFuture, EditRequest, Exec, Fs, GlobQuery, GrepQuery, ListRequest, Outcome, Search, SpawnSpec, Workspace, WriteRequest,
};

/// An SSH-backed workspace with no resident remote executable.
pub struct AgentlessWorkspace {
    connection: Connection,
    root: String,
    home: String,
    caps: Caps,
    channels: Arc<Semaphore>,
    processes: Mutex<BTreeMap<String, Arc<Process>>>,
}

struct Process {
    _permit: OwnedSemaphorePermit,
    remote_marker: String,
    child: Mutex<Child>,
    output: Mutex<ProcessOutput>,
    exit: Mutex<Option<ExitStatus>>,
    streams_open: AtomicU8,
    notify: Notify,
}

struct ProcessOutput {
    chunks: VecDeque<aim_proto::harness::OutputChunk>,
    bytes: usize,
    last_seq: u64,
    dropped_before: Option<u64>,
}

const MAX_PROCESS_OUTPUT: usize = 8 * 1024 * 1024;
static PROCESS_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl AgentlessWorkspace {
    /// Open a remote directory, resolving its canonical root on the remote host.
    ///
    /// # Errors
    /// Returns a protocol error if the remote root cannot be resolved.
    pub async fn open(connection: Connection, root: &str) -> Outcome<Self> {
        let output = connection.run(&format!("realpath -- {}", quote(root)), &[]).await?;
        let root = String::from_utf8(output).map_err(|_| error(ErrorCode::Unavailable, "remote root is not UTF-8"))?.trim_end().to_owned();
        if !root.starts_with('/') {
            return Err(error(ErrorCode::InvalidParams, "remote root is not absolute"));
        }
        let platform =
            super::bootstrap::probe(&connection).await.map_err(|_| error(ErrorCode::Unavailable, "remote platform probe failed"))?;
        let target = platform.target.unwrap_or("unknown");
        let (os, arch) = if target.contains("linux") {
            ("linux", target.split('-').next().unwrap_or("unknown"))
        } else if target.contains("darwin") {
            ("macos", target.split('-').next().unwrap_or("unknown"))
        } else {
            ("unknown", "unknown")
        };
        let native_search = connection.run("command -v rg >/dev/null 2>&1", &[]).await.is_ok();
        let caps = Caps {
            exec: true,
            pty: true,
            watch: false,
            native_search,
            atomic_rename: true,
            resumable: false,
            max_concurrency: Some(10),
            os: os.to_owned(),
            arch: arch.to_owned(),
            shell: Some("sh".to_owned()),
        };
        Ok(Self {
            connection,
            root,
            home: platform.home,
            caps,
            channels: Arc::new(Semaphore::new(9)),
            processes: Mutex::new(BTreeMap::new()),
        })
    }

    async fn run(&self, script: &str, input: &[u8]) -> Outcome<Vec<u8>> {
        let _permit = self.channels.acquire().await.map_err(|_| error(ErrorCode::Unavailable, "SSH channels closed"))?;
        self.connection.run(script, input).await
    }

    async fn safe(&self, path: &str, may_create: bool) -> Outcome<String> {
        if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(error(ErrorCode::Denied, "path is not confined"));
        }
        let script = if may_create {
            format!(
                "if [ -e {0} ] || [ -L {0} ]; then realpath -- {0}; else p=$(dirname -- {0}); while [ ! -e \"$p\" ]; do p=$(dirname -- \"$p\"); done; realpath -- \"$p\"; fi",
                quote(path)
            )
        } else {
            format!("realpath -- {}", quote(path))
        };
        let resolved = self.run(&script, &[]).await.map_err(|_| error(ErrorCode::NotFound, "remote path does not exist"))?;
        let resolved =
            String::from_utf8(resolved).map_err(|_| error(ErrorCode::Unavailable, "remote path is not UTF-8"))?.trim_end().to_owned();
        if resolved != self.root && !resolved.starts_with(&format!("{}/", self.root)) {
            return Err(error(ErrorCode::Denied, "path leaves workspace root"));
        }
        Ok(path.to_owned())
    }

    async fn hash(&self, path: &str) -> Outcome<ContentHash> {
        let script = format!(
            "if command -v sha256sum >/dev/null 2>&1; then sha256sum -- {0}; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 -- {0}; else exit 127; fi",
            quote(path)
        );
        let output = self.run(&script, &[]).await?;
        let text = String::from_utf8(output).map_err(|_| error(ErrorCode::Unavailable, "invalid remote hash"))?;
        let hash = text.split_whitespace().next().ok_or_else(|| error(ErrorCode::Unavailable, "empty remote hash"))?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(error(ErrorCode::Unavailable, "invalid remote hash"));
        }
        Ok(ContentHash(format!("sha256:{hash}")))
    }

    async fn meta(&self, path: &str, with_hash: bool) -> Outcome<Meta> {
        if path == self.root {
            self.safe(path, false).await?;
        } else {
            let parent = std::path::Path::new(path)
                .parent()
                .and_then(|parent| parent.to_str())
                .ok_or_else(|| error(ErrorCode::InvalidParams, "invalid remote path"))?;
            self.safe(parent, false).await?;
        }
        let script = format!(
            "p={}; if [ -L \"$p\" ]; then printf 'symlink\\n0'; elif [ -f \"$p\" ]; then printf 'file\\n'; wc -c < \"$p\"; elif [ -d \"$p\" ]; then printf 'dir\\n0'; elif [ -e \"$p\" ]; then printf 'other\\n0'; else exit 44; fi",
            quote(path)
        );
        let output = self.run(&script, &[]).await.map_err(|_| error(ErrorCode::NotFound, "remote path does not exist"))?;
        let text = String::from_utf8(output).map_err(|_| error(ErrorCode::Unavailable, "invalid remote metadata"))?;
        let mut lines = text.lines();
        let kind = match lines.next() {
            Some("file") => EntryKind::File,
            Some("dir") => EntryKind::Dir,
            Some("symlink") => EntryKind::Symlink,
            Some(_) => EntryKind::Other,
            None => return Err(error(ErrorCode::Unavailable, "empty remote metadata")),
        };
        let size = lines.next().unwrap_or("0").trim().parse().map_err(|_| error(ErrorCode::Unavailable, "invalid remote size"))?;
        let hash = if with_hash && kind == EntryKind::File { Some(self.hash(path).await?) } else { None };
        Ok(Meta { kind, size, mtime_ms: None, hash })
    }

    async fn read_file(&self, path: &str, range: Option<ByteRange>, max_bytes: u64) -> Outcome<FsReadResult> {
        self.safe(path, false).await?;
        let meta = self.meta(path, false).await?;
        let size = match meta.kind {
            EntryKind::File => meta.size,
            EntryKind::Symlink => {
                let output = self.run(&format!("wc -c < {}", quote(path)), &[]).await?;
                String::from_utf8(output)
                    .map_err(|_| error(ErrorCode::Unavailable, "invalid remote size"))?
                    .trim()
                    .parse()
                    .map_err(|_| error(ErrorCode::Unavailable, "invalid remote size"))?
            }
            _ => return Err(error(ErrorCode::InvalidParams, "remote path is not a file")),
        };
        let hash = self.hash(path).await?;
        let start = range.map_or(0, |range| range.start);
        let requested = range.map_or(size.saturating_sub(start), |range| range.len);
        let count = requested.min(max_bytes);
        let script = format!("dd if={} bs=1 skip={start} count={count} 2>/dev/null", quote(path));
        let content = self.run(&script, &[]).await?;
        Ok(FsReadResult {
            content: Content::from_bytes(content),
            size,
            hash,
            truncated: requested > count && start.saturating_add(count) < size,
        })
    }

    async fn write_file(&self, req: WriteRequest<'_>) -> Outcome<WriteOutcome> {
        self.safe(req.path, true).await?;
        let current = self.meta(req.path, true).await.ok();
        match req.precondition {
            Precondition::IfAbsent if current.is_some() => return Err(error(ErrorCode::PreconditionFailed, "remote file exists")),
            Precondition::IfHash { hash } if current.as_ref().and_then(|meta| meta.hash.as_ref()) != Some(hash) => {
                return Err(error(ErrorCode::PreconditionFailed, "remote file changed"));
            }
            _ => {}
        }
        let bytes = req.content.clone().into_bytes();
        let quoted = quote(req.path);
        let precondition = match req.precondition {
            Precondition::Any => String::new(),
            Precondition::IfAbsent => "[ ! -e \"$p\" ] && [ ! -L \"$p\" ] || exit 42;".to_owned(),
            Precondition::IfHash { hash } => format!(
                "if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum -- \"$p\"); elif command -v shasum >/dev/null 2>&1; then h=$(shasum -a 256 -- \"$p\"); else exit 127; fi; [ \"sha256:${{h%% *}}\" = {} ] || exit 42;",
                quote(&hash.0)
            ),
        };
        let script = format!(
            "umask 077; p={quoted}; d=$(dirname -- \"$p\"); {} t=$(mktemp \"$d/.aimx.XXXXXXXX\") || exit; trap 'rm -f \"$t\"' EXIT HUP INT TERM; cat > \"$t\" || exit; {precondition} mv -f \"$t\" \"$p\" || exit; trap - EXIT HUP INT TERM",
            if req.create_dirs { "mkdir -p \"$d\" || exit;" } else { "" }
        );
        let _permit = self.channels.acquire().await.map_err(|_| error(ErrorCode::Unavailable, "SSH channels closed"))?;
        let (status, _) = self.connection.run_with_status(&script, &bytes, false).await?;
        if status == 42 {
            return Err(error(ErrorCode::PreconditionFailed, "remote file changed"));
        }
        if status != 0 {
            return Err(error(ErrorCode::Unavailable, "remote write failed"));
        }
        Ok(WriteOutcome {
            hash: ContentHash(format!("sha256:{:x}", Sha256::digest(&bytes))),
            size: bytes.len() as u64,
            created: current.is_none(),
        })
    }

    async fn grep_fallback(&self, query: &GrepQuery<'_>, options: &str) -> Outcome<GrepResult> {
        let mut builder = GlobSetBuilder::new();
        for pattern in query.globs {
            builder.add(Glob::new(pattern).map_err(|_| error(ErrorCode::InvalidParams, "invalid search glob"))?);
        }
        let globs = builder.build().map_err(|_| error(ErrorCode::InvalidParams, "invalid search glob"))?;
        let files = self.run(&format!("find {} -type f -print0", quote(query.path)), &[]).await?;
        let mut matches = Vec::new();
        for path in files.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
            let path = String::from_utf8(path.to_vec()).map_err(|_| error(ErrorCode::Unavailable, "remote path is not UTF-8"))?;
            let relative = path.strip_prefix(&format!("{}/", self.root)).unwrap_or(&path);
            if !query.globs.is_empty() && !globs.is_match(relative) && !globs.is_match(path.rsplit('/').next().unwrap_or(&path)) {
                continue;
            }
            let command = format!("grep -n{options} -C {} -e {} -- {}", query.context, quote(query.pattern), quote(&path));
            let (status, output) = self.connection.run_with_status(&command, &[], false).await?;
            if status > 1 {
                return Err(error(ErrorCode::Unavailable, "remote grep failed"));
            }
            let output = String::from_utf8(output).map_err(|_| error(ErrorCode::Unavailable, "grep result is not UTF-8"))?;
            let mut lines = BTreeMap::new();
            let mut hits = Vec::new();
            for line in output.lines() {
                if let Some((number, text)) = line.split_once(':')
                    && let Ok(number) = number.parse::<u64>()
                {
                    hits.push((number, text.to_owned()));
                    lines.insert(number, text.to_owned());
                } else if let Some((number, text)) = line.split_once('-')
                    && let Ok(number) = number.parse::<u64>()
                {
                    lines.insert(number, text.to_owned());
                }
            }
            for (number, text) in hits {
                let mut before = Vec::new();
                let mut after = Vec::new();
                for offset in (1..=query.context).rev() {
                    if let Some(line) = number.checked_sub(u64::from(offset))
                        && let Some(text) = lines.get(&line)
                    {
                        before.push(text.clone());
                    }
                }
                for offset in 1..=query.context {
                    if let Some(line) = number.checked_add(u64::from(offset))
                        && let Some(text) = lines.get(&line)
                    {
                        after.push(text.clone());
                    }
                }
                matches.push(GrepMatch { path: relative.to_owned(), line: number, text, before, after });
            }
        }
        let limit = usize::try_from(query.max_matches).unwrap_or(usize::MAX);
        let truncated = matches.len() > limit;
        matches.truncate(limit);
        Ok(GrepResult { matches, truncated })
    }
}

impl Workspace for AgentlessWorkspace {
    fn caps(&self) -> &Caps {
        &self.caps
    }
    fn root(&self) -> &str {
        &self.root
    }
    fn fs(&self) -> &dyn Fs {
        self
    }
    fn exec(&self) -> Option<&dyn Exec> {
        Some(self)
    }
    fn search(&self) -> &dyn Search {
        self
    }
}

impl Fs for AgentlessWorkspace {
    fn stat<'a>(&'a self, path: &'a str, hash: bool) -> BoxFuture<'a, Outcome<Meta>> {
        Box::pin(async move { self.meta(path, hash).await })
    }
    fn read<'a>(&'a self, path: &'a str, range: Option<ByteRange>, max_bytes: u64) -> BoxFuture<'a, Outcome<FsReadResult>> {
        Box::pin(async move { self.read_file(path, range, max_bytes).await })
    }
    fn write<'a>(&'a self, req: WriteRequest<'a>) -> BoxFuture<'a, Outcome<WriteOutcome>> {
        Box::pin(async move { self.write_file(req).await })
    }
    fn edit<'a>(&'a self, req: EditRequest<'a>) -> BoxFuture<'a, Outcome<EditOutcome>> {
        Box::pin(async move {
            let read = self.read_file(req.path, None, u64::MAX).await?;
            let text = String::from_utf8(read.content.into_bytes()).map_err(|_| error(ErrorCode::Conflict, "exact edit requires UTF-8"))?;
            let mut text = text;
            let mut replacements = Vec::new();
            for edit in req.edits {
                if edit.old.is_empty() {
                    return Err(error(ErrorCode::Conflict, "empty edit pattern"));
                }
                let count = text.matches(&edit.old).count();
                if count == 0 || (!edit.replace_all && count != 1) {
                    return Err(error(ErrorCode::Conflict, "edit pattern is not unique"));
                }
                replacements.push(u32::try_from(count).map_err(|_| error(ErrorCode::LimitExceeded, "too many replacements"))?);
                text = if edit.replace_all { text.replace(&edit.old, &edit.new) } else { text.replacen(&edit.old, &edit.new, 1) };
            }
            let content = Content::Utf8 { text };
            match req.precondition {
                Precondition::IfAbsent => return Err(error(ErrorCode::PreconditionFailed, "edit target exists")),
                Precondition::IfHash { hash } if hash != &read.hash => {
                    return Err(error(ErrorCode::PreconditionFailed, "remote file changed"));
                }
                Precondition::Any | Precondition::IfHash { .. } => {}
            }
            let write = self
                .write_file(WriteRequest {
                    path: req.path,
                    content: &content,
                    precondition: &Precondition::IfHash { hash: read.hash },
                    create_dirs: false,
                    key: req.key,
                })
                .await?;
            Ok(EditOutcome { write, replacements })
        })
    }
    fn list<'a>(&'a self, req: ListRequest<'a>) -> BoxFuture<'a, Outcome<FsListResult>> {
        Box::pin(async move {
            self.safe(req.path, false).await?;
            let script = format!("find {} -mindepth 1 -maxdepth 1 -print0", quote(req.path));
            let output = self.run(&script, &[]).await?;
            let mut names = output
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .filter_map(|path| {
                    let path = String::from_utf8(path.to_vec()).ok()?;
                    path.rsplit('/').next().map(str::to_owned)
                })
                .filter(|name| req.include_hidden || !name.starts_with('.'))
                .collect::<Vec<_>>();
            names.sort();
            let start = req.page_token.and_then(|token| names.iter().position(|name| name.as_str() == token)).map_or(0, |index| index + 1);
            let mut entries = Vec::new();
            let limit = usize::try_from(req.limit).unwrap_or(usize::MAX);
            for name in names.iter().skip(start).take(limit) {
                let path = format!("{}/{name}", req.path.trim_end_matches('/'));
                let meta = self.meta(&path, false).await?;
                entries.push(DirEntry { name: name.clone(), kind: meta.kind, size: meta.size });
            }
            let next_page = if start + entries.len() < names.len() { entries.last().map(|entry| entry.name.clone()) } else { None };
            Ok(FsListResult { entries, next_page })
        })
    }
    fn mkdir<'a>(&'a self, path: &'a str, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            self.safe(path, true).await?;
            self.run(&format!("mkdir -p -- {}", quote(path)), &[]).await.map(|_| ())
        })
    }
    fn remove<'a>(&'a self, path: &'a str, recursive: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            self.safe(path, false).await?;
            if path == self.root {
                return Err(error(ErrorCode::Denied, "cannot remove workspace root"));
            }
            let quoted = quote(path);
            let script = if recursive {
                format!("rm -rf -- {quoted}")
            } else {
                format!("if [ -d {quoted} ] && [ ! -L {quoted} ]; then rmdir -- {quoted}; else rm -f -- {quoted}; fi")
            };
            self.run(&script, &[]).await.map(|_| ())
        })
    }
    fn rename<'a>(&'a self, from: &'a str, to: &'a str, overwrite: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            self.safe(from, false).await?;
            self.safe(to, true).await?;
            if !overwrite && self.meta(to, false).await.is_ok() {
                return Err(error(ErrorCode::Conflict, "destination exists"));
            }
            self.run(&format!("mv {} -- {} {}", if overwrite { "-f" } else { "-n" }, quote(from), quote(to)), &[]).await?;
            if !overwrite && self.meta(from, false).await.is_ok() {
                return Err(error(ErrorCode::Conflict, "destination appeared during rename"));
            }
            Ok(())
        })
    }
}

fn error(code: ErrorCode, message: &str) -> ProtoError {
    ProtoError::new(code, message)
}

impl Exec for AgentlessWorkspace {
    fn spawn<'a>(&'a self, spec: SpawnSpec<'a>) -> BoxFuture<'a, Outcome<ProcId>> {
        Box::pin(async move {
            self.safe(spec.cwd, false).await?;
            let command = match spec.command {
                Command::Argv { argv } if !argv.is_empty() => argv.iter().map(|arg| quote(arg)).collect::<Vec<_>>().join(" "),
                Command::Argv { .. } => return Err(error(ErrorCode::InvalidParams, "empty command")),
                Command::Shell { script } => format!("sh -c {}", quote(script)),
            };
            let id = ProcId::new(format!("ssh-{}-{}", std::process::id(), PROCESS_SEQUENCE.fetch_add(1, Ordering::Relaxed)));
            let remote_marker = format!("{}/.aim/run/{}.pid", self.home, id.as_str());
            let mut script = format!(
                "umask 077; mkdir -p {} || exit; printf '%s\\n' \"$$\" > {} || exit; cd {} || exit;",
                quote(&format!("{}/.aim/run", self.home)),
                quote(&remote_marker),
                quote(spec.cwd)
            );
            for (name, value) in spec.env {
                if name.is_empty()
                    || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    || name.starts_with(|c: char| c.is_ascii_digit())
                {
                    return Err(error(ErrorCode::InvalidParams, "invalid environment name"));
                }
                let _ = write!(script, " export {name}={};", quote(value));
            }
            let _ = write!(script, " exec {command}");
            let permit =
                Arc::clone(&self.channels).acquire_owned().await.map_err(|_| error(ErrorCode::Unavailable, "SSH channels closed"))?;
            let mut cmd = self.connection.command(&script, spec.pty.is_some());
            cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn().map_err(|_| error(ErrorCode::Unavailable, "cannot start remote process"))?;
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            if !spec.stdin {
                drop(child.stdin.take());
            }
            let process = Arc::new(Process {
                _permit: permit,
                remote_marker: remote_marker.clone(),
                child: Mutex::new(child),
                output: Mutex::new(ProcessOutput { chunks: VecDeque::new(), bytes: 0, last_seq: 0, dropped_before: None }),
                exit: Mutex::new(None),
                streams_open: AtomicU8::new(u8::from(stdout.is_some()) + u8::from(stderr.is_some())),
                notify: Notify::new(),
            });
            if let Some(stdout) = stdout {
                let stream =
                    if spec.pty.is_some() { aim_proto::harness::OutputStream::Pty } else { aim_proto::harness::OutputStream::Stdout };
                tokio::spawn(collect_output(stdout, Arc::clone(&process), stream));
            }
            if let Some(stderr) = stderr {
                tokio::spawn(collect_output(stderr, Arc::clone(&process), aim_proto::harness::OutputStream::Stderr));
            }
            let marker_check = format!("test -s {}", quote(&remote_marker));
            let mut ready = false;
            for _ in 0..10 {
                if self.connection.run(&marker_check, &[]).await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if !ready {
                drop(process.child.lock().await.start_kill());
                return Err(error(ErrorCode::Unavailable, "remote process did not start"));
            }
            if let Some(timeout) = spec.timeout {
                let connection = self.connection.clone();
                let process_for_timeout = Arc::clone(&process);
                tokio::spawn(async move {
                    tokio::time::sleep(timeout).await;
                    let mut child = process_for_timeout.child.lock().await;
                    if child.try_wait().is_ok_and(|status| status.is_none()) {
                        drop(remote_signal(&connection, &process_for_timeout.remote_marker, Signal::Kill).await);
                        drop(child.start_kill());
                        *process_for_timeout.exit.lock().await = Some(ExitStatus::TimedOut);
                        process_for_timeout.notify.notify_waiters();
                    }
                });
            }
            self.processes.lock().await.insert(id.0.clone(), process);
            Ok(id)
        })
    }

    fn read<'a>(&'a self, proc: &'a ProcId, after_seq: u64, max_bytes: u64, wait: Duration) -> BoxFuture<'a, Outcome<ExecReadResult>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.get(proc.as_str()).cloned().ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            if let Ok(Some(status)) = process.child.lock().await.try_wait() {
                let mut exit = process.exit.lock().await;
                if exit.is_none() {
                    *exit = Some(ExitStatus::Exited { code: status.code().unwrap_or(255) });
                }
            }
            let snapshot = || async {
                let output = process.output.lock().await;
                let mut chunks = Vec::new();
                let mut size = 0_u64;
                for chunk in output.chunks.iter().filter(|chunk| chunk.seq > after_seq) {
                    let next = chunk.data.len() as u64;
                    if size.saturating_add(next) > max_bytes {
                        if chunks.is_empty() && max_bytes > 0 {
                            return Err(error(ErrorCode::LimitExceeded, "max_bytes is smaller than the next output chunk"));
                        }
                        break;
                    }
                    size += next;
                    chunks.push(chunk.clone());
                }
                let exit = if process.streams_open.load(Ordering::Acquire) == 0 { *process.exit.lock().await } else { None };
                Ok(ExecReadResult { chunks, dropped_before: output.dropped_before, exit })
            };
            let mut result = snapshot().await?;
            if result.chunks.is_empty() && result.exit.is_none() && !wait.is_zero() {
                let _ = tokio::time::timeout(wait, process.notify.notified()).await;
                if let Ok(Some(status)) = process.child.lock().await.try_wait() {
                    let mut exit = process.exit.lock().await;
                    if exit.is_none() {
                        *exit = Some(ExitStatus::Exited { code: status.code().unwrap_or(255) });
                    }
                }
                result = snapshot().await?;
            }
            Ok(result)
        })
    }

    fn write_stdin<'a>(&'a self, proc: &'a ProcId, data: &'a [u8], eof: bool) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.get(proc.as_str()).cloned().ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            let mut child = process.child.lock().await;
            let stdin = child.stdin.as_mut().ok_or_else(|| error(ErrorCode::Unavailable, "stdin is closed"))?;
            stdin.write_all(data).await.map_err(|_| error(ErrorCode::Unavailable, "remote stdin failed"))?;
            if eof {
                drop(child.stdin.take());
            }
            Ok(())
        })
    }

    fn resize<'a>(&'a self, _proc: &'a ProcId, _size: PtySize) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async { Err(error(ErrorCode::Unavailable, "agentless SSH cannot resize a PTY")) })
    }

    fn signal<'a>(&'a self, proc: &'a ProcId, signal: Signal) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.get(proc.as_str()).cloned().ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            remote_signal(&self.connection, &process.remote_marker, signal).await
        })
    }

    fn release<'a>(&'a self, proc: &'a ProcId) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.remove(proc.as_str()).ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            if process.child.lock().await.try_wait().is_ok_and(|status| status.is_none()) {
                drop(remote_signal(&self.connection, &process.remote_marker, Signal::Kill).await);
            }
            drop(process.child.lock().await.start_kill());
            drop(self.connection.run(&format!("rm -f -- {}", quote(&process.remote_marker)), &[]).await);
            Ok(())
        })
    }
}

async fn collect_output<R: tokio::io::AsyncRead + Unpin>(mut reader: R, process: Arc<Process>, stream: aim_proto::harness::OutputStream) {
    let mut buffer = [0_u8; 8192];
    while let Ok(count) = reader.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        let mut output = process.output.lock().await;
        output.last_seq = output.last_seq.saturating_add(1);
        let seq = output.last_seq;
        output.bytes = output.bytes.saturating_add(count);
        output.chunks.push_back(aim_proto::harness::OutputChunk {
            seq,
            stream,
            data: Content::from_bytes(buffer.get(..count).unwrap_or(&[]).to_vec()),
        });
        while output.bytes > MAX_PROCESS_OUTPUT {
            let Some(removed) = output.chunks.pop_front() else {
                break;
            };
            output.bytes = output.bytes.saturating_sub(removed.data.len());
            output.dropped_before = Some(removed.seq);
        }
        process.notify.notify_waiters();
    }
    process.streams_open.fetch_sub(1, Ordering::Release);
    process.notify.notify_waiters();
}

async fn remote_signal(connection: &Connection, marker: &str, signal: Signal) -> Outcome<()> {
    let name = match signal {
        Signal::Interrupt => "INT",
        Signal::Terminate => "TERM",
        Signal::Kill => "KILL",
    };
    let script = format!("p=$(cat -- {}) || exit; case \"$p\" in ''|*[!0-9]*) exit 1;; esac; kill -{name} \"$p\"", quote(marker));
    connection.run(&script, &[]).await.map(|_| ())
}

impl Search for AgentlessWorkspace {
    fn grep<'a>(&'a self, query: GrepQuery<'a>) -> BoxFuture<'a, Outcome<GrepResult>> {
        Box::pin(async move {
            self.safe(query.path, false).await?;
            if query.context > 1000 {
                return Err(error(ErrorCode::LimitExceeded, "search context is too large"));
            }
            let insensitive = match query.case {
                CaseMode::Insensitive => true,
                CaseMode::Sensitive => false,
                CaseMode::Smart => !query.pattern.chars().any(char::is_uppercase),
            };
            let mut options = String::new();
            if insensitive {
                options.push_str(" -i");
            }
            if query.fixed_strings {
                options.push_str(" -F");
            }
            if !self.caps.native_search {
                return self.grep_fallback(&query, &options).await;
            }
            let mut args = format!("rg --json{options} -C {}", query.context);
            for glob in query.globs {
                let _ = write!(args, " -g {}", quote(glob));
            }
            let _ = write!(args, " -e {} -- {}", quote(query.pattern), quote(query.path));
            let output = self.connection.run_with_status(&args, &[], false).await?;
            if output.0 > 1 {
                return Err(error(ErrorCode::Unavailable, "remote search failed"));
            }
            let text = String::from_utf8(output.1).map_err(|_| error(ErrorCode::Unavailable, "search result is not UTF-8"))?;
            let mut matches = Vec::new();
            let mut context_lines = BTreeMap::new();
            for line in text.lines() {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let event = value.get("type").and_then(serde_json::Value::as_str);
                if !matches!(event, Some("match" | "context")) {
                    continue;
                }
                let Some(data) = value.get("data") else {
                    continue;
                };
                let Some(path) = data.get("path").and_then(|item| item.get("text")).and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(line_number) = data.get("line_number").and_then(serde_json::Value::as_u64) else {
                    continue;
                };
                let Some(content) = data.get("lines").and_then(|item| item.get("text")).and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let path = path.strip_prefix(&format!("{}/", self.root)).unwrap_or(path).to_owned();
                let content = content.trim_end_matches('\n').to_owned();
                context_lines.insert((path.clone(), line_number), content.clone());
                if event == Some("match") {
                    matches.push(GrepMatch { path, line: line_number, text: content, before: Vec::new(), after: Vec::new() });
                }
            }
            for item in &mut matches {
                for offset in (1..=query.context).rev() {
                    if let Some(line) = item.line.checked_sub(u64::from(offset))
                        && let Some(text) = context_lines.get(&(item.path.clone(), line))
                    {
                        item.before.push(text.clone());
                    }
                }
                for offset in 1..=query.context {
                    if let Some(line) = item.line.checked_add(u64::from(offset))
                        && let Some(text) = context_lines.get(&(item.path.clone(), line))
                    {
                        item.after.push(text.clone());
                    }
                }
            }
            let limit = usize::try_from(query.max_matches).unwrap_or(usize::MAX);
            let truncated = matches.len() > limit;
            matches.truncate(limit);
            Ok(GrepResult { matches, truncated })
        })
    }

    fn glob<'a>(&'a self, query: GlobQuery<'a>) -> BoxFuture<'a, Outcome<GlobResult>> {
        Box::pin(async move {
            self.safe(query.path, false).await?;
            let mut builder = GlobSetBuilder::new();
            for pattern in query.patterns {
                builder.add(Glob::new(pattern).map_err(|_| error(ErrorCode::InvalidParams, "invalid glob"))?);
            }
            let patterns = builder.build().map_err(|_| error(ErrorCode::InvalidParams, "invalid glob"))?;
            let output = if self.caps.native_search {
                self.run(&format!("cd {} && rg --files -0 --hidden", quote(query.path)), &[]).await?
            } else {
                self.run(&format!("cd {} && find . -type f -print0", quote(query.path)), &[]).await?
            };
            let prefix = query.path.strip_prefix(&self.root).unwrap_or("").trim_matches('/');
            let mut paths = output
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .filter_map(|path| String::from_utf8(path.to_vec()).ok())
                .map(|path| path.trim_start_matches("./").to_owned())
                .filter_map(|path| {
                    let full = if prefix.is_empty() { path.clone() } else { format!("{prefix}/{path}") };
                    (patterns.is_match(&path) || patterns.is_match(&full)).then_some(full)
                })
                .collect::<Vec<_>>();
            paths.sort();
            let limit = usize::try_from(query.max_results).unwrap_or(usize::MAX);
            let truncated = paths.len() > limit;
            paths.truncate(limit);
            Ok(GlobResult { paths, truncated })
        })
    }
}
