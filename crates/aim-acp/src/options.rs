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

/// Who executes the agent's tools (docs/architecture.md §6.1).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolAuthority {
    /// The agent's built-ins run in its own process. aim sees them only as ACP updates: they are
    /// rendered and logged but not admitted, hooked or shadowed.
    #[default]
    Native,
    /// Built-ins are disabled and every tool call goes to aim's MCP server (named
    /// [`AIM_MCP_SERVER`], which the caller must pass in [`SessionOptions::mcp_servers`]).
    Aim {
        /// Claude built-ins to keep (e.g. `WebSearch`, `WebFetch`, `TodoWrite`, `Task`); empty
        /// disables all of them.
        keep_builtins: Vec<String>,
        /// Built-in name → aim MCP tool, so model-emitted `Bash` etc. still resolve.
        aliases: BTreeMap<String, String>,
    },
}

impl ToolAuthority {
    /// Aim tool authority with no built-ins kept and the [`DEFAULT_ALIASES`].
    #[must_use]
    pub fn aim() -> Self {
        Self::Aim {
            keep_builtins: Vec::new(),
            aliases: DEFAULT_ALIASES.iter().map(|(from, to)| ((*from).to_owned(), (*to).to_owned())).collect(),
        }
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
        let masked = |map: &BTreeMap<String, String>| map.keys().map(|k| (k.clone(), "***")).collect::<BTreeMap<_, _>>();
        match self {
            Self::Stdio { name, command, args, env } => f
                .debug_struct("Stdio")
                .field("name", name)
                .field("command", command)
                .field("args", args)
                .field("env", &masked(env))
                .finish(),
            Self::Http { name, url, headers } => {
                f.debug_struct("Http").field("name", name).field("url", url).field("headers", &masked(headers)).finish()
            }
        }
    }
}

/// How to create a session.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
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
}

impl SessionOptions {
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
        }
    }

    /// The `_meta` object for `session/new` (`None` when nothing needs to be said).
    #[must_use]
    pub fn meta(&self) -> Option<Map<String, Value>> {
        let mut options = self.claude_options.clone();
        if let ToolAuthority::Aim { keep_builtins, aliases } = &self.tool_authority {
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
