# ADR 0003: Pin tooling through mise

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0002
- Scope: Developer and CI toolchain selection; not runtime dependency approval.

## Context

The maintainer chose mise as the tool installer (`docs/vision.md`, Clarifications). The Verus
release pins one Rust compiler, and `cargo verus` builds the verified dependency closure through
that compiler (`docs/research/verus.md`, F2 Rust toolchain coupling). A free-floating Rust update
could therefore break verification even when ordinary Cargo still builds.

The Verus installation probe found that mise's `github:` backend accepts exact release tags,
that `mise.lock` records and checks asset sha256 digests, and that rustfmt leaves `verus!` bodies
unformatted (`docs/research/verus.md`, F1 Installing Verus with mise and F4 The toy workspace).

## Decision

Declare tools and tasks in `mise.toml`. Commit `mise.lock` with sha256 digests for supported
platform assets. Pin the stable Verus release and the Rust version named by that release's
`rust-toolchain.toml`; bump Rust, Verus and the exact `vstd` version together in one change.
Use Verus's `cargo-verus` and `verusfmt` for the kernel (`mise.toml`, `[tools]`,
`[tasks.verify]`, `[tasks.fmt]`).

Run ordinary formatting, Clippy, tests and repository invariants through `mise run check`.
Run kernel proof verification through `mise run verify`. Clean the kernel before verification
because changing Verus flags alone does not invalidate Cargo's cached verification result
(`docs/research/verus.md`, F4 Gates and traps).

Do not add a project `rust-toolchain.toml`: mise already selects Rust in activated shells,
including `RUSTUP_TOOLCHAIN` (`docs/research/verus.md`, Implications for aim, Toolchain strategy).
Keep the kernel's dependency closure small enough for Verus's pinned compiler.

## Consequences

A checkout selects reproducible tools and verifies downloaded release assets. Tool upgrades are
coordinated changes rather than incidental local state. Platforms without a prebuilt Verus
asset require a supported verification runner; the research found no Linux arm64 release asset
for the pinned version (`docs/research/verus.md`, F1 Installing Verus with mise).

## Verification

`mise run check` and `mise run verify` are the current gates (`mise.toml`, `[tasks.check]` and
`[tasks.verify]`). The research exercised both the mise checksum rejection and a verified toy
kernel under the matching Rust toolchain (`docs/research/verus.md`, F1 and F4). CI must install
from the committed lockfile before running those tasks.
