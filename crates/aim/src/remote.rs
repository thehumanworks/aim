//! A remote workspace reached through the local aimx SSH backend.

use std::path::Path;
use std::process::Stdio;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use serde_json::Value;
use tokio::process::{Child, Command};

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;
use crate::harness::HarnessClient;

/// A harness connection and its owned SSH transport process.
pub struct RemoteHarness {
    /// The connected harness; its workspace root comes from the remote handshake.
    pub client: HarnessClient,
    child: Child,
}

impl RemoteHarness {
    /// Starts `aimx serve --stdio --ssh <destination> --root <remote root>`.
    ///
    /// # Errors
    /// Returns a protocol error if the process or harness handshake fails.
    pub async fn connect(aimx: &Path, destination: &str, root: &str) -> Result<Self, ProtoError> {
        if destination.is_empty() || destination.starts_with('-') {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "invalid SSH destination"));
        }
        let mut command = transport_command(aimx, destination, root);
        let mut child = command.spawn().map_err(|err| ProtoError::new(ErrorCode::Unavailable, format!("starting aimx: {err}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ProtoError::new(ErrorCode::Internal, "aimx has no stdio pipes"));
        };
        let client = HarnessClient::connect(stdout, stdin, root).await?;
        Ok(Self { client, child })
    }

    /// Closes the protocol and stops the SSH transport.
    pub async fn shutdown(mut self) {
        self.client.shutdown().await;
        if let Err(err) = self.child.kill().await {
            tracing::debug!(%err, "remote harness process already exited");
        }
    }
}

fn transport_command(aimx: &Path, destination: &str, root: &str) -> Command {
    let mut command = Command::new(aimx);
    command
        .args(["serve", "--stdio", "--ssh", destination, "--root", root])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(config) = std::env::var_os("AIM_SSH_CONFIG") {
        command.arg("--ssh-config").arg(config);
    }
    // The local aimx needs OpenSSH's config, agent and prompt environment, but no provider
    // credentials or other ambient variables should travel into its SSH child processes.
    for key in
        ["PATH", "HOME", "USER", "LOGNAME", "SSH_AUTH_SOCK", "SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "DISPLAY", "TERM", "XDG_RUNTIME_DIR"]
    {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

impl ToolHost for RemoteHarness {
    fn specs(&self) -> Vec<ToolSpec> {
        self.client.specs()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.client.call(name, arguments, key)
    }

    fn reserve_blob(&self, path: String, key: IdempotencyKey) -> BoxFuture<Result<String, ProtoError>> {
        self.client.reserve_blob(path, key)
    }

    fn finalize_blob(&self, reservation: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.client.finalize_blob(reservation, bytes, key)
    }

    fn cancel_blob(&self, reservation: String, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.client.cancel_blob(reservation, key)
    }

    fn write_blob(&self, path: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.client.write_blob(path, bytes, key)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::transport_command;

    #[test]
    fn transport_environment_has_only_ssh_inputs() {
        let mut command = transport_command(Path::new("aimx"), "example", ".");
        let keys: Vec<_> = command.as_std_mut().get_envs().map(|(key, _)| key.to_string_lossy().into_owned()).collect();
        assert!(keys.iter().all(|key| matches!(
            key.as_str(),
            "PATH"
                | "HOME"
                | "USER"
                | "LOGNAME"
                | "SSH_AUTH_SOCK"
                | "SSH_ASKPASS"
                | "SSH_ASKPASS_REQUIRE"
                | "DISPLAY"
                | "TERM"
                | "XDG_RUNTIME_DIR"
        )));
    }
}
