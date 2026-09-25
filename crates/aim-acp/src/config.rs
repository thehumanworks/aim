//! How to launch an ACP agent: `acp:NAME` profiles (tny ADR 0029) as data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AcpError;

/// The command of the Claude Code ACP adapter as installed by mise
/// (`npm:@agentclientprotocol/claude-agent-acp`).
pub const CLAUDE_AGENT_ACP: &str = "claude-agent-acp";

/// How an agent spells a variant of a model value, such as a context size (docs/adr/0075). A
/// profile declares the spellings its agent accepts; aim folds only those, so another ACP agent
/// never has `-1m` read as a variant it does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "syntax", rename_all = "snake_case")]
pub enum VariantSyntax {
    /// A trailing `[<digits><unit>]`: `opus[1m]` is `opus` with variant `1m`.
    Bracketed {
        /// The unit letter after the digits (`m`: millions of tokens of context).
        unit: char,
    },
    /// A trailing `-<digits><unit>`: `opus-1m` is `opus` with variant `1m`.
    Dashed {
        /// The unit letter after the digits.
        unit: char,
    },
}

/// A named ACP agent and how to start it over stdio.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpAgentConfig {
    /// Profile name, e.g. `claude` for the `acp:claude` provider.
    pub profile_name: String,
    /// Executable path, or a name looked up on `PATH`.
    pub command: PathBuf,
    /// Arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables added to aim's own environment for the agent process. Values may be
    /// credentials: they are never printed ([`core::fmt::Debug`] masks them).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory of the agent process (sessions carry their own cwd); aim's own cwd when
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// How the agent spells variants of its model values (docs/adr/0075). Empty: a requested
    /// model matches exactly, ignoring case, or by family, and no suffix is read as a variant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_variants: Vec<VariantSyntax>,
}

impl AcpAgentConfig {
    /// A profile running `command` with no arguments.
    #[must_use]
    pub fn new(profile_name: impl Into<String>, command: impl Into<PathBuf>) -> Self {
        Self {
            profile_name: profile_name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            model_variants: Vec::new(),
        }
    }

    /// The `claude` profile: the `claude-agent-acp` adapter from `PATH` (pinned via mise). Its
    /// model values carry a context size as `[1m]`, and the adapter reads `-1m` as the same
    /// (claude-agent-acp 0.81.2 `dist/session-model.js:3-4`).
    #[must_use]
    pub fn claude() -> Self {
        let mut config = Self::new("claude", CLAUDE_AGENT_ACP);
        config.model_variants = vec![VariantSyntax::Bracketed { unit: 'm' }, VariantSyntax::Dashed { unit: 'm' }];
        config
    }

    /// Appends arguments.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets one environment variable for the agent process.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Sets the agent process's working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// The provider id for sessions of this agent (`acp:<profile>`), used as the
    /// [`aim_proto::conversation::NativeItem::provider`] of items it produces.
    #[must_use]
    pub fn provider_id(&self) -> String {
        format!("acp:{}", self.profile_name)
    }

    /// Resolves [`Self::command`] to an existing executable: a path is checked as is, a bare name
    /// is searched on `PATH` (the configured `PATH` override first, then aim's own).
    ///
    /// # Errors
    ///
    /// [`AcpError::AgentNotFound`] when nothing executable matches.
    pub fn resolve_command(&self) -> Result<PathBuf, AcpError> {
        let not_found = || AcpError::AgentNotFound {
            command: "<redacted>".to_owned(),
            hint: if self.command.as_os_str() == CLAUDE_AGENT_ACP {
                "install the pinned adapter with `mise install` (npm:@agentclientprotocol/claude-agent-acp) and run aim under mise"
                    .to_owned()
            } else {
                "check the profile's `command` (an absolute path, or a program on PATH)".to_owned()
            },
        };
        if self.command.components().count() > 1 || self.command.is_absolute() {
            let path = match &self.cwd {
                Some(cwd) if self.command.is_relative() => cwd.join(&self.command),
                _ => self.command.clone(),
            };
            return if is_executable(&path) { Ok(path) } else { Err(not_found()) };
        }
        let search = self.env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
        std::env::split_paths(&search).map(|dir| dir.join(&self.command)).find(|candidate| is_executable(candidate)).ok_or_else(not_found)
    }
}

/// Whether `path` is a file the current user may execute.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl core::fmt::Debug for AcpAgentConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AcpAgentConfig")
            .field("profile_name", &"***")
            .field("command", &"***")
            .field("args", &"***")
            .field("env_count", &self.env.len())
            .field("cwd", &"***")
            .field("model_variants", &self.model_variants)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_masks_env_values() {
        let config = AcpAgentConfig::claude().with_env("ANTHROPIC_API_KEY", "sk-secret-value");
        let debug = format!("{config:?}");
        assert!(!debug.contains("ANTHROPIC_API_KEY"));
        assert!(!debug.contains("sk-secret-value"));
    }

    #[test]
    fn missing_binary_is_a_typed_error_with_a_hint() {
        let config = AcpAgentConfig::new("x", "aim-acp-definitely-not-installed").with_env("PATH", "/nonexistent");
        match config.resolve_command() {
            Err(AcpError::AgentNotFound { command, .. }) => assert_eq!(command, "<redacted>"),
            other => panic!("unexpected {other:?}"),
        }
        let claude = AcpAgentConfig::claude().with_env("PATH", "/nonexistent");
        match claude.resolve_command() {
            Err(AcpError::AgentNotFound { hint, .. }) => assert!(hint.contains("mise")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn profiles_round_trip_as_config_data() {
        let config = AcpAgentConfig::claude().with_args(["--hide-claude-auth"]).with_cwd("/tmp");
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"profile_name": "claude", "command": "claude-agent-acp", "args": ["--hide-claude-auth"], "cwd": "/tmp",
                               "model_variants": [{"syntax": "bracketed", "unit": "m"}, {"syntax": "dashed", "unit": "m"}]})
        );
        // A profile that declares no variant syntax has none.
        let other = serde_json::from_value::<AcpAgentConfig>(serde_json::json!({"profile_name": "x", "command": "x-acp"})).unwrap();
        assert!(other.model_variants.is_empty());
        assert_eq!(serde_json::from_value::<AcpAgentConfig>(json).unwrap(), config);
        assert_eq!(config.provider_id(), "acp:claude");
    }
}
