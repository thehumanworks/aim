# ADR 0017: Render extensible UI as declarative surfaces

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0015, 0016
- Scope: TUI and web surface data, actions, and themes; not their screen layouts.

## Context

The TUI, daemon, web client, plugins, and agents run across process boundaries. A plugin-specific
widget API would make the web and terminal experiences diverge. A2UI has flat, id-keyed components,
JSON-Pointer data updates, and actions back to the agent, although v1.0 is still a candidate and
its message names have changed (`docs/research/extensibility.md`, §2.1, lines 234–249;
§C, lines 555–561). MCP Apps HTML cannot render natively in a terminal (lines 251–259).

## Decision

Define UI surfaces as `aim-proto` data using the A2UI v1.0 shape: surface IDs, id-keyed component
upserts, pointer-based data updates, and actions carrying source component and context. Pin the
A2UI version in aim's envelope and isolate import/export naming in an adapter.

Advertise client catalogs; add `aim/terminal@1` for terminal-first Text, Markdown, Code, Diff,
layout, lists/tables/trees, status, image, input, and `Cells` components. Support placements
`status.*`, `widget.*`, `panel.side`, `overlay`, `dialog`, `transcript`, `tool(call_id)`, `toast`,
and `title`. Render unknown components using their fallback child or text.

Route aim's own status line, tool rows, and dialogs through this protocol. Agents use `ui.show`,
`ui.update`, and `ui.close`; schema-validate and bound their surfaces, assign session ownership,
and record updates in the session log for replay (`docs/architecture.md`, §8.2, lines 472–483;
`docs/research/extensibility.md`, §C, lines 563–581).

Themes are DTCG token files with semantic color roles and a terminal extension for ANSI 256/16
fallbacks, text attributes, and glyph sets. Resolve the same semantic tokens for TUI and web;
keep cell sizing in layout, not theme (`docs/research/extensibility.md`, §2.1, lines 278–280;
§C, lines 583–589).

## Consequences

Renderers maintain catalog and fallback behavior, while plugins can replace built-in surfaces.
A2UI version churn stays in the adapter; terminal-only components still need web representations.

## Verification

In M7, add `ui_surface_roundtrip`, `unknown_component_fallback`, and
`builtin_surface_tui_web_parity` contract tests against the same `aim-proto` fixture.
The existing research's ratatui A2UI compatibility claim remains unverified (line 249), so
these tests must exercise aim's renderer rather than assuming that crate conforms.
