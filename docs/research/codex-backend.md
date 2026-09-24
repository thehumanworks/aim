# ChatGPT Codex Responses backend: implementation research

Scope: local shallow reference snapshots reviewed 2026-09-25. `refs/...:line` paths are relative to the supplied scratchpad reference root. This is a source-derived implementation contract, not a successful authenticated service test. No inference request was sent and no credential value was read or printed. The ChatGPT subscription backend is a product-specific surface; ordinary Platform Responses documentation does not establish its entitlements.

## TL;DR

- **FACT:** Browser login uses `https://auth.openai.com` OAuth authorization code with PKCE and a loopback callback; device login has a separate user-code/poll phase, then the same code exchange. `refs/codex/codex-rs/login/src/server.rs:76-79,584-615,795-827`; `refs/codex/codex-rs/login/src/device_code_auth.rs:165-214`.
- **FACT:** Existing Codex credentials have a documented JSON shape and refresh-token rotation. Import rather than copy token strings into project state. `refs/codex/codex-rs/login/src/auth/storage.rs:39-65`; `refs/codex/codex-rs/login/src/auth/manager.rs:1598-1648`.
- **FACT:** The ChatGPT backend uses `/backend-api/codex/responses`, SSE over HTTP POST or WebSocket `response.create`; the latter supports `previous_response_id` input elision. `refs/codex/codex-rs/model-provider-info/src/lib.rs:74-78`; `refs/codex/codex-rs/codex-api/src/endpoint/responses.rs:135-156`; `refs/codex/codex-rs/core/src/client.rs:1988-2022`.
- **FACT:** Codex's request serializes `store:false`, `stream:true`, full `input`, `instructions`, `include:["reasoning.encrypted_content"]`, cache key, tools, and reasoning. `refs/codex/codex-rs/core/src/client.rs:962-1015`.
- **FACT:** Keep raw reasoning/encrypted content and hosted tool items in replay history; tool call outputs reference `call_id`. `refs/tny/src/backends/openai/responses.cpp:73-145`.
- **FACT:** HTTP SSE completion requires a terminal event; the final `response.completed` holds usage. Function/custom tool arguments can be finalized from `response.output_item.done`. `refs/codex/codex-rs/codex-api/src/sse/responses.rs:117-166,354-549`.
- **FACT:** WebSocket has an `OpenAI-Beta: responses_websockets=2026-02-06` handshake and HTTP fallback on 426. No controlled latency/byte advantage was measured in the references. `refs/codex/codex-rs/core/src/client.rs:1307-1341,1922-1925`.
- **FACT:** ChatGPT Codex compaction V2 is a normal Responses stream with a trailing `compaction_trigger` item and a returned encrypted `compaction` item. Do not assume `/responses/compact` works on this backend. `refs/codex/codex-rs/core/src/compact_remote_v2_attempt.rs:69-87`; `refs/oh-my-pi/packages/agent/src/compaction/openai.ts:287-298`.
- **FACT:** Model discovery is `GET /models?client_version=...` with ETag; catalog visibility and usable model entitlement are distinct. `refs/codex/codex-rs/codex-api/src/endpoint/models.rs:33-45,69-90`; `refs/tny/docs/adr/0170-codex-catalog-discovery-version.md`.
- **FACT:** tny uses separate ChatGPT endpoints for batch dictation and image generation, and a one-turn hosted `web_search` call for provider-independent search. `refs/tny/src/core/dictation_codex.c:18-35`; `refs/tny/src/core/image_codex.c:50-111`; `refs/tny/src/core/search_codex.c:242-277`.
- **FACT:** Codex workspace crates use Apache-2.0 but version `0.0.0`; most relevant names are not published on crates.io, and the published `codex-protocol` name belongs to another repository. `refs/codex/codex-rs/Cargo.toml:158-165`; crates.io API read 2026-09-25.
- **RECOMMENDATION:** Build a typed, provider-specific Codex adapter around small internal request/event types. Keep auth, transport, hosted tools, and transcript replay separate.

## Findings

### 1. Browser OAuth login

- **FACT:** Default issuer is `https://auth.openai.com`. Browser callback binds `127.0.0.1:1455/auth/callback`, with 1457 fallback in current source. `refs/codex/codex-rs/login/src/server.rs:76-79,193-201`.
- **FACT:** Client ID is `app_EMoamEEZ73f0CkXaXp7hrann`. `refs/codex/codex-rs/login/src/auth/manager.rs:1716-1729`.
- **FACT:** Authorization URL is issuer `/oauth/authorize`. Query has `response_type=code`, `client_id`, `redirect_uri`, `scope`, `code_challenge`, `code_challenge_method=S256`, `state`; Codex also supplies `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, and `originator`. Scopes are `openid profile email offline_access api.connectors.read api.connectors.invoke`. `refs/codex/codex-rs/login/src/server.rs:584-615`.
- **FACT:** Verify returned state and exchange the code at issuer `/oauth/token` using authorization-code grant, client ID, redirect URI, and PKCE verifier. `refs/codex/codex-rs/login/src/server.rs:709-717,795-827`.
- **FACT:** Account ID is derived from `chatgpt_account_id` in the `https://api.openai.com/auth` JWT claim, checking access and ID token claims. Codex classifies plan from the ID token's `chatgpt_plan_type`. `refs/tny/src/core/codex_auth.c:124-133,166-186`; `refs/codex/codex-rs/login/src/auth/manager.rs:675-694`.
- **UNVERIFIED:** No source evidence established that the server mandates a particular `instructions` string. Codex sends its own instructions; avoid treating local prompt text as a backend authentication requirement.
- **UNVERIFIED:** Any API-key token exchange as part of *normal* ChatGPT login is not established by these sources. Preserve separate API-key auth mode instead of silently converting the subscription token.

### 2. Device-code login

- **FACT:** POST `https://auth.openai.com/api/accounts/deviceauth/usercode` with JSON `{ "client_id": "..." }`; response includes `device_auth_id`, `user_code`, and interval (source deserializes interval as numeric string). `refs/codex/codex-rs/login/src/device_code_auth.rs:19-52,62-96,165-179`.
- **FACT:** Display `https://auth.openai.com/codex/device` and the user code. Poll POST `/api/accounts/deviceauth/token` with `{ "device_auth_id": "...", "user_code": "..." }` at the supplied interval, for up to 15 minutes. 403/404 during poll mean pending rather than a finished login. `refs/codex/codex-rs/login/src/device_code_auth.rs:99-156,160-179`.
- **FACT:** Successful poll returns `authorization_code`, `code_challenge`, `code_verifier`; exchange using redirect URI `https://auth.openai.com/deviceauth/callback`. `refs/codex/codex-rs/login/src/device_code_auth.rs:55-60,182-214`.
- **RECOMMENDATION:** Keep device UX cancellable, retain the server's interval, and distinguish pending, expiry, denial, network failure, and exchange failure. Do not shorten polling based on UI impatience.

### 3. Credential storage, import, refresh

- **FACT:** `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) fields include `auth_mode`, `OPENAI_API_KEY`, `tokens`, `last_refresh`, and newer optional identity/PAT/Bedrock fields. `refs/codex/codex-rs/login/src/auth/storage.rs:39-65`.
- **FACT:** tny credential precedence is explicit CLI token/account ID, environment `CHATGPT_ACCESS_TOKEN`/`CHATGPT_ACCOUNT_ID`, `~/.tny/codex-auth.json`, then `$CODEX_HOME/auth.json`. The tny file supports `auth_mode`, `OPENAI_API_KEY`, `tokens`, `last_refresh`, `expires_at`. `refs/tny/src/core/codex_auth.c:1-33,64-87,189-205`.
- **FACT:** Codex refreshes at `/oauth/token` with JSON `grant_type=refresh_token`, `client_id`, `refresh_token`; optional returned ID/access/refresh tokens replace old values, supporting rotation. It checks JWT expiry with a five-minute margin and otherwise uses a `last_refresh` fallback interval. `refs/codex/codex-rs/login/src/auth/manager.rs:1598-1648,3004-3025`.
- **RECOMMENDATION:** Import by reading the existing path with user permissions; retain token provenance and never log token contents. Write a separate aim credential store only when aim itself performs login. Coordinate refresh so concurrent workers cannot overwrite a newly rotated refresh token.
- **UNVERIFIED:** pi/oh-my-pi import precedence was not traced exhaustively in this pass; do not infer they share tny's priority order.

### 4. Responses transport, headers and body

- **FACT:** Codex's explicit ChatGPT base is `https://chatgpt.com/backend-api/codex`, then POST `/responses`; HTTP sets `Accept: text/event-stream`. `refs/codex/codex-rs/model-provider-info/src/lib.rs:74-78`; `refs/codex/codex-rs/codex-api/src/endpoint/responses.rs:135-156`.
- **FACT:** Authorization is bearer access token, with ChatGPT account header when available. Default client headers include `originator` and `User-Agent`; `session-id`, `thread-id`, and `x-client-request-id` identify transport context. Optional headers include `x-codex-beta-features`, sticky `x-codex-turn-state`, `x-codex-routing-hint`, installation/window/parent metadata and attestation. Treat these as capability/routing context, not fabricated mandatory values. `refs/codex/codex-rs/login/src/auth/default_client.rs:456-469`; `refs/codex/codex-rs/codex-api/src/requests/headers.rs:5-14`; `refs/codex/codex-rs/core/src/client.rs:160-172,1307-1341,2308-2331`.
- **FACT:** WebSocket handshake adds `OpenAI-Beta: responses_websockets=2026-02-06`. `refs/codex/codex-rs/core/src/client.rs:1331-1334`.
- **FACT:** Codex request type includes `model`, `instructions`, `input`, `tools`, `tool_choice`, `parallel_tool_calls`, `reasoning`, `store`, `stream`, `stream_options`, `include`, `service_tier`, `prompt_cache_key`, `text`, `client_metadata`, `access_programs`. `refs/codex/codex-rs/codex-api/src/common.rs:277-303`.
- **FACT:** Core builder chooses `tool_choice:"auto"`, `store:false`, `stream:true`, `include:["reasoning.encrypted_content"]`, optional `text` verbosity and `service_tier`, plus `prompt_cache_key`. Responses Lite disables parallel calls when tools are present. `refs/codex/codex-rs/core/src/client.rs:962-1015`; `refs/oh-my-pi/docs/provider-endpoint-constraints.md:284-296`.
- **FACT:** Function tools use `type:"function"` with name/description/JSON parameters; custom freeform grammar tools can be used for `apply_patch`. tny defaults function `strict:false`. `refs/tny/src/backends/openai/responses.cpp:169-204`; `refs/oh-my-pi/docs/provider-endpoint-constraints.md:284-296`.
- **FACT:** Codex's serialized request does not include `max_output_tokens` in this builder. oh-my-pi's Codex transport notes the backend rejects caller caps. `refs/codex/codex-rs/core/src/client.rs:999-1015`; `refs/oh-my-pi/docs/provider-endpoint-constraints.md:72-88`.
- **UNVERIFIED:** A single exhaustive server-enforced header set cannot be proven from client code; clients may send extra compatibility headers and backend checks can change. The minimal example below is a source-aligned starting shape, not a live conformance result.

Request-field decisions for a first Rust adapter (`refs/codex/codex-rs/codex-api/src/common.rs:277-303`; `refs/codex/codex-rs/core/src/client.rs:962-1015`):

| Field | Source behavior | aim treatment |
| --- | --- | --- |
| `model` | Catalog slug | Require discovered or explicitly configured slug. |
| `instructions` | Codex-built string | Send a stable, explicit string; test empty separately. |
| `input` | Native response items | Retain exact provider item order. |
| `tools` | Optional schema array | Keep serializable provider-specific tool descriptors. |
| `tool_choice` | `auto` in Codex core | Default `auto`; standalone search uses `required`. |
| `parallel_tool_calls` | Disabled for Responses Lite | Gate by model mode and tool executor capacity. |
| `reasoning` | Effort/summary | Clamp to catalog-supported effort. |
| `text` | Optional verbosity/schema controls | Emit only when supported. |
| `store` | `false` | Replay input locally; do not depend on server persistence. |
| `stream` | `true` | Require terminal completion. |
| `include` | Encrypted reasoning | Preserve opaque response items. |
| `prompt_cache_key` | Stable session/workspace key | Avoid changing for incidental turns. |
| `service_tier` | Model/provider-dependent | Validate against catalog. |
| `client_metadata` | Optional string map | Keep sparse; do not put secrets in it. |
| `stream_options` | Optional summary delivery mode | Emit only on supported models. |
| `access_programs` | Optional | Omit until a concrete use case exists. |

Minimal ChatGPT session request (placeholder values; HTTP headers shown separately):

```http
POST https://chatgpt.com/backend-api/codex/responses
Authorization: Bearer <access-token>
ChatGPT-Account-Id: <account-id>
originator: aim
User-Agent: aim/<version>
session-id: <stable-session-id>
thread-id: <thread-id>
Accept: text/event-stream
Content-Type: application/json
```

```json
{
  "model": "<catalog-slug>",
  "instructions": "You are an assistant.",
  "input": [{"role":"user","content":[{"type":"input_text","text":"Hello"}]}],
  "tool_choice": "auto",
  "parallel_tool_calls": false,
  "reasoning": {"effort":"medium","summary":"auto"},
  "store": false,
  "stream": true,
  "include": ["reasoning.encrypted_content"],
  "prompt_cache_key": "<stable-workspace-key>"
}
```

More realistic request (tool schema and model-dependent fields illustrative, based on the same request struct):

```json
{
  "model": "<catalog-slug>",
  "instructions": "<stable system instructions and tool policy>",
  "input": [
    {"role":"user","content":[{"type":"input_text","text":"Inspect src/main.rs"}]},
    {"type":"reasoning","id":"<prior-id>","encrypted_content":"<opaque-prior-ciphertext>"},
    {"type":"function_call","name":"read_file","call_id":"call_1","arguments":"{\"path\":\"src/main.rs\"}"},
    {"type":"function_call_output","call_id":"call_1","output":"<bounded result>"}
  ],
  "tools": [
    {"type":"function","name":"read_file","description":"Read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},"strict":false},
    {"type":"custom","name":"apply_patch","description":"Apply a patch","format":{"type":"grammar","syntax":"lark","definition":"<supported patch grammar>"}}
  ],
  "tool_choice":"auto",
  "parallel_tool_calls":true,
  "reasoning":{"effort":"high","summary":"auto"},
  "text":{"verbosity":"medium"},
  "store":false,
  "stream":true,
  "include":["reasoning.encrypted_content"],
  "prompt_cache_key":"<stable-workspace-key>",
  "service_tier":"<catalog-supported-tier>"
}
```

For `apply_patch`, obtain the actual grammar from the selected Codex tool definition; the placeholder above is deliberately not a runnable grammar. Omit `text`, tier and tool fields when the selected model/catalog does not support them. `refs/codex/codex-rs/core/src/client.rs:970-1015`.

### 5. SSE parsing, usage, errors and rate limits

- **FACT:** Parse `response.created`, `response.output_item.added`, `response.output_item.done`, `response.output_text.delta`, `response.reasoning_summary_part.added`, `response.reasoning_summary_text.delta/done`, `response.reasoning_text.delta`, `response.custom_tool_call_input.delta`, `response.completed`, `response.failed`, `response.incomplete`, and error. This source ignores `response.function_call_arguments.delta/done`, relying on completed output items. `refs/codex/codex-rs/codex-api/src/sse/responses.rs:354-549`.
- **FACT:** Preserve output item IDs, order, call IDs, raw arguments, custom input, messages and hosted tool output. Finalize dispatch only after a complete item; an incomplete tool call must not execute. `refs/codex/codex-rs/codex-api/src/sse/responses.rs:354-415`; `refs/pi/packages/agent/src/agent-loop.ts:469-490`.
- **FACT:** Final usage contains `input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`, optional `cache_write_tokens`, `output_tokens_details.reasoning_tokens`, and total. This accounting is post-request, not an expiry timestamp. `refs/codex/codex-rs/codex-api/src/sse/responses.rs:117-166`.
- **FACT:** Rate-limit headers carry primary/secondary used percent, window minutes, reset time, plus credit fields; Codex parses these into user-facing limits. `refs/codex/codex-rs/codex-api/src/rate_limits.rs:57-101,195-230`.
- **FACT:** The legacy default prefix is `x-codex`; per-limit prefixes normalize the server's limit ID, so the parser constructs `x-<limit>-primary-used-percent`, `-primary-window-minutes`, `-primary-reset-at`, and corresponding `secondary` names. `refs/codex/codex-rs/codex-api/src/rate_limits.rs:53-80`.
- **RECOMMENDATION:** Store both raw limit ID and parsed window snapshot; do not collapse two simultaneous windows into a single percentage.
- **RECOMMENDATION:** Treat reset times as server observations and render them with local clock skew in mind.
- **FACT:** SSE terminal `response.failed`/`incomplete` map to errors, as do stream error events. Closing without terminal completion is not success. `refs/codex/codex-rs/codex-api/src/sse/responses.rs:418-549`; `refs/oh-my-pi/docs/provider-endpoint-constraints.md:58-70`.
- **RECOMMENDATION:** Classify HTTP authentication, throttle, transport timeout and model errors separately; retry only before any user-visible output unless a resume mechanism is proven. Persist raw event/item data for recovery without logging secrets or opaque reasoning.
- **UNVERIFIED:** Exact status-code taxonomy for every backend error and all `x-codex-*` fields was not exhaustively proven. Treat unknown error JSON/header fields as data, and preserve server message/code in diagnostics with sanitization.

### 6. WebSocket Responses

- **FACT:** Upgrade the provider's HTTPS `/responses` route to WSS, carrying auth/default/extra headers. Send JSON text frames of `type:"response.create"` with the normal request body; warmup can use `generate:false`. `refs/codex/codex-rs/codex-api/src/endpoint/responses_websocket.rs:390-415,495-511,891-923`; `refs/codex/codex-rs/codex-client/src/provider.rs:91-102`.
- **FACT:** The WS request type adds optional `previous_response_id` to the HTTP fields. On an incremental turn Codex can omit already acknowledged input and reference the prior response; HTTP SSE replays history. `refs/codex/codex-rs/codex-api/src/common.rs:305-359,384-390`; `refs/codex/codex-rs/core/src/client.rs:1988-2022`; `refs/oh-my-pi/docs/provider-endpoint-constraints.md:72-88`.
- **FACT:** Codex falls back to HTTP on WS 426. `refs/codex/codex-rs/core/src/client.rs:1922-1925`.
- **UNVERIFIED:** No measured latency, transmitted bytes, or cache savings were available for this snapshot. WS input elision is a structural benefit; its magnitude requires an identical-workload benchmark.

### 7. Remote compaction

- **FACT:** Current Codex V2 compaction appends `{ "type":"compaction_trigger" }` to a normal streamed Responses request containing the history, instructions and tools. It requires a completed response and exactly one encrypted `compaction` output item. `refs/codex/codex-rs/core/src/compact_remote_v2_attempt.rs:69-87`; `refs/codex/codex-rs/core/src/compact_remote_v2.rs:386-500`.
- **FACT:** oh-my-pi explicitly gates Codex away from V1 `/responses/compact` without a configured compatible endpoint; its V2 request uses `stream:true`, `store:false`, optional reasoning/include/cache key and tools. `refs/oh-my-pi/packages/agent/src/compaction/openai.ts:287-298`; `refs/oh-my-pi/packages/agent/src/compaction/compaction-v2-streaming.ts:191-224`.
- **FACT:** The returned `compaction` item carries opaque encrypted content and is replayed as a native input item with retained user context. The public Platform Responses object documents `{type:"compaction",id,encrypted_content}`; this does not by itself prove ChatGPT endpoint availability for every account. [OpenAI Responses streaming reference](https://platform.openai.com/docs/api-reference/responses-streaming/response/refusal?lang=python); `refs/oh-my-pi/docs/compaction.md:310`.
- **RECOMMENDATION:** Implement local compaction fallback and store encrypted compaction as opaque bytes. Activate remote V2 only after capability discovery or a safe test; never replace history on a failed/incomplete compaction stream.

### 8. Model catalog

- **FACT:** GET `/models?client_version=<version>` accepts ETag/cache identity. Model metadata includes slug, default/supported reasoning levels, service tiers, verbosity support/default, context window, auto-compact limit and compaction hash. `refs/codex/codex-rs/codex-api/src/endpoint/models.rs:33-45,69-90`; `refs/codex/codex-rs/protocol/src/openai_models.rs:402-471`.
- **FACT:** tny ADR 0170 records the client-version gate: models may be hidden when `client_version` is too old even if listed in another client's catalog. Its 2026-09-23 probe saw `gpt-6-sol` and `gpt-6-luna` only at `client_version>=0.155.0`, and distinguishes catalog visibility from turn acceptance. This is historical evidence, not a current model list. `refs/tny/docs/adr/0170-codex-catalog-discovery-version.md:8-17,34-51`.
- **UNVERIFIED:** Current account-specific model IDs, `xhigh`/`max` eligibility, fast tier, and turn-level acceptance. The report does not make an authenticated catalog call or spend quota. Query the authenticated catalog at runtime and validate a model with an actual turn before asserting availability.

### 9. Dictation

- **FACT:** tny's Codex batch transcription calls `POST https://chatgpt.com/backend-api/transcribe`, derived by removing `/codex` from the service base. It sends bearer token, ChatGPT account ID and originator. `refs/tny/src/core/dictation_codex.c:18-35`; `refs/tny/src/core/dictation_http.c:26-70`.
- **FACT:** Multipart part `file` is `audio.wav` with `audio/wav`; JSON response supplies `text`. tny records 24-kHz mono PCM16 WAV, 25-MiB audio limit and a 64-KiB transcript cap. `refs/tny/src/core/dictation_http.c:88-131`; `refs/tny/docs/adr/0079-provider-independent-dictation.md:15-45`.
- **FACT:** The ADR says this endpoint was derived from an older Codex tag and absent from a newer inspected implementation. `refs/tny/docs/adr/0079-provider-independent-dictation.md:18-24`.
- **UNVERIFIED:** Live entitlement and support on the current ChatGPT account. This is an internal, potentially unstable endpoint; gate it as an optional capability.

### 10. Image generation

- **FACT:** tny calls JSON `POST <service-base>/images/generations` or `/images/edits`, rather than a Responses `image_generation` hosted tool. Requests include bearer/account headers, `model`, `prompt`, `background:"auto"`, quality default `high`, size default `auto`; edits add `images:[{"image_url":"data:..."}]`. It reads `data[0].b64_json`. `refs/tny/src/core/image_codex.c:50-111,163-185`.
- **FACT:** tny's documented default is `gpt-image-2.5-sunburst`, quality high; this is a first-party user preference, not a guarantee of present entitlement. `refs/tny/docs/adr/0084-codex-image-defaults.md:13-28`.
- **UNVERIFIED:** Current model availability, partial image events, size/quality enumerations and server limits for this subscription. No source-backed ChatGPT `image_generation` Responses tool contract was established. Keep the service adapter separate from a future hosted-tool adapter.

### 11. Hosted web search

- **FACT:** tny's standalone call POSTs `/responses` with `stream:true`, `store:false`, explicit instructions, only the query as input, `tools:[{"type":"web_search","external_web_access":true}]`, `tool_choice:"required"`, `reasoning:{"effort":"low"}`. It sends bearer/account/beta/originator headers. `refs/tny/src/core/search_codex.c:242-277`.
- **FACT:** It accepts a completed `web_search_call` plus nonempty answer text and extracts `url_citation` annotations, coalescing output by item ID. `refs/tny/src/core/search_codex.c:56-125`; `refs/tny/docs/adr/0109-provider-independent-codex-search.md:17-56`.
- **FACT:** It deliberately omits prior response ID and conversation affinity for this standalone operation. Inline hosted search remains available to the Codex agent; other providers can call the isolated search tool. `refs/tny/docs/adr/0109-provider-independent-codex-search.md:30-40,68-77`.
- **FACT:** oh-my-pi's provider implementation uses `search_context_size:"high"` and rejects a response without a `web_search_call`. `refs/oh-my-pi/docs/tools/web_search.md:148-149`.
- **RECOMMENDATION:** Expose search as a harness capability returning answer, citation URL/title/span metadata and usage; record that it spent a model turn and that citations must be checked before user-facing claims.

### 12. Relevant openai/codex crates and reuse

- **FACT:** Workspace package version is `0.0.0`, license `Apache-2.0`. `refs/codex/codex-rs/Cargo.toml:158-165`.
- `codex-login`: OAuth/device/token storage and refresh. `refs/codex/codex-rs/login/Cargo.toml:1-12`.
- `codex-client`: HTTP provider session and SSE plumbing. `refs/codex/codex-rs/codex-client/Cargo.toml:1-12`.
- `codex-api`: typed Responses/models endpoints, request and SSE parsing. `refs/codex/codex-rs/codex-api/Cargo.toml:1-14`.
- `codex-websocket-client`: low-level WS transport. `refs/codex/codex-rs/websocket-client/Cargo.toml:1-12`.
- `codex-core`: agent loop, request assembly, compaction, hosted tool wiring. `refs/codex/codex-rs/core/Cargo.toml:1-14`.
- `codex-protocol`: internal model/events types. `refs/codex/codex-rs/protocol/Cargo.toml:1-14`.
- `codex-models-manager`/`codex-model-provider`: catalog and provider state. `refs/codex/codex-rs/models-manager/Cargo.toml:1-15`; `refs/codex/codex-rs/model-provider/Cargo.toml:1-14`.
- `codex-chatgpt`, `codex-realtime-webrtc`, `codex-voice-host`: ChatGPT services and realtime/voice implementation; they are larger than a batch transcription adapter. `refs/codex/codex-rs/chatgpt/Cargo.toml:1-12`; `refs/codex/codex-rs/realtime-webrtc/Cargo.toml:1-12`; `refs/codex/codex-rs/voice-host/Cargo.toml:1-15`.
- `codex-apply-patch`: parser/application for Codex patch tool. `refs/codex/codex-rs/apply-patch/Cargo.toml:1-17`.
- `codex-backend-client`: backend service client, explicitly `publish=false`. `refs/codex/codex-rs/backend-client/Cargo.toml:1-8`.
- **FACT:** Read-only crates.io API lookup on 2026-09-25 gave HTTP 404 for `codex-core`, `codex-login`, `codex-backend-client`, `codex-apply-patch`, `codex-client`, `codex-chatgpt`, `codex-models-manager`, `codex-realtime-webrtc`, `codex-websocket-client` and `codex-api`. `codex-protocol` exists at version 0.63.0 but its repository is `namastexlabs/codex`, so it is not this workspace crate. These names may change; check at adoption time.
- **RECOMMENDATION:** Reimplement the narrow transport/auth protocol behind aim-owned types. Vendoring a small parser is plausible, but direct dependency on `codex-core` pulls an internal `0.0.0` workspace and ties the harness execution layer to Codex's agent loop. Track upstream changes with source-driven contract tests.

### 13. Token and latency efficiency evidence

- **FACT:** tny computes a stable prompt-cache routing key from workspace/SSH/tool profile unless configured for session scope, and sends it with `store:false` and encrypted reasoning include. `refs/tny/src/backends/openai/openai.c:854-929`.
- **FACT:** tny replays native reasoning/hosted items and suppresses duplicate translated assistant text when raw hosted message exists. `refs/tny/src/backends/openai/responses.cpp:117-145`; `refs/tny/docs/adr/0106-native-web-search-and-duckduckgo.md:17-24`.
- **FACT:** tny bounds shell output, while oh-my-pi's archive serialization caps tool results and argument strings. These are specific truncation policies, not measured token savings. `refs/tny/src/core/tools_shell.c:322-345,416-420`; `refs/oh-my-pi/docs/compaction.md:201`.
- **FACT:** oh-my-pi can select a freeform `apply_patch` grammar; its default edit mode is hashline. Tool schema and edit format affect prompt size and model behavior, so there is no universal winning format in these sources. `refs/oh-my-pi/docs/tools/edit.md:24`; `refs/oh-my-pi/docs/settings.md:643-660`.
- **UNVERIFIED:** Numeric cache hit rates, model output fidelity, latency improvement over Codex/pi/oh-my-pi, or tool-format superiority. Source code alone cannot establish them. `cached_tokens` is an after-the-fact usage metric, not a cache expiration forecast.

### Full-turn state machine

```text
LOGIN/IMPORT -> REFRESH_IF_NEEDED -> SELECT_CATALOG_MODEL
  -> BUILD full replay input + stable instructions + current tools
  -> HTTP POST /responses or WS response.create
  -> consume created / added / deltas; accumulate by item id
  -> terminal completed? persist response id, usage, raw output items
       | failed/incomplete/error/early close -> classify; never dispatch incomplete call
       | completed without calls -> deliver assistant text; wait for user
       | completed with calls -> execute each complete function/custom call
            -> append raw assistant reasoning (incl encrypted_content)
            -> append hosted search/message/native output items in original order
            -> append each function_call_output/custom_tool_call_output by call_id
            -> next request: full replay (HTTP), or previous_response_id + new input (WS)
  -> if context threshold reached: V2 compaction_trigger stream
       -> verify one compaction item + completion; atomically replace replay history
```

The native reasoning and hosted-item replay requirement follows tny's transcript conversion and Codex's `store:false` builder. `refs/tny/src/backends/openai/responses.cpp:73-145`; `refs/codex/codex-rs/core/src/client.rs:999-1015`.

## Implications for aim

- **RECOMMENDATION:** Put OAuth, refresh and credential import in an auth service; a Codex protocol crate owns typed requests, SSE/WS events, catalog and replay; hosted search/dictation/image live behind harness tool interfaces. This preserves the requested execution/agent split and allows the tools to run without the agent loop.
- **RECOMMENDATION:** Start with HTTP SSE because its replay behavior is explicit. Add WS as a negotiated transport with response-ID state, fallback and reconnect tests. Benchmark exact repeated workloads before claiming token or latency gains.
- **RECOMMENDATION:** Store opaque reasoning/compaction items exactly, with encryption at rest and no diagnostic printing. Keep conversation state provider-neutral but preserve provider-native item envelopes in a sidecar.
- **RECOMMENDATION:** Discover catalog/capabilities per account and version. A visible model, `xhigh`, fast tier, image endpoint or dictation endpoint still needs a live acceptance check; degrade capability-by-capability.
- **RECOMMENDATION:** Make standalone hosted search a small, audited request that enforces a completed search call and citations. Expose native inline search separately to avoid polluting another provider's transcript.
- **RECOMMENDATION:** Track source-derived contract tests using captured, redacted fixtures for OAuth, SSE, WS, compaction and errors; add live smoke tests only with explicit quota consent.

## Open questions for the user

- Should aim share and refresh `$CODEX_HOME/auth.json` in place, or import once into aim's credential store? Sharing gives seamless login but creates a cross-process refresh-rotation race.
- Is one minimal authenticated smoke request acceptable before implementation to test the current account's catalog and hosted-tool entitlement? This report did not spend quota.

<!-- REPORT COMPLETE -->
