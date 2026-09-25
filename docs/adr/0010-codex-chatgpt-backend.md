# ADR 0010: Implement the ChatGPT Codex backend narrowly

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0007
- Scope: ChatGPT subscription auth, Responses transport and Codex media services.

## Context

The Codex backend uses OAuth, a product-specific Responses endpoint and a live model catalog
(research/codex-backend.md, §§1–8). Its internal Rust crates are 0.0.0 workspace packages,
mostly unpublished; importing their agent loop would couple aim to an unstable internal API
(research/codex-backend.md, §12, lines 192–205). Authenticated probes found catalog efforts
from `low` through `ultra`, service tiers, token attribution and usable media endpoints
(research/live-probes.md, ChatGPT codex backend, lines 8–22).

## Decision

- Implement aim's own browser authorization-code grant with PKCE on loopback port 1455,
  falling back to 1457, and a cancellable device-code flow with the server polling interval.
  Verify OAuth state and exchange the code; never use an API key as a subscription token.
- Store aim's credentials in the OS keyring, with a 0600-file fallback. Read
  `~/.codex/auth.json` only as a fallback source and never refresh or rewrite its token:
  rotating a borrowed refresh token can log out Codex. One daemon refresher lease handles
  aim-owned tokens, so concurrent sessions do not race rotation.
- Implement `POST https://chatgpt.com/backend-api/codex/responses` over HTTP SSE first.
  Send `store:false`, a stable instruction prefix, native ordered input, and encrypted
  reasoning inclusion. Retain raw provider items and call IDs alongside normalized events;
  dispatch a tool only after its output item is complete. A stream without a terminal
  `response.completed` is not success (research/codex-backend.md, §§4–5).
- Add WebSocket Responses and `previous_response_id` elision only after an identical-workload
  benchmark proves benefit; use the HTTP path as the initial contract. Use V2 compaction by
  streaming a `compaction_trigger` and retaining its opaque encrypted `compaction` item.
  Failed compaction leaves the lossless history intact (research/codex-backend.md, §§6–7).
- Fetch `GET /models?client_version=…` with ETag. Model slugs, effort ladders, service tiers,
  context limits and `tool_mode` come from the catalog. Catalog listing is distinct from
  turn acceptance. Capture rate-limit windows, credits, `x-codex-turn-state` affinity and
  per-field `usage.attribution` (research/live-probes.md, lines 12–19).
- Offer provider-independent services: `/dictate` calls `POST /backend-api/transcribe` and
  discloses its observed 30-day audio retention; images use the separate Codex image endpoint;
  standalone web search is a one-turn Responses `web_search` call with citation annotations.
  Keep these separate from the selected conversation provider.

## Consequences

The adapter is smaller and can preserve Codex-native replay without importing Codex's own
agent loop. The product-specific service surface may change, so every endpoint needs live
contract checks and sanitized errors. Credentials and raw reasoning require careful storage.

## Verification

- M2-llm ignored live tests `live_codex_oauth_browser`, `live_codex_oauth_device`,
  `live_codex_responses_tool_turn`, and `live_codex_catalog` are required.
- M8 ignored live tests `live_codex_transcribe`, `live_codex_images` and
  `live_codex_web_search` exercise separate service adapters; redact credential values.
- Before enabling WS, benchmark the same turns over SSE and WS for latency, bytes and
  cache behavior; retain SSE until that evidence exists.
