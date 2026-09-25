# ADR 0006: Own core protocols and adapt standards at the edges

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0002, 0005
- Scope: Agent, harness and daemon wire contracts; provider APIs are separate adapters.

## Context

MCP's 2026-07-28 release removed initialization and sessions in a clean break and still lacks
partial tool output and PTY semantics (`docs/research/acp-mcp.md`, §4 MCP). The research measured
JSON-RPC round trips of 11, 18 and 212 µs for 256 B, 4 KB and 64 KB payloads, respectively;
SSH and model latency dominate (`docs/research/acp-mcp.md`, TL;DR). A core built around one MCP
era would either lose aim's execution semantics or inherit standards churn.

## Decision

Define `aim-harness/1` for `aim` ↔ `aimx`: JSON-RPC 2.0, NDJSON on stdio/unix/SSH streams,
WebSocket text frames, and HTTP request/response plus SSE. Initialize once per connection with
supported generations, capabilities, principal proof and optional resume. Use the newest common
generation chosen by ADR 0005; include typed fs, exec/PTY, search, watch, tools and resume
methods (`docs/architecture.md`, §4.1).

Define `aim-daemon/1` for CLI, TUI and web clients: JSON-RPC on local unix sockets and remote
WebSocket. Model messages and tool calls as upserts, plus state, config and usage updates; add
multi-client attach, session search and aim-specific services (`docs/architecture.md`, §4.2).
`aim-proto` owns Rust wire and durable types; adapters derive schemas from them rather than
maintaining independent hand-written formats (`docs/architecture.md`, §§1, 3).

Keep standards at edges: `aimx mcp` for harness tools, `aim mcp` for agent services; serve both
the 2025 initialize era and 2026-07-28 `server/discover` era. Use MCP client for user servers,
ACP client for Claude, ACP agent later, and A2A for later federation (`docs/architecture.md`,
§4.3; `docs/research/acp-mcp.md`, §§3–5). The brief's “http/grpc/websockets” means HTTP and
WebSocket ship in the MVP; schedule gRPC after M2 as a tonic transport adapter over the same
types, not a second schema (`docs/vision.md`, Requirements; `docs/architecture.md`, §4.3).

Freeze `/1` failure semantics before declaring it stable: closed error codes with human message
and optional typed data; base64 binary in JSON; advertised message/stream limits; connection
request IDs, stable resource IDs and mutation idempotency keys. Resume tokens bind principal and
harness instance with TTL and bounded buffers. Ignore unknown wire fields, preserve and forward
unknown stored events. Version stored events independently from wire generations, migrate forward
with rollback-readable expand/contract changes. Store large blobs content-addressed, reference
count from events and GC after a grace period (`docs/architecture.md`, §4.4).

## Consequences

Standards upgrades stay in adapters, while aim owns cancellation, stream sequence and resume
semantics. The custom protocols need explicit codec and transport conformance suites. Base64 and
JSON add overhead, accepted until benchmarks show a negotiated binary path is useful.

## Verification

M1-proto adds round-trip and schema tests for both `/1` protocols. Its transport conformance
suite must test generation negotiation, closed error codes, binary limits, unknown-field/event
handling, idempotent retry, disconnect/resume within TTL and refusal after expiry
(`docs/architecture.md`, §§4.1–4.4, 15). The negotiation proof functions are named in ADR 0005.
