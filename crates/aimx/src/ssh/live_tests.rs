//! Live tests use a private, unprivileged sshd and never edit SSH user configuration.

use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aim_proto::content::Content;
use aim_proto::harness::{CaseMode, Command as RemoteCommand, Precondition};
use aim_proto::ids::IdempotencyKey;
use tempfile::TempDir;

use super::agentless::AgentlessWorkspace;
use super::bootstrap::{Artifact, install, local_sha256};
use super::conn::{Connection, SshOptions};
use crate::workspace::{EditRequest, GlobQuery, GrepQuery, ListRequest, SpawnSpec, Workspace, WriteRequest};

struct Sshd {
    dir: TempDir,
    child: Child,
    config: PathBuf,
}

impl Sshd {
    fn start(native_search: bool) -> Self {
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
        let rg_path = if native_search {
            let output = Command::new("mise").args(["which", "rg"]).output().expect("mise which rg");
            assert!(output.status.success(), "ripgrep must be installed through mise");
            let path = PathBuf::from(String::from_utf8(output.stdout).expect("rg path").trim());
            path.parent().expect("rg directory").to_string_lossy().into_owned()
        } else {
            String::new()
        };
        let path = format!("{rg_path}:/usr/bin:/bin:/usr/sbin:/sbin");
        std::fs::write(&sshd_config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\nPidFile {}\nSetEnv HOME={} PATH={path}\n", host.display(), dir.path().join("authorized_keys").display(), dir.path().join("sshd.pid").display(), remote_home.display())).expect("sshd config");
        let pubkey = std::fs::read_to_string(host.with_extension("pub")).expect("host public key");
        std::fs::write(dir.path().join("known_hosts"), format!("[127.0.0.1]:{port} {pubkey}")).expect("known_hosts");
        std::fs::write(&config, format!("Host aim-test\n  HostName 127.0.0.1\n  Port {port}\n  User {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  LogLevel ERROR\n", std::env::var("USER").expect("user"), client.display(), dir.path().join("known_hosts").display())).expect("ssh config");
        let check = Command::new("/usr/sbin/sshd").args(["-t", "-f"]).arg(&sshd_config).output().expect("sshd config check");
        assert!(check.status.success(), "sshd config invalid: {}", String::from_utf8_lossy(&check.stderr));
        let child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sshd");
        Self { dir, child, config }
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
        drop(
            Command::new("ssh")
                .arg("-F")
                .arg(&self.config)
                .arg("-S")
                .arg(self.dir.path().join("ctl/%C"))
                .args(["-O", "exit", "aim-test"])
                .output(),
        );
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn key() -> IdempotencyKey {
    IdempotencyKey::new("live-test-key")
}
fn remote_path(root: &Path, name: &str) -> String {
    root.join(name).to_string_lossy().into_owned()
}

async fn timed<T>(name: &str, operation: impl Future<Output = T>) -> T {
    let started = Instant::now();
    let result = operation.await;
    eprintln!("{name}_ms={}", started.elapsed().as_millis());
    result
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
    let root = sshd.dir.path().join("native_search");
    std::fs::create_dir(&root).expect("root");
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
}

async fn exercise_agentless(sshd: &Sshd, connection: Connection) {
    let remote_root = sshd.dir.path().join("remote");
    let local_root = sshd.dir.path().join("local");
    std::fs::create_dir_all(&remote_root).expect("remote root");
    std::fs::create_dir_all(&local_root).expect("local root");
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
    assert!(!local_root.join("sample.txt").exists(), "local workspace must remain untouched");
    assert!(timed("stat", workspace.fs().stat(&file, true)).await.expect("stat").hash.is_some());
    let read = timed("read", workspace.fs().read(&file, None, 100)).await.expect("read");
    assert_eq!(read.content, content);
    let edit = aim_proto::harness::ExactEdit { old: "before".to_owned(), new: "after".to_owned(), replace_all: false };
    timed(
        "edit",
        workspace.fs().edit(EditRequest {
            path: &file,
            edits: &[edit],
            precondition: &Precondition::IfHash { hash: read.hash },
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
    assert!(!local_root.join("sample.txt").exists());
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
    let range = workspace.fs().read(&binary_path, Some(aim_proto::harness::ByteRange { start: 1, len: 2 }), 2).await.expect("range read");
    assert_eq!(range.content.into_bytes(), vec![255, 65]);
    let inside = remote_root.join("inside-link");
    std::os::unix::fs::symlink(&binary_path, &inside).expect("inside symlink fixture");
    assert_eq!(
        workspace.fs().read(&inside.to_string_lossy(), None, 10).await.expect("read inside symlink").content.into_bytes(),
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
        aim_proto::error::ErrorCode::PreconditionFailed
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
        aim_proto::error::ErrorCode::Denied
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
            Some(aim_proto::harness::ExitStatus::TimedOut)
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
        aim_proto::error::ErrorCode::Unavailable
    );
    workspace.exec().expect("exec").release(&proc).await.expect("pty release");
}
