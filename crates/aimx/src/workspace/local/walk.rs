//! Descriptor-relative resolution: how the local backend *enforces* confinement (REV4-A finding 3).
//!
//! [`crate::authz::confine`] (the kernel's verified, lexical decision) says a path *names*
//! something inside the root. This module makes the operation *act* inside it, even while another
//! process swaps directories for symlinks: a path is walked one component at a time from a
//! directory descriptor held on the root, opening each directory with
//! `O_NOFOLLOW | O_DIRECTORY` relative to the one before (`openat2(RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS)` per component on Linux), so the kernel never follows a symlink for us.
//! Symlinks are expanded here instead: a relative target continues from the directory that holds
//! the link (`..` pops a held directory, and popping the root is an escape); an absolute target is
//! re-walked from the root descriptor after its root prefix is removed. The operation then runs
//! with `*at` calls on the descriptors this walk returns, so a directory swapped after it was
//! checked is never re-resolved by path.
//!
//! Semantics (the same on every OS): in-root symlinks are followed, anywhere; a link leading out of
//! the root is `denied`; a dangling link is `denied` (its target cannot be checked); missing
//! trailing components are allowed, for creation; at most [`MAX_LINKS`] links per resolution.
//!
//! The descriptors are what the operation acts on, so a directory that is *moved* out of the root
//! after it was opened is still acted on where it now is. Moving it requires write access to both
//! places already; ADR 0008 treats that as outside aimx's control.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

/// Most symlinks one resolution follows (the kernel's `MAXSYMLINKS` on Linux).
pub(super) const MAX_LINKS: usize = 40;
/// How often one component is retried when it changes between `openat` and `fstatat`.
const MAX_RETRIES: usize = 16;

/// The workspace root, held open.
#[derive(Debug)]
pub(super) struct Root {
    /// Canonical path of the root.
    pub(super) path: PathBuf,
    /// The root directory.
    pub(super) fd: Arc<OwnedFd>,
}

impl Root {
    /// Opens one directory descriptor and derives its actual path from that descriptor.
    pub(super) fn acquire(path: &Path) -> io::Result<Self> {
        let fd = rustix::fs::openat(rustix::fs::CWD, path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        let actual = descriptor_path(&fd)?;
        if !actual.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "workspace descriptor has no absolute path"));
        }
        Ok(Self { path: actual, fd: Arc::new(fd) })
    }
}

#[cfg(target_os = "macos")]
fn descriptor_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    let path = rustix::fs::getpath(fd)?;
    Ok(PathBuf::from(OsString::from_vec(path.into_bytes())))
}

#[cfg(target_os = "linux")]
fn descriptor_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;

    let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    if path.to_string_lossy().ends_with(" (deleted)") {
        return Err(io::Error::new(io::ErrorKind::NotFound, "workspace root was removed during acquisition"));
    }
    Ok(path)
}

/// Whether the final component's symlink is followed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Follow {
    /// Operate on the link's target (read, write, list, search, a process's directory).
    Final,
    /// Operate on the link itself (stat, remove, rename, a copy's destination).
    NoFinal,
}

/// One open directory on the way from the root to a target.
#[derive(Debug)]
pub(super) struct Hop {
    /// Its name in the directory before it (`None` for the root).
    pub(super) name: Option<OsString>,
    /// The open directory.
    pub(super) fd: OwnedFd,
}

/// A resolved target, with every directory on its way held open.
#[derive(Debug)]
pub(super) struct Loc {
    /// The open directories from the root down to the one that holds the target (never empty).
    pub(super) hops: Vec<Hop>,
    /// Directories that do not exist yet between the last hop and the target.
    pub(super) missing: Vec<OsString>,
    /// The target's name in the last hop, or `None` when the target is the last hop itself (the
    /// root).
    pub(super) name: Option<OsString>,
    /// The target, when it is an existing directory reached with [`Follow::Final`].
    pub(super) opened: Option<OwnedFd>,
}

impl Loc {
    /// The directory holding the target (the target itself for the root).
    pub(super) fn dir(&self) -> io::Result<&OwnedFd> {
        self.hops.last().map(|hop| &hop.fd).ok_or_else(|| io::Error::other("empty resolution"))
    }

    /// The target as a directory, when it is one (the root, or an existing directory followed).
    pub(super) fn target_dir(&self) -> Option<&OwnedFd> {
        match &self.name {
            None => self.hops.last().map(|hop| &hop.fd),
            Some(_) => self.opened.as_ref(),
        }
    }

    /// The real path the walk arrived at (for results and messages; never re-resolved).
    pub(super) fn real_path(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        for hop in &self.hops {
            if let Some(name) = &hop.name {
                path.push(name);
            }
        }
        for name in &self.missing {
            path.push(name);
        }
        if let Some(name) = &self.name {
            path.push(name);
        }
        path
    }
}

/// Why a walk refused a path.
#[derive(Debug)]
pub(super) enum WalkError {
    /// It (or a link on it) leads out of the root.
    Outside,
    /// A symlink on it points to something that does not exist.
    Dangling,
    /// More than [`MAX_LINKS`] links.
    TooManyLinks,
    /// A component that must be a directory is not one.
    NotADirectory(OsString),
    /// The filesystem failed.
    Io(io::Error),
}

impl From<io::Error> for WalkError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<Errno> for WalkError {
    fn from(err: Errno) -> Self {
        Self::Io(err.into())
    }
}

/// Opens directory `name` in `dir` without following a symlink.
#[cfg(target_os = "linux")]
pub(super) fn open_dir(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    use rustix::fs::ResolveFlags;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    for _ in 0..MAX_RETRIES {
        match rustix::fs::openat2(dir, name, flags, Mode::empty(), resolve) {
            // A rename raced the lookup; try again.
            Err(Errno::AGAIN) => {}
            // Kernels before 5.6 have no `openat2`.
            Err(Errno::NOSYS) => return rustix::fs::openat(dir, name, flags, Mode::empty()).map_err(Into::into),
            other => return other.map_err(Into::into),
        }
    }
    Err(Errno::AGAIN.into())
}

/// Opens directory `name` in `dir` without following a symlink.
#[cfg(not(target_os = "linux"))]
pub(super) fn open_dir(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    rustix::fs::openat(dir, name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .map_err(Into::into)
}

/// Opens entry `name` in `dir` without following a symlink (`O_NONBLOCK`, so a FIFO cannot block
/// the opener; callers check the type with `fstat`).
pub(super) fn open_entry(dir: &OwnedFd, name: &OsStr, flags: OFlags, mode: Mode) -> io::Result<std::fs::File> {
    let fd = rustix::fs::openat(dir, name, flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, mode)?;
    Ok(std::fs::File::from(fd))
}

/// `lstat` of entry `name` in `dir`.
pub(super) fn stat_entry(dir: &OwnedFd, name: &OsStr) -> io::Result<Stat> {
    rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(Into::into)
}

/// The type of a `Stat`.
pub(super) fn kind(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

/// The path components of `path` as names, with `..` kept (`.` and the root dropped).
fn names(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// Where an absolute symlink target lands relative to the root, when it does.
///
/// A target spelled under the canonical root is taken lexically (a `..` in the rest is resolved by
/// the walk, which cannot climb above the root). A target spelled through a symlinked prefix
/// outside the root (macOS's `/var` → `/private/var`) is canonicalised to find where it lands;
/// that only *chooses* an in-root path, which the walk then enforces from the root descriptor, so
/// a race on the outside prefix cannot lead anywhere else.
fn beneath(root: &Path, target: &Path) -> Option<PathBuf> {
    if let Ok(rest) = target.strip_prefix(root) {
        return Some(rest.to_path_buf());
    }
    let mut existing = target.to_path_buf();
    let mut missing = Vec::new();
    let real = loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            break real;
        }
        missing.push(existing.file_name()?.to_owned());
        existing = existing.parent()?.to_path_buf();
    };
    let mut rest = real.strip_prefix(root).ok()?.to_path_buf();
    for name in missing.iter().rev() {
        rest.push(name);
    }
    Some(rest)
}

/// Resolves `rel` (relative to the root, lexically normalised) by walking descriptors.
pub(super) fn walk(root: &Root, rel: &Path, follow: Follow) -> Result<Loc, WalkError> {
    let mut hops = vec![Hop { name: None, fd: root.fd.try_clone()? }];
    // Each pending name carries whether it came from a link's target (which must exist).
    let mut pending: VecDeque<(OsString, bool)> = names(rel).into_iter().map(|name| (name, false)).collect();
    let mut links = 0usize;
    let mut retries = 0usize;
    while let Some((name, from_link)) = pending.pop_front() {
        if name == ".." {
            if hops.len() <= 1 {
                return Err(WalkError::Outside);
            }
            hops.pop();
            continue;
        }
        if name.is_empty() || name == "." {
            continue;
        }
        let last = pending.is_empty();
        if last && follow == Follow::NoFinal {
            return Ok(Loc { hops, missing: Vec::new(), name: Some(name), opened: None });
        }
        let parent = &hops.last().ok_or(WalkError::Outside)?.fd;
        match open_dir(parent, &name) {
            Ok(fd) if last => return Ok(Loc { hops, missing: Vec::new(), name: Some(name), opened: Some(fd) }),
            Ok(fd) => hops.push(Hop { name: Some(name), fd }),
            Err(err) if err.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                if from_link {
                    return Err(WalkError::Dangling);
                }
                // `name` and everything after it do not exist: directories to create, then the target.
                let mut rest: Vec<OsString> = std::iter::once(name).chain(pending.into_iter().map(|(name, _)| name)).collect();
                if rest.iter().any(|name| name == "..") {
                    return Err(WalkError::Dangling);
                }
                let target = rest.pop();
                return Ok(Loc { hops, missing: rest, name: target, opened: None });
            }
            Err(err) if [Errno::NOTDIR, Errno::LOOP].iter().any(|errno| err.raw_os_error() == Some(errno.raw_os_error())) => {
                let stat = match stat_entry(parent, &name) {
                    Ok(stat) => stat,
                    Err(err) if err.raw_os_error() == Some(Errno::NOENT.raw_os_error()) && retries < MAX_RETRIES => {
                        // It changed under us; look again.
                        retries += 1;
                        pending.push_front((name, from_link));
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                };
                match kind(&stat) {
                    FileType::Symlink => {
                        links += 1;
                        if links > MAX_LINKS {
                            return Err(WalkError::TooManyLinks);
                        }
                        let target = PathBuf::from(OsString::from_vec(rustix::fs::readlinkat(parent, &name, Vec::new())?.into_bytes()));
                        let expanded = if target.is_absolute() {
                            let inside = beneath(&root.path, &target).ok_or(WalkError::Outside)?;
                            hops.truncate(1);
                            names(&inside)
                        } else {
                            names(&target)
                        };
                        for name in expanded.into_iter().rev() {
                            pending.push_front((name, true));
                        }
                    }
                    FileType::Directory if retries < MAX_RETRIES => {
                        // It became a directory under us; look again.
                        retries += 1;
                        pending.push_front((name, from_link));
                    }
                    _ if last => return Ok(Loc { hops, missing: Vec::new(), name: Some(name), opened: None }),
                    _ => return Err(WalkError::NotADirectory(name)),
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    // The walk ended on a directory it holds (the root, or one reached through `..`).
    let target = hops.pop().ok_or(WalkError::Outside)?;
    if hops.is_empty() {
        return Ok(Loc { hops: vec![target], missing: Vec::new(), name: None, opened: None });
    }
    Ok(Loc { hops, missing: Vec::new(), name: target.name, opened: Some(target.fd) })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn root() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(real.join("ws/a/b")).unwrap();
        std::fs::write(real.join("ws/a/b/f"), "x").unwrap();
        std::fs::create_dir(real.join("out")).unwrap();
        let root = Root::acquire(&real.join("ws")).unwrap();
        (dir, root)
    }

    fn real(root: &Root, rel: &str, follow: Follow) -> Result<PathBuf, WalkError> {
        walk(root, Path::new(rel), follow).map(|loc| loc.real_path(&root.path))
    }

    #[test]
    fn plain_paths_and_the_root() {
        let (_dir, root) = root();
        assert_eq!(real(&root, "a/b/f", Follow::Final).unwrap(), root.path.join("a/b/f"));
        let loc = walk(&root, Path::new(""), Follow::Final).unwrap();
        assert!(loc.name.is_none() && loc.target_dir().is_some());
        let loc = walk(&root, Path::new("a/b"), Follow::Final).unwrap();
        assert!(loc.target_dir().is_some(), "a directory target is held open");
        let loc = walk(&root, Path::new("a/new/deeper/file"), Follow::Final).unwrap();
        assert_eq!(loc.missing, [OsString::from("new"), OsString::from("deeper")]);
        assert_eq!(loc.name.as_deref(), Some(OsStr::new("file")));
        assert!(matches!(walk(&root, Path::new("a/b/f/x"), Follow::Final), Err(WalkError::NotADirectory(_))));
    }

    #[test]
    fn links_inside_are_followed_and_links_outside_refused() {
        let (_dir, root) = root();
        let ws = root.path.clone();
        symlink("a/b", ws.join("rel")).unwrap();
        symlink(ws.join("a"), ws.join("abs")).unwrap();
        symlink("../out", ws.join("up")).unwrap();
        symlink(ws.parent().unwrap().join("out"), ws.join("abs-out")).unwrap();
        symlink("b/../../a", ws.join("a/loop-back")).unwrap();
        symlink("missing", ws.join("dangling")).unwrap();
        symlink("self", ws.join("self")).unwrap();
        assert_eq!(real(&root, "rel/f", Follow::Final).unwrap(), ws.join("a/b/f"));
        assert_eq!(real(&root, "abs/b/f", Follow::Final).unwrap(), ws.join("a/b/f"));
        assert_eq!(real(&root, "a/loop-back/b", Follow::Final).unwrap(), ws.join("a/b"));
        assert!(matches!(walk(&root, Path::new("up/x"), Follow::Final), Err(WalkError::Outside)));
        assert!(matches!(walk(&root, Path::new("abs-out"), Follow::Final), Err(WalkError::Outside)));
        assert!(matches!(walk(&root, Path::new("dangling"), Follow::Final), Err(WalkError::Dangling)));
        assert!(matches!(walk(&root, Path::new("self"), Follow::Final), Err(WalkError::TooManyLinks)));
        // Not following the final link names the link itself.
        assert_eq!(real(&root, "up", Follow::NoFinal).unwrap(), ws.join("up"));
        // Creating through a linked directory is fine; through a dangling link it is not.
        let loc = walk(&root, Path::new("rel/new"), Follow::Final).unwrap();
        assert_eq!(loc.real_path(&root.path), ws.join("a/b/new"));
        assert!(matches!(walk(&root, Path::new("dangling/new"), Follow::Final), Err(WalkError::Dangling)));
    }
}
