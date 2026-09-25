//! System OpenSSH connection manager and askpass relay.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aim_proto::error::{ErrorCode, ProtoError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;

use super::quote;

const ASKPASS_SOCKET: &str = "AIM_SSH_ASKPASS_SOCKET";
const MAX_CHANNEL_OUTPUT: usize = 64 * 1024 * 1024;

/// A prompt handler supplied by the embedding user interface.
pub trait Prompter: Send + Sync {
    /// Return the answer, or `None` to cancel. `echo` requests visible input.
    fn prompt(&self, text: &str, echo: bool) -> Option<String>;
}

/// OpenSSH process and connection settings.
#[derive(Clone, Debug)]
pub struct SshOptions {
    /// Destination host or `user@host`.
    pub destination: String,
    /// OpenSSH executable (normally `ssh`).
    pub program: PathBuf,
    /// Optional `ssh_config` path, useful for isolated hosts.
    pub config_file: Option<PathBuf>,
    /// Seconds for the idle master to persist.
    pub persist_seconds: u32,
    /// Program invoked by OpenSSH for prompts; it must call [`askpass_client`].
    pub askpass_program: Option<PathBuf>,
    /// Override the private control directory (for isolated tests).
    pub control_dir: Option<PathBuf>,
}

impl SshOptions {
    /// Settings using the system OpenSSH client.
    #[must_use]
    pub fn new(destination: impl Into<String>) -> Self {
        Self {
            destination: destination.into(),
            program: PathBuf::from("ssh"),
            config_file: None,
            persist_seconds: 600,
            askpass_program: None,
            control_dir: None,
        }
    }
}

/// A checked multiplexed SSH connection.
#[derive(Clone, Debug)]
pub struct Connection {
    options: SshOptions,
    control_path: PathBuf,
    /// Effective OpenSSH configuration from `ssh -G`.
    pub effective_config: String,
}

impl Connection {
    /// Resolve the user's config and start or reuse a `ControlMaster`.
    ///
    /// # Errors
    /// Returns an error when configuration, authentication or master startup fails.
    pub async fn connect(options: SshOptions, prompter: Option<Arc<dyn Prompter>>) -> Result<Self, ProtoError> {
        if options.destination.is_empty()
            || options.destination.starts_with('-')
            || options.destination.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "invalid SSH destination"));
        }
        if prompter.is_some() != options.askpass_program.is_some() {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "askpass program and prompter must be provided together"));
        }
        let mut query = base_command(&options);
        query.arg("-G").arg(&options.destination);
        let output = query.output().await.map_err(|_| unavailable("cannot run ssh -G"))?;
        if !output.status.success() {
            return Err(unavailable("ssh -G rejected destination"));
        }
        let effective_config = String::from_utf8(output.stdout).map_err(|_| unavailable("ssh -G output is not UTF-8"))?;
        let own_dir = options.control_dir.clone().map_or_else(ssh_dir, Ok)?;
        std::fs::create_dir_all(&own_dir).map_err(|_| unavailable("cannot create SSH control directory"))?;
        std::fs::set_permissions(&own_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| unavailable("cannot secure SSH control directory"))?;
        let own_template = own_dir.join("%C");
        let mut expand = base_command(&options);
        expand.arg("-G").arg("-o").arg(format!("ControlPath={}", own_template.display())).arg(&options.destination);
        let expanded = expand.output().await.map_err(|_| unavailable("cannot expand SSH control path"))?;
        if !expanded.status.success() {
            return Err(unavailable("cannot expand SSH control path"));
        }
        let expanded_text = String::from_utf8(expanded.stdout).map_err(|_| unavailable("SSH configuration is not UTF-8"))?;
        let own_path = config_value(&expanded_text, "controlpath").ok_or_else(|| unavailable("missing SSH control path"))?;
        let configured_path = config_value(&effective_config, "controlpath");
        if let Some(existing) = configured_path.filter(|path| *path != "none") {
            let candidate =
                Self { options: options.clone(), control_path: PathBuf::from(existing), effective_config: effective_config.clone() };
            if candidate.check_master().await {
                return Ok(candidate);
            }
        }
        let connection = Self { options, control_path: PathBuf::from(own_path), effective_config };
        if connection.check_master().await {
            return Ok(connection);
        }
        let lock_path = connection.control_path.with_extension("lock");
        let lock = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .open(lock_path)
                .map_err(|_| unavailable("cannot create SSH master lock"))?;
            file.lock().map_err(|_| unavailable("cannot lock SSH master"))?;
            Ok::<_, ProtoError>(file)
        })
        .await
        .map_err(|_| unavailable("SSH master lock task failed"))??;
        if connection.check_master().await {
            drop(lock);
            return Ok(connection);
        }
        let mut master = connection.base();
        master
            .arg("-f")
            .arg("-N")
            .arg("-o")
            .arg("ControlMaster=yes")
            .arg("-o")
            .arg(format!("ControlPersist={}", connection.options.persist_seconds))
            .arg("-o")
            .arg(format!("ControlPath={}", own_template.display()));
        if config_value(&connection.effective_config, "serveraliveinterval").is_none_or(|v| v == "0") {
            master.arg("-o").arg("ServerAliveInterval=15");
            if config_value(&connection.effective_config, "serveralivecountmax").is_none_or(|v| v == "3") {
                master.arg("-o").arg("ServerAliveCountMax=4");
            }
        }
        let mut bridge = None;
        if let (Some(prompt), Some(program)) = (prompter, &connection.options.askpass_program) {
            let relay = AskpassRelay::start(&own_dir, prompt)?;
            master.env("SSH_ASKPASS", program).env("SSH_ASKPASS_REQUIRE", "force").env(ASKPASS_SOCKET, &relay.socket);
            bridge = Some(relay);
        }
        master.arg(&connection.options.destination);
        let output = master.output().await.map_err(|_| unavailable("cannot start SSH master"))?;
        drop(bridge);
        if !output.status.success() || !connection.check_master().await {
            return Err(unavailable("SSH master failed to start"));
        }
        drop(lock);
        Ok(connection)
    }

    /// The expanded `ControlMaster` socket path.
    #[must_use]
    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// Whether the multiplexed connection is live.
    pub async fn check_master(&self) -> bool {
        self.base()
            .arg("-S")
            .arg(&self.control_path)
            .arg("-O")
            .arg("check")
            .arg(&self.options.destination)
            .output()
            .await
            .is_ok_and(|output| output.status.success())
    }

    /// Run a remote POSIX shell script. Bytes are passed through stdin, never shell arguments.
    ///
    /// # Errors
    /// Returns an error when the SSH channel or remote command fails.
    pub async fn run(&self, script: &str, input: &[u8]) -> Result<Vec<u8>, ProtoError> {
        self.run_with_status(script, input, false)
            .await
            .and_then(|(status, output)| if status == 0 { Ok(output) } else { Err(unavailable("remote command failed")) })
    }

    /// Run a script and retain its exit code for callers that map errors.
    ///
    /// # Errors
    /// Returns an error when the SSH channel fails.
    pub async fn run_with_status(&self, script: &str, input: &[u8], pty: bool) -> Result<(i32, Vec<u8>), ProtoError> {
        let mut command = self.base();
        command.arg("-S").arg(&self.control_path).arg("-o").arg("ControlMaster=no");
        if pty {
            command.arg("-tt");
        }
        command.arg(&self.options.destination).arg("--").arg(format!("sh -c {}", quote(script)));
        command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let mut child = command.spawn().map_err(|_| unavailable("cannot start ssh channel"))?;
        let mut stdin = child.stdin.take().ok_or_else(|| unavailable("SSH stdin unavailable"))?;
        let input = input.to_vec();
        let writer = tokio::spawn(async move { stdin.write_all(&input).await });
        let mut stdout = child.stdout.take().ok_or_else(|| unavailable("SSH stdout unavailable"))?;
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let Ok(count) = stdout.read(&mut buffer).await else {
                drop(child.start_kill());
                drop(child.wait().await);
                writer.abort();
                return Err(unavailable("SSH stdout failed"));
            };
            if count == 0 {
                break;
            }
            if output.len().saturating_add(count) > MAX_CHANNEL_OUTPUT {
                drop(child.start_kill());
                drop(child.wait().await);
                writer.abort();
                return Err(ProtoError::new(ErrorCode::LimitExceeded, "SSH channel output limit exceeded"));
            }
            output.extend_from_slice(buffer.get(..count).unwrap_or(&[]));
        }
        let status = child.wait().await.map_err(|_| unavailable("SSH channel failed"))?;
        if status.success() {
            writer.await.map_err(|_| unavailable("SSH stdin task failed"))?.map_err(|_| unavailable("SSH stdin failed"))?;
        } else {
            writer.abort();
        }
        Ok((status.code().unwrap_or(255), output))
    }

    /// A channel command for a long-running remote process.
    #[must_use]
    pub fn command(&self, script: &str, pty: bool) -> Command {
        let mut command = self.base();
        command.arg("-S").arg(&self.control_path).arg("-o").arg("ControlMaster=no");
        if pty {
            command.arg("-tt");
        }
        command.arg(&self.options.destination).arg("--").arg(format!("sh -c {}", quote(script)));
        command
    }

    fn base(&self) -> Command {
        base_command(&self.options)
    }
}

fn base_command(options: &SshOptions) -> Command {
    let mut command = Command::new(&options.program);
    if let Some(config) = &options.config_file {
        command.arg("-F").arg(config);
    }
    command
}

fn config_value<'a>(config: &'a str, name: &str) -> Option<&'a str> {
    config.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn ssh_dir() -> Result<PathBuf, ProtoError> {
    let home = std::env::var_os("HOME").ok_or_else(|| unavailable("HOME is unset"))?;
    Ok(PathBuf::from(home).join(".aim/ssh"))
}

fn unavailable(message: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, message)
}

struct AskpassRelay {
    socket: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

static ASKPASS_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl AskpassRelay {
    fn start(dir: &Path, prompt: Arc<dyn Prompter>) -> Result<Self, ProtoError> {
        let socket = dir.join(format!("askpass-{}-{}.sock", std::process::id(), ASKPASS_SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        drop(std::fs::remove_file(&socket));
        let listener = UnixListener::bind(&socket).map_err(|_| unavailable("cannot bind SSH askpass socket"))?;
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut data = Vec::new();
                if stream.read_to_end(&mut data).await.is_err() || data.len() > 8192 {
                    continue;
                }
                let Ok(text) = String::from_utf8(data) else {
                    continue;
                };
                let echo = text.to_ascii_lowercase().contains("yes/no");
                if let Some(reply) = prompt.prompt(&text, echo) {
                    drop(stream.write_all(reply.as_bytes()).await);
                }
            }
        });
        Ok(Self { socket, task })
    }
}

impl Drop for AskpassRelay {
    fn drop(&mut self) {
        self.task.abort();
        drop(std::fs::remove_file(&self.socket));
    }
}

/// Askpass executable entry point: relay one OpenSSH prompt through the inherited socket.
/// The embedding binary should invoke this for its `askpass` command.
///
/// # Errors
/// Returns an error when the socket is absent or its exchange fails.
pub async fn askpass_client(prompt: &str) -> Result<Option<String>, ProtoError> {
    let socket = std::env::var_os(ASKPASS_SOCKET).ok_or_else(|| unavailable("missing askpass socket"))?;
    askpass_client_at(Path::new(&socket), prompt).await
}

async fn askpass_client_at(socket: &Path, prompt: &str) -> Result<Option<String>, ProtoError> {
    let mut stream = UnixStream::connect(socket).await.map_err(|_| unavailable("askpass relay unavailable"))?;
    stream.write_all(prompt.as_bytes()).await.map_err(|_| unavailable("askpass request failed"))?;
    stream.shutdown().await.map_err(|_| unavailable("askpass request failed"))?;
    let mut answer = Vec::new();
    stream.take(8193).read_to_end(&mut answer).await.map_err(|_| unavailable("askpass reply failed"))?;
    if answer.len() > 8192 {
        return Err(unavailable("askpass reply too large"));
    }
    if answer.is_empty() { Ok(None) } else { String::from_utf8(answer).map(Some).map_err(|_| unavailable("askpass reply is not UTF-8")) }
}

#[cfg(test)]
mod tests {
    use super::{AskpassRelay, Prompter, askpass_client_at, config_value};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    struct TestPrompter;
    impl Prompter for TestPrompter {
        fn prompt(&self, text: &str, echo: bool) -> Option<String> {
            if text == "Password:" && !echo { Some("test-answer".to_owned()) } else { None }
        }
    }
    #[test]
    fn parses_effective_configuration() {
        assert_eq!(config_value("host x\nserveraliveinterval 0\n", "serveraliveinterval"), Some("0"));
    }

    #[tokio::test]
    #[ignore = "exercises the OpenSSH askpass process protocol"]
    async fn live_ssh_askpass_relay() {
        let dir = tempfile::Builder::new().prefix("aimask").tempdir_in("/private/tmp").expect("tempdir");
        let relay = AskpassRelay::start(dir.path(), Arc::new(TestPrompter)).expect("relay");
        assert_eq!(askpass_client_at(&relay.socket, "Password:").await.expect("client"), Some("test-answer".to_owned()));
        let askpass = dir.path().join("askpass");
        std::fs::write(&askpass, "#!/bin/sh\nprintf '%s' \"$1\" | nc -U \"$AIM_SSH_ASKPASS_SOCKET\"\n").expect("askpass script");
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let fake_ssh = dir.path().join("fake-ssh");
        std::fs::write(&fake_ssh, "#!/bin/sh\nexec \"$SSH_ASKPASS\" 'Password:'\n").expect("fake ssh script");
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let output = tokio::process::Command::new(&fake_ssh)
            .env("SSH_ASKPASS", &askpass)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(super::ASKPASS_SOCKET, &relay.socket)
            .output()
            .await
            .expect("fake ssh");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"test-answer");
    }
}
