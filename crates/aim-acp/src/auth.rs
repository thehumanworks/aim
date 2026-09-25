//! Login: the agent's advertised `authMethods` and the command that performs a terminal login.
//!
//! `claude-agent-acp` has no API-key `authenticate` path; subscription and Console logins are
//! *terminal* methods the client runs in a real TTY (docs/research/acp-mcp.md §2.2). aim runs the
//! command in the current terminal (`aim login claude`) or a TUI pane (docs/adr/0012).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::AcpAgentConfig;
use crate::error::AcpError;
use crate::wire;

/// The kind of an advertised login method.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthMethodKind {
    /// The client runs the agent program with `args` appended, in a terminal (ACP v1
    /// `AuthMethod::Terminal`).
    Terminal {
        /// Arguments appended to the agent's own command.
        args: Vec<String>,
        /// Environment overrides.
        env: BTreeMap<String, String>,
    },
    /// The agent handles `authenticate` itself (e.g. the adapter's `gateway` method).
    Agent,
    /// A method type this client does not know.
    Other {
        /// The advertised `type`.
        method_type: String,
    },
}

/// The legacy `_meta["terminal-auth"]` block: an explicit command line (the adapter fills in its
/// own interpreter and script path).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TerminalAuthMeta {
    /// Program to run.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Environment overrides.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Button label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One login method advertised in the `initialize` response.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AuthMethodInfo {
    /// Method id, e.g. `claude-ai-login`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How the method is performed.
    pub kind: AuthMethodKind,
    /// The explicit command, when the agent provided one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_auth: Option<TerminalAuthMeta>,
}

impl AuthMethodInfo {
    /// Whether the method runs as a terminal command.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.kind, AuthMethodKind::Terminal { .. }) || self.terminal_auth.is_some()
    }
}

/// Parses `authMethods` from a raw `initialize` result, skipping malformed entries.
#[must_use]
pub fn parse_auth_methods(initialize_result: &Value) -> Vec<AuthMethodInfo> {
    wire::array(initialize_result, "authMethods").iter().filter_map(parse_auth_method).collect()
}

fn parse_auth_method(raw: &Value) -> Option<AuthMethodInfo> {
    let id = wire::str(raw, "id")?.to_owned();
    let name = wire::str(raw, "name").unwrap_or(&id).to_owned();
    let description = wire::str(raw, "description").map(str::to_owned);
    let kind = match wire::str(raw, "type") {
        Some("terminal") => AuthMethodKind::Terminal { args: wire::strings(raw, "args"), env: wire::string_map(raw.get("env")) },
        None | Some("agent") => AuthMethodKind::Agent,
        Some(other) => AuthMethodKind::Other { method_type: other.to_owned() },
    };
    let terminal_auth = raw.get("_meta").and_then(|meta| meta.get("terminal-auth")).and_then(|block| {
        Some(TerminalAuthMeta {
            command: wire::str(block, "command")?.to_owned(),
            args: wire::strings(block, "args"),
            env: wire::string_map(block.get("env")),
            label: wire::str(block, "label").map(str::to_owned),
        })
    });
    Some(AuthMethodInfo { id, name, description, kind, terminal_auth })
}

/// A login command to run interactively in a terminal. Its output is the user's to see; aim does
/// not capture it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LoginCommand {
    /// The method this performs.
    pub method_id: String,
    /// A label for UIs.
    pub label: String,
    /// The program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Environment variables to add (the agent profile's plus the method's overrides).
    pub env: BTreeMap<String, String>,
}

/// The command performing `method` for an agent launched as `agent`: the method's explicit
/// `_meta["terminal-auth"]` command when present, otherwise the agent's own command with the
/// method's `args` appended (the ACP v1 terminal-auth rule).
///
/// # Errors
///
/// [`AcpError::NotTerminalAuth`] when the method is not a terminal login.
pub fn login_command(method: &AuthMethodInfo, agent: &AcpAgentConfig) -> Result<LoginCommand, AcpError> {
    let mut env = agent.env.clone();
    if let Some(meta) = &method.terminal_auth {
        env.extend(meta.env.clone());
        return Ok(LoginCommand {
            method_id: method.id.clone(),
            label: meta.label.clone().unwrap_or_else(|| method.name.clone()),
            program: PathBuf::from(&meta.command),
            args: meta.args.clone(),
            env,
        });
    }
    let AuthMethodKind::Terminal { args, env: method_env } = &method.kind else {
        return Err(AcpError::NotTerminalAuth { id: method.id.clone() });
    };
    env.extend(method_env.clone());
    let program = agent.resolve_command().unwrap_or_else(|_| agent.command.clone());
    let mut all_args = agent.args.clone();
    all_args.extend(args.iter().cloned());
    Ok(LoginCommand { method_id: method.id.clone(), label: method.name.clone(), program, args: all_args, env })
}
