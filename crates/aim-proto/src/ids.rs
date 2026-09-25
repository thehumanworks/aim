//! Identifiers shared across the protocols.
//!
//! All ids are opaque strings on the wire. Newtypes keep them from being mixed up in code.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! opaque_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps an existing id.
            #[must_use]
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The id as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(
    /// A workspace opened on a harness (`workspace.open`), valid for the harness session.
    WorkspaceId
);
opaque_id!(
    /// A process spawned by `exec.spawn`, valid until released or its resume token expires.
    ProcId
);
opaque_id!(
    /// A client-generated key that makes a mutating request safe to retry: the harness records the
    /// outcome under this key and replays it instead of executing twice (§4.1 mutation safety).
    IdempotencyKey
);
opaque_id!(
    /// Proof of a harness session that survives transport drops: presented at `initialize` to
    /// resume processes and streams within the session's TTL. Scoped to (principal, harness).
    ResumeToken
);
opaque_id!(
    /// A handle to a large tool output kept by the harness; the model sees head/tail plus this.
    OutputHandle
);
