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
use std::path::{Path, PathBuf};
use std::pin::Pin;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{DirEntry, EntryKind, FsList, FsListParams, FsReadMany, FsReadManyParams, ReadManyEntry};
use aim_proto::ids::WorkspaceId;
use aim_rpc::Peer;
use sha2::{Digest as _, Sha256};

/// A boxed, sendable future borrowing its source.
pub type FilesFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Most paths one `fs.read_many` names: with 64 KiB files that stays within aimx's 2 MiB reply
/// budget (`limit_exceeded` past it).
pub const READ_BATCH: usize = 32;

/// A file's text as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileText {
    /// The text (UTF-8), at most the requested bytes.
    pub text: String,
    /// `sha256:<hex>`: of the whole file for harness reads, of the bytes read for local ones.
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
    /// Lists `dir`, hidden entries included, at most `limit` entries, sorted by name. `Ok(None)`
    /// when the directory does not exist.
    ///
    /// # Errors
    /// Anything but absence (denied, not a directory, transport).
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>>;

    /// Reads `paths`, each up to `max_bytes`; one outcome per path, in order.
    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>>;

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

fn text_of(content: Content, path: &str) -> Result<String, String> {
    match content {
        Content::Utf8 { text } => Ok(text),
        Content::Base64 { .. } => Err(format!("{path} is not UTF-8 text")),
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
                let params =
                    FsReadManyParams { workspace: self.workspace.clone(), paths: batch.to_vec(), max_bytes_per_file: Some(max_bytes) };
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

    fn display(&self, path: &str) -> String {
        path.to_owned()
    }
}

fn read_of(entry: ReadManyEntry) -> Read {
    match entry {
        ReadManyEntry::Ok { path, read } => match text_of(read.content, &path) {
            Ok(text) => Read::Ok(FileText { text, hash: read.hash.0, size: read.size, truncated: read.truncated }),
            Err(message) => Read::Failed(message),
        },
        ReadManyEntry::Error { code, .. } if code == ErrorCode::NotFound.name() => Read::Missing,
        ReadManyEntry::Error { code, message, .. } => Read::Failed(format!("{code}: {message}")),
    }
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
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
        let kind = match entry.file_type() {
            Ok(t) if t.is_symlink() => EntryKind::Symlink,
            Ok(t) if t.is_dir() => EntryKind::Dir,
            Ok(t) if t.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let size = if kind == EntryKind::File { entry.metadata().map_or(0, |m| m.len()) } else { 0 };
        entries.push(DirEntry { name: entry.file_name().to_string_lossy().into_owned(), kind, size });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(Some(entries))
}

fn read_local(path: &Path, max_bytes: u64) -> Read {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(err) => return Read::Failed(format!("{}: {err}", path.display())),
    };
    let size = match file.metadata() {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => return Read::Failed(format!("{} is not a file", path.display())),
        Err(err) => return Read::Failed(format!("{}: {err}", path.display())),
    };
    let mut bytes = Vec::new();
    if let Err(err) = file.take(max_bytes).read_to_end(&mut bytes) {
        return Read::Failed(format!("{}: {err}", path.display()));
    }
    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) < size;
    let hash = sha256(&bytes);
    // A cut may split a character: keep the valid prefix.
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) if truncated => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        Err(_) => return Read::Failed(format!("{} is not UTF-8 text", path.display())),
    };
    Read::Ok(FileText { text, hash, size, truncated })
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
}

impl MemoryFiles {
    /// Files at relative paths (`a/b.md`), with their text.
    #[must_use]
    pub fn new<P: Into<String>, T: Into<String>>(files: impl IntoIterator<Item = (P, T)>) -> Self {
        Self { files: files.into_iter().map(|(p, t)| (p.into(), t.into())).collect() }
    }
}

impl Files for MemoryFiles {
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{}/", dir.trim_end_matches('/')) };
        let mut entries: Vec<DirEntry> = Vec::new();
        for (path, text) in &self.files {
            let Some(rest) = path.strip_prefix(&prefix) else { continue };
            let (name, kind, size) = match rest.split_once('/') {
                Some((name, _)) => (name, EntryKind::Dir, 0),
                None => (rest, EntryKind::File, u64::try_from(text.len()).unwrap_or(u64::MAX)),
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

    fn display(&self, path: &str) -> String {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_aimx() {
        assert_eq!(sha256(b""), "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
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
