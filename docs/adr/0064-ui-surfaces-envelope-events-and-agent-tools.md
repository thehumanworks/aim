# ADR 0064: Carry agent UI surfaces as validated, logged A2UI-shaped messages

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0017, 0023, 0051
- Scope: the `aim-proto::ui` envelope, the `ui` session event and update, surfaces in attach replies,
  the `ui_show`/`ui_update`/`ui_close`/`ui_catalog` agent tools, their limits and ownership, and how a
  pressed button reaches the agent. Not: routing aim's built-in status line, tool rows and dialogs
  through the protocol, DTCG themes, plugin `ui` grants, inputs other than Button, `Cells`, images,
  or per-client catalog advertisement (later slices of ADR 0017).

## Context

ADR 0017 chose the A2UI v1.0 shape for UI that crosses process boundaries and asked for a pinned
version, an adapter for A2UI's names, session ownership, bounds and replay from the session log. A2UI
v1.0 is a release candidate (2026-06-08) whose messages are `createSurface` (which may embed
components and a data model), `updateComponents`, `updateDataModel{path, value}` (a `null` value
deletes) and `deleteSurface`, with `action{name, surfaceId, sourceComponentId, timestamp, context}`
flowing back and vendor data under `metadata.extensions` (A2UI v1.0 specification, *Extensions* and
`agent_to_renderer.json`/`renderer_to_agent.json`; `docs/research/extensibility.md` §2.1 and §C).
The session host records `SessionUpdate`s into the log (ADR 0007) and `attach` returns a transcript
snapshot taken atomically with the update subscription (ADR 0026, 0040).

Tool hosts are composed per session from `ToolsFactory`s that see only the `SessionSpec`; changing
`ToolHost`/`ToolsFactory` to carry a session context was rejected because other work depends on them.
Intercepting finished `ui_show` calls in the host was rejected: tool names can be shadowed or narrowed,
and validation would be duplicated.

## Decision

**Envelope.** `aim_proto::ui::UiEnvelope { a2ui: "1.0", <UiMessage> }` with
`UiMessage::{CreateSurface{surface_id, replace?, catalog_id, placement, components?, data?},
UpdateComponents{surface_id, components, ops?}, UpdateDataModel{ops: [{path, value}]}, DeleteSurface}`.
Every message is one atomic step: `replace` swaps a surface with the same id in one message (a refused
replacement leaves the old surface as it was), and `UpdateComponents` may carry data `ops` applied with
its upserts, all or nothing (REV19 A1, A2). Components are flat and id-keyed
(`{id, component, fallback?, ...props}`), one has the id `root`, children are referenced by id, and a
prop `{"path": "/pointer"}` binds to the data model (RFC 6901; `""` and `/` are the root, as in A2UI).
Every component may carry `fallback`: text, or `{"child": id}`. Placements are `status.left|right`,
`widget.above_editor|below_editor`, `panel.side`, `overlay`, `dialog`, `transcript` (default),
`tool(<call_id>)`, `toast` and `title`, written as those strings. `aim_proto::ui::a2ui` is the only
code that knows A2UI's wire names: it exports to and imports from A2UI v1.0 (`version: "v1.0"`),
carrying placement and fallback under `metadata.extensions.dev_aim`, a Button's `action` as
`action.event`, one A2UI `updateDataModel` per op, and a replacement as `deleteSurface` then
`createSurface` (A2UI has no replace, so the atomicity is aim's). Function calls are refused on
import.

**One fold.** `aim_proto::ui::model::Surfaces::apply` is the state every party runs — the session
host, the TUI and the web client — so they cannot diverge. It creates, replaces (the new surface goes
last and the change reports the old one), upserts by id (a replaced component keeps its position),
applies data ops all-or-nothing, and deletes.

**Catalog as data.** `aim_proto::ui::catalog::TERMINAL` (`aim/terminal@1`) lists Text (styled spans),
Markdown, Code, Diff, Row, Column, Box, Divider, List, Table, KeyValue, Progress, Spinner, Badge, Log and
Button with typed props; the host validates against it strictly (unknown props, wrong types and
missing required props are refused), and a component type outside the catalog is accepted only with a
`fallback`, which renderers show instead.

**Bounds and ownership (host policy, `aim::ui::Limits`).** Per message 64 KiB serialized; per surface
256 components, 128 KiB serialized (components and data), and a component graph that is a forest:
no component contained twice, no cycle anywhere (also among components `root` does not reach), and no
tree deeper than 16 (REV19 A3); 16 surfaces per session; a rate of 20 messages burst refilled at 10 per
second per session. Surface ids are 1–64 characters of `[A-Za-z0-9_.-]`. A surface belongs to the
session that created it: messages from any other session are refused. Worst case, a session's
surfaces are 16 × 128 KiB = 2 MiB, well under the 36 MiB daemon frame, so they travel whole.

**Events.** Each accepted message is emitted as `SessionUpdate::Ui{message}` and recorded as
`EventBody::Ui{message}` (both additive; older readers keep unknown events verbatim). `attach` and
`attach_paged` replies gain `surfaces: Vec<Surface>` (additive, omitted when empty), taken under the
same lock as the transcript; each surface records `anchor`, the number of transcript items before its
creation, so a `transcript` surface replays where it appeared. A resumed session rebuilds its surfaces
by folding its `ui` events.

**Agent tools.** Native sessions get `ui_show` (create, or replace a surface with the same id),
`ui_update` (component upserts and data ops), `ui_close` and `ui_catalog` (a component's props, on
demand). Descriptions stay compact — argument shape plus a short component list — and parameter
schemas are `{"type": "object"}`; the host validates and its errors name the offending component's
props. Together they add 773 bytes to an OpenRouter (Chat Completions) request that already offers
tools and 781 bytes in the codex (Responses) form, under the 800-byte budget (measured with the
providers' request builders, `ui::tools::tests::the_tools_add_under_800_bytes_to_a_request`). The
model boundary is forgiving where intent is unambiguous, after the raw arguments passed the same
byte bound as a message and list at most as many components as a surface holds (REV19 A4; flattening
is also bounded by the component count and depth): components nested inline are flattened into
the id-keyed list, a lone top-level component becomes `root` and several are stacked under a new root
`Column`, and a `ui_show` without an id gets `ui1`, `ui2`, … named in its result; the protocol behind
it stays strict. The tools reach their session through a task-local outlet the session actor
installs around each turn, so a call can only touch the surfaces of the session whose turn runs it;
outside a turn (e.g. on a spawned task, such as a nested call from code mode) they answer
`unavailable`.

**Clients.** Both clients fold with `Surfaces::apply`. The TUI never rewrites scrollback: a
transcript surface is printed like any finished entry, and one updated after it was printed shows
live in the pinned block and is committed once, in its final state, when the turn ends — a re-attach
after a dropped stream reconciles the same way (REV19 A5); widgets,
dialogs (modal, boxed, focused), toasts (five seconds), status segments and fullscreen's side panel
render from the current state; `overlay`, `title` and an unknown `tool(<call_id>)` degrade to the
transcript. The web renders surfaces into a tree of fixed tags whose model text is only ever text
nodes (Markdown through `pulldown-cmark`; raw HTML stays text; links keep `http(s)`/`mailto` targets
only), under the existing CSP. A closed or replaced transcript surface stays as it last was with inert
buttons, and a click is sent only if its surface, component and resolved action are still current
(REV19 A6).

**Actions.** A pressed Button becomes a user input item whose text is
`<ui_action>{"surface":…,"component":…,"name":…,"context":{…}}</ui_action>`
(`UiAction::to_input_text`), sent with `session.prompt`: it starts a turn when idle and steers the
running one otherwise, so the agent sees it on its next request. Clients render it as an action row,
not as typed text. A client from before this ADR drops `ui` updates (its parser refuses the unknown
variant and skips the notification), ignores `surfaces` in attach replies, and keeps `ui` log events
verbatim as unknown events (`an_old_client_skips_ui_updates_and_keeps_everything_else`).

## Consequences

TUI and web render the same surfaces from the same fold; the web shows the fallback of the TUI-only
components. The host changes are small (a `ui` field on the live session, a scope around the turn, a
mirror update in `publish`, surfaces in `attach`). Actions ride on prompts, so no provider or daemon
method changes; the cost is that an action is plain text to providers. `tool(<call_id>)` needs an id
the agent already knows; tools do not learn their own provider call id. The version pin and adapter
must move when A2UI v1.0 is final.

## Verification

- `crates/aim-proto/tests/ui_contract.rs`: `ui_surface_roundtrip` (envelope, event, update and A2UI
  adapter round trips over the shared fixture `tests/fixtures/ui_surface.json`), additive attach
  replies, schemas for every message and placement.
- `crates/aim/src/ui` tests: property tests for bounds, JSON-Pointer ops and ownership.
- TUI and web renderers: every component, placement degradation, fallback, parity on the shared
  fixture, and the web's inert rendering of `<script>`.
- Host and PTY tests: replay after detach/re-attach and after resume; a Button press reaching the
  agent; the live OpenRouter smoke test (`crates/aim/tests/ui_live.rs`,
  `live_ui_surfaces_openrouter`). Its first run (2026-09-25, `openai/gpt-4.1-mini`) rendered a table
  with a data-bound progress bar in 3.5 s — the model's first `ui_show` used an unknown prop, the
  refusal listed the component's props, and its second call succeeded — and moved the bar to 60%
  with `ui_update` in 2.2 s.
