//! The code-mode relay (ADR 0076): a workspace's tools in code mode, served over MCP stdio.
//!
//! A strict `acp:claude` session reaches its workspace through one MCP server named `aim`. When
//! code mode is `on` or `only`, that server is `aim code-mcp` instead of `aimx mcp`: it connects
//! the workspace through aimx exactly as a native session does (locally, or over SSH through
//! `aimx serve --ssh`), composes the code tool, the saved-program tools and the mode's direct tools
//! over it ([`crate::host::with_code_mode`]), and serves them. Every call, nested or direct,
//! still crosses aimx's authorization; aimx itself spawns nothing new.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aim_proto::daemon::{Location, Persistence, SessionSpec};
use aim_proto::error::ProtoError;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::agent::ToolHost;
use crate::coderun::mode::{self, Mode};
use crate::host::{BoxFuture, CodeConfig};

/// The `aim` subcommand that serves the relay.
pub const SUBCOMMAND: &str = "code-mcp";

/// What the relay serves: a workspace, the aimx that connects it, and code mode's configuration.
#[derive(Clone, Debug)]
pub struct CodeRelay {
    /// Workspace root on its host.
    pub root: String,
    /// Where the workspace is: local or over SSH.
    pub location: Location,
    /// The local aimx binary.
    pub aimx: PathBuf,
    /// The worker, the user's programs and the mode (`on` or `only`).
    pub code: CodeConfig,
}

impl CodeRelay {
    /// The relay's command-line arguments after the `aim` executable. Everything it needs is
    /// explicit: the adapter that spawns it may not pass aim's environment on.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            SUBCOMMAND.to_owned(),
            "--root".to_owned(),
            self.root.clone(),
            "--aimx".to_owned(),
            self.aimx.to_string_lossy().into_owned(),
            "--coderun".to_owned(),
            self.code.worker.to_string_lossy().into_owned(),
            "--programs".to_owned(),
            self.code.user_programs.to_string_lossy().into_owned(),
            "--code-mode".to_owned(),
            mode::label(self.code.mode).to_owned(),
        ];
        if let Location::Ssh { destination } = &self.location {
            args.extend(["--ssh".to_owned(), destination.clone()]);
        }
        args
    }

    /// The environment the relay needs besides its arguments: the SSH config aimx reads.
    #[must_use]
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if matches!(self.location, Location::Ssh { .. })
            && let Some(config) = std::env::var_os("AIM_SSH_CONFIG")
        {
            env.insert("AIM_SSH_CONFIG".to_owned(), config.to_string_lossy().into_owned());
        }
        env
    }

    /// Connects the workspace and composes code mode over its tools. Returns the tools to serve
    /// and what ends them: the session's cells first, then the workspace (ADR 0066).
    ///
    /// # Errors
    /// aimx did not start, or the workspace did not open.
    pub async fn connect(&self) -> Result<(Arc<dyn ToolHost>, Box<dyn FnOnce() -> BoxFuture<()> + Send>), ProtoError> {
        let spec = SessionSpec {
            workspace: self.root.clone(),
            location: self.location.clone(),
            provider: "acp:claude".to_owned(),
            model: None,
            effort: None,
            agent: None,
            persistence: Persistence::Persistent,
        };
        let connected = crate::host::aimx_workspaces(self.aimx.clone())(&spec).await?;
        // A `CodeRelay` is built from a found worker on a sandboxing platform; an ACP session has
        // no named agent, so its ceiling permits `run_code`.
        let exposure = mode::decide(Some(self.code.mode), self.code.worker.exists(), cfg!(target_os = "macos"), true);
        if !exposure.code {
            return Ok((connected.tools, connected.shutdown));
        }
        let session = format!("acp-code-{}", uuid::Uuid::now_v7());
        let (tools, cells) =
            crate::host::with_code_mode(connected.tools, &self.code, exposure.direct, None, None, &session, connected.project);
        let workspace = connected.shutdown;
        let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(move || {
            Box::pin(async move {
                cells.close(Duration::from_secs(2)).await;
                workspace().await;
            })
        });
        Ok((tools, shutdown))
    }
}

/// Serves the relay on `reader`/`writer` until the client closes it, then ends its cells and its
/// workspace connection.
///
/// # Errors
/// The workspace did not connect, or the MCP transport failed.
pub async fn serve<R, W>(relay: &CodeRelay, reader: R, writer: W) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (tools, shutdown) = relay.connect().await.map_err(|err| format!("code-mode relay: {}", err.message))?;
    let served = super::server::AimMcpServer::new(Arc::clone(&tools)).serve(reader, writer).await;
    drop(tools);
    shutdown().await;
    served.map_err(|_| "code-mode relay: MCP stdio transport failed".to_owned())
}

/// The relay's mode from its `--code-mode` argument.
///
/// # Errors
/// The value is not `on` or `only`.
pub fn relay_mode(value: &str) -> Result<Mode, String> {
    match mode::parse(value) {
        Some(mode @ (Mode::On | Mode::Only)) => Ok(mode),
        _ => Err(format!("--code-mode must be on or only, not {value:?}")),
    }
}
