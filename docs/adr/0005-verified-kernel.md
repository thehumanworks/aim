# ADR 0005: Lock pure decisions in the verified kernel

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0001, 0003
- Scope: Pure decisions and protocol-generation negotiation; not proof of I/O effects.

## Context

The brief prefers proofs to tests by example for decisions (`docs/vision.md`, Clarifications).
The Verus probe verified a small pure kernel, rejected planted logic bugs, and also showed a
public executable function with `requires` can verify while an unverified caller violates its
precondition at runtime (`docs/research/verus.md`, F4 The toy workspace).

`docs/architecture.md`, §§4.1, 13, puts protocol compatibility and other pure decisions in
`aim-kernel` while codecs, providers, filesystems and databases remain in shells.

## Decision

Keep `aim-kernel` a `no_std` leaf crate with `vstd` as its only dependency. Its public executable
APIs are total: no public exec `fn` with `requires`. Use private fields, type invariants and
`Result`/`Option` to reject invalid input; pass time, randomness and I/O observations as values.
Do not use `assume`, `admit`, `external_body` or `assume_specification` in the kernel. Verify with
`--no-cheating` forwarded to roots only, because vstd contains trusted axioms
(`docs/architecture.md`, §13; `mise.toml`, `[tasks.verify]`).

Mark each final decision spec `LOCKED(ADR-NNNN)` and each unfinished one
`DRAFT(milestone)`. `crates/aim-kernel/LOCKED.toml` records a digest over each locked spec's
transitive closure of referenced specs and types; the gate protects the specs and manifest.
Changing a locked decision requires the maintainer and a superseding ADR. Proofs and executable
implementations may change while preserving that decision (`docs/architecture.md`, §§10.1, 13;
`xtask/src/locked.rs`, module contract).

The first locked decision is `crates/aim-kernel/src/negotiate.rs::agreed`: for inclusive
generation ranges, choose the newest shared generation (`Some(min(a.max, b.max))` when the ranges
overlap); return `None` when they are disjoint. Both `/1` handshakes use the total executable
`Generations::negotiate` result, independent of exact binary build (`docs/architecture.md`, §4.1).

## Consequences

Kernel decisions can be mechanically checked against their own specs. Conversions between kernel
types and `aim-proto` wire types need tests; proofs cannot establish authentication, symlink
handling, disk writes, transport delivery or external model behavior (`docs/architecture.md`,
§13 Verification strategy). The manifest must be recorded for the locked negotiation spec.

## Verification

`negotiate.rs::theorem_agreed_is_symmetric` proves initiator independence;
`theorem_agreed_is_newest_common` proves both peers support the selected generation and no newer
common one exists; `theorem_refusal_iff_disjoint` proves refusal exactly for disjoint valid
ranges. `Generations::new` excludes empty ranges and executable `Generations::negotiate` ensures
equality to `agreed` (`crates/aim-kernel/src/negotiate.rs`, spec, proofs and impl).

`mise run verify` cleans and re-verifies the kernel with `--no-cheating`; `cargo xtask check`
checks public API rules and locked digests (`mise.toml`, `[tasks.verify]`; `xtask/src/kernel.rs`,
`xtask/src/locked.rs`). M1-proto must add a handshake conformance test that exercises accepted
overlap and refused disjoint ranges across the wire.
