# ADR 0076: Select code mode with `AIM_CODE_MODE`, decide its exposure in the kernel, and serve it to Claude through aim's relay

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0018, 0056, 0066, 0012
- Scope: Which tools a model is offered when code mode is off, on, or the only way to act: in
  native sessions, in `aim mcp`, and in strict `acp:claude` sessions. The model-visible code tool
  description and nested results. The benchmark hook that selects a mode per arm, and the default
  that benchmark chose (§6). Not `acp:claude-native`, whose tools are Claude's own.

## Context

The maintainer asked for a switch that runs aim's MCP server in code mode, where "an agent
[scripts] multiple tool calls via typed interfaces so that a sequence like 'find something → read
multiple files → summarize' can be done with a single script", and decided that code mode is
preferred over individual tool calls, but that a benchmark must validate its effect before it
becomes the default. Claude over ACP must get code mode through an MCP relay that aim serves in
front of aimx, since aimx may not spawn processes outside its backends (AGENTS.md).

What existed (the integration branch at `36f1bd8`):

- **Always on, no switch.** Native sessions got `run_code` (or codex's `exec`/`wait`, ADR 0018)
  and the program tools whenever the worker was found on macOS. ADR 0056's compact set hid
  `Glob`, `Grep`, `KillShell`, `search_sessions` and `read_session` from direct use.
- **Declarations never shown.** The model saw only a list of cell tool names. The 32 KiB
  TypeScript declarations (`coderun/types.rs`) were never in a request. Nested text had to be
  dug out of `result.content[0].text`.
- **Code mode went unused.** In W26's final live cohort the OpenRouter model never called
  `run_code`. Its tools were Read 10, Bash 7, Edit 6, LS 6, BashOutput 1
  (`bench/results/w26-final-live-openrouter.json`, `tool_calls_by_name`). Codex called `exec`
  once in 20 tool calls (`w26-final-live-codex.json`).
- **Claude got no code mode.** Strict `acp:claude` sessions got `aimx mcp`: 18 entries, each
  tool twice (`Read`/`read`), 11,292 bytes of tool JSON (measured, below). Claude's `toolAliases`
  pointed `Glob` and `Grep` at them.
- **`aim mcp` cut code calls short.** It offered `run_code` over its services with a 60 s call
  timeout, below a cell's 120 s default and 300 s maximum deadline (ADR 0066).
- **Code mode could widen a ceiling.** The review of native subagents (REV20, `docs/handover.md`
  §4) found a child escaping its parent's ceiling through code mode. So the exposure decision
  must never widen a ceiling.

## Decision

### 1. `AIM_CODE_MODE`: `off`, `on`, `only`

The variable takes `off`, `on` or `only`, and also `0`, `1`, `false` or `true`, in any case
(`coderun::mode::parse`). An unset variable means the default. An invalid value logs a warning
and also means the default.

- **`off`:** no code tool and no program tools. Every direct tool is offered.
- **`on`:** the code tool(s) and the program tools, beside a compact direct set. In native
  sessions and in the ACP relay that set is everything except ADR 0056's hidden five. In `aim mcp`
  nothing is hidden: each of its few services is a primary action.
- **`only`:** the code tool(s) and the program tools alone. Every other tool is reachable only
  inside a cell, and calling one directly fails with "no tool named".

**The default** is one named constant, `aim_kernel::code_mode::DEFAULT_MODE = Off`. It was `On`
provisionally until task T4b's pre-registered benchmark decided it (§6): code mode is opt-in.
Changing it again is a one-line change plus this ADR's successor.

### 2. The decision is verified

`aim_kernel::code_mode` decides from five inputs:

- the request (`None` for unset or invalid);
- the default;
- whether the worker was found;
- whether the platform sandboxes it (macOS);
- whether the session's ceiling permits `run_code`.

`decide` returns an `Exposure`:

- the effective mode;
- whether the code and program tools are offered;
- which direct set is offered: `Full`, `Compact` or `Hidden`;
- the `Fallback` reason: platform, worker or not permitted.

`direct_tools` then selects the direct tools by host-assigned name ids. The shell asks
everywhere code mode is composed:

- `providers::code_mode` checks the worker and the platform. A `CodeConfig` exists only if both
  hold.
- `host.rs` checks a session's ceiling.
- `aim mcp` (`services::with_code_mode`) and the relay (`mcp::proxy`) check their own inputs.
- `acp.rs` derives the Claude aliases from the kernel's answer.

A requested mode that falls back is logged: as a warning when it was set explicitly, at debug
level when it was the default, and always at debug level for a ceiling without `run_code`. A
missing worker is normal on some machines, and a configured ceiling is not a misconfiguration.

### 3. The code tool sells itself, with types

The `run_code` description, and codex's `exec`, now carry:

- **One sentence:** prefer one script to several tool calls for multi-step work (find, read
  several files, summarize) and for fan-out.
- **A one-line `Promise.all` example** that reads two files.
- **A budgeted TypeScript API** (`coderun::types::api`) of the tools callable in cells. The tools
  the model cannot call directly are typed first: in `on`, `Glob`, `Grep`, `KillShell` and the
  session-search tools; in `only`, all of them. The limits are 1 KiB per signature and 4 KiB in
  total. Every other tool is listed by name, within 512 bytes (W26's index is the floor), and
  `describe(name)` and `search(query)` remain.

A nested result's `.text` joins its text parts, and its string form is the same text. This is
defined non-enumerable in the worker's bootstrap, so a result's JSON and `content` are unchanged
and codex's `exec`/`wait` contract is kept. `system.md` gains a two-line "# Code mode" section:
use a script for multi-step and fan-out work, and a direct tool for a single simple action.

**Scripts that fail must say why.** The first live run (below) showed that a model's natural
scripts failed silently:
- `const { tools } = global` reached the model only as "Exception generated by QuickJS";
- `main().then(v => console.log(v))` ended before its calls ran, with no output;
- a trailing `({ summary })` expression returned nothing.

The model answered that there were no TODOs, which was wrong. So the worker now:
- reports an exception by its name and message ("the script threw ReferenceError: require is
  not defined"). For `require`, a missing module, `fetch` or `process`, it adds that a cell is not
  Node and names `tools.*`. QuickJS's own memory and stack limits stay `LimitExceeded`.
- returns a cell's last top-level expression statement as its value, as a REPL does, so a
  trailing promise chain is awaited. This applies to `run_code` and to codex's `exec` cells
  (ADR 0018's contract gains it; `codex_exec_wait_contract` still passes), not to saved programs,
  which return from `main`.
- maps `console.log`, `info`, `warn`, `error` and `debug` to `text`, and `global` to
  `globalThis`.

`run_code` names an empty result: "no output: emit results with text(value), or end the script
with an expression; await its promises". The `run_code` and `exec` descriptions add "Not Node: no
require/import, fs or fetch".

### 4. Claude gets code mode through aim's relay

When the effective mode is `on` or `only`, a strict `acp:claude` session's single `aim` MCP server
is `aim code-mcp`, a hidden subcommand of aim's own executable. Its arguments are all explicit,
because the adapter that spawns it need not pass aim's environment on: `--root`, `--aimx`,
`--coderun`, `--programs` and `--code-mode`, plus `--ssh` and `AIM_SSH_CONFIG` for a remote
workspace.

**Upstream: aimx's harness protocol.** The relay connects the workspace as a native session
does, through `host::aimx_workspaces`: locally with `aimx serve --stdio`, or over SSH through
`aimx serve --ssh`. It composes code mode with the same function as native sessions
(`host::with_code_mode`) and serves it with `AimMcpServer` over stdio.

The task suggested wrapping `aimx mcp` with aim's MCP client instead. The harness protocol was
chosen for four reasons:

- It is aimx's native interface, and `aimx mcp` is itself a façade over it.
- It has no duplicated lower-case names to collapse.
- It avoids the MCP client's 30 s call timeout and 1 MiB frame limit (`mcp/client.rs`).
- Project programs reach the workspace through `Files`.

**Authority is unchanged.** Every call, nested or direct, still crosses aimx's authorization.
aimx spawns nothing new, and the aim crate's spawning is outside the `crates/aimx/src` OS-access
rule.

**The relay's executable** is `$AIM_BIN`, else the running process when it is `aim`. An embedder,
such as a test binary hosting sessions in process, keeps `aimx mcp` rather than spawning itself.

**The relay's session.** A relay session has no named agent (ACP refuses them), so its ceiling
permits `run_code`. Nested calls from a relay cell run without child events, because no agent
turn observes them (ADR 0066 §2, "Direct callers").

**Aliases** follow the relay (`aim_acp::AimRoute`). No alias points at a tool the mode hides:

- `aimx mcp` keeps the lower-case `DEFAULT_ALIASES`.
- In `on`, `Bash`, `Read`, `Edit` and `Write` alias to `mcp__aim__Bash` and the like. `Glob` and
  `Grep` are hidden, so they get no alias.
- In `only`, there are no aliases.

**The conformance challenge** follows the route too:

- Where the relay shows a read (or, over SSH, a write) tool, the challenge calls it directly.
- In `only`, it asks Claude to call `mcp__aim__run_code` with a cell that calls `tools.Read` (or
  `tools.Write`).
- A witness records its route, and it matches only a session with the same relay and route.

`off` (the default, §6), or no worker, keeps `aimx mcp` exactly as before. `acp:claude-native` is
unchanged.

**Timeouts.** `AimMcpServer` gives `run_code`, `exec`, `wait` and `run_program` at least a 330 s
call timeout: a cell's 300 s maximum deadline plus a margin. Other tools in `aim mcp` keep 60 s.
The relay serves every call with a 630 s timeout, because aimx's `Bash` may run 600 s.
`aim_acp::CODE_RELAY_TOOL_TIMEOUT_MS` holds that value, and it is also Claude's
`MCP_TOOL_TIMEOUT` in `_meta.claudeCode.options.env` on the code route. Claude Code's own default
could not be established: the minified `claude` 2.1.282 binary defines its fallback name twice,
once as `1e3` and once as `1e8`. So aim does not rely on it. `aimx mcp` sessions are unchanged.

### 5. Benchmark arms

`bench/run.py` accepts an arm per aim harness: `aim_openrouter@off`, `@on` and `@only` set
`AIM_CODE_MODE` for that run. An arm-less aim harness gets the caller's `AIM_CODE_MODE`. The
legacy `AIM_BENCH_CODE_MODE=off` still means `off`, and nothing set means aim's default.
`AIM_CODERUN` is always explicit, and the old missing-worker trick is gone. Arms are not in the
committed wire baseline, so they are compared with `--no-gate`. The scripted mock sends a
`run_code` cell for its shell steps when a request offers `run_code` but no direct `Bash`, so the
wire tier measures `only` too.

### 6. The default is `off`: a pre-registered benchmark found no efficiency gain

The maintainer prefers code mode, provided a benchmark shows its effect on tool-calling efficiency
first. T4b pre-registered the benchmark before any live run (`bench/plans/code-mode.md`, commit
`7a788e5`): arms, tasks, metrics and a rule whose margins live in `bench/manifest.toml`
`[code_mode]` and which `bench/code_mode.py` applies to the result files.

**The rule.** `off` is the reference. `on` or `only` becomes the default only if all three hold:

- (a) its pass rate over all nine tasks is at least `off`'s minus 8 points;
- (b) on the three scripting tasks, its mean model requests per trial or its ITE per passed task
  is at least 25% below `off`'s, and the other is at most 10% above;
- (c) on the six existing tasks, its ITE per passed task is at most 10% above `off`'s.

**The tasks.** The live tier's six fix-a-function tasks, and three new fan-out tasks that end in
a report: count TODO/FIXME per file over ten files (`todo_table`), list every call site of
`load_config` among eleven modules with decoys (`callers`), and index the first heading of ten
Markdown files (`doc_index`). Hidden tests grade the reports.

**Primary cohort** (decides): `openai/gpt-4.1-mini` on OpenRouter, three repetitions of all nine
tasks per arm, arm order rotated (`bench/results/t4b-code-mode-openrouter-r{1,2,3}.json`):

| Arm | Pass | Requests (scripting / existing) | ITE per passed (scripting / existing / all) | USD per passed | p50 wall |
|---|---|---|---|---|---|
| `off` | 23/27 | 6.22 / 6.83 | 13,113 / 5,770 / 7,367 | $0.0034 | 12.4 s |
| `on` | 23/27 | 7.22 / 6.89 | 16,026 / 5,091 / 7,468 | $0.0035 | 13.5 s |
| `only` | 15/27 | 12.0 / 13.39 | 218,307 / 21,982 / 35,071 | $0.0157 | 27.9 s |

- **`on` fails (b):** on the scripting tasks it took 16% *more* requests and 22% more ITE per
  passed task than `off`. gpt-4.1-mini called `run_code` in 1 of 27 `on` runs.
- **`only` fails (a), (b) and (c):** 8 fewer passes, twice the requests, 4.8 times the ITE. Of
  its 292 `run_code` calls, 97 failed (54 `ReferenceError`s, mostly `require`); it made 109
  program-tool calls (`list_programs` against an empty store) and hit the 24-request cap 5 times.
- **What the rule does not reward:** `on` spent 12% less ITE than `off` on the existing tasks.
  That is its smaller prefix (ADR 0056's compact direct set and shorter `Read`/`Bash`/`LS`
  descriptions), not code: no existing-task `on` run called `run_code`.

**Secondary cohorts** (the three scripting tasks, two repetitions each):

| Arm | Pass | Requests | Direct / nested calls | ITE per passed | p50 wall |
|---|---|---|---|---|---|
| `aim_codex@off` (gpt-6-sol, low) | 6/6 | 4.33 | 9.83 / 0 | 6,541 | 20.9 s |
| `aim_codex@on` | 6/6 | 4.33 | 6.83 / 2.67 | 6,737 | 18.1 s |
| `acp_claude@off` (sonnet) | 6/6 | 4.83 | 6.33 / — | 20,917 | 16.2 s |
| `acp_claude@on` | 6/6 | 4.83 | 6.50 / — | 21,763 | 20.3 s |

Codex used `exec` in 4 of its 6 `on` runs and Claude used `run_code` in 2 of 6, with the same
request count as `off` and 3–4% more ITE. Neither cohort meets the per-provider bar the plan set
(the rejected mode winning (b) there, or the chosen mode losing two passes), so there is no
per-provider default. Claude's requests come from its own transcript; the relay's nested calls
are not observable (§4).

**Decision:** `DEFAULT_MODE = Off`. `on` and `only` remain available through `AIM_CODE_MODE` for
native sessions, `aim mcp` and the `acp:claude` relay.

## Consequences

**Amends ADR 0018 and ADR 0056.**

- ADR 0018 put "only a compact tool index" in prompts. The code tool now carries a budgeted typed
  API of its cell-only tools.
- ADR 0056's compact set is now `on`'s direct set, chosen by the kernel. `off` and `only` are new.

**Request size** (mock wire tier, OpenRouter Chat Completions, first request of W1):

| Mode | Before (`36f1bd8`) | After | Tools | Change |
|---|---|---|---|---|
| `on` (default) | 6,703 | 7,650 | 14 | +947 |
| `off` | 8,017 | 8,258 | 15 | +241 |
| `only` | — | 4,903 | 4 | −1,800 against `on` before |

- **Where `on` grows:** +241 bytes of instructions (the system prompt went from 1,440 to 1,681
  characters) and +706 bytes of code-tool description.
- **The relay:** it lists 10 tools in 5,629 bytes in `on` and 4 tools in 2,868 bytes in `only`,
  against `aimx mcp`'s 18 tools in 11,292 bytes. Its `run_code` description is 926 bytes in `on`
  and 1,262 bytes in `only`.
- **Measurements:** an MCP `initialize` plus `tools/list` against each relay's debug binary,
  and `bench/run.py wire --harnesses aim_openrouter@off,aim_openrouter@on,aim_openrouter@only
  --cases W1 --repetitions 1 --no-gate`. The "before" column is the same wire run on `36f1bd8`.

These sizes were measured under macOS's `/var/folders/…/T/` TMPDIR; under the benchmark's fixed
`/tmp` trial root they are 44 bytes smaller (7,745 for `on`).

**The wire gate** (`mise run bench:wire`) failed on the integration branch, before and after this
change. T4b repaired it and re-recorded the baseline on the decided default
(`bench/results/t4b-main-baseline.json`; the attribution table is in `bench/README.md`, "Wire gate
repair"). The mock now scripts a `run_code` cell for `only`, so every mode has a wire measurement.

**Other consequences:**

- **What `only` costs.** The model reaches every workspace tool through a cell, so a single
  simple action costs a script. The benchmark measured it (§6).
- **Code mode is opt-in.** With the default `off`, `aim mcp` offers no `run_code` and a strict
  `acp:claude` session uses `aimx mcp`, as before this ADR; `AIM_CODE_MODE=on` or `only` selects
  the code tool and the relay. A default `off` session still sends the system prompt's two-line
  "# Code mode" section (241 bytes) and does not get ADR 0056's shorter `Read`/`Bash`/`LS`
  descriptions, which only code-mode sessions apply: both are follow-ups.
- **Allowlist warnings.** An allowlisted agent in `only` warns that its allowed direct tools are
  not offered (`AllowedTools::unknown`). This is cosmetic.
- **`aim run` shows no warning.** It installs no tracing subscriber, so an invalid
  `AIM_CODE_MODE` there is silent. The daemon logs it.
- **The relay's model is fixed.** It always uses portable `run_code`; there is no catalog there.

## Verification

**Proofs:** `mise run verify` (`crates/aim-kernel/src/code_mode.rs`, 21 obligations). The
theorems take the default as an input, so they hold for `Off`. The benchmark has settled the
default; the specs stay `DRAFT(ADR-0076)` until the maintainer locks them.

| Theorem | What it proves |
|---|---|
| `theorem_unset_means_default` | An unset or invalid request is a request for the default. |
| `theorem_code_needs_worker_platform_and_permission` | Code and program tools are offered only when all three facts hold. |
| `theorem_never_widens_a_ceiling` | A ceiling without `run_code` gets no code tools and keeps `Full`, whatever was requested; `only` never applies to it. |
| `theorem_off_offers_no_code` | `off` offers no code or program tools and every direct tool. |
| `theorem_only_is_never_empty` | Hidden direct tools imply that the code tool is offered; `only` that cannot run falls back to `off` with its cause. |
| `theorem_fallback_names_its_cause` | A fallback happens exactly when a mode other than `off` is blocked, and it names the blocker. |
| `theorem_full_and_hidden_direct_sets` | `Full` is every tool, in order; `Hidden` is none. |
| `theorem_direct_sets_narrow` | Every direct set is within the full set, and the compact set never offers a hidden tool. |

A mutation that drops the permission check fails verification.

**Tests:**

- `coderun::mode::tests` (parsing, fallbacks, direct sets) and `coderun::types::tests` (the
  typed API's budget).
- `tests/resources.rs`: `code_modes_compose_their_tool_lists_and_never_widen_a_ceiling`, the
  native tool list for each mode and ceiling.
- `tests/code_mode.rs`:
  - `aim_mcp_serves_each_mode_s_tools`;
  - `the_acp_relay_serves_code_mode_over_a_real_workspace`: `aim code-mcp` against a real aimx
    and worker lists each mode's tools, runs one `Promise.all` cell that returns two files' first
    lines, and refuses a direct `Read` in `only`.
- `acp::tests::strict_sessions_point_at_the_code_mode_relay_when_code_mode_is_on`: the
  `session/new` payload per mode.
- `challenge_tests` (the conformance prompts), and `bench/test_bench.py`
  `test_aim_arms_select_aim_code_mode`.
- `cells.rs`: `nested_results_expose_their_text_and_run_together` and
  `scripts_written_like_node_return_their_output_and_their_errors`.

**Live** (ADR 0022): the two `live_*` smoke tests below, and the runs in "Live evidence".

### Live evidence

All runs were on 2026-09-25 against a scratch git repository with four TODO comments in four
files (and one FIXME). The prompt was "Find all TODO comments across the files in this
repository and summarize them". The binaries were release builds of this branch.
`~/.codex/auth.json` had the same SHA-256 before and after the runs, `7315f3c9…91b0`.

**Native, OpenRouter `openai/gpt-4.1-mini`, `aim run --ephemeral --json`:**

| Run | Requests | Top-level calls | Cost | Answer |
|---|---|---|---|---|
| `only`, before the runtime fixes | 6 | 5 `run_code` | $0.0043 | **wrong**: "no TODO comments" |
| `only`, after (1) | 2 | 1 `run_code` (1 nested `Grep`) | $0.0012 | right, 4/4 |
| `only`, after the Node hint (a–e) | 3, 4, 3, 8, 16 | `run_code` 2, 3, 2, 8, 4; `list_programs` 0, 1, 0, 4, 26 | $0.0019–$0.0070 | right, 4/4 in all five |
| `on` | 3 | 2 `Bash` (no `run_code`) | $0.0016 | right, 4/4 |
| `off` | 2 | 2 `Grep` | $0.0017 | right, 4/4 |

- **Before the fixes**, the model's scripts failed silently (see Decision §3), and it answered
  that there were no TODOs.
- **After the fixes**, every `only` run was right, but the cost varied widely. gpt-4.1-mini
  still writes `require('fs')` first. Its longest run (16 requests) spent 26 of its 30 calls on
  `list_programs`, which answers `[]`: in `only`, the program tools distract a weak model. That
  is an input for the benchmark, not fixed here.
- **These are single runs, not a benchmark.** They show the modes work end to end; they do not
  choose the default.

**`acp:claude` (`claude-agent-acp` 0.81.2, `-m sonnet`), `aim run --json`:**

- **`on`:** the strict session started through `aim code-mcp`, so its conformance challenge read
  through `mcp__aim__Read`. Claude made one call, `mcp__aim__run_code`, whose cell called
  `tools.Grep`, a tool hidden from direct use in `on`. It answered right, 4/4. The prompt used
  23,756 input tokens (11,740 cached) and 311 output tokens.
- **`only`:** the session started, so the conformance challenge passed through `run_code`.
  Claude again used one `run_code` cell and answered right.

**`acp:claude` over SSH** (`acp_bridge.rs` `live_acp_claude_ssh_remote_changed_local_untouched`,
private sshd; it spawns the `aim` binary, so the relay applies):

- **`AIM_CODE_MODE=only`:** the SSH conformance write went through `run_code`, and Claude did the
  test's read, edit, glob, grep and run steps in two `mcp__aim__run_code` cells. The remote file
  changed and the local sentinel did not.
- **`on` (the default before §6):** Claude called `Read`, `Edit` and `Bash` directly, and
  `run_code` for the hidden `Glob` and `Grep`.

Both passed. The relay needs the adapter to pass its environment to MCP servers: `HOME`, `PATH`
and `SSH_AUTH_SOCK` for `aimx serve --ssh`. The `aimx mcp` relay needs the same.

**Smoke tests:** `live_openrouter_code_mode_only_summarizes_todos_through_run_code` and
`live_acp_claude_code_mode_only_works_through_the_relay` (`crates/aim/tests/code_mode.rs`, run by
`mise run smoke`) passed. The OpenRouter run took 4 requests and 3 top-level calls.
