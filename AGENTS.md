# Instructions for agents working on aim

aim ("Agent I am") is a Rust harness for all agents, built and updated by the agents that use it.
Read [`docs/vision.md`](docs/vision.md) (the brief and the maintainer's decisions),
[`docs/architecture.md`](docs/architecture.md) (the design) and the ADR index in
[`docs/adr/`](docs/adr/README.md) before changing anything structural. Evidence behind the design
is in [`docs/research/`](docs/research/). This file is what an agent needs to know that those do
not say. It is itself a target of aim's self-improvement loop: keep it short and true.

## Setup and gates

- `mise install` — every tool is pinned in `mise.toml`/`mise.lock` (Rust 1.98.1, Verus, verusfmt).
  Never install tools another way (ADR 0003).
- `mise run check` — the quality gate: rustfmt + verusfmt check, clippy with `-D warnings`, tests,
  `cargo xtask check` (ADR numbering, kernel API rules, LOCKED digests). Must be green before any
  commit lands on `main`.
- `mise run verify` — Verus proofs of `aim-kernel` with `--no-cheating`. Must be green whenever the
  kernel changes. `mise run verify:fast` is the edit loop.
- `mise run smoke` — live smoke tests (`#[ignore]` tests named `live_*`) against real services with
  the maintainer's credentials. They are **required** evidence for every integration (ADR 0022);
  mocks and fixtures are additional, never a substitute. Never print a token, key or secret.

## Rules that are easy to get wrong

- **Proofs over example tests.** Pure decision logic (state machines, policies, planners, budgets,
  arithmetic, negotiation) goes in `crates/aim-kernel` as a spec + proof (ADR 0005): no public exec
  fn with `requires`, no `assume`/`admit`/`external_body`, no floats, total APIs returning
  `Result`/`Option`. Example tests are for I/O and integration; prefer property/model tests there.
- **LOCKED is protected.** A spec marked `LOCKED(ADR-NNNN)` and `crates/aim-kernel/LOCKED.toml`
  must not be edited by agents. Changing a locked decision needs the maintainer and a superseding
  ADR. You may change proofs and implementations freely as long as `mise run verify` stays green.
- **OS access only in aimx's backends.** Under `crates/aimx/src`, only `workspace/local`, `ssh`
  and `server` (and the binary entry point) may use `std::fs`/`std::process`/`tokio::fs`/
  `tokio::process` — `cargo xtask check` enforces it. Tools reach files and processes through the
  `Workspace` trait so every call can be shadowed over SSH. In the agent layer, project resources
  (`AGENTS.md`, `.agents/`) are read through the workspace too.
- **Lint exceptions** use `#[expect(lint, reason = "…")]`, never `#[allow]` (ADR 0004).
- **Contracts change with their ADR.** A change to a protocol type in `aim-proto`, a public trait,
  a persisted format, a policy default or a verified invariant lands together with a new or
  superseding ADR in the same commit (`docs/adr/TEMPLATE.md`; numbers must be unique).
- **Capabilities are data.** Model ids, effort ladders, tiers and provider quirks come from catalogs
  and config, never hardcoded enums.
- **Cite evidence.** Docs and ADRs state facts with sources (research reports, live probes,
  benchmarks, proofs); mark anything unverified as such.

## Working in parallel

- Each worker gets a git worktree on its own branch, `agent/<worker>/<topic>`, and stays inside the
  paths its task names. Rebase on `main` before handing off; keep commits focused.
- Cross-model review: code written by a Claude agent is reviewed by a codex agent and vice versa
  before it merges into `main`.
- The lead merges into `main` (the evolution gate will take over this role, ADR 0020).

## Commits and pushes

In this repository agents **commit and push** their own branch as work progresses, and the lead
pushes `main` to `origin` (the private `thehumanworks/aim`). This differs from the maintainer's
other repositories, where commits wait to be asked for. Anything outward-facing beyond that —
making the repo public, releases, publishing crates, touching other repositories — needs the
maintainer's explicit request. Commit messages: imperative subject, a body that says why, and the
co-author trailer of the agent that wrote the change.

## Layout

```
crates/aim-kernel   Verus-verified decisions (vstd only, no_std)
crates/aim-proto    wire + durable types (serde/schemars, no I/O, wasm-safe)
crates/aim-rpc      JSON-RPC peer and transports
crates/aimx         execution layer (workspace backends, tools, authz, server, MCP)
crates/aim-llm*     model providers (trait, codex backend, OpenAI-compatible)
crates/aim-acp      ACP client (Claude Code via claude-agent-acp)
crates/aim          agent layer (loop, dispatcher, store, config, daemon, CLI, TUI)
xtask               repository invariants (`cargo xtask check`)
docs/               vision, architecture, ADRs, research
bench/              benchmark harness and pilots
```
