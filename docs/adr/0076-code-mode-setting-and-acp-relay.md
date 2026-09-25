# ADR 0076: Select code mode with `AIM_CODE_MODE`, decide its exposure in the kernel, and serve it to Claude through aim's relay

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0018, 0056, 0066, 0012
- Scope: Which tools a model is offered when code mode is off, on, or the only way to act: in
  native sessions, in `aim mcp`, and in strict `acp:claude` sessions. The model-visible code tool
  description and nested results. The benchmark hook that selects a mode per arm. Not the choice
  of the default itself (a benchmark decides it, below), and not `acp:claude-native`, whose tools
  are Claude's own.

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

**The default** is one named constant, `aim_kernel::code_mode::DEFAULT_MODE = On`, which is
today's behavior. It is **provisional**: The benchmark cohort of task T4b (`docs/tasks.md`) (`bench/run.py` arms, §5) decides
whether `only` (or `off`) replaces it. Changing it is a one-line change plus this ADR's
successor.

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
level when it was the default. A missing worker is normal on some machines.

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
  trailing promise chain is awaited.
- maps `console.log`, `info`, `warn`, `error` and `debug` to `text`, and `global` to
  `globalThis`.

`run_code` names an empty result: "no output: emit results with text(value), or end the script
with an expression; await its promises". The description adds "Not Node: no require/import, fs or
fetch; use tools.*".

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

`off`, or no worker, keeps `aimx mcp` exactly as before. `acp:claude-native` is unchanged.

**Timeouts.** `AimMcpServer` gives `run_code`, `exec`, `wait` and `run_program` a 330 s call
timeout: a cell's 300 s maximum deadline plus a margin. Other tools keep 60 s. Claude Code's own
default MCP tool timeout is 1e8 ms, unless `MCP_TOOL_TIMEOUT` or a per-server `timeout` is set.
This was read from the `claude` 2.1.282 binary (`a.MCP_TOOL_TIMEOUT??Es`, `Es=1e8`) and is not
verified against the adapter's bundled build. So long cells are not cut short on Claude's side.

### 5. Benchmark arms

`bench/run.py` accepts an arm per aim harness: `aim_openrouter@off`, `@on` and `@only` set
`AIM_CODE_MODE` for that run. An arm-less aim harness gets the caller's `AIM_CODE_MODE`. The
legacy `AIM_BENCH_CODE_MODE=off` still means `off`, and nothing set means aim's default.
`AIM_CODERUN` is always explicit, and the old missing-worker trick is gone. Arms are not in the
committed wire baseline, so they are compared with `--no-gate`.

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

**The wire gate** (`mise run bench:wire`) fails on this branch. It failed before this change
too, and for other reasons:

- The integration branch already offers 14 tools against the committed baseline's 10, because
  of ADR 0064's UI tools.
- Its W1 is 6,703 bytes, against the 6,000-byte ceiling.

This change adds instruction characters and code-tool bytes, which the gate also reports. The
baseline (`bench/results/w26-main-baseline.json`) and the manifest's aim ceilings need one
acknowledged re-recording after the batch merges: this ADR, task T3's batching guidance and ADR 0064
all change them. In `only`, the scripted W2 trajectory calls `Bash` directly and gets "no tool
named", because the mock does not know the mode. The live cohort, not the mock, measures `only`.

**Other consequences:**

- **What `only` costs.** The model reaches every workspace tool through a cell, so a single
  simple action costs a script. That is the tradeoff the benchmark weighs.
- **Allowlist warnings.** An allowlisted agent in `only` warns that its allowed direct tools are
  not offered (`AllowedTools::unknown`). This is cosmetic.
- **`aim run` shows no warning.** It installs no tracing subscriber, so an invalid
  `AIM_CODE_MODE` there is silent. The daemon logs it.
- **The relay's model is fixed.** It always uses portable `run_code`; there is no catalog there.

## Verification

**Proofs:** `mise run verify` (`crates/aim-kernel/src/code_mode.rs`, 21 obligations). Each spec
is `DRAFT(ADR-0076)` until the benchmark settles the default.

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
| `only`, after the Node hint (a–e) | 3, 4, 3, 8, 16 | 2, 4, 2, 12, 30 | $0.0019–$0.0089 | right, 4/4 in all five |
| `on` | 3 | 2 `Bash` (no `run_code`) | $0.0016 | right, 4/4 |
| `off` | 2 | 2 `Grep` | $0.0017 | right, 4/4 |

- **Before the fixes**, the model's scripts failed silently (see Decision §3), and it answered
  that there were no TODOs.
- **After the fixes**, every `only` run was right, but the cost varied widely. gpt-4.1-mini
  still writes `require('fs')` first. Its two long runs (8 and 16 requests) spent most of their
  calls on `list_programs`, which answers `[]`: in `only`, the program tools distract a weak model.
  That is an input for the benchmark, not fixed here.
- **These are single runs, not a benchmark.** They show the modes work end to end; they do not
  choose the default.

**`acp:claude` (`claude-agent-acp` 0.81.2, `-m sonnet`), `aim run --json`:**

- **`on`:** the strict session started through `aim code-mcp`, so its conformance challenge read
  through `mcp__aim__Read`. Claude made one call, `mcp__aim__run_code`, whose cell called
  `tools.Grep`, a tool hidden from direct use in `on`. It answered right, 4/4. The prompt used
  23,756 input tokens (11,740 cached) and 311 output tokens.
- **`only`:** the session started, so the conformance challenge passed through `run_code`.
  Claude again used one `run_code` cell and answered right.

**Smoke tests:** `live_openrouter_code_mode_only_summarizes_todos_through_run_code` and
`live_acp_claude_code_mode_only_works_through_the_relay` (`crates/aim/tests/code_mode.rs`, run by
`mise run smoke`) passed. The OpenRouter run took 4 requests and 3 top-level calls.
