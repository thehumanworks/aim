# ADR 0065: LOCKED digests resolve kernel names by module

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0048
- Scope: how `cargo xtask check` and `mise run locked:update` compute the digest of a LOCKED
  kernel decision. This ADR changes no decision and no spec text.

## Context

ADR 0005 locks each final decision by recording a digest over the spec and the transitive
closure of kernel items it references. The scanner in `xtask/src/locked.rs` keyed every item by
its **bare name** across all kernel modules. The kernel now declares six names in more than one
module (`Event`, `Phase`, `inv`, `next`, `view`, `wf`). The last module scanned in path order won
each name, which caused two defects:

- **A digest could cover the wrong item.** A `job` spec calling `next` or using `Event` was
  fingerprinted against `turn::next` or `turn::Event`. So an edit to `job.rs` could change a locked
  decision without tripping the check.
- **A LOCKED spec could go unrecorded.** `job::wf` has been marked `LOCKED(ADR-0048)` since
  `357ad43`. The scanner attributed `wf` to `turn` (which does not lock it), so `job::wf` never
  reached `LOCKED.toml`. Only five of ADR 0048's six job specs were actually locked.

The FIX15 worker found the problem when a newly marked `job::next` failed to appear among the
unrecorded LOCKED specs. It proposed renaming the spec instead. The root cause is in the checker,
so this ADR fixes the checker.

## Decision

- Kernel items are keyed `module::name`. A module may declare a name more than once (methods of
  different types), and a reference to that name covers every such declaration.
- Inside module `m`, an identifier resolves in this order:
  1. an explicit `module::name` path, exactly;
  2. `m`'s own declaration;
  3. the module it is imported from, via `use crate::module::{…}`;
  4. every kernel declaration of that name. This covers glob imports and anything else the
     scanner cannot place. An unplaceable reference widens a digest and never narrows it.
- A LOCKED spec whose name is declared more than once in its own module is refused: its key
  would be ambiguous.
- The digest hashes each item's kind, `module::name` key and normalized text. Every recorded
  digest therefore changed once, with the algorithm. `LOCKED.toml` was re-recorded under this ADR.
  - The set of `(decision, ADR)` pairs is unchanged, except that `job::wf` (ADR 0048) is now
    recorded.
  - The kernel source is byte-identical to `263efb6`, whose gate passed under the old checker.

## Consequences

- Same-named items in other modules no longer mask edits to a locked decision's closure.
- A wide, conservative resolution can flag a locked decision when an unrelated same-named item
  changes (only for names reached through a glob import). The remedy is an explicit import or
  path, not a manifest update.
- The gate's own copy of this checker (ADR 0020, W28) must carry this resolution.

## Verification

- Unit tests in `xtask/src/locked.rs` cover four cases:
  - a same-named item in another module stays out of the digest, while the spec's own helper
    and type are in it;
  - imports and `crate::module::name` paths pick the named module;
  - a glob-imported name widens the digest;
  - a LOCKED name declared twice in its module is refused.
- `mise run check` passes with the re-recorded manifest.
