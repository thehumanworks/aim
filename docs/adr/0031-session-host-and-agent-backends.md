# ADR 0031: Host sessions as actors over pluggable agent backends

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0007, 0012
- Scope: the session host, `SessionClient`, `agent::Backend` and the backend/workspace factories, the `aim-daemon/1` handshake and `aim-rpc` notification backpressure; not daemon stream termination (ADR 0026), compaction (ADRs 0025, 0032) or tool authority (ADR 0027).

## Context

The daemon, the headless CLI and the TUI all drive the same sessions. An early version of `aim run` built its own agent, recorder and renderer (`crates/aim/src/cli.rs` before 5dd0988), so every new backend would have needed wiring three times.

Claude Code through ACP is an agent backend, not a model provider (architecture §6.1; ADR 0012). The native loop's `Agent` could not host it.

The daemon contract had no handshake, although ADR 0005/0006 require generation negotiation on every protocol.

After the review fixes (FIX2), `aim-rpc` closed a connection whose ordered notification queue overflowed. The aimx conformance test `exec::ring_buffer_reports_dropped_output` then failed intermittently on main, because a chatty process's `exec.output` burst cut the harness connection.

## Decision

**Session host.** `aim::host::SessionHost` runs each live session as an actor that alone owns the session's backend and recorder.
- A prompt while idle starts a turn. A prompt during a turn steers it. Steering that arrives as the turn ends is handed back as `steers_returned`.
- `set_config` applies at once when idle, and from the next turn otherwise.
- Updates are recorded, mirrored into the transcript and broadcast in order. Finished items are mirrored and broadcast under the transcript lock, so `attach` returns a snapshot plus a subscription with nothing missed or doubled.
- Attaching to a stored session resumes it exactly once: resumes are serialized, and cleanup is guarded with `ptr_eq`.

**`SessionClient`.** UIs program against `SessionClient`, which covers create, list, attach, prompt, cancel, set_config and close. The in-process host implements it, and so does `DaemonClient` over `aim-daemon/1`, so a UI cannot tell them apart.

**`agent::Backend`.** A session runs its turns through the `Backend` trait:
- `run_turn(input, events, cancel, steer)`, which keeps the turn contract of the native loop: the user item first, steering continues the turn or is returned, exactly one terminal event;
- `set_config` returns what is actually in force;
- `wants_environment`;
- `shutdown`.

The native `Agent` implements it, and so does `acp::AcpBackend`. ACP has no mid-turn steering, so it sends queued steering as a follow-up prompt within the same aim turn.

**Factories.** `BackendFactory` and `WorkspaceFactory` build sessions, as `ProviderFactory` already builds providers.
- `native_backends` combines a provider, an aimx workspace and aim's instructions.
- `acp::with_acp` routes `acp:*` providers to ACP.
- `providers::backends` is the full set a build hosts.
- Tests inject fakes through the same seams.

**Daemon handshake.** `aim-daemon/1` starts with `initialize { generations, client, auth? }` and returns the negotiated generation (kernel `negotiate`), the server, its pid and the message limit.

**Notification backpressure.** A full ordered notification queue makes the `aim-rpc` reader wait. The sender is slowed down and nothing is dropped. Capacity 0 refuses notifications. Consequently, a notification handler must never wait for a response from the same peer.

## Consequences

- One code path serves the CLI, the daemon and the TUI, and a new backend is one factory.
- Backends must each uphold the turn contract. The ACP bridge has its own tests (`crates/aim/tests/acp_bridge.rs`).
- A slow notification handler delays responses behind it on that connection (head-of-line). Handlers hand off quickly: the daemon client uses bounded per-stream queues that end the stream with `lagged` (ADR 0026).

## Verification

- `crates/aim/tests/host.rs`: turns, steering, cancel, config, close, and resume exactly once under concurrent attach.
- `crates/aim/tests/acp_bridge.rs`: the bridge, the refusals and `live_acp_claude_session_through_the_host`.
- `crates/aim-rpc/tests/peer.rs::a_full_notification_queue_applies_backpressure_and_loses_nothing`.
- `crates/aim/tests/daemon.rs`: initialize, refusal and ordering.
- Live evidence: `aim run` through the host completed the median fix with codex (10.0 s), OpenRouter (12.8 s) and acp:claude (16.9 s), with no stray aimx processes afterwards (commit 5dd0988).
