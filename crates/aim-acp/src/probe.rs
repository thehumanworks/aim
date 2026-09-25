//! The capability probe: what aim relies on, checked against the running adapter. aim gates
//! features on this report, never on a version string (docs/adr/0012).

use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::client::{AcpClient, AgentCapabilities, AgentInfo};
use crate::config_options::{self, ConfigKey, ConfigOption};
use crate::error::AcpError;
use crate::options::{SessionOptions, ToolAuthority};

/// Something aim needs from the agent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// A terminal login method (aim never sends API keys through `authenticate`).
    TerminalLogin,
    /// A `model` config option (category `model`).
    ModelOption,
    /// A `mode` config option (category `mode`).
    ModeOption,
    /// `session/set_config_option` answers with the applied options.
    SetConfigOption,
    /// Claude Code `_meta.claudeCode.options` (tool replacement, `persistSession`).
    ClaudeCodeOptions,
    /// `session/close`, to end probe and private sessions deterministically.
    SessionClose,
}

/// What the probe found.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// The agent (informational).
    pub agent: AgentInfo,
    /// Advertised capabilities.
    pub capabilities: AgentCapabilities,
    /// Ids of the terminal login methods.
    pub terminal_login_methods: Vec<String>,
    /// Config options of a fresh session.
    pub config_options: Vec<ConfigOption>,
    /// Whether the current model has an effort option (it varies per model).
    pub effort_option: bool,
    /// Whether a no-op `session/set_config_option` (mode → its current value) succeeded.
    pub set_config_option: bool,
    /// Latency of `session/new` for the probe session, in milliseconds.
    pub session_new_ms: u64,
    /// Latency of the no-op `session/set_config_option`, in milliseconds.
    pub set_config_ms: Option<u64>,
    /// Requirements of native-tools sessions that are not met.
    pub missing_native: Vec<Requirement>,
    /// Requirements of aim-tools sessions that are not met. The probe cannot see whether stdio MCP
    /// servers actually spawn (adapter issue #883): that needs the live conformance run.
    pub missing_aim_tools: Vec<Requirement>,
}

impl core::fmt::Debug for ProbeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProbeReport")
            .field("capabilities", &self.capabilities)
            .field("missing_native", &self.missing_native)
            .field("missing_aim_tools", &self.missing_aim_tools)
            .finish_non_exhaustive()
    }
}

impl ProbeReport {
    /// Whether sessions with `authority` can be offered.
    #[must_use]
    pub fn supports(&self, authority: &ToolAuthority) -> bool {
        match authority {
            ToolAuthority::Native => self.missing_native.is_empty(),
            ToolAuthority::Aim | ToolAuthority::LocalMixed { .. } => self.missing_aim_tools.is_empty(),
        }
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl AcpClient {
    /// Probes the agent: the `initialize` capabilities plus a throwaway, unpersisted session in
    /// `cwd` (no model call) to read its config options and exercise
    /// `session/set_config_option`.
    ///
    /// # Errors
    ///
    /// Only when the probe session cannot be created (including [`AcpError::NeedsLogin`]); a
    /// missing capability is reported, not an error.
    pub async fn probe(&self, cwd: &Path) -> Result<ProbeReport, AcpError> {
        let mut options = SessionOptions::new(cwd);
        options.persist = false;
        self.probe_with(options).await
    }

    /// [`Self::probe`] with a caller-chosen probe session, e.g. aim-tools options carrying aim's MCP
    /// server: Claude Code connects session MCP servers while it creates the session, before any
    /// prompt (observed live on 0.81.2), so aim's server itself can confirm it was spawned.
    ///
    /// # Errors
    ///
    /// As [`Self::probe`].
    pub async fn probe_with(&self, options: SessionOptions) -> Result<ProbeReport, AcpError> {
        let start = Instant::now();
        let mut session = self.new_session_unchecked(options).await?;
        let session_new_ms = elapsed_ms(start);
        let config = session.config_options().to_vec();
        let mode = config_options::find(&config, &ConfigKey::Mode).and_then(ConfigOption::current);
        let (set_config_option, set_config_ms) = match mode {
            Some(mode) => {
                let start = Instant::now();
                let ok = session.set_config(&ConfigKey::Mode, &mode).await.is_ok();
                (ok, Some(elapsed_ms(start)))
            }
            None => (false, None),
        };
        let capabilities = self.capabilities().clone();
        if capabilities.sessions.close {
            // Best effort: the probe's result does not depend on the session ending cleanly.
            drop(session.close().await);
        }
        let terminal_login_methods: Vec<String> = self.auth_methods().iter().filter(|m| m.is_terminal()).map(|m| m.id.clone()).collect();
        let mut missing_native = Vec::new();
        let checks = [
            (Requirement::TerminalLogin, !terminal_login_methods.is_empty()),
            (Requirement::ModelOption, config_options::find(&config, &ConfigKey::Model).is_some()),
            (Requirement::ModeOption, config_options::find(&config, &ConfigKey::Mode).is_some()),
            (Requirement::SetConfigOption, set_config_option),
            (Requirement::SessionClose, capabilities.sessions.close),
        ];
        missing_native.extend(checks.iter().filter(|(_, ok)| !ok).map(|(requirement, _)| *requirement));
        let mut missing_aim_tools = missing_native.clone();
        if !capabilities.claude_code {
            missing_aim_tools.push(Requirement::ClaudeCodeOptions);
        }
        Ok(ProbeReport {
            agent: self.agent().clone(),
            effort_option: config_options::find(&config, &ConfigKey::Effort).is_some(),
            capabilities,
            terminal_login_methods,
            config_options: config,
            set_config_option,
            session_new_ms,
            set_config_ms,
            missing_native,
            missing_aim_tools,
        })
    }
}
