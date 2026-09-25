//! Resident harness lifecycle and the byte-for-byte SSH proxy.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncWriteExt as _, copy};
use tokio::net::UnixStream;

use crate::server::{Server, ServerConfig, default_protected, local_principal};

/// Paths of one resident server, keyed by its canonical workspace root.
#[derive(Clone, Debug)]
pub struct ResidentPaths {
    /// Canonical workspace root.
    pub root: PathBuf,
    /// Private Unix socket.
    pub socket: PathBuf,
    /// PID file.
    pub pid: PathBuf,
    /// Exclusive startup and lifetime lock.
    pub lock: PathBuf,
}

/// Canonicalize a root and derive stable private run paths.
///
/// # Errors
/// Returns an I/O error when the root or private run directory is unusable.
pub fn paths(root: &Path) -> io::Result<ResidentPaths> {
    let root = std::fs::canonicalize(root)?;
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
    let directory = PathBuf::from(home).join(".aim/run");
    private_dir(&directory)?;
    let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    let prefix = hash.get(..16).ok_or_else(|| io::Error::other("invalid root hash"))?;
    Ok(ResidentPaths {
        root,
        socket: directory.join(format!("aimx-{prefix}.sock")),
        pid: directory.join(format!("aimx-{prefix}.pid")),
        lock: directory.join(format!("aimx-{prefix}.lock")),
    })
}

fn private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != rustix::process::getuid().as_raw() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "resident run directory is not private"));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).open(path)
}

/// Launch a detached resident if one is not already answering on its socket.
///
/// # Errors
/// Returns an I/O error if the daemon cannot start or answer before the startup deadline.
pub async fn ensure(root: &Path, idle: Duration) -> io::Result<ResidentPaths> {
    let paths = paths(root)?;
    if UnixStream::connect(&paths.socket).await.is_ok() {
        return Ok(paths);
    }
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable);
    child
        .args(["serve", "--resident-child", "--root"])
        .arg(&paths.root)
        .arg("--idle-secs")
        .arg(idle.as_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let _child = child.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if UnixStream::connect(&paths.socket).await.is_ok() {
            return Ok(paths);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "resident aimx did not start"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Serve the resident Unix socket until it has been idle for `idle`.
///
/// # Errors
/// Returns an I/O error if the lock, socket or listener fails.
pub async fn serve_child(root: &Path, idle: Duration) -> io::Result<()> {
    let paths = paths(root)?;
    let lock = open_lock(&paths.lock)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(err)) => return Err(err),
    }
    let mut pid = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&paths.pid)?;
    writeln!(pid, "{}", std::process::id())?;
    let home = std::env::var("HOME").map_err(|_| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
    let principal = local_principal(&[&paths.root], false)?;
    let server = Server::new(ServerConfig::new(principal, default_protected(&home)));
    let listener = Server::bind_unix(&paths.socket)?;
    let idle_watch = async {
        let mut since = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if server.idle().await {
                if since.elapsed() >= idle {
                    break;
                }
            } else {
                since = Instant::now();
            }
        }
    };
    let outcome = tokio::select! {
        result = server.serve_listener(listener) => result,
        () = idle_watch => Ok(()),
    };
    server.shutdown().await;
    drop(std::fs::remove_file(&paths.socket));
    drop(std::fs::remove_file(&paths.pid));
    drop(lock);
    outcome
}

/// Copy bytes between process stdio and one resident Unix socket.
///
/// # Errors
/// Returns an I/O error if startup, connection or copying fails.
pub async fn proxy(root: &Path, idle: Duration) -> io::Result<()> {
    let paths = ensure(root, idle).await?;
    let stream = UnixStream::connect(&paths.socket).await?;
    let (mut reader, mut writer) = stream.into_split();
    let incoming = async {
        copy(&mut tokio::io::stdin(), &mut writer).await?;
        writer.shutdown().await
    };
    let outgoing = async { copy(&mut reader, &mut tokio::io::stdout()).await.map(|_| ()) };
    tokio::try_join!(incoming, outgoing)?;
    Ok(())
}
