# ADR 0037: Bound ordered RPC notifications without blocking control frames

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0031
- Scope: `aim-rpc` notification delivery on one peer connection. Amends ADR 0031's notification backpressure decision; the rest of ADR 0031 is unchanged.

## Context

ADR 0031 made the connection reader wait as soon as its ordered notification queue filled. This preserved `exec.output` bursts that had been cut off by the earlier close-on-overflow policy, but it also stopped the only reader from parsing a response or `$/cancel` behind the full queue. REV6 finding 4 describes the resulting same-peer callback deadlock and delayed cancellation. The capacity-one reproductions in `crates/aim-rpc/tests/peer.rs` failed before this change.

## Decision

The notification handler still receives every admitted notification in wire order. When its queue fills, the reader stores newer notifications in a FIFO backlog of up to three times the configured queue capacity. No new notification enters the handler queue ahead of an older backlog entry. While the backlog has room, the reader continues to dispatch responses, requests and `$/cancel` immediately.

The queue plus backlog hold about four times `PeerConfig::notification_queue_capacity`; capacity zero continues to reject the first notification. Only when the backlog is also full does the reader wait for the handler queue to drain. That wait is bounded by the existing 30-second `WRITE_TIMEOUT`. If it expires, the peer warns with `limit_exceeded` and closes the connection. A handler may call back through `NotificationCtx::peer`, but a sender that continuously floods beyond the bound can still close that connection.

## Consequences

Finite notification bursts no longer block same-peer callback responses, inbound requests or cancellation. Delivery remains ordered and lossless for connections that stay within the bound. Memory use per connection is bounded; a sustained sender outrunning a stuck handler receives a bounded close rather than an indefinite read stall. The daemon's own per-stream `lagged` policy remains separate (ADR 0026).

## Verification

- `crates/aim-rpc/tests/peer.rs::notification_handler_can_call_back_with_more_notifications_ahead_of_reply` and `cancellation_behind_a_full_notification_queue_is_prompt` failed on the ADR 0031 implementation and pass with the backlog.
- `notification_flood_beyond_the_backlog_closes_after_the_bound` uses a paused Tokio clock and checks the `limit_exceeded` warning. Existing backpressure and wire-order tests remain green.
- `crates/aimx/tests/conformance/exec.rs::ring_buffer_reports_dropped_output` and `crates/aim/tests/daemon.rs::reattach_while_streaming_preserves_finished_items_once` exercise the affected integration paths under repeated runs.
