//! Length-capped byte-stream bridges for local and harness-hosted stdio MCP servers.

use std::collections::BTreeMap;
use std::io;
use std::process::Stdio;

use aim_proto::content::Content;
use aim_proto::harness::{
    Command, ExecRead, ExecReadParams, ExecRelease, ExecReleaseParams, ExecSpawn, ExecSpawnParams, ExecWriteStdin, ExecWriteStdinParams,
    OutputStream,
};
use aim_proto::ids::{IdempotencyKey, ProcId, WorkspaceId};
use aim_rpc::Peer;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio::process::{Child, Command as ProcessCommand};
use tokio::task::JoinHandle;

const MAX_FRAME: usize = 1024 * 1024;
const PIPE_BYTES: usize = 128 * 1024;

/// Connection details for an MCP process that must run through the selected aimx workspace.
#[derive(Clone)]
pub struct WorkspacePipe {
    /// The already authenticated harness connection.
    pub peer: Peer,
    /// The opened workspace on that connection.
    pub workspace: WorkspaceId,
}

enum Process {
    Local(Child),
    Workspace { peer: Peer, proc: ProcId },
}

/// One bounded MCP byte stream and its owned process.
pub(crate) struct Bridge {
    pub(crate) stream: Option<DuplexStream>,
    process: Process,
    pumps: Vec<JoinHandle<()>>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for pump in &self.pumps {
            pump.abort();
        }
        match &mut self.process {
            Process::Local(child) => {
                let _killed = child.start_kill();
            }
            Process::Workspace { peer, proc } => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let peer = peer.clone();
                    let proc = proc.clone();
                    runtime.spawn(async move {
                        let _released = peer.call::<ExecRelease>(ExecReleaseParams { proc, scope: None }).await;
                    });
                }
            }
        }
        // `kill_on_drop(true)` terminates a local child without a blocking destructor.
    }
}

fn key() -> IdempotencyKey {
    IdempotencyKey::new(format!("mcp-{}", uuid::Uuid::new_v4()))
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() { Ok(None) } else { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete MCP frame")) };
        }
        let end = available.iter().position(|byte| *byte == b'\n').map_or(available.len(), |position| position + 1);
        let finished = available.get(end.saturating_sub(1)) == Some(&b'\n');
        if line.len().saturating_add(end) > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "MCP frame exceeds 1 MiB"));
        }
        line.extend_from_slice(available.get(..end).unwrap_or_default());
        reader.consume(end);
        if finished {
            return Ok(Some(line));
        }
    }
}

async fn forward_lines<R, W>(source: R, mut destination: W) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(source);
    while let Some(line) = read_line(&mut reader).await? {
        destination.write_all(&line).await?;
    }
    destination.shutdown().await
}

fn checked_bytes(bytes: &[u8], line_len: &mut usize) -> io::Result<()> {
    for byte in bytes {
        if *byte == b'\n' {
            *line_len = 0;
        } else {
            *line_len = line_len.saturating_add(1);
            if *line_len >= MAX_FRAME {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "MCP frame exceeds 1 MiB"));
            }
        }
    }
    Ok(())
}

/// Spawn a trusted server on the aim process's machine.
pub(crate) fn local(command: &str, args: &[String], env: &BTreeMap<String, String>) -> Result<Bridge, String> {
    let mut process = ProcessCommand::new(command);
    process.args(args).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    for name in ["PATH", "HOME", "USER", "LOGNAME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    for (name, value) in env {
        process.env(name, value);
    }
    let mut child = process.spawn().map_err(|_| "cannot start trusted MCP server".to_owned())?;
    let stdin = child.stdin.take().ok_or("MCP server has no stdin")?;
    let stdout = child.stdout.take().ok_or("MCP server has no stdout")?;
    let (stream, other) = tokio::io::duplex(PIPE_BYTES);
    let (read, write) = tokio::io::split(other);
    let to_child = tokio::spawn(async move {
        let _result = forward_lines(read, stdin).await;
    });
    let from_child = tokio::spawn(async move {
        let _result = forward_lines(stdout, write).await;
    });
    Ok(Bridge { stream: Some(stream), process: Process::Local(child), pumps: vec![to_child, from_child] })
}

/// Spawn a trusted server on the selected aimx workspace, including an SSH workspace.
pub(crate) async fn workspace(
    binding: &WorkspacePipe,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<Bridge, String> {
    let command_line = std::iter::once(command.to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>();
    let spawned = binding
        .peer
        .call::<ExecSpawn>(ExecSpawnParams {
            workspace: binding.workspace.clone(),
            command: Command::Argv { argv: command_line },
            cwd: None,
            env: env.clone(),
            pty: None,
            stdin: true,
            timeout_ms: None,
            idempotency_key: key(),
            scope: None,
        })
        .await
        .map_err(|_| "cannot start trusted MCP server on workspace".to_owned())?;
    let proc = spawned.proc;
    let (stream, other) = tokio::io::duplex(PIPE_BYTES);
    let (read, mut write) = tokio::io::split(other);
    let sender_peer = binding.peer.clone();
    let sender_proc = proc.clone();
    let to_child = tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        while let Ok(Some(line)) = read_line(&mut reader).await {
            if sender_peer
                .call::<ExecWriteStdin>(ExecWriteStdinParams {
                    proc: sender_proc.clone(),
                    data: Content::from_bytes(line),
                    eof: false,
                    idempotency_key: key(),
                    scope: None,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let receiver_peer = binding.peer.clone();
    let receiver_proc = proc.clone();
    let from_child = tokio::spawn(async move {
        let mut after_seq = 0;
        let mut line_len = 0;
        loop {
            let Ok(result) = receiver_peer
                .call::<ExecRead>(ExecReadParams {
                    proc: receiver_proc.clone(),
                    after_seq,
                    max_bytes: Some(64 * 1024),
                    wait_ms: 500,
                    scope: None,
                })
                .await
            else {
                break;
            };
            if result.dropped_before.is_some_and(|dropped| dropped > after_seq.saturating_add(1)) {
                break;
            }
            for chunk in result.chunks {
                after_seq = after_seq.max(chunk.seq);
                if chunk.stream == OutputStream::Stdout {
                    let bytes = chunk.data.into_bytes();
                    if checked_bytes(&bytes, &mut line_len).is_err() || write.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
            }
            if result.exit.is_some() {
                break;
            }
        }
        let _closed = write.shutdown().await;
    });
    Ok(Bridge { stream: Some(stream), process: Process::Workspace { peer: binding.peer.clone(), proc }, pumps: vec![to_child, from_child] })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use aim_proto::harness::{Command, ExecRead, ExecReadResult, ExecRelease, ExecSpawn, ExecSpawnResult, ExecWriteStdin};
    use aim_proto::ids::{ProcId, WorkspaceId};
    use aim_rpc::{NoHandler, Peer, PeerConfig, Router};

    use super::{WorkspacePipe, checked_bytes, workspace};

    #[test]
    fn frame_bound_survives_chunks_and_resets_at_newline() {
        let mut length = 0;
        assert!(checked_bytes(&vec![b'a'; 700_000], &mut length).is_ok());
        assert!(checked_bytes(&vec![b'b'; 400_000], &mut length).is_err());
        assert!(checked_bytes(b"\nok\n", &mut length).is_ok());
        assert_eq!(length, 0);
    }

    #[tokio::test]
    async fn workspace_process_is_spawned_and_released_through_harness() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let router = Router::new(Arc::clone(&calls))
            .method::<ExecSpawn, _, _>(|calls, _, params| async move {
                assert_eq!(params.workspace, WorkspaceId::new("remote"));
                assert_eq!(params.command, Command::Argv { argv: vec!["remote-mcp".into(), "--serve".into()] });
                assert!(params.stdin);
                calls.lock().expect("call log").push("spawn".into());
                Ok(ExecSpawnResult { proc: ProcId::new("fake-process") })
            })
            .method::<ExecRead, _, _>(|_, _, _| async move {
                Ok(ExecReadResult {
                    chunks: Vec::new(),
                    dropped_before: None,
                    exit: Some(aim_proto::harness::ExitStatus::Exited { code: 0 }),
                })
            })
            .method::<ExecWriteStdin, _, _>(|_, _, _| async move { Ok(()) })
            .method::<ExecRelease, _, _>(|calls, _, params| async move {
                assert_eq!(params.proc, ProcId::new("fake-process"));
                calls.lock().expect("call log").push("release".into());
                Ok(())
            });
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let _server = Peer::spawn(br, bw, router, PeerConfig::default());
        let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
        let binding = WorkspacePipe { peer: client, workspace: WorkspaceId::new("remote") };
        let bridge =
            workspace(&binding, "remote-mcp", &["--serve".into()], &std::collections::BTreeMap::default()).await.expect("workspace bridge");
        drop(bridge);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if calls.lock().expect("call log").as_slice() == ["spawn", "release"] {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("remote process release");
    }
}
