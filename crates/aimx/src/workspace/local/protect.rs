//! The backend's protected-path check, by filesystem identity (REV4-A findings 1 and 2).
//!
//! The lexical check in [`crate::authz::Grant`] is an early rejection only; this one is
//! authoritative. The target is described by the walk that resolved it (every directory it holds
//! open, identified with `fstat`), each protected path by the filesystem at the time of the check
//! (its deepest existing ancestor canonicalised, then `lstat` per component; for a protected path
//! that is a symlink, both the link and what it resolves to), and [`guards`] decides. So every
//! spelling of a protected path or of an ancestor — another case, another Unicode normalization, a
//! symlink, a hard link — is refused, and so is creating a protected path that does not exist yet.
//!
//! Whether a directory's names compare ignoring case is probed on the directory itself (its own
//! name looked up with the case swapped, from its parent), per volume and not per OS, and cached
//! by identity; when that cannot tell (no cased letter, a mount point), names are folded, which
//! can only over-protect.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rustix::fs::{FileType, Stat};
use rustix::io::Errno;

use super::walk::{Loc, kind, stat_entry};
use crate::authz::ProtectedPaths;
use crate::authz::identity::{Chain, FileId, Step, guards};

/// Directories whose case sensitivity is remembered (the cache is cleared beyond this).
const MAX_PROBED: usize = 256;

/// Checks mutation targets against the protected paths.
#[derive(Debug)]
pub(super) struct Protector {
    protected: Arc<ProtectedPaths>,
    /// The root and its ancestors, from `/`.
    root: Vec<Step>,
    /// Whether names in the root compare ignoring case.
    root_fold: bool,
    /// Directories already probed for case sensitivity.
    folds: Mutex<HashMap<FileId, bool>>,
}

fn file_id(stat: &Stat) -> FileId {
    FileId { dev: i128::from(stat.st_dev), ino: i128::from(stat.st_ino) }
}

fn lstat_id(path: &Path) -> Option<FileId> {
    rustix::fs::lstat(path).ok().map(|stat| file_id(&stat))
}

/// `name` with every cased letter's case swapped, when it has one.
fn swapped(name: &OsStr) -> Option<OsString> {
    let text = name.to_str()?;
    let swapped: String = text
        .chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if c.is_lowercase() {
                Box::new(c.to_uppercase())
            } else if c.is_uppercase() {
                Box::new(c.to_lowercase())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect();
    (swapped != text).then(|| OsString::from(swapped))
}

/// Whether `found` (the lookup of the swapped name) shows the directory `id` ignores case; `None`
/// when it cannot tell.
fn probe(found: io::Result<Stat>, id: FileId, parent_dev: Option<i128>) -> Option<bool> {
    if parent_dev.is_some_and(|dev| dev != id.dev) {
        // A mount point: its name is looked up on the parent's volume, which says nothing here.
        return None;
    }
    match found {
        Ok(stat) => Some(file_id(&stat) == id),
        Err(err) if err.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Some(false),
        Err(_) => None,
    }
}

/// Each component of the canonical `path`, with its identity.
fn path_steps(path: &Path) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => {
                prefix.push("/");
                steps.push(Step { name: OsString::new(), id: lstat_id(&prefix) });
            }
            Component::Normal(name) => {
                prefix.push(name);
                steps.push(Step { name: name.to_owned(), id: lstat_id(&prefix) });
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {}
        }
    }
    steps
}

impl Protector {
    /// A checker for `protected`, for a workspace rooted at the canonical `root`.
    pub(super) fn new(protected: Arc<ProtectedPaths>, root: &Path) -> Self {
        let mut protector = Self { protected, root: path_steps(root), root_fold: true, folds: Mutex::new(HashMap::new()) };
        protector.root_fold = protector.path_folds(root);
        protector
    }

    fn cached(&self, id: FileId, probe: impl FnOnce() -> Option<bool>) -> bool {
        let mut folds = self.folds.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(fold) = folds.get(&id) {
            return *fold;
        }
        let fold = probe().unwrap_or(true);
        if folds.len() >= MAX_PROBED {
            folds.clear();
        }
        folds.insert(id, fold);
        fold
    }

    /// Whether names in the directory at the canonical `path` compare ignoring case.
    fn path_folds(&self, path: &Path) -> bool {
        let Some(id) = lstat_id(path) else { return true };
        self.cached(id, || {
            let (parent, name) = (path.parent()?, path.file_name()?);
            let parent_dev = lstat_id(parent).map(|parent| parent.dev);
            let found = rustix::fs::lstat(parent.join(swapped(name)?)).map_err(io::Error::from);
            probe(found, id, parent_dev)
        })
    }

    /// Whether names in the open directory `dir` (named `name` in the open `parent`) compare
    /// ignoring case.
    fn fd_folds(&self, parent: &OwnedFd, name: &OsStr, dir: &OwnedFd) -> io::Result<bool> {
        let id = file_id(&rustix::fs::fstat(dir)?);
        Ok(self.cached(id, || {
            let parent_dev = rustix::fs::fstat(parent).ok().map(|stat| file_id(&stat).dev);
            probe(stat_entry(parent, &swapped(name)?), id, parent_dev)
        }))
    }

    /// The chain of a resolved target.
    fn target(&self, loc: &Loc) -> io::Result<Chain> {
        let mut steps = self.root.clone();
        for hop in loc.hops.iter().skip(1) {
            steps.push(Step { name: hop.name.clone().unwrap_or_default(), id: Some(file_id(&rustix::fs::fstat(&hop.fd)?)) });
        }
        let fold_case = match loc.hops.as_slice() {
            [.., parent, last] => self.fd_folds(&parent.fd, last.name.as_deref().unwrap_or_default(), &last.fd)?,
            _ => self.root_fold,
        };
        steps.extend(loc.missing.iter().map(|name| Step { name: name.clone(), id: None }));
        if let Some(name) = &loc.name {
            let id = match (&loc.opened, loc.missing.is_empty()) {
                (Some(fd), _) => Some(file_id(&rustix::fs::fstat(fd)?)),
                (None, true) => stat_entry(loc.dir()?, name).ok().map(|stat| file_id(&stat)),
                (None, false) => None,
            };
            steps.push(Step { name: name.clone(), id });
        }
        Ok(Chain { steps, fold_case })
    }

    /// The chain of the protected entry at `path` (a symlink is described as itself).
    fn protected(&self, path: &Path) -> Chain {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Chain { steps: path_steps(path), fold_case: true };
        };
        let mut existing = parent.to_path_buf();
        let mut missing = Vec::new();
        let real = loop {
            if let Ok(real) = std::fs::canonicalize(&existing) {
                break real;
            }
            match (existing.file_name(), existing.parent()) {
                (Some(last), Some(up)) => {
                    missing.push(last.to_owned());
                    existing = up.to_path_buf();
                }
                _ => break PathBuf::from("/"),
            }
        };
        let mut steps = path_steps(&real);
        let fold_case = self.path_folds(&real);
        steps.extend(missing.iter().rev().map(|name| Step { name: name.clone(), id: None }));
        let id = if missing.is_empty() { lstat_id(&real.join(name)) } else { None };
        steps.push(Step { name: name.to_owned(), id });
        Chain { steps, fold_case }
    }

    /// The protected path that modifying the target of `loc` would modify (`tree`: including by
    /// removing or moving something that contains it), if any.
    pub(super) fn hit(&self, loc: &Loc, tree: bool) -> io::Result<Option<&str>> {
        let target = self.target(loc)?;
        for path in self.protected.paths() {
            let path_ref = Path::new(path);
            let mut chains = vec![self.protected(path_ref)];
            // A protected symlink protects what it points to as well.
            if rustix::fs::lstat(path_ref).is_ok_and(|stat| kind(&stat) == FileType::Symlink)
                && let Ok(real) = std::fs::canonicalize(path_ref)
            {
                chains.push(self.protected(&real));
            }
            if chains.iter().any(|chain| guards(&target, chain, tree)) {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }
}
