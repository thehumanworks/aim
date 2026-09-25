//! Session options and the `session/new` parameters they produce, including the
//! `_meta.claudeCode.options` that decide Claude's tool authority (docs/adr/0012,
//! docs/research/acp-mcp.md §2.7–2.8).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The name aim's MCP server must have inside a Claude session: tool aliases and `allowedTools`
/// refer to its tools as `mcp__aim__<tool>`.
pub const AIM_MCP_SERVER: &str = "aim";

/// Claude Code built-ins that aim replaces in [`ToolAuthority::Aim`] mode, with the aim MCP tool
/// each one is aliased to.
pub const DEFAULT_ALIASES: [(&str, &str); 6] = [
    ("Bash", "mcp__aim__bash"),
    ("Read", "mcp__aim__read"),
    ("Edit", "mcp__aim__edit"),
    ("Write", "mcp__aim__write"),
    ("Glob", "mcp__aim__glob"),
    ("Grep", "mcp__aim__grep"),
];

/// The code tool of aim's code-mode relay, as Claude names it (ADR 0076).
pub const AIM_CODE_TOOL: &str = "mcp__aim__run_code";

/// Which strict aim relay a session uses, and so which aim tools stand in for Claude's built-ins
/// (ADR 0076).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AimRoute {
    /// `aimx mcp`: its lowercase tools, aliased by [`DEFAULT_ALIASES`].
    #[default]
    Aimx,
    /// aim's code-mode relay (`aim code-mcp`): `run_code`, the saved-program tools, and these
    /// direct tools under their own names (none in code mode `only`).
    Code {
        /// The direct tools the relay shows, by name (`Read`, `Bash`, …).
        direct: Vec<String>,
    },
}

impl AimRoute {
    /// The aim MCP tool that stands in for Claude's built-in `builtin`, when the relay shows one.
    #[must_use]
    pub fn tool_for(&self, builtin: &str) -> Option<String> {
        match self {
            Self::Aimx => DEFAULT_ALIASES.iter().find(|(from, _)| *from == builtin).map(|(_, to)| (*to).to_owned()),
            Self::Code { direct } => (DEFAULT_ALIASES.iter().any(|(from, _)| *from == builtin)
                && direct.iter().any(|name| name == builtin))
            .then(|| format!("mcp__{AIM_MCP_SERVER}__{builtin}")),
        }
    }

    /// Built-in → aim MCP tool aliases; never one to a tool the relay does not show.
    #[must_use]
    pub fn aliases(&self) -> BTreeMap<String, String> {
        DEFAULT_ALIASES.iter().filter_map(|(from, _)| self.tool_for(from).map(|to| ((*from).to_owned(), to))).collect()
    }
}

/// Who executes the agent's tools (docs/architecture.md §6.1).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolAuthority {
    /// The agent's built-ins run in its own process. aim sees them only as ACP updates: they are
    /// rendered and logged but not admitted, hooked or shadowed.
    #[default]
    Native,
    /// Strict aim authority: no built-ins or other MCP servers. Use
    /// [`SessionOptions::strict_aim`] to construct and validate it.
    Aim,
    /// An explicitly local, mixed-authority session. Kept built-ins run on the adapter host;
    /// this variant must never be accepted for SSH shadowing.
    LocalMixed {
        /// Claude built-ins to keep (e.g. `WebSearch`, `WebFetch`, `TodoWrite`, `Task`); empty
        /// disables all of them.
        keep_builtins: Vec<String>,
        /// Built-in name → aim MCP tool, so model-emitted `Bash` etc. still resolve.
        aliases: BTreeMap<String, String>,
    },
}

impl core::fmt::Debug for ToolAuthority {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self {
            Self::Native => "Native",
            Self::Aim => "Aim",
            Self::LocalMixed { .. } => "LocalMixed",
        };
        f.debug_tuple("ToolAuthority").field(&kind).finish()
    }
}

impl ToolAuthority {
    /// Strict aim tool authority with no built-ins.
    #[must_use]
    pub fn aim() -> Self {
        Self::Aim
    }
}

/// An MCP server the agent should connect to for the session.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerSpec {
    /// A server the agent spawns over stdio.
    Stdio {
        /// Server name (tools appear as `mcp__<name>__<tool>`).
        name: String,
        /// Executable.
        command: PathBuf,
        /// Arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Environment (may hold credentials; masked in [`core::fmt::Debug`]).
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// A streamable-HTTP server.
    Http {
        /// Server name.
        name: String,
        /// Endpoint URL.
        url: String,
        /// Request headers (may hold credentials; masked in [`core::fmt::Debug`]).
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl McpServerSpec {
    /// The server's name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Http { name, .. } => name,
        }
    }

    /// The ACP `McpServer` JSON (stdio entries carry no `type`, as ACP v1 requires).
    #[must_use]
    pub fn to_acp(&self) -> Value {
        let pairs =
            |map: &BTreeMap<String, String>| map.iter().map(|(name, value)| json!({"name": name, "value": value})).collect::<Vec<_>>();
        match self {
            Self::Stdio { name, command, args, env } => json!({"name": name, "command": command, "args": args, "env": pairs(env)}),
            Self::Http { name, url, headers } => json!({"type": "http", "name": name, "url": url, "headers": pairs(headers)}),
        }
    }
}

impl core::fmt::Debug for McpServerSpec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Stdio { env, .. } => f
                .debug_struct("Stdio")
                .field("name", &"***")
                .field("command", &"***")
                .field("args", &"***")
                .field("env_count", &env.len())
                .finish(),
            Self::Http { headers, .. } => {
                f.debug_struct("Http").field("name", &"***").field("url", &"***").field("headers_count", &headers.len()).finish()
            }
        }
    }
}

/// How to create a session.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOptions {
    /// Absolute working directory **on the agent's host** (the adapter rejects missing
    /// directories; remote workspaces use a local scratch directory, docs/adr/0012).
    pub cwd: PathBuf,
    /// MCP servers for the session.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSpec>,
    /// Who executes tools.
    #[serde(default)]
    pub tool_authority: ToolAuthority,
    /// Whether the agent may keep a transcript on disk. `false` requests `persistSession: false`
    /// (private/ephemeral sessions, docs/architecture.md §5.4).
    pub persist: bool,
    /// Text appended to Claude Code's own system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_append: Option<String>,
    /// Extra Claude SDK options merged into `_meta.claudeCode.options` *before* aim's required
    /// keys (so they cannot weaken the tool-authority settings), e.g. `maxTurns`.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub claude_options: Map<String, Value>,
    /// The strict relay's kind, which decides the built-in aliases (ADR 0076).
    #[serde(default)]
    pub aim_route: AimRoute,
}

impl core::fmt::Debug for SessionOptions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let authority = match self.tool_authority {
            ToolAuthority::Native => "native",
            ToolAuthority::Aim => "aim",
            ToolAuthority::LocalMixed { .. } => "local_mixed",
        };
        f.debug_struct("SessionOptions")
            .field("tool_authority", &authority)
            .field("persist", &self.persist)
            .field("mcp_server_count", &self.mcp_servers.len())
            .finish_non_exhaustive()
    }
}

impl SessionOptions {
    /// Creates a strict aim-tools session with one aim MCP relay.
    ///
    /// # Errors
    ///
    /// The relay must be named `aim`.
    pub fn strict_aim(cwd: impl Into<PathBuf>, relay: McpServerSpec) -> Result<Self, crate::AcpError> {
        Self::strict_aim_via(cwd, relay, AimRoute::Aimx)
    }

    /// [`SessionOptions::strict_aim`] through a relay of kind `route` (ADR 0076).
    ///
    /// # Errors
    ///
    /// The relay must be named `aim`.
    pub fn strict_aim_via(cwd: impl Into<PathBuf>, relay: McpServerSpec, route: AimRoute) -> Result<Self, crate::AcpError> {
        let mut options = Self::new(cwd);
        options.tool_authority = ToolAuthority::Aim;
        options.mcp_servers = vec![relay];
        options.aim_route = route;
        options.validate_authority()?;
        Ok(options)
    }

    /// Checks that strict aim authority cannot be weakened by extra servers or SDK options.
    ///
    /// # Errors
    ///
    /// Invalid strict-authority options are rejected before `session/new`.
    pub fn validate_authority(&self) -> Result<(), crate::AcpError> {
        if self.claude_options.contains_key("persistSession") {
            return Err(crate::AcpError::InvalidState("session persistence must use the witnessed option".into()));
        }
        if !matches!(self.tool_authority, ToolAuthority::Aim) {
            return Ok(());
        }
        if self.mcp_servers.len() != 1 || self.mcp_servers.first().is_none_or(|server| server.name() != AIM_MCP_SERVER) {
            return Err(crate::AcpError::InvalidState("strict aim authority needs exactly one aim MCP server".into()));
        }
        // An allowlist is easier to audit than a blacklist of rapidly changing SDK options.
        if self.claude_options.keys().any(|key| !matches!(key.as_str(), "maxTurns" | "maxBudgetUsd")) {
            return Err(crate::AcpError::InvalidState("strict aim authority has a conflicting Claude SDK option".into()));
        }
        Ok(())
    }

    /// Options for a session in `cwd` with native tools, persisted by the agent.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            mcp_servers: Vec::new(),
            tool_authority: ToolAuthority::Native,
            persist: true,
            system_prompt_append: None,
            claude_options: Map::new(),
            aim_route: AimRoute::Aimx,
        }
    }

    /// The `_meta` object for `session/new` (`None` when nothing needs to be said).
    #[must_use]
    pub fn meta(&self) -> Option<Map<String, Value>> {
        let mut options = self.claude_options.clone();
        let tool_settings = match &self.tool_authority {
            ToolAuthority::Native => None,
            ToolAuthority::Aim => Some((Vec::new(), self.aim_route.aliases())),
            ToolAuthority::LocalMixed { keep_builtins, aliases } => Some((keep_builtins.clone(), aliases.clone())),
        };
        if let Some((keep_builtins, aliases)) = tool_settings {
            options.insert("tools".into(), json!(keep_builtins));
            options.insert("toolAliases".into(), json!(aliases));
            options.insert("strictMcpConfig".into(), json!(true));
            options.insert("settingSources".into(), json!([]));
            options.insert("allowedTools".into(), json!([format!("mcp__{AIM_MCP_SERVER}")]));
            // MCP tools are otherwise deferred behind the ToolSearch built-in, which `tools`
            // removes (docs/research/acp-mcp.md §2.7 caveat 5).
            let mut env = options.get("env").and_then(Value::as_object).cloned().unwrap_or_default();
            env.insert("ENABLE_TOOL_SEARCH".into(), json!("false"));
            options.insert("env".into(), Value::Object(env));
        }
        if !self.persist {
            options.insert("persistSession".into(), json!(false));
        }
        let mut meta = Map::new();
        if !options.is_empty() {
            meta.insert("claudeCode".into(), json!({ "options": options }));
        }
        if let Some(append) = &self.system_prompt_append {
            meta.insert("systemPrompt".into(), json!({ "append": append }));
        }
        (!meta.is_empty()).then_some(meta)
    }

    /// The complete `session/new` params.
    #[must_use]
    pub fn new_session_params(&self) -> Value {
        let mut params = json!({
            "cwd": self.cwd,
            "mcpServers": self.mcp_servers.iter().map(McpServerSpec::to_acp).collect::<Vec<_>>(),
        });
        if let (Some(meta), Some(object)) = (self.meta(), params.as_object_mut()) {
            object.insert("_meta".into(), Value::Object(meta));
        }
        params
    }
}
