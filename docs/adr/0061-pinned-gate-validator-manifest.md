# ADR 0061: Embed the protected set in the pinned gate validator

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0020
- Scope: The gate's source protection manifest and independent candidate validators; not OS sandbox policy or promotion receipts.

## Context

ADR 0020 requires the gate to inspect the complete candidate diff, dependency closure, disabled
tests and LOCKED decisions after candidate execution. The candidate may edit `xtask` and its own
logs. A validator or policy file read from the candidate would therefore let it grade itself. The
self-improvement research records fabricated successful logs and weakened evaluators
(`docs/research/self-improvement.md`, §1 and §4).

## Decision

Build `aim-gate-validators` into the pinned gate binary. `xtask` calls the same library for its
LOCKED check, but the gate never executes candidate `xtask` as a protected validator. The library
embeds `protected.toml` at build time. The gate may pass another manifest only from its own pinned
configuration, never from either candidate checkout.

The manifest is TOML version 1. `protected` and `sensitive` are arrays of repository-relative
paths. A terminal `/` means that path and its descendants; otherwise a rule names exactly one
path. Paths must contain only normal components. `protected` changes are always rejected.
`sensitive` changes need an exact or subtree entry in `allowed_sensitive`. `allowed_packages`
contains exact `name@version#source~checksum` Cargo.lock identities; packages with no registry
source use `local` and entries without checksums use `none`. The built-in manifest protects the gate
implementation, validator, xtask, benchmark evaluator/baselines, kernel and aimx permission-policy
source, gate policy files and LOCKED manifest. It marks Cargo/mise/toolchain/CI/script
controls as sensitive. Gate-home config, key, ledger, suites and baselines are outside the
candidate sandbox and are never candidate inputs.

The validator walks both final checkout trees after candidate exit, including untracked/generated
files outside `.git` and `target`, and compares regular file bytes and symlink targets. It checks
every changed path, new `build.rs`, Cargo.lock package/source/checksum additions, compiled test
inventories keyed by test-binary target from sandboxed `cargo test -- --list`, newly ignored or
cfg-gated source tests, and the LOCKED
closure digests from the pinned library. Missing/empty test inventories and unreadable files fail
closed. The gate stores the full changed-path list and findings in its evaluation result.

## Consequences

Changing the protected set requires rebuilding and repinning the gate. A candidate can still
add tests, change implementations and proofs, or change dependencies on an explicit gate-owned
allowlist. This validator is one layer: the Seatbelt profile and final `--offline --locked`
compilation remain necessary. Text inspection of test attributes cannot model arbitrary macro
expansion; compiled inventories and independent suites remain necessary too. The gate must capture
Cargo's `Running` target headers and libtest names in one ordered stream; an unscoped inventory
entry fails closed.

## Verification

`aim-gate-validators/tests/validation.rs` uses tiny temporary repositories to reject a changed
LOCKED spec, deleted/ignored/cfg-gated test, new dependency, edited xtask, changed pin and new
build script; it also checks a generated source path is included in the complete diff. `xtask`
uses the extracted checker, so `cargo xtask check` remains the repository quality gate.
