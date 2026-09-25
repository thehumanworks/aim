# ADR 0004: Apply a strict workspace lint policy

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0003
- Scope: Rust and Clippy levels across workspace crates; exceptions remain local and justified.

## Context

The brief asks for fully typed Rust and strict Clippy (`docs/vision.md`, Requirements). In the
Verus toy workspace, pedantic plus nursery produced warnings on authored code, no macro-generated
spans, and every suggested fix still verified (`docs/research/verus.md`, F4 Clippy friction).
Strictness is practical for a new agent-written codebase, where lint findings can be fixed at
introduction rather than deferred across a mature tree.

The maintainer's jevgrep ADR 0006 avoided blanket pedantic because its existing CLI produced
style noise. Aim deliberately chooses pedantic: its brief, greenfield state and measured Verus
macro behavior support a stronger starting gate (style precedent: maintainer's jevgrep ADR 0006,
Context and Decision). Restriction remains selected individually.

## Decision

Set lint levels in workspace `Cargo.toml`, and opt member crates into workspace lints. Warn on
Clippy `all` and `pedantic`, then pass `-D warnings` in the required `mise run check` Clippy
command. Deny `correctness` and `suspicious` even for a local Clippy run. Allow only the two
documented pedantic noise cases, `module_name_repetitions` and `must_use_candidate`
(`Cargo.toml`, `[workspace.lints.clippy]`; `mise.toml`, `[tasks.check]`).

Select restriction lints for panic and hidden-failure paths: deny `unwrap_used`, `expect_used`,
`panic`, `todo`, `unimplemented`, `dbg_macro`, `exit`, `get_unwrap`, `string_slice`,
`mem_forget`, and the other explicit entries in `Cargo.toml`. Warn on `indexing_slicing` and
output/debug concerns as declared there; tests have targeted Clippy configuration in
`clippy.toml`. Do not enable the whole restriction group.

Deny `allow_attributes` and `allow_attributes_without_reason`. Use a scoped
`#[expect(lint, reason = "…")]` for necessary exceptions; the expectation should fail when the
lint no longer fires. Forbid `unsafe_code`; a platform-specific exception requires a narrow,
documented policy change rather than a broad local suppression (`Cargo.toml`,
`[workspace.lints.rust]` and `[workspace.lints.clippy]`). Warn on `missing_docs`.

Tool crates must configure `disallowed-methods`/`disallowed-types` for OS filesystem and process
access outside `aimx::workspace::local`. This makes `Workspace` the only path to local or SSH
effects (`docs/architecture.md`, §§9, 13; `docs/research/infra.md`, Implications A2).

## Consequences

The gate fails on warnings from authored executable code. Proof/spec code inside `verus!` still
needs Verus and verusfmt because Clippy and rustfmt see only erased executable code
(`docs/research/verus.md`, F4 Clippy friction). New crates must inherit the policy; OS-access
rules require crate-local configuration when execution modules arrive.

## Verification

`mise run check` runs `cargo clippy --workspace --all-targets --locked -- -D warnings` now
(`mise.toml`, `[tasks.check]`). M1a must add a check that forbidden OS methods fail in tool
modules while `aimx::workspace::local` is permitted; the live SSH conformance suite then checks
the effect boundary (`docs/architecture.md`, §§9, 15).
