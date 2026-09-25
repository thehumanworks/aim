# ADR 0019: Make the job ledger the blackboard authority

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0007, 0008
- Scope: Swarm jobs, attempts, messages, artifacts, and notifications; not agent model quality.

## Context

Swarm messages and webhook delivery cannot themselves decide whether work started, succeeded,
or was accepted. The research rejects exactly-once assumptions, implicit merge, and treating a
lease timeout as proof that a worker stopped (`docs/research/swarm.md`, TL;DR, lines 10–16;
§7, lines 113–116). The existing `aim-kernel::job::next` is explicitly DRAFT(swarm): it models
single-job states and bounded retries, but has no attempt generations or claim tokens yet.

## Decision

Use the SQLite job/attempt ledger as the sole execution authority. `Run` owns a namespace;
`Job` states a deliverable, acceptance criteria, dependencies, budget, and workspace policy;
`Attempt` has a generation, fenced claim token, lease, heartbeat, and cleanup state. Bounded
typed `Message`s, immutable hashed `Artifact`s, separate `Review`s, and
`Subscription`/`Outbox` records complete the model. Board views, A2A tasks, and notifications
are projections (`docs/architecture.md`, §7, lines 421–446;
`docs/research/swarm.md`, §Proposed internal blackboard model, lines 137–158).

Expose `post`, `assign`, `claim`, `heartbeat`, `message`, `complete`, `fail`, `cancel`, `retry`,
`watch`, `poll`, `subscribe`, `review`, and `integrate` through one service. Commit state and
outbox event together; use polling as the reconciliation read. Record process launch intent
before external effects, then reconcile ownership. A stale claim token cannot mutate a newer
attempt (`docs/research/swarm.md`, §API surface, lines 214–234).

Eligibility, permissions, capacity, and dependencies are deterministic gates; Jev may rank
only eligible workers. Editing attempts receive individually owned git worktrees by default.
Serialized integration preserves conflicts and requires explicit review acceptance. Start with
in-process/unix watch; then signed HTTP webhooks using Standard Webhooks and an outbox; then
A2A federation (`docs/research/swarm.md`, §§6–7, lines 83–116).

## Consequences

One DB actor and a durable ledger simplify recovery but demand explicit crash reconciliation.
A completed worker is not an accepted or merged contribution; notification failure does not
change job truth.

## Verification

Extend DRAFT `crates/aim-kernel/src/job.rs::next` in M5 with attempt-generation and claim-token
specs; retain `lemma_only_holder_progresses` and `theorem_bounded_lifecycle` where applicable.
Add M5 crash/replay and concurrent-claim tests for stale tokens, uncertain cleanup, transactional
outbox, and serialized integration. Current DRAFT proofs do not prove those future properties.
