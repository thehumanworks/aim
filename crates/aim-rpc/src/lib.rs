//! `aim-rpc`: the JSON-RPC 2.0 peer used by `aim-harness/1` and `aim-daemon/1`
//! (docs/architecture.md §4, docs/adr/0006).
//!
//! A [`Peer`] is one side of a connection over any byte stream (stdio, unix socket, an SSH
//! channel, an in-process duplex). Both sides can call, notify and serve:
//!
//! - outgoing calls are correlated by id; dropping a call's future sends `$/cancel`;
//! - incoming requests run concurrently, each with a cancellation token tripped by `$/cancel`;
//! - incoming notifications reach handlers in wire order through one bounded queue per connection;
//! - when the connection ends, every pending call fails with `unavailable`;
//! - a typed [`Router`] turns [`aim_proto::rpc::Method`] markers into handlers, so parameters are
//!   validated against their Rust types before handler code runs.
//!
//! Framing is NDJSON (one compact JSON message per line) with a maximum message size.
mod framing;
mod peer;
mod router;

pub use framing::DEFAULT_MAX_MESSAGE_BYTES;
pub use peer::{Handler, NoHandler, NotificationCtx, Peer, PeerConfig, RequestCtx};
pub use router::Router;
