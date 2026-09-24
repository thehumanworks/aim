# Live service probes (2026-09-25)

Evidence from real calls made with the maintainer's credentials (live smoke calls are a project
requirement). Probe scripts live outside the repo. No secrets were printed or stored. These
results **supersede** any `UNVERIFIED` item in [`codex-backend.md`](codex-backend.md) that they
cover.

## ChatGPT codex backend (`https://chatgpt.com/backend-api/codex`)

| Probe | Result |
| --- | --- |
| `GET /models?client_version=0.158.0` | 200, ETag present. Listed: `gpt-6-astra`, `gpt-6-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-daybreak-blue-latest`, `gpt-5.5`; hidden: `gpt-reserve`, `codex-auto-review`. All report `context_window: 272000`, `auto_compact_token_limit: null`. |
| Reasoning efforts | Catalog-driven and **wider than codex's UI**: `low, medium, high, xhigh, max, ultra` (luna/5.6-luna/reserve stop at `max`; `gpt-5.5` at `xhigh`). aim's effort ladder must come from the catalog, never a hardcoded enum. |
| Service tiers | `priority` ("Fast", 1.5×–2× speed, some "increased usage"). |
| Access-token JWT | `https://api.openai.com/auth` claim keys: `amr, chatgpt_account_id, chatgpt_account_user_id, chatgpt_compute_residency, chatgpt_plan_type, chatgpt_user_id, localhost, poid, user_id`. Plan `pro`. Access token lifetime ≈ 7 days. |
| Full tool turn, `gpt-6-luna`, effort `low` | Turn 1 (function call) 1.27 s total, first delta 1.03 s; turn 2 (replay + `function_call_output`) 2.46 s, first delta 0.83 s. A **custom one-line `instructions`** string was accepted — the backend does not require codex's prompt. |
| SSE events seen | `response.created`, `response.in_progress`, `response.output_item.added/done`, `response.function_call_arguments.delta/done` (argument streaming exists — usable for live tool cards), `response.content_part.added/done`, `response.output_text.delta/done`, `response.output_text.annotation.added`, `response.web_search_call.in_progress/searching/completed`, `response.completed`. Message items carry a `phase` field. |
| Usage | `input_tokens`, `input_tokens_details.{cached_tokens, cache_write_tokens}`, `output_tokens`, `output_tokens_details.reasoning_tokens`, `total_tokens`, plus **`attribution`**: per-item and per-request-field token accounting (e.g. `request_fields.tools.input_tokens = 119`, `request_fields.instructions.input_tokens = 15`). This is exact, free telemetry for token-efficiency work. |
| Response headers | `x-codex-active-limit`, `x-codex-plan-type`, `x-codex-primary-{used-percent,window-minutes,reset-at,reset-after-seconds}`, `x-codex-secondary-{…}`, `x-codex-primary-over-secondary-limit-percent`, `x-codex-credits-{balance,has-credits,unlimited}`, `x-codex-turn-state` (sticky routing token to echo back). |
| Dictation `POST https://chatgpt.com/backend-api/transcribe` (multipart `file=audio.wav`, 24 kHz mono PCM16) | 200 in 0.48 s, exact transcript. Response: `{text, asset_pointer: "sediment://file_…", asset_ttl: "30d", asset_format: "wav"}` — **the audio is retained server-side for 30 days**; private/ephemeral modes must disclose this. |
| Standalone hosted web search (`/responses`, `tools:[{type:web_search, external_web_access:true}]`, `tool_choice:required`, effort low) | 200 in ~12 s. `web_search_call` item exposes `action.queries[]`; answer text carries `url_citation` annotations `{url, title, start_index, end_index}`. Usable as a harness-wide tool. |
| Image generation `POST /backend-api/codex/images/generations` (`gpt-image-2.5-sunburst`, quality low, 1024²) | 200 in 31.6 s. Response keys `created, background, data[{b64_json, generation_id}], output_format, quality, size, usage{input/output tokens incl. image_tokens}`. |

## TypeSafe Jev (`https://api.typesafe.ai/v1/systemone`)

- One request with a `score` (5 levels) and a `noul` question: **0.60 s**, model `jev-1.13.0`,
  364 input / 35 output tokens. Score answer: `{score: 0.45, confidence: 0.63, legend, probabilities}`;
  noul answer `{noul: 0.16}`. Latency is compatible with per-step decisions if computed off the
  critical path (e.g. during tool execution).

## OpenAI-compatible gateways

| Gateway | Result |
| --- | --- |
| OpenRouter `https://openrouter.ai/api/v1/chat/completions` | 200 in 0.81 s. Usage includes `cost`, `cost_details`, `prompt_tokens_details.{cached_tokens, cache_write_tokens}`, `completion_tokens_details.reasoning_tokens`. |
| Vercel AI Gateway `https://ai-gateway.vercel.sh/v1/chat/completions` | 200 (0.81 s gpt-4.1-mini, 1.73 s claude-sonnet-4.5) **only with `max_tokens ≥ 16`** — it maps to an upstream Responses call whose `max_output_tokens` minimum is 16 (400 otherwise). Usage adds `market_cost, surcharge_cost, gateway_cost, zero_data_retention_cost, cache_creation_input_tokens`. `GET /v1/models` lists 390 models incl. `anthropic/claude-opus-5.5`, `anthropic/claude-fable-5.1`, `google/gemini-3.x`. |

## Design consequences

1. Model capabilities (efforts, tiers, context window) are **data from catalogs**, cached with ETag
   and refreshed; the Jev effort controller maps onto whatever ladder the selected model exposes.
2. Provider adapters normalise quirks as data (e.g. `min_output_tokens = 16` for AI Gateway) rather
   than code branches.
3. Usage records keep provider-native detail (attribution, cost breakdowns) next to the normalised
   counters; the self-optimisation loop consumes the attribution data.
4. Media services (transcribe, images, search) are independent harness services with their own
   latency budgets (0.5 s, ~30 s, ~12 s) and disclosure rules (30-day audio retention).
