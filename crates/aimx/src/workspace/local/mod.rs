//! The local backend: this host's filesystem and processes.
//!
//! One of the three places in aimx allowed to touch the OS directly (with `ssh` and `server`).
//! Paths arrive lexically confined (see [`crate::authz::confine`], the kernel's decision); this
//! backend enforces it: every path is resolved by walking directory descriptors from one held on
//! the root, without the kernel ever following a symlink ([`walk`]), and the operation acts on
//! those descriptors, so a directory swapped for a symlink mid-request cannot redirect it. Blocking
//! filesystem calls run on tokio's blocking pool.

mod exec;
mod fs;
mod protect;
mod search;
mod walk;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::Caps;
use tokio::sync::Semaphore;

#[cfg(test)]
tokio::task_local! {
    static ROOT_OPEN_HOOK: Arc<(tokio::sync::Barrier, tokio::sync::Barrier)>;
}

#[cfg(test)]
async fn pause_after_root_choice() {
    if let Ok(hook) = ROOT_OPEN_HOOK.try_with(Arc::clone) {
        hook.0.wait().await;
        hook.1.wait().await;
    }
}

use self::exec::LocalExec;
use self::fs::LocalFs;
use self::protect::Protector;
use self::search::LocalSearch;
use self::walk::{Follow, Loc, Root, WalkError};
use super::{Exec, Fs, Outcome, Search, Workspace};
use crate::authz::{Access, Grant, ProtectedPaths};

/// Recognizes the local PTY permit refusal, which occurs before process creation.
pub(crate) fn spawn_admission_refused(err: &ProtoError) -> bool {
    err.code == ErrorCode::LimitExceeded && err.message == exec::PTY_CAPACITY_MESSAGE
}

/// Default bytes of output retained per process.
pub const DEFAULT_OUTPUT_RING_BYTES: usize = 8 * 1024 * 1024;

/// Settings of a local workspace.
#[derive(Clone, Debug)]
pub struct LocalConfig {
    /// Output retained per process for `exec.read` and resume.
    pub output_ring_bytes: usize,
    /// Paths never modified, checked against real (symlink-resolved) locations.
    pub protected: Arc<ProtectedPaths>,
    /// Admission for pty processes (each holds a reader and a waiter thread while it runs);
    /// `None` admits every one.
    pub ptys: Option<Arc<Semaphore>>,
    /// The live-process cap the caller enforces, advertised as `Caps.max_concurrency`.
    pub max_concurrency: Option<u16>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            output_ring_bytes: DEFAULT_OUTPUT_RING_BYTES,
            protected: Arc::new(ProtectedPaths::default()),
            ptys: None,
            max_concurrency: None,
        }
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

/// A local root whose path and identity come from the same held directory descriptor.
#[derive(Debug)]
pub(crate) struct OpenRoot {
    canonical: String,
    root: Root,
}

impl OpenRoot {
    /// The descriptor's actual absolute path, used for authorization and the grant.
    pub(crate) fn path(&self) -> &str {
        &self.canonical
    }

    /// The descriptor shared with the grant, so its authority stays bound to this root.
    pub(crate) fn descriptor(&self) -> Arc<std::os::fd::OwnedFd> {
        Arc::clone(&self.root.fd)
    }
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
        let opened = Self::acquire_root(root).await?;
        #[cfg(test)]
        pause_after_root_choice().await;
        Self::from_open_root(opened, config).await
    }

    /// Acquires a local root once and derives its authorization path from that descriptor.
    pub(crate) async fn acquire_root(root: &str) -> Outcome<OpenRoot> {
        if !root.starts_with('/') {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "workspace root must be an absolute path"));
        }
        let requested = root.to_owned();
        blocking(move || {
            let root = Root::acquire(Path::new(&requested)).map_err(|err| io_error(&err, &requested))?;
            let canonical = path_string(&root.path)?;
            Ok(OpenRoot { canonical, root })
        })
        .await
    }

    /// Builds the backend from the same descriptor already selected for authorization.
    pub(crate) async fn from_open_root(opened: OpenRoot, config: LocalConfig) -> Outcome<Self> {
        let OpenRoot { canonical, root } = opened;
        let protected = config.protected;
        let base =
            blocking(move || Ok(Base { protector: Arc::new(Protector::new(protected, &root.path)), root: Arc::new(root), grant: None }))
                .await?;
        let base = Arc::new(base);
        Ok(Self {
            caps: Caps { max_concurrency: config.max_concurrency, ..local_caps() },
            fs: LocalFs::new(Arc::clone(&base)),
            exec: LocalExec::new(Arc::clone(&base), config.output_ring_bytes, config.ptys),
            search: LocalSearch::new(base),
            root: canonical,
        })
    }
}

#[cfg(test)]
mod root_race_tests {
    use std::os::unix::fs::symlink;

    use aim_proto::error::ErrorCode;

    use super::{LocalConfig, LocalWorkspace, ROOT_OPEN_HOOK, Workspace};

    async fn raced_root(ancestor: bool) {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let root = parent.join("ws");
        let outside_parent = dir.path().join("outside-parent");
        let outside = if ancestor { outside_parent.join("ws") } else { dir.path().join("outside") };
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("secret"), "inside").unwrap();
        std::fs::write(outside.join("secret"), "OUTSIDE-SENTINEL").unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap().to_str().unwrap().to_owned();
        let hook = std::sync::Arc::new((tokio::sync::Barrier::new(2), tokio::sync::Barrier::new(2)));
        let opening = ROOT_OPEN_HOOK.scope(std::sync::Arc::clone(&hook), LocalWorkspace::open(&canonical, LocalConfig::default()));
        let swapping = async {
            hook.0.wait().await;
            if ancestor {
                std::fs::rename(&parent, dir.path().join("parked-parent")).unwrap();
                symlink(&outside_parent, &parent).unwrap();
            } else {
                std::fs::rename(&root, dir.path().join("parked-root")).unwrap();
                symlink(&outside, &root).unwrap();
            }
            hook.1.wait().await;
        };
        let (opened, ()) = tokio::join!(opening, swapping);
        if let Ok(workspace) = opened {
            assert_eq!(workspace.root(), canonical);
            let path = format!("{canonical}/secret");
            match workspace.fs().read(&path, None, 1024, true).await {
                Ok(read) => assert_eq!(read.content.into_bytes(), b"inside", "the held root descriptor reached the outside sentinel"),
                Err(err) => assert!(matches!(err.code, ErrorCode::Denied | ErrorCode::NotFound | ErrorCode::Conflict)),
            }
        }
    }

    #[tokio::test]
    async fn root_swap_after_authorization_cannot_rebind_backend() {
        raced_root(false).await;
    }

    #[tokio::test]
    async fn ancestor_swap_after_authorization_cannot_rebind_backend() {
        raced_root(true).await;
    }
}

#[cfg(test)]
mod scope_tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    use aim_kernel::policy::Limits as PolicyLimits;
    use aim_proto::content::Content;
    use aim_proto::error::ErrorCode;
    use aim_proto::harness::{CallScope, CaseMode, Command, Precondition};
    use aim_proto::ids::IdempotencyKey;

    use super::{LocalConfig, LocalWorkspace, Workspace};
    use crate::authz::{Grant, Principal, ProtectedPaths};
    use crate::workspace::{GlobQuery, GrepQuery, SpawnSpec, WriteRequest};

    #[tokio::test]
    async fn scoped_backend_checks_resolved_target_for_every_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("allowed")).unwrap();
        std::fs::create_dir_all(root.join("private")).unwrap();
        std::fs::write(root.join("private/secret"), "PRIVATE-SENTINEL").unwrap();
        symlink("../private", root.join("allowed/link")).unwrap();
        let workspace = LocalWorkspace::open(root.to_str().unwrap(), LocalConfig::default()).await.unwrap();
        let root = workspace.root().to_owned();
        let principal = Arc::new(Principal { id: "test".into(), roots: vec![root.clone()], read_only: false });
        let grant = Grant::new(principal, Arc::new(ProtectedPaths::default()), root.clone(), None)
            .scoped(
                None,
                Some(&CallScope {
                    roots: vec!["allowed".into()],
                    ops: vec!["read".into(), "write".into(), "exec".into()],
                    deny_write: Vec::new(),
                    max_processes: None,
                    max_output_bytes: None,
                }),
                PolicyLimits { max_processes: 10, max_output_bytes: 1024 },
            )
            .unwrap();
        let scoped = workspace.scoped(grant).unwrap();
        let link = format!("{root}/allowed/link");
        let target = format!("{link}/secret");
        let allowed = format!("{root}/allowed");
        let key = IdempotencyKey::new("scope-test");

        assert_eq!(scoped.fs().read(&target, None, 1024, false).await.unwrap_err().code, ErrorCode::Denied);
        assert_eq!(
            scoped
                .fs()
                .write(WriteRequest {
                    path: &target,
                    content: &Content::from_bytes(b"overwrite".to_vec()),
                    precondition: &Precondition::Any,
                    create_dirs: false,
                    key: &key,
                })
                .await
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            scoped
                .search()
                .grep(GrepQuery {
                    pattern: "PRIVATE-SENTINEL",
                    path: &link,
                    globs: &[],
                    case: CaseMode::Sensitive,
                    fixed_strings: true,
                    context: 0,
                    max_matches: 10,
                })
                .await
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            scoped.search().glob(GlobQuery { patterns: &["*".into()], path: &link, max_results: 10 }).await.unwrap_err().code,
            ErrorCode::Denied
        );
        assert_eq!(
            scoped
                .exec()
                .unwrap()
                .spawn(SpawnSpec {
                    command: &Command::Shell { script: "true".into() },
                    cwd: &link,
                    env: &BTreeMap::new(),
                    pty: None,
                    stdin: false,
                    timeout: None,
                    key: &key,
                })
                .await
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(std::fs::read(std::path::Path::new(&root).join("private/secret")).unwrap(), b"PRIVATE-SENTINEL");
        assert!(
            scoped
                .fs()
                .list(crate::workspace::ListRequest { path: &allowed, limit: 10, page_token: None, include_hidden: false })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn dropping_a_scoped_view_keeps_its_process_available() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = LocalWorkspace::open(dir.path().to_str().unwrap(), LocalConfig::default()).await.unwrap();
        let root = workspace.root().to_owned();
        let principal = Arc::new(Principal { id: "test".into(), roots: vec![root.clone()], read_only: false });
        let grant = Grant::new(principal, Arc::new(ProtectedPaths::default()), root.clone(), None);
        let scoped = workspace.scoped(grant).unwrap();
        let key = IdempotencyKey::new("scope-process-test");
        let proc = scoped
            .exec()
            .unwrap()
            .spawn(SpawnSpec {
                command: &Command::Shell { script: "printf shared".into() },
                cwd: &root,
                env: &BTreeMap::new(),
                pty: None,
                stdin: false,
                timeout: None,
                key: &key,
            })
            .await
            .unwrap();
        drop(scoped);
        let read = workspace.exec().unwrap().read(&proc, 0, 1024, std::time::Duration::from_secs(2)).await.unwrap();
        let output: Vec<u8> = read.chunks.into_iter().flat_map(|chunk| chunk.data.into_bytes()).collect();
        assert_eq!(output, b"shared");
        workspace.exec().unwrap().release(&proc).await.unwrap();
    }
}

/// Canonicalises a workspace root: absolute, symlink-free, an existing directory.
///
/// # Errors
/// As [`LocalWorkspace::open`].
pub async fn canonical_root(root: &str) -> Outcome<String> {
    LocalWorkspace::acquire_root(root).await.map(|opened| opened.canonical)
}

/// What this host's backend can do (with no concurrency cap; the server sets its own).
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
    fn scoped(&self, grant: Grant) -> Option<Arc<dyn Workspace>> {
        if grant.root() != self.root {
            return None;
        }
        let protector = if grant.write_denies().is_empty() {
            Arc::clone(&self.fs.base.protector)
        } else {
            let protected = Arc::new(grant.protected().as_ref().clone().with(grant.write_denies().iter().cloned()));
            Arc::new(Protector::new(protected, &self.fs.base.root.path))
        };
        let base = Arc::new(Base { root: Arc::clone(&self.fs.base.root), protector, grant: Some(grant) });
        Some(Arc::new(Self {
            root: self.root.clone(),
            caps: self.caps.clone(),
            fs: self.fs.scoped(Arc::clone(&base)),
            exec: self.exec.scoped(Arc::clone(&base)),
            search: LocalSearch::new(base),
        }))
    }

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

/// What every part of the backend shares: the root (held open) and the protected-path check.
#[derive(Debug)]
struct Base {
    root: Arc<Root>,
    protector: Arc<Protector>,
    grant: Option<Grant>,
}

#[derive(Clone, Copy)]
enum Authority {
    Path(Access),
    Exec,
}

impl Base {
    /// Resolves a lexically confined path by walking descriptors from the root ([`walk`]):
    /// symlinks inside the root are followed (the final one only with [`Follow::Final`]), anything
    /// leading out of it or dangling is `denied`, and missing trailing components are allowed for
    /// creation.
    fn resolve(&self, path: &str, follow: Follow, authority: Authority) -> Outcome<Loc> {
        let relative_path = Path::new(path).strip_prefix(&self.root.path).map_err(|_| outside(path))?;
        let loc = walk::walk(&self.root, relative_path, follow).map_err(|err| match err {
            WalkError::Outside => outside(path),
            WalkError::Dangling => ProtoError::new(ErrorCode::Denied, format!("`{path}` passes through a dangling symlink")),
            WalkError::TooManyLinks => {
                ProtoError::new(ErrorCode::Denied, format!("`{path}` passes through more than {} symlinks", walk::MAX_LINKS))
            }
            WalkError::NotADirectory(name) => {
                ProtoError::new(ErrorCode::Conflict, format!("`{path}`: `{}` is not a directory", name.to_string_lossy()))
            }
            WalkError::Io(err) => io_error(&err, path),
        })?;
        if let Some(grant) = &self.grant {
            let resolved_path = path_string(&self.real(&loc))?;
            match authority {
                Authority::Path(access) => {
                    grant.path(&resolved_path, access)?;
                }
                Authority::Exec => {
                    grant.exec_path(&resolved_path)?;
                }
            }
        }
        Ok(loc)
    }

    /// The real path a resolution arrived at.
    fn real(&self, loc: &Loc) -> PathBuf {
        loc.real_path(&self.root.path)
    }

    /// Refuses a mutation of a resolved target that is (or lies inside, or for `tree` operations
    /// contains) a protected path, compared by filesystem identity ([`protect`]). A check that
    /// cannot be made refuses too.
    fn check_protected(&self, loc: &Loc, tree: bool) -> Outcome<()> {
        let real = self.real(loc);
        match self.protector.hit(loc, tree) {
            Ok(None) => Ok(()),
            Ok(Some(protected)) => {
                Err(ProtoError::new(ErrorCode::Denied, format!("`{}` is the protected path `{protected}` (or affects it)", real.display())))
            }
            Err(err) => {
                Err(ProtoError::new(ErrorCode::Denied, format!("cannot check `{}` against the protected paths: {err}", real.display())))
            }
        }
    }

    /// `real` relative to the root, for results (`""` for the root).
    fn relative(&self, real: &Path) -> String {
        real.strip_prefix(&self.root.path).map(|rel| rel.to_string_lossy().into_owned()).unwrap_or_default()
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
    // `ELOOP` from an `O_NOFOLLOW` open: the entry became a symlink after it was resolved.
    if err.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
        return ProtoError::new(ErrorCode::Conflict, format!("`{path}` changed while it was opened (it is now a symlink); retry"));
    }
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
