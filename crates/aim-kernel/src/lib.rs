//! `aim-kernel`: aim's verified functional core.
//!
//! Every decision aim must never get wrong — state transitions, policies, planners, budgets,
//! protocol negotiation — is a pure function here, specified and proved with Verus
//! (docs/adr/0005). The rest of aim is an imperative shell that asks this crate what to do.
//!
//! Rules for this crate (checked by `cargo xtask check` and `mise run verify`):
//!
//! - **No public function has a `requires` clause.** A precondition is only an assumption about
//!   the caller, and unverified callers can break it. Public entry points are total: they take
//!   plain values and return `Result`/`Option`, with private fields and type invariants.
//! - **Each decision is one spec function** whose doc comment starts with `LOCKED(ADR-NNNN)` once
//!   the decision is final (or `DRAFT(<milestone>)` before). The spec *is* the documentation of the
//!   decision; theorems next to it say why it is safe. A `LOCKED` spec is in the protected set:
//!   changing it needs the maintainer and a superseding ADR (`crates/aim-kernel/LOCKED.toml`
//!   records its digest).
//! - **No cheating:** no `assume`, `admit`, `external_body` or `assume_specification`; time,
//!   randomness and I/O results enter as plain arguments.
//! - **No floats, no async, no I/O, and vstd is the only dependency.**
#![no_std]
#![expect(clippy::indexing_slicing, reason = "every index in aim-kernel is proved in bounds by Verus")]
#![expect(clippy::needless_range_loop, reason = "Verus loop invariants are stated over the index")]
#![expect(clippy::semicolon_if_nothing_returned, reason = "Verus proof blocks erase to unit expressions in the plain build")]
// Under Verus the ghost code is compiled too, and its derive expansion adds undocumented helper
// fns. The plain build still enforces `missing_docs`, and `cargo xtask check` requires a doc
// comment on every public spec and proof fn.
#![cfg_attr(verus_keep_ghost, allow(missing_docs, reason = "Verus derive expansion adds undocumented ghost helpers"))]
extern crate alloc;

pub mod agent_tools;
pub mod board;
pub mod cells;
pub mod compaction;
pub mod dedup;
pub mod discovery;
pub mod effort;
pub mod job;
pub mod negotiate;
pub mod path;
pub mod policy;
pub mod turn;
