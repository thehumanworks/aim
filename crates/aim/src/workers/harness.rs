//! Git and check commands run through aimx, including for SSH workspaces.

use std::collections::BTreeMap;
use std::path::Path;

use aim_proto::daemon::Location;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{Command, ExecRead, ExecReadParams, ExecRelease, ExecReleaseParams, ExecSpawn, ExecSpawnParams, ExitStatus};
use aim_proto::ids::IdempotencyKey;
use uuid::Uuid;

use crate::harness::HarnessClient;
use crate::remote::RemoteHarness;

const OUTPUT_LIMIT: usize = 512 * 1024;

enum Connection {
    Local(HarnessClient),
    Ssh(RemoteHarness),
}

/// One command's bounded output and exit state.
pub struct CommandOutput {
    /// Combined stdout and stderr; ordering follows the harness sequence.
    pub text: String,
    /// Whether output was truncated locally or by the harness ring.
    pub truncated: bool,
    /// Process exit state.
    pub exit: ExitStatus,
}

impl CommandOutput {
    /// Whether the process exited with code zero.
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.exit, ExitStatus::Exited { code: 0 })
    }
}

/// A harness bound to the Git repository or one of its isolated worktrees.
pub struct GitHarness {
    connection: Connection,
}

impl GitHarness {
    /// Connects aimx to the workspace host selected by the posted job.
    ///
    /// # Errors
    /// Returns a harness transport or workspace-open error.
    pub async fn connect(aimx: &Path, root: &str, location: &Location) -> Result<Self, ProtoError> {
        let connection = match location {
            Location::Local => Connection::Local(HarnessClient::spawn_stdio(&aimx.to_string_lossy(), root).await?),
            Location::Ssh { destination } => Connection::Ssh(RemoteHarness::connect(aimx, destination, root).await?),
        };
        Ok(Self { connection })
    }

    fn client(&self) -> &HarnessClient {
        match &self.connection {
            Connection::Local(client) => client,
            Connection::Ssh(remote) => &remote.client,
        }
    }

    /// Runs a typed argv command with bounded output; the harness owns the child process.
    ///
    /// # Errors
    /// Returns a transport error or a loss of retained process output.
    pub async fn argv(&self, argv: Vec<String>, timeout_ms: u64) -> Result<CommandOutput, ProtoError> {
        self.run(Command::Argv { argv }, timeout_ms).await
    }

    /// Runs a job-authored check command under the workspace's shell.
    ///
    /// # Errors
    /// Returns a transport error or a loss of retained process output.
    pub async fn shell(&self, script: String, timeout_ms: u64) -> Result<CommandOutput, ProtoError> {
        self.run(Command::Shell { script }, timeout_ms).await
    }

    async fn run(&self, command: Command, timeout_ms: u64) -> Result<CommandOutput, ProtoError> {
        let client = self.client();
        let peer = client.peer();
        let spawned = peer
            .call::<ExecSpawn>(ExecSpawnParams {
                scope: None,
                workspace: client.workspace().id.clone(),
                command,
                cwd: None,
                env: BTreeMap::new(),
                pty: None,
                stdin: false,
                timeout_ms: Some(timeout_ms),
                idempotency_key: IdempotencyKey::new(Uuid::new_v4().to_string()),
            })
            .await?;
        let mut after_seq = 0;
        let mut output = Vec::new();
        let mut truncated = false;
        let exit = loop {
            let read = peer
                .call::<ExecRead>(ExecReadParams {
                    scope: None,
                    proc: spawned.proc.clone(),
                    after_seq,
                    max_bytes: Some(64 * 1024),
                    wait_ms: 1_000,
                })
                .await?;
            truncated |= read.dropped_before.is_some();
            for chunk in read.chunks {
                after_seq = chunk.seq;
                let bytes = chunk.data.into_bytes();
                let remaining = OUTPUT_LIMIT.saturating_sub(output.len());
                output.extend_from_slice(bytes.get(..remaining.min(bytes.len())).unwrap_or_default());
                truncated |= bytes.len() > remaining;
            }
            if let Some(exit) = read.exit {
                break exit;
            }
        };
        peer.call::<ExecRelease>(ExecReleaseParams { scope: None, proc: spawned.proc }).await?;
        let text = String::from_utf8(output).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "command output is not UTF-8"))?;
        Ok(CommandOutput { text, truncated, exit })
    }

    /// Stops the harness transport after all owned commands and sessions have ended.
    pub async fn shutdown(self) {
        match self.connection {
            Connection::Local(client) => client.shutdown().await,
            Connection::Ssh(remote) => remote.shutdown().await,
        }
    }
}
