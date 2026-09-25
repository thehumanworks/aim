# ADR 0040: Keep daemon attachments recoverable under load

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0026, 0031, 0037
- Scope: Local daemon attachment, transcript paging, idle and shutdown lifecycle.

## Context

REV7 reproduced repeated updates on same-connection reattach, a blocked retry after a dropped
attach, lost stop signals, and an irrecoverable 40 MiB transcript. A failed oversized legacy
attach still started a forwarder (REV7-daemon-mcp.md, M1–M6). The 36 MiB JSON-RPC frame bound is
necessary for resource control, but a user-visible transcript keeps growing after model-context
compaction.

## Decision

- A forwarder's enqueue and replacement/detach cancellation share one asynchronous send gate.
  The old update is queued before `replaced`, or cancellation wins before the enqueue.
- A client attachment stages a subscriber under a drop guard. Abandoning an active attach sends
  best-effort `session.detach` before the next attach proceeds; waiting for replacement is bounded.
- Add `session.attach_paged` and `session.transcript` to `aim-daemon/1`. The first method returns a
  session summary, an opaque connection-local snapshot id, the byte length of the immutable
  serialized transcript, and its first bounded base64 chunk. The second returns successive
  bounded byte chunks by offset. The daemon client reassembles the existing
  `SessionAttachResult`, so its `SessionClient` interface remains unchanged. The server drops a
  snapshot after its final chunk, replacement, detach, cancellation, or connection close.
  Legacy `session.attach` remains for snapshots that fit one frame and rejects larger ones
  before it registers a forwarder.
- The session host exposes live actor summaries from memory for daemon idle checks and shutdown.
  The daemon registers SIGTERM and SIGINT once, retries transient accept errors, treats a failed
  idle check as busy, and still runs shutdown if a final listing fails. `aim daemon stop` waits
  for the socket and pid file to disappear or returns a timeout.

## Consequences

Large snapshots use multiple RPCs and temporarily hold serialized bytes in the connection until
transfer completes. Live updates remain buffered behind the snapshot and may end with `lagged`
if the client cannot keep up. A client that only speaks legacy `session.attach` receives
`limit_exceeded` for an oversized snapshot and can upgrade to the paged method.

## Verification

Multi-thread daemon reproductions from REV7 check snapshot/update uniqueness, dropped-attach
retry, 40 MiB attachment and failed-attach forwarding. Daemon lifecycle tests cover idle reads,
signals and store errors; `mise run check` covers protocol and implementation gates.
