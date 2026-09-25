//! The real `aimx` binary: `version`, `serve --unix` and `serve --stdio`, plus the live smoke run
//! against this repository (ADR 0022: `cargo test -p aimx -- --ignored live_`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    CaseMode, Command, ExecRead, ExecReadParams, ExecSpawn, ExecSpawnParams, FsRead, FsReadParams, FsWrite, FsWriteParams, Glob,
    GlobParams, Grep, GrepParams, Precondition, ToolsCall, ToolsCallParams,
};
use serde_json::json;

use crate::common::{Client, client_over, connect, initialize, key, open, text};

const AIMX: &str = env!("CARGO_BIN_EXE_aimx");

fn serve_unix(socket: &Path, root: &Path, read_only: bool) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(AIMX);
    command.args(["serve", "--unix"]).arg(socket).arg("--root").arg(root).env("AIMX_LOG", "warn").kill_on_drop(true);
    if read_only {
        command.arg("--read-only");
    }
    command.spawn().unwrap()
}

async fn wait_for(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while tokio::net::UnixStream::connect(socket).await.is_err() {
        assert!(Instant::now() < deadline, "aimx did not start listening");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn version_names_the_generations() {
    let out = std::process::Command::new(AIMX).arg("version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("aimx "), "{stdout}");
    assert!(stdout.contains("aim-harness generations 1..=1"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_unix_binary_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir(&root).unwrap();
    // The socket's parent does not exist yet: aimx creates it with mode 0700.
    let socket = dir.path().join("run/aimx.sock");
    let mut child = serve_unix(&socket, &root, false);
    wait_for(&socket).await;
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(socket.parent().unwrap()).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o700);

    let client = connect(&socket).await;
    initialize(&client, None).await;
    let ws = open(&client, &root).await;
    let wrote = client
        .peer
        .call::<ToolsCall>(ToolsCallParams {
            scope: None,
            workspace: ws.clone(),
            name: "Bash".into(),
            arguments: json!({"command": "echo from-binary > out.txt && cat out.txt"}),
            idempotency_key: Some(key()),
        })
        .await
        .unwrap();
    assert!(!wrote.is_error);
    assert_eq!(std::fs::read_to_string(root.join("out.txt")).unwrap(), "from-binary\n");

    // A second server on the same socket is refused while the first is alive.
    let second = std::process::Command::new(AIMX)
        .args(["serve", "--unix"])
        .arg(&socket)
        .arg("--root")
        .arg(&root)
        .env("AIMX_LOG", "off")
        .output()
        .unwrap();
    assert!(!second.status.success());

    // A stale socket (the server died) is replaced on restart.
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let mut restarted = serve_unix(&socket, &root, true);
    wait_for(&socket).await;
    let client = connect(&socket).await;
    let init = initialize(&client, None).await;
    assert!(init.principal.read_only);
    let ws = open(&client, &root).await;
    let params = FsWriteParams {
        scope: None,
        workspace: ws,
        path: "out.txt".into(),
        content: text("x"),
        precondition: Precondition::Any,
        create_dirs: false,
        idempotency_key: key(),
    };
    assert_eq!(client.peer.call::<FsWrite>(params).await.unwrap_err().code, ErrorCode::Denied);
    restarted.kill().await.unwrap();
}

fn stdio_client(root: &Path) -> (tokio::process::Child, Client) {
    let mut child = tokio::process::Command::new(AIMX)
        .args(["serve", "--stdio", "--root"])
        .arg(root)
        .env("AIMX_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    (child, client_over(stdout, stdin))
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_stdio_binary_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, client) = stdio_client(dir.path());
    initialize(&client, None).await;
    let ws = open(&client, dir.path()).await;
    let params = FsWriteParams {
        scope: None,
        workspace: ws.clone(),
        path: "f".into(),
        content: text("over stdio"),
        precondition: Precondition::IfAbsent,
        create_dirs: false,
        idempotency_key: key(),
    };
    client.peer.call::<FsWrite>(params).await.unwrap();
    let read =
        client.peer.call::<FsRead>(FsReadParams { scope: None, hash: true, workspace: ws, path: "f".into(), range: None }).await.unwrap();
    assert_eq!(read.content.into_bytes(), b"over stdio");
    // Closing stdin ends the server cleanly.
    client.peer.close();
    drop(client);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
    assert!(status.success());
}

fn percentile(samples: &mut [Duration], p: usize) -> Duration {
    samples.sort();
    samples[(samples.len() * p / 100).min(samples.len() - 1)]
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Live smoke (ADR 0022): the real binary over a real socket, on this repository (read-only) and a
/// scratch workspace (read-write), with latencies.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live smoke: run with `cargo test -p aimx -- --ignored live_`"]
async fn live_binary_on_this_repository() {
    let repo = repo_root();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("live.sock");
    let started = Instant::now();
    let mut child = serve_unix(&socket, &repo, true);
    wait_for(&socket).await;
    let startup = started.elapsed();

    let client = connect(&socket).await;
    let t = Instant::now();
    initialize(&client, None).await;
    let handshake = t.elapsed();
    let ws = open(&client, &repo).await;

    let mut reads = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        let read = client
            .peer
            .call::<FsRead>(FsReadParams { scope: None, hash: true, workspace: ws.clone(), path: "Cargo.toml".into(), range: None })
            .await
            .unwrap();
        reads.push(t.elapsed());
        assert!(read.size > 0);
    }
    let t = Instant::now();
    let grep = client
        .peer
        .call::<Grep>(GrepParams {
            scope: None,
            workspace: ws.clone(),
            pattern: "pub fn confine".into(),
            path: None,
            globs: vec!["*.rs".into()],
            case: CaseMode::Sensitive,
            fixed_strings: true,
            context: 0,
            max_matches: None,
        })
        .await
        .unwrap();
    let grep_time = t.elapsed();
    assert!(grep.matches.iter().any(|m| m.path == "crates/aimx/src/authz/confine.rs"), "{grep:?}");
    assert!(grep.matches.iter().all(|m| !m.path.starts_with("target/")), "target/ is gitignored");
    let t = Instant::now();
    let glob = client
        .peer
        .call::<Glob>(GlobParams {
            scope: None,
            workspace: ws.clone(),
            patterns: vec!["**/*.rs".into()],
            path: None,
            max_results: Some(100_000),
        })
        .await
        .unwrap();
    let glob_time = t.elapsed();
    assert!(glob.paths.len() > 20);
    let tool = client
        .peer
        .call::<ToolsCall>(ToolsCallParams {
            scope: None,
            workspace: ws.clone(),
            name: "Grep".into(),
            arguments: json!({"pattern": "LOCKED\\(ADR-0005\\)", "output_mode": "files_with_matches"}),
            idempotency_key: None,
        })
        .await
        .unwrap();
    assert!(!tool.is_error);
    let spawn = ExecSpawnParams {
        scope: None,
        workspace: ws,
        command: Command::Shell { script: "true".into() },
        cwd: None,
        env: std::collections::BTreeMap::default(),
        pty: None,
        stdin: false,
        timeout_ms: None,
        idempotency_key: key(),
    };
    assert_eq!(client.peer.call::<ExecSpawn>(spawn).await.unwrap_err().code, ErrorCode::Denied, "read-only principal");
    child.kill().await.unwrap();

    // Read-write scratch workspace: process round trips.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let socket = dir.path().join("rw.sock");
    let mut child = serve_unix(&socket, &scratch, false);
    wait_for(&socket).await;
    let client = connect(&socket).await;
    initialize(&client, None).await;
    let ws = open(&client, &scratch).await;
    let mut spawns = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        let proc = client
            .peer
            .call::<ExecSpawn>(ExecSpawnParams {
                scope: None,
                workspace: ws.clone(),
                command: Command::Argv { argv: vec!["true".into()] },
                cwd: None,
                env: std::collections::BTreeMap::default(),
                pty: None,
                stdin: false,
                timeout_ms: None,
                idempotency_key: key(),
            })
            .await
            .unwrap()
            .proc;
        loop {
            let read = client
                .peer
                .call::<ExecRead>(ExecReadParams { scope: None, proc: proc.clone(), after_seq: 0, max_bytes: None, wait_ms: 5000 })
                .await
                .unwrap();
            if read.exit.is_some() {
                break;
            }
        }
        spawns.push(t.elapsed());
    }
    let mut writes = Vec::new();
    for i in 0..100 {
        let t = Instant::now();
        client
            .peer
            .call::<FsWrite>(FsWriteParams {
                scope: None,
                workspace: ws.clone(),
                path: format!("f{i}"),
                content: text("payload"),
                precondition: Precondition::Any,
                create_dirs: false,
                idempotency_key: key(),
            })
            .await
            .unwrap();
        writes.push(t.elapsed());
    }
    child.kill().await.unwrap();

    eprintln!("live aimx: startup {startup:?}, initialize {handshake:?}");
    eprintln!("live aimx: fs.read Cargo.toml p50 {:?} p99 {:?} (n=200)", percentile(&mut reads, 50), percentile(&mut reads, 99));
    eprintln!("live aimx: fs.write (fsync) p50 {:?} p99 {:?} (n=100)", percentile(&mut writes, 50), percentile(&mut writes, 99));
    eprintln!("live aimx: spawn+exit `true` p50 {:?} p99 {:?} (n=50)", percentile(&mut spawns, 50), percentile(&mut spawns, 99));
    eprintln!(
        "live aimx: grep repo {grep_time:?} ({} matches), glob **/*.rs {glob_time:?} ({} paths)",
        grep.matches.len(),
        glob.paths.len()
    );
}
