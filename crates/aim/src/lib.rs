//! `aim`: the agent layer (docs/architecture.md §2, §6).
//!
//! - [`agent`] — the native agent loop, driven by the verified turn state machine.
//! - [`harness`] — the client side of `aim-harness/1`: tools executed by aimx.
//! - [`store`] — session persistence (SQLite by default, memory for ephemeral sessions).
//! - [`session`] — recording agent events into the durable session log.
//! - [`search`] — persistent conversation search and read-only agent tools.
//! - [`context`] — instructions and environment given to the model.
//! - [`resources`] — project and user resources: instructions, rules, skills, agents, prompts, memory.
//! - [`cli`] — the headless `aim run`.
//! - [`host`] — hosting live sessions (the daemon's core; also used in process).
//! - [`providers`] — model providers by id.
//! - [`acp`] — Claude Code (and other ACP agents) as a session backend.
//! - [`login`] — `aim login codex | claude`.
//! - [`daemon`] — the local daemon server, client and auto-spawn.
//! - [`tui`] — the terminal UI: inline chat on a [`host::SessionClient`].
//! - [`board`] — the durable job ledger and board CLI.
//! - [`workers`] — isolated board attempt execution and Git integration.
pub mod acp;
pub mod agent;
pub mod board;
pub mod cli;
pub mod coderun;
pub mod context;
pub mod daemon;
pub mod harness;
pub mod host;
pub mod jev;
pub mod login;
pub mod mcp;
pub mod media;
pub mod programs;
pub mod providers;
pub mod remote;
pub mod resources;
pub mod search;
pub mod session;
pub mod store;
pub mod tui;
pub mod workers;
