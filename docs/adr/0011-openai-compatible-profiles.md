# ADR 0011: Configure OpenAI-compatible providers as named profiles

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0010
- Scope: OpenAI-compatible chat and Responses providers, excluding ChatGPT OAuth.

## Context

The brief requires OpenRouter, Vercel AI Gateway and user-configured compatible endpoints.
They share wire shapes but differ in limits and accounting. A live probe found OpenRouter
cost fields and Vercel AI Gateway rejecting `max_tokens < 16` (research/live-probes.md,
OpenAI-compatible gateways, lines 31–36). The profile and quirk boundary is defined in
docs/architecture.md §6.5.

## Decision

- Configure named profiles (`aim_llm_openai::Profile`) with `{id, base_url, api_key_env, wire,
  quirks, headers, models}`. `wire` selects `chat` or `responses`; do not infer it from a model
  name. Only `chat` is implemented: a `responses` profile is rejected when the provider is built
  (`InvalidRequest`, a configuration error). Reference an environment variable by name, never
  save its value in project configuration or session events. The key is read at request time.
- Ship presets for OpenRouter and Vercel AI Gateway (`Profile::openrouter()`,
  `Profile::ai_gateway()`), while allowing user-defined endpoints. A preset supplies defaults and
  an explicit user profile can override them. *As implemented:* a profile is complete, and code
  derives a variant by editing a preset; merging a partial user profile over a preset by `id` is
  not implemented yet.
- Express provider differences as profile data interpreted by the adapter, not branches
  keyed to provider names. Vercel AI Gateway sets `min_output_tokens = 16`; reject or raise
  a smaller requested limit before sending the request, and report the effective limit
  (`OpenAiProvider::effective_max_output_tokens`). Unknown profile and quirk keys are rejected.
- Preserve provider-specific usage as a sidecar to normalized usage. Do not flatten cost,
  cache-write and surcharge fields into a value whose semantics differ by gateway. The normalized
  `cost_micro_usd` is the **billed** amount named by the profile's `cost_pointer`; the whole usage
  object stays in `Usage.native`. Usage a profile asked for is required: budgets and cost
  accounting never see a completed turn with zero usage the server did not report. This is a
  `Protocol` error, not an "unknown usage" value. `aim-proto`'s `Usage` has no such state, both
  gateways send usage on every streamed turn (verbatim captures, seven live tests), and an
  endpoint that cannot report usage says so with `supports_stream_usage = false`.
- Derive available models and caps from endpoint catalogs when provided; a configured slug
  remains explicit when discovery is absent. A listed model still needs a live turn before
  aim claims it is usable. A capability the catalog does not state is assumed absent: in
  particular a model is sent tool-result images only when its catalog entry lists image input.
  The agent layer does not call `catalog()` today, so the provider fetches it itself the first
  time a request carries a tool-result image for a model it has not seen.

*Amended 2026-09-25, same day:* the profile settings below record what `crates/aim-llm-openai`
implements after its first two reviews. The first version named `key_env` (the field is
`api_key_env`) and listed only `min_output_tokens` as a quirk.

### Profile settings

| Field | Meaning |
| --- | --- |
| `id` | Provider id; also scopes replayed provider-native items (`NativeItem.provider`). |
| `base_url` | Endpoint root without a route (`…/v1`); `chat/completions` and `models` are appended. |
| `api_key_env` | Environment variable holding the bearer key, read per request. |
| `wire` | `chat` (implemented) or `responses` (rejected at construction). |
| `headers` | Extra request headers with literal, non-secret values. `Authorization` is rejected; names and values are validated at construction; `Debug` prints names only. |
| `models` | Static catalog for endpoints without `GET /models`; when set, the catalog is never fetched. |

| Quirk | Meaning | OpenRouter | AI Gateway |
| --- | --- | --- | --- |
| `min_output_tokens` | A smaller requested output cap is raised to this. | – | `16` |
| `max_output_tokens_field` | `max_tokens`, `max_completion_tokens` or `max_output_tokens`. | `max_tokens` | `max_tokens` |
| `supports_parallel_tool_calls` | Send `parallel_tool_calls` when tools are offered. | yes | yes |
| `supports_stream_usage` | Request usage with `stream_options.include_usage` **and require it**: a response without a usage chunk (numeric `prompt_tokens` and `completion_tokens`; `usage: {}` does not count) is a `Protocol` error, never a turn reported with zero usage. `false` opts out: usage is neither requested nor required, and a turn without it reports zero counts and no `native` usage. | yes | yes |
| `reasoning_param` | Effort as `reasoning: {effort}` (`open_router`), `reasoning_effort` (`open_ai`) or unsupported (`none`, a request with an effort is `InvalidRequest`). | `open_router` | `open_ai` |
| `cost_pointer` | JSON pointer into the streamed `usage` object naming the billed USD amount. | `/cost` | `/gateway_cost` |
| `replay_reasoning_details` | Replay streamed `reasoning_details` on the assistant message of the same response (only this profile's own). | yes | yes |
| `tool_result_images` | Send tool-result images as `image_url` parts in a user message after the tool results, to models whose catalog entry accepts images; otherwise a text placeholder. A model the provider has not seen is looked up in the catalog first (one fetch per provider, never with a static `models` list); a model the catalog does not vouch for, or a failed fetch, gets the placeholder. | yes | yes |
| `extra_body` | Fields merged into the top level of every request body. | `{"cache_control": {"type": "ephemeral"}}` | `{"providerOptions": {"gateway": {"caching": "auto"}}}` |
| `session_header` | Header carrying `Request.session_id` for cache affinity. | `x-session-id` | `x-session-affinity` |
| `cache_key_field` | Body field carrying `Request.cache_key` (`prompt_cache_key` for OpenAI direct). | – | – |
| `idle_timeout_secs` | Seconds without any response byte (headers, data, keepalive comments) before the call fails as `Transport`; connect timeout is 10 s; there is no whole-request timeout. | 300 (default) | 300 (default) |

Evidence for the preset values (live probes of 2026-09-25 unless a document is cited):

- **16-token minimum.** AI Gateway answers `max_tokens: 15` with HTTP 400 "Expected a value
  >= 16, but got 15"; the preset raises a request for 1 to 16 and the call succeeds
  (`live_ai_gateway_text_turn` sends both).
- **Billed cost.** On AI Gateway `usage.cost` is the market price and `usage.gateway_cost` the
  amount debited, including surcharges such as zero data retention: a streamed gpt-4.1-mini call
  returned `cost 0.00001`, `surcharge_cost 0.0001`, `gateway_cost 0.00011`, and
  `GET /v1/generation?id=…` for it returned `total_cost 0.00011`. Vercel documents `gateway_cost`
  as "Total amount debited from your AI Gateway balance … Same as `total_cost`"
  ([REST API](https://vercel.com/docs/ai-gateway/sdks-and-apis/rest-api),
  [custom reporting](https://vercel.com/docs/ai-gateway/observability-and-spend/custom-reporting)).
  OpenRouter's `usage.cost` (research/live-probes.md, line 35) is taken as its billed amount;
  that is not verified against account statements.
- **Caching and affinity.** With `extra_body` above and a ~3k-token system prompt sent twice to
  `anthropic/claude-sonnet-4.5`, both gateways wrote 2984 cache tokens on the first call and read
  2984 on the second (OpenRouter cost fell from $0.0113 to $0.0010). The same body was accepted by
  `openai/gpt-4.1-mini` and `google/gemini-2.5-flash`, whose upstreams cache implicitly.
- **Reasoning replay.** `live_openrouter_reasoning_tool_turn` and
  `live_ai_gateway_reasoning_tool_turn` replay one merged, signed `reasoning_details` entry on
  the message that carries its tool calls; the same request with a tampered signature is rejected
  upstream, so the replay reaches Anthropic and is validated there.

## Consequences

New compatible gateways can be added by data without growing the core provider branch tree.
Quirks remain versioned and testable. Compatibility is bounded by each gateway's actual
behavior, so the adapter must surface unsupported features instead of approximating them.

## Verification

- Ignored live tests in `crates/aim-llm-openai/tests/provider.rs` make real provider calls
  (`cargo test -p aim-llm-openai -- --ignored live_ --test-threads=1`):
  `live_openrouter_catalog`, `live_openrouter_text_turn`, `live_openrouter_tool_turn`,
  `live_openrouter_reasoning_tool_turn`, `live_ai_gateway_text_turn` (the 16-token boundary and
  billed `gateway_cost`), `live_ai_gateway_tool_turn`, `live_ai_gateway_reasoning_tool_turn`,
  `live_openrouter_tool_image_gating` (a provider that never listed its catalog sends text-only
  `openai/gpt-oss-20b` the placeholder and vision `openai/gpt-4.1-mini` the image; the image
  forced onto the text-only model is rejected with 404 "No endpoints found that support image
  input").
  The first version of this ADR named them `live_openrouter_chat` and `live_ai_gateway_chat`.
- Unit and local-HTTP tests in the crate cover explicit wire selection, key-env lookup without
  logging, quirk application, unknown-key rejection, verbatim stream captures of both gateways
  and error mapping. Preset override precedence is untested because merging is not implemented.
