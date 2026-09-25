//! The local backend: this host's filesystem and processes.
//!
//! One of the three places in aimx allowed to touch the OS directly (with `ssh` and `server`).
//! Paths arrive lexically confined (see [`crate::authz::confine`]); this backend additionally
//! resolves symlinks on every existing ancestor (and on the final component for operations that
//! follow it) and refuses any path whose real location leaves the workspace root. Blocking
//! filesystem calls run on tokio's blocking pool.
//!
//! Known limit: resolution and the operation are separate system calls, so a concurrent process
//! that swaps a directory for a symlink in between could still redirect one operation
//! (`openat2(RESOLVE_BENEATH)`/`O_NOFOLLOW` walking would close this; ADR 0008 lists it as a shell
//! assumption).

mod exec;
mod fs;
mod search;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::Caps;

use self::exec::LocalExec;
use self::fs::LocalFs;
use self::search::LocalSearch;
use super::{Exec, Fs, Outcome, Search, Workspace};
use crate::authz::ProtectedPaths;

/// Default bytes of output retained per process.
pub const DEFAULT_OUTPUT_RING_BYTES: usize = 8 * 1024 * 1024;

/// Settings of a local workspace.
#[derive(Clone, Debug)]
pub struct LocalConfig {
    /// Output retained per process for `exec.read` and resume.
    pub output_ring_bytes: usize,
    /// Paths never modified, checked against real (symlink-resolved) locations.
    pub protected: Arc<ProtectedPaths>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self { output_ring_bytes: DEFAULT_OUTPUT_RING_BYTES, protected: Arc::new(ProtectedPaths::default()) }
    }
}

/// A workspace on this host.
pub struct LocalWorkspace {
    root: String,
    caps: Caps,
    fs: LocalFs,
    exec: LocalExec,
    search: LocalSearch,
}

impl std::fmt::Debug for LocalWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalWorkspace").field("root", &self.root).finish_non_exhaustive()
    }
}

impl LocalWorkspace {
    /// Opens the workspace rooted at the existing directory `root` (canonicalised).
    ///
    /// # Errors
    /// `invalid_params` for a relative root, `not_found` when it does not exist, `conflict` when it
    /// is not a directory.
    pub async fn open(root: &str, config: LocalConfig) -> Outcome<Self> {
        let canonical = canonical_root(root).await?;
        let base = Arc::new(Base { root: PathBuf::from(&canonical), protected: config.protected });
        Ok(Self {
            caps: local_caps(),
            fs: LocalFs::new(Arc::clone(&base)),
            exec: LocalExec::new(Arc::clone(&base), config.output_ring_bytes),
            search: LocalSearch::new(base),
            root: canonical,
        })
    }
}

/// Canonicalises a workspace root: absolute, symlink-free, an existing directory.
///
/// # Errors
/// As [`LocalWorkspace::open`].
pub async fn canonical_root(root: &str) -> Outcome<String> {
    if !root.starts_with('/') {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "workspace root must be an absolute path"));
    }
    let root = root.to_owned();
    blocking(move || {
        let canonical = std::fs::canonicalize(&root).map_err(|err| io_error(&err, &root))?;
        let meta = std::fs::metadata(&canonical).map_err(|err| io_error(&err, &root))?;
        if !meta.is_dir() {
            return Err(ProtoError::new(ErrorCode::Conflict, format!("`{root}` is not a directory")));
        }
        path_string(&canonical)
    })
    .await
}

/// What this host's backend can do.
#[must_use]
pub fn local_caps() -> Caps {
    Caps {
        exec: true,
        pty: true,
        watch: false,
        native_search: true,
        atomic_rename: true,
        resumable: true,
        max_concurrency: None,
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        shell: Some("sh".to_owned()),
    }
}

impl Workspace for LocalWorkspace {
    fn caps(&self) -> &Caps {
        &self.caps
    }

    fn root(&self) -> &str {
        &self.root
    }

    fn fs(&self) -> &dyn Fs {
        &self.fs
    }

    fn exec(&self) -> Option<&dyn Exec> {
        Some(&self.exec)
    }

    fn search(&self) -> &dyn Search {
        &self.search
    }
}

/// What every part of the backend shares: the canonical root and the protected set.
#[derive(Debug)]
struct Base {
    root: PathBuf,
    protected: Arc<ProtectedPaths>,
}

/// Whether to resolve a symlink in the final path component.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Follow {
    /// Operate on the link's target (read, write, list, search).
    Final,
    /// Operate on the link itself (stat, remove, rename).
    NoFinal,
}

impl Base {
    /// Resolves a lexically confined path to its real location, refusing anything outside the
    /// root. Missing trailing components are allowed (for creation); a dangling symlink on the way
    /// is refused, since its target cannot be checked.
    fn resolve(&self, path: &str, follow: Follow) -> Outcome<PathBuf> {
        let lexical = Path::new(path);
        if !lexical.starts_with(&self.root) {
            return Err(outside(path));
        }
        if lexical == self.root {
            return Ok(self.root.clone());
        }
        let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
            return Err(outside(path));
        };
        let real_parent = self.real_prefix(parent, path)?;
        let candidate = real_parent.join(name);
        if follow == Follow::Final
            && let Ok(meta) = std::fs::symlink_metadata(&candidate)
            && meta.file_type().is_symlink()
        {
            return match std::fs::canonicalize(&candidate) {
                Ok(target) if target.starts_with(&self.root) => Ok(target),
                Ok(_) => Err(outside(path)),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    Err(ProtoError::new(ErrorCode::Denied, format!("`{path}` is a dangling symlink")))
                }
                Err(err) => Err(io_error(&err, path)),
            };
        }
        Ok(candidate)
    }

    /// The real location of directory `dir`: its deepest existing ancestor canonicalised, plus
    /// the missing components.
    fn real_prefix(&self, dir: &Path, display: &str) -> Outcome<PathBuf> {
        let mut existing = dir.to_path_buf();
        let mut missing: Vec<OsString> = Vec::new();
        loop {
            match std::fs::canonicalize(&existing) {
                Ok(real) => {
                    if !real.starts_with(&self.root) {
                        return Err(outside(display));
                    }
                    let mut out = real;
                    for component in missing.iter().rev() {
                        out.push(component);
                    }
                    return Ok(out);
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    if std::fs::symlink_metadata(&existing).is_ok() {
                        return Err(ProtoError::new(ErrorCode::Denied, format!("`{display}` passes through a dangling symlink")));
                    }
                    match (existing.file_name(), existing.parent()) {
                        (Some(name), Some(parent)) => {
                            missing.push(name.to_owned());
                            existing = parent.to_path_buf();
                        }
                        _ => return Err(ProtoError::new(ErrorCode::NotFound, format!("`{display}` does not exist"))),
                    }
                }
                Err(err) => return Err(io_error(&err, display)),
            }
        }
    }

    /// Refuses mutations whose real location is protected.
    fn check_protected(&self, real: &Path, tree: bool) -> Outcome<()> {
        let real = path_string(real)?;
        let hit = if tree { self.protected.guards_tree(&real) } else { self.protected.guards(&real) };
        if hit { Err(ProtoError::new(ErrorCode::Denied, format!("`{real}` is a protected path"))) } else { Ok(()) }
    }

    /// `real` relative to the root, for results (`""` for the root).
    fn relative(&self, real: &Path) -> String {
        real.strip_prefix(&self.root).map(|rel| rel.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

fn outside(path: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Denied, format!("`{path}` resolves outside the workspace root"))
}

/// A path as UTF-8 (paths on the wire are strings).
fn path_string(path: &Path) -> Outcome<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "path is not valid UTF-8"))
}

/// Maps an I/O error on `path` to a protocol error.
fn io_error(err: &io::Error, path: &str) -> ProtoError {
    let code = match err.kind() {
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => ErrorCode::Denied,
        io::ErrorKind::AlreadyExists
        | io::ErrorKind::NotADirectory
        | io::ErrorKind::IsADirectory
        | io::ErrorKind::DirectoryNotEmpty
        | io::ErrorKind::CrossesDevices
        | io::ErrorKind::InvalidFilename => ErrorCode::Conflict,
        io::ErrorKind::TimedOut => ErrorCode::Timeout,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded | io::ErrorKind::FileTooLarge => ErrorCode::LimitExceeded,
        _ => ErrorCode::Internal,
    };
    ProtoError::new(code, format!("`{path}`: {err}"))
}

/// Runs blocking work on tokio's blocking pool.
async fn blocking<T, F>(work: F) -> Outcome<T>
where
    T: Send + 'static,
    F: FnOnce() -> Outcome<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|err| Err(ProtoError::new(ErrorCode::Internal, format!("blocking task failed: {err}"))))
}
