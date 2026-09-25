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

- Configure named profiles with `{base_url, key_env, wire, quirks}`. `wire` selects `chat`
  or `responses`; do not infer it from a model name. Reference an environment variable by
  name, never save its value in project configuration or session events.
- Ship presets for OpenRouter and Vercel AI Gateway, while allowing user-defined endpoints.
  A preset supplies defaults and an explicit user profile can override them.
- Express provider differences as profile data interpreted by the adapter, not branches
  keyed to provider names. Vercel AI Gateway sets `min_output_tokens = 16`; reject or raise
  a smaller requested limit before sending the request, and report the effective limit.
- Preserve provider-specific usage as a sidecar to normalized usage. Do not flatten cost,
  cache-write and surcharge fields into a value whose semantics differ by gateway.
- Derive available models and caps from endpoint catalogs when provided; a configured slug
  remains explicit when discovery is absent. A listed model still needs a live turn before
  aim claims it is usable.

## Consequences

New compatible gateways can be added by data without growing the core provider branch tree.
Quirks remain versioned and testable. Compatibility is bounded by each gateway's actual
behavior, so the adapter must surface unsupported features instead of approximating them.

## Verification

- M2-llm ignored tests `live_openrouter_chat` and `live_ai_gateway_chat` require real
  provider calls, including the 16-token boundary and usage fields.
- Profile contract tests cover explicit wire selection, key-env lookup without logging,
  preset override precedence and quirk application.
