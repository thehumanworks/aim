# ADR 0075: Resolve requested ACP config values to the values the agent advertises

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0005, 0012, 0022, 0038
- Scope: How aim turns a user's model or effort value (`-m opus`, `/model claude-opus-5-5`,
  `/effort High`) into one of the values an ACP agent advertises in its `configOptions`, and what
  it reports when it cannot. It covers `session/set_config_option` at session creation and
  mid-session, in `aim-acp` (`config_options`) and in `aim`'s pre-check (`acp.rs`
  `check_option`). It does not cover native providers' model catalogs, the TUI's completion of
  advertised values, or how the agent itself interprets a value it is sent.

## Context

aim runs Claude Code through the pinned adapter `@agentclientprotocol/claude-agent-acp` 0.81.2
(ADR 0012) and sets the model with `session/set_config_option {configId: "model", value}`. Until
now aim required the requested value to equal an advertised value byte for byte, twice: the
pre-check in `crates/aim/src/acp.rs` (`check_option`) and `config_options::set_params`; then
`confirm` required the agent to report that same string as current.

The adapter does not advertise a plain `opus`. Recorded live in
`crates/aim-acp/tests/fixtures/permission_turn.jsonl` line 6, and again by `live_probe` on
2026-09-25, the `model` option offers exactly: `default` ("Default (recommended)", description
"Opus (1M context)"), `opus[1m]` ("Opus 5.5"), `claude-fable-5-1[1m]` ("Fable 5.1"), `sonnet`
("Sonnet 5") and `haiku` ("Haiku 4.5"). So `aim run -p acp:claude -m opus` and
`-m claude-opus-5-5` failed inside aim before the adapter saw them, with an error that did not
list the allowed values; and `opus[1m]` is a glob in zsh when typed unquoted, so the one value
that worked was also the hardest to type.

The adapter itself accepts aliases: when a model value is not an exact option value,
`setSessionConfigOption` falls back to `resolveModelPreference`
(`dist/acp-agent.js:5165-5186`) and then applies and reports the canonical option value
(`dist/acp-agent.js:5190`, "Use the canonical option value"). `resolveModelPreference`
(`dist/session-model.js:97-147`) tries the exact value, a case-insensitive value or display name,
then substring and token-score tiers; it treats `[1m]` and `-1m` as the same context hint
(`session-model.js:3-4`), refuses a candidate whose version differs from a version in the request
(`modelVersionsCompatible`, `session-model.js:23-31`), and drops the word `claude` from requests
(`session-model.js:80`). Its last tier returns the best score, so a tie is settled silently by
list order.

Alternatives considered:

- *Send the raw value and let the adapter resolve it.* Rejected: aim would lose the local
  rejection with the allowed values, `confirm` could not compare the reported value to the
  request, and the fuzzy tiers pick silently.
- *Hardcode aliases (`opus` → `opus[1m]`).* Rejected: capabilities are data (AGENTS.md); the
  next adapter release changes the list.
- *Port `resolveModelPreference`.* Rejected: substring and score tiers are hard to state as a
  decision a user can predict, and they break ties by order.

## Decision

A requested select value is resolved against the option's advertised values by
`aim_kernel::model_match::resolve` (`DRAFT(ADR-0075)`), over keys the shell computes. Tiers, best
first:

1. **Exact** — the same bytes.
2. **Folded** — the same value or display name after lowercasing, trimming, collapsing
   whitespace, and writing the variant spellings the agent's profile declares one way (`Opus`,
   `OPUS[1M]`, `Opus 5.5`; for `acp:claude` also `opus-1m`).
3. **Family** — the request contains the value's family word and names the same variant (or
   neither names one).
4. **Family, any variant** — the request contains the family word and names no variant; the value
   has one (`opus` → `opus[1m]`).

Tiers 2–4 also require generation compatibility: a request that names a generation (`5-5`, `5.5`)
matches only values of that generation, and a value with no known generation does not match it.
The first tier with any match decides. One match resolves; two or more are `Ambiguous` and aim
picks none; none at all is `Unknown`.

Before tiers 3–4 the family must be clear. Two advertised values *conflict* when the request's
words name both of their (different) families (`opus sonnet`), or when the request names no
generation and both match it at a family tier with different generations (`opus` while Opus 4.5
and Opus 5.5 are offered). A conflict ends resolution as `Conflict`, reported like an ambiguity
with the two values; naming the generation or a variant that only one of them has settles it.
Exact and folded matches are tried before, so `claude-opus-4-5` or `opus[1m]` still resolve.

How far resolution goes is per option, and whether an option is the model or the effort is
decided by the same lookup that finds it for a key (`config_options::find`: category first, else
the conventional id): the option found for the model uses all four tiers; the one found for the
effort uses tiers 1–2; every other option (`mode`, `fast`, custom ids) matches exactly only, so
the display name of a permission mode is never an alias. Booleans keep `true`/`on`/`false`/`off`.

Variant spellings are capability data of the agent's profile, not a rule for every ACP agent:
`AcpAgentConfig::model_variants` lists them (`VariantSyntax::Bracketed { unit }` for
`name[<n><unit>]`, `VariantSyntax::Dashed { unit }` for `name-<n><unit>`), and the `claude`
profile declares both with unit `m`, the adapter's two context-hint spellings
(`session-model.js:3-4`). The session attaches its profile's list to every option list it holds
(`ConfigOption::model_variants`, never on the wire), so aim's pre-check and the session resolve by
the same rule. A profile that declares none gets no variant keys: model values then match exactly,
ignoring case, or by family and generation, and no suffix is read as a variant.

The shell (`aim-acp` `config_options`) derives the keys from the agent's own data, with no model,
family or vendor word built in:

- ids: one table per resolution maps each normalized string to a sequential `u64`; equal strings
  share an id, distinct strings never do. The kernel compares ids only.
- variant: a trailing suffix in one of the profile's declared spellings (for `acp:claude`,
  `[<n>m]` or `-<n>m`); none when the profile declares none.
- generation: the first number of one to three digits, with an optional `.`/`-` minor, taken from
  the value (variant removed) and else from the display name, encoded as `major * 1000 + minor`
  (`claude-opus-5-5` and "Opus 5.5" are 5005; a zero minor is dropped; dates are not versions).
- family: the last word (two or more letters) that the value and its display name share —
  `opus` for `opus[1m]` "Opus 5.5", `fable` for `claude-fable-5-1[1m]` "Fable 5.1". Descriptions
  are never used, so `default` ("Opus (1M context)") is not an Opus and `opus` is not a tie.
- request words: every word of the request, so `claude-opus-5-5` names `opus`; `claude` names no
  advertised family and is ignored rather than dropped by rule.

`set_params` sends the resolved advertised value and `confirm` checks that exact value against
what the agent reports, so `ConfigNotApplied` keeps its meaning. `check_option` in `aim` calls
the same resolver (`aim_acp::resolve_config_value`); its separate lookup is gone.

Errors list what the agent offers as `` `value` (Name) `` pairs, at creation and mid-session:
`ConfigValueRejected` (its `allowed` is now `Vec<ConfigValue>`) and the new
`ConfigValueAmbiguous` (ties and conflicts), which also names two of the values. Advertised model
and effort values are capability data, not secrets; they still pass through `redact` before they
are stored in an error. The request itself is never repeated in a message ("the requested model is
not offered; the agent offers …"): it could be a mistyped secret, and pattern redaction cannot
recognize every secret. `AcpBackend::set_config` prefixes only the option's label.

## Consequences

- `-m opus`, `-m Opus`, `-m claude-opus-5-5`, `-m opus-1m` and `/model fable` work with
  claude-agent-acp 0.81.2 and need no shell quoting; `-m 'opus[1m]'` still works.
- A request that names another generation (`claude-opus-4-5` while only Opus 5.5 is offered) is
  refused with the list rather than silently upgraded. When the adapter's list changes, the
  aliases follow it with no code change.
- When a family comes both with and without a variant in one generation, a request without one
  prefers the value without one (tier 3 before 4), like the adapter's own substring tier, which
  requires equal context hints (`session-model.js:117`). Across generations there is no
  preference: `opus` with Opus 4.5 and Opus 5.5 offered is refused with both named, since aim
  cannot know which is newer or preferred. A value with no readable generation counts as a
  generation of its own there.
- Another ACP agent gets the family and generation tiers but no variant folding until its
  profile declares its spellings; `-1m` is then just part of the value.
- The key derivation is a heuristic on the agent's strings. Its failure mode is a refusal
  (`Unknown`) or `Ambiguous`, both of which list the values; the kernel guarantees that a wrong
  silent pick needs a wrong key, never a tie.
- The TUI and daemon report the resolved value (`opus[1m]`) as the model in force, since that is
  what the agent reports.

## Verification

- Verus (`mise run verify`, `--no-cheating`), `crates/aim-kernel/src/model_match.rs`:
  `resolve` ensures its result equals `resolve_spec`, so resolution is a function of its keys;
  `theorem_resolved_is_the_unique_best` (the index is in bounds, matches at its tier, within the
  depth, is the only match there, and nothing matches at a better tier);
  `theorem_ambiguity_is_a_tie_at_the_best_tier`; `theorem_conflict_is_real`;
  `theorem_unqualified_family_never_picks_a_generation` (a request naming no generation never
  resolves at a family tier while two values it matches there differ in generation);
  `theorem_two_families_never_resolve` (a request whose words name two advertised families never
  resolves at a family tier); `theorem_unknown_means_no_match`;
  `theorem_unique_best_is_resolved` (the answer depends only on which values match at which tier,
  not on list order); `theorem_exact_wins`; `theorem_generation_is_respected`;
  `theorem_variant_is_respected`.
- Shell tests, `crates/aim-acp/src/config_options.rs`: every alias above against the recorded
  fixture list; `gpt-6`, `no-such-model`, `claude`, `claude-opus-4-5`, `sonnet[1m]` refused with
  all five `value (Name)` pairs and without repeating the request; a constructed tie; `opus`
  across two generations and `opus sonnet` refused with the two values; an option found as the
  model by its id resolving like the model; variants folded only when the profile declares them;
  effort case folding; property tests (every advertised value resolves to itself in any case and
  list order, Opus aliases in any case and order, generations never crossed, folding idempotent).
- Wire and backend tests: `crates/aim-acp/tests/wire.rs`
  `a_model_alias_is_sent_as_the_advertised_value_and_confirmed_by_it` (aim sends `opus[1m]` for
  `opus` and confirms the agent's canonical current value); `crates/aim/src/acp.rs`
  `a_model_alias_resolves_to_the_advertised_value`.
- Live (ADR 0022): `crates/aim-acp/tests/live.rs` `live_set_config` sets `opus` and asserts the
  turn's usage model is a `claude-opus` model; `crates/aim/tests/acp_bridge.rs`
  `live_acp_claude_session_with_model_alias` creates an `acp:claude` session with
  `model: Some("opus")` through the host. Results are recorded below.

### Live results

Recorded 2026-09-25 against claude-agent-acp 0.81.2 with the maintainer's Claude login:

- `live_probe` (before the change): the `model` option offers `default`, `opus[1m]`,
  `claude-fable-5-1[1m]`, `sonnet`, `haiku` — the fixture list.
- `cargo test --locked -p aim-acp --test live -- --ignored live_set_config`: `effort=low` and
  `model=haiku` confirmed; `no-such-model` refused locally with "model `no-such-model` is not
  offered; the agent offers `default` (Default (recommended)), `opus[1m]` (Opus 5.5),
  `claude-fable-5-1[1m]` (Fable 5.1), `sonnet` (Sonnet 5), `haiku` (Haiku 4.5)"; `model=opus`
  resolved to `opus[1m]` and confirmed; the turn replied "OK" with usage model
  `claude-opus-5-5`. Passed.
- `cargo test --locked -p aim --test acp_bridge -- --ignored live_acp_claude_session_with_model_alias`:
  a strict `acp:claude` host session with model `gpt-6` is refused with the same list; with
  model `opus` it starts on `opus[1m]` and replies "ok". Passed.
- Release binary built in the worker's worktree (`cargo build --release --locked -p aim -p aimx
  -p aim-coderun`): `aim run -p acp:claude -m opus "Reply with the word ok."` printed "ok"
  (`EndTurn`, one request), and `aim sessions` records the session as `acp:claude/opus[1m]`;
  `aim run -p acp:claude -m gpt-6 …` exits 1 with "acp:claude: model `gpt-6` is not offered;
  the agent offers …" listing all five values.

After the review fixes (conflicts, lookup-based classification, profile variants, no echo of the
request), re-run the same day:

- `live_set_config`: passed; `no-such-model` refused with "the requested model is not offered;
  the agent offers `default` (Default (recommended)), `opus[1m]` (Opus 5.5), …"; `opus` resolved
  to `opus[1m]`; usage model `claude-opus-5-5`.
- `live_acp_claude_session_with_model_alias`: passed; `gpt-6` refused with
  "acp:claude: the requested model is not offered; …" (the request is not repeated); `opus`
  starts on `opus[1m]` and replies "ok".
- Release binary rebuilt: `aim run -p acp:claude -m opus …` prints "ok" (`EndTurn`, exit 0);
  `-m gpt-6` exits 1 with the list.
