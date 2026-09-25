//! System OpenSSH connection manager and askpass relay.

use std::fmt::Write as _;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aim_proto::error::{ErrorCode, ProtoError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;

const ASKPASS_SOCKET: &str = "AIM_SSH_ASKPASS_SOCKET";
const ASKPASS_TOKEN: &str = "AIM_SSH_ASKPASS_TOKEN";
const MAX_CHANNEL_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_CONTROL_PATH: usize = 100;

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
            || !options
                .destination
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b':' | b'[' | b']' | b'-'))
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
        let directory = std::fs::symlink_metadata(&own_dir).map_err(|_| unavailable("cannot inspect SSH control directory"))?;
        if !directory.file_type().is_dir() || directory.uid() != rustix::process::geteuid().as_raw() {
            return Err(unavailable("SSH control directory is not private"));
        }
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
        if own_path.len() > MAX_CONTROL_PATH {
            return Err(unavailable("SSH control path is too long"));
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
        if let Ok(metadata) = std::fs::symlink_metadata(&connection.control_path) {
            if !metadata.file_type().is_socket() || metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(unavailable("SSH control path is occupied by an untrusted file"));
            }
            std::fs::remove_file(&connection.control_path).map_err(|_| unavailable("cannot remove stale SSH control socket"))?;
        }
        connection.start_master(&own_dir, &own_template, prompter).await?;
        drop(lock);
        Ok(connection)
    }

    async fn start_master(&self, own_dir: &Path, own_template: &Path, prompter: Option<Arc<dyn Prompter>>) -> Result<(), ProtoError> {
        let mut master = self.base();
        master
            .arg("-N")
            .arg("-T")
            .arg("-o")
            .arg("ControlMaster=yes")
            .arg("-o")
            .arg("ClearAllForwardings=yes")
            .arg("-o")
            .arg("ForwardAgent=no")
            .arg("-o")
            .arg("ForwardX11=no")
            .arg("-o")
            .arg("RemoteCommand=none")
            .arg("-o")
            .arg(format!("ControlPersist={}", self.options.persist_seconds))
            .arg("-o")
            .arg(format!("ControlPath={}", own_template.display()));
        if config_value(&self.effective_config, "serveraliveinterval").is_none_or(|v| v == "0") {
            master.arg("-o").arg("ServerAliveInterval=15");
            if config_value(&self.effective_config, "serveralivecountmax").is_none_or(|v| v == "3") {
                master.arg("-o").arg("ServerAliveCountMax=4");
            }
        }
        let mut bridge = None;
        if let (Some(prompt), Some(program)) = (prompter, &self.options.askpass_program) {
            let relay = AskpassRelay::start(own_dir, prompt)?;
            master
                .env("SSH_ASKPASS", program)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(ASKPASS_SOCKET, &relay.socket)
                .env(ASKPASS_TOKEN, &relay.token);
            bridge = Some(relay);
        }
        master.arg(&self.options.destination);
        master.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let mut child = master.spawn().map_err(|_| unavailable("cannot start SSH master"))?;
        let startup = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if self.check_master().await {
                    return Ok(());
                }
                if child.try_wait().map_err(|_| unavailable("cannot inspect SSH master"))?.is_some() {
                    return Err(unavailable("SSH master failed to start"));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        drop(bridge);
        if !matches!(startup, Ok(Ok(()))) {
            drop(child.start_kill());
            drop(child.wait().await);
            return Err(match startup {
                Ok(Err(error)) => error,
                _ => unavailable("SSH master startup timed out"),
            });
        }
        drop(child);
        Ok(())
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
        let mut command = self.command(script, pty);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
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
        let code = status.code().unwrap_or(255);
        if code == 255 && !self.check_master().await {
            return Err(unavailable("SSH master unavailable"));
        }
        Ok((code, output))
    }

    /// A channel command for a long-running remote process.
    #[must_use]
    pub fn command(&self, script: &str, pty: bool) -> Command {
        let mut command = self.base();
        command.arg("-S").arg(&self.control_path).arg("-o").arg("ControlMaster=no");
        for option in ["BatchMode=yes", "ForwardAgent=no", "ForwardX11=no", "RemoteCommand=none", "RequestTTY=no", "ProxyCommand=false"] {
            command.arg("-o").arg(option);
        }
        if pty {
            command.arg("-tt");
        } else {
            command.arg("-T");
        }
        command.arg(&self.options.destination).arg("--").arg(remote_command(script));
        command
    }

    fn base(&self) -> Command {
        base_command(&self.options)
    }
}

fn base_command(options: &SshOptions) -> Command {
    let mut command = Command::new(&options.program);
    // OpenSSH may forward selected environment variables through user `SendEnv` rules. Keep
    // provider credentials and unrelated process state out of every SSH child by construction.
    command.env_clear();
    for name in [
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "SSH_AUTH_SOCK",
        "KRB5CCNAME",
        "TERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "DISPLAY",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(config) = &options.config_file {
        command.arg("-F").arg(config);
    }
    command
}

fn remote_command(script: &str) -> String {
    // Only inert octal escapes cross the user's login shell. The inner POSIX shell decodes
    // the script, keeping its original stdin available for file bytes and process input.
    let mut encoded = String::with_capacity(script.len().saturating_mul(5));
    for byte in script.bytes() {
        encoded.push('\\');
        encoded.push('0');
        encoded.push(char::from(b'0' + (byte >> 6)));
        encoded.push(char::from(b'0' + ((byte >> 3) & 7)));
        encoded.push(char::from(b'0' + (byte & 7)));
    }
    // The sentinel preserves trailing newlines through command substitution.
    format!("sh -c 'code=$(printf %b \"{encoded}\"; printf x); eval \"${{code%x}}\"'")
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
    token: String,
    task: tokio::task::JoinHandle<()>,
}

static ASKPASS_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl AskpassRelay {
    fn start(dir: &Path, prompt: Arc<dyn Prompter>) -> Result<Self, ProtoError> {
        let socket = dir.join(format!("askpass-{}-{}.sock", std::process::id(), ASKPASS_SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        drop(std::fs::remove_file(&socket));
        let listener = UnixListener::bind(&socket).map_err(|_| unavailable("cannot bind SSH askpass socket"))?;
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(|_| unavailable("cannot generate SSH askpass token"))?;
        let mut token = String::with_capacity(64);
        for byte in random {
            write!(&mut token, "{byte:02x}").map_err(|_| unavailable("cannot encode SSH askpass token"))?;
        }
        let expected_token = token.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut data = Vec::new();
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(60), (&mut stream).take(8192 + 66).read_to_end(&mut data)).await,
                    Ok(Ok(_))
                ) || data.len() > 8192 + 65
                {
                    continue;
                }
                let Ok(text) = String::from_utf8(data) else {
                    continue;
                };
                let Some((received_token, request)) = text.split_once('\n') else {
                    continue;
                };
                if received_token != expected_token || request.len() > 8192 {
                    continue;
                }
                let echo = request.to_ascii_lowercase().contains("yes/no");
                let prompt = Arc::clone(&prompt);
                let request = request.to_owned();
                let reply =
                    tokio::time::timeout(Duration::from_secs(60), tokio::task::spawn_blocking(move || prompt.prompt(&request, echo))).await;
                if let Ok(Ok(Some(reply))) = reply
                    && reply.len() <= 8192
                {
                    drop(tokio::time::timeout(Duration::from_secs(60), stream.write_all(reply.as_bytes())).await);
                }
            }
        });
        Ok(Self { socket, token, task })
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
    let token = std::env::var(ASKPASS_TOKEN).map_err(|_| unavailable("missing askpass token"))?;
    askpass_client_at(Path::new(&socket), &token, prompt).await
}

async fn askpass_client_at(socket: &Path, token: &str, prompt: &str) -> Result<Option<String>, ProtoError> {
    let mut stream = UnixStream::connect(socket).await.map_err(|_| unavailable("askpass relay unavailable"))?;
    stream.write_all(token.as_bytes()).await.map_err(|_| unavailable("askpass request failed"))?;
    stream.write_all(b"\n").await.map_err(|_| unavailable("askpass request failed"))?;
    stream.write_all(prompt.as_bytes()).await.map_err(|_| unavailable("askpass request failed"))?;
    stream.shutdown().await.map_err(|_| unavailable("askpass request failed"))?;
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.take(8193).read_to_end(&mut answer))
        .await
        .map_err(|_| unavailable("askpass reply timed out"))?
        .map_err(|_| unavailable("askpass reply failed"))?;
    if answer.len() > 8192 {
        return Err(unavailable("askpass reply too large"));
    }
    if answer.is_empty() { Ok(None) } else { String::from_utf8(answer).map(Some).map_err(|_| unavailable("askpass reply is not UTF-8")) }
}

#[cfg(test)]
mod tests {
    use super::{AskpassRelay, Connection, Prompter, SshOptions, askpass_client_at, config_value};
    use aim_proto::error::ErrorCode;
    use std::io::Write as _;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;

    struct TestSshd {
        dir: tempfile::TempDir,
        child: Child,
        config: PathBuf,
    }

    impl TestSshd {
        fn start() -> Self {
            let dir = tempfile::Builder::new().prefix("aimconn").tempdir_in("/private/tmp").expect("tempdir");
            let host = dir.path().join("host");
            let client = dir.path().join("client");
            for path in [&host, &client] {
                assert!(
                    Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(path).status().expect("keygen").success()
                );
            }
            std::fs::copy(client.with_extension("pub"), dir.path().join("authorized_keys")).expect("authorized keys");
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
            let port = listener.local_addr().expect("address").port();
            drop(listener);
            let server_config = dir.path().join("sshd_config");
            std::fs::write(&server_config, format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\nPidFile {}\n",
                host.display(), dir.path().join("authorized_keys").display(), dir.path().join("sshd.pid").display()
            )).expect("sshd config");
            let pubkey = std::fs::read_to_string(host.with_extension("pub")).expect("host public key");
            std::fs::write(dir.path().join("known_hosts"), format!("[127.0.0.1]:{port} {pubkey}")).expect("known hosts");
            let config = dir.path().join("ssh_config");
            std::fs::create_dir(dir.path().join("shared")).expect("shared path");
            std::fs::write(&config, format!(
                "Host aim-conn-test\n HostName 127.0.0.1\n Port {port}\n User {}\n IdentityFile {}\n IdentitiesOnly yes\n UserKnownHostsFile {}\n StrictHostKeyChecking yes\n LogLevel ERROR\n ForwardAgent yes\n ForwardX11 yes\n RemoteCommand printf bad\n RequestTTY force\n ControlPath {}/%C\n",
                std::env::var("USER").expect("user"), client.display(), dir.path().join("known_hosts").display(), dir.path().join("shared").display()
            )).expect("ssh config");
            assert!(Command::new("/usr/sbin/sshd").args(["-t", "-f"]).arg(&server_config).status().expect("sshd check").success());
            let child = Command::new("/usr/sbin/sshd")
                .args(["-D", "-e", "-f"])
                .arg(&server_config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("sshd");
            Self { dir, child, config }
        }

        fn options(&self) -> SshOptions {
            let mut options = SshOptions::new("aim-conn-test");
            options.config_file = Some(self.config.clone());
            options.control_dir = Some(self.dir.path().join("ctl"));
            options
        }

        async fn connect(&self) -> Connection {
            for _ in 0..30 {
                if let Ok(connection) = Connection::connect(self.options(), None).await {
                    return connection;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            panic!("private sshd unreachable");
        }
    }

    impl Drop for TestSshd {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(self.dir.path().join("ctl")) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_socket()) {
                        drop(
                            Command::new("ssh")
                                .arg("-F")
                                .arg(&self.config)
                                .arg("-S")
                                .arg(entry.path())
                                .args(["-O", "exit", "aim-conn-test"])
                                .output(),
                        );
                    }
                }
            }
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }

    struct TestPrompter;
    impl Prompter for TestPrompter {
        fn prompt(&self, text: &str, echo: bool) -> Option<String> {
            if text == "Password:" && !echo { Some("test-answer".to_owned()) } else { None }
        }
    }

    struct SlowPrompter;
    impl Prompter for SlowPrompter {
        fn prompt(&self, _: &str, _: bool) -> Option<String> {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Some("answer".to_owned())
        }
    }

    struct LongPrompter;
    impl Prompter for LongPrompter {
        fn prompt(&self, _: &str, _: bool) -> Option<String> {
            Some("x".repeat(8193))
        }
    }
    #[test]
    fn parses_effective_configuration() {
        assert_eq!(config_value("host x\nserveraliveinterval 0\n", "serveraliveinterval"), Some("0"));
    }

    #[test]
    fn login_shells_round_trip_complex_scripts() {
        let script = "printf '%s\\n' 'a\nb' '!!' 'slash\\\\'";
        for shell in ["/bin/sh", "/bin/csh", "/bin/tcsh"] {
            if !std::path::Path::new(shell).exists() {
                continue;
            }
            let output = Command::new(shell).arg("-c").arg(super::remote_command(script)).output().expect("shell");
            assert!(output.status.success(), "{shell} rejected script: {}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(output.stdout, b"a\nb\n!!\nslash\\\\\n", "{shell} changed script output");
            let mut child = Command::new(shell)
                .arg("-c")
                .arg(super::remote_command("cat"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("shell with stdin");
            child.stdin.take().expect("stdin").write_all(b"file\0bytes").expect("write stdin");
            assert_eq!(child.wait_with_output().expect("shell output").stdout, b"file\0bytes", "{shell} changed stdin");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn askpass_rejects_wrong_token_without_blocking_runtime() {
        let dir = tempfile::Builder::new().prefix("aimask").tempdir_in("/private/tmp").expect("tempdir");
        let relay = AskpassRelay::start(dir.path(), Arc::new(SlowPrompter)).expect("relay");
        assert_eq!(askpass_client_at(&relay.socket, "wrong", "Password:").await.expect("wrong token"), None);
        let client = tokio::spawn({
            let socket = relay.socket.clone();
            let token = relay.token.clone();
            async move { askpass_client_at(&socket, &token, "Password:").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let started = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(100), "prompt blocked the current-thread runtime");
        assert_eq!(client.await.expect("task").expect("answer"), Some("answer".to_owned()));
    }

    #[tokio::test]
    async fn askpass_discards_oversized_replies() {
        let dir = tempfile::Builder::new().prefix("aimask").tempdir_in("/private/tmp").expect("tempdir");
        let relay = AskpassRelay::start(dir.path(), Arc::new(LongPrompter)).expect("relay");
        assert_eq!(askpass_client_at(&relay.socket, &relay.token, "Password:").await.expect("oversized reply"), None);
    }

    #[tokio::test]
    async fn rejects_destination_metacharacters_before_spawning_ssh() {
        let mut options = SshOptions::new("host;touch");
        options.program = "/nonexistent/ssh".into();
        let error = Connection::connect(options, None).await.expect_err("invalid destination");
        assert_eq!(error.code, ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn channels_force_safe_ssh_settings() {
        let connection = Connection {
            options: SshOptions::new("example"),
            control_path: "/private/tmp/aim-ssh-test.sock".into(),
            effective_config: String::new(),
        };
        let command = connection.command("printf ok", false);
        let arguments = command.as_std().get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();
        for option in ["BatchMode=yes", "ForwardAgent=no", "ForwardX11=no", "RemoteCommand=none", "RequestTTY=no", "ProxyCommand=false"] {
            assert!(arguments.iter().any(|arg| arg == option), "missing {option}");
        }
        assert!(arguments.iter().any(|arg| arg == "-T"));
    }

    #[tokio::test]
    #[ignore = "starts a private user-space sshd"]
    async fn live_ssh_config_isolation_and_stale_master_recovery() {
        let sshd = TestSshd::start();
        let connection = sshd.connect().await;
        assert!(connection.control_path().starts_with(sshd.dir.path().join("ctl")));
        assert_eq!(connection.run("printf ok", &[]).await.expect("channel"), b"ok");
        assert_eq!(connection.run("printf '%s' \"${SSH_AUTH_SOCK:-}\"", &[]).await.expect("agent socket"), b"");
        let output = Command::new("ssh")
            .arg("-F")
            .arg(&sshd.config)
            .arg("-S")
            .arg(connection.control_path())
            .args(["-O", "check", "aim-conn-test"])
            .output()
            .expect("master check");
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let pid = text
            .split("pid=")
            .nth(1)
            .and_then(|tail| tail.split(|ch: char| !ch.is_ascii_digit()).next())
            .and_then(|pid| pid.parse::<u32>().ok())
            .expect("master pid");
        assert!(Command::new("kill").args(["-KILL", &pid.to_string()]).status().expect("kill master").success());
        for _ in 0..30 {
            if !connection.check_master().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(connection.run("printf nope", &[]).await.expect_err("no fallback connection").code, ErrorCode::Unavailable);
        let replacement = sshd.connect().await;
        assert_eq!(replacement.run("printf restored", &[]).await.expect("recovered channel"), b"restored");
    }

    #[tokio::test]
    #[ignore = "exercises the OpenSSH askpass process protocol"]
    async fn live_ssh_askpass_relay() {
        let dir = tempfile::Builder::new().prefix("aimask").tempdir_in("/private/tmp").expect("tempdir");
        let relay = AskpassRelay::start(dir.path(), Arc::new(TestPrompter)).expect("relay");
        assert_eq!(askpass_client_at(&relay.socket, &relay.token, "Password:").await.expect("client"), Some("test-answer".to_owned()));
        let askpass = dir.path().join("askpass");
        let binary = crate::ssh::live_tests::aimx_binary();
        std::fs::write(&askpass, format!("#!/bin/sh\nexec {} askpass \"$@\"\n", crate::ssh::quote(&binary.to_string_lossy())))
            .expect("askpass script");
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let fake_ssh = dir.path().join("fake-ssh");
        std::fs::write(&fake_ssh, "#!/bin/sh\nexec \"$SSH_ASKPASS\" 'Password:'\n").expect("fake ssh script");
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let output = tokio::process::Command::new(&fake_ssh)
            .env("SSH_ASKPASS", &askpass)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(super::ASKPASS_SOCKET, &relay.socket)
            .env(super::ASKPASS_TOKEN, &relay.token)
            .output()
            .await
            .expect("fake ssh");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"test-answer\n");

        let marker = dir.path().join("master-ready");
        let pid_path = dir.path().join("master-pid");
        let socket = dir.path().join("ctl/master.sock");
        std::fs::write(
            &fake_ssh,
            format!(
                "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = '-G' ]; then printf 'controlpath %s\\n' {}; exit 0; fi\ndone\ncase \" $* \" in\n  *' -O check '*) [ -e {} ]; exit $?;;\nesac\nanswer=$(\"$SSH_ASKPASS\" 'Password:') || exit 1\n[ \"$answer\" = 'test-answer' ] || exit 1\necho $$ > {}\n: > {}\nexec sleep 60\n",
                crate::ssh::quote(&socket.to_string_lossy()),
                crate::ssh::quote(&marker.to_string_lossy()),
                crate::ssh::quote(&pid_path.to_string_lossy()),
                crate::ssh::quote(&marker.to_string_lossy())
            ),
        )
        .expect("connection fake ssh");
        let mut options = SshOptions::new("fake-host");
        options.program = fake_ssh;
        options.askpass_program = Some(askpass);
        options.control_dir = Some(dir.path().join("ctl"));
        let connected = Connection::connect(options, Some(Arc::new(TestPrompter))).await.expect("connect through askpass");
        assert_eq!(connected.control_path(), socket);
        let pid = std::fs::read_to_string(pid_path).expect("master pid");
        assert!(Command::new("kill").arg(pid.trim()).status().expect("stop fake master").success());
    }
}
