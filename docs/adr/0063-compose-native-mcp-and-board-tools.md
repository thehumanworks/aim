# ADR 0063: Compose trusted MCP and durable board tools into native sessions

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0014, 0018, 0030, 0045
- Scope: Native session tool composition, MCP catalog persistence, and `aim mcp` program tools.

## Context

[Architecture §4.3 and §6.3](../architecture.md) require MCP, board and program tools to share the agent's dispatcher and authority ceiling. MCP tool names and schemas arrive only after `tools/list`; waiting for a server at session creation or before the first model request adds unbounded startup latency. A prompt cannot advertise an unknown tool, so a private last-known catalog is needed for stable first-request tool definitions. The source-hash grant of [ADR 0045](0045-trusted-mcp-edges.md) gives that catalog a precise invalidation key.

Board attempts are durable shared state ([ADR 0030](0030-board-daemon-contract.md)); private and ephemeral sessions promise memory-only state. Saved programs already run through code mode's narrowed host ([ADR 0018](0018-code-mode.md)).

## Decision

- Compose trusted MCP and board hosts through `NativeServices.tools`, before the named agent's allowlist. The code-mode program host remains after the narrowed host, so nested program calls retain the same ceiling. `aim mcp --stdio` composes its service host with the existing code-mode program host when the worker is present.
- Omit board tools from private and ephemeral sessions. Persistent native sessions use a stable board run derived from the workspace location and connected canonical root. Tool factories receive that root, including after resume. Trusted MCP servers may be used in every persistence mode.
- Discover project MCP config through a harness connection for the session's local, SSH, or network location. Start each trusted MCP server independently in the background. The first model request never waits for an MCP server; later requests see any newly listed tools. A call to a cached tool waits at most 20 seconds for connection and then returns a tool error. One failed server does not fail the session or hide other servers.
- For persistent sessions only, store each server's last-known tool specs under private `~/.aim/cache/mcp/`, keyed by its trusted config entry hash, source path and location. Require an owned private directory and regular mode-0600 file. Limit each catalog to 64 tools and 256 KiB, and retain at most 128 catalog files. An untrusted, changed, malformed or oversized entry is not loaded. A live list replaces a differing cached list once. Private and ephemeral sessions neither read nor write this shared cache.

## Consequences

The first request can advertise trusted cached tools without a server handshake; a newly configured server appears after its background discovery. Tool catalog changes alter a later model request, never an in-progress one. MCP specs cost context: the pinned Everything server added 7,007 bytes to the first normalized native request in the W29 controlled measurement (2,121 to 9,128 bytes, 13 MCP tools; this is not billed-token or provider-wire accounting). Board run identity is shared by sessions for the same location and connected root.

## Verification

`mcp::cache::tests::cached_catalog_is_private_and_bound_to_grant` checks cache invalidation and file modes. `mcp::session::tests::cached_tool_stays_visible_and_failed_connect_is_a_tool_error` checks failed connection behavior. Host tests cover MCP and board allowlist narrowing, private board omission, and unavailable MCP calls without session failure. `providers::tests::persistent_board_tools_are_real_and_private_sessions_do_not_open_a_ledger` checks the real board service. Ignored live tests call the pinned Everything server from a real OpenRouter native session, call `board_list`, measure first normalized request sizes, and drive `aim mcp --stdio` from an MCP client.
