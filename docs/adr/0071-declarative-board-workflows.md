# ADR 0071: Run declarative workflows through the board ledger

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0018, 0019, 0027, 0030, 0046, 0048
- Scope: Version 1 project workflow manifests, manual runs, durable step scheduling, and local execution; not scheduled or event triggers, code-mode workflows, remote federation, or automatic review of externally worked jobs.

## Context

The architecture defines a versioned `.agents/workflows/<name>/` directory with typed inputs, a
capability ceiling, a budget, a DAG and durable steps (§8.4). ADR 0019 and the swarm research
make the board ledger the authority for job attempts and dependencies. A separate workflow
executor that bypasses the board would duplicate claim and retry decisions (the single-authority
principle also illustrated by tny ADR 0143). ADR 0048 makes accepted review evidence, rather than
mere worker completion, the predecessor gate for a board job. ADRs 0027 and 0046 require narrowing
authority at the harness, including calls made by a model selected as a workflow step.

## Decision

`workflow.toml` version `1` has a directory-safe name, `trigger = "manual"`, JSON-encoded JSON
Schema strings for `params` and `results`, a `CallScope` ceiling, `budget` (`max_steps`,
`max_tokens`, `timeout_seconds`), and bounded `[[steps]]`. Every step has an ID, kind, dependency
IDs, retry count, and optional timeout. `tool` has one dispatcher tool name and JSON arguments;
`agent` has one named agent and prompt, with optional provider/model; `job` has a board title and
description. Only `${params.name}` and `${steps.id.result}` interpolation is allowed. A result
reference requires a transitive dependency. The supported JSON Schema subset has explicit types,
object properties/required/additionalProperties, array items, and enum. Unknown schema keywords
fail closed. The run result is the final step result and must satisfy `results`.

The daemon posts each ready step as a board job in the run's board namespace. Board dependencies
are the corresponding predecessor job IDs. A `job` step waits for an external worker's accepted
review; the workflow does not claim or review it. For a `tool` or `agent` step, the workflow worker
holds a fenced board claim. It records a private claim token before claiming, records intent
before the first effect, performs the step through the dispatcher, stores result evidence as a
content-hashed artifact, confirms cleanup, and accepts that evidence under a distinct controller
review identity. Thus the board decides admission, capacity, retries and dependency release for
all three kinds. The workflow table is a durable projection of manifest parameters, rendered
step results, and external-call intent; it cannot override board state.

The daemon stores each run and step in additive `workflow_runs` and `workflow_steps` tables in
`aim.db` under the shared migration lock, with WAL and full synchronization. IDs and attempt keys
are stable. The exact trusted manifest text and digest are snapshotted at run creation, so an edit
cannot silently change a run after restart. A completed step is never dispatched again. On
restart, an in-flight step is reconciled against the board's claim and immutable artifact. A
completed and accepted attempt can be projected into the workflow table. If an effect was in
flight and its outcome cannot be established, the controller fails it closed; it never blindly
replays a call whose deduplication window may have expired. A new attempt requires board cleanup
confirmation and the declared retry budget. Cancellation is persisted before any external stop
and propagates to dependent steps.

A project manifest is read through the harness and needs an explicit private trust grant keyed
by canonical workspace source path plus the full manifest SHA-256. Editing it loses trust for new
runs. Every tool call carries the workflow ceiling as the harness call scope; named-agent tool
policy narrows the available tools further through the verified policy intersection. Agent
project resource reads also carry the ceiling. No model gets a board claim token. The run budget
and per-step timeout cap work, with exceeded budgets causing cancellation/failure rather than a
new unbounded attempt.

The CLI offers `workflow list`, `run`, `status`, `cancel`, `trust`, and `untrust`. `run` validates
and queues a durable run, and the daemon resumes active runs on startup. Future triggers may
enqueue the same versioned manifest contract but are not accepted in version 1.

## Consequences

The board's independent review rule adds an artifact and cleanup transition even for small tool
calls. A job step can wait indefinitely for an external worker until the run timeout or user
cancellation. Workflow metadata and board state share SQLite but commit separately, so recovery
must inspect both and use stable idempotency keys. No power-loss or distributed exactly-once
claim is made for external effects; uncertain outcomes stop until reconciled.

## Verification

`aim_kernel::workflow` locks DAG validity, dependency readiness, bounded retries and cancellation
with `LOCKED(ADR-0071)` specs and Verus proofs. Parser and template tests cover malformed DAGs,
bounds and references. Store and runner tests cover reopen and daemon restart without rerunning a
finished step. A real three-step workflow exercises tool write, an OpenRouter agent edit, and
tool verification in a temporary Git repository. Exact proof counts, mutation results and live
evidence are recorded in the W33 handoff report.
