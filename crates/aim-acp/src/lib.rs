//! `aim-acp`: aim as an ACP client of external agents — Claude Code through `claude-agent-acp`
//! first (docs/architecture.md §6.1, docs/adr/0012).
//!
//! - [`AcpClient`] spawns an agent over stdio (or connects over any byte pipe), runs
//!   `initialize`, and exposes the agent's [`AgentCapabilities`], login methods
//!   ([`AcpClient::login_command`]) and account state.
//! - [`AcpClient::probe`] checks what aim relies on and returns a typed [`ProbeReport`]; aim gates
//!   on that, never on a version string.
//! - [`AcpClient::new_session`] creates an [`AcpSession`] from [`SessionOptions`], including the
//!   `_meta.claudeCode.options` that select [`ToolAuthority::Native`] or [`ToolAuthority::Aim`]
//!   and `persistSession: false` for private sessions.
//! - [`AcpSession::prompt`] streams a turn as [`AcpEvent`]s: every `session/update` typed *and*
//!   raw, permission decisions, complete [`aim_proto::conversation::Item`]s where the mapping is
//!   lossless, and the stop reason with usage.
//! - [`PermissionHandler`] answers `session/request_permission`; [`YoloPermissions`] is the
//!   default (docs/adr/0021).
//!
//! Wire: ACP v1 via `agent-client-protocol` 2.2 with no unstable features. Requests are sent
//! untyped and results decoded tolerantly (field by field), so an adapter adding fields, update
//! kinds or stop reasons never breaks a session; unknown updates arrive as [`Update::Other`] with
//! their raw payload.
//!
//! Process handling: the agent leads its own process group, which is killed when the last
//! handle drops; stderr is kept as a bounded, redacted tail for diagnostics; a missing binary is
//! a typed [`AcpError::AgentNotFound`].

mod auth;
mod client;
mod config;
mod config_options;
mod error;
mod events;
mod options;
mod permission;
mod probe;
mod process;
pub mod redact;
mod session;
mod wire;

pub use auth::{AuthMethodInfo, AuthMethodKind, LoginCommand, TerminalAuthMeta, login_command, parse_auth_methods};
pub use client::{
    AcpClient, AcpClientBuilder, AgentCapabilities, AgentInfo, AuthStatus, DEFAULT_REQUEST_TIMEOUT, McpCapabilities, PromptCapabilities,
    SessionCapabilities, initialize_params,
};
pub use config::{AcpAgentConfig, CLAUDE_AGENT_ACP};
pub use config_options::{ConfigKey, ConfigKind, ConfigOption, ConfigValue, parse_config_options};
pub use error::{AUTH_REQUIRED_CODE, AcpError};
pub use events::{
    AcpEvent, AvailableCommand, Chunk, ContentPart, ContextUsage, PlanEntry, ToolCallContent, ToolCallState, ToolCallStatus, ToolKind,
    ToolLocation, TurnCollector, TurnEnd, Update, parse_turn_end, parse_update,
};
pub use options::{AIM_MCP_SERVER, DEFAULT_ALIASES, McpServerSpec, SessionOptions, ToolAuthority};
pub use permission::{
    PermissionDecision, PermissionHandler, PermissionKind, PermissionOption, PermissionRequest, YoloPermissions, yolo_choice,
};
pub use probe::{ProbeReport, Requirement};
pub use process::{MAX_LINE_BYTES, STDERR_TAIL_BYTES, WireDirection, WireTap};
pub use session::{ABANDONED_TURN_GRACE, AcpSession, Turn};
