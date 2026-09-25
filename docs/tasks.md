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
| T0 | Base: merge FIX16 (`agent/claude/fix16-coderun`) and W26 (`agent/perf/tokens`) as-is (maintainer decision) | done | Merged (`eda6444`, `36f1bd8`); conflicts resolved in event.rs, session.rs, agent/mod.rs, agent/tools.rs, host.rs; clippy, xtask and 963/963 tests green | — | integration branch |
| T1 | `/provider`, `/model`, `/effort` suggestions and completion; model list follows the selected provider (one `SessionOptions` update, ADR 0074) | review | Merged (`84bbbc4`); kernel `switch`; live completion checked on codex, openrouter, acp:claude; follow-ups running (effort case-fold, `auto` only where supported, stale model on reattach); codex REV-T1 running | Merge follow-ups and review fixes | `agent/claude/tui-options` · `../aim-wt/tui-options` |
| T2 | `/clear` clears the chat and starts fresh | review | Merged with T1 (transcript, inline rows, row cache, screen + scrollback best effort, new session) | With T1 | with T1 |
| T3 | Parallel tools: batching guidance in prompts, `readOnlyHint` on aimx MCP, end-to-end concurrency tests, FIX16 cell scheduler moved to the kernel with proofs | done | Merged incl. REV-T3 B1 fix (`bd418ae`): rendezvous tests; forced serialization fails them | Optional codex re-check of the fix | `agent/claude/parallel-tools` · `../aim-wt/parallel-tools` |
| T4a | `AIM_CODE_MODE` (off/on/only): verified decision, native sessions, `aim mcp`, code-mode MCP proxy in front of aimx for `acp:claude`, typed declarations + `Promise.all` in the code tool (ADR 0076) | in progress | Worker running | Review its report, merge, then T4b | `agent/claude/code-mode` · `../aim-wt/code-mode` |
| T4b | Benchmark code mode (offline + live, pre-declared rule), then set the default; repair the wire gate | todo | Waits for T4a | Wire gate: 31 regressions on the merged base + 4 from T3's prompt | — |
| T5 | ACP Claude: selecting Opus (and any model) works — verified model-id resolution (ADR 0075) | review | Merged; codex REV-T5: MERGE AFTER FIXES (B1–B5); worker fixing on the same branch | Merge the fix commits; re-check | `agent/claude/acp-models` · `../aim-wt/acp-models` |
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
- 2026-09-25 — Merged base green (963 tests). Four workers launched (T1, T3, T4a, T5), Opus, one worktree each.
- 2026-09-25 — T5 done (`4e00940`, merged). Kernel `model_match` (7 theorems), aim-acp resolver,
  errors list `value (Name)` pairs, ADR 0075. Live: `live_set_config` and
  `live_acp_claude_session_with_model_alias` pass; `aim run -p acp:claude -m opus` → `ok`.
  Open (adapter behaviour, not aim): switching to `haiku` flipped `mode` to `acceptEdits` and dropped
  the effort/fast options; back on `opus`, effort returned at `medium`. Worth a follow-up so
  `/model` after `/effort` keeps the effort and aim re-asserts its permission mode.
- 2026-09-25 — T3 done (`2c19fc6`, merged). Kernel `cells` (FIX16 scheduler, DRAFT(ADR-0066)).
  aimx MCP already sent readOnlyHint etc. (now tested). New e2e timing tests (session, MCP,
  Promise.all cell). Live: gpt-4.1-mini issued 3 Reads in one response (also with the old prompt);
  `acp:claude` overlapped two `mcp__aim__grep` calls. Claude Code (SDK 0.3.280) treats an MCP tool
  as concurrency-safe iff `readOnlyHint` (read from the bundled binary).
- 2026-09-25 — Wire gate (`bench:wire`, part of `mise run check`) fails on the merged base: 31
  regressions from merging W26 (14 advertised tools vs 10 expected, W1 6703 B vs a 6000 B budget,
  every codex/pi row), plus 4 from T3's prompt (+139 B/request). Belongs to T4b.
- 2026-09-25 — `mise run verify` recipe now tags target/verus (fresh-worktree failure). Merged tree
  (base + T5 + T3): 302 verified, 0 errors.
- 2026-09-25 — Codex REV-T5 (gpt-6-sol, high; auth.json hash unchanged): MERGE AFTER FIXES.
  B1 bare family picks an older generation when two are advertised; B2 a request naming two
  families picks one silently; B3 `AcpBackend::set_config` re-echoes the unredacted request;
  B4 option lookup and resolution depth classify options differently; B5 the `[1m]`/`-1m` variant
  syntax applies to every ACP profile (should be capability data). Sent back to the T5 worker.
- 2026-09-25 — Codex REV-T3 (auth.json hash unchanged): MERGE AFTER FIXES. Kernel specs and shell
  equivalence confirmed (272 verified). B1: 1.8 s ceilings for two 1 s calls are load-sensitive;
  replace with a rendezvous proving both calls were in flight. The reviewer's code-cell failure was
  its own sandbox refusing nested `sandbox-exec`, not the branch. Sent back to the T3 worker.
- 2026-09-25 — REV-T3 B1 fixed (`bd418ae`, merged): rendezvous tests; with temporary mutexes all three fail, without they pass 3/3.
- 2026-09-25 — T1+T2 done (`7bcd18b`, merged as `84bbbc4`). SessionOptions update + attach replay
  (ADR 0074), kernel `switch` (318 verified on the rebased tree). Live: codex `/model` listed the
  real catalog; `/provider openrouter` reset to the gateway default; acp:claude listed the
  adapter's five models. Lead decisions sent back: case-fold effort/model in the shell registry
  (so ACP resolution of `Low` works), offer `auto` only where the backend accepts it (ACP refuses),
  fix the stale reattach model if small. Codex REV-T1 started on `7bcd18b`.
