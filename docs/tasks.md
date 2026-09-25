# Task register

The live register of in-flight work: one row per task with its status, what is happening now and
what comes next. It exists so a crashed session can be resumed and so a reviewer can verify each
step as it lands. The lead updates it at every state change and commits it with the work.

- **Statuses:** `todo` · `investigating` · `in progress` · `review` · `blocked` · `done` · `dropped`.
- **Recovery:** read this file, then `git worktree list` and `git log --oneline main..<branch>`
  for each branch below. A worker's worktree holds its uncommitted state; its branch holds the
  rest.

## Batch 2026-09-25: TUI completion, /clear, parallel tools and code mode, ACP models

Integration branch: `agent/claude/tui-codemode-acp` (from `main` at `1e8cf8c`). Workers run as
Claude Code sub-agents (`.claude/agents/aim-worker.md`: Opus, high effort, no sub-agents), at most
four at a time, each in its own worktree.

**Scope:** no work on the web UI (`crates/aim-web`, the daemon's `--web` listener); the maintainer
may deprecate it. Features land in the TUI and CLI only; protocol changes stay compatible so the web
client still builds.

| ID | Task | Status | Now | Next | Branch / worktree |
|---|---|---|---|---|---|
| T1 | `/provider`, `/model`, `/effort` suggestions and completion; model list follows the selected provider | investigating | Reading the TUI completion broker and the provider catalogs | Design, then a worker | — |
| T2 | `/clear` (and `/new`) clears the chat and starts fresh | investigating | Checking what `/new` does to the transcript | Fold into T1's worker (same files) | — |
| T3 | Parallel tool execution end to end; tools not MCP-only | investigating | Findings in (log): native path concurrent; code-mode supervisor serializes cells; prompts do not push batching; ACP gets aimx MCP only | Design with T4 | — |
| T4 | `AIM_CODE_MODE`: aim's MCP server (and sessions) in code mode; default decided by a benchmark | blocked | Findings in (log); overlaps unmerged FIX16 and W26 | Maintainer decision: build on main or after FIX16/W26; ACP code mode via `aim` relay or not | — |
| T5 | ACP Claude: selecting Opus (and any model) works | investigating | Root cause found (log) | Verified alias resolver + surface advertised models + live test | — |
| T6 | Verus proofs for the decision logic of T1–T5 | todo | — | One kernel module per decision | — |
| T7 | Merge the worker branches, gates, push, PR | todo | — | — | — |

## Log

- 2026-09-25 — Batch opened. Four read-only investigations launched (T1+T2, T3, T4, T5).
- 2026-09-25 — Maintainer: no web UI work in this batch (may be deprecated).
- 2026-09-25 — T3 findings: the agent loop, HarnessClient, aim-rpc peer and aimx server run calls
  concurrently. Serialization: `coderun/supervisor.rs:42,106` holds one lock per cell for the
  whole cell; agentless SSH caps at 3 channels. Prompt: only `prompts/system.md:10` asks for batched
  calls; the ACP prompt and the `run_code` description say nothing about batching or `Promise.all`,
  and the TypeScript declarations are never shown to the model. ACP `acp:claude` gets only
  `aimx mcp --stdio` (18 entries, each tool listed twice), no code mode; Bash lacks
  `readOnlyHint`, so Claude runs it serially.
- 2026-09-25 — T5 root cause: aim matches the requested model exactly against the adapter's
  advertised `model` config option (`acp.rs:161`, `aim-acp/src/config_options.rs:201`) and
  `confirm` wants the literal value back (`config_options.rs:231`). claude-agent-acp 0.81.2
  advertises `default` (Opus 1M), `opus[1m]`, `claude-fable-5-1[1m]`, `sonnet`, `haiku` — no plain
  `opus` — so `-m opus`/`claude-opus-5-5` fail inside aim. The rejection error hides the allowed
  values (`error.rs:162`), and advertised values never reach the UIs (`acp.rs:575`).
- 2026-09-25 — T4 findings: two MCP servers. `aim mcp` (services + `run_code` over services only,
  60 s call timeout < 120 s cell default) and `aimx mcp` (file/shell tools; what `acp:claude` gets).
  Code mode = QuickJS-ng worker; `Promise.all` works inside a cell; TypeScript declarations exist
  (`coderun/types.rs`) but are never shown. Code tools are added beside the direct tools, never
  instead. Live bench tasks never trigger `run_code` (W26 run). Overlap: `agent/claude/fix16-coderun`
  (10 commits, ADR 0066, REV21 fixes pending) rewrites the supervisor (per-session cell queue);
  `agent/perf/tokens` (W26, ADR 0056) adds `DirectCodeTools`, the compact index and
  `AIM_BENCH_CODE_MODE`. Both are 16 commits behind `main`.
