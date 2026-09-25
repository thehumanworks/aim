# ADR 0002: Keep two applications and a separate gate

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0001
- Scope: Process ownership and crate boundaries, not the full protocol payloads.

## Context

The brief requires the execution layer and agent layer to work independently, including local
and remote harness use (`docs/vision.md`, Requirements). A single process would couple tool
execution to LLM credentials, UI and conversations. Conversely, splitting every subsystem into
its own crate before it earns an independent contract would multiply integration cost.

`docs/architecture.md`, §§2–3, defines the processes and the deliberate crate rule: separate
deployable apps, shared contracts and dependency-heavy adapters have boundaries; other concerns
begin as modules. Its §10.1 explains why the evolution gate cannot be just another agent module.

## Decision

Build `aimx` as the execution application: workspaces, fs, exec, PTY, search, watch, tools,
authentication and enforcement. It holds no conversations or LLM credentials. It can serve
direct clients locally or remotely, and expose its tools through `aimx mcp`.

Build `aim` as the agent application: daemon, headless CLI, TUI and web server, sessions,
providers, credentials, memory, swarm, plugins and evolution controller. It uses `aimx` for
workspace effects, yet `aimx` remains usable without `aim` (`docs/architecture.md`, §2
Applications and processes).

Build `aim-gate` as a separate trusted service with its own OS identity or sandbox and protected
credentials. It evaluates and promotes candidates; candidate code never runs with its identity
(`docs/architecture.md`, §10.1 Trust boundary). The controller in `aim` requests gate actions
but is not the authority for acceptance.

Keep `aim-kernel`, `aim-proto` and `aim-rpc` as shared boundaries. Keep provider, ACP and heavy
adapter crates where dependencies or an independent owner justify them. Start agent features as
modules in `aim`, splitting only when necessary (`docs/architecture.md`, §3 Crate map).

## Consequences

Standalone harness clients can operate without an agent daemon. Credentials and conversation
state stay outside `aimx`; `aimx` must nevertheless enforce requests itself. The gate adds an
operational process and identity, justified by its authority over self-modification.

## Verification

M1a's standalone `aimx` conformance suite and M2a's headless `aim run` on local `aimx` are
planned integration checks (`docs/architecture.md`, §15 Milestones). Gate isolation needs the
candidate-sandbox adversarial test planned for M10; a Rust crate split alone does not establish
an OS trust boundary (`docs/architecture.md`, §§10.1, 13).
