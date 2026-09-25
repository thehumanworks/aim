//! `aim`: the agent layer (docs/architecture.md §2, §6).
//!
//! - [`agent`] — the native agent loop, driven by the verified turn state machine.
//! - [`harness`] — the client side of `aim-harness/1`: tools executed by aimx.
//! - [`store`] — session persistence (SQLite by default, memory for ephemeral sessions).
//! - [`session`] — recording agent events into the durable session log.
//! - [`context`] — instructions and environment given to the model.
//! - [`cli`] — the headless `aim run`.
//! - [`host`] — hosting live sessions (the daemon's core; also used in process).
//! - [`providers`] — model providers by id.
//! - [`acp`] — Claude Code (and other ACP agents) as a session backend.
//! - [`login`] — `aim login codex | claude`.
//! - [`daemon`] — the local daemon server, client and auto-spawn.
pub mod acp;
pub mod agent;
pub mod cli;
pub mod context;
pub mod daemon;
pub mod harness;
pub mod host;
pub mod login;
pub mod providers;
pub mod remote;
pub mod session;
pub mod store;
