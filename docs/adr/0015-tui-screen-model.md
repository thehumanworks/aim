# ADR 0015: Keep chat inline with native scrollback

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006
- Scope: TUI screen ownership, rendering and completion behavior.

## Context

Codex, pi and oh-my-pi retain terminal scrollback for ordinary chat and use the
alternate screen for overlays. Pi diffs changed lines and coalesces paints; tny's
dashboard cleared scrollback (research/tui-ux.md, TL;DR and §1, lines 7–140).
The maintainer chose inline chat as the default (docs/architecture.md §11).

## Decision

- Render normal chat inline into native terminal scrollback, with a bounded pinned
  region for the streaming partial, composer, completion popup, queue and status.
  Do not clear accumulated scrollback as part of ordinary navigation.
- Borrow the alternate screen for pickers, search, dashboards, diff inspection and
  plugin panels, then restore the inline view. `--fullscreen` selects a canvas-style
  alternate-screen chat layout from launch.
- Keep one semantic transcript model for both layouts and for reattachment. Rendering
  changes must not alter transcript or session event semantics.
- Use one frame scheduler with synchronized output and dirty-line diffing. Render
  keystrokes immediately and coalesce streaming output; settle resize and backlog
  without discarding session events.
- Provide a multiline composer with bracketed paste (collapse large pastes), history
  search, optional vim/external editor, visible steering and queue outcomes, and
  `/dictate` entry. UI surfaces use the same declarative protocol as the web client.
- Route file/directory, slash command, skill, agent, session and program completions
  through one async broker. Use `ignore` plus `nucleo` for workspace paths. Cancel stale
  requests and fence results by input generation; later cloud buckets implement the
  same broker contract (research/tui-ux.md, §3, lines 198–251).

## Consequences

Terminal users keep familiar scrollback and can still open rich temporary views.
The renderer must reconcile immutable printed history with a live pinned area and
resize behavior. A single transcript model prevents layout-specific state drift.

## Verification

- M3 PTY tests `inline_scrollback_preserved`, `overlay_restores_inline`,
  `fullscreen_transcript_parity`, and `completion_stale_result_fenced` are to be added.
- M3 render benchmark measures first paint, keystroke response, stream backlog and
  memory at fixed transcript sizes; no performance claim is made before measurement.
