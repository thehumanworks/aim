# ADR 0045: Bind MCP imports to their source and run location

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0009, 0014, 0022, 0035
- Scope: User MCP server imports and aim's stdio MCP service edge. Session composition remains with the agent host.

## Context

MCP configuration can name local executables, workspace executables, or HTTP services. Foreign configurations may be discovered from Claude, Codex, and Cursor without making those executables safe to start. An SSH workspace adds a separate process location: starting its declared command on the aim client's machine could run a different binary or expose local data. MCP also has the 2025 `initialize` and 2026-07-28 `server/discover` lifecycles. The protocol evidence and transport choices are recorded in [the MCP research](../research/acp-mcp.md), [ADR 0009](0009-ssh-shadowing.md), and the [official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk).

## Decision

- Read project MCP definitions through the workspace harness; read user definitions locally. Preserve each source path, origin, location and exact entry hash.
- Native `~/.aim/mcp.json` entries are user controlled. Project and foreign entries are listed but remain inert until `aim mcp trust` records a path, entry-hash and location grant in private `~/.aim/trust.toml`. An edited entry loses its grant. `untrust` removes it.
- Spawn trusted `location: workspace` stdio servers only through aimx `exec.spawn`/`exec.read`/`exec.write_stdin`/`exec.release`. Spawn trusted local stdio servers locally with a limited inherited environment. Refuse workspace HTTP endpoints until a workspace-host HTTP proxy exists; never silently route them through the local host.
- Use bounded MCP streams and tool catalogs, namespace imported tools as `mcp__<server>__<tool>`, and treat external tool annotations as untrusted hints. Calls have a deadline and cancellation notification.
- `aim mcp --stdio` exposes the agent's existing search, board, memory and available media tools under both lifecycles. The server adapter accepts any `ToolHost`, so later program tools can be composed without changing the wire implementation.

## Consequences

Imported executables need a new grant after their definition changes. Workspace HTTP MCP is unavailable until a proxy can preserve the declared location. aim's stdio server is explicitly launched by the external client and has the launching user's local service authority.

## Verification

Configuration and trust tests cover hash-bound grants and inert imports. MCP server tests cover both lifecycles, calls, errors, cancellation, deadlines and frame limits. Client and harness tests cover namespacing, transport, and workspace spawning. Ignored `live_*` tests use the mise-pinned Everything server and a real Rust SDK client to call `aim mcp`'s `search_sessions`.
