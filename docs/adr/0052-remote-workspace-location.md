# ADR 0052: Persist remote harness URLs as workspace locations without credentials

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0008, 0009, 0022, 0047
- Scope: Native aim sessions and board workers using an authenticated network aimx workspace; ACP MCP relay over a network harness remains separate work.

## Context

ADR 0047 added authenticated WebSocket and HTTP harness transports but deferred the persisted workspace location. A session needs its remote endpoint after daemon restart, while a bearer must never enter a session event, CLI argument, URL, or log. The `Location` type already distinguishes local and SSH workspaces (`crates/aim-proto/src/daemon.rs`). The harness client already validates network URLs and opens the remote root (`crates/aim/src/harness.rs`).

## Decision

- Add `Location::Remote { url }` to the daemon contract. The URL is a workspace endpoint and must contain no userinfo or query credentials; the existing harness URL validation applies before connection. Session metadata records `remote:<url>` and reconstructs the location on resume.
- Native sessions and board workers connect through `HarnessClient::connect_ws` for `ws(s)` and `connect_http` for `http(s)`. They use the resulting workspace tools and project resource reader, so the remote root and instructions are authoritative.
- The bearer is read at connection time from `AIM_REMOTE_TOKEN`, or from the file named by `AIM_REMOTE_TOKEN_FILE`. A file must be a regular, non-symlink, owner-only 0600 file. Neither source is serialized into the location. A restart needs the credential source provisioned again.
- `aim run --remote` and `aim tui --remote` select this location. `-C` names the path on the remote host and is not canonicalized on the client. `--remote` and `--ssh` are mutually exclusive for `run`.
- Strict ACP sessions refuse this location until its MCP relay can reach the same network harness. Native ACP tools already refuse nonlocal locations.

## Consequences

The same session spec now names local, SSH, and authenticated network workspaces. A remote workspace is still controlled by aimx's server grants, token lifetime, and per-call scopes (ADRs 0046 and 0047). An unset or unsafe bearer source prevents connection. Remote URLs can disclose hostnames in persisted metadata but contain no bearer.

## Verification

The wire round trip is checked in `remote_location_is_additive_and_contains_no_credential`. Local credential-file and remote routing tests cover private-file admission, resource routing and nonlocal root handling. A live loopback TLS WebSocket smoke with one OpenRouter editing turn is required by ADR 0022; its result is recorded in the W25 report.
