//! `aim`: the agent layer (docs/architecture.md §2, §6).
//!
//! - [`agent`] — the native agent loop, driven by the verified turn state machine.
//! - [`harness`] — the client side of `aim-harness/1`: tools executed by aimx.
//! - [`store`] — session persistence (SQLite by default, memory for ephemeral sessions).
//! - [`session`] — recording agent events into the durable session log.
//! - [`context`] — instructions and environment given to the model.
//! - [`cli`] — the headless `aim run`.
pub mod agent;
pub mod cli;
pub mod context;
pub mod harness;
pub mod session;
pub mod store;
