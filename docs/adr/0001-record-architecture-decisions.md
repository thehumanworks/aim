# ADR 0001: Record architecture decisions

- Status: Accepted
- Date: 2026-09-25
- Scope: Decision records and their repository checks; implementation details live with the code.

## Context

The brief expects a harness whose agents can revise its contracts and source, while proofs document
decisions that must stay locked (`docs/vision.md`, Requirements and Ideas and goals). Untracked
contract changes would leave agents to infer policy from code. The process already described in
`docs/adr/README.md`, Rules, makes evidence and numbering part of the repository contract.

The maintainer's tny history had duplicate ADR numbers that made number-based indexing silently
drop decisions (`docs/adr/README.md`, Rules). An overwrite-in-place process would also erase the
reason for an earlier choice, especially when the self-improvement gate later evaluates changes.

## Decision

Keep numbered `NNNN-kebab-slug.md` ADRs in `docs/adr/`. Numbers are unique and never reused.
`cargo xtask check` rejects duplicate numbers and malformed names or headers. Record the status,
ISO date, optional supersession and baseline, scope, context, decision and consequences using
`docs/adr/TEMPLATE.md`.

ADRs are append-only as decisions: changing an accepted decision needs a new ADR naming the old
one in `Supersedes: NNNN`; retain the prior file and mark it `Superseded by NNNN`. Code, proofs and
documentation identify a decision by `docs/adr/NNNN` (`docs/adr/README.md`, opening and Rules).

An agent changing an architectural contract—protocol, public trait, persisted format, policy
default or verified invariant—writes the ADR in the same change. The ADR states an actionable
contract and cites source, research, live probe or measured benchmark evidence. A `Verification`
section names any proof, test or benchmark that locks or checks it; planned evidence is labelled
as planned rather than claimed as a pass.

## Consequences

Contract changes become reviewable without reconstructing intent from implementation diffs.
The index and checker need maintenance, and superseded records remain available for history.
Pure decisions may be backed by Verus proofs; I/O claims still need boundary tests
(`docs/architecture.md`, §13 Verification strategy).

## Verification

`xtask/src/adr.rs::check` checks unique numbers, filenames and header shape; `cargo xtask check`
is the repository invariant gate. `mise run check` includes that command (`mise.toml`,
`[tasks.check]`). A proof or smoke test named by a later ADR verifies its particular decision,
not the entire ADR process.
