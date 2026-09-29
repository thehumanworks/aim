//! Where resource files are read from.
//!
//! [`Files`] is all discovery needs: list a directory, read a batch of files. Two sources
//! implement it:
//!
//! - [`HarnessFiles`] — the **project**, through the session's harness (`fs.list`,
//!   `fs.read_many`), so under `--ssh` the remote project's files apply and the local disk is
//!   never consulted (ADR 0014, docs/architecture.md §6.8);
//! - [`LocalFiles`] — the **user's** resources under `~/.aim`, read on this machine.
//!
//! Hidden entries are listed (`.agents/`, `.claude/` are hidden) and `.gitignore` is not
//! consulted: projects commonly ignore `.claude/`, and ignored is not absent.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    ByteRange, DirEntry, EntryKind, FsList, FsListParams, FsRead, FsReadMany, FsReadManyParams, FsReadParams, ReadManyEntry,
};
use aim_proto::ids::WorkspaceId;
use aim_rpc::Peer;
use sha2::{Digest as _, Sha256};

/// A boxed, sendable future borrowing its source.
pub type FilesFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Most paths one `fs.read_many` names: with 64 KiB files that stays within aimx's 2 MiB reply
/// budget (`limit_exceeded` past it).
pub const READ_BATCH: usize = 32;
/// Largest binary project resource accepted by the plugin loader. The default aimx read cap is
/// also 2 MiB, so a single RPC can return the whole component or fail closed.
pub const MAX_BINARY_BYTES: u64 = 2 * 1024 * 1024;

/// A file's text as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileText {
    /// The text (UTF-8), at most the requested bytes.
    pub text: String,
    /// `sha256:<hex>`: of the whole file when supplied, otherwise of the bytes read.
    pub hash: String,
    /// The file's full size in bytes.
    pub size: u64,
    /// Fewer bytes than the file holds were read.
    pub truncated: bool,
}

/// The outcome of reading one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// The file was read.
    Ok(FileText),
    /// There is no such file.
    Missing,
    /// It exists but could not be read (denied, not UTF-8, over a budget, …).
    Failed(String),
}

/// A source of resource files: a directory tree addressed by relative paths.
pub trait Files: Send + Sync {
    /// Lists `dir`, hidden entries included, at most `limit` entries, sorted by name. Which
    /// entries a directory holding more than `limit` yields is up to the source: enumeration stops
    /// at the limit. `Ok(None)` when the directory does not exist.
    ///
    /// # Errors
    /// Anything but absence (denied, not a directory, transport).
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>>;

    /// Reads `paths`, each up to `max_bytes`; one outcome per path, in order.
    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>>;

    /// Reads one complete binary file, at most `max_bytes` (and at most [`MAX_BINARY_BYTES`]).
    /// A missing file returns `Ok(None)`; a truncated read is an error, never a valid component.
    /// Project implementations must route this through their workspace so SSH and remote roots
    /// retain the same authority and path confinement as text resources.
    fn read_binary<'a>(&'a self, _path: &'a str, _max_bytes: u64) -> FilesFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async { Err("binary reads are unavailable for this file source".to_owned()) })
    }

    /// How `path` is shown to people and models: workspace-relative for a project (what the
    /// workspace tools accept), absolute on this machine for the user's files.
    fn display(&self, path: &str) -> String;
}

/// The project's files, read through the session's harness.
#[derive(Clone)]
pub struct HarnessFiles {
    peer: Peer,
    workspace: WorkspaceId,
}

impl HarnessFiles {
    /// Reads workspace `workspace` over `peer`.
    #[must_use]
    pub const fn new(peer: Peer, workspace: WorkspaceId) -> Self {
        Self { peer, workspace }
    }
}

impl core::fmt::Debug for HarnessFiles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HarnessFiles").field("workspace", &self.workspace).finish_non_exhaustive()
    }
}

fn text_of(content: Content, truncated: bool, path: &str) -> Result<String, String> {
    match content {
        Content::Utf8 { text } => Ok(text),
        // A cut at the byte limit can split the last character, and aimx then sends the bytes as
        // base64 (REV8-10): keep the text before it.
        Content::Base64 { data } => utf8_prefix(data.0, truncated).ok_or_else(|| format!("{path} is not UTF-8 text")),
    }
}

/// `bytes` as text. When they were `truncated` and only their last character is incomplete, the
/// text before it; invalid UTF-8 anywhere else is not text.
fn utf8_prefix(bytes: Vec<u8>, truncated: bool) -> Option<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) if truncated && err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

impl Files for HarnessFiles {
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        Box::pin(async move {
            let params = FsListParams {
                workspace: self.workspace.clone(),
                path: dir.to_owned(),
                limit: Some(limit),
                page_token: None,
                include_hidden: true,
                scope: None,
            };
            match self.peer.call::<FsList>(params).await {
                Ok(listed) => Ok(Some(listed.entries)),
                Err(ProtoError { code: ErrorCode::NotFound, .. }) => Ok(None),
                Err(err) => Err(format!("{}: {}", err.code, err.message)),
            }
        })
    }

    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>> {
        Box::pin(async move {
            let batches = paths.chunks(READ_BATCH).map(|batch| {
                let params = FsReadManyParams {
                    workspace: self.workspace.clone(),
                    paths: batch.to_vec(),
                    max_bytes_per_file: Some(max_bytes),
                    prefix_only: true,
                    scope: None,
                };
                let count = batch.len();
                async move {
                    match self.peer.call::<FsReadMany>(params).await {
                        Ok(result) => result.entries.into_iter().map(read_of).collect::<Vec<_>>(),
                        Err(err) => vec![Read::Failed(format!("{}: {}", err.code, err.message)); count],
                    }
                }
            });
            futures_util::future::join_all(batches).await.into_iter().flatten().collect()
        })
    }

    fn read_binary<'a>(&'a self, path: &'a str, max_bytes: u64) -> FilesFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            check_binary_limit(max_bytes)?;
            let params = FsReadParams {
                workspace: self.workspace.clone(),
                path: path.to_owned(),
                range: Some(ByteRange { start: 0, len: max_bytes }),
                scope: None,
                hash: false,
            };
            match self.peer.call::<FsRead>(params).await {
                Ok(read) => complete_binary(read.content.into_bytes(), read.size, read.truncated, max_bytes).map(Some),
                Err(ProtoError { code: ErrorCode::NotFound, .. }) => Ok(None),
                Err(err) => Err(format!("{}: {}", err.code, err.message)),
            }
        })
    }

    fn display(&self, path: &str) -> String {
        path.to_owned()
    }
}

fn read_of(entry: ReadManyEntry) -> Read {
    match entry {
        ReadManyEntry::Ok { path, read } => match text_of(read.content.clone(), read.truncated, &path) {
            Ok(text) => Read::Ok(FileText {
                text,
                hash: read.hash.map_or_else(|| sha256(&read.content.into_bytes()), |hash| hash.0),
                size: read.size,
                truncated: read.truncated,
            }),
            Err(message) => Read::Failed(message),
        },
        ReadManyEntry::Error { code, .. } if code == ErrorCode::NotFound.name() => Read::Missing,
        ReadManyEntry::Error { code, message, .. } => Read::Failed(format!("{code}: {message}")),
    }
}

fn check_binary_limit(max_bytes: u64) -> Result<(), String> {
    if max_bytes == 0 || max_bytes > MAX_BINARY_BYTES {
        return Err(format!("binary read limit must be 1..={MAX_BINARY_BYTES} bytes"));
    }
    Ok(())
}

fn complete_binary(bytes: Vec<u8>, size: u64, truncated: bool, max_bytes: u64) -> Result<Vec<u8>, String> {
    if truncated || size > max_bytes || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != size {
        return Err("binary file exceeds read limit or was truncated".to_owned());
    }
    Ok(bytes)
}

/// Files under a local directory (the user's `~/.aim`).
#[derive(Clone, Debug)]
pub struct LocalFiles {
    root: PathBuf,
}

impl LocalFiles {
    /// Files under `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory read.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// `sha256:<hex>` of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

fn list_local(dir: &Path, limit: u32) -> Result<Option<Vec<DirEntry>>, String> {
    let reader = match std::fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", dir.display())),
    };
    let entries = reader.map(|entry| {
        let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
        let kind = match entry.file_type() {
            Ok(t) if t.is_symlink() => EntryKind::Symlink,
            Ok(t) if t.is_dir() => EntryKind::Dir,
            Ok(t) if t.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let size = if kind == EntryKind::File { entry.metadata().map_or(0, |m| m.len()) } else { 0 };
        Ok(DirEntry { name: entry.file_name().to_string_lossy().into_owned(), kind, size })
    });
    first_entries(entries, limit).map(Some)
}

/// The first `limit` of `entries` in the order they come (a directory's own order), sorted by
/// name. Enumeration stops at the limit, so a huge directory costs no more than `limit` entries
/// (REV8-8).
fn first_entries(entries: impl Iterator<Item = Result<DirEntry, String>>, limit: u32) -> Result<Vec<DirEntry>, String> {
    let mut first = entries.take(usize::try_from(limit).unwrap_or(usize::MAX)).collect::<Result<Vec<_>, _>>()?;
    first.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(first)
}

/// Reads at most `max_bytes` of the regular file at `path`. Anything else (a FIFO, a socket, a
/// device) is refused without blocking: it is checked before opening, opened non-blocking, and
/// checked again on the open descriptor (REV8-8).
fn read_local(path: &Path, max_bytes: u64) -> Read {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Read::Failed(format!("{} is not a regular file", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(err) => return Read::Failed(format!("{}: {err}", path.display())),
    }
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(nix::libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(err) => return Read::Failed(format!("{}: {err}", path.display())),
    };
    let size = match file.metadata() {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => return Read::Failed(format!("{} is not a regular file", path.display())),
        Err(err) => return Read::Failed(format!("{}: {err}", path.display())),
    };
    let mut bytes = Vec::new();
    if let Err(err) = file.take(max_bytes).read_to_end(&mut bytes) {
        return Read::Failed(format!("{}: {err}", path.display()));
    }
    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) < size;
    let hash = sha256(&bytes);
    match utf8_prefix(bytes, truncated) {
        Some(text) => Read::Ok(FileText { text, hash, size, truncated }),
        None => Read::Failed(format!("{} is not UTF-8 text", path.display())),
    }
}

impl Files for LocalFiles {
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        let dir = self.root.join(dir);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || list_local(&dir, limit)).await.map_err(|err| format!("listing failed: {err}"))?
        })
    }

    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>> {
        let root = self.root.clone();
        Box::pin(async move {
            let count = paths.len();
            tokio::task::spawn_blocking(move || paths.iter().map(|p| read_local(&root.join(p), max_bytes)).collect())
                .await
                .unwrap_or_else(|err| vec![Read::Failed(format!("reading failed: {err}")); count])
        })
    }

    fn display(&self, path: &str) -> String {
        self.root.join(path).to_string_lossy().into_owned()
    }
}

/// Files held in memory: a project for tests and scripted sessions.
#[derive(Clone, Debug, Default)]
pub struct MemoryFiles {
    files: BTreeMap<String, String>,
    binary_files: BTreeMap<String, Vec<u8>>,
}

impl MemoryFiles {
    /// Files at relative paths (`a/b.md`), with their text.
    #[must_use]
    pub fn new<P: Into<String>, T: Into<String>>(files: impl IntoIterator<Item = (P, T)>) -> Self {
        Self { files: files.into_iter().map(|(p, t)| (p.into(), t.into())).collect(), binary_files: BTreeMap::new() }
    }

    /// Adds a binary file to a fixture, preserving bytes that are not UTF-8.
    #[must_use]
    pub fn with_binary(mut self, path: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.binary_files.insert(path.into(), bytes);
        self
    }
}

impl Files for MemoryFiles {
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{}/", dir.trim_end_matches('/')) };
        let mut entries: Vec<DirEntry> = Vec::new();
        for (path, size) in self
            .files
            .iter()
            .map(|(path, text)| (path, text.len()))
            .chain(self.binary_files.iter().map(|(path, bytes)| (path, bytes.len())))
        {
            let Some(rest) = path.strip_prefix(&prefix) else { continue };
            let (name, kind, size) = match rest.split_once('/') {
                Some((name, _)) => (name, EntryKind::Dir, 0),
                None => (rest, EntryKind::File, u64::try_from(size).unwrap_or(u64::MAX)),
            };
            if !entries.iter().any(|e| e.name == name) {
                entries.push(DirEntry { name: name.to_owned(), kind, size });
            }
        }
        entries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let listed = if entries.is_empty() { None } else { Some(entries) };
        Box::pin(async move { Ok(listed) })
    }

    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>> {
        let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let reads = paths
            .iter()
            .map(|path| match self.files.get(path) {
                None => Read::Missing,
                Some(text) => {
                    let (kept, truncated) = super::clip(text, max);
                    let size = u64::try_from(text.len()).unwrap_or(u64::MAX);
                    Read::Ok(FileText { text: kept.to_owned(), hash: sha256(text.as_bytes()), size, truncated })
                }
            })
            .collect();
        Box::pin(async move { reads })
    }

    fn read_binary<'a>(&'a self, path: &'a str, max_bytes: u64) -> FilesFuture<'a, Result<Option<Vec<u8>>, String>> {
        let result = check_binary_limit(max_bytes).and_then(|()| {
            self.binary_files
                .get(path)
                .cloned()
                .or_else(|| self.files.get(path).map(|text| text.as_bytes().to_vec()))
                .map(|bytes| {
                    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                    complete_binary(bytes, size, false, max_bytes)
                })
                .transpose()
        });
        Box::pin(async move { result })
    }

    fn display(&self, path: &str) -> String {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aim_proto::harness::FsReadResult;

    use super::*;

    #[test]
    fn hashes_like_aimx() {
        assert_eq!(sha256(b""), "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[tokio::test]
    async fn binary_reads_are_complete_bounded_and_preserve_non_utf8() {
        let files = MemoryFiles::default().with_binary(".agents/plugins/example.wasm", vec![0, 0xff, 1]);
        let entries = files.list(".agents/plugins", 8).await.unwrap().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(files.read_binary(".agents/plugins/example.wasm", 3).await.unwrap(), Some(vec![0, 0xff, 1]));
        assert_eq!(files.read_binary(".agents/plugins/missing.wasm", 3).await.unwrap(), None);
        assert!(files.read_binary(".agents/plugins/example.wasm", 2).await.is_err());
        assert!(files.read_binary(".agents/plugins/example.wasm", MAX_BINARY_BYTES + 1).await.is_err());
        assert!(complete_binary(vec![1, 2], 3, true, 3).is_err());
    }

    fn entry(read: FsReadResult) -> ReadManyEntry {
        ReadManyEntry::Ok { path: "AGENTS.md".into(), read }
    }

    fn bytes(data: &[u8], truncated: bool) -> FsReadResult {
        FsReadResult {
            content: Content::from_bytes(data.to_vec()),
            size: 70_000,
            hash: Some(aim_proto::harness::ContentHash("sha256:x".into())),
            truncated,
        }
    }

    #[test]
    fn harness_reads_keep_the_text_before_a_split_character() {
        // aimx cut `é` (C3 A9) after its first byte, so it sent the bytes as base64 (REV8-10).
        let Read::Ok(file) = read_of(entry(bytes(b"abc\xC3", true))) else { panic!("a valid prefix") };
        assert_eq!((file.text.as_str(), file.truncated), ("abc", true));
        // Invalid UTF-8 elsewhere, or in a whole file, is not text.
        assert!(matches!(read_of(entry(bytes(b"a\xFFb\xC3", true))), Read::Failed(m) if m.contains("not UTF-8")));
        assert!(matches!(read_of(entry(bytes(b"abc\xC3", false))), Read::Failed(_)));
        let mut prefix = bytes(b"abc", true);
        prefix.hash = None;
        let Read::Ok(prefix) = read_of(entry(prefix)) else { panic!("prefix read") };
        assert_eq!(prefix.hash, sha256(b"abc"));
    }

    #[test]
    fn listing_stops_at_its_limit() {
        // An endless directory: enumeration must stop at the limit rather than collect it all.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut n = 0_u64;
            let endless = std::iter::from_fn(|| {
                n = n.wrapping_add(1);
                Some(Ok(DirEntry { name: format!("e{:06}", u64::MAX - n), kind: EntryKind::File, size: 0 }))
            });
            let _sent = tx.send(first_entries(endless, 3));
        });
        let listed = rx.recv_timeout(Duration::from_secs(5)).expect("bounded").unwrap();
        assert_eq!(listed.len(), 3);
        assert!(listed.windows(2).all(|w| w[0].name < w[1].name), "sorted");
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("MEMORY.md");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let files = LocalFiles::new(dir.path());
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let reads = runtime.block_on(files.read_many(vec!["MEMORY.md".into()], 1024));
            // Dropping the runtime waits for its blocking reads: it must not hang either.
            drop(runtime);
            let _sent = tx.send(reads);
        });
        let outcome = rx.recv_timeout(Duration::from_secs(3));
        if outcome.is_err() {
            // Release the blocked reader so the test process can exit, then fail.
            drop(std::fs::OpenOptions::new().write(true).open(&fifo));
        }
        let reads = outcome.expect("a FIFO never blocks a read (REV8-8)");
        reader.join().unwrap();
        assert!(matches!(reads.as_slice(), [Read::Failed(message)] if message.contains("not a regular file")), "{reads:?}");
    }

    #[tokio::test]
    async fn local_reads_are_bounded_and_keep_whole_characters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "héllo").unwrap();
        let files = LocalFiles::new(dir.path());
        let reads = files.read_many(vec!["a.md".into(), "missing.md".into()], 2).await;
        assert_eq!(reads[1], Read::Missing);
        let Read::Ok(first) = &reads[0] else { panic!("{reads:?}") };
        assert_eq!((first.text.as_str(), first.size, first.truncated), ("h", 6, true));
        assert_eq!(files.list("nowhere", 10).await, Ok(None));
        let listed = files.list("", 10).await.unwrap().unwrap();
        assert_eq!(listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.md"]);
    }
}
