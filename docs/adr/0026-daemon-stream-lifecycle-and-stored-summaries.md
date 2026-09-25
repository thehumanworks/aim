# ADR 0026: Order daemon attachments and report stream termination

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0005, 0006, 0007
- Scope: Local daemon attachment delivery and stored session listings; not durable prompt deduplication or remote transports.

## Context

`session.attach` returns a transcript snapshot followed by live `session.update` notifications. The session host takes the snapshot and subscribes under one lock (`crates/aim/src/host.rs`), but `aim-rpc` enqueues a handler's response only after that handler returns. Starting the daemon forwarder inside the handler could put an update ahead of the attach response. W06 used a private `session.ready` notification to avoid that ordering race; it was absent from the `aim-daemon/1` contract (`crates/aim-proto/src/daemon.rs`; W06 handoff report).

A host broadcast ends its `UpdateStream` when a consumer lags or the session closes (`crates/aim/src/host.rs::updates_of`). Without a terminal wire message, a remote client's stream could wait forever. W06 also projected non-live stored sessions with zero turns and creation time as last activity, regardless of their event logs (`crates/aim/src/host.rs::list`).

## Decision

`aim-rpc::RequestCtx::after_reply` registers synchronous work that runs after the request response is accepted by the FIFO writer queue. The daemon starts attachment forwarding from that hook, so updates follow the attach response on the wire. Hooks are discarded if a reply cannot be queued. The private `session.ready` step is removed.

`aim-daemon/1` adds `session.detached { session, reason }`, with `lagged`, `closed`, and `replaced` reasons. The server sends it after the last update of the affected attachment. A replacement's detached notification is enqueued before the new attach response; a client stages the new stream until it processes that notification. In-process host streams continue to end on lag or closure; daemon clients also learn the reason. A client-local bounded queue ends only that attachment on overflow and records `lagged` without blocking the RPC reader.

`SessionStore::summarize(limit)` returns durable session metadata, maximum turn number, last effective event time, and `Closed` state. SQLite computes summaries, including fork prefixes, in one query. `SessionHost::list` merges those projections with live actor summaries, then filters and sorts the result. Stored sessions are `Closed` because no actor is running until resumed.

## Consequences

Raw daemon clients need no private readiness message. Stream termination has an explicit wire reason. The SQLite summary query must stay consistent with fork materialization and event semantics. A `SessionClient` update stream still has no error item; direct daemon clients can inspect the last detached reason.

Prompt idempotency remains process-local in this change. A restart-safe idempotency record requires a durable event or table and explicit handling of interrupted, unknown outcomes; it is a separate decision.

## Verification

`crates/aim-rpc/tests/peer.rs` exercises response-before-hook-notification ordering with 128 requests and checks cancellation, errors, disconnect, and backpressure. `crates/aim/tests/daemon.rs` exercises attach ordering under streaming load, stream termination reasons, re-attachment, and a real Codex turn. `crates/aim/tests/store.rs` compares SQLite and memory summaries for several sessions and fork prefixes; `crates/aim/tests/host.rs` checks the stored listing projection. `mise run check` covers the combined implementation.
