# ADR 0051: Serve the browser client through the daemon

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0023, 0026, 0037, 0040, 0047
- Scope: First daemon browser listener, token registry, static assets, and Leptos client; not a new daemon protocol generation or an authorization grant system.

## Context

ADR 0023 chooses a Rust/Leptos browser client on `aim-daemon/1`. The daemon already has ordered updates, terminal attachment notifications, and paged snapshots (ADRs 0026, 0037, 0040). The harness network transport established a bounded WebSocket bridge and network protection rules (ADR 0047); `docs/architecture.md` §§11–12 requires the same protections for daemon browsers. A browser must load its assets and connect to the daemon without exposing a bearer in a URL or crossing origins.

## Decision

- `aim daemon --web ADDR` runs the web listener alongside the owner's existing unix listener and session host. It serves static assets and `/ws` from one origin. The assets can be located explicitly with `--web-assets`; a source checkout uses `crates/aim-web/dist` by default. The HTML response carries a restrictive Content Security Policy, and the client uses external scripts and styles. Asset paths are resolved within the configured directory.
- `aim daemon token create` displays a random bearer once. The daemon keeps only its SHA-256 digest and expiry in a mode-0600 registry under the aim home, with a stable lock during concurrent issuance. Browser WebSocket admission checks the bearer, expiry, and an explicit Origin allowlist; a loopback origin is accepted for the loopback listener. An off-loopback bind requires TLS or `--behind-proxy`. Frame size, connection count, and header admission are bounded. The browser supplies the bearer in `initialize` after the WebSocket handshake rather than in the URL.
- The Leptos CSR app uses `aim-proto` daemon messages. Its token lives in memory or `sessionStorage`, never in a URL or `localStorage`. It pages large attachments, applies `session.update` in wire order, and reattaches after `session.detached` or lag. Creating a private session sets `Persistence::Ephemeral` and labels it in the UI. Composer state distinguishes a running turn's steering from a new prompt.
- Trunk and the matching `wasm-bindgen-cli` are pinned in `mise.toml`. `mise run web:build` is a separate gate because a WASM link and bundle are much slower than the native format, lint, and test loop.

## Consequences

An installed binary needs a built asset directory passed via `--web-assets` until packaging embeds or installs the bundle. Operators who expose the listener through a proxy must protect the proxy-to-daemon hop and declare the browser Origins they allow. Web clients inherit the existing session host's authority; the token authenticates access to that host and does not widen any session or tool grant. Private mode retains the backend-specific privacy restrictions of ADR 0023.

## Verification

`crates/aim/tests/web_daemon.rs` tests bearer and Origin refusal, network limits, and ordered updates over the listener. `mise run check` covers native Rust and repository invariants; `mise run web:build` checks the WASM bundle. The W24 handoff report records the live headless browser OpenRouter turn, latency, and bundle size.
