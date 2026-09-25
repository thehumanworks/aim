//! Trusted local artifact selection and remote installation.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::conn::Connection;
use super::quote;

/// A caller-verified artifact for a specific target.
#[derive(Clone, Debug)]
pub struct Artifact {
    /// Local executable path.
    pub path: PathBuf,
    /// Expected SHA-256 digest, as 64 lowercase hexadecimal characters.
    pub sha256: String,
    /// Target triple selected by the caller.
    pub target: String,
    /// Protocol generation.
    pub generation: u32,
    /// Version component of the remote filename.
    pub version: String,
}

/// Why remote installation cannot be trusted or performed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapError {
    /// The remote OS or CPU is unsupported.
    UnsupportedTarget,
    /// The caller's artifact does not match the remote target.
    WrongTarget,
    /// The local artifact does not match its caller-provided trust root.
    LocalHashMismatch,
    /// No independent remote hash utility is available.
    UnverifiedRemoteHash,
    /// The remote hash differs from the verified local digest.
    RemoteHashMismatch,
    /// Local or SSH I/O failed.
    Unavailable,
    /// A filename component would be unsafe in a remote shell path.
    InvalidArtifact,
}

/// One round-trip observation of the remote platform and installed binaries.
#[derive(Clone, Debug)]
pub struct Probe {
    /// Supported target triple, if known.
    pub target: Option<&'static str>,
    /// Remote home directory.
    pub home: String,
    /// Names already present in `~/.aim/bin`.
    pub binaries: Vec<String>,
}

/// Map the POSIX `uname -sm` result to one distribution target.
#[must_use]
pub fn target_for_uname(uname: &str) -> Option<&'static str> {
    match uname.trim() {
        "Linux x86_64" => Some("x86_64-unknown-linux-musl"),
        "Linux aarch64" | "Linux arm64" => Some("aarch64-unknown-linux-musl"),
        "Darwin x86_64" => Some("x86_64-apple-darwin"),
        "Darwin arm64" | "Darwin aarch64" => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// Probe platform, home and installed binaries using a single SSH channel.
///
/// # Errors
/// Returns an error if the probe channel or platform response fails.
pub async fn probe(connection: &Connection) -> Result<Probe, BootstrapError> {
    let output = connection
        .run("uname -sm; printf '%s\\n' \"$HOME\"; ls -1 \"$HOME/.aim/bin\" 2>/dev/null || :", &[])
        .await
        .map_err(|_| BootstrapError::Unavailable)?;
    let text = String::from_utf8(output).map_err(|_| BootstrapError::Unavailable)?;
    let mut lines = text.lines();
    let target = lines.next().and_then(target_for_uname);
    let home = lines.next().ok_or(BootstrapError::Unavailable)?.to_owned();
    if !home.starts_with('/') {
        return Err(BootstrapError::Unavailable);
    }
    Ok(Probe { target, home, binaries: lines.map(str::to_owned).collect() })
}

/// Verify locally, skip a matching installation, or upload and independently hash it.
/// A caller may explicitly accept the absence of a remote hash utility.
///
/// # Errors
/// Returns an error for unsupported targets, digest mismatches, or failed I/O.
pub async fn install(connection: &Connection, artifact: &Artifact, allow_unverified_remote_hash: bool) -> Result<String, BootstrapError> {
    let platform = probe(connection).await?;
    let target = platform.target.ok_or(BootstrapError::UnsupportedTarget)?;
    if target != artifact.target {
        return Err(BootstrapError::WrongTarget);
    }
    if !safe_component(&artifact.version) || !safe_component(&artifact.target) || !valid_digest(&artifact.sha256) {
        return Err(BootstrapError::InvalidArtifact);
    }
    let bytes = std::fs::read(&artifact.path).map_err(|_| BootstrapError::Unavailable)?;
    if digest(&bytes) != artifact.sha256 {
        return Err(BootstrapError::LocalHashMismatch);
    }
    let name = format!("aimx-g{}-{}-{}", artifact.generation, artifact.version, artifact.target);
    let remote = format!("{}/.aim/bin/{name}", platform.home);
    let utility = remote_hash_utility(connection).await?;
    if utility.is_none() && !allow_unverified_remote_hash {
        return Err(BootstrapError::UnverifiedRemoteHash);
    }
    if platform.binaries.iter().any(|existing| existing == &name)
        && let Some(command) = utility
        && remote_digest(connection, command, &remote).await? == artifact.sha256
    {
        return Ok(remote);
    }
    let quoted = quote(&remote);
    let script = format!(
        "umask 077; mkdir -p \"$HOME/.aim/bin\" || exit; t=$(mktemp \"$HOME/.aim/bin/.aimx.XXXXXXXX\") || exit; trap 'rm -f \"$t\"' EXIT HUP INT TERM; cat > \"$t\" || exit; chmod 700 \"$t\" || exit; mv -f \"$t\" {quoted} || exit; trap - EXIT HUP INT TERM"
    );
    connection.run(&script, &bytes).await.map_err(|_| BootstrapError::Unavailable)?;
    if let Some(command) = utility
        && remote_digest(connection, command, &remote).await? != artifact.sha256
    {
        drop(connection.run(&format!("rm -f -- {}", quote(&remote)), &[]).await);
        return Err(BootstrapError::RemoteHashMismatch);
    }
    Ok(remote)
}

/// Compute the digest that callers should place in a development [`Artifact`].
///
/// # Errors
/// Returns an error if the local file cannot be read.
pub fn local_sha256(path: &Path) -> Result<String, BootstrapError> {
    std::fs::read(path).map(|bytes| digest(&bytes)).map_err(|_| BootstrapError::Unavailable)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn safe_component(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn remote_hash_utility(connection: &Connection) -> Result<Option<&'static str>, BootstrapError> {
    let output = connection.run("if command -v sha256sum >/dev/null 2>&1; then printf 'sha256sum'; elif command -v shasum >/dev/null 2>&1; then printf 'shasum'; fi", &[])
        .await.map_err(|_| BootstrapError::Unavailable)?;
    match output.as_slice() {
        b"sha256sum" => Ok(Some("sha256sum")),
        b"shasum" => Ok(Some("shasum -a 256")),
        _ => Ok(None),
    }
}

async fn remote_digest(connection: &Connection, utility: &str, path: &str) -> Result<String, BootstrapError> {
    let output = connection.run(&format!("{utility} -- {}", quote(path)), &[]).await.map_err(|_| BootstrapError::Unavailable)?;
    let text = String::from_utf8(output).map_err(|_| BootstrapError::Unavailable)?;
    text.split_whitespace().next().filter(|hash| valid_digest(hash)).map(str::to_owned).ok_or(BootstrapError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::target_for_uname;

    #[test]
    fn maps_supported_targets() {
        assert_eq!(target_for_uname("Darwin arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for_uname("Linux x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target_for_uname("FreeBSD x86_64"), None);
    }
}
