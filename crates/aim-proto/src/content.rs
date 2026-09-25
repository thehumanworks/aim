//! Bytes on the wire: UTF-8 text when possible (readable, token-cheap), base64 otherwise.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Arbitrary bytes, carried as a base64 string in JSON (docs/architecture.md §4.4).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Base64Bytes(pub Vec<u8>);

impl Serialize for Base64Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        STANDARD.decode(encoded.as_bytes()).map(Self).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Base64Bytes {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Base64Bytes".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "contentEncoding": "base64" })
    }
}

/// File or stream content: text when it is valid UTF-8, base64 bytes otherwise.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum Content {
    /// Valid UTF-8 text, carried as-is.
    Utf8 {
        /// The text.
        text: String,
    },
    /// Bytes that are not valid UTF-8.
    Base64 {
        /// The bytes.
        data: Base64Bytes,
    },
}

impl Content {
    /// Chooses the cheapest faithful encoding for `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Utf8 { text },
            Err(err) => Self::Base64 { data: Base64Bytes(err.into_bytes()) },
        }
    }

    /// The raw bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Utf8 { text } => text.into_bytes(),
            Self::Base64 { data } => data.0,
        }
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Utf8 { text } => text.len(),
            Self::Base64 { data } => data.0.len(),
        }
    }

    /// Whether the content is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
