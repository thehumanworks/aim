# ADR 0023: Build the web UI in Rust with Leptos

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0007, 0017
- Scope: Browser client and private web sessions; not daemon persistence or TUI screen layout.

## Context

The brief requires a web chat UI and a private mode (`docs/vision.md`, Ideas and goals,
lines 65–74). `aim-proto` is the no-I/O shared contract that compiles to wasm, and the daemon's
UI surface protocol already describes both terminal and browser views (`docs/architecture.md`,
§3, lines 106–119; §8.2, lines 472–483). A separate browser-only widget contract would split
the plugin surface and complicate replay.

## Decision

Build `aim-web` as a Rust/WASM Leptos client served by the daemon at M9. Reuse `aim-proto` types
and speak `aim-daemon/1` over WebSocket. Render the same declarative UI surfaces and themes as
the TUI, including built-in status, tool rows, dialogs, and unknown-component fallbacks
(`docs/architecture.md`, §11, lines 609–624).

Web private mode constructs an ephemeral session backed by `MemoryStore`; the session does not
enter the persistent DB, index, memory writer, or telemetry. The `Persistent` witness is
unavailable to it. Apply backend-specific privacy rules, including refusing Claude private
mode if ACP persistence cannot be disabled and disclosing dictation retention before use
(`docs/architecture.md`, §5.4, lines 226–239).

Authenticate browser WebSocket connections with scoped tokens and check `Origin`; a
non-loopback listener also requires TLS or a declared protected reverse proxy. Web UI rendering
does not create a second source of authorization (`docs/architecture.md`, §12, lines 635–641).

## Consequences

Rust types cross daemon and browser boundaries directly, while Leptos and wasm add an M9 build
target. Terminal-specific UI components need browser renderings or declared fallbacks.

## Verification

In M9, add `web_ui_protocol_roundtrip`, `web_unknown_component_fallback`,
`web_private_no_persistent_writes`, and browser WS origin/authentication tests. Replay one
surface fixture in both TUI and web renderers and compare its semantic actions.
