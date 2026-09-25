# ADR 0042: Bound media output and require private-session opt-in

- Status: Proposed
- Date: 2026-09-25
- Supersedes: 0029
- Baseline: 0009, 0013, 0022, 0029
- Scope: Standalone Codex search and image tools, their model-facing output and private-session access; the `media.transcribe` daemon contract remains as in ADR 0029.

## Context

ADR 0029 established Codex media services and explicit retention consent for private audio. The cross-model review in `scratchpad/reviews/REV9-jev-media.md` found that a generated image of up to 25 MiB could exceed the harness's 16 MiB JSON-RPC frame after base64 encoding. It also observed that image tool content is retained in the transcript and sent on every later model request, and that a second generation at the same path spent quota before `fs.write` rejected the collision. Standalone search rejected completed answers without URL annotations. The live probes in `docs/research/live-probes.md` establish the endpoints and observed audio retention; image retention remains unverified.

The same review found that ephemeral sessions currently offer media tools with a model-facing disclosure only. Session persistence and consent are known to the host, while the dispatcher receives neither. This ADR remains proposed until the host can pass an explicit per-session decision without a global setting.

## Decision

- Media model IDs come from runtime configuration: `AIM_CODEX_SEARCH_MODEL` and `AIM_CODEX_IMAGE_MODEL`. An unset or empty value disables that capability. The service does not embed model IDs as defaults.
- A completed hosted search may return no citations. Its tool result marks `uncited: true`; unknown hosted output items are ignored with a debug record. Callers must not present an uncited answer as sourced.
- A generated image is limited to 11 MiB decoded so its base64 payload fits the current 16 MiB harness frame with room for the JSON envelope. The tool result records the workspace path and media type as text, not image bytes in the persistent transcript. The model may use the workspace `Read` tool to inspect the image when needed.
- `fs.write` retains `IfAbsent`. A path collision after generation saves to a fresh path with a fresh idempotency key, never overwriting the old file. Other write failures tell the model that generation succeeded but the image was not saved.
- Private and ephemeral sessions should omit media tools unless the user explicitly opts in to sending prompts or queries to the external service. `Dispatcher::with_policy` accepts that access decision. The host must supply the session's persistence and opt-in state before this paragraph is enforced. Until then, its existing `Dispatcher::new` call retains ADR 0029's behavior; that gap is part of why this ADR is proposed.
- Before paid generation, the workspace should check or reserve the target under the same authority as `fs.write`. The current `ToolHost` has only `write_blob` and cannot make that atomic preflight. A host/tool API change is required; this branch checks invalid path syntax early and guarantees collision fallback after generation.

## Consequences

Image bytes no longer inflate later model requests, daemon notifications or stored transcripts. An image may require one later workspace read when the model needs pixels. Unconfigured services are unavailable until the runtime environment supplies model IDs. A filesystem failure can still waste one generation until the preflight API is wired. The private-session gate must be completed before this ADR can replace ADR 0029.

## Verification

`crates/aim-llm-codex/src/media/tests.rs` covers uncited and unknown-item search, configured model IDs and image bounds. `crates/aim/src/media/dispatcher.rs` tests path-only results, an oversized fake image, collision fallback after one generation, and policy-disabled tools. W15 and FIX12 live service smokes are reported in their scratchpad reports. A host-level private-session opt-in test and an atomic writeability preflight test remain required before acceptance.
