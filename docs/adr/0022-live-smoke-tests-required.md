# ADR 0022: Require live smoke evidence for integrations

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0006, 0010, 0011, 0012
- Scope: External provider, protocol, service, and SSH integration acceptance; not pure kernel logic.

## Context

Mocks verify parsers and deterministic behavior but cannot establish current provider access,
OAuth rotation, remote tool authority, or service retention behavior. The 2026-09-25 live probes
found catalog efforts through `ultra`, provider `usage.attribution`, gateway output-token minima,
and dictation's 30-day audio retention (`docs/research/live-probes.md`, §§ChatGPT codex backend,
OpenAI-compatible gateways, lines 8–36). Source-only uncertainty about attribution was settled
by a real response (`docs/research/self-improvement.md`, TL;DR, lines 8–12).

## Decision

Every external integration has a real, bounded smoke call in `mise run smoke`. Name ignored
Rust tests `live_*` so ordinary offline checks can run without credentials, while the smoke
task explicitly selects and executes them. A green fixture/mock suite is additional evidence,
never a substitute for a required live call (`docs/architecture.md`, §§1, 13, lines 41–43,
678–680; §15, lines 719–721).

Exercise the actual user-facing boundary: login and one streamed turn for each provider,
ACP capability and private-session probes, MCP era negotiation, SSH remote-change isolation,
and media/search calls when those services ship. For failures, report the exact service,
entitlement, network, or credential gap rather than marking the integration complete.

Keep credentials in environment/keyring, redact request/response fixtures, and never print
tokens, OAuth material, audio payloads, or other secrets. Bind a smoke result to exact source,
toolchain, adapter, and service response metadata without persisting credential values.

## Consequences

Acceptance requires live access and can expose upstream drift; offline CI still covers stable
contracts. Tny's standing gates required explicit authorization for live inference
(`docs/research/tny-lessons.md`, §Lessons, lines 305–309). Aim instead makes an actual live
smoke call a required acceptance gate for every integration; jevgrep-style local proof and
test evidence cannot replace that service-level check.

## Verification

At each integration milestone, `mise run smoke` must report the selected `live_*` test names,
their actual execution, and pass/fail/skip reasons. M2a requires a real codex tool turn; M2b
requires local and SSH turns for all three launch backends. CI fixture tests remain separate.
