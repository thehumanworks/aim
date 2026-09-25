//! Errors of the ACP client. Messages never contain secrets: agent-provided text passes through
//! [`crate::redact::redact`] before it is stored here.

use aim_proto::error::ErrorCode;
use serde_json::Value;

/// The JSON-RPC error code ACP assigns to `authRequired`.
pub const AUTH_REQUIRED_CODE: i32 = -32000;

/// Why an ACP operation failed.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum AcpError {
    /// The adapter executable could not be found (not on `PATH`, or the configured path does not
    /// exist).
    AgentNotFound {
        /// The command as configured.
        command: String,
        /// What to do about it.
        hint: String,
    },
    /// The adapter process could not be started.
    Spawn {
        /// The command as configured.
        command: String,
        /// The OS error.
        message: String,
    },
    /// The adapter process exited or closed its protocol stream.
    AgentExited {
        /// Exit code, when the process exited normally.
        code: Option<i32>,
        /// The tail of the adapter's stderr (bounded, redacted), for diagnostics.
        stderr_tail: String,
    },
    /// The agent needs the user to log in (ACP `authRequired`). Run
    /// [`crate::AcpClient::login_command`] for one of the advertised methods, then retry.
    NeedsLogin {
        /// The agent's message.
        message: String,
        /// A machine reason the agent gave, e.g. `claude_subscription_not_supported`.
        reason: Option<String>,
        /// Ids of the advertised login methods.
        methods: Vec<String>,
    },
    /// The agent answered a request with a JSON-RPC error.
    Rpc {
        /// The method that failed.
        method: String,
        /// The JSON-RPC error code.
        code: i32,
        /// The agent's message (redacted).
        message: String,
        /// The agent's structured error data, if any.
        data: Option<Value>,
    },
    /// The agent's answer did not have the shape ACP requires.
    Protocol {
        /// The method whose answer was malformed, or the notification name.
        method: String,
        /// What was wrong.
        message: String,
    },
    /// A request did not complete in time.
    Timeout {
        /// The method that timed out.
        method: String,
        /// The deadline in milliseconds.
        after_ms: u64,
    },
    /// No login method with this id was advertised.
    UnknownAuthMethod {
        /// The requested id.
        id: String,
    },
    /// The login method is not a terminal method, so it cannot be run as a command.
    NotTerminalAuth {
        /// The method id.
        id: String,
    },
    /// The session does not advertise the requested configuration option.
    ConfigUnavailable {
        /// The requested option (`model`, `effort`, `mode`, or an id).
        key: String,
    },
    /// The requested value is not one of the option's advertised values.
    ConfigValueRejected {
        /// The option id.
        id: String,
        /// The requested value.
        value: String,
        /// The advertised values.
        allowed: Vec<String>,
    },
    /// The agent accepted `session/set_config_option` but did not apply the value.
    ConfigNotApplied {
        /// The option id.
        id: String,
        /// The requested value.
        requested: String,
        /// The value the agent reports as current.
        current: Option<String>,
    },
    /// A previous turn on this session did not finish after being cancelled.
    TurnStillRunning,
    /// The peer sent more updates than the session can safely buffer.
    QueueOverflow,
    /// The operation is not valid in the client's current state.
    InvalidState(String),
}

impl AcpError {
    /// The closest aim protocol error code (docs/architecture.md §4.4).
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::AgentNotFound { .. } | Self::Spawn { .. } | Self::AgentExited { .. } => ErrorCode::Unavailable,
            Self::NeedsLogin { .. } => ErrorCode::Unauthenticated,
            Self::Rpc { .. } | Self::Protocol { .. } => ErrorCode::Internal,
            Self::Timeout { .. } => ErrorCode::Timeout,
            Self::UnknownAuthMethod { .. } | Self::ConfigUnavailable { .. } => ErrorCode::NotFound,
            Self::NotTerminalAuth { .. } | Self::ConfigValueRejected { .. } => ErrorCode::InvalidParams,
            Self::ConfigNotApplied { .. } | Self::TurnStillRunning | Self::InvalidState(_) => ErrorCode::PreconditionFailed,
            Self::QueueOverflow => ErrorCode::LimitExceeded,
        }
    }
}

impl core::fmt::Debug for AcpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Debug is routinely recorded by test failures and telemetry. Do not echo any
        // caller-supplied or peer-supplied field, including structured error data.
        f.debug_struct("AcpError").field("code", &self.code()).finish_non_exhaustive()
    }
}

impl core::fmt::Display for AcpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AgentNotFound { .. } => f.write_str("ACP agent not found; check the configured command and mise installation"),
            Self::Spawn { .. } => f.write_str("could not start ACP agent"),
            Self::AgentExited { code, stderr_tail } => {
                match code {
                    Some(code) => write!(f, "ACP agent exited with code {code}")?,
                    None => f.write_str("ACP agent closed the connection")?,
                }
                if stderr_tail.is_empty() { Ok(()) } else { f.write_str("; agent stderr omitted") }
            }
            Self::NeedsLogin { reason, .. } => {
                f.write_str("login required")?;
                if let Some(reason) = reason {
                    write!(f, " ({reason})")?;
                }
                Ok(())
            }
            Self::Rpc { method, code, .. } => write!(f, "`{method}` failed ({code}); agent details omitted"),
            Self::Protocol { method, .. } => write!(f, "malformed `{method}` from the agent"),
            Self::Timeout { method, after_ms } => write!(f, "`{method}` timed out after {after_ms} ms"),
            Self::UnknownAuthMethod { .. } => f.write_str("the agent advertises no matching login method"),
            Self::NotTerminalAuth { .. } => f.write_str("the login method is not a terminal login"),
            Self::ConfigUnavailable { .. } => f.write_str("the session has no matching configuration option"),
            Self::ConfigValueRejected { .. } => f.write_str("the configuration value was not advertised"),
            Self::ConfigNotApplied { .. } => f.write_str("the agent did not apply the configuration value"),
            Self::TurnStillRunning => f.write_str("the previous turn is still running after cancellation"),
            Self::QueueOverflow => f.write_str("ACP session update queue exceeded its memory limit"),
            Self::InvalidState(_) => f.write_str("ACP client state does not permit the operation"),
        }
    }
}

impl core::error::Error for AcpError {}

impl From<AcpError> for aim_proto::error::ProtoError {
    fn from(error: AcpError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}
