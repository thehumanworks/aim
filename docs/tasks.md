# Task register

The live register of in-flight work: one row per task with its status, what is happening now and
what comes next. It exists so a crashed session can be resumed and so a reviewer can verify each
step as it lands. The lead updates it at every state change and commits it with the work.

- **Statuses:** `todo` · `investigating` · `in progress` · `review` · `blocked` · `done` · `dropped`.
- **Recovery:** read this file, then `git worktree list` and `git log --oneline main..<branch>`
  for each branch below. A worker's worktree holds its uncommitted state; its branch holds the
  rest.

## Idea to explore: Jev as a hackable system-one service

**Status:** todo — exploration prompt; no implementation decision yet.

> Explore the idea of **TypeSafe Jev as a hackable “system-one brain” for aim**: a lightweight decision engine that can make small, instinct-like assessments on an agent’s behalf while the agent focuses on deliberate work. Its decision trees or definitions should be changeable at runtime, so agents and users can evolve what the engine notices and when it offers advice.
>
> Consider how to expose this as an **isolated harness service**, available to agents and other clients through a clear interface. The TUI may display or interact with its decisions, but it should be a client of the service—not where the engine lives.
>
> Use decisions such as “might this code-mode workflow be worth saving for reuse?” as examples, not as the limits of the design. Explore what makes the engine useful without making it intrusive: when it should stay quiet, when it should surface an observation, and how its judgments could improve with experience. Ground the proposal in aim’s existing Jev integration and architecture, and leave room for alternative designs rather than assuming a particular implementation.

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
| T1 | `/provider`, `/model`, `/effort` suggestions and completion; model list follows the selected provider (one `SessionOptions` update, ADR 0074) | done | Merged incl. follow-ups, REV-T1 and REV-T1b fixes (`147a5a9`); codex REV-T1c: MERGE | — | `agent/claude/tui-options` (worktree removed) |
| T2 | `/clear` clears the chat and starts fresh | done | Merged with T1 (transcript, inline rows, row cache, screen + scrollback best effort, new session) | With T1 | with T1 |
| T3 | Parallel tools: batching guidance in prompts, `readOnlyHint` on aimx MCP, end-to-end concurrency tests, FIX16 cell scheduler moved to the kernel with proofs | done | Merged incl. REV-T3 B1 fix (`bd418ae`): rendezvous tests; forced serialization fails them | Optional codex re-check of the fix | `agent/claude/parallel-tools` (worktree removed) |
| T4a | `AIM_CODE_MODE` (off/on/only): verified decision, native sessions, `aim mcp`, code-mode MCP proxy in front of aimx for `acp:claude`, typed declarations + `Promise.all` in the code tool (ADR 0076) | done | Merged incl. REV-T4a and REV-T4a-b fixes (`e569a5e`) | Residual in ADR 0076: aimx needs a SIGTERM handler (orphan shells if it ignores close) | `agent/claude/code-mode` · `../aim-wt/code-mode` |
| T4b | Benchmark code mode (offline + live, pre-declared rule), then set the default; repair the wire gate | done | Merged (`2df7b76`): exact graders (codex REV-T4b3: MERGE), cohort 2 rule verdict `off`, shipped default On per maintainer, wire gate green | — | `agent/claude/code-mode-bench` (worktree removed) |
| T4c | Per-session code mode: client-side `AIM_CODE_MODE` / `--code-mode` carried in the session spec, kept on resume and `/new`, shown in the status line; ACP relay follows it | review | Merged (`d04bcf1`, merge `b7eaf87`); kernel 364 verified on branch; live: `code:on` with a daemon started without the variable; codex REV-T4c running | Close on verdict; remove worktree | `agent/claude/code-mode` · `../aim-wt/code-mode` |
| T5 | ACP Claude: selecting Opus (and any model) works — verified model-id resolution (ADR 0075) | done | Merged incl. REV-T5 fixes (`c4a1b35`); codex re-check: MERGE | Follow-up (not in this batch): adapter resets mode/effort on model switch | `agent/claude/acp-models` (worktree removed) |
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
- 2026-09-25 — Codex REV-T1 (auth.json hash unchanged): MERGE AFTER FIXES. B1: an options lookup can
  publish after a newer ConfigChanged (aborted task not awaited; no generation fence) — stale
  effort ladder in stream and snapshot. N1: `switch` spec does not pin candidate order. Queued to
  the T1 worker with its follow-ups. Protocol compatibility, `/provider` reset, `/clear` and the
  kernel (318 verified) confirmed.
- 2026-09-25 — REV-T5 B1–B5 fixed (`ef65740`, `c4a1b35`; merged `1deaa65`): conflicting
  generations/families refuse (kernel `Conflict`, 3 new theorems), errors never echo the request,
  one classification rule, variant spellings declared by the claude profile. Merged tree with T1:
  clippy clean, aim-acp+kernel 101/101, aim lib 277/277. Codex re-check started.
- 2026-09-25 — T4a done (`6c65e75`, merged `58f857e`). Kernel `code_mode`; `AIM_CODE_MODE` in native
  sessions, `aim mcp` (code tools 330 s timeout) and a hidden `aim code-mcp` relay for strict
  acp:claude (harness protocol to aimx, local or SSH). Request bytes W1: on 7,650 (+947),
  off 8,258, only 4,903. Live: gpt-4.1-mini in `on` ignored run_code; in `only` used it (2–16
  requests, answers right after worker fixes); Claude used run_code in `on` and `only`, also
  over SSH. Merged tree (T1+T3+T4a+T5): clippy clean, xtask ok, Verus 353 verified, 0 errors.
- 2026-09-25 — Codex REV-T4a started; T4b launched (wire gate repair + pre-registered code-mode
  benchmark + default decision).
- 2026-09-25 — Codex REV-T4a (auth.json hash unchanged): MERGE AFTER FIXES. No ceiling-widening path
  found; kernel matches ADR 0076 (254 verified on branch). B1: script error messages bypass the
  cell output budget (runtime.rs:239, cells.rs:100). B2: on client EOF the relay waits for
  pending calls (server.rs:79, proxy.rs:118), so aimx/ssh children can outlive the client by up
  to 630 s. N1: an invalid AIM_CODE_MODE silently selects the default; `aim run` shows no
  warning. Sent back to the T4a worker.
- 2026-09-25 — T1 follow-ups + REV-T1 fixes merged (`d3a31a0`): case-folded choices, `auto_effort`,
  generation-fenced option publishing, pinned candidate order, live summary model. Merged tree:
  clippy clean; aim lib 286/286, host 30/30, acp_bridge 5/5. Codex re-check of T1 started.
- 2026-09-25 — Codex REV-T1b (re-check; auth.json unchanged): B1 and N1 confirmed fixed (323
  verified). New B2: `announce` broadcasts ConfigChanged before updating the summary's model
  (host.rs:1045/1050), so a concurrent attach can pair the old model with the new ladder. Sent
  back to the T1 worker.
- 2026-09-25 — REV-T1b B2 fixed (`147a5a9`, merged): publish() updates the summary and broadcasts ConfigChanged under the transcript lock; deterministic + stress tests. Branch Verus 358 verified.
- 2026-09-25 — REV-T4a fixes merged (`8c8ea26`): errors bounded at worker, parent and exec/wait;
  `aim mcp` and the relay end pending calls 2 s after client EOF (relay test: aimx, bash and sleep
  gone, no `survived` file); harness close lets aimx reap first; invalid AIM_CODE_MODE = off,
  reported once on stderr, proved. Branch: 256 verified; 593 tests passed. Codex re-check started.
- 2026-09-25 — Codex REV-T5b (re-check; auth.json unchanged): VERDICT MERGE — B1–B5 fixed, theorems meaningful, 277 verified. (First attempt hung on an open stdin; rerun with `< /dev/null`.)
- 2026-09-25 — Codex REV-T4a-b (re-check; auth.json unchanged): main B1/B2/N1 paths closed (256
  verified), residual gaps: exec/wait accept a zero-token limit smaller than the truncation note;
  run_program/save_program workspace errors and nested observer events are not cut; a reply write
  can block forever if the client stops reading; if aimx ignores close its shells can be orphaned;
  the invalid-value warning echoes the whole env value. Sent back to the T4a worker.
- 2026-09-25 — Codex REV-T1c (final re-check; auth.json unchanged): VERDICT MERGE. Note: the unit test's 50 ms wait makes its detection timing-dependent (the stress test backs it).
- 2026-09-25 — REV-T4a-b residuals fixed (`e569a5e`, merged): 100-byte exec/wait floor, program + nested errors cut, MCP replies via bounded writer (test fails on the old server), env value not echoed. Residual documented: `aimx serve --stdio` has no SIGTERM handler.
- 2026-09-25 — T4b done (`ad49be0`, merged `02d987e`). Wire gate: 35 regressions → passed. 26 were a
  TMPDIR leak (+44 B in every codex/pi row; trials now under a fixed /tmp root); aim's growth is
  attributed byte by byte (ui_* tools +773, batching +139, code-mode section +241, run_code
  guidance +706). New baseline `t4b-main-baseline.json`, new cohort. Benchmark (pre-registered
  `7a788e5`): gpt-4.1-mini 3 reps × 9 tasks — off 23/27, on 23/27 (+16% requests, +22% ITE on
  scripting tasks; rule needed −25%), only 15/27; codex and acp:claude: equal requests, +3–4% ITE.
  Decision: DEFAULT_MODE = Off (code mode opt-in via AIM_CODE_MODE). Spend: OpenRouter $0.53,
  codex 12/12, Claude 13/16; auth.json unchanged. Merged-tree Verus 360 verified.
- 2026-09-25 — Follow-up sent: the "# Code mode" prompt section only when code tools are offered
  (−241 B/request at the default). Codex REV-T4b started.
- 2026-09-25 — T4b follow-up merged (`1d6c157`): code-mode prompt section only where a code tool is
  offered; W1 8,353 → 8,112 B; bound 8,160.
- 2026-09-25 — Codex REV-T4b (auth.json unchanged): MERGE AFTER FIXES. Confirmed: plan preceded
  the runs; decision recomputes to `off`; ledger reconciles; wire attribution coherent; default
  routes through Off (360 verified). B1: the three new graders accept wrong reports (wrong paths /
  wrong function names). Per the plan, grader changes after the smoke need a new cohort: fix
  graders + false-pass tests, re-register, rerun the primary arms (~$0.55), re-grade secondaries
  offline if their reports were kept. Sent back to the T4b worker.
- 2026-09-25 — Full `mise run check` on `b10ad82` (all merges except T4b's grader fix): green — 1,036 tests passed, 0 failed; wire gate passed. Tree clean.
- 2026-09-25 — Maintainer tried `export AIM_CODE_MODE=1; mise run tui` and got no code mode: the
  running daemon (pid 5761, started 18:11) lacked the variable, and a no-op `mise run build` does
  not restart it. Verified in-process: `AIM_CODE_MODE=1 aim run` offers run_code/save_program/
  run_program/list_programs (and hides Glob/Grep/KillShell/session search); `off` does the reverse.
- 2026-09-25 — Maintainer decisions: (1) make code mode per-session and client-carried with a
  status-line indicator (T4c); (2) make code mode the DEFAULT (`on`), overriding the benchmark
  rule's `off` — T4b's worker flips DEFAULT_MODE, re-records the wire baseline and records the
  override in ADR 0076 next to the (rerun) numbers.
- 2026-09-25 — T4b final merged: graders exact (false passes now fail; cohort 1 kept reports
  re-grade unchanged); cohort 2 (gpt-4.1-mini, 3×9, rotated arms): off 23/27, on 24/27, only 16/27;
  scripting requests off 5.56 / on 7.33; rule verdict `off`. Shipped DEFAULT_MODE = On (maintainer).
  Wire at default: W1 7,745 B, 14 tools; bound 7,793. OpenRouter total $0.94 of $3; no new
  codex/Claude runs; auth.json unchanged. Branch Verus 360 verified.
- 2026-09-25 — Codex REV-T4b2 (re-check; auth.json unchanged): original false passes fixed; cohort 2
  properly registered; verdict and override recorded separately; 360 verified. MERGE AFTER FIXES:
  B1 `callers` accepts duplicate call sites; B2 `todo_table` rejects correct tables with the File
  column not first or linked paths. Sent back: fix, then re-grade cohort 2's kept reports offline
  (cohort 3 only if reports were not kept); default stays On.
- 2026-09-25 — REV-T4b2 fixes merged (`9807c7c`): graders reject duplicates, accept decorated tables; offline re-grade of 66 kept trials changed 0 verdicts; $0. Codex REV-T4b3 (final; auth.json unchanged): VERDICT MERGE (34 adversarial probes as expected). T4b closed.
- 2026-09-25 — Maintainer: prefer release builds; clean up worker worktrees after merging. Removed
  the worktrees and local branches of T1, T3, T5 and T4b (all merged, reviews closed; remote
  branches kept). Several finished worktrees had lost their `target/` earlier (build caches only);
  T4c's was recreated and is rebuilding. T4c told to build binaries with `--release`.
- 2026-09-25 — This register was found deleted from the lead's working tree (cause unknown; no
  other tracked file changed); the lead's next commit recorded the deletion (`d50bc4f`). Restored
  from `da5418c`.
- 2026-09-25 — T4c done (`d04bcf1`, merged `b7eaf87`): SessionSpec.code_mode from the client
  (`--code-mode`, else client AIM_CODE_MODE), kernel precedence session > daemon > default before
  the guards, SessionMeta.code_mode recorded and kept on resume and /new, /clear, /provider,
  `code:on|off|only` in the status line and `aim sessions`. 685 tests passed. The worker reported
  an outside `find … -name target -exec rm -rf` deleted its target/ mid-run (not a batch worker).
  DEFAULT_MODE confirmed On after the merge. Release binaries rebuilt in the lead checkout
  (`cargo build --release`, daemon not stopped). Codex REV-T4c started.
