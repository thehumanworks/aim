//! `aimx`: aim's execution layer (docs/architecture.md §2, §4.1, §9; docs/adr/0002, 0008, 0009).
//!
//! Workspaces, tools and their enforcement, served over `aim-harness/1` (and MCP). aimx holds no
//! conversations and no model credentials, and enforces its own policy for every caller.
//!
//! Module map (OS access is confined to `workspace/local`, `ssh` and `server` —
//! `cargo xtask check` enforces it):
//! - [`workspace`] — the `Workspace`/`Fs`/`Exec`/`Search` traits and the local backend.
pub mod workspace;
