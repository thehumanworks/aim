//! `aim`: the agent layer (docs/architecture.md §2, §6).
//!
//! - [`agent`] — the native agent loop, driven by the verified turn state machine.
//! - [`harness`] — the client side of `aim-harness/1`: tools executed by aimx.
//! - [`store`] — session persistence (SQLite by default, memory for ephemeral sessions).
pub mod agent;
pub mod harness;
pub mod store;
