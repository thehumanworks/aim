//! `aimx`: aim's execution layer (docs/architecture.md §2, §4.1, §9; docs/adr/0002, 0008, 0009).
//!
//! Workspaces, tools and their enforcement, served over `aim-harness/1` (and MCP). aimx holds no
//! conversations and no model credentials, and enforces its own policy for every caller.
//!
//! Module map (OS access is confined to `workspace/local`, `ssh` and `server` —
//! `cargo xtask check` enforces it):
//! - [`workspace`] — the `Workspace`/`Fs`/`Exec`/`Search` traits and the local backend.
//! - [`authz`] — principals, grants, path confinement and protected paths.
//! - [`tools`] — the model-facing tools, written only against the `Workspace` traits.
//! - [`server`] — `aim-harness/1` over unix sockets and stdio: sessions, resume, idempotency.
//! - [`edit`], [`ring`], [`dedup`] — pure decision logic (kernel candidates): exact-edit
//!   application, the output ring buffer and the idempotency table.
pub mod authz;
pub mod dedup;
pub mod edit;
mod id;
pub mod ring;
pub mod server;
pub mod tools;
pub mod workspace;
