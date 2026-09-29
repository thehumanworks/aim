//! The durable journal of live file reservations (ADR 0067), so a reservation marker left behind by
//! an aimx that was killed or crashed is removed at the next `workspace.open` of its root.
//!
//! One private directory per OS user (`~/.aim/aimx/reservations`, mode 0700, a protected path)
//! holds one small JSON file (mode 0600) per live reservation: `{version, root, path, hash}`. The
//! owning server keeps that file open and exclusively `flock`ed for the reservation's lifetime and
//! deletes it when the reservation ends. An entry whose lock anyone can take therefore belongs to a
//! process that is gone. The sweep at `workspace.open` takes such an entry's lock, removes the
//! marker only while it still has the recorded hash (so any later write survives), and deletes the
//! entry. A live reservation of a concurrent aimx keeps its lock and is never swept.
//!
//! **Trusted storage (REV19 B1).** Both writing and sweeping open the directory component by
//! component from `/` without following any symlink, require it to be a directory owned by this
//! user (tightening its mode to 0700), and then act only relative to that held descriptor. Storage
//! that fails these checks is neither written nor swept, so nothing is ever deleted from it.
//!
//! **Names.** An entry is `<tag>-<random>.json`, where `<tag>` is 16 hex digits of the SHA-256 of
//! its root, so a sweep reads every name but opens only its own root's entries (REV19 B2). It is
//! written under the temporary name `.tmp-<tag>-<random>`, locked, filled and renamed into place,
//! so a sweeper never locks a half-written entry; an unlocked temporary older than [`STALE_TEMP`]
//! is removed by a sweep.
//!
//! **Bounds.** At most [`MAX_ENTRIES`] files besides the `.lock` file. Admission (count, then create)
//! runs under an exclusive `flock` on `.lock`, which serializes recorders across threads and aimx
//! processes (REV19 B3). A reservation that cannot be journaled is refused (fail closed), since its
//! marker could otherwise outlive a crash. A sweep reads at most [`MAX_SCAN`] names and hands back
//! at most [`MAX_CLEANUPS`] abandoned entries, starting at a random point of its root's sorted
//! entries, so repeated sweeps reach every entry even when some cleanups keep failing.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aim_proto::harness::ContentHash;
use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The journal's format version (the `version` field of every entry).
const VERSION: u32 = 1;
/// Most files the journal directory may hold, besides [`LOCK`].
pub(crate) const MAX_ENTRIES: usize = 4096;
/// Most names one sweep reads (every possible entry, the lock file, and slack for strays).
const MAX_SCAN: usize = MAX_ENTRIES + 64;
/// Most abandoned entries one sweep hands back for cleanup, so a `workspace.open` stays quick.
const MAX_CLEANUPS: usize = 256;
/// Largest entry read.
const MAX_ENTRY_BYTES: u64 = 16 * 1024;
/// An unlocked temporary entry older than this was abandoned mid-write.
const STALE_TEMP: Duration = Duration::from_secs(60);
/// Prefix of temporary entry names.
const TEMP_PREFIX: &str = ".tmp-";
/// Suffix of committed entry names.
const SUFFIX: &str = ".json";
/// The admission lock file.
const LOCK: &str = ".lock";

/// One journaled reservation (the persisted format, ADR 0067).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// Format version, [`VERSION`].
    pub(crate) version: u32,
    /// Canonical root of the workspace the marker is in.
    pub(crate) root: String,
    /// Absolute, confined path of the marker.
    pub(crate) path: String,
    /// `sha256:<hex>` of the marker's bytes.
    pub(crate) hash: String,
}

/// The journal directory.
#[derive(Clone, Debug)]
pub(crate) struct Journal {
    dir: PathBuf,
}

/// A live entry: its file stays open and locked until [`Entry::remove`] (the reservation ended)
/// or until it is dropped (the process ends; the entry is then swept at a later open).
#[derive(Debug)]
pub(crate) struct Entry {
    dir: Arc<OwnedFd>,
    name: String,
    _file: File,
}

impl Entry {
    /// Deletes the entry, relative to the directory it was found in: its reservation ended
    /// (finalized, cancelled, or no longer ours).
    pub(crate) fn remove(self) {
        match rustix::fs::unlinkat(&*self.dir, self.name.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(err) => tracing::warn!(%err, entry = %self.name, "could not delete a reservation journal entry"),
        }
    }
}

/// 16 hex digits naming a root's entries.
fn root_tag(root: &str) -> String {
    crate::id::hex(Sha256::digest(root.as_bytes()).get(..8).unwrap_or_default())
}

fn rejected(path: &Path, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("the reservation journal `{}` {why}", path.display()))
}

/// Opens `path` as a private directory of this user: component by component from `/`, never
/// following a symlink (`O_NOFOLLOW` on every component); `create` makes missing components with
/// mode 0700. `None` when it does not exist and `create` is false.
///
/// # Errors
/// A relative path or a `..` component, a symlink or non-directory anywhere on the path, a final
/// directory owned by another user, or an I/O failure.
fn open_private_dir(path: &Path, create: bool) -> io::Result<Option<OwnedFd>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(rejected(path, "is not an absolute path"));
    }
    let mut dir = rustix::fs::open("/", flags, Mode::empty())?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(rejected(path, "has a `.` or `..` component"));
        };
        dir = match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&dir, name, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(err) => return Err(err.into()),
                }
                rustix::fs::openat(&dir, name, flags, Mode::empty()).map_err(|err| match err {
                    Errno::LOOP | Errno::NOTDIR => rejected(path, "passes through a symlink or a file"),
                    other => other.into(),
                })?
            }
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => return Err(rejected(path, "passes through a symlink or a file")),
            Err(err) => return Err(err.into()),
        };
    }
    let stat = rustix::fs::fstat(&dir)?;
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(rejected(path, "is owned by another user"));
    }
    if Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO) {
        rustix::fs::fchmod(&dir, Mode::RWXU)?;
    }
    Ok(Some(dir))
}

/// Opens entry `name` of `dir` read-write without following a symlink; `create` makes a new one
/// (mode 0600).
fn open_entry(dir: &OwnedFd, name: &str, create: bool) -> io::Result<File> {
    let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if create {
        flags |= OFlags::CREATE | OFlags::EXCL;
    }
    Ok(File::from(rustix::fs::openat(dir, name, flags, Mode::RUSR | Mode::WUSR)?))
}

fn try_lock(file: &File) -> io::Result<bool> {
    match rustix::fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::WOULDBLOCK) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// The names in `dir` (without `.` and `..`), at most [`MAX_SCAN`].
fn names(dir: &OwnedFd) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        if names.len() >= MAX_SCAN {
            break;
        }
        // Journal names are ASCII; anything else is a stray this journal never wrote.
        if let Some(name) = name.to_str() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// A random start among `len` entries.
fn random_offset(len: usize) -> usize {
    let random = crate::id::random_hex();
    let value = random.get(..16).and_then(|hex| u64::from_str_radix(hex, 16).ok()).unwrap_or(0);
    usize::try_from(value).unwrap_or(0).checked_rem(len).unwrap_or(0)
}

impl Journal {
    /// A journal in `dir` (created on first use).
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Journals a reservation before its marker is created; the returned entry holds the lock.
    /// Admission runs under the journal's `.lock`, so no two recorders (threads or processes) can
    /// both take the last slot. Blocking: call it off the async runtime.
    ///
    /// # Errors
    /// The journal is full ([`MAX_ENTRIES`]; `ErrorKind::QuotaExceeded`), is not trusted storage
    /// (`PermissionDenied`), or cannot be written.
    pub(crate) fn record(&self, root: &str, path: &str, hash: &ContentHash) -> io::Result<Entry> {
        let dir = Arc::new(open_private_dir(&self.dir, true)?.ok_or_else(|| rejected(&self.dir, "vanished"))?);
        let admission = File::from(rustix::fs::openat(
            &*dir,
            LOCK,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        rustix::fs::flock(&admission, FlockOperation::LockExclusive)?;
        let taken = names(&dir)?.iter().filter(|name| name.as_str() != LOCK).count();
        if taken >= MAX_ENTRIES {
            return Err(io::Error::new(io::ErrorKind::QuotaExceeded, "the reservation journal is full"));
        }
        let unique = format!("{}-{}", root_tag(root), crate::id::random_hex());
        let temp = format!("{TEMP_PREFIX}{unique}");
        let name = format!("{unique}{SUFFIX}");
        let mut file = open_entry(&dir, &temp, true)?;
        let written = (|| -> io::Result<()> {
            if !try_lock(&file)? {
                return Err(io::Error::other("a new reservation journal entry is locked by someone else"));
            }
            let record = Record { version: VERSION, root: root.to_owned(), path: path.to_owned(), hash: hash.0.clone() };
            file.write_all(&serde_json::to_vec(&record).map_err(io::Error::other)?)?;
            file.sync_all()?;
            rustix::fs::renameat(&*dir, temp.as_str(), &*dir, name.as_str())?;
            rustix::fs::fsync(&*dir).map_err(Into::into)
        })();
        if let Err(err) = written {
            if let Err(cleanup) = rustix::fs::unlinkat(&*dir, temp.as_str(), AtFlags::empty()) {
                tracing::debug!(%cleanup, "could not remove a failed reservation journal entry");
            }
            return Err(err);
        }
        drop(admission);
        Ok(Entry { dir, name, _file: file })
    }

    /// The abandoned entries of `root` (their owner is gone), each locked by the caller now, at
    /// most [`MAX_CLEANUPS`]. Malformed entries of `root` whose lock is free, and stale temporary
    /// files, are deleted on the way. Untrusted storage yields nothing and loses nothing.
    pub(crate) fn abandoned(&self, root: &str) -> Vec<(Record, Entry)> {
        let dir = match open_private_dir(&self.dir, false) {
            Ok(Some(dir)) => Arc::new(dir),
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(%err, "not sweeping an untrusted reservation journal");
                return Vec::new();
            }
        };
        let Ok(names) = names(&dir) else { return Vec::new() };
        let prefix = format!("{}-", root_tag(root));
        let mut mine = Vec::new();
        for name in names {
            if name.starts_with(TEMP_PREFIX) {
                sweep_temp(&dir, &name);
            } else if name.starts_with(&prefix) && name.ends_with(SUFFIX) {
                mine.push(name);
            }
        }
        mine.sort();
        let start = random_offset(mine.len());
        mine.rotate_left(start);
        let mut found = Vec::new();
        for name in mine {
            if found.len() >= MAX_CLEANUPS {
                break;
            }
            let Ok(mut file) = open_entry(&dir, &name, false) else { continue };
            if !try_lock(&file).unwrap_or(false) {
                continue;
            }
            let mut bytes = Vec::new();
            let record = (&mut file)
                .take(MAX_ENTRY_BYTES)
                .read_to_end(&mut bytes)
                .ok()
                .and_then(|_| serde_json::from_slice::<Record>(&bytes).ok())
                .filter(|record| record.version == VERSION);
            let entry = Entry { dir: Arc::clone(&dir), name, _file: file };
            match record {
                Some(record) if record.root == root => found.push((record, entry)),
                // Another root with the same tag: not ours to judge.
                Some(_) => {}
                None => entry.remove(),
            }
        }
        found
    }
}

/// Deletes a temporary entry left by a writer that died before committing it.
fn sweep_temp(dir: &OwnedFd, name: &str) {
    let Ok(file) = open_entry(dir, name, false) else { return };
    let old = file
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_TEMP);
    if old
        && try_lock(&file).unwrap_or(false)
        && let Err(err) = rustix::fs::unlinkat(dir, name, AtFlags::empty())
    {
        tracing::debug!(%err, "could not remove a stale reservation journal temporary");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Barrier;

    use super::*;

    fn hash() -> ContentHash {
        ContentHash("sha256:00".into())
    }

    /// A temporary directory spelled without symlinks (macOS temporaries live under `/var`, a
    /// symlink to `/private/var`, which the journal refuses to pass through).
    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        (dir, real)
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
        names.retain(|name| name != LOCK);
        names
    }

    fn eventually_abandoned(journal: &Journal, root: &str, wanted: usize) -> Vec<(Record, Entry)> {
        // A child forked by a concurrent test holds a copy of every descriptor until it execs
        // (then `CLOEXEC` closes them), and with it an entry's lock: retry briefly.
        let mut abandoned = journal.abandoned(root);
        for _ in 0..100 {
            if abandoned.len() >= wanted {
                break;
            }
            drop(abandoned);
            std::thread::sleep(Duration::from_millis(10));
            abandoned = journal.abandoned(root);
        }
        abandoned
    }

    #[test]
    fn live_entries_are_not_abandoned_and_dropped_ones_are() {
        let (_guard, dir) = tempdir();
        let journal = Journal::new(dir.join("reservations"));
        let live = journal.record("/w", "/w/a.png", &hash()).unwrap();
        let dead = journal.record("/w", "/w/b.png", &hash()).unwrap();
        let other = journal.record("/elsewhere", "/elsewhere/c.png", &hash()).unwrap();
        assert_eq!(std::fs::metadata(dir.join("reservations")).unwrap().permissions().mode() & 0o777, 0o700);
        let entry = dir.join("reservations").join(&live.name);
        assert_eq!(std::fs::metadata(entry).unwrap().permissions().mode() & 0o777, 0o600);
        drop(dead);
        drop(other);
        let abandoned = eventually_abandoned(&journal, "/w", 1);
        assert_eq!(abandoned.iter().map(|(record, _)| record.path.as_str()).collect::<Vec<_>>(), ["/w/b.png"]);
        // While a sweeper holds the abandoned entry, another sweeper skips it too.
        assert!(journal.abandoned("/w").is_empty());
        for (_, entry) in abandoned {
            entry.remove();
        }
        live.remove();
        assert_eq!(files(&dir.join("reservations")).len(), 1, "only the other root's entry is left");
    }

    #[test]
    fn malformed_entries_are_deleted_and_fresh_temporaries_kept() {
        let (_guard, dir) = tempdir();
        let journal = Journal::new(dir.join("j"));
        drop(journal.record("/w", "/w/x", &hash()).unwrap());
        let garbage = format!("{}-garbage{SUFFIX}", root_tag("/w"));
        std::fs::write(dir.join("j").join(&garbage), b"{not json").unwrap();
        std::fs::write(dir.join("j/.tmp-fresh"), b"").unwrap();
        let abandoned = eventually_abandoned(&journal, "/w", 1);
        assert_eq!(abandoned.len(), 1);
        assert!(!dir.join("j").join(garbage).exists());
        assert!(dir.join("j/.tmp-fresh").exists(), "a fresh temporary may still be being written");
    }

    /// REV19 B1: a symlinked journal directory, or one reached through a symlinked ancestor, is
    /// neither written nor swept, and nothing in it is deleted.
    #[test]
    fn untrusted_storage_is_neither_written_nor_swept() {
        let (_guard, dir) = tempdir();
        let real = Journal::new(dir.join("real/reservations"));
        drop(real.record("/w", "/w/marker", &hash()).unwrap());
        std::os::unix::fs::symlink(dir.join("real/reservations"), dir.join("alias")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("parent-alias")).unwrap();
        for untrusted in [dir.join("alias"), dir.join("parent-alias/reservations")] {
            let journal = Journal::new(untrusted.clone());
            assert_eq!(journal.record("/w", "/w/no", &hash()).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "{untrusted:?}");
            assert!(journal.abandoned("/w").is_empty(), "{untrusted:?}");
        }
        assert_eq!(files(&dir.join("real/reservations")).len(), 1, "the entry behind the symlink is untouched");
        let relative = Journal::new(PathBuf::from("relative/journal"));
        assert_eq!(relative.record("/w", "/w/no", &hash()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    /// REV19 B2: a sweep reaches its root's abandoned entries however many other entries (other
    /// roots, live reservations) the directory holds, and repeated sweeps reach all of them.
    #[test]
    fn sweeps_reach_every_abandoned_entry() {
        let (_guard, dir) = tempdir();
        let journal = Journal::new(dir.join("j"));
        for index in 0..300 {
            drop(journal.record("/other", &format!("/other/{index}"), &hash()).unwrap());
        }
        let live: Vec<Entry> = (0..20).map(|index| journal.record("/wanted", &format!("/wanted/live-{index}"), &hash()).unwrap()).collect();
        for index in 0..300 {
            drop(journal.record("/wanted", &format!("/wanted/{index}"), &hash()).unwrap());
        }
        let mut swept = 0;
        let mut sweeps = 0;
        while swept < 300 && sweeps < 100 {
            sweeps += 1;
            let abandoned = journal.abandoned("/wanted");
            assert!(abandoned.len() <= MAX_CLEANUPS);
            assert!(abandoned.iter().all(|(record, _)| !record.path.contains("live")), "live entries are never handed out");
            if abandoned.is_empty() {
                // A concurrent test's forked child may briefly hold copies of the locks.
                std::thread::sleep(Duration::from_millis(10));
            }
            swept += abandoned.len();
            for (_, entry) in abandoned {
                entry.remove();
            }
        }
        assert_eq!(swept, 300, "after {sweeps} sweeps");
        assert_eq!(files(&dir.join("j")).len(), 300 + live.len(), "the other root's entries and the live ones remain");
    }

    /// REV19 B3: concurrent recorders never exceed the bound, whatever their interleaving.
    #[test]
    fn concurrent_admission_keeps_the_bound() {
        let (_guard, dir) = tempdir();
        let journal = Journal::new(dir.join("j"));
        drop(journal.record("/w", "/w/seed", &hash()).unwrap());
        for index in 0..MAX_ENTRIES - 2 {
            std::fs::write(dir.join(format!("j/fill-{index}")), "").unwrap();
        }
        let gate = Arc::new(Barrier::new(32));
        let threads: Vec<_> = (0..32)
            .map(|index| {
                let gate = Arc::clone(&gate);
                let journal = journal.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    journal.record("/w", &format!("/w/{index}"), &hash())
                })
            })
            .collect();
        let entries: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_eq!(entries.iter().filter(|entry| entry.is_ok()).count(), 1);
        assert!(entries.iter().filter_map(|entry| entry.as_ref().err()).all(|err| err.kind() == io::ErrorKind::QuotaExceeded));
        assert_eq!(files(&dir.join("j")).len(), MAX_ENTRIES);
    }
}
