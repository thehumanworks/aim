# ADR 0055: Bind board cleanup to an attempt and narrow worker authority

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0019, 0048, 0050
- Scope: Board attempt confirmation, admission representation, claim performance, ledger compatibility, worker session authority, and timestamped dedup keys; not an OS sandbox for arbitrary same-UID processes.

## Context

The REV11b board verification found that cleanup confirmation in `job::next` lacked an attempt identity, and that `board::may_claim` depended on an executable `Job` view outside the locked specification closure. It also measured a claim taking 11.8 seconds with 100 accepted jobs and 4 MiB evidence each, while the shared SQLite store has a five-second busy timeout. An early cleanup receipt could strand a worker slot. A worker agent with shell tools could act as a same-UID controller, reading receipts or reviewing its own work. FIX15 and the lead's N4 decision in `scratchpad/questions/kernel.md` set the boundary to implement now.

## Decision

`Event::ConfirmCleanup` names `{ generation, claim_id }`. The kernel accepts it only for the current or last matching attempt in a terminal state with pending cleanup, regardless of lease expiry. `job::next` is locked under ADR 0048. `board::may_claim` and its counting helper consume plain `Seq<JobView>`; executable admission maps each supplied job to that view, and the spec is locked under ADR 0048. Executable lifecycle and admission contracts state both the success condition and resulting state.

Claims restore only the candidate's dependencies and jobs with worker capacity holds. Accepted evidence is hash-verified at review; immutable artifact rows and their persisted identifiers support later restoration without rehashing every unrelated artifact. A repeated identical cleanup receipt may confirm a later terminal attempt, and completion consults an already recorded receipt. Reviewer and worker names are compared after case folding.

The board actor has a bounded queue and checks optimistic update counts. A versioned board schema migrates legacy tables additively and retains columns read by the previous binary during expand/contract. Duplicate dependency IDs are rejected before persistence.

The runner creates a private, immutable file-tool-only agent definition for each worker. Its workspace is confined to the attempt worktree and aimx protects `AIM_HOME`. The runner executes the configured check and feeds bounded failure output into a subsequent worker turn. Worker model tools cannot invoke shell, code, programs, media, or daemon operations. This is a temporary capability reduction: shell returns only after `exec.spawn` runs under the baseline OS sandbox in architecture §10.1 (Seatbelt or bubblewrap, denying `AIM_HOME`, credentials, and the daemon socket), reusing W28's Seatbelt profile work. Arbitrary same-UID processes outside aim remain outside this worker-session boundary. Review uses independently verified ledger artifacts, not worker-writable state outside the worktree.

The dedup kernel decides whether a parsed UUIDv7 mint time is older than the retention horizon. The shell still parses the key and supplies the clock and horizon; an opaque key retains the documented post-horizon reuse limit.

## Consequences

Stale cleanup receipts cannot release a newer attempt, and claims no longer scale with unrelated accepted evidence. Worker agents temporarily lose the ability to run their own commands; the runner supplies check feedback. Ledger migrations must remain readable by the previous binary through the expand phase. The file-tool boundary relies on aimx enforcing its workspace and protected-path grants; it is not a general same-UID OS isolation mechanism.

## Verification

`job::lemma_stale_attempt_fenced`, `board::theorem_admission_respects_capacity`, and the `job::next`/`board::may_claim` executable contracts protect the kernel changes. FIX15 ledger regressions cover early cleanup receipts, duplicate dependencies, migration, claim scale, and concurrent session writes. A live board worker run and a worker tool-surface denial test cover the runner boundary. The FIX15 report records exact gate outcomes and limitations.
