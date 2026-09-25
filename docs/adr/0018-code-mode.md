# ADR 0018: Run code mode in an isolated worker

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0008, 0016
- Scope: Agent-authored code cells and saved programs; not plugin component execution.

## Context

Code mode can keep intermediate tool data out of the model context and expose many tools through
a compact typed interface. Codex models already advertise `tool_mode` and an `exec`/`wait` JS
contract (`docs/research/extensibility.md`, §3.1, lines 293–304). QuickJS measurements found
about 0.1 ms cold startup and 10.7 µs per awaited host call; Monty supports Python suspension
but is beta and can abort its host process (§3.3, lines 354–378; §D, lines 598–613).

## Decision

Place a `CodeRuntime` trait behind a crash-isolated `aim-coderun` worker process. Apply the OS
sandbox with no ambient network access, deadlines, memory/output limits, and a supervisor. Ship
JS/TS first with rquickjs/QuickJS-ng and oxc type stripping; add Monty for Python second.

For Codex models, follow the catalog `tool_mode` and preserve the `exec`/`wait` contract: raw JS,
pragma, `tools.*`, `ALL_TOOLS`, persistent `store`/`load`, output helpers, and yielded-cell wait.
Other models receive `run_code` (`docs/architecture.md`, §8.3, lines 485–494;
`docs/research/extensibility.md`, §D, lines 591–613). Generate budgeted `.d.ts` and `.pyi`
stubs from tool schemas; put only a compact tool index and describe/search affordances in prompts.

Every `tools.*` call crosses the agent dispatcher and its admission, hooks, audit, and aimx
enforcement. A code cell has no independent tool authority. Cap model-visible cell output;
record nested calls with cell provenance (`docs/research/extensibility.md`, §D, lines 628–636).

Store repeatable programs in git under `~/.aim/programs` or `.agents/programs`, with typed
parameters/results, tool references, provenance, and a saved grant snapshot. Program execution
uses saved grants intersected with current policy, including trust state. Retrieval may combine
FTS5, vectors, and one Jev rerank (`docs/architecture.md`, §8.3, lines 496–499;
`docs/research/extensibility.md`, §E, lines 638–664).

## Consequences

Worker RPC and supervision add machinery, but interpreter failures cannot crash the daemon.
TypeScript stripping is not a type checker; runtime schema checks remain necessary.

## Verification

In M7, add `coderun_nested_call_uses_dispatcher`, `coderun_timeout_and_crash_isolation`,
`codex_exec_wait_contract`, and `program_grants_only_narrow` integration tests. Benchmark
worker cold start and awaited host calls against the §3.3 research baseline.
