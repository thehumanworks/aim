# ADR 0060: Verify gate promotion before reporting activation

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0020, 0021
- Scope: Promotion state and its verified invariants; cryptographic, OS, and persistence adapters remain in `aim-gate`.

## Context

ADR 0020 and architecture §10 require autonomous gate-only promotion of candidate Rust code and
other mutable surfaces. An agent can control its own diff and logs, so an independent evaluator
must decide whether an exact candidate may move to a protected ref and deployment. The DGM safety
experiment documented fabricated passing logs and evaluator-marker removal; the research survey
also found benchmark-poisoning attacks (`docs/research/self-improvement.md`, TL;DR and §1).

This first slice runs the gate under the maintainer's **same UID**, outside each Seatbelt candidate
sandbox. `~/.aim-gate/` is mode 0700; its ed25519 receipt key is mode 0600. The candidate has no
read access to that home, `~/.codex`, `~/.aim`, `~/.ssh`, or `~/.config`, and can write only its
fresh clone and target. Builds, proc macros, tests, xtask and code cells are candidate code and
remain inside the sandbox. Offline toolchain caches are read-only; the gate fetches them before
launch. The gate owns protected validators, evaluator baselines and receipt signing. A separate OS
user would provide stronger isolation but would require sudo and is deferred.

## Decision

The kernel models the following ordered promotion:

`proposed → evaluated(receipt) → merged(base CAS) → pushed(remote readback) → deployed(exact SHA
and digests) → canary → active`.

`rejected`, `failed(step)`, and `rolled_back` are explicit exits. The checked transition takes
trusted observations: signature validity; equality of receipt candidate/tree/baseline and pinned
evaluator digests; passing independent evaluation; base compare-and-swap; resulting tree;
remote-ref readback; deployment SHA and binary/config/schema digest readback; passing canary; and
retained runnable predecessor. A mismatch refuses the transition. The gate shell must derive
those observations from bytes and independent readback, never candidate assertions. The executable
kernel entrypoint `gate::advance` refines the `gate::next` spec.

Every transition also receives the observed authority ceiling and protected-set digest. The
ceiling may only lose bits; the digest must remain equal. A proposal requires an already runnable
rollback artifact, and no accepted step drops it. The previous binary must remain compatible with
the store schema; migrations remain expand/contract. Rollback restores the predecessor pointer
and requires predecessor-pointer readback; it does not claim to reverse external effects.

The state also retains a nonzero abstract rollback identity across every step. The shell maps it
to the full predecessor SHA, checks that exact SHA still names a runnable artifact before deploy,
and compares the rollback pointer against that SHA on readback.

The kernel's `u64` protected-digest and rollback-identity fields are **abstract equality tokens**,
not truncated hashes.
The trusted shell compares the complete SHA-256 digests first and supplies an equal token only
after full-byte equality. Mapping raw digests to 64 bits would introduce a collision route that
the proof does not cover. The same trusted-byte-comparison boundary supplies the evaluator,
candidate, tree, baseline, remote-ref and deployed-digest booleans.

The kernel models ledger append as an exact sequence extension. The gate shell persists a
hash-chained append-only ledger and anchors checkpoints by pushing `gate/ledger` to the private
origin. Sequence hashing, durable transactions and external anchoring are I/O guarantees outside
the pure proof. The gate cannot report `active` until readback of each preceding step matched.
Candidate mutation of LOCKED specs, the gate, evaluator suites, config, policy and other protected
inputs is refused by independent validators from the gate's pinned build.

The specs in `crates/aim-kernel/src/gate.rs` are marked `LOCKED(ADR-0060)`. Their digests enter
`LOCKED.toml` only through the maintainer's protected lock procedure at merge. Agents do not edit
the manifest.

## Consequences

The runtime must construct observations from verified cryptographic and filesystem facts. The
kernel proves the decision over those observations, not ed25519 correctness, SHA-256 collision
resistance, Seatbelt confinement, remote availability, or disk crash durability. Same-UID gate
isolation remains a risk; a dedicated user is the follow-up boundary. Initial promotion targets
`gate/trial`, leaving `main` unchanged.

## Verification

`gate::activation_requires_pinned_receipt`, `gate::ledger_append_only`,
`gate::rollback_target_retained`, `gate::authority_never_widens`, and
`gate::protected_digest_unchanged` are proved by `mise run verify --no-cheating`. The gate crate's
temporary-repo and sandbox tests check the trusted adapters and their failure paths.
