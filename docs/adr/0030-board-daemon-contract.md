# ADR 0030: Expose the board ledger through typed daemon methods

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0019
- Scope: Local board methods, snapshots, notification and reconciliation contract on `aim-daemon/1`; not remote federation, HTTP webhooks, or worker execution.

## Context

ADR 0019 makes the job/attempt ledger the sole authority and calls for `post`, `claim`,
`heartbeat`, `message`, `complete`, `fail`, `cancel`, `retry`, `review`, `watch`, and `poll`. The
session store already persists in the daemon's SQLite file (`docs/architecture.md`, §§5.3, 7).
The swarm research distinguishes committed truth from notification delivery: a watch event is a
hint, and a client must reconcile by polling or fetching the current job
(`docs/research/swarm.md`, §§6, API surface, Crash and retry cases).

## Decision

Add typed `board.*` request/result markers and a `board.event` notification to `aim-daemon/1`
without changing existing session methods or generation 1. `board.post` can create a run namespace
or add a job to one. The job contract includes a deliverable, acceptance criteria, explicit
accepted dependencies, retry budget and optional workspace. A claim returns a fenced attempt and
one-time claim token; ordinary snapshots, events and messages never repeat that secret. Attempt
mutations present the attempt ID and token, while job-level mutations use observed job versions.
Execution state and review state remain separate. Job snapshots include content-hashed artifact
summaries so a reviewer can name exact evidence IDs without receiving the raw bytes.

`board.watch` first returns an authoritative snapshot and cursor, then delivers bounded
`board.event` hints on that daemon connection. `board.poll` returns current snapshots and ordered
events after a cursor. Duplicate notifications do not create additional state, and reconnecting
clients reconcile from the ledger. The ledger commits each state change and outbox event in one
transaction; callback/webhook delivery and the A2A projection remain later slices.

Board calls use the authenticated daemon connection. The service must validate caller authority
and use its own clock for leases rather than treating a client timestamp as proof of time. The
`now_ms` fields are observations useful for the local/test adapter; they cannot grant extra lease
or authorize a stale token. The exact typed shapes live in `crates/aim-proto/src/board.rs`.

## Consequences

The CLI and in-process service share one contract with daemon clients. Wire evolution remains
additive within generation 1; unknown operation names fail rather than silently receiving
access. Claim tokens require private handling at the CLI and must not enter human output, JSON
summaries, logs, snapshots or event payloads. An empty per-run event page is not proof that the
job did not change; clients use the returned snapshots as authority.

## Verification

`crates/aim-proto/tests/board_contract.rs` asserts unique method names, JSON schemas for every
method's params and result, the `board.event` notification schema, and stable round trips for a
job contract and committed event. The W14 ledger and daemon tests exercise the transaction,
watch/poll and claim-token behavior. These tests do not prove external worker or webhook effects.
