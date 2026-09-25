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
| T0 | Base: merge FIX16 (`agent/claude/fix16-coderun`) and W26 (`agent/perf/tokens`) as-is (maintainer decision) | in progress | Merged (`eda6444`, `36f1bd8`); conflicts resolved in event.rs, session.rs, agent/mod.rs, agent/tools.rs, host.rs; clippy and xtask green | Workspace tests on the merged tree | integration branch |
| T1 | `/provider`, `/model`, `/effort` suggestions and completion; model list follows the selected provider (one `SessionOptions` update, ADR 0074) | todo | Brief written | Launch worker | `agent/claude/tui-options` · `../aim-wt/tui-options` |
| T2 | `/clear` clears the chat and starts fresh | todo | Folded into T1 (same files) | — | with T1 |
| T3 | Parallel tools: batching guidance in prompts, `readOnlyHint` on aimx MCP, end-to-end concurrency tests, FIX16 cell scheduler moved to the kernel with proofs | todo | Brief written | Launch worker | `agent/claude/parallel-tools` · `../aim-wt/parallel-tools` |
| T4a | `AIM_CODE_MODE` (off/on/only): verified decision, native sessions, `aim mcp`, code-mode MCP proxy in front of aimx for `acp:claude`, typed declarations + `Promise.all` in the code tool (ADR 0076) | todo | Brief written | Launch worker | `agent/claude/code-mode` · `../aim-wt/code-mode` |
| T4b | Benchmark code mode (offline + live, pre-declared rule), then set the default | todo | Waits for T4a | — | — |
| T5 | ACP Claude: selecting Opus (and any model) works — verified model-id resolution (ADR 0075) | todo | Brief written | Launch worker | `agent/claude/acp-models` · `../aim-wt/acp-models` |
| T6 | Verus proofs for the decision logic (done inside T1, T3, T4a, T5) | todo | — | Check each worker's `mise run verify` | — |
| T7 | Merge worker branches, full gate + verify, cross-model review, push, PR | todo | — | — | integration branch |

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
- 2026-09-25 — Maintainer decisions for T4: build on top of FIX16 and W26 as-is (their pending review
  fixes stay open and are noted in the PR); aim serves a code-mode MCP proxy in front of aimx for
  `acp:claude`; "not MCP tools exclusively" means code mode is preferred over individual calls.
- 2026-09-25 — `.claude/agents/aim-worker.md` needs a session restart to load (new directory), so
  workers run as `general-purpose` on Opus; nesting is blocked by the PreToolUse hook
  `.claude/hooks/no-nested-agents.py` (verified live: a sub-agent's Agent call was refused).
- 2026-09-25 — FIX16's scheduler runs at most one cell per session by design (REV13a H1); in code
  mode, parallelism is `Promise.all` inside a cell. Kept; the scheduler moves to the kernel (T3).
