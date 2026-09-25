//! The agent layer's side of `aim-harness/1` (docs/architecture.md §4.1).
//!
//! [`HarnessClient`] connects to an aimx over any byte stream. The common case spawns
//! `aimx serve --stdio --root <dir>` as a child — the same command SSH runs on a remote host — and
//! opens the workspace. It is a [`ToolHost`]: the native loop's harness tool calls go through it.

use std::process::Stdio;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    BackendSpec, GenerationRange, Initialize, InitializeParams, InitializeResult, PeerInfo, ToolsCall, ToolsCallParams, ToolsList,
    ToolsListParams, WorkspaceInfo, WorkspaceOpen, WorkspaceOpenParams,
};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::{NoHandler, Peer, PeerConfig};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};

use crate::agent::tools::{BoxFuture, ToolHost};

/// A connected harness with one open workspace.
pub struct HarnessClient {
    peer: Peer,
    init: InitializeResult,
    workspace: WorkspaceInfo,
    tools: Vec<ToolSpec>,
    child: Option<Child>,
}

impl core::fmt::Debug for HarnessClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HarnessClient").field("workspace", &self.workspace.root).field("tools", &self.tools.len()).finish_non_exhaustive()
    }
}

impl HarnessClient {
    /// Spawns `program serve --stdio --root <root>` and connects to it.
    ///
    /// # Errors
    /// `unavailable` when the program cannot start; any error from the handshake.
    pub async fn spawn_stdio(program: &str, root: &str) -> Result<Self, ProtoError> {
        let mut command = Command::new(program);
        command
            .args(["serve", "--stdio", "--root", root])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| ProtoError::new(ErrorCode::Unavailable, format!("starting {program}: {err}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ProtoError::new(ErrorCode::Internal, "child process has no stdio pipes"));
        };
        let mut client = Self::connect(stdout, stdin, root).await?;
        client.child = Some(child);
        Ok(client)
    }

    /// Connects over an existing byte stream (unix socket, SSH channel, in-process duplex).
    ///
    /// # Errors
    /// Any error from `initialize`, `workspace.open` or `tools.list`.
    pub async fn connect<R, W>(reader: R, writer: W, root: &str) -> Result<Self, ProtoError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let peer = Peer::spawn(reader, writer, NoHandler, PeerConfig::default());
        let (min, max) = aim_proto::HARNESS_GENERATIONS;
        let init = peer
            .call::<Initialize>(InitializeParams {
                generations: GenerationRange { min, max },
                client: PeerInfo { name: "aim".to_owned(), version: env!("CARGO_PKG_VERSION").to_owned() },
                auth: None,
                resume: None,
            })
            .await?;
        let workspace = peer.call::<WorkspaceOpen>(WorkspaceOpenParams { root: root.to_owned(), backend: BackendSpec::Local }).await?;
        let tools = peer.call::<ToolsList>(ToolsListParams::default()).await?.tools;
        Ok(Self { peer, init, workspace, tools, child: None })
    }

    /// The handshake result (negotiated generation, principal, limits).
    #[must_use]
    pub const fn init(&self) -> &InitializeResult {
        &self.init
    }

    /// The open workspace.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceInfo {
        &self.workspace
    }

    /// The raw connection, for primitives beyond tools (`fs.read` of project instructions, …).
    #[must_use]
    pub const fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Ends the connection and stops a spawned harness.
    pub async fn shutdown(mut self) {
        self.peer.close();
        if let Some(mut child) = self.child.take()
            && let Err(err) = child.kill().await
        {
            tracing::debug!(%err, "harness child already exited");
        }
    }
}

impl ToolHost for HarnessClient {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let peer = self.peer.clone();
        let workspace = self.workspace.id.clone();
        Box::pin(async move { peer.call::<ToolsCall>(ToolsCallParams { workspace, name, arguments, idempotency_key: Some(key) }).await })
    }
}
