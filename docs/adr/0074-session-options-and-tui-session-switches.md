# ADR 0074: Sessions advertise what they can switch to; the TUI derives new sessions in the kernel

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0031, 0038, 0056, 0064
- Scope: the `aim-daemon/1` update `SessionUpdate::Options` with `SessionOptions`/`ChoiceValue`,
  the `options` field of `session.attach` and `session.attach_paged` replies, the `Backend::options`
  hook that fills them, and the TUI's `/provider`, `/clear`, `/new`, `/model` and `/effort` decisions
  (`aim_kernel::switch`). It does not cover how an ACP agent resolves a model alias (T5), the web
  client (which ignores the update), or making `auto` meaningful for ACP agents.

## Context

- The TUI completed `/model` and `/effort` from values it had *seen* (the configured model, models of
  listed sessions, efforts of past `ConfigChanged` updates; `crates/aim/src/tui/app.rs` `models` /
  `efforts` before this change). Those lists mixed providers: after attaching a codex session and
  listing sessions, an `openrouter` session was offered codex model ids.
- The information exists but never reached clients:
  - native providers return a catalog of `ModelInfo { id, display_name, efforts, default_effort,
    hidden, … }` (`crates/aim-llm/src/lib.rs:46-74`); W26 already fetches it once while building a
    codex session and deliberately skips that fetch for `openrouter`/`ai-gateway` so it never holds
    up the first request (`crates/aim/src/host.rs:252-262`, ADR 0056);
  - ACP agents advertise `configOptions` with categories `model` and `thought_level`, parsed into
    `AcpSession::config_options()` (`crates/aim-acp/src/session.rs:85`). claude-agent-acp 0.81.2
    offers models `default` (Opus 1M), `opus[1m]`, `claude-fable-5-1[1m]`, `sonnet`, `haiku` and
    efforts `default`, `low`, `medium`, `high`, `xhigh`, `max`
    (`crates/aim-acp/tests/fixtures/aim_tools_turn.jsonl`, line 6). The task log records that these
    values "never reach the UIs" (`docs/tasks.md`, T5 finding).
- A session's provider is fixed at creation: `session.set_config` takes only `model` and `effort`
  (`crates/aim-proto/src/daemon.rs:443`). Switching providers therefore means a new session. A
  model or effort carried into it would be chosen under the old provider: `providers::build` uses
  an explicit model verbatim and the provider's default only when none is given
  (`crates/aim/src/providers.rs:100,105`), so a codex id would be sent to OpenRouter.
- Surfaces set the pattern for state a late client must see (ADR 0064): the attach reply carries a
  snapshot taken under the same lock that orders the stream. Old clients drop update types they do
  not know and read on (`crates/aim/src/daemon/client.rs:99`, REV19 test
  `crates/aim-proto/tests/ui_contract.rs:249`).

Alternatives considered: a `session.options` request (a round trip per keystroke, or a cache with
its own invalidation, and nothing tells the client when the ladder changed); putting the lists in
`SessionSummary` (it is persisted and listed for stored sessions, whose catalog is unknown); keeping
seen values (wrong across providers).

## Decision

1. **One contract.** `SessionUpdate::Options { options: SessionOptions }` with
   `SessionOptions { models: Vec<ChoiceValue>, efforts: Vec<ChoiceValue> }` and
   `ChoiceValue { value, name?, description? }`. `models` are the values `session.set_config`
   takes for this session's provider; `efforts` is the ladder of the model in force, least effort
   first (empty: none known). `auto` (`AUTO_EFFORT`) is never listed; clients add it. The latest
   options are replayed as `options` in `session.attach` and `session.attach_paged` replies; both
   fields and the update are additive and are not recorded in the session log.
2. **Backends fill it, off the critical path.** `Backend::options()` returns a `'static` future
   (nothing borrowed from the backend). The host spawns it after the session is live and again after
   every announced configuration, aborting an older lookup, and publishes the answer under the
   transcript lock (so an attach sees it in its snapshot or on its stream, never both or neither).
   An unchanged answer is not sent again, except after a model change: clients treat the ladder as
   unknown from a model's `ConfigChanged` until the next `Options`.
   Native sessions answer from the catalog W26 fetched at start (codex) or from one bounded
   background fetch (the gateways' catalog is cached by the provider), without hidden models, with
   the current model's ladder and the default effort marked. ACP sessions answer from the agent's
   advertised `model` and `thought_level` values, which are fresh after every `set_config`.
3. **The TUI switches sessions by a verified derivation** (`aim_kernel::switch`):
   - `/new` and `/clear` create a session like the attached one (provider, location, workspace,
     persistence, model, effort); `/clear` also clears the transcript, the screen and the
     scrollback first, `/new` continues below the old output;
   - `/provider <id>` validates the id against `providers::KNOWN`, is a no-op for the current
     provider, and otherwise creates a session like the attached one on that provider with **no
     model and no effort**, so the provider picks its defaults;
   - `/effort <level>` is sent only when it is `auto`, the ladder is not known yet (or empty: the
     backend decides, as before), or it is in the ladder; `/effort` completes the ladder plus
     `auto`, without duplicates. `/model` completes the session's `models` and is always sent (an
     ACP agent may resolve aliases).
   - Until a session's options arrive, completion falls back to values seen *for its provider*.

## Consequences

- A reattached or second client completes correctly at once; a provider switch never offers another
  provider's models.
- Each native model change costs one catalog lookup in the background (cached for the gateways; a
  conditional `If-None-Match` GET for codex, as `set_config` already does).
- The TUI offers `auto` for ACP sessions too, which claude-agent-acp refuses
  (`crates/aim/src/acp.rs:650`); the refusal is reported as before. Open until ACP effort semantics
  are decided.
- A backend that cannot tell (a test fake, a future agent) sends nothing; clients keep their
  fallback.

## Verification

- Proofs in `crates/aim-kernel/src/switch.rs`: `provider_switch_resets_model_and_effort`,
  `same_provider_switch_is_a_no_op`, `new_and_clear_keep_the_session_shape`,
  `switches_keep_place_and_privacy`, `effort_candidates_are_the_ladder_and_auto` (with the exec
  fns' `ensures`: no duplicates, exactly ladder ∪ {auto}), `effort_sent_only_if_offered`.
- Contract: `crates/aim-proto/tests/contract.rs::adr_0074_session_options_are_additive`.
- Host and daemon: `crates/aim/tests/host.rs::options_are_published_replayed_on_attach_and_follow_the_model`,
  `a_session_without_a_catalog_sends_no_options_and_is_not_held_up`,
  `crates/aim/tests/daemon.rs::session_options_reach_daemon_clients_on_attach_and_on_the_stream`;
  ACP: `crates/aim/src/acp.rs::claude_agent_acp_models_and_efforts_become_options` (the recorded
  0.81.2 answer) and `advertised_options_follow_the_agents_answers`.
- TUI: unit tests in `crates/aim/src/tui/app/tests.rs` and `crates/aim/src/tui/choices.rs`; PTY tests
  in `crates/aim/tests/tui_pty.rs` (`provider_popup_lists_the_known_providers`,
  `model_popup_lists_the_scripted_catalog_and_efforts_follow_the_model`,
  `clear_wipes_the_screen_and_starts_a_new_session`,
  `clear_in_fullscreen_leaves_no_old_rows_on_either_screen`); the scrollback purge in tmux
  (`crates/aim/tests/tui_tmux.rs::tmux_clear_purges_the_scrollback`, ignored: needs tmux).
- Live, 2026-09-25, release build in tmux: on `codex`, `/model ` listed the real catalog
  (`gpt-6-astra`, `gpt-6-sol`, `gpt-6-luna`, `gpt-5.6-*`, … with 272k windows) and `/effort ` the
  `gpt-6-sol` ladder `low … ultra` with `medium` marked default, plus `auto`; `/effort bogus` was
  refused locally; `/provider openrouter` started a session on `anthropic/claude-sonnet-5` (the
  gateway default, no codex model carried) whose `/model ` listed OpenRouter's catalog; on
  `acp:claude`, `/model ` listed `default`, `opus[1m]`, `claude-fable-5-1[1m]`, `sonnet`, `haiku` and
  `/effort ` `default … max`, and after `/model haiku` the adapter advertised no effort option, so
  only `auto` was offered.
