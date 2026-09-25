//! Live tests use a private, unprivileged sshd and never edit SSH user configuration.

use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aim_proto::content::Content;
use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    BackendSpec, CallScope, ExecRead, ExecReadParams, ExecRelease, ExecReleaseParams, ExecSpawn, ExecSpawnParams, FsRead, FsReadParams,
    FsWrite, FsWriteParams, GenerationRange, Initialize, InitializeParams, InitializeResult, PeerInfo, ToolsCall, ToolsCallParams,
    WorkspaceOpen, WorkspaceOpenParams,
};
use aim_proto::harness::{CaseMode, Command as RemoteCommand, ExactEdit, ExitStatus, Precondition};
use aim_proto::ids::IdempotencyKey;
use aim_proto::ids::ProcId;
use aim_proto::ids::ResumeToken;
use aim_proto::ids::WorkspaceId;
use aim_proto::tool::{ToolContent, ToolResult};
use aim_rpc::{NoHandler, Peer, PeerConfig};
use tempfile::TempDir;

use super::agentless::AgentlessWorkspace;
use super::bootstrap::{Artifact, install, local_sha256};
use super::conn::{Connection, SshOptions};
use crate::workspace::{EditRequest, GlobQuery, GrepQuery, ListRequest, SpawnSpec, Workspace, WriteRequest};

struct Sshd {
    dir: TempDir,
    child: Child,
    config: PathBuf,
    sandbox: PathBuf,
}

pub(super) fn aimx_binary() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| {
            assert!(Command::new("cargo").args(["build", "-q", "-p", "aimx", "--bin", "aimx"]).status().expect("build aimx").success());
            let executable = std::env::current_exe().expect("test executable");
            executable.parent().expect("deps directory").parent().expect("target directory").join("aimx")
        })
        .clone()
}

fn aim_binary() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| {
            assert!(Command::new("cargo").args(["build", "-q", "-p", "aim", "--bin", "aim"]).status().expect("build aim").success());
            let executable = std::env::current_exe().expect("test executable");
            executable.parent().expect("deps directory").parent().expect("target directory").join("aim")
        })
        .clone()
}

fn spawn_forward(sshd: &Sshd, root: &Path, bootstrap: &str) -> (Peer, tokio::process::Child) {
    let local_home = sshd.dir.path().join("local_home");
    std::fs::create_dir_all(&local_home).expect("local home");
    let local_root = sshd.local_root(&root.file_name().expect("workspace name").to_string_lossy());
    let mut command = tokio::process::Command::new("sandbox-exec");
    command
        .arg("-f")
        .arg(&sshd.sandbox)
        .arg(aimx_binary())
        .args(["serve", "--stdio", "--ssh", "aim-test", "--root"])
        .arg(root)
        .args(["--ssh-config"])
        .arg(&sshd.config)
        .args(["--bootstrap", bootstrap, "--idle-secs", "2"])
        .current_dir(local_root)
        .env("HOME", local_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("aimx forward");
    let stdin = child.stdin.take().expect("forward stdin");
    let stdout = child.stdout.take().expect("forward stdout");
    (Peer::spawn(stdout, stdin, NoHandler, PeerConfig::default()), child)
}

async fn initialize(peer: &Peer, resume: Option<ResumeToken>) -> InitializeResult {
    let (min, max) = aim_proto::HARNESS_GENERATIONS;
    peer.call::<Initialize>(InitializeParams {
        generations: GenerationRange { min, max },
        client: PeerInfo { name: "ssh-live-test".to_owned(), version: "0".to_owned() },
        auth: None,
        resume,
    })
    .await
    .expect("initialize")
}

fn tool_text(result: &ToolResult) -> &str {
    result
        .content
        .iter()
        .find_map(|item| match item {
            ToolContent::Text { text } => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .unwrap_or("")
}

async fn tool(peer: &Peer, workspace: &WorkspaceId, name: &str, arguments: serde_json::Value, key: &str) -> ToolResult {
    let result = peer
        .call::<ToolsCall>(ToolsCallParams {
            scope: None,
            workspace: workspace.clone(),
            name: name.to_owned(),
            arguments,
            idempotency_key: Some(IdempotencyKey::new(key)),
        })
        .await
        .expect("tool call");
    assert!(!result.is_error, "tool {name} failed: {}", tool_text(&result));
    result
}

async fn exercise_tools(sshd: &Sshd, peer: &Peer, workspace: &WorkspaceId, remote: &Path, local: &Path) {
    std::fs::write(local.join("tool.txt"), "local").expect("local fixture");
    tool(peer, workspace, "Write", serde_json::json!({"file_path":"tool.txt","content":"alpha\n"}), "tool-write").await;
    assert_eq!(std::fs::read_to_string(remote.join("tool.txt")).expect("remote write"), "alpha\n");
    sshd.assert_local_denied(&remote.join("tool.txt"));
    assert!(tool_text(&tool(peer, workspace, "Read", serde_json::json!({"file_path":"tool.txt"}), "tool-read").await).contains("alpha"));
    tool(peer, workspace, "Edit", serde_json::json!({"file_path":"tool.txt","old_string":"alpha","new_string":"beta"}), "tool-edit").await;
    assert!(tool_text(&tool(peer, workspace, "LS", serde_json::json!({"path":"."}), "tool-ls").await).contains("tool.txt"));
    assert!(tool_text(&tool(peer, workspace, "Glob", serde_json::json!({"pattern":"*.txt"}), "tool-glob").await).contains("tool.txt"));
    assert!(
        tool_text(&tool(peer, workspace, "Grep", serde_json::json!({"pattern":"beta","output_mode":"content"}), "tool-grep").await)
            .contains("beta")
    );
    let bash = tool(peer, workspace, "Bash", serde_json::json!({"command":"printf shell > command.txt; cat tool.txt"}), "tool-bash").await;
    assert!(tool_text(&bash).contains("beta"));
    assert_eq!(std::fs::read_to_string(remote.join("command.txt")).expect("remote bash"), "shell");
    sshd.assert_local_denied(&remote.join("command.txt"));
    let background =
        tool(peer, workspace, "Bash", serde_json::json!({"command":"printf started; sleep 30","run_in_background":true}), "tool-bg").await;
    let id = tool_text(&background).split("id ").nth(1).and_then(|text| text.split('.').next()).expect("background id");
    tool(peer, workspace, "BashOutput", serde_json::json!({"id":id}), "tool-output").await;
    tool(peer, workspace, "KillShell", serde_json::json!({"id":id}), "tool-kill").await;
    assert_eq!(std::fs::read_to_string(local.join("tool.txt")).expect("local untouched"), "local");
    assert!(!local.join("command.txt").exists());
}

fn kill_ssh_child(parent: &tokio::process::Child) {
    let pid = parent.id().expect("forwarder pid");
    let output = Command::new("pgrep").args(["-P", &pid.to_string()]).output().expect("find ssh child");
    assert!(output.status.success(), "SSH channel child must exist");
    let text = String::from_utf8(output.stdout).expect("pid list");
    let child = text.lines().next().expect("ssh pid");
    let command = Command::new("ps").args(["-p", child, "-o", "comm="]).output().expect("inspect child");
    assert!(String::from_utf8_lossy(&command.stdout).contains("ssh"));
    assert!(Command::new("kill").args(["-KILL", child]).status().expect("kill ssh").success());
}

impl Sshd {
    fn remote_root(&self, name: &str) -> PathBuf {
        let root = self.dir.path().join("remote_tree").join(name);
        std::fs::create_dir_all(&root).expect("remote root");
        root
    }

    fn local_root(&self, name: &str) -> PathBuf {
        let root = self.dir.path().join("local_tree").join(name);
        std::fs::create_dir_all(&root).expect("local root");
        root
    }

    fn start(native_search: bool) -> Self {
        Self::start_with(native_search, "", "")
    }

    fn start_with(native_search: bool, daemon_config: &str, client_config: &str) -> Self {
        Self::start_with_path(native_search, daemon_config, client_config, None)
    }

    fn start_with_path(native_search: bool, daemon_config: &str, client_config: &str, remote_path: Option<&str>) -> Self {
        let dir = tempfile::Builder::new().prefix("aimssh").tempdir_in("/private/tmp").expect("tempdir");
        let host = dir.path().join("host");
        let client = dir.path().join("client");
        for path in [&host, &client] {
            assert!(
                Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(path).status().expect("ssh-keygen").success()
            );
        }
        std::fs::copy(client.with_extension("pub"), dir.path().join("authorized_keys")).expect("authorized_keys");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
        let port = listener.local_addr().expect("local address").port();
        drop(listener);
        let config = dir.path().join("ssh_config");
        let sshd_config = dir.path().join("sshd_config");
        let remote_home = dir.path().join("remote_home");
        std::fs::create_dir(&remote_home).expect("remote home");
        let sandbox = dir.path().join("local_sandbox.sb");
        std::fs::write(
            &sandbox,
            format!(
                "(version 1)\n(allow default)\n(deny file-read* file-write* (subpath \"{}\"))\n(deny file-read* file-write* (subpath \"{}\"))\n",
                dir.path().join("remote_tree").display(),
                remote_home.display()
            ),
        )
        .expect("local sandbox profile");
        let rg_path = if native_search {
            let output = Command::new("mise").args(["which", "rg"]).output().expect("mise which rg");
            assert!(output.status.success(), "ripgrep must be installed through mise");
            let path = PathBuf::from(String::from_utf8(output.stdout).expect("rg path").trim());
            path.parent().expect("rg directory").to_string_lossy().into_owned()
        } else {
            String::new()
        };
        let path = remote_path.map_or_else(|| format!("{rg_path}:/usr/bin:/bin:/usr/sbin:/sbin"), str::to_owned);
        std::fs::write(&sshd_config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\nPidFile {}\nSetEnv HOME={} PATH={path}\n{daemon_config}", host.display(), dir.path().join("authorized_keys").display(), dir.path().join("sshd.pid").display(), remote_home.display())).expect("sshd config");
        let pubkey = std::fs::read_to_string(host.with_extension("pub")).expect("host public key");
        std::fs::write(dir.path().join("known_hosts"), format!("[127.0.0.1]:{port} {pubkey}")).expect("known_hosts");
        std::fs::write(&config, format!("Host aim-test\n  HostName 127.0.0.1\n  Port {port}\n  User {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  LogLevel ERROR\n{client_config}", std::env::var("USER").expect("user"), client.display(), dir.path().join("known_hosts").display())).expect("ssh config");
        let check = Command::new("/usr/sbin/sshd").args(["-t", "-f"]).arg(&sshd_config).output().expect("sshd config check");
        assert!(check.status.success(), "sshd config invalid: {}", String::from_utf8_lossy(&check.stderr));
        let child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sshd");
        Self { dir, child, config, sandbox }
    }

    fn assert_local_denied(&self, remote_file: &Path) {
        assert!(remote_file.is_file(), "remote fixture must exist before isolation check");
        let output = Command::new("sandbox-exec")
            .arg("-f")
            .arg(&self.sandbox)
            .arg(std::env::current_exe().expect("test binary"))
            .args(["--exact", "ssh::sandbox_probe::local_std_fs_denied_at_remote_path", "--ignored"])
            .env("AIM_SSH_SANDBOX_PROBE", remote_file)
            .output()
            .expect("probe local sandbox");
        assert!(output.status.success(), "local Rust std::fs could access the remote filesystem path");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "local sandbox probe did not execute");
    }

    fn options(&self) -> SshOptions {
        let mut options = SshOptions::new("aim-test");
        options.config_file = Some(self.config.clone());
        options.control_dir = Some(self.dir.path().join("ctl"));
        options
    }

    async fn connect(&self) -> Connection {
        for _ in 0..30 {
            if let Ok(connection) = Connection::connect(self.options(), None).await {
                return connection;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("user-space sshd was not reachable");
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        for control in [self.dir.path().join("ctl"), self.dir.path().join("local_home/.aim/ssh")] {
            if let Ok(entries) = std::fs::read_dir(control) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|extension| extension == "lock") {
                        continue;
                    }
                    drop(
                        Command::new("ssh")
                            .arg("-F")
                            .arg(&self.config)
                            .arg("-S")
                            .arg(entry.path())
                            .args(["-O", "exit", "aim-test"])
                            .output(),
                    );
                }
            }
        }
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

/// A disposable Linux sshd. The Docker daemon is managed by the caller; this fixture owns only
/// its container, image, keys, SSH config and control socket.
struct LinuxSshd {
    dir: TempDir,
    image: String,
    container: String,
    config: PathBuf,
}

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker").args(args).output().expect("run docker")
}

fn docker_checked(args: &[&str]) -> String {
    let output = docker(args);
    assert!(output.status.success(), "docker {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("docker UTF-8 output").trim().to_owned()
}

impl LinuxSshd {
    fn start(distribution: &str) -> Self {
        let dir = tempfile::Builder::new().prefix("aimlx").tempdir_in("/private/tmp").expect("Linux fixture tempdir");
        let image = format!("aim-ssh-live-{distribution}-{}", std::process::id());
        let dockerfile = match distribution {
            "debian" => {
                "FROM debian:bookworm-slim\nRUN apt-get update && apt-get install -y --no-install-recommends openssh-server procps perl coreutils && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd\n"
            }
            "alpine" => "FROM alpine:3.20\nRUN apk add --no-cache openssh-server && mkdir -p /run/sshd\n",
            other => panic!("unsupported Linux SSH target {other}"),
        };
        std::fs::write(dir.path().join("Dockerfile"), dockerfile).expect("Dockerfile");
        let build_dir = dir.path().to_string_lossy();
        docker_checked(&["build", "-q", "-t", &image, &build_dir]);
        let container = docker_checked(&["run", "--rm", "-d", "-p", "127.0.0.1::22", &image, "sleep", "3600"]);
        assert!(!container.is_empty(), "docker did not return a container id");
        let config = dir.path().join("ssh_config");
        let instance = Self { dir, image, container, config };
        let dir = &instance.dir;
        let container = &instance.container;
        let fixture = dir.path().join("fixture");
        std::fs::create_dir(&fixture).expect("fixture dir");
        let host = fixture.join("host");
        let client = dir.path().join("client");
        for key_path in [&host, &client] {
            assert!(
                Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(key_path)
                    .status()
                    .expect("generate temporary SSH key")
                    .success()
            );
        }
        std::fs::copy(client.with_extension("pub"), fixture.join("authorized_keys")).expect("authorized keys");
        std::fs::write(
            fixture.join("sshd_config"),
            "Port 22\nListenAddress 0.0.0.0\nHostKey /tmp/aim-fixture/host\nAuthorizedKeysFile /tmp/aim-fixture/authorized_keys\nPermitRootLogin yes\nPasswordAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\nMaxSessions 10\nPidFile /tmp/aim-sshd.pid\n",
        )
        .expect("Linux sshd config");
        let copy_source = fixture.to_string_lossy();
        docker_checked(&["cp", &copy_source, &format!("{container}:/tmp/aim-fixture")]);
        docker_checked(&["exec", container, "/usr/sbin/sshd", "-t", "-f", "/tmp/aim-fixture/sshd_config"]);
        docker_checked(&["exec", "-d", container, "/usr/sbin/sshd", "-D", "-e", "-f", "/tmp/aim-fixture/sshd_config"]);
        let address = docker_checked(&["port", container, "22/tcp"]);
        let port = address.rsplit(':').next().expect("published SSH port");
        let host_key = std::fs::read_to_string(host.with_extension("pub")).expect("host public key");
        std::fs::write(dir.path().join("known_hosts"), format!("[127.0.0.1]:{port} {host_key}")).expect("known hosts");
        std::fs::write(
            &instance.config,
            format!(
                "Host aim-linux\n  HostName 127.0.0.1\n  Port {port}\n  User root\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  LogLevel ERROR\n",
                client.display(),
                dir.path().join("known_hosts").display()
            ),
        )
        .expect("Linux SSH config");
        instance
    }

    fn options(&self) -> SshOptions {
        let mut options = SshOptions::new("aim-linux");
        options.config_file = Some(self.config.clone());
        options.control_dir = Some(self.dir.path().join("ctl"));
        options
    }

    async fn connect(&self) -> Connection {
        for _ in 0..40 {
            if let Ok(connection) = Connection::connect(self.options(), None).await {
                return connection;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Linux sshd did not become reachable");
    }
}

impl Drop for LinuxSshd {
    fn drop(&mut self) {
        for control in ["ctl", "shortctl"] {
            if let Ok(entries) = std::fs::read_dir(self.dir.path().join(control)) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|extension| extension == "lock") {
                        continue;
                    }
                    drop(
                        Command::new("ssh")
                            .arg("-F")
                            .arg(&self.config)
                            .arg("-S")
                            .arg(entry.path())
                            .args(["-O", "exit", "aim-linux"])
                            .output(),
                    );
                }
            }
        }
        drop(docker(&["rm", "-f", &self.container]));
        drop(docker(&["image", "rm", &self.image]));
    }
}

fn key() -> IdempotencyKey {
    IdempotencyKey::new("live-test-key")
}
fn remote_path(root: &Path, name: &str) -> String {
    root.join(name).to_string_lossy().into_owned()
}

fn agentless_root(sshd: &Sshd, name: &str) -> PathBuf {
    sshd.remote_root(name)
}

async fn open_agentless(sshd: &Sshd, root: &Path) -> AgentlessWorkspace {
    AgentlessWorkspace::open(sshd.connect().await, &root.to_string_lossy()).await.expect("agentless workspace")
}

async fn run_to_exit(workspace: &AgentlessWorkspace, root: &Path, command: &RemoteCommand, timeout: Option<Duration>) {
    let env = std::collections::BTreeMap::new();
    let root_text = root.to_string_lossy();
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec { command, cwd: &root_text, env: &env, pty: None, stdin: false, timeout, key: &key() })
        .await
        .expect("spawn");
    for _ in 0..20 {
        if workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_millis(200)).await.expect("read").exit.is_some() {
            workspace.exec().expect("exec").release(&proc).await.expect("release");
            return;
        }
    }
    workspace.exec().expect("exec").release(&proc).await.expect("release stalled process");
    panic!("process did not exit");
}

async fn linux_spawn(workspace: &AgentlessWorkspace, root: &str, script: String, timeout: Option<Duration>) -> ProcId {
    let command = RemoteCommand::Shell { script };
    let env = std::collections::BTreeMap::new();
    workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec { command: &command, cwd: root, env: &env, pty: None, stdin: false, timeout, key: &key() })
        .await
        .expect("Linux process spawn")
}

async fn linux_exit(workspace: &AgentlessWorkspace, proc: &ProcId) -> ExitStatus {
    for _ in 0..30 {
        if let Some(exit) =
            workspace.exec().expect("exec").read(proc, 0, 100, Duration::from_millis(200)).await.expect("Linux process read").exit
        {
            workspace.exec().expect("exec").release(proc).await.expect("release exited Linux process");
            return exit;
        }
    }
    panic!("Linux process did not exit");
}

async fn assert_linux_sleep_stopped(connection: &Connection, workspace: &AgentlessWorkspace, marker: &str) {
    let pid = workspace.fs().read(marker, None, 100, true).await.expect("read Linux sleep PID").content.into_bytes();
    let pid = String::from_utf8(pid).expect("decimal PID");
    assert!(!pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()));
    let check = format!("if [ -r /proc/{pid}/stat ]; then awk '{{print $3}}' /proc/{pid}/stat; else printf gone; fi");
    for _ in 0..40 {
        let state = connection.run(&check, &[]).await.expect("inspect remote process");
        if state == b"gone" || state == b"Z\n" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("remote sleep {pid} remained running");
}

async fn linux_master_recovery(sshd: &LinuxSshd) {
    let mut short_options = sshd.options();
    short_options.persist_seconds = 1;
    short_options.control_dir = Some(sshd.dir.path().join("shortctl"));
    let short = Connection::connect(short_options, None).await.expect("short-lived Linux master");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!short.check_master().await, "Linux master did not expire");
    assert_eq!(short.run("printf recovered", &[]).await.expect("recover Linux master"), b"recovered");
    assert!(short.check_master().await, "Linux recovery bypassed multiplexing");
}

async fn linux_regressions(distribution: &str) {
    let sshd = LinuxSshd::start(distribution);
    let connection = sshd.connect().await;
    let root = "/tmp/aim-work";
    connection.run("mkdir -p /tmp/aim-work", &[]).await.expect("create Linux workspace");
    let workspace = Arc::new(AgentlessWorkspace::open(connection.clone(), root).await.expect("open Linux agentless workspace"));

    // N1: GNU stat must preserve mode when overwriting and editing, including empty files.
    let file = format!("{root}/script.sh");
    workspace
        .fs()
        .write(WriteRequest {
            path: &file,
            content: &Content::Utf8 { text: "old\n".to_owned() },
            precondition: &Precondition::IfAbsent,
            create_dirs: false,
            key: &key(),
        })
        .await
        .expect("create Linux file");
    connection.run("chmod 755 /tmp/aim-work/script.sh", &[]).await.expect("mark Linux file executable");
    workspace
        .fs()
        .write(WriteRequest {
            path: &file,
            content: &Content::Utf8 { text: "before\n".to_owned() },
            precondition: &Precondition::Any,
            create_dirs: false,
            key: &key(),
        })
        .await
        .expect("overwrite Linux file");
    let edit = [ExactEdit { old: "before".to_owned(), new: "after".to_owned(), replace_all: false }];
    workspace
        .fs()
        .edit(EditRequest { path: &file, edits: &edit, precondition: &Precondition::Any, key: &key() })
        .await
        .expect("edit Linux file");
    assert_eq!(workspace.fs().read(&file, None, 100, true).await.expect("read Linux edit").content.into_bytes(), b"after\n");
    assert_eq!(connection.run("stat -c %a /tmp/aim-work/script.sh", &[]).await.expect("Linux mode"), b"755\n");
    connection.run("touch /tmp/aim-work/empty.txt", &[]).await.expect("empty fixture");
    let listing =
        workspace.fs().list(ListRequest { path: root, limit: 100, page_token: None, include_hidden: false }).await.expect("Linux listing");
    assert!(listing.entries.iter().any(|entry| entry.name == "empty.txt" && entry.kind == aim_proto::harness::EntryKind::File));

    // N2: an expired master must recover through multiplexing on the same connection.
    linux_master_recovery(&sshd).await;

    // N4: 64 KiB heredocs exceeded Linux MAX_ARG_STRLEN after double embedding and octal expansion.
    let payload = "x".repeat(64 * 1024);
    let script = format!("cat > /tmp/aim-work/large.txt <<'AIM_EOF'\n{payload}\nAIM_EOF");
    let proc = linux_spawn(&workspace, root, script, None).await;
    assert_eq!(linux_exit(&workspace, &proc).await, ExitStatus::Exited { code: 0 });
    assert_eq!(
        workspace.fs().read(&format!("{root}/large.txt"), None, 70_000, true).await.expect("large script result").content.len(),
        65_537
    );

    // N5 and N6: Alpine has busybox realpath/ps and no perl; exit codes and group kills must work.
    if distribution == "alpine" {
        assert!(connection.run("command -v perl", &[]).await.is_err(), "Alpine fixture unexpectedly has perl");
    }
    let proc = linux_spawn(&workspace, root, "exit 3".to_owned(), None).await;
    assert_eq!(linux_exit(&workspace, &proc).await, ExitStatus::Exited { code: 3 });
    for _ in 0..3 {
        let proc = linux_spawn(&workspace, root, "sleep 30".to_owned(), None).await;
        workspace.exec().expect("exec").signal(&proc, aim_proto::harness::Signal::Kill).await.expect("Linux group signal");
        assert_eq!(linux_exit(&workspace, &proc).await, ExitStatus::Signaled { signal: 9 });
    }

    let marker = format!("{root}/timed-sleep.pid");
    let proc =
        linux_spawn(&workspace, root, format!("sleep 30 & printf '%s' \"$!\" > {marker}; wait"), Some(Duration::from_millis(500))).await;
    assert_eq!(linux_exit(&workspace, &proc).await, ExitStatus::TimedOut);
    assert_linux_sleep_stopped(&connection, &workspace, &marker).await;

    // N3: process slots plus transient reads fill sshd's ten-session limit. Concurrent release
    // must still kill every process, report failures, and leave capacity for the reads.
    let mut processes = Vec::new();
    for index in 0..6 {
        let marker = format!("{root}/sleep-{index}.pid");
        let script = format!("sleep 30 & printf '%s' \"$!\" > {marker}; wait");
        let proc = linux_spawn(&workspace, root, script, None).await;
        processes.push((proc, marker));
    }
    for (_, marker) in &processes {
        for _ in 0..40 {
            if workspace.fs().read(marker, None, 100, true).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(workspace.fs().read(marker, None, 100, true).await.is_ok(), "Linux sleep PID marker missing");
    }
    let mut tasks = tokio::task::JoinSet::new();
    for (proc, _) in &processes {
        let workspace = Arc::clone(&workspace);
        let proc = proc.clone();
        tasks.spawn(async move { workspace.exec().expect("exec").release(&proc).await.expect("concurrent Linux release") });
    }
    for _ in 0..3 {
        let workspace = Arc::clone(&workspace);
        tasks.spawn(async move {
            workspace.fs().stat(root, false).await.expect("concurrent Linux stat");
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("Linux concurrent operation panicked");
    }
    for (_, marker) in &processes {
        assert_linux_sleep_stopped(&connection, &workspace, marker).await;
    }
}

fn linux_target_enabled(distribution: &str) -> bool {
    let Ok(targets) = std::env::var("AIM_SSH_LINUX_TARGETS") else {
        eprintln!("Linux SSH live target {distribution} skipped: set AIM_SSH_LINUX_TARGETS=debian,alpine");
        return false;
    };
    if !targets.split(',').any(|target| target.trim() == distribution) {
        eprintln!("Linux SSH live target {distribution} skipped: not selected by AIM_SSH_LINUX_TARGETS");
        return false;
    }
    let available =
        Command::new("docker").arg("info").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status| status.success());
    if !available {
        eprintln!("Linux SSH live target {distribution} skipped: Docker/colima is unavailable");
    }
    available
}

#[tokio::test]
#[ignore = "requires Docker/colima; select with AIM_SSH_LINUX_TARGETS=debian"]
async fn live_ssh_linux_debian_regressions() {
    if linux_target_enabled("debian") {
        linux_regressions("debian").await;
    }
}

#[tokio::test]
#[ignore = "requires Docker/colima; select with AIM_SSH_LINUX_TARGETS=alpine"]
async fn live_ssh_linux_alpine_regressions() {
    if linux_target_enabled("alpine") {
        linux_regressions("alpine").await;
    }
}

async fn wait_for_sleep_pid(marker: &Path) -> String {
    for _ in 0..40 {
        if let Ok(pid) = std::fs::read_to_string(marker)
            && !pid.is_empty()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("remote sleep pid marker was not written");
}

fn running_sleep(pid: &str) -> bool {
    let output = Command::new("ps").args(["-p", pid, "-o", "comm="]).output().expect("inspect sleep process");
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim().ends_with("sleep")
}

async fn wait_for_sleep_exit(pid: &str) -> bool {
    for _ in 0..40 {
        if !running_sleep(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn stop_sleep_if_running(pid: &str) {
    if running_sleep(pid) {
        drop(Command::new("kill").args(["-KILL", pid]).status());
    }
}

async fn timed<T>(name: &str, operation: impl Future<Output = T>) -> T {
    let started = Instant::now();
    let result = operation.await;
    eprintln!("{name}_ms={}", started.elapsed().as_millis());
    result
}

#[tokio::test]
#[ignore = "starts a private sshd and contrasts local sandbox with remote SSH access"]
async fn live_ssh_local_sandbox_and_remote_have_distinct_file_access() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("sandbox_probe");
    let file = root.join("sentinel.txt");
    std::fs::write(&file, "remote v1").expect("remote fixture");
    sshd.assert_local_denied(&file);
    let connection = sshd.connect().await;
    let script =
        format!("cat -- {}; printf 'remote v2' > {}", super::quote(&file.to_string_lossy()), super::quote(&file.to_string_lossy()));
    assert_eq!(connection.run(&script, &[]).await.expect("remote read and write"), b"remote v1");
    assert_eq!(std::fs::read_to_string(&file).expect("remote changed"), "remote v2");
    sshd.assert_local_denied(&file);
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_process_permits_return_after_exit() {
    let sshd = Sshd::start(false);
    let root = agentless_root(&sshd, "permit");
    let workspace = open_agentless(&sshd, &root).await;
    let command = RemoteCommand::Argv { argv: vec!["true".to_owned()] };
    for index in 0..12 {
        tokio::time::timeout(Duration::from_secs(5), run_to_exit(&workspace, &root, &command, Some(Duration::from_secs(60))))
            .await
            .unwrap_or_else(|_| panic!("short process {index} held a channel permit after exit"));
    }
    tokio::time::timeout(Duration::from_secs(5), workspace.fs().stat(&root.to_string_lossy(), false))
        .await
        .expect("fs operation stalled behind released process permits")
        .expect("remote root stat");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_parallel_edits_never_silently_lose_updates() {
    let sshd = Sshd::start(false);
    let root = agentless_root(&sshd, "edits");
    let workspace = Arc::new(open_agentless(&sshd, &root).await);
    let file = root.join("shared.txt");
    let path = file.to_string_lossy().into_owned();
    let a = [ExactEdit { old: "alpha".to_owned(), new: "ALPHA".to_owned(), replace_all: false }];
    let b = [ExactEdit { old: "beta".to_owned(), new: "BETA".to_owned(), replace_all: false }];
    let left_key = key();
    let right_key = key();
    for trial in 0..20 {
        std::fs::write(&file, "alpha\nbeta\n").expect("fixture");
        let (left, right) = tokio::join!(
            workspace.fs().edit(EditRequest { path: &path, edits: &a, precondition: &Precondition::Any, key: &left_key }),
            workspace.fs().edit(EditRequest { path: &path, edits: &b, precondition: &Precondition::Any, key: &right_key }),
        );
        let result = std::fs::read_to_string(&file).expect("edited file");
        assert!(
            left.is_err() || right.is_err() || result == "ALPHA\nBETA\n",
            "trial {trial}: both edits reported success but one was lost: {result:?}"
        );
    }
}

#[tokio::test]
#[ignore = "starts and stops a private user-space sshd"]
async fn live_ssh_transport_failure_is_unavailable_and_conflicts_are_conflicts() {
    let mut sshd = Sshd::start(false);
    let root = sshd.remote_root("errors");
    let file = root.join("exists.txt");
    std::fs::write(&file, "x").expect("file fixture");
    let full = root.join("full");
    std::fs::create_dir(&full).expect("directory fixture");
    std::fs::write(full.join("child"), "x").expect("child fixture");
    let workspace = open_agentless(&sshd, &root).await;
    assert_eq!(workspace.fs().mkdir(&file.to_string_lossy(), &key()).await.expect_err("mkdir over file").code, ErrorCode::Conflict);
    assert_eq!(workspace.fs().remove(&full.to_string_lossy(), false, &key()).await.expect_err("nonempty rmdir").code, ErrorCode::Conflict);
    assert_eq!(workspace.fs().read(&full.to_string_lossy(), None, 10, true).await.expect_err("read directory").code, ErrorCode::Conflict);
    let connection = sshd.connect().await;
    drop(
        Command::new("ssh")
            .arg("-F")
            .arg(&sshd.config)
            .arg("-S")
            .arg(connection.control_path())
            .args(["-O", "exit", "aim-test"])
            .output()
            .expect("stop master"),
    );
    sshd.child.kill().expect("stop sshd");
    sshd.child.wait().expect("reap sshd");
    for _ in 0..20 {
        if !connection.check_master().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let path = file.to_string_lossy();
    assert_eq!(workspace.fs().read(&path, None, 10, true).await.expect_err("offline read").code, ErrorCode::Unavailable);
    assert_eq!(workspace.fs().stat(&path, false).await.expect_err("offline stat").code, ErrorCode::Unavailable);
    let new_path = root.join("new.txt").to_string_lossy().into_owned();
    let content = Content::Utf8 { text: "new".to_owned() };
    assert_eq!(
        workspace
            .fs()
            .write(WriteRequest {
                path: &new_path,
                content: &content,
                precondition: &Precondition::IfAbsent,
                create_dirs: false,
                key: &key()
            })
            .await
            .expect_err("offline write")
            .code,
        ErrorCode::Unavailable
    );
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_symlink_to_whitespace_sibling_is_denied() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("project");
    let sibling = root.with_file_name("project ");
    std::fs::create_dir(&sibling).expect("sibling fixture");
    std::os::unix::fs::symlink(&sibling, root.join("link")).expect("escape link");
    let workspace = open_agentless(&sshd, &root).await;
    let escaped = root.join("link/escaped.txt").to_string_lossy().into_owned();
    assert_eq!(
        workspace
            .fs()
            .write(WriteRequest {
                path: &escaped,
                content: &Content::Utf8 { text: "escape".to_owned() },
                precondition: &Precondition::IfAbsent,
                create_dirs: false,
                key: &key(),
            })
            .await
            .expect_err("sibling escape")
            .code,
        ErrorCode::Denied
    );
    assert!(!sibling.join("escaped.txt").exists(), "sibling outside the workspace was changed");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_write_preserves_mode_links_and_directory_type() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("semantics");
    let workspace = open_agentless(&sshd, &root).await;
    let executable = root.join("build.sh");
    std::fs::write(&executable, "#!/bin/sh\necho old\n").expect("script fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).expect("executable mode");
    let edit = [ExactEdit { old: "old".to_owned(), new: "new".to_owned(), replace_all: false }];
    workspace
        .fs()
        .edit(EditRequest { path: &executable.to_string_lossy(), edits: &edit, precondition: &Precondition::Any, key: &key() })
        .await
        .expect("edit executable");
    assert_eq!(std::fs::metadata(&executable).expect("edited mode").mode() & 0o777, 0o755);

    let target = root.join("AGENTS.md");
    let link = root.join("CLAUDE.md");
    std::fs::write(&target, "rules v1\n").expect("target fixture");
    std::os::unix::fs::symlink("AGENTS.md", &link).expect("link fixture");
    let edit = [ExactEdit { old: "v1".to_owned(), new: "v2".to_owned(), replace_all: false }];
    workspace
        .fs()
        .edit(EditRequest { path: &link.to_string_lossy(), edits: &edit, precondition: &Precondition::Any, key: &key() })
        .await
        .expect("edit in-root symlink");
    assert!(std::fs::symlink_metadata(&link).expect("link metadata").file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&target).expect("target after edit"), "rules v2\n");
    workspace
        .fs()
        .write(WriteRequest {
            path: &link.to_string_lossy(),
            content: &Content::Utf8 { text: "rules v3\n".to_owned() },
            precondition: &Precondition::Any,
            create_dirs: false,
            key: &key(),
        })
        .await
        .expect("write in-root symlink");
    assert!(std::fs::symlink_metadata(&link).expect("link metadata").file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&target).expect("target after write"), "rules v3\n");

    let directory = root.join("directory");
    std::fs::create_dir(&directory).expect("directory fixture");
    assert_eq!(
        workspace
            .fs()
            .write(WriteRequest {
                path: &directory.to_string_lossy(),
                content: &Content::Utf8 { text: "wrong".to_owned() },
                precondition: &Precondition::Any,
                create_dirs: false,
                key: &key(),
            })
            .await
            .expect_err("directory write")
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(std::fs::read_dir(&directory).expect("directory remains empty").count(), 0);
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_expired_master_does_not_fall_back_to_direct_connection() {
    let sshd = Sshd::start(false);
    let mut options = sshd.options();
    options.persist_seconds = 1;
    let mut connection = None;
    for _ in 0..30 {
        if let Ok(ready) = Connection::connect(options.clone(), None).await {
            connection = Some(ready);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let connection = connection.expect("connect");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!connection.check_master().await, "fixture master should have expired");
    let output = connection.run("printf ok", &[]).await.expect("idle connection must restore its master");
    assert_eq!(output, b"ok");
    assert!(connection.check_master().await, "the restored channel must use a master");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_stale_master_socket_recovers_without_leaked_connections() {
    let sshd = Sshd::start(false);
    let connection = sshd.connect().await;
    let socket = connection.control_path().to_path_buf();
    let check = Command::new("ssh")
        .arg("-F")
        .arg(&sshd.config)
        .arg("-S")
        .arg(&socket)
        .args(["-O", "check", "aim-test"])
        .output()
        .expect("check master pid");
    assert!(check.status.success(), "master check");
    let check_text = String::from_utf8_lossy(&check.stderr);
    let pid = check_text.split("pid=").nth(1).and_then(|tail| tail.split(|ch: char| !ch.is_ascii_digit()).next()).expect("master pid");
    assert!(!pid.is_empty(), "master pid missing");
    assert!(Command::new("kill").args(["-KILL", pid]).status().expect("kill master").success());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(socket.symlink_metadata().is_ok(), "fixture must leave a stale socket");
    let reconnected = Connection::connect(sshd.options(), None).await;
    let ps = Command::new("ps").args(["-axo", "pid=,command="]).output().expect("list ssh processes");
    let process_list = String::from_utf8_lossy(&ps.stdout);
    let leaked: Vec<_> = process_list
        .lines()
        .filter(|line| line.contains(&sshd.config.to_string_lossy().to_string()) && line.contains("ssh -F") && line.contains(" -f "))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    for pid in &leaked {
        drop(Command::new("kill").args(["-KILL", pid]).status());
    }
    assert!(reconnected.is_ok(), "stale socket prevented reconnection: {reconnected:?}");
    assert!(leaked.is_empty(), "failed master start leaked {} detached SSH processes", leaked.len());
    assert!(reconnected.expect("reconnected").check_master().await);
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_host_config_cannot_force_remote_command_or_tty() {
    let sshd = Sshd::start_with(false, "", "  RemoteCommand printf wrong\n  RequestTTY force\n");
    let connection = sshd.connect().await;
    assert_eq!(connection.run("printf ok", &[]).await.expect("channel with forced config"), b"ok");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_configured_agent_forwarding_is_disabled_for_model_channel() {
    let sshd = Sshd::start_with(false, "AllowAgentForwarding yes\n", "  ForwardAgent yes\n");
    let socket = sshd.dir.path().join("empty-agent.sock");
    let mut agent = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("empty ssh-agent");
    for _ in 0..20 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(socket.exists(), "empty agent socket was not ready");
    let wrapper = sshd.dir.path().join("ssh-with-agent");
    std::fs::write(&wrapper, format!("#!/bin/sh\nSSH_AUTH_SOCK={} exec /usr/bin/ssh \"$@\"\n", super::quote(&socket.to_string_lossy())))
        .expect("ssh wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).expect("ssh wrapper mode");
    let mut options = sshd.options();
    options.program = wrapper;
    let output = tokio::time::timeout(Duration::from_secs(10), async {
        let connection = Connection::connect(options, None).await.expect("connect with configured agent forwarding");
        connection
            .run("if [ -n \"$SSH_AUTH_SOCK\" ]; then printf forwarded; else printf none; fi", &[])
            .await
            .expect("probe remote agent socket")
    })
    .await;
    agent.kill().expect("stop empty agent");
    agent.wait().expect("reap empty agent");
    let output = output.expect("agent forwarding probe stalled");
    assert_eq!(output, b"none", "remote model command received the local agent socket");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_agentless_regex_search_matches_alternation() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("regex");
    std::fs::write(root.join("sample.txt"), "foo\nbar\n").expect("search fixture");
    let workspace = open_agentless(&sshd, &root).await;
    assert!(!workspace.caps().native_search);
    let result = workspace
        .search()
        .grep(GrepQuery {
            pattern: "foo|bar",
            path: &root.to_string_lossy(),
            globs: &[],
            case: CaseMode::Sensitive,
            fixed_strings: false,
            context: 0,
            max_matches: 10,
        })
        .await
        .expect("fallback regex search");
    assert_eq!(result.matches.len(), 2, "fallback regex silently lost alternation matches");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd with ripgrep"]
async fn live_ssh_native_search_truncates_large_results() {
    let sshd = Sshd::start(true);
    let root = sshd.remote_root("large_search");
    std::fs::write(root.join("sample.txt"), "match\n".repeat(400_000)).expect("large search fixture");
    let workspace = open_agentless(&sshd, &root).await;
    assert!(workspace.caps().native_search);
    let result = workspace
        .search()
        .grep(GrepQuery {
            pattern: "match",
            path: &root.to_string_lossy(),
            globs: &[],
            case: CaseMode::Sensitive,
            fixed_strings: true,
            context: 0,
            max_matches: 10,
        })
        .await
        .expect("bounded native search");
    assert_eq!(result.matches.len(), 10);
    assert!(result.truncated);
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_nonreading_stdin_cannot_block_process_timeout() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("stdin_timeout");
    let marker = root.join("sleep.pid");
    let workspace = open_agentless(&sshd, &root).await;
    let env = std::collections::BTreeMap::new();
    let command =
        RemoteCommand::Shell { script: format!("printf '%s' \"$$\" > {}; exec sleep 30", super::quote(&marker.to_string_lossy())) };
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec {
            command: &command,
            cwd: &root.to_string_lossy(),
            env: &env,
            pty: None,
            stdin: true,
            timeout: Some(Duration::from_millis(500)),
            key: &key(),
        })
        .await
        .expect("spawn nonreader");
    let pid = wait_for_sleep_pid(&marker).await;
    let write =
        tokio::time::timeout(Duration::from_secs(5), workspace.exec().expect("exec").write_stdin(&proc, &vec![b'x'; 8 << 20], false)).await;
    let read =
        tokio::time::timeout(Duration::from_secs(3), workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_millis(100))).await;
    let released = tokio::time::timeout(Duration::from_secs(3), workspace.exec().expect("exec").release(&proc)).await;
    let process_gone = wait_for_sleep_exit(&pid).await;
    stop_sleep_if_running(&pid);
    assert!(write.is_ok(), "write_stdin stayed blocked after process timeout");
    assert!(read.is_ok_and(|result| result.is_ok_and(|value| value.exit.is_some())), "process timeout did not become readable");
    assert!(released.is_ok_and(|result| result.is_ok()), "release stayed blocked after process timeout");
    assert!(process_gone, "nonreading remote process survived the timeout");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_timeout_kills_remote_pipeline_children() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("timeout_group");
    let marker = root.join("sleep.pid");
    let workspace = open_agentless(&sshd, &root).await;
    let command = RemoteCommand::Shell {
        script: format!("sleep 30 & child=$!; printf '%s' \"$child\" > {}; wait \"$child\"", super::quote(&marker.to_string_lossy())),
    };
    let env = std::collections::BTreeMap::new();
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec {
            command: &command,
            cwd: &root.to_string_lossy(),
            env: &env,
            pty: None,
            stdin: false,
            timeout: Some(Duration::from_millis(500)),
            key: &key(),
        })
        .await
        .expect("spawn pipeline");
    let pid = wait_for_sleep_pid(&marker).await;
    let mut timed_out = false;
    for _ in 0..20 {
        let result = workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_millis(200)).await.expect("read timeout");
        if matches!(result.exit, Some(ExitStatus::TimedOut)) {
            timed_out = true;
            break;
        }
    }
    workspace.exec().expect("exec").release(&proc).await.expect("release timed out pipeline");
    let child_gone = wait_for_sleep_exit(&pid).await;
    stop_sleep_if_running(&pid);
    assert!(timed_out, "pipeline timeout was not reported");
    assert!(child_gone, "pipeline child survived the timeout and release");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_dropping_workspace_stops_remote_processes() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("drop_group");
    let marker = root.join("sleep.pid");
    let workspace = open_agentless(&sshd, &root).await;
    let command = RemoteCommand::Shell {
        script: format!("sleep 30 & child=$!; printf '%s' \"$child\" > {}; wait \"$child\"", super::quote(&marker.to_string_lossy())),
    };
    let env = std::collections::BTreeMap::new();
    let _proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec {
            command: &command,
            cwd: &root.to_string_lossy(),
            env: &env,
            pty: None,
            stdin: false,
            timeout: None,
            key: &key(),
        })
        .await
        .expect("spawn process");
    let pid = wait_for_sleep_pid(&marker).await;
    assert!(running_sleep(&pid), "fixture child did not start");
    drop(workspace);
    let child_gone = wait_for_sleep_exit(&pid).await;
    stop_sleep_if_running(&pid);
    assert!(child_gone, "remote process survived workspace drop");
}

#[tokio::test]
#[ignore = "benchmarks a private user-space sshd on localhost"]
async fn live_ssh_large_read_and_listing_have_bounded_localhost_latency() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("roundtrips");
    let large = root.join("large.bin");
    std::fs::write(&large, vec![b'x'; 4 << 20]).expect("large fixture");
    let directory = root.join("many");
    std::fs::create_dir(&directory).expect("listing fixture");
    for index in 0..100 {
        std::fs::write(directory.join(format!("file-{index:03}.txt")), b"x").expect("listing child");
    }
    let workspace = open_agentless(&sshd, &root).await;
    let started = Instant::now();
    let read = workspace.fs().read(&large.to_string_lossy(), None, 4 << 20, true).await.expect("large read");
    let read_duration = started.elapsed();
    assert_eq!(read.content.len(), 4 << 20);
    let started = Instant::now();
    let listing = workspace
        .fs()
        .list(ListRequest { path: &directory.to_string_lossy(), limit: 100, page_token: None, include_hidden: false })
        .await
        .expect("100-entry listing");
    let list_duration = started.elapsed();
    assert_eq!(listing.entries.len(), 100);
    eprintln!("ssh_read_4m_ms={} ssh_list_100_ms={}", read_duration.as_millis(), list_duration.as_millis());
    assert!(read_duration < Duration::from_secs(2), "4 MiB SSH read took {read_duration:?}");
    assert!(list_duration < Duration::from_secs(2), "100-entry SSH listing took {list_duration:?}");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd with shasum-only PATH"]
async fn live_ssh_hashes_filenames_with_backslashes_via_stdin() {
    let sshd = Sshd::start_with_path(false, "", "", Some("/usr/bin:/bin:/usr/sbin"));
    let connection = sshd.connect().await;
    assert!(connection.run("command -v sha256sum", &[]).await.is_err(), "fixture must exclude sha256sum");
    assert!(connection.run("command -v shasum", &[]).await.is_ok(), "fixture needs shasum");
    let root = sshd.remote_root("hash_filename");
    let workspace = AgentlessWorkspace::open(connection, &root.to_string_lossy()).await.expect("open");
    let path = root.join("a\\b.txt").to_string_lossy().into_owned();
    workspace
        .fs()
        .write(WriteRequest {
            path: &path,
            content: &Content::Utf8 { text: "before".to_owned() },
            precondition: &Precondition::IfAbsent,
            create_dirs: false,
            key: &key(),
        })
        .await
        .expect("write unusual filename");
    let read = workspace.fs().read(&path, None, 100, true).await.expect("read unusual filename");
    assert_eq!(read.content.into_bytes(), b"before");
    let edit = [ExactEdit { old: "before".to_owned(), new: "after".to_owned(), replace_all: false }];
    workspace
        .fs()
        .edit(EditRequest { path: &path, edits: &edit, precondition: &Precondition::Any, key: &key() })
        .await
        .expect("edit unusual filename");
    assert_eq!(std::fs::read_to_string(path).expect("edited filename"), "after");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_root_aliases_cannot_be_removed_or_renamed() {
    let sshd = Sshd::start(false);
    let remove_root = sshd.remote_root("protected_remove");
    std::fs::write(remove_root.join("sentinel"), "keep").expect("root sentinel");
    let remove_workspace = open_agentless(&sshd, &remove_root).await;
    assert_eq!(
        remove_workspace.fs().remove(&format!("{}/", remove_root.display()), true, &key()).await.expect_err("remove root alias").code,
        ErrorCode::Denied
    );
    assert_eq!(std::fs::read_to_string(remove_root.join("sentinel")).expect("root preserved"), "keep");

    let rename_root = sshd.remote_root("protected_rename");
    let rename_workspace = open_agentless(&sshd, &rename_root).await;
    let destination = rename_root.join("moved").to_string_lossy().into_owned();
    assert_eq!(
        rename_workspace
            .fs()
            .rename(&format!("{}/", rename_root.display()), &destination, false, &key())
            .await
            .expect_err("rename root alias")
            .code,
        ErrorCode::Denied
    );
    assert!(rename_root.is_dir());
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_pty_respects_requested_rows_and_columns() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("pty_size");
    let workspace = open_agentless(&sshd, &root).await;
    let command = RemoteCommand::Argv { argv: vec!["stty".to_owned(), "size".to_owned()] };
    let env = std::collections::BTreeMap::new();
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec {
            command: &command,
            cwd: &root.to_string_lossy(),
            env: &env,
            pty: Some(aim_proto::harness::PtySize { rows: 24, cols: 80 }),
            stdin: false,
            timeout: None,
            key: &key(),
        })
        .await
        .expect("spawn pty");
    let mut output = Vec::new();
    let mut cursor = 0;
    for _ in 0..20 {
        let read = workspace.exec().expect("exec").read(&proc, cursor, 100, Duration::from_millis(200)).await.expect("read pty size");
        for chunk in read.chunks {
            cursor = chunk.seq;
            output.extend(chunk.data.into_bytes());
        }
        if read.exit.is_some() {
            break;
        }
    }
    workspace.exec().expect("exec").release(&proc).await.expect("release pty");
    assert_eq!(String::from_utf8_lossy(&output).trim(), "24 80");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_listing_follows_confined_directory_symlink() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("list_symlink");
    let directory = root.join("directory");
    std::fs::create_dir(&directory).expect("directory fixture");
    std::fs::write(directory.join("a.txt"), b"a").expect("first file");
    std::fs::write(directory.join("b.txt"), b"b").expect("second file");
    let link = root.join("link");
    std::os::unix::fs::symlink("directory", &link).expect("directory link");
    let workspace = open_agentless(&sshd, &root).await;
    let listing = workspace
        .fs()
        .list(ListRequest { path: &link.to_string_lossy(), limit: 10, page_token: None, include_hidden: false })
        .await
        .expect("list in-root symlink");
    assert_eq!(listing.entries.len(), 2);
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_pagination_after_removed_token_does_not_repeat_entries() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("list_pages");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.join(name), b"x").expect("page fixture");
    }
    let workspace = open_agentless(&sshd, &root).await;
    let first = workspace
        .fs()
        .list(ListRequest { path: &root.to_string_lossy(), limit: 2, page_token: None, include_hidden: false })
        .await
        .expect("first page");
    assert_eq!(first.entries.first().expect("first entry").name, "a.txt");
    assert_eq!(first.entries.get(1).expect("second entry").name, "b.txt");
    let token = first.next_page.expect("page token");
    std::fs::remove_file(root.join("b.txt")).expect("remove token entry");
    let second = workspace
        .fs()
        .list(ListRequest { path: &root.to_string_lossy(), limit: 1, page_token: Some(&token), include_hidden: false })
        .await
        .expect("page after removal");
    assert_eq!(second.entries.first().expect("next entry").name, "c.txt");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_output_ring_reports_only_unseen_evictions() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("ring");
    let workspace = open_agentless(&sshd, &root).await;
    let command = RemoteCommand::Shell { script: "head -c 16777216 /dev/zero".to_owned() };
    let env = std::collections::BTreeMap::new();
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec {
            command: &command,
            cwd: &root.to_string_lossy(),
            env: &env,
            pty: None,
            stdin: false,
            timeout: None,
            key: &key(),
        })
        .await
        .expect("spawn output producer");
    for _ in 0..20 {
        if workspace.exec().expect("exec").read(&proc, u64::MAX, 1, Duration::from_millis(100)).await.expect("wait producer").exit.is_some()
        {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let first = workspace.exec().expect("exec").read(&proc, 0, 9 << 20, Duration::ZERO).await.expect("read retained ring");
    let first_seq = first.chunks.first().expect("retained output").seq;
    let last_seq = first.chunks.last().expect("last output").seq;
    assert!(first_seq > 1, "fixture did not evict output from its ring");
    assert_eq!(first.dropped_before, Some(first_seq), "dropped_before must identify the first retained sequence");
    let caught_up = workspace.exec().expect("exec").read(&proc, last_seq, 100, Duration::ZERO).await.expect("read caught-up cursor");
    workspace.exec().expect("exec").release(&proc).await.expect("release output producer");
    assert_eq!(caught_up.dropped_before, None, "caught-up reader must not report past eviction");
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_short_process_read_does_not_wait_for_full_poll_timeout() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("read_wakeup");
    let workspace = open_agentless(&sshd, &root).await;
    let command = RemoteCommand::Argv { argv: vec!["printf".to_owned(), "x".to_owned()] };
    let env = std::collections::BTreeMap::new();
    for trial in 0..40 {
        let proc = workspace
            .exec()
            .expect("exec")
            .spawn(SpawnSpec {
                command: &command,
                cwd: &root.to_string_lossy(),
                env: &env,
                pty: None,
                stdin: false,
                timeout: None,
                key: &key(),
            })
            .await
            .expect("spawn short process");
        let mut cursor = 0;
        let mut finished = false;
        for _ in 0..4 {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                workspace.exec().expect("exec").read(&proc, cursor, 100, Duration::from_secs(5)),
            )
            .await;
            let Ok(Ok(read)) = result else {
                workspace.exec().expect("exec").release(&proc).await.expect("release stalled read");
                panic!("trial {trial}: read waited after short process completed");
            };
            if let Some(last) = read.chunks.last() {
                cursor = last.seq;
            }
            if read.exit.is_some() {
                finished = true;
                break;
            }
        }
        workspace.exec().expect("exec").release(&proc).await.expect("release short process");
        assert!(finished, "trial {trial}: process exit was not reported promptly");
    }
}

#[tokio::test]
#[ignore = "starts a private user-space sshd"]
async fn live_ssh_master_bootstrap_and_agentless() {
    let sshd = Sshd::start(false);
    let connected = Instant::now();
    let connection = sshd.connect().await;
    assert!(connection.check_master().await);
    let reused = Connection::connect(sshd.options(), None).await.expect("reuse master");
    assert_eq!(connection.control_path(), reused.control_path());
    eprintln!("master_ms={}", connected.elapsed().as_millis());

    let artifact_path = std::env::current_exe().expect("test binary");
    let probe = super::bootstrap::probe(&connection).await.expect("probe");
    assert_eq!(probe.home, sshd.dir.path().join("remote_home").to_string_lossy());
    let target = probe.target.expect("target").to_owned();
    let artifact = Artifact {
        path: artifact_path.clone(),
        sha256: local_sha256(&artifact_path).expect("local hash"),
        target,
        generation: 1,
        version: "live-test".to_owned(),
    };
    let started = Instant::now();
    let installed = install(&connection, &artifact, false).await.expect("install");
    eprintln!("bootstrap_ms={}", started.elapsed().as_millis());
    let remote_hash = connection.run(&format!("shasum -a 256 -- {}", super::quote(&installed)), &[]).await.expect("remote hash");
    assert!(String::from_utf8(remote_hash).expect("remote hash text").starts_with(&artifact.sha256));
    let inode = std::fs::metadata(&installed).expect("installed metadata").ino();
    assert_eq!(install(&connection, &artifact, false).await.expect("skip upload"), installed);
    assert_eq!(std::fs::metadata(&installed).expect("skipped metadata").ino(), inode);
    let mut bad_artifact = artifact.clone();
    bad_artifact.sha256 = "0".repeat(64);
    assert_eq!(
        install(&connection, &bad_artifact, false).await.expect_err("local hash mismatch"),
        super::bootstrap::BootstrapError::LocalHashMismatch
    );
    std::fs::write(&installed, b"tampered").expect("tamper remote fixture");
    assert_eq!(install(&connection, &artifact, false).await.expect("replace tampered remote"), installed);
    assert_ne!(std::fs::metadata(&installed).expect("replaced metadata").ino(), inode);

    exercise_agentless(&sshd, connection).await;
}

#[tokio::test]
#[ignore = "starts a private user-space sshd with ripgrep"]
async fn live_ssh_native_search() {
    let sshd = Sshd::start(true);
    let connection = sshd.connect().await;
    let root = sshd.remote_root("native_search");
    let local = sshd.local_root("native_search");
    std::fs::write(local.join("sample.txt"), "local sentinel\n").expect("local sentinel");
    std::fs::write(root.join("sample.txt"), "first\nmatch\nlast\n").expect("search fixture");
    let workspace = AgentlessWorkspace::open(connection, &root.to_string_lossy()).await.expect("open");
    assert!(workspace.caps().native_search);
    let globs = vec!["*.txt".to_owned()];
    let result = workspace
        .search()
        .grep(GrepQuery {
            pattern: "match",
            path: &root.to_string_lossy(),
            globs: &globs,
            case: CaseMode::Sensitive,
            fixed_strings: true,
            context: 1,
            max_matches: 10,
        })
        .await
        .expect("native grep");
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches.first().expect("match").before, vec!["first"]);
    assert_eq!(result.matches.first().expect("match").after, vec!["last"]);
    sshd.assert_local_denied(&root.join("sample.txt"));
    assert_eq!(std::fs::read_to_string(local.join("sample.txt")).expect("local sentinel"), "local sentinel\n");
}

#[tokio::test]
#[ignore = "runs the aimx binary through a private user-space sshd"]
async fn live_ssh_resident_resume_two_proxies_and_idle_exit() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("workspace");
    let local = sshd.local_root("workspace");
    std::fs::write(local.join("different.txt"), "local").expect("local fixture");
    let started = Instant::now();
    let (peer, child) = spawn_forward(&sshd, &root, "auto");
    let init = initialize(&peer, None).await;
    assert!(!init.resumed);
    let workspace = peer
        .call::<WorkspaceOpen>(WorkspaceOpenParams {
            ceiling: None,
            root: root.to_string_lossy().into_owned(),
            backend: BackendSpec::Local,
        })
        .await
        .expect("resident workspace");
    assert!(workspace.caps.resumable);
    eprintln!("resident_handshake_ms={}", started.elapsed().as_millis());
    let path = root.join("different.txt").to_string_lossy().into_owned();
    peer.call::<FsWrite>(FsWriteParams {
        scope: None,
        workspace: workspace.id.clone(),
        path: path.clone(),
        content: Content::Utf8 { text: "remote".to_owned() },
        precondition: Precondition::IfAbsent,
        create_dirs: false,
        idempotency_key: IdempotencyKey::new("resident-write"),
    })
    .await
    .expect("resident write");
    let read_only = CallScope {
        roots: vec![root.to_string_lossy().into_owned()],
        ops: vec!["read".into()],
        deny_write: Vec::new(),
        max_processes: None,
        max_output_bytes: None,
    };
    let scoped_read = peer
        .call::<FsRead>(FsReadParams {
            workspace: workspace.id.clone(),
            path: path.clone(),
            range: None,
            scope: Some(read_only.clone()),
            hash: false,
        })
        .await
        .expect("read-only scope permits SSH read");
    assert_eq!(scoped_read.content.into_bytes(), b"remote");
    assert!(scoped_read.hash.is_none());
    let denied = peer
        .call::<FsWrite>(FsWriteParams {
            workspace: workspace.id.clone(),
            path: path.clone(),
            content: Content::Utf8 { text: "blocked".to_owned() },
            precondition: Precondition::Any,
            create_dirs: false,
            idempotency_key: IdempotencyKey::new("resident-scoped-write"),
            scope: Some(read_only),
        })
        .await
        .expect_err("read-only scope denies SSH write");
    assert_eq!(denied.code, ErrorCode::Denied);
    assert_eq!(std::fs::read_to_string(&path).expect("remote file"), "remote");
    sshd.assert_local_denied(&root.join("different.txt"));
    assert_eq!(std::fs::read_to_string(local.join("different.txt")).expect("local file"), "local");
    exercise_tools(&sshd, &peer, &workspace.id, &root, &local).await;
    resume_two_proxies(&sshd, &root, peer, child, init, workspace.id).await;
}

async fn resume_two_proxies(
    sshd: &Sshd,
    root: &Path,
    peer: Peer,
    mut child: tokio::process::Child,
    init: InitializeResult,
    workspace: WorkspaceId,
) {
    let (peer_two, mut child_two) = spawn_forward(sshd, root, "auto");
    initialize(&peer_two, None).await;
    let run_dir = sshd.dir.path().join("remote_home/.aim/run");
    let pid_files = std::fs::read_dir(&run_dir)
        .expect("run dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pid"))
        .count();
    assert_eq!(pid_files, 1, "two proxies must share the resident");

    let process = peer
        .call::<ExecSpawn>(ExecSpawnParams {
            scope: None,
            workspace: workspace.clone(),
            command: RemoteCommand::Shell { script: "printf 'first'; sleep 1; printf 'second'".to_owned() },
            cwd: Some(root.to_string_lossy().into_owned()),
            env: std::collections::BTreeMap::new(),
            pty: None,
            stdin: false,
            timeout_ms: None,
            idempotency_key: IdempotencyKey::new("resident-spawn"),
        })
        .await
        .expect("spawn")
        .proc;
    let mut first_seq = 0;
    for _ in 0..8 {
        let read = peer
            .call::<ExecRead>(ExecReadParams { scope: None, proc: process.clone(), after_seq: 0, max_bytes: Some(1024), wait_ms: 500 })
            .await
            .expect("read first output");
        if let Some(chunk) = read.chunks.first() {
            first_seq = chunk.seq;
            break;
        }
    }
    assert!(first_seq > 0);
    child.kill().await.expect("kill first ssh forwarder");
    peer.close();
    let (resumed_peer, mut resumed_child) = spawn_forward(sshd, root, "auto");
    let resumed = initialize(&resumed_peer, Some(init.resume_token)).await;
    assert!(resumed.resumed);
    let mut remaining = Vec::new();
    let mut cursor = first_seq;
    for _ in 0..8 {
        let read = resumed_peer
            .call::<ExecRead>(ExecReadParams { scope: None, proc: process.clone(), after_seq: cursor, max_bytes: Some(1024), wait_ms: 500 })
            .await
            .expect("read resumed output");
        for chunk in read.chunks {
            cursor = chunk.seq;
            remaining.extend(chunk.data.into_bytes());
        }
        if read.exit.is_some() {
            break;
        }
    }
    assert_eq!(remaining, b"second");
    resumed_peer.call::<ExecRelease>(ExecReleaseParams { scope: None, proc: process }).await.expect("release");
    resumed_peer.close();
    peer_two.close();
    resumed_child.kill().await.expect("stop resumed proxy");
    child_two.kill().await.expect("stop second proxy");
    let mut gone = false;
    for _ in 0..60 {
        if std::fs::read_dir(&run_dir)
            .expect("run dir")
            .filter_map(Result::ok)
            .all(|entry| entry.path().extension().is_none_or(|ext| ext != "sock"))
        {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "resident should exit after becoming idle");
}

#[tokio::test]
#[ignore = "runs the aimx binary through a private user-space sshd"]
async fn live_ssh_agentless_stdio_fallback() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("workspace");
    let local = sshd.local_root("workspace");
    std::fs::write(local.join("fallback.txt"), "local sentinel").expect("local sentinel");
    let (peer, mut child) = spawn_forward(&sshd, &root, "never");
    initialize(&peer, None).await;
    let workspace = peer
        .call::<WorkspaceOpen>(WorkspaceOpenParams {
            ceiling: None,
            root: root.to_string_lossy().into_owned(),
            backend: BackendSpec::Local,
        })
        .await
        .expect("agentless workspace");
    assert!(!workspace.caps.resumable);
    let path = root.join("fallback.txt").to_string_lossy().into_owned();
    peer.call::<FsWrite>(FsWriteParams {
        scope: None,
        workspace: workspace.id.clone(),
        path: path.clone(),
        content: Content::Utf8 { text: "remote-only".to_owned() },
        precondition: Precondition::IfAbsent,
        create_dirs: false,
        idempotency_key: IdempotencyKey::new("fallback-write"),
    })
    .await
    .expect("agentless write");
    let read = peer
        .call::<FsRead>(FsReadParams { scope: None, hash: true, workspace: workspace.id.clone(), path, range: None })
        .await
        .expect("agentless read");
    assert_eq!(read.content.into_bytes(), b"remote-only");
    sshd.assert_local_denied(&root.join("fallback.txt"));
    assert_eq!(std::fs::read_to_string(local.join("fallback.txt")).expect("local sentinel"), "local sentinel");
    exercise_tools(&sshd, &peer, &workspace.id, &root, &local).await;
    peer.close();
    child.kill().await.expect("stop fallback");
}

#[tokio::test]
#[ignore = "kills one OpenSSH channel in a private user-space sshd"]
async fn live_ssh_transparent_reconnect_after_channel_killed() {
    let sshd = Sshd::start(false);
    let root = sshd.remote_root("reconnect_workspace");
    let (peer, mut child) = spawn_forward(&sshd, &root, "auto");
    initialize(&peer, None).await;
    let workspace = peer
        .call::<WorkspaceOpen>(WorkspaceOpenParams {
            ceiling: None,
            root: root.to_string_lossy().into_owned(),
            backend: BackendSpec::Local,
        })
        .await
        .expect("open");
    let proc = peer
        .call::<ExecSpawn>(ExecSpawnParams {
            scope: None,
            workspace: workspace.id,
            command: RemoteCommand::Shell { script: "printf first; sleep 2; printf second".to_owned() },
            cwd: Some(root.to_string_lossy().into_owned()),
            env: std::collections::BTreeMap::new(),
            pty: None,
            stdin: false,
            timeout_ms: None,
            idempotency_key: IdempotencyKey::new("reconnect-process"),
        })
        .await
        .expect("spawn")
        .proc;
    let first = peer
        .call::<ExecRead>(ExecReadParams { scope: None, proc: proc.clone(), after_seq: 0, max_bytes: Some(1024), wait_ms: 1000 })
        .await
        .expect("first output");
    let first_seq = first.chunks.first().expect("first chunk").seq;
    assert_eq!(first.chunks.first().expect("first chunk").data.clone().into_bytes(), b"first");
    let started = Instant::now();
    kill_ssh_child(&child);
    let mut cursor = first_seq;
    let mut rest = Vec::new();
    for _ in 0..8 {
        let read = peer
            .call::<ExecRead>(ExecReadParams { scope: None, proc: proc.clone(), after_seq: cursor, max_bytes: Some(1024), wait_ms: 1000 })
            .await
            .expect("read through reconnection");
        for chunk in read.chunks {
            cursor = chunk.seq;
            rest.extend(chunk.data.into_bytes());
        }
        if read.exit.is_some() {
            break;
        }
    }
    assert_eq!(rest, b"second");
    assert!(child.try_wait().expect("forwarder status").is_none(), "local aimx stream must stay alive");
    eprintln!("transparent_reconnect_ms={}", started.elapsed().as_millis());
    peer.call::<ExecRelease>(ExecReleaseParams { scope: None, proc }).await.expect("release");
    peer.close();
    child.kill().await.expect("stop forwarder");
}

#[tokio::test]
#[ignore = "uses a real OpenRouter turn through a private user-space sshd"]
async fn live_ssh_aim_run_openrouter_edits_and_executes_remote_file() {
    assert!(std::env::var_os("OPENROUTER_API_KEY").is_some(), "OpenRouter credential is required for this live test");
    let sshd = Sshd::start(false);
    let remote = sshd.remote_root("provider_workspace");
    let local = sshd.local_root("provider_workspace");
    std::fs::write(local.join("probe.sh"), "local sentinel\n").expect("local sentinel");
    let started = Instant::now();
    let output = Command::new("sandbox-exec")
        .arg("-f")
        .arg(&sshd.sandbox)
        .arg(aim_binary())
        .arg("run")
        .args(["--ssh", "aim-test", "-p", "openrouter", "--model", "openai/gpt-4.1-mini", "--ephemeral", "--max-requests", "6", "--aimx"])
        .arg(aimx_binary())
        .arg("-C")
        .arg(&remote)
        .current_dir(&local)
        .arg("Use Write to create probe.sh containing exactly '#!/bin/sh\nprintf ssh-ok\n'. Then use Bash to run 'sh probe.sh'. Reply with the command output.")
        .env("AIM_SSH_CONFIG", &sshd.config)
        .output()
        .expect("aim run");
    eprintln!("aim_ssh_openrouter_turn_ms={}", started.elapsed().as_millis());
    assert!(output.status.success(), "aim run status: {}", output.status);
    assert_eq!(std::fs::read_to_string(remote.join("probe.sh")).expect("remote script"), "#!/bin/sh\nprintf ssh-ok\n");
    sshd.assert_local_denied(&remote.join("probe.sh"));
    assert_eq!(std::fs::read_to_string(local.join("probe.sh")).expect("local sentinel"), "local sentinel\n");
    assert!(String::from_utf8_lossy(&output.stdout).contains("ssh-ok"), "agent should report execution output");
}

async fn exercise_agentless(sshd: &Sshd, connection: Connection) {
    let remote_root = sshd.remote_root("agentless");
    let local_root = sshd.local_root("agentless");
    std::fs::write(local_root.join("sample.txt"), "local sentinel").expect("local sentinel");
    let workspace = AgentlessWorkspace::open(connection, &remote_root.to_string_lossy()).await.expect("open");
    assert!(!workspace.caps().native_search);
    let file = remote_path(&remote_root, "sample.txt");
    let content = Content::Utf8 { text: "first\nbefore\nlast\n".to_owned() };
    let write = timed(
        "write",
        workspace.fs().write(WriteRequest {
            path: &file,
            content: &content,
            precondition: &Precondition::IfAbsent,
            create_dirs: false,
            key: &key(),
        }),
    )
    .await
    .expect("write");
    assert!(write.created);
    assert_eq!(std::fs::read_to_string(local_root.join("sample.txt")).expect("local sentinel"), "local sentinel");
    assert!(timed("stat", workspace.fs().stat(&file, true)).await.expect("stat").hash.is_some());
    let read = timed("read", workspace.fs().read(&file, None, 100, true)).await.expect("read");
    assert_eq!(read.content, content);
    let prefix = workspace.fs().read(&file, None, 4, false).await.expect("prefix read");
    assert_eq!(prefix.content.into_bytes(), b"firs");
    assert_eq!(prefix.size, content.len() as u64);
    assert!(prefix.hash.is_none());
    assert!(prefix.truncated);
    let past_end = workspace
        .fs()
        .read(&file, Some(aim_proto::harness::ByteRange { start: u64::MAX, len: 4 }), 4, false)
        .await
        .expect("out-of-range prefix read");
    assert!(past_end.content.into_bytes().is_empty());
    assert!(!past_end.truncated);
    let edit = ExactEdit { old: "before".to_owned(), new: "after".to_owned(), replace_all: false };
    timed(
        "edit",
        workspace.fs().edit(EditRequest {
            path: &file,
            edits: &[edit],
            precondition: &Precondition::IfHash { hash: read.hash.expect("requested hash") },
            key: &key(),
        }),
    )
    .await
    .expect("edit");
    assert_eq!(
        timed(
            "list",
            workspace.fs().list(ListRequest { path: &remote_root.to_string_lossy(), limit: 20, page_token: None, include_hidden: false })
        )
        .await
        .expect("list")
        .entries
        .len(),
        1
    );
    let grep = timed(
        "grep",
        workspace.search().grep(GrepQuery {
            pattern: "after",
            path: &remote_root.to_string_lossy(),
            globs: &[],
            case: CaseMode::Sensitive,
            fixed_strings: true,
            context: 1,
            max_matches: 10,
        }),
    )
    .await
    .expect("grep");
    assert_eq!(grep.matches.len(), 1);
    assert_eq!(grep.matches.first().expect("match").before, vec!["first"]);
    assert_eq!(grep.matches.first().expect("match").after, vec!["last"]);
    let patterns = vec!["*.txt".to_owned()];
    let glob =
        timed("glob", workspace.search().glob(GlobQuery { patterns: &patterns, path: &remote_root.to_string_lossy(), max_results: 10 }))
            .await
            .expect("glob");
    assert_eq!(glob.paths, vec!["sample.txt"]);
    let renamed = remote_path(&remote_root, "renamed.txt");
    timed("rename", workspace.fs().rename(&file, &renamed, false, &key())).await.expect("rename");
    timed("remove", workspace.fs().remove(&renamed, false, &key())).await.expect("remove");
    let subdir = remote_path(&remote_root, "subdir");
    timed("mkdir", workspace.fs().mkdir(&subdir, &key())).await.expect("mkdir");
    assert!(Path::new(&subdir).is_dir());
    let env = std::collections::BTreeMap::new();
    let proc = timed(
        "exec_spawn",
        workspace.exec().expect("exec").spawn(SpawnSpec {
            command: &RemoteCommand::Shell { script: "printf remote-exec".to_owned() },
            cwd: &remote_root.to_string_lossy(),
            env: &env,
            pty: None,
            stdin: false,
            timeout: None,
            key: &key(),
        }),
    )
    .await
    .expect("spawn");
    let result = timed("exec_read", workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_secs(3))).await.expect("exec read");
    assert!(!result.chunks.is_empty());
    workspace.exec().expect("exec").release(&proc).await.expect("release");
    assert_eq!(std::fs::read_to_string(local_root.join("sample.txt")).expect("local sentinel"), "local sentinel");
    exercise_edge_cases(&workspace, &remote_root, &local_root).await;
}

async fn exercise_edge_cases(workspace: &AgentlessWorkspace, remote_root: &Path, local_root: &Path) {
    let binary_path = remote_path(remote_root, "binary.dat");
    let binary = Content::from_bytes(vec![0, 255, 65]);
    workspace
        .fs()
        .write(WriteRequest {
            path: &binary_path,
            content: &binary,
            precondition: &Precondition::IfAbsent,
            create_dirs: false,
            key: &key(),
        })
        .await
        .expect("binary write");
    let range =
        workspace.fs().read(&binary_path, Some(aim_proto::harness::ByteRange { start: 1, len: 2 }), 2, true).await.expect("range read");
    assert_eq!(range.content.into_bytes(), vec![255, 65]);
    let inside = remote_root.join("inside-link");
    std::os::unix::fs::symlink(&binary_path, &inside).expect("inside symlink fixture");
    assert_eq!(
        workspace.fs().read(&inside.to_string_lossy(), None, 10, true).await.expect("read inside symlink").content.into_bytes(),
        binary.clone().into_bytes()
    );
    let stale = Precondition::IfHash { hash: aim_proto::harness::ContentHash(format!("sha256:{}", "0".repeat(64))) };
    assert_eq!(
        workspace
            .fs()
            .write(WriteRequest { path: &binary_path, content: &binary, precondition: &stale, create_dirs: false, key: &key() })
            .await
            .expect_err("stale write")
            .code,
        ErrorCode::PreconditionFailed
    );
    let nested = remote_path(remote_root, "nested/child/file.txt");
    workspace
        .fs()
        .write(WriteRequest {
            path: &nested,
            content: &Content::Utf8 { text: "nested".to_owned() },
            precondition: &Precondition::IfAbsent,
            create_dirs: true,
            key: &key(),
        })
        .await
        .expect("nested write");
    assert!(Path::new(&nested).exists());
    let nested_patterns = vec!["*.txt".to_owned()];
    assert_eq!(
        workspace
            .search()
            .glob(GlobQuery { patterns: &nested_patterns, path: &remote_path(remote_root, "nested/child"), max_results: 10 })
            .await
            .expect("nested glob")
            .paths,
        vec!["nested/child/file.txt"]
    );
    let empty = remote_path(remote_root, "empty");
    workspace.fs().mkdir(&empty, &key()).await.expect("empty dir");
    workspace.fs().remove(&empty, false, &key()).await.expect("nonrecursive rmdir");
    let outside = remote_root.join("outside");
    std::os::unix::fs::symlink(local_root, &outside).expect("symlink fixture");
    assert_eq!(
        workspace.fs().stat(&outside.to_string_lossy(), false).await.expect("lstat symlink").kind,
        aim_proto::harness::EntryKind::Symlink
    );
    assert!(
        workspace
            .fs()
            .list(ListRequest { path: &remote_root.to_string_lossy(), limit: 100, page_token: None, include_hidden: true })
            .await
            .expect("list symlink")
            .entries
            .iter()
            .any(|entry| entry.name == "outside")
    );
    let escaped = outside.join("escape.txt").to_string_lossy().into_owned();
    assert_eq!(
        workspace
            .fs()
            .write(WriteRequest { path: &escaped, content: &binary, precondition: &Precondition::Any, create_dirs: false, key: &key() })
            .await
            .expect_err("symlink escape")
            .code,
        ErrorCode::Denied
    );
    assert!(!local_root.join("escape.txt").exists());
    workspace.fs().remove(&remote_path(remote_root, "nested"), true, &key()).await.expect("recursive remove");
    exercise_exec_io(workspace, &remote_root.to_string_lossy()).await;
}

async fn exercise_exec_io(workspace: &AgentlessWorkspace, root: &str) {
    let env = std::collections::BTreeMap::new();
    let command = RemoteCommand::Shell { script: "cat".to_owned() };
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec { command: &command, cwd: root, env: &env, pty: None, stdin: true, timeout: None, key: &key() })
        .await
        .expect("stdin spawn");
    timed("exec_stdin", workspace.exec().expect("exec").write_stdin(&proc, b"stdin-data", true)).await.expect("stdin write");
    let result = workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_secs(3)).await.expect("stdin read");
    assert!(result.chunks.iter().flat_map(|chunk| chunk.data.clone().into_bytes()).collect::<Vec<_>>().starts_with(b"stdin-data"));
    workspace.exec().expect("exec").release(&proc).await.expect("stdin release");
    exercise_exec_controls(workspace, root).await;
}

async fn exercise_exec_controls(workspace: &AgentlessWorkspace, root: &str) {
    let env = std::collections::BTreeMap::new();
    let sleep = RemoteCommand::Argv { argv: vec!["sleep".to_owned(), "10".to_owned()] };
    let process_key = key();
    let spec = |timeout| SpawnSpec { command: &sleep, cwd: root, env: &env, pty: None, stdin: false, timeout, key: &process_key };
    let proc = workspace.exec().expect("exec").spawn(spec(None)).await.expect("sleep spawn");
    timed("exec_signal", workspace.exec().expect("exec").signal(&proc, aim_proto::harness::Signal::Terminate))
        .await
        .expect("remote signal");
    let mut ended = false;
    for _ in 0..10 {
        if workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_millis(200)).await.expect("signal read").exit.is_some() {
            ended = true;
            break;
        }
    }
    assert!(ended, "remote signal must stop the process");
    workspace.exec().expect("exec").release(&proc).await.expect("signal release");

    let proc = workspace.exec().expect("exec").spawn(spec(Some(Duration::from_millis(100)))).await.expect("timeout spawn");
    let mut timed_out = false;
    for _ in 0..10 {
        if matches!(
            workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_millis(200)).await.expect("timeout read").exit,
            Some(ExitStatus::TimedOut)
        ) {
            timed_out = true;
            break;
        }
    }
    assert!(timed_out, "timeout must stop the remote process");
    workspace.exec().expect("exec").release(&proc).await.expect("timeout release");

    let command = RemoteCommand::Argv { argv: vec!["printf".to_owned(), "pty-ok".to_owned()] };
    let proc = workspace
        .exec()
        .expect("exec")
        .spawn(SpawnSpec {
            command: &command,
            cwd: root,
            env: &env,
            pty: Some(aim_proto::harness::PtySize { rows: 24, cols: 80 }),
            stdin: false,
            timeout: None,
            key: &key(),
        })
        .await
        .expect("pty spawn");
    let output = workspace.exec().expect("exec").read(&proc, 0, 100, Duration::from_secs(3)).await.expect("pty read");
    assert!(output.chunks.iter().any(|chunk| chunk.stream == aim_proto::harness::OutputStream::Pty));
    assert_eq!(
        workspace
            .exec()
            .expect("exec")
            .resize(&proc, aim_proto::harness::PtySize { rows: 40, cols: 100 })
            .await
            .expect_err("agentless resize unavailable")
            .code,
        ErrorCode::Unavailable
    );
    workspace.exec().expect("exec").release(&proc).await.expect("pty release");
}
