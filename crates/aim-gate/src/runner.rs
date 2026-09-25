//! Fresh checkout and confined command execution. No candidate command inherits gate credentials.

use std::fs;
use std::io::{ErrorKind, Read as _};
use std::net::TcpListener;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use rustix::process::{Pid, Signal, kill_process_group};

use crate::formats::{CommandSpec, GateConfig};
use crate::sandbox::{Network, Policy};

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_mins(15);

/// Output captured by the trusted parent from both child streams in write order.
#[derive(Debug)]
pub struct CommandResult {
    /// Combined stdout/stderr, bounded to 4 MiB.
    pub output: String,
}

fn drain(mut reader: UnixStream, leader_done: Arc<AtomicBool>) -> Result<String> {
    reader.set_read_timeout(Some(Duration::from_millis(250))).context("set sandbox output deadline")?;
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(err) if matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                ensure!(!leader_done.load(Ordering::Acquire), "sandbox descendant retained an output channel after leader exit");
                continue;
            }
            Err(err) => return Err(err).context("read sandbox output"),
        };
        if count == 0 {
            break;
        }
        if output.len() < OUTPUT_LIMIT {
            let keep = (OUTPUT_LIMIT - output.len()).min(count);
            let selected = chunk.get(..keep).context("output slice exceeds chunk")?;
            output.extend_from_slice(selected);
        }
    }
    ensure!(output.len() < OUTPUT_LIMIT, "sandbox output exceeds capture limit");
    String::from_utf8(output).context("sandbox output is not UTF-8")
}

fn complete(mut command: Command, timeout: Duration) -> Result<CommandResult> {
    let (reader, writer) = UnixStream::pair().context("create sandbox output channel")?;
    let stderr = writer.try_clone().context("clone sandbox output channel")?;
    command.stdout(Stdio::from(OwnedFd::from(writer))).stderr(Stdio::from(OwnedFd::from(stderr)));
    command.process_group(0);
    let mut child = command.spawn().context("spawn sandbox command")?;
    let leader_done = Arc::new(AtomicBool::new(false));
    let output_done = Arc::clone(&leader_done);
    let output = thread::spawn(move || drain(reader, output_done));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("poll sandbox command")? {
            break status;
        }
        if Instant::now() >= deadline {
            if let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
                let _signal = kill_process_group(pid, Signal::KILL);
            }
            let _status = child.wait().context("reap timed-out sandbox command")?;
            leader_done.store(true, Ordering::Release);
            let _drained = output.join().map_err(|_| anyhow::anyhow!("sandbox output reader failed"))?;
            bail!("sandbox command timed out");
        }
        thread::sleep(Duration::from_millis(25));
    };
    leader_done.store(true, Ordering::Release);
    let captured = output.join().map_err(|_| anyhow::anyhow!("sandbox output reader failed"))??;
    ensure!(status.success(), "sandbox command failed with {status}");
    Ok(CommandResult { output: captured })
}

fn protected_homes() -> Vec<PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    [".aim-gate", ".codex", ".aim", ".ssh", ".config"].into_iter().map(|name| PathBuf::from(&home).join(name)).collect()
}

/// A concrete sandbox instance for one fresh clone.
#[derive(Debug)]
pub struct Sandbox {
    root: PathBuf,
    profile: String,
    broker_port: Option<u16>,
}

impl Sandbox {
    /// Build an explicit deny-by-default profile. The clone and target must lie outside the
    /// gate home; every supplied cache/toolchain path must be pre-canonicalized by the caller.
    ///
    /// # Errors
    /// Missing clone, overlap with gate home, or invalid profile.
    pub fn new(root: &Path, gate_home: &Path, config: &GateConfig, network: Network) -> Result<Self> {
        let root = fs::canonicalize(root).context("sandbox clone missing")?;
        let gate_home = fs::canonicalize(gate_home).context("gate home missing")?;
        ensure!(!root.starts_with(&gate_home) && !gate_home.starts_with(&root), "clone overlaps gate home");
        let mut readable = vec![PathBuf::from("/System"), PathBuf::from("/usr"), PathBuf::from("/dev/null"), PathBuf::from("/dev/urandom"), root.clone()];
        readable.push(fs::canonicalize(&config.evaluator_root).context("canonicalize evaluator root")?);
        readable.extend(
            config
                .readable_roots
                .iter()
                .map(fs::canonicalize)
                .collect::<std::io::Result<Vec<_>>>()
                .context("canonicalize toolchain roots")?,
        );
        let mut denied = protected_homes();
        denied.push(gate_home);
        let profile = Policy { readable, writable: vec![root.clone()], denied, network }.render().map_err(anyhow::Error::msg)?;
        let broker_port = if let Network::LoopbackPort { port, .. } = network { Some(port) } else { None };
        Ok(Self { root, profile, broker_port })
    }

    /// Run an argv command with no inherited environment, no provider key, offline Cargo, and a
    /// candidate-local HOME/TMPDIR/target. The profile covers all descendants.
    ///
    /// # Errors
    /// Sandbox rejection, timeout, nonzero exit or oversized output.
    pub fn run(&self, spec: &CommandSpec) -> Result<CommandResult> {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(&self.profile).arg(&spec.program).args(&spec.args).current_dir(&self.root);
        let home = self.root.join(".gate-home");
        let temp = self.root.join(".gate-tmp");
        let target = self.root.join("target");
        for dir in [&home, &temp, &target] {
            fs::create_dir_all(dir).context("create clone-local command directory")?;
        }
        command.env_clear();
        command.env("HOME", &home);
        command.env("TMPDIR", &temp);
        command.env("CARGO_TARGET_DIR", &target);
        command.env("CARGO_NET_OFFLINE", "true");
        command.env("CARGO_INCREMENTAL", "0");
        command.env("CARGO_PROFILE_DEV_DEBUG", "0");
        command.env("MISE_YES", "1");
        command.env("MISE_CONFIG_DIR", self.root.join(".gate-mise"));
        command.env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()));
        if let Some(port) = self.broker_port {
            command.env("AIM_GATE_BENCH_PORT", port.to_string());
        }
        complete(command, COMMAND_TIMEOUT)
    }
}

/// Run a trusted gate-side command without candidate environment inheritance.
///
/// # Errors
/// Command failure or timeout.
pub fn trusted(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env_clear();
    command.env("PATH", "/usr/bin:/bin:/opt/homebrew/bin");
    command.env("HOME", "/var/empty");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_TERMINAL_PROMPT", "0");
    Ok(complete(command, COMMAND_TIMEOUT)?.output.trim().to_owned())
}

/// Require an exact Git object id, avoiding option/path argument injection.
///
/// # Errors
/// Malformed SHA.
pub fn validate_sha(sha: &str) -> Result<()> {
    ensure!(sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()), "expected a full Git SHA-1 commit id");
    Ok(())
}

/// Clone without checkout outside the sandbox, then checkout the requested commit inside it.
/// Git's candidate-controlled attributes and filters are therefore processed only in Seatbelt.
///
/// # Errors
/// Invalid SHA, clone or confined checkout failure.
pub fn fresh_checkout(repository: &Path, sha: &str, destination: &Path, gate_home: &Path, config: &GateConfig) -> Result<()> {
    validate_sha(sha)?;
    ensure!(!destination.exists(), "fresh clone destination already exists");
    let repo = repository.to_str().context("repository path is not UTF-8")?;
    let dest = destination.to_str().context("clone path is not UTF-8")?;
    trusted("git", &["clone", "--local", "--no-hardlinks", "--no-checkout", "--", repo, dest], repository)?;
    let sandbox = Sandbox::new(destination, gate_home, config, Network::Off)?;
    sandbox.run(&CommandSpec::new("git", &["-c", "core.hooksPath=/dev/null", "checkout", "--detach", sha]))?;
    ensure!(trusted("git", &["rev-parse", "HEAD"], destination)? == sha, "fresh checkout SHA readback mismatch");
    Ok(())
}

/// Reserve one ephemeral loopback port for the protected benchmark runner, which must check
/// bind ownership using its ready-file handshake. The gate never authorizes all loopback ports.
///
/// # Errors
/// No port available.
pub fn bench_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("reserve benchmark port")?;
    Ok(listener.local_addr().context("read benchmark port")?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_unsafe_git_sha() {
        assert!(validate_sha("--upload-pack=evil").is_err());
        assert!(validate_sha(&"a".repeat(40)).is_ok());
    }
}
