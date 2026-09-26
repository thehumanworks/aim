# ADR 0077: Configure TUI commands with explicit output visibility

- Status: Superseded by 0078
- Date: 2026-09-25
- Baseline: 0014, 0015, 0017, 0034
- Scope: User-owned TUI command templates and presentation settings; not daemon workflows or executable project plugins.

## Context

The TUI previously dispatched only a fixed command table, even though aim already has an argument
template expander (`resources::prompts`). User-only account information must not become a model
prompt, steering message, stored conversation, or prompt-history entry. Codex already reports
normalized subscription windows through `SessionUpdate::RateLimits`; see
`aim-llm-codex/src/limits.rs`, its captured live header fixture, and `live_codex_rate_limits`.

## Decision

Read optional `aim_home()/tui.json` at TUI startup, bounded to 64 KiB. Unknown fields, invalid
command names and collisions with built-ins are errors. Only the user's file is read; a workspace
cannot inject executable commands. Missing configuration preserves the existing presentation.

Commands declare `description`, `output` (`agent` or `user`), and `text`. Templates use aim's
existing `$ARGUMENTS`, `$1`… and `$$` expansion, without evaluating shell syntax. The same registry
feeds dispatch, help and completion. Expanded output is not recursively interpreted as a command.

- Agent output follows the ordinary prompt/steering/queue path and its existing privacy/history rules.
- User output becomes a local TUI notice only. Neither the invocation nor its output goes to
  SessionClient, prompt history, the session event log, search or model context.
- Built-in `/status` is also user-only. It displays every normalized Codex subscription window,
  with percentage used, duration and reset timestamp. It labels the data as latest reported,
  **not a live refresh**, and explains missing data and unsupported providers. It never displays
  the provider's opaque `native` metadata. It does not make a paid model call to fetch usage.

Presentation options choose the initial fullscreen layout, plain theme, and the optional
status-line segments (tokens, limits, workspace) in order. Essential session state, model and privacy
indicators remain visible. Details and examples are in [the TUI guide](../tui.md).

## Consequences

Users can install prompt routines and private informational templates without code changes.
Account status is useful after a Codex turn has reported limits; after a new attachment with no
reported limits it honestly reports unavailable data. Live account refresh, direct tool-call
commands, project command discovery, runtime reload and WASM plugins are separate extensions,
not silently executed template features. Existing declarative agent UI surfaces remain unchanged.

## Verification

- `custom_commands_route_output_without_reinterpreting_slashes`: user output has no session or
  history effects when idle, running or closed; agent output follows the prompt path.
- `status_is_user_only_and_does_not_print_native_metadata`: normalized usage is rendered locally.
- `custom_commands_complete_from_the_same_registry`: completion uses configured descriptions.
- `status_segments_can_be_hidden`: optional presentation does not remove essential state.
- `config_rejects_ambiguous_commands_and_unknown_fields`: configuration parsing fails closed.
- `configured_commands_route_output_in_a_terminal`: real PTY config loading, user-only display,
  prompt expansion and prompt-history isolation.
- Existing provider live test: `aim-llm-codex::live::live_codex_rate_limits` passed on 2026-09-25
  against the real backend (one test; credentials never printed).
