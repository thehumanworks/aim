//! The closed set of machine-readable error codes (docs/architecture.md §4.4).
//!
//! JSON-RPC's standard codes cover transport-level failures; aim's application codes live in the
//! implementation-defined server range (-32000..=-32099). Every error also carries its code's
//! stable name in `data.kind`, so logs and humans never need the numeric table.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Why a request failed. Closed: adding a code is a protocol change with an ADR.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The bytes received were not valid JSON.
    ParseError,
    /// The message was not a valid JSON-RPC request.
    InvalidRequest,
    /// The method does not exist (or not in the negotiated generation).
    MethodNotFound,
    /// The parameters did not match the method's schema.
    InvalidParams,
    /// A bug or unexpected failure inside the peer.
    Internal,
    /// The caller is not authenticated.
    Unauthenticated,
    /// Policy forbids the operation for this principal (including protected paths).
    Denied,
    /// The target (workspace, path, process, tool) does not exist.
    NotFound,
    /// A write precondition (`IfAbsent`, `IfHash`) did not hold.
    PreconditionFailed,
    /// The operation conflicts with the target's current state (e.g. an ambiguous exact edit).
    Conflict,
    /// A mutation may or may not have happened and its idempotency record has expired.
    UnknownOutcome,
    /// The capability exists but is not available right now (backend down, feature missing).
    Unavailable,
    /// The operation did not finish within its deadline.
    Timeout,
    /// The caller cancelled the request.
    Cancelled,
    /// A size, rate or concurrency limit was exceeded.
    LimitExceeded,
    /// The peers share no protocol generation.
    UnsupportedGeneration,
}

impl ErrorCode {
    /// Every code, in declaration order.
    pub const ALL: [Self; 16] = [
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::Internal,
        Self::Unauthenticated,
        Self::Denied,
        Self::NotFound,
        Self::PreconditionFailed,
        Self::Conflict,
        Self::UnknownOutcome,
        Self::Unavailable,
        Self::Timeout,
        Self::Cancelled,
        Self::LimitExceeded,
        Self::UnsupportedGeneration,
    ];

    /// The numeric JSON-RPC code.
    #[must_use]
    pub const fn number(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::Internal => -32603,
            Self::Unauthenticated => -32001,
            Self::Denied => -32002,
            Self::NotFound => -32003,
            Self::PreconditionFailed => -32004,
            Self::Conflict => -32005,
            Self::UnknownOutcome => -32006,
            Self::Unavailable => -32007,
            Self::Timeout => -32008,
            Self::Cancelled => -32009,
            Self::LimitExceeded => -32010,
            Self::UnsupportedGeneration => -32011,
        }
    }

    /// The code for a numeric JSON-RPC code; unknown numbers map to [`ErrorCode::Internal`].
    #[must_use]
    pub fn from_number(number: i64) -> Self {
        Self::ALL.into_iter().find(|code| code.number() == number).unwrap_or(Self::Internal)
    }

    /// The stable `snake_case` name carried in `data.kind`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::InvalidParams => "invalid_params",
            Self::Internal => "internal",
            Self::Unauthenticated => "unauthenticated",
            Self::Denied => "denied",
            Self::NotFound => "not_found",
            Self::PreconditionFailed => "precondition_failed",
            Self::Conflict => "conflict",
            Self::UnknownOutcome => "unknown_outcome",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::LimitExceeded => "limit_exceeded",
            Self::UnsupportedGeneration => "unsupported_generation",
        }
    }
}

impl core::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// A protocol error: a code, a human message, and optional typed detail.
#[derive(Clone, PartialEq, Debug)]
pub struct ProtoError {
    /// Machine-readable reason.
    pub code: ErrorCode,
    /// Human-readable explanation (never contains secrets).
    pub message: String,
    /// Optional structured detail, method-specific.
    pub detail: Option<serde_json::Value>,
}

impl ProtoError {
    /// An error with no structured detail.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), detail: None }
    }

    /// Attaches structured detail.
    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

impl core::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl core::error::Error for ProtoError {}
