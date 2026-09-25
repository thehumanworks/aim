# ADR 0020: Promote self-improvements only through the trusted gate

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0007, 0008, 0019
- Scope: Candidate evaluation and autonomous promotion, including Rust source; not model quality.

## Context

The maintainer chose autonomous, gate-only activation for all agent-mutable surfaces, including
aim's Rust source (`docs/architecture.md`, §10, lines 546–552). A separate DGM experiment found
fabricated passing logs and evaluator-marker removal; poisoned benchmark tasks induced later
vulnerable behavior in other self-improving agents (`docs/research/self-improvement.md`, TL;DR,
lines 14–31; §1, lines 87–101). Candidate logs cannot be the gate's evidence.

## Decision

Run `aim-gate` as a separate trusted service and OS identity, built from a pinned protected
commit. It alone holds receipt-signing and protected-branch push credentials. Candidate agents,
build scripts, proc macros, tests, code cells, and plugins run in Seatbelt/bubblewrap confinement
without gate credentials; gate-brokered live provider smoke keeps secrets outside the sandbox.

Write-deny the protected set in candidate sandboxes and ordinary yolo sessions: gate binary and
configuration, evaluator/suites/hidden cases/baselines, receipt key, evolution ledger, policy
files, every LOCKED kernel spec, and `crates/aim-kernel/LOCKED.toml`. Candidate changes to a
LOCKED spec or manifest are rejected and require the maintainer plus a superseding ADR;
implementations and proofs may change (`docs/architecture.md`, §10.1, lines 554–575).

The gate inspects the full diff and dependency closure, rejects protected changes, deleted or
disabled tests, and unreviewed dependencies. It runs protected validators on final artifacts
after candidate exit, compares paired baseline/candidate repetitions against predeclared safety,
quality, latency, and cost thresholds, uses a fresh holdout once per campaign, and signs a
receipt bound to candidate/evaluator/environment/baseline digests. This is independent
evaluation, not a human approval step (`docs/architecture.md`, §10.2, lines 576–590;
`docs/research/self-improvement.md`, §4, lines 201–260).

Promote `proposed → evaluated(receipt) → merged(base SHA CAS) → pushed(remote ref readback) →
deployed(exact SHA and digests) → canary → active`; record rejected, failed, and rolled-back
exits. Keep a runnable predecessor and forward-compatible database migrations. Externally
anchor the append-only hash-chained ledger through private-repository checkpoints. Rollback
restores a runnable artifact, not external side effects (`docs/architecture.md`, §10.3,
lines 591–605).

## Consequences

Autonomy depends on the gate's OS boundary and independent evaluator, which become high-value
maintenance obligations. A valid kernel proof remains conditional on trusted inputs and shell
behavior; it cannot prove evaluator honesty or sandbox isolation.

## Verification

In M10, add kernel proofs `activation_requires_pinned_receipt`, `ledger_append_only`,
`rollback_target_retained`, and `protected_digest_unchanged`; add sandbox escape, forged-log,
holdout, CAS, deployment readback, and rollback fault-injection tests. Run them from the pinned
gate against final candidate artifacts, never from candidate-controlled logs.
