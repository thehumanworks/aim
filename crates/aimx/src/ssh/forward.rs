//! Local aimx stdio entry for resident SSH and agentless fallback.

use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::{Command as SyncCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;

use aim_proto::HARNESS_GENERATIONS;

use super::agentless::AgentlessWorkspace;
use super::bootstrap::{self, Artifact};
use super::conn::{Connection, Prompter, SshOptions};
use super::quote;
use crate::authz::{Principal, ProtectedPaths};
use crate::server::{Server, ServerConfig};
use crate::workspace::Workspace;

/// Options of `aimx serve --stdio --ssh`.
#[derive(Clone, Debug)]
pub struct ForwardOptions {
    /// Destination resolved by OpenSSH.
    pub destination: String,
    /// Workspace path on that host.
    pub root: PathBuf,
    /// Whether to install and use a resident binary.
    pub bootstrap: bool,
    /// Optional caller-verified binary.
    pub artifact: Option<PathBuf>,
    /// Expected digest of `artifact`.
    pub sha256: Option<String>,
    /// Optional OpenSSH configuration file.
    pub ssh_config: Option<PathBuf>,
    /// Remote resident idle shutdown delay.
    pub idle: Duration,
}

pub(super) struct VerifiedBinary {
    pub(super) path: String,
    pub(super) sha256: String,
}

struct TtyPrompter;

impl Prompter for TtyPrompter {
    fn prompt(&self, text: &str, echo: bool) -> Option<String> {
        let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
        write!(tty, "{text} ").ok()?;
        tty.flush().ok()?;
        let mut guard = if echo { None } else { Some(EchoGuard::disable(&tty)?) };
        let mut line = String::new();
        let read = std::io::BufReader::new(tty.try_clone().ok()?).read_line(&mut line).ok()?;
        if let Some(guard) = guard.take() {
            drop(guard);
        }
        if read == 0 {
            return None;
        }
        Some(line.trim_end_matches(['\n', '\r']).to_owned())
    }
}

struct EchoGuard {
    tty: File,
}

impl EchoGuard {
    fn disable(tty: &File) -> Option<Self> {
        let clone = tty.try_clone().ok()?;
        if !SyncCommand::new("stty").arg("-echo").stdin(Stdio::from(clone)).status().ok()?.success() {
            return None;
        }
        Some(Self { tty: tty.try_clone().ok()? })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Ok(tty) = self.tty.try_clone() {
            drop(SyncCommand::new("stty").arg("echo").stdin(Stdio::from(tty)).status());
        }
    }
}

struct AskpassScript {
    path: PathBuf,
}

impl AskpassScript {
    fn create() -> Result<Self, String> {
        let home = std::env::var_os("HOME").ok_or("HOME is unset")?;
        let dir = PathBuf::from(home).join(".aim/ssh");
        Self::create_in(&dir)
    }

    fn create_in(dir: &std::path::Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
        let metadata = std::fs::symlink_metadata(dir).map_err(|err| err.to_string())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != rustix::process::getuid().as_raw() {
            return Err("askpass directory is not private".to_owned());
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|err| err.to_string())?;
        let path = dir.join(format!("askpass-{}.sh", crate::id::random_hex()));
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o700).open(&path).map_err(|err| err.to_string())?;
        let script = Self { path };
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        write!(file, "#!/bin/sh\nexec {} askpass \"$@\"\n", quote(&exe.to_string_lossy())).map_err(|err| err.to_string())?;
        file.flush().map_err(|err| err.to_string())?;
        Ok(script)
    }
}

impl Drop for AskpassScript {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

/// Connect by SSH, prefer the verified resident, and serve agentlessly when it is unavailable.
///
/// # Errors
/// Returns an error when SSH itself or both serving modes fail.
pub async fn serve_ssh(options: ForwardOptions) -> Result<(), String> {
    let connection = connect_ssh(&options).await?;
    if options.bootstrap {
        match remote_binary(&connection, &options).await {
            Ok(binary) => return super::reconnect::relay(&options, connection, binary).await,
            Err(err) => tracing::warn!(%err, "remote bootstrap unavailable; using agentless SSH"),
        }
    }
    serve_agentless(connection, &options).await
}

pub(super) async fn connect_ssh(options: &ForwardOptions) -> Result<Connection, String> {
    let askpass = AskpassScript::create()?;
    let mut ssh = SshOptions::new(&options.destination);
    ssh.askpass_program = Some(askpass.path.clone());
    ssh.config_file = options.ssh_config.clone();
    let connection = Connection::connect(ssh, Some(Arc::new(TtyPrompter))).await.map_err(|err| err.to_string())?;
    drop(askpass);
    Ok(connection)
}

/// Whether a binary built for this process's OS and CPU runs on `target` (a triple from
/// [`bootstrap::target_for_uname`]). Linux C-library differences are not checked here: the
/// resident install then fails and the caller falls back to agentless SSH.
fn runs_here(target: &str) -> bool {
    let os = if target.contains("-apple-darwin") {
        "macos"
    } else if target.contains("-linux") {
        "linux"
    } else {
        return false;
    };
    target.split('-').next() == Some(std::env::consts::ARCH) && os == std::env::consts::OS
}

pub(super) async fn remote_binary(connection: &Connection, options: &ForwardOptions) -> Result<VerifiedBinary, String> {
    let probe = bootstrap::probe(connection).await.map_err(|err| format!("probe: {err:?}"))?;
    let target = probe.target.ok_or("unsupported SSH host")?;
    // Without an explicit artifact, the only binary on hand is this aimx: offer it only to a host
    // of the same OS and CPU. A macOS aimx must never be installed on a Linux host (it would
    // fail to execute there instead of falling back to agentless SSH).
    if options.artifact.is_none() && !runs_here(target) {
        return Err(format!(
            "no aimx build for {target} (this aimx is {}-{}); pass --artifact",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }
    let path = options.artifact.clone().map_or_else(std::env::current_exe, Ok).map_err(|err| err.to_string())?;
    let sha256 = if let Some(hash) = &options.sha256 {
        hash.clone()
    } else {
        bootstrap::local_sha256(&path).map_err(|err| format!("hash: {err:?}"))?
    };
    let artifact = Artifact {
        path,
        sha256,
        target: target.to_owned(),
        generation: HARNESS_GENERATIONS.1,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let binary = bootstrap::install(connection, &artifact, false).await.map_err(|err| format!("install: {err:?}"))?;
    connection.run(&format!("{} version >/dev/null", quote(&binary)), &[]).await.map_err(|err| err.to_string())?;
    Ok(VerifiedBinary { path: binary, sha256: artifact.sha256 })
}

async fn serve_agentless(connection: Connection, options: &ForwardOptions) -> Result<(), String> {
    let root = options.root.to_str().ok_or("remote root is not UTF-8")?;
    let workspace = AgentlessWorkspace::open(connection.clone(), root).await.map_err(|err| err.to_string())?;
    let platform = bootstrap::probe(&connection).await.map_err(|err| format!("probe: {err:?}"))?;
    let root = workspace.root().to_owned();
    let principal = Principal { id: format!("ssh:{}", options.destination), roots: vec![root], read_only: false, ceiling: None };
    let protected = ProtectedPaths::defaults(&platform.home, None);
    let server = Server::new_agentless(ServerConfig::new(principal, protected), Arc::new(workspace));
    server.serve_stdio().await;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_a_host_of_this_os_and_cpu_gets_this_aimx() {
        let here = format!("{}-{}", std::env::consts::ARCH, if cfg!(target_os = "macos") { "apple-darwin" } else { "unknown-linux-musl" });
        assert!(super::runs_here(&here));
        assert!(!super::runs_here(if cfg!(target_os = "macos") { "aarch64-unknown-linux-musl" } else { "aarch64-apple-darwin" }));
        assert!(!super::runs_here(if cfg!(target_os = "macos") { "x86_64-unknown-linux-musl" } else { "x86_64-apple-darwin" }));
        let other_arch = if std::env::consts::ARCH == "aarch64" { "x86_64" } else { "aarch64" };
        assert!(!super::runs_here(&here.replacen(std::env::consts::ARCH, other_arch, 1)));
        assert!(!super::runs_here("riscv64-unknown-freebsd"));
    }

    use std::os::unix::fs::symlink;

    use super::AskpassScript;

    #[test]
    fn askpass_script_does_not_follow_existing_symlink() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let target = dir.path().join("sentinel");
        std::fs::write(&target, "keep").expect("write sentinel");
        symlink(&target, dir.path().join(format!("askpass-{}.sh", std::process::id()))).expect("create old predictable link");
        let script = AskpassScript::create_in(dir.path()).expect("create askpass");
        assert_ne!(script.path, dir.path().join(format!("askpass-{}.sh", std::process::id())));
        assert_eq!(std::fs::read_to_string(target).expect("read sentinel"), "keep");
    }

    #[test]
    fn askpass_rejects_symlinked_directory() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let actual = dir.path().join("actual");
        std::fs::create_dir(&actual).expect("create directory");
        let link = dir.path().join("link");
        symlink(actual, &link).expect("create directory symlink");
        assert!(AskpassScript::create_in(&link).is_err());
    }
}
