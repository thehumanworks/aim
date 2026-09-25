//! Agentless workspace operations over a multiplexed OpenSSH connection.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
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
use tokio::process::{Child, ChildStdin};
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
    mode_query: &'static str,
    channels: Arc<Semaphore>,
    signal_channel: Arc<Semaphore>,
    process_slots: Arc<Semaphore>,
    mutations: Mutex<()>,
    processes: Mutex<BTreeMap<String, Arc<Process>>>,
}

struct Process {
    remote_marker: String,
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    output: Mutex<crate::ring::OutputRing>,
    exit: Mutex<Option<ExitStatus>>,
    kill_failure: Mutex<Option<ProtoError>>,
    last_signal: AtomicI32,
    released: AtomicBool,
    control: Mutex<()>,
    streams_open: AtomicU8,
    notify: Notify,
}

const MAX_PROCESS_OUTPUT: usize = 8 * 1024 * 1024;
const INLINE_SCRIPT_LIMIT: usize = 8 * 1024;
const MAX_SCRIPT_BYTES: usize = 1024 * 1024;
const PGID_QUERY: &str = "if [ -r \"/proc/$p/stat\" ]; then IFS= read -r stat < \"/proc/$p/stat\" || exit 44; fields=${stat##*) }; set -- $fields; g=$3; else g=$(ps -A -o pid= -o pgid= | awk -v wanted=\"$p\" '$1 == wanted { print $2; exit }'); fi; case \"$g\" in ''|*[!0-9]*) exit 44;; esac;";

impl AgentlessWorkspace {
    /// Open a remote directory, resolving its canonical root on the remote host.
    ///
    /// # Errors
    /// Returns a protocol error if the remote root cannot be resolved.
    pub async fn open(connection: Connection, root: &str) -> Outcome<Self> {
        let root_argument = if root.starts_with('/') { root.to_owned() } else { format!("./{root}") };
        let output = connection.run(&format!("realpath {}", quote(&root_argument)), &[]).await?;
        let root = realpath_output(output)?;
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
        let mode_query = if connection.run("stat -c %a . >/dev/null 2>&1", &[]).await.is_ok() {
            "stat -c %a"
        } else if connection.run("stat -f %Lp . >/dev/null 2>&1", &[]).await.is_ok() {
            "stat -f %Lp"
        } else {
            return Err(error(ErrorCode::Unavailable, "remote stat cannot report file modes"));
        };
        let caps = Caps {
            exec: true,
            pty: true,
            watch: false,
            native_search,
            atomic_rename: true,
            resumable: false,
            max_concurrency: Some(6),
            os: os.to_owned(),
            arch: arch.to_owned(),
            shell: Some("sh".to_owned()),
        };
        Ok(Self {
            connection,
            root,
            home: platform.home,
            caps,
            mode_query,
            channels: Arc::new(Semaphore::new(3)),
            signal_channel: Arc::new(Semaphore::new(1)),
            process_slots: Arc::new(Semaphore::new(6)),
            mutations: Mutex::new(()),
            processes: Mutex::new(BTreeMap::new()),
        })
    }

    async fn run(&self, script: &str, input: &[u8]) -> Outcome<Vec<u8>> {
        let (status, output) = self.run_status(script, input).await?;
        if status == 0 { Ok(output) } else { Err(error(ErrorCode::Unavailable, "remote command failed")) }
    }

    async fn run_status(&self, script: &str, input: &[u8]) -> Outcome<(i32, Vec<u8>)> {
        let _permit = tokio::time::timeout(Duration::from_secs(5), self.channels.acquire())
            .await
            .map_err(|_| error(ErrorCode::Unavailable, "SSH channel capacity exhausted"))?
            .map_err(|_| error(ErrorCode::Unavailable, "SSH channels closed"))?;
        self.connection.run_with_status(script, input, false).await
    }

    async fn prepared_process_script(&self, script: &str, id: &ProcId) -> Outcome<(String, Option<String>)> {
        if script.contains('\0') {
            return Err(error(ErrorCode::InvalidParams, "remote process script contains NUL"));
        }
        if script.len() > MAX_SCRIPT_BYTES {
            return Err(error(ErrorCode::LimitExceeded, "remote process script is too large"));
        }
        if script.len() <= INLINE_SCRIPT_LIMIT {
            return Ok((script.to_owned(), None));
        }
        let run_dir = format!("{}/.aim/run", self.home);
        let path = format!("{run_dir}/{}.sh", id.as_str());
        let upload = format!("umask 077; mkdir -p {0} || exit; (set -C; cat > {1})", quote(&run_dir), quote(&path));
        self.run(&upload, script.as_bytes()).await?;
        // The remote shell consumes and removes the staged script before evaluating it. `eval`
        // leaves stdin attached to the user's process, and the final `exec` keeps its PID/PGID.
        let command = format!(
            "f={}; trap 'rm -f \"$f\"' EXIT HUP INT TERM; payload=$(cat \"$f\") || exit; rm -f \"$f\" || exit; trap - EXIT HUP INT TERM; eval \"$payload\"",
            quote(&path)
        );
        Ok((command, Some(path)))
    }

    async fn safe(&self, path: &str, may_create: bool) -> Outcome<String> {
        if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(error(ErrorCode::Denied, "path is not confined"));
        }
        let script = if may_create {
            format!(
                "if [ -e {0} ] || [ -L {0} ]; then printf 'E'; realpath {0}; else p=$(dirname -- {0}); while [ ! -e \"$p\" ]; do p=$(dirname -- \"$p\"); done; printf 'A%s\\0' \"$p\"; realpath \"$p\"; fi",
                quote(path)
            )
        } else {
            format!("[ -e {0} ] || [ -L {0} ] || exit 44; realpath {0}", quote(path))
        };
        let (status, output) = self.run_status(&script, &[]).await?;
        if status != 0 {
            return Err(error(if status == 44 { ErrorCode::NotFound } else { ErrorCode::Unavailable }, "remote path cannot be resolved"));
        }
        let resolved = if may_create {
            match output.first().copied() {
                Some(b'E') => realpath_output(output.get(1..).unwrap_or_default().to_vec())?,
                Some(b'A') => {
                    let body = output.get(1..).unwrap_or_default();
                    let split =
                        body.iter().position(|byte| *byte == 0).ok_or_else(|| error(ErrorCode::Unavailable, "invalid remote path"))?;
                    let ancestor = std::str::from_utf8(body.get(..split).unwrap_or_default())
                        .map_err(|_| error(ErrorCode::Unavailable, "remote path is not UTF-8"))?;
                    let canonical = realpath_output(body.get(split + 1..).unwrap_or_default().to_vec())?;
                    let suffix = path.strip_prefix(ancestor).ok_or_else(|| error(ErrorCode::Unavailable, "invalid remote path"))?;
                    format!("{canonical}{suffix}")
                }
                _ => return Err(error(ErrorCode::Unavailable, "invalid remote path")),
            }
        } else {
            realpath_output(output)?
        };
        if resolved != self.root && !resolved.starts_with(&format!("{}/", self.root)) {
            return Err(error(ErrorCode::Denied, "path leaves workspace root"));
        }
        Ok(resolved)
    }

    async fn hash(&self, path: &str) -> Outcome<ContentHash> {
        let script = format!(
            "if command -v sha256sum >/dev/null 2>&1; then sha256sum < {0}; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 < {0}; else exit 127; fi",
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
        let (status, output) = self.run_status(&script, &[]).await?;
        if status != 0 {
            return Err(error(if status == 44 { ErrorCode::NotFound } else { ErrorCode::Unavailable }, "remote metadata failed"));
        }
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
        let hash = if with_hash && matches!(kind, EntryKind::File | EntryKind::Symlink) { Some(self.hash(path).await?) } else { None };
        Ok(Meta { kind, size, mtime_ms: None, hash })
    }

    async fn read_file(&self, path: &str, range: Option<ByteRange>, max_bytes: u64, hash: bool) -> Outcome<FsReadResult> {
        let target = self.safe(path, false).await?;
        let start = range.map_or(0, |range| range.start);
        let count = range.map_or(max_bytes, |range| range.len.min(max_bytes));
        let body = if !hash {
            // POSIX dd with bs=1 limits the remote file reads exactly, even when a pipe's
            // producer would otherwise read ahead of head -c.
            format!("dd if=\"$p\" bs=1 skip={start} count={count} 2>/dev/null")
        } else if start == 0 && count == u64::MAX {
            "cat -- \"$p\"".to_owned()
        } else {
            format!("tail -c +{} -- \"$p\" | head -c {count}", start.saturating_add(1))
        };
        let header = if hash {
            "if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum < \"$p\"); elif command -v shasum >/dev/null 2>&1; then h=$(shasum -a 256 < \"$p\"); else exit 127; fi; printf '%s\\n%s\\n' \"$n\" \"${h%% *}\""
        } else {
            "printf '%s\\n' \"$n\""
        };
        let script = format!(
            "p={}; [ -f \"$p\" ] || exit 43; n=$(stat -c %s \"$p\" 2>/dev/null || stat -f %z \"$p\" 2>/dev/null) || exit; {header}; {body}",
            quote(&target)
        );
        let (status, output) = self.run_status(&script, &[]).await?;
        if status == 43 {
            return Err(error(ErrorCode::Conflict, "remote path is not a file"));
        }
        if status != 0 {
            return Err(error(ErrorCode::Unavailable, "remote read failed"));
        }
        let first =
            output.iter().position(|byte| *byte == b'\n').ok_or_else(|| error(ErrorCode::Unavailable, "invalid remote read header"))?;
        let second = if hash {
            output
                .iter()
                .enumerate()
                .skip(first + 1)
                .find_map(|(index, byte)| (*byte == b'\n').then_some(index))
                .ok_or_else(|| error(ErrorCode::Unavailable, "invalid remote read header"))?
        } else {
            first
        };
        let size = std::str::from_utf8(output.get(..first).unwrap_or_default())
            .map_err(|_| error(ErrorCode::Unavailable, "invalid remote size"))?
            .trim()
            .parse::<u64>()
            .map_err(|_| error(ErrorCode::Unavailable, "invalid remote size"))?;
        let digest = if hash {
            let digest = std::str::from_utf8(output.get(first + 1..second).unwrap_or_default())
                .map_err(|_| error(ErrorCode::Unavailable, "invalid remote hash"))?;
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(error(ErrorCode::Unavailable, "invalid remote hash"));
            }
            Some(ContentHash(format!("sha256:{digest}")))
        } else {
            None
        };
        let content = output.get(second + 1..).unwrap_or_default().to_vec();
        let requested = range.map_or(size.saturating_sub(start), |range| range.len);
        Ok(FsReadResult {
            content: Content::from_bytes(content),
            size,
            hash: digest,
            truncated: requested > count && start.saturating_add(count) < size,
        })
    }

    async fn write_file(&self, req: WriteRequest<'_>) -> Outcome<WriteOutcome> {
        let _mutation = self.mutations.lock().await;
        self.write_file_locked(req).await
    }

    async fn write_file_locked(&self, req: WriteRequest<'_>) -> Outcome<WriteOutcome> {
        let target = self.safe(req.path, true).await?;
        let current = match self.meta(req.path, true).await {
            Ok(meta) => Some(meta),
            Err(err) if err.code == ErrorCode::NotFound => None,
            Err(err) => return Err(err),
        };
        match req.precondition {
            Precondition::IfAbsent if current.is_some() => return Err(error(ErrorCode::PreconditionFailed, "remote file exists")),
            Precondition::IfHash { hash } if current.as_ref().and_then(|meta| meta.hash.as_ref()) != Some(hash) => {
                return Err(error(ErrorCode::PreconditionFailed, "remote file changed"));
            }
            _ => {}
        }
        let bytes = req.content.clone().into_bytes();
        let quoted = quote(&target);
        let precondition = match req.precondition {
            Precondition::Any => String::new(),
            Precondition::IfAbsent => "[ ! -e \"$p\" ] && [ ! -L \"$p\" ] || exit 42;".to_owned(),
            Precondition::IfHash { hash } => format!(
                "if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum < \"$p\"); elif command -v shasum >/dev/null 2>&1; then h=$(shasum -a 256 < \"$p\"); else exit 127; fi; [ \"sha256:${{h%% *}}\" = {} ] || exit 42;",
                quote(&hash.0)
            ),
        };
        let script = format!(
            "p={quoted}; d=$(dirname -- \"$p\"); {} [ -d \"$p\" ] && exit 43; t=$(mktemp \"$d/.aimx.XXXXXXXX\") || exit; trap 'rm -f \"$t\"' EXIT HUP INT TERM; cat > \"$t\" || exit; {precondition} if [ -e \"$p\" ]; then mode=$({} \"$p\" 2>/dev/null) || exit; case \"$mode\" in ''|*[!0-7]*) exit 43;; esac; else mode=$(printf '%o' $((0666 & ~0$(umask)))); fi; chmod \"$mode\" \"$t\" || exit; {} trap - EXIT HUP INT TERM",
            if req.create_dirs { "mkdir -p \"$d\" || exit 43;" } else { "" },
            self.mode_query,
            if matches!(req.precondition, Precondition::IfAbsent) {
                "ln \"$t\" \"$p\" || exit 42; rm -f \"$t\";"
            } else {
                "mv -f -- \"$t\" \"$p\" || exit;"
            }
        );
        let (status, _) = self.run_status(&script, &bytes).await?;
        if status == 42 {
            return Err(error(ErrorCode::PreconditionFailed, "remote file changed"));
        }
        if status == 43 {
            return Err(error(ErrorCode::Conflict, "remote path is a directory"));
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

    async fn grep_fallback(&self, query: &GrepQuery<'_>, options: &str, target: &str) -> Outcome<GrepResult> {
        let mut builder = GlobSetBuilder::new();
        for pattern in query.globs {
            builder.add(Glob::new(pattern).map_err(|_| error(ErrorCode::InvalidParams, "invalid search glob"))?);
        }
        let globs = builder.build().map_err(|_| error(ErrorCode::InvalidParams, "invalid search glob"))?;
        let cap = 16 * 1024 * 1024;
        let script = format!(
            "find {} -type f -exec sh -c 'pattern=$1; shift; for f do printf \"F%s\\0\" \"$f\"; grep -EnH{options} -C {} -e \"$pattern\" -- \"$f\" 2>/dev/null || :; printf \"\\0\"; done' sh {} {{}} + | head -c {cap}",
            quote(target),
            query.context,
            quote(query.pattern)
        );
        let files = self.run(&script, &[]).await?;
        let mut matches = Vec::new();
        let mut fields = files.split(|byte| *byte == 0);
        while let (Some(path), Some(output)) = (fields.next(), fields.next()) {
            let path = std::str::from_utf8(path.strip_prefix(b"F").unwrap_or_default())
                .map_err(|_| error(ErrorCode::Unavailable, "remote path is not UTF-8"))?;
            let relative = path.strip_prefix(&format!("{}/", self.root)).unwrap_or(path);
            if !query.globs.is_empty() && !globs.is_match(relative) && !globs.is_match(path.rsplit('/').next().unwrap_or(path)) {
                continue;
            }
            let output = String::from_utf8_lossy(output);
            let mut lines = BTreeMap::new();
            let mut hits = Vec::new();
            for line in output.lines() {
                let line = line.strip_prefix(path).unwrap_or(line);
                let line = line.strip_prefix(':').or_else(|| line.strip_prefix('-')).unwrap_or(line);
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
        let truncated = matches.len() > limit || files.len() >= cap;
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
        self.caps.exec.then_some(self)
    }
    fn search(&self) -> &dyn Search {
        self
    }
}

impl Drop for AgentlessWorkspace {
    fn drop(&mut self) {
        let Ok(processes) = self.processes.try_lock() else { return };
        for process in processes.values() {
            if process.child.try_lock().is_ok_and(|mut child| child.try_wait().is_ok_and(|status| status.is_none())) {
                let script = signal_script(&process.remote_marker, Signal::Kill);
                let mut command = self.connection.command(&script, false);
                command
                    .as_std_mut()
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                drop(command.as_std_mut().spawn());
            }
            if let Ok(mut child) = process.child.try_lock() {
                drop(child.start_kill());
            }
        }
    }
}

impl Fs for AgentlessWorkspace {
    fn stat<'a>(&'a self, path: &'a str, hash: bool) -> BoxFuture<'a, Outcome<Meta>> {
        Box::pin(async move { self.meta(path, hash).await })
    }
    fn read<'a>(&'a self, path: &'a str, range: Option<ByteRange>, max_bytes: u64, hash: bool) -> BoxFuture<'a, Outcome<FsReadResult>> {
        Box::pin(async move { self.read_file(path, range, max_bytes, hash).await })
    }
    fn write<'a>(&'a self, req: WriteRequest<'a>) -> BoxFuture<'a, Outcome<WriteOutcome>> {
        Box::pin(async move { self.write_file(req).await })
    }
    fn edit<'a>(&'a self, req: EditRequest<'a>) -> BoxFuture<'a, Outcome<EditOutcome>> {
        Box::pin(async move {
            let _mutation = self.mutations.lock().await;
            let read = self.read_file(req.path, None, u64::MAX, true).await?;
            let read_hash = read.hash.ok_or_else(|| error(ErrorCode::Unavailable, "remote read omitted hash"))?;
            let applied = crate::edit::apply_edits(&read.content.into_bytes(), req.edits).map_err(ProtoError::from)?;
            let content = Content::from_bytes(applied.content);
            match req.precondition {
                Precondition::IfAbsent => return Err(error(ErrorCode::PreconditionFailed, "edit target exists")),
                Precondition::IfHash { hash } if hash != &read_hash => {
                    return Err(error(ErrorCode::PreconditionFailed, "remote file changed"));
                }
                Precondition::Any | Precondition::IfHash { .. } => {}
            }
            let write = self
                .write_file_locked(WriteRequest {
                    path: req.path,
                    content: &content,
                    precondition: &Precondition::IfHash { hash: read_hash },
                    create_dirs: false,
                    key: req.key,
                })
                .await?;
            Ok(EditOutcome { write, replacements: applied.replacements })
        })
    }
    fn list<'a>(&'a self, req: ListRequest<'a>) -> BoxFuture<'a, Outcome<FsListResult>> {
        Box::pin(async move {
            let resolved = self.safe(req.path, false).await?;
            let script = format!(
                "find {} -mindepth 1 -maxdepth 1 -exec sh -c 'printf \"%s\\0\" \"$#\"; for p do printf \"%s\\0\" \"${{p##*/}}\"; done; if [ \"$(uname -s)\" = Darwin ]; then stat -f \"%HT|%z\" \"$@\"; else stat -c \"%F|%s\" \"$@\"; fi | tr \"\\n\" \"\\0\"' sh {{}} +",
                quote(&resolved)
            );
            let output = self.run(&script, &[]).await?;
            let mut entries = Vec::new();
            let mut fields = output.split(|byte| *byte == 0);
            while let Some(count) = fields.next().filter(|part| !part.is_empty()) {
                let count = std::str::from_utf8(count)
                    .map_err(|_| error(ErrorCode::Unavailable, "invalid remote listing"))?
                    .parse::<usize>()
                    .map_err(|_| error(ErrorCode::Unavailable, "invalid remote listing"))?;
                let names = (0..count)
                    .map(|_| {
                        fields.next().ok_or_else(|| error(ErrorCode::Unavailable, "incomplete remote listing")).and_then(|name| {
                            String::from_utf8(name.to_vec()).map_err(|_| error(ErrorCode::Unavailable, "remote name is not UTF-8"))
                        })
                    })
                    .collect::<Outcome<Vec<_>>>()?;
                for name in names {
                    let metadata = fields.next().ok_or_else(|| error(ErrorCode::Unavailable, "incomplete remote listing"))?;
                    if !req.include_hidden && name.starts_with('.') {
                        continue;
                    }
                    let metadata = std::str::from_utf8(metadata).map_err(|_| error(ErrorCode::Unavailable, "invalid remote listing"))?;
                    let (kind, size) = metadata.split_once('|').ok_or_else(|| error(ErrorCode::Unavailable, "invalid remote listing"))?;
                    let kind = match kind {
                        "Regular File" | "regular file" | "regular empty file" => EntryKind::File,
                        "Directory" | "directory" => EntryKind::Dir,
                        "Symbolic Link" | "symbolic link" => EntryKind::Symlink,
                        _ => EntryKind::Other,
                    };
                    let size = if kind == EntryKind::File {
                        size.parse::<u64>().map_err(|_| error(ErrorCode::Unavailable, "invalid remote size"))?
                    } else {
                        0
                    };
                    entries.push(DirEntry { name, kind, size });
                }
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let start = req.page_token.map_or(0, |token| entries.partition_point(|entry| entry.name.as_str() <= token));
            let limit = usize::try_from(req.limit).unwrap_or(usize::MAX);
            let end = start.saturating_add(limit).min(entries.len());
            let next_page = if end < entries.len() { entries.get(end.saturating_sub(1)).map(|entry| entry.name.clone()) } else { None };
            Ok(FsListResult { entries: entries.get(start..end).unwrap_or_default().to_vec(), next_page })
        })
    }
    fn mkdir<'a>(&'a self, path: &'a str, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let _mutation = self.mutations.lock().await;
            let target = self.safe(path, true).await?;
            let (status, _) = self.run_status(&format!("mkdir -p -- {}", quote(&target)), &[]).await?;
            if status == 0 { Ok(()) } else { Err(error(ErrorCode::Conflict, "remote mkdir failed")) }
        })
    }
    fn remove<'a>(&'a self, path: &'a str, recursive: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let _mutation = self.mutations.lock().await;
            let resolved = self.safe(path, false).await?;
            if resolved == self.root {
                return Err(error(ErrorCode::Denied, "cannot remove workspace root"));
            }
            let quoted = quote(path);
            let script = if recursive {
                format!("rm -rf -- {quoted}")
            } else {
                format!("if [ -d {quoted} ] && [ ! -L {quoted} ]; then rmdir -- {quoted}; else rm -f -- {quoted}; fi")
            };
            let (status, _) = self.run_status(&script, &[]).await?;
            if status == 0 { Ok(()) } else { Err(error(ErrorCode::Conflict, "remote remove failed")) }
        })
    }
    fn rename<'a>(&'a self, from: &'a str, to: &'a str, overwrite: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let _mutation = self.mutations.lock().await;
            let source = self.safe(from, false).await?;
            let destination = self.safe(to, true).await?;
            if source == self.root || destination == self.root {
                return Err(error(ErrorCode::Denied, "cannot rename workspace root"));
            }
            if !overwrite {
                match self.meta(to, false).await {
                    Ok(_) => return Err(error(ErrorCode::Conflict, "destination exists")),
                    Err(err) if err.code == ErrorCode::NotFound => {}
                    Err(err) => return Err(err),
                }
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

fn realpath_output(output: Vec<u8>) -> Outcome<String> {
    let text = String::from_utf8(output).map_err(|_| error(ErrorCode::Unavailable, "remote path is not UTF-8"))?;
    text.strip_suffix('\n').map(str::to_owned).ok_or_else(|| error(ErrorCode::Unavailable, "invalid realpath output"))
}

fn process_script(home: &str, spec: &SpawnSpec<'_>, cwd: &str, marker: &str) -> Outcome<String> {
    let command = match spec.command {
        Command::Argv { argv } if !argv.is_empty() => argv.iter().map(|arg| quote(arg)).collect::<Vec<_>>().join(" "),
        Command::Argv { .. } => return Err(error(ErrorCode::InvalidParams, "empty command")),
        Command::Shell { script } => format!("sh -c {}", quote(script)),
    };
    let mut script = format!("umask 077; mkdir -p {} || exit; cd {} || exit;", quote(&format!("{home}/.aim/run")), quote(cwd));
    for (name, value) in spec.env {
        if name.is_empty()
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || name.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(error(ErrorCode::InvalidParams, "invalid environment name"));
        }
        let _ = write!(script, " export {name}={};", quote(value));
    }
    let pty_setup = spec.pty.map_or_else(String::new, |size| format!("stty rows {} cols {} 2>/dev/null || :;", size.rows, size.cols));
    let _ = write!(script, " {pty_setup} p=$$; {PGID_QUERY} printf '%s %s\\n' \"$p\" \"$g\" > {} || exit; exec {command}", quote(marker));
    Ok(script)
}

fn spawn_waiter(process: &Arc<Process>, permit: OwnedSemaphorePermit) {
    let waiter = Arc::downgrade(process);
    tokio::spawn(async move {
        let _permit = permit;
        loop {
            let Some(process) = waiter.upgrade() else { break };
            let control = process.control.lock().await;
            let status = process.child.lock().await.try_wait();
            if let Ok(Some(status)) = status {
                let mut exit = process.exit.lock().await;
                if exit.is_none() {
                    let signal = process.last_signal.load(Ordering::Acquire);
                    *exit = Some(if signal != 0 {
                        ExitStatus::Signaled { signal }
                    } else {
                        ExitStatus::Exited { code: status.code().unwrap_or(255) }
                    });
                }
                process.notify.notify_waiters();
                break;
            }
            drop(control);
            drop(process);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });
}

fn spawn_timeout(process: &Arc<Process>, connection: Connection, signal_channel: Arc<Semaphore>, timeout: Duration) {
    let process = Arc::downgrade(process);
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let Some(process) = process.upgrade() else { return };
        let _control = process.control.lock().await;
        if process.released.load(Ordering::Acquire) {
            return;
        }
        let running = process.child.lock().await.try_wait().is_ok_and(|status| status.is_none());
        if running {
            let mut killed = false;
            for attempt in 1..=3 {
                match remote_signal(&connection, &signal_channel, &process.remote_marker, Signal::Kill).await {
                    Ok(()) | Err(ProtoError { code: ErrorCode::NotFound, .. }) => {
                        killed = true;
                        break;
                    }
                    Err(cause) => {
                        tracing::warn!(attempt, code = ?cause.code, "remote timeout kill failed; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            if killed {
                *process.exit.lock().await = Some(ExitStatus::TimedOut);
                process.notify.notify_waiters();
                drop(process.child.lock().await.start_kill());
            } else {
                tracing::error!("remote timeout kill failed after retries; process remains tracked");
                *process.kill_failure.lock().await = Some(error(ErrorCode::Unavailable, "remote timeout kill failed"));
                process.notify.notify_waiters();
            }
        }
    });
}

impl Exec for AgentlessWorkspace {
    fn spawn<'a>(&'a self, spec: SpawnSpec<'a>) -> BoxFuture<'a, Outcome<ProcId>> {
        Box::pin(async move {
            let cwd = self.safe(spec.cwd, false).await?;
            let id = ProcId::new(format!("ssh-{}", crate::id::random_hex()));
            let remote_marker = format!("{}/.aim/run/{}.pid", self.home, id.as_str());
            let script = process_script(&self.home, &spec, &cwd, &remote_marker)?;
            let permit = Arc::clone(&self.process_slots)
                .try_acquire_owned()
                .map_err(|_| error(ErrorCode::LimitExceeded, "too many remote processes"))?;
            let (command_script, staged_path) = self.prepared_process_script(&script, &id).await?;
            if let Err(cause) = self.connection.ensure_master().await {
                if let Some(path) = &staged_path {
                    drop(self.run(&format!("rm -f -- {}", quote(path)), &[]).await);
                }
                return Err(cause);
            }
            let mut cmd = self.connection.command(&command_script, spec.pty.is_some());
            cmd.kill_on_drop(true);
            cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let Ok(mut child) = cmd.spawn() else {
                if let Some(path) = staged_path {
                    drop(self.run(&format!("rm -f -- {}", quote(&path)), &[]).await);
                }
                return Err(error(ErrorCode::Unavailable, "cannot start remote process"));
            };
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let stdin = if spec.stdin { child.stdin.take() } else { None };
            let process = Arc::new(Process {
                remote_marker: remote_marker.clone(),
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
                output: Mutex::new(crate::ring::OutputRing::new(MAX_PROCESS_OUTPUT)),
                exit: Mutex::new(None),
                kill_failure: Mutex::new(None),
                last_signal: AtomicI32::new(0),
                released: AtomicBool::new(false),
                control: Mutex::new(()),
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
                if self.run(&marker_check, &[]).await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if !ready {
                drop(process.child.lock().await.start_kill());
                if let Some(path) = &staged_path {
                    drop(self.run(&format!("rm -f -- {}", quote(path)), &[]).await);
                }
                return Err(error(ErrorCode::Unavailable, "remote process did not start"));
            }
            spawn_waiter(&process, permit);
            if let Some(timeout) = spec.timeout {
                spawn_timeout(&process, self.connection.clone(), Arc::clone(&self.signal_channel), timeout);
            }
            self.processes.lock().await.insert(id.0.clone(), process);
            Ok(id)
        })
    }

    fn read<'a>(&'a self, proc: &'a ProcId, after_seq: u64, max_bytes: u64, wait: Duration) -> BoxFuture<'a, Outcome<ExecReadResult>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.get(proc.as_str()).cloned().ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            {
                let _control = process.control.lock().await;
                if let Ok(Some(status)) = process.child.lock().await.try_wait() {
                    let mut exit = process.exit.lock().await;
                    if exit.is_none() {
                        let signal = process.last_signal.load(Ordering::Acquire);
                        *exit = Some(if signal != 0 {
                            ExitStatus::Signaled { signal }
                        } else {
                            ExitStatus::Exited { code: status.code().unwrap_or(255) }
                        });
                    }
                }
            }
            if let Some(failure) = process.kill_failure.lock().await.clone() {
                return Err(failure);
            }
            let snapshot = || async {
                let output = process.output.lock().await;
                let mut chunks = Vec::new();
                let slice = output.read(after_seq, usize::try_from(max_bytes).unwrap_or(usize::MAX));
                for chunk in slice.chunks {
                    chunks.push(aim_proto::harness::OutputChunk {
                        seq: chunk.seq,
                        stream: chunk.stream,
                        data: Content::from_bytes(chunk.data),
                    });
                }
                let exit = if process.streams_open.load(Ordering::Acquire) == 0 { *process.exit.lock().await } else { None };
                ExecReadResult { chunks, dropped_before: slice.dropped_before, exit }
            };
            let notified = process.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut result = snapshot().await;
            if result.chunks.is_empty() && result.exit.is_none() && !wait.is_zero() {
                let _ = tokio::time::timeout(wait, &mut notified).await;
                let _control = process.control.lock().await;
                if let Ok(Some(status)) = process.child.lock().await.try_wait() {
                    let mut exit = process.exit.lock().await;
                    if exit.is_none() {
                        let signal = process.last_signal.load(Ordering::Acquire);
                        *exit = Some(if signal != 0 {
                            ExitStatus::Signaled { signal }
                        } else {
                            ExitStatus::Exited { code: status.code().unwrap_or(255) }
                        });
                    }
                }
                if let Some(failure) = process.kill_failure.lock().await.clone() {
                    return Err(failure);
                }
                result = snapshot().await;
            }
            Ok(result)
        })
    }

    fn write_stdin<'a>(&'a self, proc: &'a ProcId, data: &'a [u8], eof: bool) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.get(proc.as_str()).cloned().ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            let mut input = process.stdin.lock().await;
            let stdin = input.as_mut().ok_or_else(|| error(ErrorCode::Unavailable, "stdin is closed"))?;
            tokio::time::timeout(Duration::from_secs(5), stdin.write_all(data))
                .await
                .map_err(|_| error(ErrorCode::Unavailable, "remote stdin stalled"))?
                .map_err(|_| error(ErrorCode::Unavailable, "remote stdin failed"))?;
            if eof {
                drop(input.take());
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
            let _control = process.control.lock().await;
            if process.released.load(Ordering::Acquire) {
                return Err(error(ErrorCode::NotFound, "process already released"));
            }
            if process.child.lock().await.try_wait().is_ok_and(|status| status.is_some()) {
                return Err(error(ErrorCode::NotFound, "process already exited"));
            }
            let number = match signal {
                Signal::Interrupt => 2,
                Signal::Terminate => 15,
                Signal::Kill => 9,
            };
            let previous = process.last_signal.swap(number, Ordering::AcqRel);
            match remote_signal(&self.connection, &self.signal_channel, &process.remote_marker, signal).await {
                Ok(()) => Ok(()),
                Err(cause) => {
                    process.last_signal.store(previous, Ordering::Release);
                    Err(cause)
                }
            }
        })
    }

    fn release<'a>(&'a self, proc: &'a ProcId) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async move {
            let process =
                self.processes.lock().await.get(proc.as_str()).cloned().ok_or_else(|| error(ErrorCode::NotFound, "process not found"))?;
            let _control = process.control.lock().await;
            if process.released.load(Ordering::Acquire) {
                return Err(error(ErrorCode::NotFound, "process already released"));
            }
            if process.child.lock().await.try_wait().is_ok_and(|status| status.is_none()) {
                let previous = process.last_signal.swap(9, Ordering::AcqRel);
                match remote_signal(&self.connection, &self.signal_channel, &process.remote_marker, Signal::Kill).await {
                    Ok(()) | Err(ProtoError { code: ErrorCode::NotFound, .. }) => {}
                    Err(cause) => {
                        process.last_signal.store(previous, Ordering::Release);
                        return Err(cause);
                    }
                }
            }
            self.run(&format!("rm -f -- {}", quote(&process.remote_marker)), &[]).await?;
            process.released.store(true, Ordering::Release);
            self.processes.lock().await.remove(proc.as_str());
            drop(process.child.lock().await.start_kill());
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
        output.push(stream, buffer.get(..count).unwrap_or(&[]).to_vec());
        process.notify.notify_waiters();
    }
    process.streams_open.fetch_sub(1, Ordering::Release);
    process.notify.notify_waiters();
}

fn signal_script(marker: &str, signal: Signal) -> String {
    let name = match signal {
        Signal::Interrupt => "INT",
        Signal::Terminate => "TERM",
        Signal::Kill => "KILL",
    };
    format!(
        "[ -f {0} ] || exit 44; read p marker_g < {0} || exit 44; case \"$p:$marker_g\" in *[!0-9:]*|:*) exit 44;; esac; {PGID_QUERY} [ \"$g\" = \"$marker_g\" ] || exit 44; if [ \"$g\" = \"$p\" ]; then kill -{name} -\"$g\"; else kill -{name} \"$p\"; fi",
        quote(marker)
    )
}

async fn remote_signal(connection: &Connection, channel: &Semaphore, marker: &str, signal: Signal) -> Outcome<()> {
    // The six long-running process channels and three general transient channels can occupy
    // nine of sshd's ten slots. This dedicated permit reserves the remaining slot for kills.
    let _permit = tokio::time::timeout(Duration::from_secs(5), channel.acquire())
        .await
        .map_err(|_| error(ErrorCode::Unavailable, "SSH signal channel capacity exhausted"))?
        .map_err(|_| error(ErrorCode::Unavailable, "SSH signal channel closed"))?;
    let script = signal_script(marker, signal);
    let (status, _) = connection.run_with_status(&script, &[], false).await?;
    match status {
        0 => Ok(()),
        44 => Err(error(ErrorCode::NotFound, "remote process already exited")),
        _ => Err(error(ErrorCode::Unavailable, "remote signal failed")),
    }
}

impl Search for AgentlessWorkspace {
    fn grep<'a>(&'a self, query: GrepQuery<'a>) -> BoxFuture<'a, Outcome<GrepResult>> {
        Box::pin(async move {
            let target = self.safe(query.path, false).await?;
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
                return self.grep_fallback(&query, &options, &target).await;
            }
            let mut args = format!("rg --json{options} -C {}", query.context);
            for glob in query.globs {
                let _ = write!(args, " -g {}", quote(glob));
            }
            let _ = write!(args, " -e {} -- {}", quote(query.pattern), quote(&target));
            let match_cap = query.max_matches.saturating_add(1).max(1);
            let args =
                format!("{args} | awk -v cap={match_cap} '{{ print; if ($0 ~ /\"type\":\"match\"/) {{ n++; if (n >= cap) exit }} }}'");
            let output = self.run_status(&args, &[]).await?;
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
            let target = self.safe(query.path, false).await?;
            let mut builder = GlobSetBuilder::new();
            for pattern in query.patterns {
                builder.add(Glob::new(pattern).map_err(|_| error(ErrorCode::InvalidParams, "invalid glob"))?);
            }
            let patterns = builder.build().map_err(|_| error(ErrorCode::InvalidParams, "invalid glob"))?;
            let output = if self.caps.native_search {
                self.run(&format!("cd {} && rg --files -0 --hidden", quote(&target)), &[]).await?
            } else {
                self.run(&format!("cd {} && find . -type f -print0", quote(&target)), &[]).await?
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
