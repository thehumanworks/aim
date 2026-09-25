# ADR 0068: Reconcile board integrations from pinned Git observations

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0019, 0048, 0055
- Scope: Local and SSH board integration intent, recovery, rescue refs, and explicit target application; not arbitrary same-UID Git ref mutation outside aim.

## Context

REV16 found that the integrator compared an accepted source OID but merged a mutable branch name. It could then move a target ref before recording the result, strand an `integrating` row on crash, or remove the detached scratch that alone retained a failed merge commit. Reading a worktree list and checking a clean status could not stop another user operation between observation and branch movement. The board ledger remains the authority for outcomes under ADR 0019; Git supplies observations and performs effects.

## Decision

Persist target and accepted source OIDs and an owned scratch path in one `integrating` transaction before creating the worktree. Merge the accepted source **OID** in a detached scratch; compare the branch OID separately as a stale-evidence check. After a passing merge/check, persist the result OID and deterministic `refs/aim/rescue/<job>` name before creating or moving any ref. Create that rescue ref before removing scratch. A separately versioned integration migration adds nullable `scratch_path`, `result_commit`, and `rescue_ref` columns while retaining board schema version 1 for prior-binary reads.

On each integrator entry, reconcile recorded `integrating` intents against observed target, source, result and scratch OIDs. If a crash left the merge commit in scratch before the ledger saved its OID, recover it from the two pinned parents and the integrator's job-specific commit marker before deciding from mutable refs. An incomplete scratch with no result can be removed and retried only while target and source still match the pins. If target already equals result, record `Integrated`. If target moved elsewhere, record `Failed` with the rescue ref when a result exists. An existing result with target still pinned is advanced only after the detached scratch switches to an **integrator-owned checkout of the target branch**. Ordinary Git refuses that switch when any other worktree already checks out the target. The integrator then fast-forwards only its owned checkout. It never reads a user's checkout and then moves that user's branch automatically. An in-process operation mutex serializes complete integrations for one `Integrator`, in addition to the cross-process lock.

If the target is checked out by the user, record a Failed outcome with `needs apply: git merge --ff-only refs/aim/rescue/<job>`. `aim board show` displays that exact command. At the user's explicit request, `aim board apply [<job>|--all]` runs the fast-forward inside the configured user checkout, verifies the observed target equals the persisted result, records `Integrated`, and removes the rescue ref. `aim board discard-rescue <job>` explicitly disposes a retained result ref. Git's `--ff-only` behavior decides whether the user's local state permits the requested operation. A moved target always has a recorded terminal or recoverable outcome.

An owned scratch guard schedules cleanup on cancellation and unwind. Restart reconciliation prunes recorded scratch paths after ensuring any checked result has a rescue ref. It also finishes rescue cleanup after an integrated result. The Verus `integration` module decides legal transitions and reconciliation from persisted intent and Git observations; exact Git bytes, checkout ownership and filesystem effects remain shell inputs.

## Consequences

Automatic advancement works only when the integrator can own the target branch checkout. The common case of a user already on `main` requires the explicit apply command. A failed or cancelled integration retains a checked result through an owned rescue ref until it is applied or disposed. The additive schema is reasoned to remain readable by the previous binary; old `integrating` rows without scratch/result fields are reconciled conservatively. A hostile same-UID process that deliberately overrides Git's worktree ownership or mutates refs outside aim is beyond this lock's authority.

The explicit apply command has ordinary Git checkout concurrency semantics: the user must not switch that checkout to another branch while the command runs. Its postcheck keeps the ledger from recording a false `Integrated` outcome if the checkout branch changes, but Git may already have fast-forwarded the new branch. This does not affect automatic integration, which requires an owned checkout. A fenced explicit apply is a follow-up design question recorded in the FIX18 scratchpad.

## Verification

The `integration::next` and `integration::reconcile` specs and proofs cover legal phase changes, moved targets, and retry/finalization gates. Temporary-repository tests inject crashes between intent, result, rescue, and target movement, then restart the integrator. Interleaving tests cover source-branch movement, a newly checked-out target, dirty target, concurrent calls on one `Integrator`, and rescue persistence. The live Codex board worker test exercises an accepted contribution and explicit apply. The FIX18 report records exact gate results and proof mutation evidence.
