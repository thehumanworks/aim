# ADR 0026: Intersect authority scopes in the verified kernel

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0008, 0021
- Scope: Pure path, operation, deny, and resource-limit decisions for grants and ceilings; not authentication, path resolution, OS sandboxing, or request dispatch.

## Context

Both aim's dispatcher and aimx need the same grant decision (`docs/architecture.md`, §12;
`docs/adr/0008`). A delegated caller must be able to request less authority without increasing
its parent's grant (`docs/adr/0021`). REV4-A finding 4 shows the current aimx grant has no per-call
ceiling, so a writable parent cannot enforce a read-only delegated call at aimx
(`scratchpad/reviews/REV4-A.md`, finding 4).

Paths are normalized byte-segment sequences, using the name predicate and representation of
`crates/aim-kernel/src/path.rs`; the policy constructor additionally rejects literal `.` and `..`
segments, which cannot be used as normalized path components. The host must still use
`path::confined` and refuse symlink escapes
at the I/O boundary (`docs/adr/0008`; `docs/architecture.md`, §13).

## Decision

A `Scope` has a union of allowed root subtrees, a three-bit operation mask (`Read`, `Write`,
`Exec`), a union of write-denied subtrees, and process/output limits. `Exec` is checked against
the process cwd. A write-deny under a path overrides an allowed root and operation. Scopes
intersect exactly: operations and allowed paths must be permitted by both, deny subtrees are
unioned, and limits take their minimum. When two roots overlap, the deeper root represents their
intersection; disjoint roots contribute no authority.

`narrows(child, parent)` means all child permits are parent permits and both child limits are no
larger. It is decided exactly by executable finite boundary probes at child allowed roots and
parent write-deny roots. The effective grant is the intersection fold of principal, session,
agent/source, and per-call scopes; an empty fold is unrestricted. The kernel's `permits`,
`narrows`, `intersection_ok`, and `effective_ok` specs are marked `LOCKED(ADR-0026)`. The
maintainer records their digests after review.

## Consequences

Every supplied ceiling can only reduce authority. The shell must bind the delegation ceiling to
authenticated session/source state, so a child cannot omit it. The protocol needs an optional typed
per-call scope whose absence means no additional narrowing; an explicitly empty scope means deny.
The receiver intersects that scope with the authenticated principal and bound ceilings before
every RPC or tool action. A per-call scope that claims to be a delegated grant should be checked
with `narrows` against the bound parent, with widening refused.

The kernel decision is conditional on normalized paths. Deleting or moving an ancestor of a
protected subtree requires the host's separate tree-operation guard; checking only the target
path as `Write` would not protect descendants. Likewise, `Exec` authorization at cwd alone does
not confine what a process can access; the OS sandbox and backend remain responsible.

## Verification

`policy::Root::new` verifies normalized segment names; `Root::under` verifies prefix matching.
`common_root` and `intersect_roots` prove the root-product rule. `Scope::intersect` ensures
`intersection_ok`, and `theorem_intersection_narrows` proves the result narrows both operands.
`theorem_deny_overrides_allow` proves deny precedence. `theorem_probes_complete` proves finite
boundary probes decide semantic narrowing; `Scope::narrows` ensures its Boolean equals that spec.
`effective` ensures `effective_ok`, while `theorem_effective_narrows` proves the fold narrows each
operand. `mise run verify` checks these under `--no-cheating`.

`crates/aim-kernel/tests/policy.rs` exercises the erased executable API at the named boundaries.
These proofs establish the pure decision given scopes and normalized paths. They do not establish
identity, token binding, confinement, symlink safety, per-call protocol delivery, or OS effects
(`docs/architecture.md`, §13).
