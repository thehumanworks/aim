//! Filesystem operations of the local backend.
//!
//! Every operation acts on the descriptors a [`walk`](super::walk) of its path returned: files are
//! opened, created, renamed and removed with `*at` calls relative to a held directory, never by
//! path, so a directory swapped for a symlink after the walk cannot redirect them (REV4-A
//! finding 3). Final entries are opened with `O_NOFOLLOW` (and `O_NONBLOCK`, so a FIFO cannot
//! block a reader).
//!
//! Writes and edits replace files atomically: the new bytes go to a temporary file created in the
//! same directory (with the umask's default mode, or the replaced file's mode), which is `fsync`ed
//! and then renamed over the target; `IfAbsent` and non-overwriting renames use an exclusive
//! rename (`renameat2(RENAME_NOREPLACE)` / `renamex_np(RENAME_EXCL)`), so they are race-free.
//! Mutations within one workspace are serialised so a read-modify-write (`edit`, `IfHash`) cannot
//! lose a concurrent update made through aimx.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read as _, SeekFrom, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    ByteRange, ContentHash, DirEntry, EditOutcome, EntryKind, ExactEdit, FsListResult, FsReadResult, Meta, Precondition, WriteOutcome,
};
use aim_proto::ids::IdempotencyKey;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags, Stat};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use super::walk::{Follow, Loc, kind, open_dir, open_entry, stat_entry};
use super::{Authority, Base, blocking, io_error};
use crate::authz::Access;
use crate::edit::apply_edits;
use crate::id::{hex, random_hex};
use crate::page::Page;
use crate::workspace::{BoxFuture, CopyRequest, EditRequest, Fs, ListRequest, Outcome, WriteRequest};

const BLOCK: usize = 64 * 1024;
/// Deepest directory tree `copy` and a recursive `remove` descend into (every level holds a
/// descriptor open).
const MAX_TREE_DEPTH: usize = 128;
/// A new file's mode before the umask (0o666).
const NEW_FILE: Mode = Mode::RUSR.union(Mode::WUSR).union(Mode::RGRP).union(Mode::WGRP).union(Mode::ROTH).union(Mode::WOTH);
/// A new directory's mode before the umask (0o777).
const NEW_DIR: Mode = Mode::RWXU.union(Mode::RWXG).union(Mode::RWXO);

/// The local filesystem, confined to a root.
#[derive(Debug)]
pub(super) struct LocalFs {
    pub(super) base: Arc<Base>,
    mutations: Arc<Mutex<()>>,
}

impl LocalFs {
    pub(super) fn new(base: Arc<Base>) -> Self {
        Self { base, mutations: Arc::new(Mutex::new(())) }
    }

    pub(super) fn scoped(&self, base: Arc<Base>) -> Self {
        Self { base, mutations: Arc::clone(&self.mutations) }
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

fn hash_file(file: &mut File) -> io::Result<ContentHash> {
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; BLOCK];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hasher.update(block.get(..n).unwrap_or_default());
    }
    Ok(content_hash(hasher))
}

fn bounded_bytes(file: &mut (impl io::Read + io::Seek), start: u64, count: u64) -> io::Result<Vec<u8>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut out = Vec::new();
    file.take(count).read_to_end(&mut out)?;
    Ok(out)
}

fn entry_kind(file_type: FileType) -> EntryKind {
    match file_type {
        FileType::Symlink => EntryKind::Symlink,
        FileType::Directory => EntryKind::Dir,
        FileType::RegularFile => EntryKind::File,
        _ => EntryKind::Other,
    }
}

/// A `Stat`'s size, in bytes.
fn size_of(stat: &Stat) -> u64 {
    u64::try_from(i128::from(stat.st_size)).unwrap_or(0)
}

/// A `Stat`'s modification time, in milliseconds since the Unix epoch.
fn mtime_ms(stat: &Stat) -> Option<i64> {
    let ms = i128::from(stat.st_mtime).checked_mul(1000)?.checked_add(i128::from(stat.st_mtime_nsec) / 1_000_000)?;
    i64::try_from(ms).ok()
}

/// A `Stat`'s permission bits.
fn mode_of(stat: &Stat) -> Mode {
    Mode::from_raw_mode(stat.st_mode)
}

fn lock(mutex: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_found(path: &str) -> ProtoError {
    ProtoError::new(ErrorCode::NotFound, format!("`{path}` does not exist"))
}

fn is_a_directory(path: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Conflict, format!("`{path}` is a directory"))
}

/// Maps an OS error on `path` to a protocol error.
fn os_error(err: Errno, path: &str) -> ProtoError {
    io_error(&io::Error::from(err), path)
}

fn errno_is(err: &io::Error, errno: Errno) -> bool {
    err.raw_os_error() == Some(errno.raw_os_error())
}

/// The directory holding the target and the target's name, when the target's directory exists.
fn entry<'a>(loc: &'a Loc, path: &str) -> Outcome<(&'a OwnedFd, &'a OsStr)> {
    match &loc.name {
        Some(name) if loc.missing.is_empty() => Ok((loc.dir().map_err(|err| io_error(&err, path))?, name.as_os_str())),
        _ => Err(not_found(path)),
    }
}

/// `lstat` of the target; `None` when it does not exist.
fn existing(dir: &OwnedFd, name: &OsStr, path: &str) -> Outcome<Option<Stat>> {
    match stat_entry(dir, name) {
        Ok(stat) => Ok(Some(stat)),
        Err(err) if errno_is(&err, Errno::NOENT) => Ok(None),
        Err(err) => Err(io_error(&err, path)),
    }
}

/// Opens an existing regular file for reading.
fn open_file(dir: &OwnedFd, name: &OsStr, path: &str) -> Outcome<File> {
    let file = open_entry(dir, name, OFlags::RDONLY, Mode::empty()).map_err(|err| io_error(&err, path))?;
    let meta = file.metadata().map_err(|err| io_error(&err, path))?;
    if meta.is_dir() {
        return Err(is_a_directory(path));
    }
    if !meta.is_file() {
        return Err(ProtoError::new(ErrorCode::Conflict, format!("`{path}` is not a regular file")));
    }
    Ok(file)
}

/// Creates the target's missing directories; returns the directory that holds the target.
fn make_dirs(loc: &Loc) -> io::Result<OwnedFd> {
    let mut current = loc.dir()?.try_clone()?;
    for name in &loc.missing {
        match rustix::fs::mkdirat(&current, name, NEW_DIR) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(err) => return Err(err.into()),
        }
        // `O_NOFOLLOW`: a symlink planted here in the meantime is refused, not followed.
        current = open_dir(&current, name)?;
    }
    Ok(current)
}

fn stat(base: &Base, path: &str, hash: bool) -> Outcome<Meta> {
    let loc = base.resolve(path, Follow::NoFinal, Authority::Path(Access::Read))?;
    let (stat, file) = if loc.name.is_some() {
        let (dir, name) = entry(&loc, path)?;
        let stat = existing(dir, name, path)?.ok_or_else(|| not_found(path))?;
        (stat, Some((dir, name)))
    } else {
        (rustix::fs::fstat(loc.dir().map_err(|err| io_error(&err, path))?).map_err(|err| os_error(err, path))?, None)
    };
    let kind = entry_kind(kind(&stat));
    let hash = match file {
        Some((dir, name)) if hash && kind == EntryKind::File => {
            Some(hash_file(&mut open_file(dir, name, path)?).map_err(|err| io_error(&err, path))?)
        }
        _ => None,
    };
    Ok(Meta { kind, size: size_of(&stat), mtime_ms: mtime_ms(&stat), hash })
}

fn read(base: &Base, path: &str, range: Option<ByteRange>, max_bytes: u64, hash: bool) -> Outcome<FsReadResult> {
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Read))?;
    if loc.target_dir().is_some() {
        return Err(is_a_directory(path));
    }
    let (dir, name) = entry(&loc, path)?;
    let mut file = open_file(dir, name, path)?;
    let (start, wanted) = range.map_or((0, u64::MAX), |r| (r.start, r.len));
    if !hash {
        let size = file.metadata().map_err(|err| io_error(&err, path))?.len();
        let count = wanted.min(max_bytes).min(size.saturating_sub(start));
        let out = bounded_bytes(&mut file, start, count).map_err(|err| io_error(&err, path))?;
        let requested = wanted.min(size.saturating_sub(start));
        let truncated = (out.len() as u64) < requested;
        return Ok(FsReadResult { content: Content::from_bytes(out), size, hash: None, truncated });
    }
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
    Ok(FsReadResult { content: Content::from_bytes(out), size, hash: Some(content_hash(hasher)), truncated })
}

fn check_precondition(dir: &OwnedFd, name: &OsStr, exists: bool, precondition: &Precondition, display: &str) -> Outcome<()> {
    match precondition {
        Precondition::IfAbsent if exists => Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` already exists"))),
        Precondition::Any | Precondition::IfAbsent => Ok(()),
        Precondition::IfHash { .. } if !exists => {
            Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` does not exist")))
        }
        Precondition::IfHash { hash } => {
            let current = hash_file(&mut open_file(dir, name, display)?).map_err(|err| io_error(&err, display))?;
            if current == *hash {
                Ok(())
            } else {
                Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` changed since it was read"))
                    .with_detail(serde_json::json!({ "current": current.0 })))
            }
        }
    }
}

fn sync_dir(dir: &OwnedFd) {
    if let Err(err) = rustix::fs::fsync(dir) {
        tracing::debug!(%err, "directory fsync failed");
    }
}

fn rename_at(from_dir: &OwnedFd, from: &OsStr, to_dir: &OwnedFd, to: &OsStr, overwrite: bool) -> io::Result<()> {
    let flags = if overwrite { RenameFlags::empty() } else { RenameFlags::NOREPLACE };
    rustix::fs::renameat_with(from_dir, from, to_dir, to, flags).map_err(Into::into)
}

fn staging_name(name: &OsStr) -> OsString {
    OsString::from(format!(".{}.aimx-{}.tmp", name.to_string_lossy(), random_hex()))
}

/// Replaces (or, when `exclusive`, creates) entry `name` of `dir` with `bytes` atomically.
fn atomic_replace(dir: &OwnedFd, name: &OsStr, bytes: &[u8], mode: Option<Mode>, exclusive: bool, display: &str) -> Outcome<()> {
    let tmp = staging_name(name);
    let written = (|| -> io::Result<()> {
        let mut file = open_entry(dir, &tmp, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, NEW_FILE)?;
        if let Some(mode) = mode {
            rustix::fs::fchmod(&file, mode)?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        rename_at(dir, &tmp, dir, name, !exclusive)
    })();
    if let Err(err) = written {
        if let Err(cleanup) = rustix::fs::unlinkat(dir, &tmp, AtFlags::empty())
            && cleanup != Errno::NOENT
        {
            tracing::warn!(%cleanup, tmp = %tmp.to_string_lossy(), "could not remove a temporary file");
        }
        if exclusive && err.kind() == io::ErrorKind::AlreadyExists {
            return Err(ProtoError::new(ErrorCode::PreconditionFailed, format!("`{display}` already exists")));
        }
        return Err(io_error(&err, display));
    }
    sync_dir(dir);
    Ok(())
}

fn write(
    base: &Base,
    mutations: &Mutex<()>,
    path: &str,
    bytes: &[u8],
    precondition: &Precondition,
    create_dirs: bool,
) -> Outcome<WriteOutcome> {
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Write))?;
    base.check_protected(&loc, false)?;
    let Some(name) = loc.name.as_deref().filter(|_| loc.opened.is_none()) else {
        return Err(is_a_directory(path));
    };
    let _serial = lock(mutations);
    let made;
    let dir = if loc.missing.is_empty() {
        loc.dir().map_err(|err| io_error(&err, path))?
    } else {
        check_precondition(loc.dir().map_err(|err| io_error(&err, path))?, name, false, precondition, path)?;
        if !create_dirs {
            return Err(ProtoError::new(ErrorCode::NotFound, format!("the directory of `{path}` does not exist")));
        }
        made = make_dirs(&loc).map_err(|err| io_error(&err, path))?;
        &made
    };
    let current = existing(dir, name, path)?;
    if current.as_ref().is_some_and(|stat| kind(stat) == FileType::Directory) {
        return Err(is_a_directory(path));
    }
    check_precondition(dir, name, current.is_some(), precondition, path)?;
    let exclusive = matches!(precondition, Precondition::IfAbsent);
    atomic_replace(dir, name, bytes, current.as_ref().map(mode_of), exclusive, path)?;
    Ok(WriteOutcome { hash: hash_bytes(bytes), size: bytes.len() as u64, created: current.is_none() })
}

fn edit(base: &Base, mutations: &Mutex<()>, path: &str, edits: &[ExactEdit], precondition: &Precondition) -> Outcome<EditOutcome> {
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Write))?;
    base.check_protected(&loc, false)?;
    if loc.target_dir().is_some() {
        return Err(is_a_directory(path));
    }
    let (dir, name) = entry(&loc, path)?;
    let _serial = lock(mutations);
    let Some(stat) = existing(dir, name, path)? else {
        return Err(not_found(path));
    };
    let mut before = Vec::new();
    open_file(dir, name, path)?.read_to_end(&mut before).map_err(|err| io_error(&err, path))?;
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
        atomic_replace(dir, name, &applied.content, Some(mode_of(&stat)), false, path)?;
    }
    Ok(EditOutcome {
        write: WriteOutcome { hash: hash_bytes(&applied.content), size: applied.content.len() as u64, created: false },
        replacements: applied.replacements,
    })
}

fn list(base: &Base, path: &str, limit: u32, page_token: Option<&str>, include_hidden: bool) -> Outcome<FsListResult> {
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Read))?;
    let Some(dir) = loc.target_dir() else {
        let (parent, name) = entry(&loc, path)?;
        return match existing(parent, name, path)? {
            Some(_) => Err(ProtoError::new(ErrorCode::Conflict, format!("`{path}` is not a directory"))),
            None => Err(not_found(path)),
        };
    };
    let reader = rustix::fs::Dir::read_from(dir).map_err(|err| os_error(err, path))?;
    // Only the page (plus one, to know whether more follow) is held, however large the directory.
    let mut page = Page::new(usize::try_from(limit).unwrap_or(usize::MAX), page_token);
    for entry in reader {
        let entry = entry.map_err(|err| os_error(err, path))?;
        let raw = OsStr::from_bytes(entry.file_name().to_bytes());
        if raw == "." || raw == ".." {
            continue;
        }
        let name = raw.to_string_lossy().into_owned();
        if (!include_hidden && name.starts_with('.')) || !page.wants(&name) {
            continue;
        }
        page.offer(name, (raw.to_owned(), entry.file_type()));
    }
    let selected = page.finish();
    let mut entries = Vec::with_capacity(selected.entries.len());
    for (name, (raw, file_type)) in selected.entries {
        let stat = stat_entry(dir, &raw).ok();
        let file_type = match (file_type, &stat) {
            (FileType::Unknown, Some(stat)) => kind(stat),
            (file_type, _) => file_type,
        };
        let kind = entry_kind(file_type);
        let size = if kind == EntryKind::File { stat.as_ref().map_or(0, size_of) } else { 0 };
        entries.push(DirEntry { name, kind, size });
    }
    Ok(FsListResult { entries, next_page: selected.next_page })
}

fn mkdir(base: &Base, mutations: &Mutex<()>, path: &str) -> Outcome<()> {
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Write))?;
    if loc.target_dir().is_some() {
        return Ok(());
    }
    base.check_protected(&loc, false)?;
    let Some(name) = loc.name.as_deref() else { return Ok(()) };
    let _serial = lock(mutations);
    let exists = || ProtoError::new(ErrorCode::Conflict, format!("`{path}` exists and is not a directory"));
    let dir = make_dirs(&loc).map_err(|err| if errno_is(&err, Errno::NOTDIR) { exists() } else { io_error(&err, path) })?;
    match rustix::fs::mkdirat(&dir, name, NEW_DIR) {
        Ok(()) => {
            sync_dir(&dir);
            Ok(())
        }
        Err(Errno::EXIST) => match existing(&dir, name, path)? {
            Some(stat) if kind(&stat) == FileType::Directory => Ok(()),
            _ => Err(exists()),
        },
        Err(err) => Err(os_error(err, path)),
    }
}

/// Removes directory `name` of `parent` and everything below it, through descriptors (a symlink
/// inside is removed, never followed).
fn remove_tree(parent: &OwnedFd, name: &OsStr, depth: usize) -> io::Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(io::Error::other("directory tree too deep to remove"));
    }
    let dir = open_dir(parent, name)?;
    // A directory changed while it was emptied is emptied again (a few passes at most).
    for _ in 0..3 {
        for entry in rustix::fs::Dir::read_from(&dir)? {
            let entry = entry?;
            let child = OsStr::from_bytes(entry.file_name().to_bytes());
            if child == "." || child == ".." {
                continue;
            }
            let file_type = match entry.file_type() {
                FileType::Unknown => kind(&stat_entry(&dir, child)?),
                file_type => file_type,
            };
            if file_type == FileType::Directory {
                remove_tree(&dir, child, depth + 1)?;
            } else {
                match rustix::fs::unlinkat(&dir, child, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(err) => return Err(err.into()),
                }
            }
        }
        match rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
            Err(Errno::NOTEMPTY) => {}
            other => return other.map_err(Into::into),
        }
    }
    Err(Errno::NOTEMPTY.into())
}

fn remove(base: &Base, mutations: &Mutex<()>, path: &str, recursive: bool) -> Outcome<()> {
    let loc = base.resolve(path, Follow::NoFinal, Authority::Path(Access::Tree))?;
    if loc.name.is_none() {
        return Err(ProtoError::new(ErrorCode::Denied, "the workspace root cannot be removed"));
    }
    base.check_protected(&loc, true)?;
    let (dir, name) = entry(&loc, path)?;
    let _serial = lock(mutations);
    let stat = existing(dir, name, path)?.ok_or_else(|| not_found(path))?;
    let removed: io::Result<()> = if kind(&stat) == FileType::Directory {
        if recursive { remove_tree(dir, name, 0) } else { rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR).map_err(Into::into) }
    } else {
        rustix::fs::unlinkat(dir, name, AtFlags::empty()).map_err(Into::into)
    };
    match removed {
        Ok(()) => {
            sync_dir(dir);
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {
            Err(ProtoError::new(ErrorCode::Conflict, format!("`{path}` is a non-empty directory; pass recursive")))
        }
        Err(err) => Err(io_error(&err, path)),
    }
}

fn cancel_if_hash(base: &Base, mutations: &Mutex<()>, path: &str, hash: &ContentHash) -> Outcome<()> {
    let loc = base.resolve(path, Follow::NoFinal, Authority::Path(Access::Write))?;
    base.check_protected(&loc, false)?;
    let (dir, name) = entry(&loc, path)?;
    let _serial = lock(mutations);
    let mut file = open_file(dir, name, path)?;
    if hash_file(&mut file).map_err(|err| io_error(&err, path))? != *hash {
        return Err(ProtoError::new(ErrorCode::PreconditionFailed, "reservation marker changed"));
    }
    rustix::fs::unlinkat(dir, name, AtFlags::empty()).map_err(|err| os_error(err, path))?;
    sync_dir(dir);
    Ok(())
}

fn rename(base: &Base, mutations: &Mutex<()>, from: &str, to: &str, overwrite: bool) -> Outcome<()> {
    let source = base.resolve(from, Follow::NoFinal, Authority::Path(Access::Tree))?;
    let target = base.resolve(to, Follow::NoFinal, Authority::Path(Access::Tree))?;
    if source.name.is_none() || target.name.is_none() {
        return Err(ProtoError::new(ErrorCode::Denied, "the workspace root cannot be moved or replaced"));
    }
    base.check_protected(&source, true)?;
    base.check_protected(&target, true)?;
    let (from_dir, from_name) = entry(&source, from)?;
    let (to_dir, to_name) = entry(&target, to)?;
    let _serial = lock(mutations);
    existing(from_dir, from_name, from)?.ok_or_else(|| not_found(from))?;
    match rename_at(from_dir, from_name, to_dir, to_name, overwrite) {
        Ok(()) => {
            sync_dir(from_dir);
            sync_dir(to_dir);
            Ok(())
        }
        Err(err) if !overwrite && err.kind() == io::ErrorKind::AlreadyExists => {
            Err(ProtoError::new(ErrorCode::Conflict, format!("`{to}` already exists; pass overwrite")))
        }
        Err(err) => Err(io_error(&err, to)),
    }
}

/// Copies the open regular file `from` to the new entry `name` of `dir`, keeping its mode.
fn copy_file(from: &mut File, dir: &OwnedFd, name: &OsStr) -> io::Result<File> {
    let mode = mode_of(&rustix::fs::fstat(&*from)?);
    let mut to = open_entry(dir, name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, Mode::RUSR.union(Mode::WUSR))?;
    io::copy(from, &mut to)?;
    rustix::fs::fchmod(&to, mode)?;
    Ok(to)
}

/// Copies the open directory `from` into the new directory `name` of `parent`; symlinks are copied
/// as links (never followed), special files are skipped, and each directory keeps its mode.
fn copy_tree(from: &OwnedFd, parent: &OwnedFd, name: &OsStr, depth: usize) -> io::Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(io::Error::other("directory tree too deep to copy"));
    }
    let mode = mode_of(&rustix::fs::fstat(from)?);
    rustix::fs::mkdirat(parent, name, Mode::RWXU)?;
    let to = open_dir(parent, name)?;
    for entry in rustix::fs::Dir::read_from(from)? {
        let entry = entry?;
        let child = OsStr::from_bytes(entry.file_name().to_bytes());
        if child == "." || child == ".." {
            continue;
        }
        let file_type = match entry.file_type() {
            FileType::Unknown => kind(&stat_entry(from, child)?),
            file_type => file_type,
        };
        match file_type {
            FileType::Symlink => {
                let target = rustix::fs::readlinkat(from, child, Vec::new())?;
                rustix::fs::symlinkat(target.as_c_str(), &to, child)?;
            }
            FileType::Directory => copy_tree(&open_dir(from, child)?, &to, child, depth + 1)?,
            FileType::RegularFile => {
                copy_file(&mut open_entry(from, child, OFlags::RDONLY, Mode::empty())?, &to, child)?;
            }
            _ => {}
        }
    }
    rustix::fs::fchmod(&to, mode).map_err(Into::into)
}

/// The identity of an open file.
fn identity(fd: &OwnedFd) -> io::Result<(i128, i128)> {
    let stat = rustix::fs::fstat(fd)?;
    Ok((i128::from(stat.st_dev), i128::from(stat.st_ino)))
}

fn copy(base: &Base, mutations: &Mutex<()>, from: &str, to: &str, overwrite: bool, recursive: bool) -> Outcome<()> {
    let source = base.resolve(from, Follow::Final, Authority::Path(Access::Read))?;
    let target = base.resolve(to, Follow::NoFinal, Authority::Path(Access::Tree))?;
    if target.name.is_none() {
        return Err(ProtoError::new(ErrorCode::Denied, "the workspace root cannot be replaced"));
    }
    base.check_protected(&target, true)?;
    let (dir, name) = entry(&target, to)?;
    let _serial = lock(mutations);
    // The source: a directory held open by the walk, or a regular file opened now.
    let mut file = None;
    let tree = if let Some(tree) = source.target_dir() {
        if !recursive {
            return Err(ProtoError::new(ErrorCode::Conflict, format!("`{from}` is a directory; pass recursive")));
        }
        let inside = identity(tree).map_err(|err| io_error(&err, from))?;
        for hop in &target.hops {
            if identity(&hop.fd).map_err(|err| io_error(&err, to))? == inside {
                return Err(ProtoError::new(ErrorCode::Conflict, format!("cannot copy `{from}` into itself")));
            }
        }
        Some(tree)
    } else {
        let (from_dir, from_name) = entry(&source, from)?;
        file = Some(open_file(from_dir, from_name, from)?);
        None
    };
    let current = existing(dir, name, to)?;
    if let Some(current) = &current {
        if !overwrite {
            return Err(ProtoError::new(ErrorCode::Conflict, format!("`{to}` already exists; pass overwrite")));
        }
        if kind(current) == FileType::Directory {
            return Err(ProtoError::new(ErrorCode::Conflict, format!("`{to}` is a directory; remove it first")));
        }
    }
    // Build the copy beside the destination, then rename it into place.
    let staging = staging_name(name);
    let placed = match (tree, file.as_mut()) {
        (Some(tree), _) => copy_tree(tree, dir, &staging, 0).and_then(|()| {
            if current.is_some() {
                rustix::fs::unlinkat(dir, name, AtFlags::empty())?;
            }
            rename_at(dir, &staging, dir, name, true)
        }),
        (None, Some(file)) => {
            copy_file(file, dir, &staging).and_then(|copied| copied.sync_all()).and_then(|()| rename_at(dir, &staging, dir, name, true))
        }
        (None, None) => Err(io::Error::other("nothing to copy")),
    };
    if let Err(err) = placed {
        let cleanup = if tree.is_some() {
            remove_tree(dir, &staging, 0)
        } else {
            rustix::fs::unlinkat(dir, &staging, AtFlags::empty()).map_err(Into::into)
        };
        if let Err(cleanup) = cleanup
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%cleanup, staging = %staging.to_string_lossy(), "could not remove a partial copy");
        }
        return Err(io_error(&err, to));
    }
    sync_dir(dir);
    Ok(())
}

impl Fs for LocalFs {
    fn stat<'a>(&'a self, path: &'a str, hash: bool) -> BoxFuture<'a, Outcome<Meta>> {
        let base = Arc::clone(&self.base);
        let path = path.to_owned();
        Box::pin(blocking(move || stat(&base, &path, hash)))
    }

    fn read<'a>(&'a self, path: &'a str, range: Option<ByteRange>, max_bytes: u64, hash: bool) -> BoxFuture<'a, Outcome<FsReadResult>> {
        let base = Arc::clone(&self.base);
        let path = path.to_owned();
        Box::pin(blocking(move || read(&base, &path, range, max_bytes, hash)))
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

    fn supports_reservations(&self) -> bool {
        true
    }

    fn cancel_if_hash<'a>(&'a self, path: &'a str, hash: &'a ContentHash) -> BoxFuture<'a, Outcome<()>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let path = path.to_owned();
        let hash = hash.clone();
        Box::pin(blocking(move || cancel_if_hash(&base, &mutations, &path, &hash)))
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

    fn copy<'a>(&'a self, req: CopyRequest<'a>) -> BoxFuture<'a, Outcome<()>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let (from, to) = (req.from.to_owned(), req.to.to_owned());
        let (overwrite, recursive) = (req.overwrite, req.recursive);
        Box::pin(blocking(move || copy(&base, &mutations, &from, &to, overwrite, recursive)))
    }

    fn rename<'a>(&'a self, from: &'a str, to: &'a str, overwrite: bool, _key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>> {
        let base = Arc::clone(&self.base);
        let mutations = Arc::clone(&self.mutations);
        let (from, to) = (from.to_owned(), to.to_owned());
        Box::pin(blocking(move || rename(&base, &mutations, &from, &to, overwrite)))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Seek, SeekFrom};

    use super::bounded_bytes;

    struct CountedReader {
        len: u64,
        pos: u64,
        bytes_read: u64,
    }

    impl Read for CountedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = usize::try_from(self.len.saturating_sub(self.pos).min(buf.len() as u64)).unwrap_or(0);
            buf.get_mut(..count).unwrap_or_default().fill(b'x');
            self.pos += count as u64;
            self.bytes_read += count as u64;
            Ok(count)
        }
    }

    impl Seek for CountedReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            match pos {
                SeekFrom::Start(offset) => {
                    self.pos = offset;
                    Ok(offset)
                }
                _ => Err(io::Error::other("unexpected seek")),
            }
        }
    }

    #[test]
    fn prefix_read_stops_at_limit() {
        let mut source = CountedReader { len: 1 << 30, pos: 0, bytes_read: 0 };
        assert_eq!(bounded_bytes(&mut source, 1 << 20, 17).unwrap(), vec![b'x'; 17]);
        assert_eq!(source.bytes_read, 17);
        assert_eq!(source.pos, (1 << 20) + 17);

        assert!(bounded_bytes(&mut source, u64::MAX, 0).unwrap().is_empty());
        assert_eq!(source.bytes_read, 17);
        assert_eq!(source.pos, (1 << 20) + 17);
    }
}
