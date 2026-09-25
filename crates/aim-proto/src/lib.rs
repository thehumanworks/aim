//! `aim-proto`: every message aim puts on a wire or on disk, defined once as Rust types
//! (docs/architecture.md §4, docs/adr/0006).
//!
//! This crate has no I/O and compiles to wasm (the web client uses it too). JSON Schema for every
//! type comes from `schemars`; MCP tool schemas, the web client's types and the protocol docs are
//! generated from here, never written by hand.
//!
//! - [`rpc`] — the JSON-RPC 2.0 envelope and the typed [`rpc::Method`]/[`rpc::Notification`]
//!   traits that `aim-rpc` dispatches on.
//! - [`error`] — the closed set of machine-readable error codes (§4.4).
//! - [`content`] — file and output bytes on the wire (UTF-8 text or base64).
//! - [`ids`] — identifiers, idempotency keys and resume tokens.
//! - [`harness`] — `aim-harness/1`: the execution layer's methods.
//! - [`daemon`] — `aim-daemon/1`: clients ↔ the agent daemon (sessions and their updates).
//! - [`tool`] — tool descriptors and results, shared by aimx, the agent layer and MCP.
//! - [`conversation`] — provider-neutral conversation items, usage and rate limits.
//! - [`event`] — durable session events (the append-only session log).
pub mod content;
pub mod conversation;
pub mod daemon;
pub mod error;
pub mod event;
pub mod harness;
pub mod ids;
pub mod rpc;
pub mod tool;

/// Protocol generations of `aim-harness` this build can speak (inclusive range).
///
/// Negotiated at `initialize` with `aim_kernel::negotiate`'s LOCKED rule: the newest generation
/// both peers support, or refusal when the ranges are disjoint (docs/adr/0005).
pub const HARNESS_GENERATIONS: (u32, u32) = (1, 1);
