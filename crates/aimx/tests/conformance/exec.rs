//! `exec.*`: pipes and ptys, exit statuses, timeouts, signals, stdin, seq-ordered output, catch-up
//! reads, ring-buffer drops and pushed notifications.

use std::time::{Duration, Instant};

use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    Command, ExecExitedParams, ExecOutputParams, ExecRead, ExecReadParams, ExecRelease, ExecReleaseParams, ExecResize, ExecResizeParams,
    ExecSignal, ExecSignalParams, ExecSpawn, ExecSpawnParams, ExecWriteStdin, ExecWriteStdinParams, ExitStatus, OutputChunk, OutputStream,
    PtySize, Signal,
};
use aim_proto::ids::{ProcId, WorkspaceId};

use crate::common::{Client, env, env_with, key, session, text};

fn spec(ws: &WorkspaceId, command: Command) -> ExecSpawnParams {
    ExecSpawnParams {
        workspace: ws.clone(),
        command,
        cwd: None,
        env: std::collections::BTreeMap::default(),
        pty: None,
        stdin: false,
        timeout_ms: None,
        idempotency_key: key(),
    }
}

fn sh(script: &str) -> Command {
    Command::Shell { script: script.into() }
}

async fn spawn(client: &Client, params: ExecSpawnParams) -> ProcId {
    client.peer.call::<ExecSpawn>(params).await.unwrap().proc
}

async fn read(client: &Client, proc: &ProcId, after_seq: u64, wait_ms: u64) -> aim_proto::harness::ExecReadResult {
    client.peer.call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq, max_bytes: None, wait_ms }).await.unwrap()
}

/// Reads until the process exits; returns every chunk and the exit status.
async fn run_to_exit(client: &Client, proc: &ProcId) -> (Vec<OutputChunk>, ExitStatus) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut chunks = Vec::new();
    let mut cursor = 0;
    loop {
        assert!(Instant::now() < deadline, "process did not exit");
        let result = read(client, proc, cursor, 5000).await;
        for chunk in result.chunks {
            assert!(chunk.seq > cursor, "seq must increase");
            cursor = chunk.seq;
            chunks.push(chunk);
        }
        if let Some(exit) = result.exit {
            return (chunks, exit);
        }
    }
}

fn joined(chunks: &[OutputChunk], stream: OutputStream) -> String {
    chunks.iter().filter(|c| c.stream == stream).map(|c| String::from_utf8(c.data.clone().into_bytes()).unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn stdout_stderr_and_exit_codes() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let proc = spawn(&client, spec(&ws, sh("echo out; echo err >&2; exit 3"))).await;
    let (chunks, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Exited { code: 3 });
    assert_eq!(joined(&chunks, OutputStream::Stdout), "out\n");
    assert_eq!(joined(&chunks, OutputStream::Stderr), "err\n");
    let seqs: Vec<u64> = chunks.iter().map(|c| c.seq).collect();
    assert_eq!(seqs, (1..=chunks.len() as u64).collect::<Vec<_>>(), "one gapless seq across both streams");

    let argv = Command::Argv { argv: vec!["printf".into(), "%s-%s".into(), "a".into(), "b c".into()] };
    let mut params = spec(&ws, argv);
    params.env.insert("IGNORED".into(), "1".into());
    let proc = spawn(&client, params).await;
    let (chunks, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Exited { code: 0 });
    assert_eq!(joined(&chunks, OutputStream::Stdout), "a-b c");

    // cwd and env are honoured.
    std::fs::create_dir(env.path("sub")).unwrap();
    let mut params = spec(&ws, sh("pwd; echo $GREETING"));
    params.cwd = Some("sub".into());
    params.env.insert("GREETING".into(), "hi".into());
    let proc = spawn(&client, params).await;
    let (chunks, _) = run_to_exit(&client, &proc).await;
    let out = joined(&chunks, OutputStream::Stdout);
    assert!(out.ends_with("/ws/sub\nhi\n"), "{out}");

    let missing =
        client.peer.call::<ExecSpawn>(spec(&ws, Command::Argv { argv: vec!["definitely-not-a-program".into()] })).await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    let empty = client.peer.call::<ExecSpawn>(spec(&ws, Command::Argv { argv: vec![] })).await.unwrap_err();
    assert_eq!(empty.code, ErrorCode::InvalidParams);
}

#[tokio::test(flavor = "multi_thread")]
async fn timeouts_kill_the_process_group() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let mut params = spec(&ws, sh("sleep 30 & echo $!; wait"));
    params.timeout_ms = Some(300);
    let started = Instant::now();
    let proc = spawn(&client, params).await;
    let (chunks, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
    let grandchild: i32 = joined(&chunks, OutputStream::Stdout).trim().parse().unwrap();
    assert!(!alive(grandchild), "the background grandchild was killed with its group");
}

/// Whether `pid` still exists, allowing a moment for a killed process to be reaped.
fn alive(pid: i32) -> bool {
    let probe = || {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().unwrap().success()
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while probe() {
        if Instant::now() > deadline {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn signals_reach_the_group() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let proc = spawn(&client, spec(&ws, sh("sleep 30 & echo $!; wait"))).await;
    let first = read(&client, &proc, 0, 5000).await;
    let grandchild: i32 = String::from_utf8(first.chunks[0].data.clone().into_bytes()).unwrap().trim().parse().unwrap();
    client.peer.call::<ExecSignal>(ExecSignalParams { proc: proc.clone(), signal: Signal::Terminate }).await.unwrap();
    let (_, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Signaled { signal: 15 });
    assert!(!alive(grandchild));
    // Signalling an exited process is a no-op.
    client.peer.call::<ExecSignal>(ExecSignalParams { proc: proc.clone(), signal: Signal::Kill }).await.unwrap();

    let proc = spawn(&client, spec(&ws, sh("sleep 30"))).await;
    client.peer.call::<ExecSignal>(ExecSignalParams { proc: proc.clone(), signal: Signal::Interrupt }).await.unwrap();
    let (_, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Signaled { signal: 2 });
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_pipes() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let mut params = spec(&ws, sh("cat; echo done"));
    params.stdin = true;
    let proc = spawn(&client, params).await;
    let write = |data: &str, eof: bool| {
        let params = ExecWriteStdinParams { proc: proc.clone(), data: text(data), eof, idempotency_key: key() };
        let peer = client.peer.clone();
        async move { peer.call::<ExecWriteStdin>(params).await }
    };
    write("abc\n", false).await.unwrap();
    write("def\n", true).await.unwrap();
    let (chunks, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Exited { code: 0 });
    assert_eq!(joined(&chunks, OutputStream::Stdout), "abc\ndef\ndone\n");
    assert_eq!(write("more", false).await.unwrap_err().code, ErrorCode::Conflict);

    // Without `stdin`, input is closed from the start.
    let proc = spawn(&client, spec(&ws, sh("cat"))).await;
    let (_, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Exited { code: 0 });
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_processes() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let mut params = spec(&ws, sh("test -t 0 && test -t 1 && echo is-a-tty; stty size; read line; echo got:$line"));
    params.pty = Some(PtySize { rows: 24, cols: 100 });
    let proc = spawn(&client, params).await;
    let first = read(&client, &proc, 0, 5000).await;
    assert!(first.chunks.iter().all(|c| c.stream == OutputStream::Pty));
    client
        .peer
        .call::<ExecWriteStdin>(ExecWriteStdinParams { proc: proc.clone(), data: text("hello\n"), eof: false, idempotency_key: key() })
        .await
        .unwrap();
    let (chunks, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Exited { code: 0 });
    let out = joined(&chunks, OutputStream::Pty);
    assert!(out.contains("is-a-tty"), "{out:?}");
    assert!(out.contains("24 100"), "{out:?}");
    assert!(out.contains("got:hello"), "{out:?}");

    let mut params = spec(&ws, sh("sleep 0.5; stty size"));
    params.pty = Some(PtySize { rows: 24, cols: 80 });
    let proc = spawn(&client, params).await;
    client.peer.call::<ExecResize>(ExecResizeParams { proc: proc.clone(), size: PtySize { rows: 30, cols: 120 } }).await.unwrap();
    let (chunks, _) = run_to_exit(&client, &proc).await;
    assert!(joined(&chunks, OutputStream::Pty).contains("30 120"));

    let mut params = spec(&ws, sh("sleep 30"));
    params.pty = Some(PtySize { rows: 24, cols: 80 });
    let proc = spawn(&client, params).await;
    client.peer.call::<ExecSignal>(ExecSignalParams { proc: proc.clone(), signal: Signal::Kill }).await.unwrap();
    let (_, exit) = run_to_exit(&client, &proc).await;
    assert_eq!(exit, ExitStatus::Signaled { signal: 9 });

    // Resizing a pipe process is a conflict.
    let proc = spawn(&client, spec(&ws, sh("true"))).await;
    let err = client.peer.call::<ExecResize>(ExecResizeParams { proc, size: PtySize { rows: 1, cols: 1 } }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_after_seq_catches_up() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let proc = spawn(&client, spec(&ws, sh("for i in 1 2 3 4 5; do echo $i; sleep 0.02; done"))).await;
    let (all, _) = run_to_exit(&client, &proc).await;
    assert_eq!(joined(&all, OutputStream::Stdout), "1\n2\n3\n4\n5\n");
    let last = all.last().unwrap().seq;
    // Re-reading from any point returns exactly the later chunks, then the exit.
    let again = read(&client, &proc, 0, 0).await;
    assert_eq!(again.chunks, all);
    assert!(again.exit.is_some());
    let tail = read(&client, &proc, 2, 0).await;
    assert_eq!(tail.chunks, all[2..].to_vec());
    let done = read(&client, &proc, last, 0).await;
    assert!(done.chunks.is_empty());
    assert_eq!(done.exit, Some(ExitStatus::Exited { code: 0 }));
    // `exit` is withheld while unread output remains.
    let partial =
        client.peer.call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq: 0, max_bytes: Some(1), wait_ms: 0 }).await.unwrap();
    assert_eq!(partial.chunks.len(), 1);
    assert_eq!(partial.exit, None);

    client.peer.call::<ExecRelease>(ExecReleaseParams { proc: proc.clone() }).await.unwrap();
    let gone = client.peer.call::<ExecRead>(ExecReadParams { proc, after_seq: 0, max_bytes: None, wait_ms: 0 }).await.unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn ring_buffer_reports_dropped_output() {
    let env = env_with(|config, _| config.output_ring_bytes = 4096).await;
    let (client, init, ws) = session(&env).await;
    assert_eq!(init.limits.output_ring_bytes, 4096);
    let proc = spawn(&client, spec(&ws, sh("i=0; while [ $i -lt 2000 ]; do echo line-$i; i=$((i+1)); done"))).await;
    // Wait for the exit without consuming anything.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline);
        if read(&client, &proc, u64::MAX - 1, 2000).await.exit.is_some() {
            break;
        }
    }
    let result = read(&client, &proc, 0, 0).await;
    let dropped = result.dropped_before.expect("a reader that fell behind is told what it missed");
    assert!(dropped > 1);
    assert_eq!(result.chunks.first().unwrap().seq, dropped);
    let retained: usize = result.chunks.iter().map(|c| c.data.len()).sum();
    assert!(retained <= 4096 + 32 * 1024, "retained {retained} bytes");
    let text = joined(&result.chunks, OutputStream::Stdout);
    assert!(text.ends_with("line-1999\n"), "the newest output is kept");
    // A reader that kept up is not told about drops.
    assert_eq!(read(&client, &proc, dropped - 1, 0).await.dropped_before, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn output_is_pushed_in_seq_order() {
    let env = env().await;
    let (mut client, _, ws) = session(&env).await;
    let proc = spawn(&client, spec(&ws, sh("for i in 1 2 3 4 5 6 7 8; do echo $i; echo e$i >&2; done"))).await;
    let mut seqs = Vec::new();
    let mut stdout = String::new();
    loop {
        let (name, params) = tokio::time::timeout(Duration::from_secs(10), client.notes.recv()).await.unwrap().unwrap();
        match name.as_str() {
            "exec.output" => {
                let note: ExecOutputParams = serde_json::from_value(params).unwrap();
                assert_eq!(note.proc, proc);
                seqs.push(note.chunk.seq);
                if note.chunk.stream == OutputStream::Stdout {
                    stdout.push_str(&String::from_utf8(note.chunk.data.into_bytes()).unwrap());
                }
            }
            "exec.exited" => {
                let note: ExecExitedParams = serde_json::from_value(params).unwrap();
                assert_eq!(note.status, ExitStatus::Exited { code: 0 });
                assert_eq!(note.last_seq, *seqs.last().unwrap());
                break;
            }
            other => panic!("unexpected notification {other}"),
        }
    }
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(stdout, "1\n2\n3\n4\n5\n6\n7\n8\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn release_kills_a_running_process() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let proc = spawn(&client, spec(&ws, sh("echo $$; sleep 30"))).await;
    let first = read(&client, &proc, 0, 5000).await;
    let pid: i32 = String::from_utf8(first.chunks[0].data.clone().into_bytes()).unwrap().trim().parse().unwrap();
    client.peer.call::<ExecRelease>(ExecReleaseParams { proc: proc.clone() }).await.unwrap();
    assert!(!alive(pid));
    let err = client.peer.call::<ExecRelease>(ExecReleaseParams { proc }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}
