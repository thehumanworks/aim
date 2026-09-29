//! Manifest, source identity, and trust contracts for aim plugins.

pub mod protocol;
mod trust;

use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use aim_proto::error::ProtoError;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use trust::TrustStore;

const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// Plugin loading or execution error.
#[derive(Debug)]
pub enum PluginError {
    /// Invalid manifest, source, or grant.
    Invalid(String),
    /// I/O failure while reading or writing trust and plugin state.
    Io(std::io::Error),
    /// Component compilation, linking, or execution failure.
    Runtime(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Runtime(message) => f.write_str(message),
            Self::Io(error) => write!(f, "plugin I/O: {error}"),
        }
    }
}

impl Error for PluginError {}

impl From<std::io::Error> for PluginError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// A manifest tool, advertised without compiling the component.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestTool {
    /// Tool name within the plugin.
    pub name: String,
    /// Model-facing description.
    pub description: String,
    /// JSON schema text for input arguments.
    pub input_schema: String,
    /// Whether the tool handles sensitive data.
    #[serde(default)]
    pub sensitive: bool,
}

/// A plugin's identity, requested capabilities, and declared tools.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PluginManifest {
    /// Stable namespace.
    pub name: String,
    /// Human-facing package version.
    pub version: String,
    /// ABI track, currently exactly `0.1`.
    pub api: String,
    /// Component path, interpreted relative to the manifest by the caller.
    pub component: String,
    /// Requested grants.
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    /// Tools advertised before compiling the guest.
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
}

impl PluginManifest {
    /// Parses and validates an adjacent `aim-plugin.toml`.
    ///
    /// # Errors
    /// Returns an error for malformed data, unsupported ABI, or invalid tool specs.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(PluginError::Invalid("manifest exceeds 64 KiB".into()));
        }
        let manifest: Self = toml::from_str(text).map_err(|e| PluginError::Invalid(e.to_string()))?;
        if manifest.api != "0.1" || !valid_name(&manifest.name) || manifest.component.is_empty() || manifest.tools.len() > 64 {
            return Err(PluginError::Invalid("invalid plugin name, API, or tool count".into()));
        }
        let mut seen = HashSet::new();
        for tool in &manifest.tools {
            if !valid_name(&tool.name) || !seen.insert(&tool.name) || tool.description.len() > 4096 {
                return Err(PluginError::Invalid("invalid or duplicate plugin tool".into()));
            }
            drop(serde_json::from_str::<Value>(&tool.input_schema).map_err(|e| PluginError::Invalid(e.to_string()))?);
        }
        Ok(manifest)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Source bytes read through the caller's authorized workspace.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct PluginSource {
    /// Contents of `aim-plugin.toml`.
    pub manifest_text: String,
    /// WebAssembly component bytes, never precompiled machine code.
    pub component: Vec<u8>,
    /// Whether source came from `.agents/plugins`.
    pub project: bool,
}

impl PluginSource {
    /// SHA-256 trust identity for the canonical manifest and component bytes. A changed tool
    /// definition cannot reuse the old component's grants; formatting alone preserves trust.
    #[must_use]
    pub fn hash(&self) -> String {
        let canonical = PluginManifest::parse(&self.manifest_text)
            .ok()
            .and_then(|mut manifest| {
                for tool in &mut manifest.tools {
                    let schema: Value = serde_json::from_str(&tool.input_schema).ok()?;
                    tool.input_schema = serde_json::to_string(&schema).ok()?;
                }
                serde_json::to_vec(&manifest).ok()
            })
            .unwrap_or_else(|| self.manifest_text.as_bytes().to_vec());
        let mut digest = Sha256::new();
        digest.update(b"aim-plugin-trust-v1\0");
        digest.update(u64::try_from(canonical.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(&canonical);
        digest.update(&self.component);
        format!("{:x}", digest.finalize())
    }

    /// SHA-256 identity for compiled component caching.
    #[must_use]
    pub fn component_hash(&self) -> String {
        format!("{:x}", Sha256::digest(&self.component))
    }
}

/// A sendable future returned by the delegated tool dispatcher.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Policy-aware dispatcher for tools called by a plugin.
pub trait PluginDelegate: Send + Sync {
    /// Calls a tool after this host has checked both grant scope and session allowlist.
    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>>;
}

/// Bounded, read-only session facts visible to plugins with `session.read`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct SessionMetadata {
    /// Session ID, when available.
    pub session_id: Option<String>,
    /// Provider profile name.
    pub provider: Option<String>,
    /// Selected model ID.
    pub model: Option<String>,
    /// Workspace display path or location.
    pub workspace: Option<String>,
    /// Persistence mode name.
    pub persistence: Option<String>,
}
