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
//! An entry is written under a temporary name, locked, filled and then renamed into place, so a
//! sweeper can never lock a half-written entry. Temporary files are removed by a sweep only once
//! they are unlocked and older than [`STALE_TEMP`]. The journal is bounded: at most [`MAX_ENTRIES`]
//! files; a reservation that cannot be journaled is refused (fail closed), since its marker could
//! otherwise outlive a crash.

use std::fs::{DirBuilder, File};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aim_proto::harness::ContentHash;
use rustix::fs::{FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};

/// The journal's format version (the `version` field of every entry).
const VERSION: u32 = 1;
/// Most files the journal directory may hold.
pub(crate) const MAX_ENTRIES: usize = 4096;
/// Most entries one sweep examines, so a `workspace.open` stays quick.
const MAX_SWEEP: usize = 256;
/// Largest entry read.
const MAX_ENTRY_BYTES: u64 = 16 * 1024;
/// An unlocked temporary entry older than this was abandoned mid-write.
const STALE_TEMP: Duration = Duration::from_secs(60);
/// Prefix of temporary entry names.
const TEMP_PREFIX: &str = ".tmp-";
/// Suffix of committed entry names.
const SUFFIX: &str = ".json";

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
    path: PathBuf,
    _file: File,
}

impl Entry {
    /// Deletes the entry: its reservation ended (finalized, cancelled, or no longer ours).
    pub(crate) fn remove(self) {
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%err, entry = %self.path.display(), "could not delete a reservation journal entry");
        }
    }
}

fn lock_exclusive(file: &File) -> io::Result<bool> {
    match rustix::fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Opens an entry read-write without following a symlink; `create` makes a new one (mode 0600).
fn open_private(path: &Path, create: bool) -> io::Result<File> {
    let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if create {
        flags |= OFlags::CREATE | OFlags::EXCL;
    }
    let fd = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)?;
    Ok(File::from(fd))
}

impl Journal {
    /// A journal in `dir` (created on first use).
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Creates the directory if needed and checks it is a private directory of this user.
    fn ensure_dir(&self) -> io::Result<()> {
        DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        let meta = std::fs::symlink_metadata(&self.dir)?;
        if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the reservation journal is not a directory of this user"));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Journals a reservation before its marker is created; the returned entry holds the lock.
    ///
    /// # Errors
    /// The journal is full ([`MAX_ENTRIES`]; `ErrorKind::QuotaExceeded`) or cannot be written.
    pub(crate) fn record(&self, root: &str, path: &str, hash: &ContentHash) -> io::Result<Entry> {
        self.ensure_dir()?;
        if std::fs::read_dir(&self.dir)?.take(MAX_ENTRIES.saturating_add(1)).count() >= MAX_ENTRIES {
            return Err(io::Error::new(io::ErrorKind::QuotaExceeded, "the reservation journal is full"));
        }
        let name = crate::id::random_hex();
        let temp = self.dir.join(format!("{TEMP_PREFIX}{name}"));
        let path_final = self.dir.join(format!("{name}{SUFFIX}"));
        let mut file = open_private(&temp, true)?;
        let written = (|| -> io::Result<()> {
            if !lock_exclusive(&file)? {
                return Err(io::Error::other("a new reservation journal entry is locked by someone else"));
            }
            let record = Record { version: VERSION, root: root.to_owned(), path: path.to_owned(), hash: hash.0.clone() };
            file.write_all(&serde_json::to_vec(&record).map_err(io::Error::other)?)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path_final)?;
            File::open(&self.dir).and_then(|dir| dir.sync_all())
        })();
        if let Err(err) = written {
            drop(std::fs::remove_file(&temp));
            return Err(err);
        }
        Ok(Entry { path: path_final, _file: file })
    }

    /// The abandoned entries of `root` (their owner is gone), each locked by the caller now, at
    /// most [`MAX_SWEEP`]. Unreadable or malformed committed entries whose lock is free, and stale
    /// temporary files, are deleted on the way.
    pub(crate) fn abandoned(&self, root: &str) -> Vec<(Record, Entry)> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut found = Vec::new();
        for entry in dir.take(MAX_SWEEP).flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let path = entry.path();
            if name.starts_with(TEMP_PREFIX) {
                sweep_temp(&path);
                continue;
            }
            if !name.ends_with(SUFFIX) {
                continue;
            }
            let Ok(mut file) = open_private(&path, false) else { continue };
            if !lock_exclusive(&file).unwrap_or(false) {
                continue;
            }
            let mut bytes = Vec::new();
            let record = (&mut file)
                .take(MAX_ENTRY_BYTES)
                .read_to_end(&mut bytes)
                .ok()
                .and_then(|_| serde_json::from_slice::<Record>(&bytes).ok())
                .filter(|record| record.version == VERSION);
            match record {
                Some(record) if record.root == root => found.push((record, Entry { path, _file: file })),
                Some(_) => {}
                None => Entry { path, _file: file }.remove(),
            }
        }
        found
    }
}

/// Deletes a temporary entry left by a writer that died before committing it.
fn sweep_temp(path: &Path) {
    let Ok(file) = open_private(path, false) else { return };
    let old = file
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_TEMP);
    if old && lock_exclusive(&file).unwrap_or(false) {
        drop(std::fs::remove_file(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> ContentHash {
        ContentHash("sha256:00".into())
    }

    #[test]
    fn live_entries_are_not_abandoned_and_dropped_ones_are() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("reservations"));
        let live = journal.record("/w", "/w/a.png", &hash()).unwrap();
        let dead = journal.record("/w", "/w/b.png", &hash()).unwrap();
        let other = journal.record("/elsewhere", "/elsewhere/c.png", &hash()).unwrap();
        let mode = std::fs::metadata(dir.path().join("reservations")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let file_mode = std::fs::metadata(&live.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        drop(dead);
        drop(other);
        let abandoned = journal.abandoned("/w");
        assert_eq!(abandoned.iter().map(|(record, _)| record.path.as_str()).collect::<Vec<_>>(), ["/w/b.png"]);
        // While a sweeper holds the abandoned entry, another sweeper skips it too.
        assert!(journal.abandoned("/w").is_empty());
        for (_, entry) in abandoned {
            entry.remove();
        }
        live.remove();
        let left: Vec<_> = std::fs::read_dir(dir.path().join("reservations")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "only the other root's entry is left: {left:?}");
    }

    #[test]
    fn malformed_entries_are_deleted_and_fresh_temporaries_kept() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("j"));
        journal.ensure_dir().unwrap();
        std::fs::write(dir.path().join("j/garbage.json"), b"{not json").unwrap();
        std::fs::write(dir.path().join("j/.tmp-fresh"), b"").unwrap();
        assert!(journal.abandoned("/w").is_empty());
        assert!(!dir.path().join("j/garbage.json").exists());
        assert!(dir.path().join("j/.tmp-fresh").exists(), "a fresh temporary may still be being written");
    }

    #[test]
    fn a_full_journal_refuses_new_entries() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("j"));
        journal.ensure_dir().unwrap();
        for index in 0..MAX_ENTRIES {
            std::fs::write(dir.path().join(format!("j/{index}.json")), b"").unwrap();
        }
        assert_eq!(journal.record("/w", "/w/x", &hash()).unwrap_err().kind(), io::ErrorKind::QuotaExceeded);
    }
}
