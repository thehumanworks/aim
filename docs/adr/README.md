# Architecture decision records

Numbered and append-only. A decision that changes an earlier one gets a **new** ADR that names what
it supersedes (`Supersedes: 0007`); the old file stays, and its status line gains
`Superseded by NNNN`. Code, proofs and docs reference decisions as `docs/adr/NNNN`.

Rules (enforced by `mise run check`):

- File name: `NNNN-kebab-slug.md`. **Numbers are unique** — tny accumulated eleven duplicated
  numbers, and tooling that indexes by number alone then silently drops decisions. The check fails
  on any duplicate.
- Header block, in this order: `Status` (Proposed | Accepted | Superseded by NNNN | Rejected),
  `Date` (ISO), optional `Supersedes`, optional `Baseline` (ADRs this one builds on), `Scope`.
- Sections: `Context`, `Decision`, `Consequences`. Add `Verification` when a Verus proof or a live
  smoke test locks the decision — name the proof function or test so the ADR and the evidence
  point at each other.
- Evidence beats assertion: cite research reports, live probes, benchmarks or proofs.
- Agents write ADRs. Any change that alters an architectural contract (a protocol, a public trait,
  a persisted format, a policy default, a verified invariant) lands with its ADR in the same
  commit.

| ADR | Decision |
| --- | --- |
