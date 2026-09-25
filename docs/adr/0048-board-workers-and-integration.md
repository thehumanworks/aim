# ADR 0048: Run fenced board attempts in isolated worktrees

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0019, 0030
- Scope: Local and SSH worker attempts, their evidence and Git integration; not remote agent federation or webhook dispatch.

## Context

ADR 0019 makes the ledger the sole authority for attempts and requires launch intent before
external effects. ADR 0030 gives the board a typed local contract, but a posted job does not yet
say how its workspace is reached, checked, or cleaned after failure. The swarm research calls for
per-attempt branches, a pinned base, explicit cleanup ownership, independent review, and a
serialized integration that preserves conflicts (`docs/research/swarm.md`, §7, "Crash and retry
cases"). A lease expiry alone does not prove that an agent or Git process stopped.

The REV11 board review found four contract failures before the kernel decision is locked:
admission trusted caller-computed predicates, completion released capacity before process cleanup,
a cancelled claim reply could lose its one-time token, and retry accepted a caller's bare cleanup
boolean. It also identified mutable caller-declared capacity, missing independent-review checks,
and snapshot and watch gaps (REV11 board review, with executable regressions in this change).

## Decision

Add optional `JobSpec.work` policy: a workspace location, target branch, check command, and
failure-worktree disposition. Older board-only jobs omit it. A worker claims through the board,
generates two version-4 UUIDs (244 random bits) with the OS-backed generator, fsyncs its private
0600 token and ownership receipt before the claim request, and sends only its SHA-256 hash, a
client-generated attempt ID, and a stable replay key. A repeated claim with matching identity
returns the same attempt. The ledger stores only the hash. It uses WAL `synchronous=FULL`; committed
outbox hints are emitted by the database actor in commit order. The runner then uses aimx `exec.spawn`
and `exec.read` to create and inspect a Git worktree. The worker's session is created through
`SessionClient` with that worktree in `SessionSpec.workspace`; `SessionSpec.agent` selects a named
agent. The model never receives the claim token. The runner heartbeats while the session works,
closes it before stopping or cleaning the worktree, and records a cleanup receipt. `Complete`
records `Succeeded/Pending`; only the separate holder-token cleanup receipt confirms stopped
effects and releases capacity. Review waits for that confirmation. `Fail`, `Cancel`, and `Expire`
retain a pending hold until cleanup is recorded. `board.expire` records a past lease without a
cleanup assertion; `Retry` has no cleanup boolean. A cancelled job may retry after confirmed
cleanup, within its existing retry budget. If a crash
leaves uncertain ownership, the ledger retains its capacity hold until reconciliation can prove
the prior process is gone.

The kernel's public claim operation reads restored dependency jobs and all jobs from one ledger
write transaction, derives accepted evidence and held capacity itself, and applies admission with
the claim. A separate transition method refuses an unadmitted Claim. The service restores reviewed
evidence from the current attempt and checks artifact hashes. A worker's fixed capacity is
registered in the ledger before its first claim and cannot be changed by a later claim parameter.
The daemon binds a claiming connection to one worker name and refuses owner operations on that
connection; only the runner and local CLI construct the trusted in-process board path. The daemon
still trusts same-UID local controller connections for owner operations, so a hostile process with
the user's UID is outside this boundary. A worker cannot review its own attempt even through a
controller connection using the same reviewer name.

Each successful attempt commits on `board/<job>/<attempt>` and records branch, commit, diff and
check evidence as immutable board artifacts. Execution success remains separate from review.
Accepted attempts may enter a serialized integration for the configured target branch. The
integrator records the target HEAD before merging, preserves conflicting files without resolving
them, checks the resulting tree, and appends an immutable integration result artifact and ledger
event. It prefers a dedicated target worktree when the branch is free; when the branch is already
checked out, it uses that checkout only if clean and on the target branch. Automatic integration
observes accepted reviews and uses the same serialized path.

`board.list` and the snapshots in `board.poll`/`board.watch` use a stable `after_job` cursor,
`next_job`, `truncated`, and an 8 MiB snapshot budget. Total raw artifacts in one completion are
capped at 24 MiB to fit the daemon frame after base64 encoding. Watch notifications are hints:
the daemon and CLI reconcile from the outbox cursor until caught up, including after a lag.

## Consequences

The separate board and session actors still share SQLite WAL; Git and provider calls occur outside
transactions. A worktree isolates source edits, but does not itself isolate credentials or prove
cleanup. A kept failed worktree is inspectable and its branch remains fenced to its attempt.
Command output is bounded before it becomes an artifact. SSH workspaces require an aimx transport
to run every Git operation on the remote workspace host.

## Verification

W17 tests exercise a two-job dependent run in a temporary Git repository, review and serialized
integration, a conflict, and restart reconciliation. Ledger regressions cover replay, registered
capacity, review independence, cleanup fencing, and 250-job pagination. The Verus
`theorem_bounded_lifecycle` bounds successful claims across an arbitrary valid trace by the retry
budget plus one. The live Codex smoke ran one real provider through the same worker path; timings
and provider-reported usage are in the W17 scratchpad report. The smoke is evidence for this one
local provider/workspace configuration, not a general SSH or power-loss claim.
