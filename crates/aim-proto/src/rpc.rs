//! The JSON-RPC 2.0 envelope and typed methods.
//!
//! Framing (NDJSON, WebSocket frames, HTTP bodies) lives in `aim-rpc`; this module only defines
//! what a message *is*. Methods are type-level markers implementing [`Method`], so a call site
//! names the marker and gets typed params and results:
//! `peer.call::<harness::FsRead>(params) -> harness::FsReadResult`.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ErrorCode, ProtoError};

/// A request/response method: its wire name and its params and result types.
pub trait Method: 'static {
    /// The wire name, e.g. `fs.read`.
    const NAME: &'static str;
    /// Request parameters.
    type Params: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
    /// Successful result.
    type Result: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

/// A one-way notification: its wire name and its params type.
pub trait Notification: 'static {
    /// The wire name, e.g. `exec.output`.
    const NAME: &'static str;
    /// Notification parameters.
    type Params: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

/// Declares a method marker type.
#[macro_export]
macro_rules! method {
    ($(#[$doc:meta])* $marker:ident = $name:literal ($params:ty) -> $result:ty) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug)]
        pub enum $marker {}
        impl $crate::rpc::Method for $marker {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
    };
}

/// Declares a notification marker type.
#[macro_export]
macro_rules! notification {
    ($(#[$doc:meta])* $marker:ident = $name:literal ($params:ty)) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug)]
        pub enum $marker {}
        impl $crate::rpc::Notification for $marker {
            const NAME: &'static str = $name;
            type Params = $params;
        }
    };
}

/// A JSON-RPC request id.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id (aim's peers always send these).
    Number(i64),
    /// String id (accepted from other clients).
    String(String),
}

/// The error object of a JSON-RPC response.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ErrorObject {
    /// Numeric code ([`ErrorCode::number`]).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// `{"kind": <ErrorCode name>, "detail": …}` for aim peers; anything for foreign peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl From<ProtoError> for ErrorObject {
    fn from(err: ProtoError) -> Self {
        let mut data = serde_json::Map::new();
        data.insert("kind".to_owned(), Value::String(err.code.name().to_owned()));
        if let Some(detail) = err.detail {
            data.insert("detail".to_owned(), detail);
        }
        Self { code: err.code.number(), message: err.message, data: Some(Value::Object(data)) }
    }
}

impl From<ErrorObject> for ProtoError {
    fn from(obj: ErrorObject) -> Self {
        let kind = obj.data.as_ref().and_then(|d| d.get("kind")).and_then(Value::as_str);
        let code =
            kind.and_then(|k| ErrorCode::ALL.into_iter().find(|c| c.name() == k)).unwrap_or_else(|| ErrorCode::from_number(obj.code));
        let detail = obj.data.and_then(|d| d.get("detail").cloned());
        Self { code, message: obj.message, detail }
    }
}

/// Any JSON-RPC message as it appears on the wire, before classification.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Envelope {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Present on requests and responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// Present on requests and notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Request or notification parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Success payload of a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure payload of a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

/// A classified JSON-RPC message.
#[derive(Clone, PartialEq, Debug)]
pub enum Message {
    /// A call expecting a response.
    Request {
        /// Correlation id.
        id: RequestId,
        /// Method name.
        method: String,
        /// Parameters (`null` when absent).
        params: Value,
    },
    /// A one-way message.
    Notification {
        /// Notification name.
        method: String,
        /// Parameters (`null` when absent).
        params: Value,
    },
    /// The answer to a request.
    Response {
        /// The request's id.
        id: RequestId,
        /// Its outcome.
        outcome: Result<Value, ErrorObject>,
    },
}

impl Message {
    /// Classifies an envelope.
    ///
    /// # Errors
    /// [`ErrorCode::InvalidRequest`] when the envelope is not a request, notification or response.
    pub fn from_envelope(env: Envelope) -> Result<Self, ProtoError> {
        if env.jsonrpc != "2.0" {
            return Err(ProtoError::new(ErrorCode::InvalidRequest, "jsonrpc must be \"2.0\""));
        }
        match (env.id, env.method) {
            (Some(id), Some(method)) => Ok(Self::Request { id, method, params: env.params.unwrap_or(Value::Null) }),
            (None, Some(method)) => Ok(Self::Notification { method, params: env.params.unwrap_or(Value::Null) }),
            (Some(id), None) => {
                let outcome = match env.error {
                    Some(error) => Err(error),
                    None => Ok(env.result.unwrap_or(Value::Null)),
                };
                Ok(Self::Response { id, outcome })
            }
            (None, None) => Err(ProtoError::new(ErrorCode::InvalidRequest, "message has neither id nor method")),
        }
    }

    /// Converts back to a wire envelope.
    #[must_use]
    pub fn into_envelope(self) -> Envelope {
        let mut env = Envelope { jsonrpc: "2.0".to_owned(), ..Envelope::default() };
        match self {
            Self::Request { id, method, params } => {
                env.id = Some(id);
                env.method = Some(method);
                env.params = Some(params);
            }
            Self::Notification { method, params } => {
                env.method = Some(method);
                env.params = Some(params);
            }
            Self::Response { id, outcome } => {
                env.id = Some(id);
                match outcome {
                    Ok(result) => env.result = Some(result),
                    Err(error) => env.error = Some(error),
                }
            }
        }
        env
    }
}

/// Cancels an in-flight request (`$/cancel`); the peer answers it with [`ErrorCode::Cancelled`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CancelParams {
    /// The request to cancel.
    pub id: RequestId,
}

notification!(
    /// `$/cancel` — cancel an in-flight request.
    Cancel = "$/cancel" (CancelParams)
);
