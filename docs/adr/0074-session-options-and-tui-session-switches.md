# ADR 0074: Sessions advertise what they can switch to; the TUI derives new sessions in the kernel

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0031, 0038, 0056, 0064
- Scope: the `aim-daemon/1` update `SessionUpdate::Options` with `SessionOptions`/`ChoiceValue`,
  the `options` field of `session.attach` and `session.attach_paged` replies, the `Backend::options`
  hook that fills them, and the TUI's `/provider`, `/clear`, `/new`, `/model` and `/effort` decisions
  (`aim_kernel::switch`). It does not cover how an ACP agent resolves a value (ADR 0075), the web
  client (which ignores the update), or what `auto` does (ADR 0038); only where it is offered.

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
- Where `auto` (`AUTO_EFFORT`, ADR 0038) is taken differs by backend:
  - the native loop always takes it: `set_config` maps it to "no level, source automatic" and never
    refuses it (`crates/aim/src/agent/backend.rs:147,172`). What it then does depends on Jev: a
    decider is attached only to persistent sessions (`crates/aim/src/host.rs:296`) and exists only
    with `TYPESAFE_API_KEY` (`crates/aim/src/providers.rs:139`); Jev decides only for an automatic
    source and a model ladder of 2 to 10 levels (`crates/aim/src/agent/mod.rs:351-363`,
    `ladder_start` at `:169`), moving one step per decision from the level in force. Without it the
    level in force stays, unpinned, and a model change resets it to the provider's default
    (`forget_window`, `crates/aim/src/agent/mod.rs:836-843`);
  - an ACP agent takes only values it advertises: ADR 0075's resolver refuses anything else
    (`crates/aim-acp/src/config_options.rs:208`, used by `check_option` at
    `crates/aim/src/acp.rs:155`), and claude-agent-acp 0.81.2 advertises no `auto` (fixture above;
    test at `crates/aim/src/acp.rs:679`).
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
   `SessionOptions { models: Vec<ChoiceValue>, efforts: Vec<ChoiceValue>, auto_effort: Option<String> }`
   and `ChoiceValue { value, name?, description? }`. `models` are the values `session.set_config`
   takes for this session's provider; `efforts` is the ladder of the model in force, least effort
   first (empty: none known). `auto` (`AUTO_EFFORT`) is never listed in `efforts`: `auto_effort` is
   `Some(what it does here)` when the session takes it and `None` when it refuses it. Native
   sessions always take it ("Jev picks the effort per request" with a decider and a usable ladder,
   else "unpinned: kept until a model change, then the provider's default"); an ACP agent takes it
   only when it advertises an `auto` effort value, which is then reported here rather than as a
   level. The latest options are replayed as `options` in `session.attach` and
   `session.attach_paged` replies; the fields and the update are additive and are not recorded in
   the session log.
2. **Backends fill it, off the critical path.** `Backend::options()` returns a `'static` future
   (nothing borrowed from the backend). The host spawns it after the session is live and again after
   every announced configuration, aborting an older lookup, and publishes the answer under the
   transcript lock (so an attach sees it in its snapshot or on its stream, never both or neither).
   An unchanged answer is not sent again, except after a model change: clients treat the ladder as
   unknown from a model's `ConfigChanged` until the next `Options`. Every lookup is tagged with the
   session's configuration generation, which `announce` bumps before it sends `ConfigChanged` and
   the actor bumps at close; a lookup publishes only if its generation is still current, checked
   under the options lock that also orders the bump. An abort alone cannot stop a lookup that has
   already resolved (REV-T1 B1). The live summary's model follows every announced change and a
   resumed session's summary shows the model it resumed with, so a client attaching later is told
   the model in force.
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
   - `/effort <value>` is refused locally only when the session said so: a level off a known ladder,
     or `auto` where `auto_effort` is `None`. An unknown ladder (options not arrived, or empty) or
     unknown `auto` support leaves the decision to the session, as before. Values match with case
     and inner whitespace folded (as ADR 0075 folds them) in the shell's id registry, so `Low`
     matches `low` while the kernel's check stays exact over ids. Native sessions are sent the
     ladder's spelling of the matched level (the loop matches exactly); ACP sessions are sent the
     value as typed, for ADR 0075's resolver. `/effort` completes the ladder's levels once, in its
     order, then `auto` only where the session takes it. `/model` completes the session's `models`
     and is always sent (an ACP agent may resolve aliases).
   - Until a session's options arrive, completion falls back to values seen *for its provider*.

## Consequences

- A reattached or second client completes correctly at once; a provider switch never offers another
  provider's models.
- Each native model change costs one catalog lookup in the background (cached for the gateways; a
  conditional `If-None-Match` GET for codex, as `set_config` already does).
- `auto` is not offered before a session's options arrive (its support is unknown then); typed, it is
  still sent and the session decides. It is never sent to a session that said it refuses it.
- A backend that cannot tell (a test fake, a future agent) sends nothing; clients keep their
  fallback.

## Verification

- Proofs in `crates/aim-kernel/src/switch.rs`: `provider_switch_resets_model_and_effort`,
  `same_provider_switch_is_a_no_op`, `new_and_clear_keep_the_session_shape`,
  `switches_keep_place_and_privacy`, `effort_sent_only_if_offered`,
  `auto_is_never_sent_where_refused`, `distinct_levels_are_the_ladder_once`, and
  `effort_candidates_are_the_ladder_and_auto` (every candidate is one the shell sends, `auto` is
  offered iff taken and comes last, no duplicates). `effort_candidates_spec` pins the order: the
  ladder's first occurrences in order (`distinct_levels`), then `auto` when taken (REV-T1 N1). The
  exec fns `derive`, `effort_sent` and `effort_candidates` are verified to equal their specs.
- Generation fence: `crates/aim/src/host.rs::tests::a_lookup_for_an_older_configuration_never_publishes`
  (a lookup released after a model change; fails without the check). Summary model:
  `crates/aim/tests/host.rs::attach_reports_the_model_in_force_after_a_change_and_a_resume`; what
  `auto` does: `options_say_what_auto_does_in_an_advised_session`.
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
  only `auto` was offered (before `auto_effort` existed).
- Live, same day, after `auto_effort` and case folding: on `acp:claude`, `/effort ` listed
  `default … max` without `auto`, `/effort Low` put `low` in force through ADR 0075's resolver, and
  `/effort auto` was refused locally; on `codex` (an ephemeral session, so no Jev), `/effort ` listed
  the ladder and `auto` described as "unpinned: kept until a model change, then the provider's
  default", and `/effort High` put `high` in force.
