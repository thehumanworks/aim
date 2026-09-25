//! Remote workspaces reached through SSH or an authenticated network aimx.

use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
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

/// Connects to a configured network harness with a bearer from the local environment or private
/// token file. Credentials are never passed through a session spec, URL, argument, or error.
///
/// # Errors
/// A missing or unsafe credential source, invalid URL, or failed harness handshake.
pub async fn connect_network(url: &str, root: &str) -> Result<HarnessClient, ProtoError> {
    let token = network_token()?;
    let parsed = reqwest::Url::parse(url).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid remote harness URL"))?;
    match parsed.scheme() {
        "ws" | "wss" => HarnessClient::connect_ws(url, &token, root).await,
        "http" | "https" => HarnessClient::connect_http(url, &token, root).await,
        _ => Err(ProtoError::new(ErrorCode::InvalidParams, "unsupported remote harness URL scheme")),
    }
}

fn valid_token(token: &str) -> Result<&str, ProtoError> {
    if token.is_empty() || token.len() > 256 || token.bytes().any(|byte| byte.is_ascii_whitespace() || !byte.is_ascii_graphic()) {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "invalid remote bearer source"));
    }
    Ok(token)
}

fn network_token() -> Result<String, ProtoError> {
    if let Some(value) = std::env::var_os("AIM_REMOTE_TOKEN") {
        let token = value.to_str().ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "invalid remote bearer source"))?;
        return valid_token(token).map(ToOwned::to_owned);
    }
    let file = std::env::var_os("AIM_REMOTE_TOKEN_FILE")
        .ok_or_else(|| ProtoError::new(ErrorCode::Unauthenticated, "AIM_REMOTE_TOKEN or AIM_REMOTE_TOKEN_FILE required"))?;
    read_token_file(Path::new(&file))
}

fn read_token_file(path: &Path) -> Result<String, ProtoError> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ProtoError::new(ErrorCode::Denied, "remote bearer file unavailable"))?;
    let metadata = opened.metadata().map_err(|_| ProtoError::new(ErrorCode::Denied, "remote bearer file unavailable"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o600 || metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(ProtoError::new(ErrorCode::Denied, "remote bearer file must be owner-owned mode 0600"));
    }
    let mut token = String::new();
    opened.take(258).read_to_string(&mut token).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid remote bearer file"))?;
    valid_token(token.trim_end_matches('\n')).map(ToOwned::to_owned)
}

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
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::{read_token_file, transport_command};

    #[test]
    fn private_remote_token_file_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bearer");
        std::fs::write(&file, "aimx_test\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token_file(&file).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(&file).unwrap(), "aimx_test");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_token_file(&link).is_err());
    }

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
