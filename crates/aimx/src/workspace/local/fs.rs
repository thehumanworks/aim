//! Filesystem operations of the local backend.
//!
//! Writes and edits replace files atomically: the new bytes go to a temporary file in the same
//! directory (created with the umask's default mode, or the replaced file's mode), which is
//! `fsync`ed and then renamed over the target; `IfAbsent` and non-overwriting renames use an
//! exclusive rename (`renameat2(RENAME_NOREPLACE)` / `renamex_np(RENAME_EXCL)`), so they are
//! race-free. Mutations within one workspace are serialised so a read-modify-write (`edit`,
//! `IfHash`) cannot lose a concurrent update made through aimx.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    ByteRange, ContentHash, DirEntry, EditOutcome, EntryKind, ExactEdit, FsListResult, FsReadResult, Meta, Precondition, WriteOutcome,
};
use aim_proto::ids::IdempotencyKey;
use sha2::{Digest as _, Sha256};

use super::{Base, Follow, blocking, io_error};
use crate::edit::apply_edits;
use crate::id::{hex, random_hex};
use crate::workspace::{BoxFuture, EditRequest, Fs, ListRequest, Outcome, WriteRequest};

const BLOCK: usize = 64 * 1024;

/// The local filesystem, confined to a root.
#[derive(Debug)]
pub(super) struct LocalFs {
    base: Arc<Base>,
    mutations: Arc<Mutex<()>>,
}

impl LocalFs {
    pub(super) fn new(base: Arc<Base>) -> Self {
        Self { base, mutations: Arc::new(Mutex::new(())) }
    }
}

fn content_hash(hasher: Sha256) -> ContentHash {
    ContentHash(format!("sha256:{}", hex(&hasher.finalize())))
}

/// The `sha256:<hex>` hash of `bytes`.
pub(super) fn hash_bytes(bytes: &[u8]) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    content_hash(hasher)
}

fn hash_file(path: &Path, display: &str) -> Outcome<ContentHash> {
    let mut file = File::open(path).map_err(|err| io_error(&err, display))?;
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; BLOCK];
    loop {
        let n = file.read(&mut block).map_err(|err| io_error(&err, display))?;
        if n == 0 {
            break;
        }
        hasher.update(block.get(..n).unwrap_or_default());
    }
    Ok(content_hash(hasher))
}

fn entry_kind(file_type: fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn stat(base: &Base, path: &str, hash: bool) -> Outcome<Meta> {
    let real = base.resolve(path, Follow::NoFinal)?;
    let meta = fs::symlink_metadata(&real).map_err(|err| io_error(&err, path))?;
    let kind = entry_kind(meta.file_type());
    let mtime_ms = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).and_then(|d| i64::try_from(d.as_millis()).ok());
    let hash = if hash && kind == EntryKind::File { Some(hash_file(&real, path)?) } else { None };
    Ok(Meta { kind, size: meta.len(), mtime_ms, hash })
}

fn read(base: &Base, path: &str, range: Option<ByteRange>, max_bytes: u64) -> Outcome<FsReadResult> {
    let real = base.resolve(path, Follow::Final)?;
    let mut file = File::open(&real).map_err(|err| io_error(&err, path))?;
    let meta = file.metadata().map_err(|err| io_error(&err, path))?;
    if meta.is_dir() {
        return Err(ProtoError::new(ErrorCode::Conflict, format!("`{path}` is a directory")));
    }
    let (start, wanted) = range.map_or((0, u64::MAX), |r| (r.start, r.len));
    let end = start.saturating_add(wanted.min(max_bytes));
    let mut out = Vec::new();
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; BLOCK];
    let mut pos = 0u64;
    loop {
        let n = file.read(&mut block).map_err(|err| io_error(&err, path))?;
        if n == 0 {
            break;
        }
        let chunk = block.get(..n).unwrap_or_default();
        hasher.update(chunk);
        let chunk_end = pos.saturating_add(n as u64);
        let lo = pos.max(start);
        let hi = chunk_end.min(end);
        if lo < hi {
            let from = usize::try_from(lo - pos).unwrap_or(0);
            let to = usize::try_from(hi - pos).unwrap_or(n);
            out.extend_from_slice(chunk.get(from..to).unwrap_or_default());
        }
        pos = chunk_end;
    }
    let size = pos;
    let requested = wanted.min(size.saturating_sub(start));
    let truncated = (out.len() as u64) < requested;
    Ok(FsReadResult { content: Content::from_bytes(out), size, hash: content_hash(hasher), truncated })
}

fn check_precondition(real: &Path, exists: bool, precondition: &Precondition, display: &str) -> Outcome<()> {
    match precondition {
        Precondition::IfAbsent if exists => Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` already exists"))),
        Precondition::Any | Precondition::IfAbsent => Ok(()),
        Precondition::IfHash { .. } if !exists => {
            Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` does not exist")))
        }
        Precondition::IfHash { hash } => {
            let current = hash_file(real, display)?;
            if current == *hash {
                Ok(())
            } else {
                Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` changed since it was read"))
                    .with_detail(serde_json::json!({ "current": current.0 })))
            }
        }
    }
}

/// Renames `from` to `to` unless `to` exists (atomically).
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(rustix::fs::CWD, from, rustix::fs::CWD, to, rustix::fs::RenameFlags::NOREPLACE).map_err(io::Error::from)
}

fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir)
        && let Err(err) = handle.sync_all()
    {
        tracing::debug!(%err, dir = %dir.display(), "directory fsync failed");
    }
}

/// Replaces (or, when `exclusive`, creates) `target` with `bytes` atomically.
fn atomic_replace(target: &Path, bytes: &[u8], mode: Option<Permissions>, exclusive: bool, display: &str) -> Outcome<()> {
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(ProtoError::new(ErrorCode::InvalidParams, format!("`{display}` is not a file path")));
    };
    let tmp = dir.join(format!(".{}.aimx-{}.tmp", name.to_string_lossy(), random_hex()));
    let written = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o666).open(&tmp)?;
        if let Some(mode) = mode {
            file.set_permissions(mode)?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if exclusive { rename_noreplace(&tmp, target) } else { fs::rename(&tmp, target) }
    })();
    if let Err(err) = written {
        if let Err(cleanup) = fs::remove_file(&tmp)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%cleanup, tmp = %tmp.display(), "could not remove a temporary file");
        }
        if exclusive && err.kind() == io::ErrorKind::AlreadyExists {
            return Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` already exists")));
        }
        return Err(io_error(&err, display));
    }
    sync_dir(dir);
    Ok(())
}

fn existing(real: &Path, display: &str) -> Outcome<Option<fs::Metadata>> {
    match fs::metadata(real) {
        Ok(meta) if meta.is_dir() => Err(ProtoError::new(ErrorCode::Conflict, format!("`{display}` is a directory"))),
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error(&err, display)),
    }
}

fn lock(mutex: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn write(
    base: &Base,
    mutations: &Mutex<()>,
    path: &str,
    bytes: &[u8],
    precondition: &Precondition,
    create_dirs: bool,
) -> Outcome<WriteOutcome> {
    let real = base.resolve(path, Follow::Final)?;
    base.check_protected(&real, false)?;
    let _serial = lock(mutations);
    if create_dirs && let Some(parent) = real.parent() {
        fs::create_dir_all(parent).map_err(|err| io_error(&err, path))?;
    }
    let current = existing(&real, path)?;
    check_precondition(&real, current.is_some(), precondition, path)?;
    let exclusive = matches!(precondition, Precondition::IfAbsent);
    atomic_replace(&real, bytes, current.as_ref().map(fs::Metadata::permissions), exclusive, path)?;
    Ok(WriteOutcome { hash: hash_bytes(bytes), size: bytes.len() as u64, created: current.is_none() })
}

fn edit(base: &Base, mutations: &Mutex<()>, path: &str, edits: &[ExactEdit], precondition: &Precondition) -> Outcome<EditOutcome> {
    let real = base.resolve(path, Follow::Final)?;
    base.check_protected(&real, false)?;
    let _serial = lock(mutations);
    let Some(meta) = existing(&real, path)? else {
        return Err(ProtoError::new(ErrorCode::NotFound, format!("`{path}` does not exist")));
    };
    let before = fs::read(&real).map_err(|err| io_error(&err, path))?;
    match precondition {
        Precondition::Any => {}
        Precondition::IfAbsent => return Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{path}` already exists"))),
        Precondition::IfHash { hash } => {
            let current = hash_bytes(&before);
            if current != *hash {
                return Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{path}` changed since it was read"))
                    .with_detail(serde_json::json!({ "current": current.0 })));
            }
        }
    }
    let applied = apply_edits(&before, edits)?;
    if applied.content != before {
        atomic_replace(&real, &applied.content, Some(meta.permissions()), false, path)?;
    }
    Ok(EditOutcome {
        write: WriteOutcome { hash: hash_bytes(&applied.content), size: applied.content.len() as u64, created: false },
        replacements: applied.replacements,
    })
}

fn list(base: &Base, path: &str, limit: u32, page_token: Option<&str>, include_hidden: bool) -> Outcome<FsListResult> {
    let real = base.resolve(path, Follow::Final)?;
    let reader = fs::read_dir(&real).map_err(|err| io_error(&err, path))?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|err| io_error(&err, path))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        if page_token.is_some_and(|token| name.as_str() <= token) {
            continue;
        }
        let file_type = entry.file_type().map_err(|err| io_error(&err, path))?;
        let kind = entry_kind(file_type);
        let size = if kind == EntryKind::File { entry.metadata().map_or(0, |m| m.len()) } else { 0 };
        entries.push(DirEntry { name, kind, size });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let limit = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    let next_page = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(|e| e.name.clone())
    } else {
        None
    };
    Ok(FsListResult { entries, next_page })
}

fn mkdir(base: &Base, mutations: &Mutex<()>, path: &str) -> Outcome<()> {
    let real = base.resolve(path, Follow::Final)?;
    base.check_protected(&real, false)?;
    let _serial = lock(mutations);
    match fs::create_dir_all(&real) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists || err.kind() == io::ErrorKind::NotADirectory => {
            Err(ProtoError::new(ErrorCode::Conflict, format!("`{path}` exists and is not a directory")))
        }
        Err(err) => Err(io_error(&err, path)),
    }
}

fn remove(base: &Base, mutations: &Mutex<()>, path: &str, recursive: bool) -> Outcome<()> {
    let real = base.resolve(path, Follow::NoFinal)?;
    if real == base.root {
        return Err(ProtoError::new(ErrorCode::Denied, "the workspace root cannot be removed"));
    }
    base.check_protected(&real, true)?;
    let _serial = lock(mutations);
    let meta = fs::symlink_metadata(&real).map_err(|err| io_error(&err, path))?;
    let removed =
        if meta.is_dir() { if recursive { fs::remove_dir_all(&real) } else { fs::remove_dir(&real) } } else { fs::remove_file(&real) };
    match removed {
        Ok(()) => {
            if let Some(parent) = real.parent() {
                sync_dir(parent);
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {
            Err(ProtoError::new(ErrorCode::Conflict, format!("`{path}` is a non-empty directory; pass recursive")))
        }
        Err(err) => Err(io_error(&err, path)),
    }
}

fn rename(base: &Base, mutations: &Mutex<()>, from: &str, to: &str, overwrite: bool) -> Outcome<()> {
    let real_from = base.resolve(from, Follow::NoFinal)?;
    let real_to = base.resolve(to, Follow::NoFinal)?;
    if real_from == base.root || real_to == base.root {
        return Err(ProtoError::new(ErrorCode::Denied, "the workspace root cannot be moved or replaced"));
    }
    base.check_protected(&real_from, true)?;
    base.check_protected(&real_to, true)?;
    let _serial = lock(mutations);
    fs::symlink_metadata(&real_from).map_err(|err| io_error(&err, from))?;
    let moved = if overwrite { fs::rename(&real_from, &real_to) } else { rename_noreplace(&real_from, &real_to) };
    match moved {
        Ok(()) => {
            for dir in [real_from.parent(), real_to.parent()].into_iter().flatten() {
                sync_dir(dir);
            }
            Ok(())
        }
        Err(err) if !overwrite && err.kind() == io::ErrorKind::AlreadyExists => {
            Err(ProtoError::new(ErrorCode::Conflict, format!("`{to}` already exists; pass overwrite")))
        }
        Err(err) => Err(io_error(&err, to)),
    }
}

impl Fs for LocalFs {
    fn stat<'a>(&'a self, path: &'a str, hash: bool) -> BoxFuture<'a, Outcome<Meta>> {
        let base = Arc::clone(&self.base);
        let path = path.to_owned();
        Box::pin(blocking(move || stat(&base, &path, hash)))
    }

    fn read<'a>(&'a self, path: &'a str, range: Option<ByteRange>, max_bytes: u64) -> BoxFuture<'a, Outcome<FsReadResult>> {
        let base = Arc::clone(&self.base);
        let path = path.to_owned();
        Box::pin(blocking(move || read(&base, &path, range, max_bytes)))
    }

    fn write<'a>(&'a self, req: WriteRequest<'a>) -> BoxFuture<'a, Outcome<WriteOutcome>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let path = req.path.to_owned();
        let bytes = req.content.clone().into_bytes();
        let precondition = req.precondition.clone();
        let create_dirs = req.create_dirs;
        Box::pin(blocking(move || write(&base, &mutations, &path, &bytes, &precondition, create_dirs)))
    }

    fn edit<'a>(&'a self, req: EditRequest<'a>) -> BoxFuture<'a, Outcome<EditOutcome>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let path = req.path.to_owned();
        let edits = req.edits.to_vec();
        let precondition = req.precondition.clone();
        Box::pin(blocking(move || edit(&base, &mutations, &path, &edits, &precondition)))
    }

    fn list<'a>(&'a self, req: ListRequest<'a>) -> BoxFuture<'a, Outcome<FsListResult>> {
        let base = Arc::clone(&self.base);
        let path = req.path.to_owned();
        let token = req.page_token.map(str::to_owned);
        let (limit, hidden) = (req.limit, req.include_hidden);
        Box::pin(blocking(move || list(&base, &path, limit, token.as_deref(), hidden)))
    }

    fn mkdir<'a>(&'a self, path: &'a str, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let path = path.to_owned();
        Box::pin(blocking(move || mkdir(&base, &mutations, &path)))
    }

    fn remove<'a>(&'a self, path: &'a str, recursive: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let path = path.to_owned();
        Box::pin(blocking(move || remove(&base, &mutations, &path, recursive)))
    }

    fn rename<'a>(&'a self, from: &'a str, to: &'a str, overwrite: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let (from, to) = (from.to_owned(), to.to_owned());
        Box::pin(blocking(move || rename(&base, &mutations, &from, &to, overwrite)))
    }
}
