# Handover: pause for maintainer testing (2026-09-25)

Work was paused at the maintainer's request so that `main` can be tested and validated. This page
records what `main` contains, how to exercise it, what is unfinished and where it stands, and
which decisions are waiting for the maintainer. It supersedes nothing: the design is still
[vision](vision.md), [architecture](architecture.md) (the status table in §15 is current up to
W25), and the [ADR index](adr/README.md).

## 1. State at a glance

- **`main`** is green: `mise run check` passes (rustfmt, verusfmt, clippy `-D warnings`, the
  workspace tests, `cargo xtask check` and the offline wire benchmark). `mise run verify` passes:
  **233 kernel obligations, 0 errors, `--no-cheating`**.
- **Locked kernel decisions:** 30, listed in `crates/aim-kernel/LOCKED.toml`. ADR 0065 made the
  checker resolve names by module and cover type aliases, consts and renamed imports. The first
  version had silently left `job::wf` unlocked.
- **All agents are stopped.** Each codex worker committed and pushed its work in progress to its
  own branch (a `wip:` subject if incomplete) and deleted its build directory. The Claude reviewers
  were stopped as well; their partial reviews carry a `PARTIAL` first line. No daemon or test
  server from the agents is left running.
- **`main` is at `eaee693` or later.** The last merges were W30 (UI surfaces), FIX15 (board, with three new locks), FIX19, W29 (MCP) and the REV13b web fixes.
- **Not on `main`:** ten branches, listed in §4. None of them is needed for `main` to work.

## 2. How to build and try it

```sh
mise install                      # the pinned toolchain: Rust 1.98.1, Verus, node, python…
CARGO_PROFILE_DEV_DEBUG=0 cargo build --release -p aim -p aimx -p aim-coderun
export PATH="$PWD/target/release:$PATH"   # aim finds aimx and aim-coderun next to itself
```

**Credentials:**
- **codex:** reads `~/.codex/auth.json`. The agents only borrowed it read-only; `aim login codex`
  exists but was never run.
- **OpenRouter:** `OPENROUTER_API_KEY`.
- **Vercel AI Gateway:** `AI_GATEWAY_API_KEY`.
- **Claude Code:** `aim login claude`, through the pinned `claude-agent-acp`.

If a daemon from an older build is running, stop it first with `aim daemon stop`.

A suggested validation pass, roughly in order of value:

| Try | Command | What to look for |
|---|---|---|
| TUI, inline | `aim` (codex by default) or `aim -p openrouter -m openai/gpt-4.1-mini` | Streaming, tool rows, `/` commands, `/fullscreen` |
| TUI, fullscreen | `aim --fullscreen` | Layout, the side panel |
| Private session | `aim --ephemeral` | Nothing written to `~/.aim` (no session, no history) |
| Headless | `aim run -p openrouter -m openai/gpt-4.1-mini "…"` (add `--json` for events) | One turn, exit code, edits in the cwd |
| SSH shadowing | `aim run --ssh <host> -C /remote/dir "…"` | Every tool call runs on the remote; the local tree is untouched |
| Claude Code | `aim -p acp:claude` | Claude runs on aim's tools, under aim's authority |
| Resume and attach | `aim sessions`, then `aim --session <id>` | The session survives closing the TUI (it lives in the daemon) |
| Search past sessions | `aim search-sessions "…"` | Ranked excerpts |
| Codex services | `aim search "…"`, `aim image "…" out.png`, `aim transcribe f.wav` | Answers, citations, files |
| MCP | `aim mcp list`, `aim mcp trust <name>`, then a session | `mcp__<server>__<tool>` tools. `aim mcp --stdio` serves aim's tools to other agents |
| Board | `aim board post …`, `aim board list`, `aim board show <job>` | Durable jobs. Workers are file-only for now (§5) |
| Web UI | `mise run web:build`, `aim daemon token create`, `aim daemon --web 127.0.0.1:8080` | A browser client with private sessions (Origin, CSP, TLS rules in ADR 0051) |
| Code mode (macOS) | Ask a model to use `run_code` | A sandboxed QuickJS worker, and saved programs |
| UI surfaces (W30) | Ask for "a table of X with a progress bar via `ui_show`" | A rendered table and progress bar in the TUI and the web |
| Offline benchmark | `mise run bench:wire` | The wire tier against the pinned peers (codex, pi) |
| Live smokes | `mise run smoke` | **Costs money and codex subscription runs**; needs every credential |

The live smoke tests (`#[ignore]`, named `live_*`) are the evidence behind every integration
(ADR 0022). Run them selectively with `cargo test -p <crate> -- --ignored live_<name>`.

## 3. What `main` contains

These are merged and reviewed across models: codex reviewed Claude's code, and Claude reviewed
codex's.

- **Harness (aimx):**
  - backends: local, SSH resident with agentless fallback (live-tested on Debian and Alpine),
    WebSocket and HTTP with bearer tokens, TLS off loopback and Origin checks, and
    `Location::Remote`;
  - per-call `CallScope` enforced on resolved paths through the verified policy kernel;
  - token ceilings, and image reservation before paid generation.
- **Providers:** the codex subscription (Responses, remote compaction, media), OpenRouter and AI
  Gateway (Chat), and Claude Code over ACP with strict aim authority.
- **Agent:** compaction (remote, or a local summary), Jev effort advice, skills and agents with
  verified tool ceilings, memory index, conversation search, code mode with saved programs, MCP
  client and server, and board tools.
- **Daemon:** auto-spawn (it now reports why a daemon failed to start), attach and resume, indexed
  summaries, a web listener, and a Leptos browser client.
- **Board:** a verified job lifecycle, workers in isolated worktrees, and serialized integration
  in a detached worktree.
- **Benchmarks:** a recording proxy, the wire tier in the gate and a live coding tier (W22). REV15
  found methodology problems, which W26 is fixing (§4).

## 4. Unfinished work (branches on `origin`, not merged)

The "Next step" column is what the lead would do to land each branch. A merge requires the
cross-model review verdict to be MERGE and the gate to pass on the merged tree. New `LOCKED`
specs are recorded with `mise run locked:update` only after that review.

| Branch | What | State when paused | Next step |
|---|---|---|---|
| `agent/claude/fix17-aimx` (`35ef4e2`) | aimx: authority bound per process and reservation, write-only scopes that cannot read, a reservation journal (ADR 0067) | Codex review REV19 said "MERGE AFTER FIXES B1–B5". All five were fixed, and the branch gate passed with 40/40 live aimx tests | Merge. An optional codex re-check of the B fixes |
| `agent/claude/fix16-coderun` (`4a1b99c`) | Code-mode fixes after REV13a: exact terminate, cell lifetimes, deny-default Seatbelt, one output budget, nested-call events (ADR 0066) | Done, branch gate green. Codex review REV21 was stopped partway, and its findings so far need fixes: in-flight nested calls can run unobserved; close/terminate races; an oversized structured program return fails after its effects; two low program-store issues | Fix the REV21 findings, finish the review, merge |
| `agent/kernel/fix18-integration` (`6198674`) | Crash-safe board integration, rescue refs, `aim board apply` (ADR 0068), and REV14 proof gaps | Done: 253 obligations, and the mutations fail as they should. Claude review REV22 was stopped before a verdict | Finish REV22, merge, then lock `integration::{reconcile, next, cleanup_allowed}` |
| `agent/aim/subagents` (`6aa7afe`) | Native subagents: the `agent` tool, child sessions with narrowed ceilings (ADR 0070) | Done, 242 obligations, live-tested. Claude review REV20 is complete: **MERGE AFTER FIXES**. **High:** a child escapes the parent's ceiling through code mode (`host.rs:349-359`); a fix candidate is in `scratchpad/reviews/REV20-probes/`. **Medium:** `exec`/`wait` are hidden from allowlisted agents; the 8-turn per-child cap never binds; the branch conflicts with `main` (board tests; the `ToolsFactory` signature against W30's UI tools) | Fix, rebase onto `main` (W30 is merged), merge, then lock `subagents::{admit_child_spec, charge_child_turn_spec}` |
| `agent/plugins/wasm` (`04f0720`) | WASM plugins: `aim:plugin@0.1.0`, trust and grants, a lazy sandboxed `aim-plugind` worker (ADR 0057) | Done. `aim` is +1.7% in size; the worker is 40 MB and spawned only for plugins. Claude review REV23 was stopped partway, with a provisional **MERGE AFTER FIXES**: `aim plugin install` does not confine the manifest's `component` path to its own directory (an absolute or `../` path copies any local file into the plugin store). Everything else traced was sound | Confine the install path, finish REV23 (cross-plugin KV test, gate, live smoke), then merge |
| `agent/perf/tokens` (`a1ca119`) | W26: catalog fetched once and off the first request, a compact code-mode index, a bounded Bash output view, and the REV15 benchmark fixes (ADR 0056) | Implemented, gate green. Live results: OpenRouter startup is 167 ms, down from 438, and the paired live pass rate is aim 6/6 against Codex CLI 5/6. Codex cold-HOME startup is a 418 ms median against a 300 ms target. The final report is not written yet | Write the report, then a Claude review |
| `agent/gate/first` (`37e1f4c`) | W28: `aim-gate`: pinned validators, Seatbelt candidates, broker, paired benchmark, signed receipts, ledger, trial deploy (ADRs 0060–0062) | Mostly done: 252 obligations, 21 gate tests. The last live proposal was rejected because the gate's scratch directories were staged ($0.013 spent across three attempts) | Exclude the scratch directories when staging, rerun one trial, write the report, Claude review, lock five specs |
| `agent/aimx/exec-sandbox` (`2eaeb89`, an empty checkpoint) | W31: Seatbelt/bubblewrap profiles for `exec.spawn`, the board worker's shell restored (ADR 0073) | Design only. Decisions 1–3 are answered; question 4 (how the board agent session carries its profile) is open, with a recommendation in `questions/daemon.md` | Answer question 4 and implement |
| `agent/aim/workflows` (`d7cb439`) | W33: declarative DAG workflows with a verified scheduler and a board adapter (ADR 0071) | Implemented: 252 obligations, 13 unit tests, clippy clean. Not yet run: the crash-resume and live tests, and the full gate. There is an open question on board dependency semantics (its option A is implemented) | Finish the tests and gate, write the report, Claude review, lock four specs |
| `agent/aimx/grpc` (`2f699f1`) | W34: aimx over gRPC (tonic, opaque frames, the same auth, TLS, scope and resume rules; ADR 0072) | Done: 81/81 conformance over gRPC, negative tests, and a live TLS OpenRouter edit. `aimx` +5 MB; `fs.read` takes 197 µs over gRPC and WebSocket, 80 µs over unix. The report is not written yet | Write the report, then a Claude review |


**Suggested landing order:** FIX17, then FIX16, FIX18, W32, W27/W27b, W26, then W31 and W28 (they
share the new `aim-sandbox` crate), then W33 and W34.

## 5. Known limits and residual risks

- **Board workers have no shell (ADR 0055).** Workers are confined to file tools inside their
  worktree, and the runner feeds check failures back to them. The owner's check command still
  runs worker-edited code unsandboxed, so its output is a side channel. W31 closes both.
- **The gate (W28) runs as the same OS user.** Candidate sandboxes deny every protected path. The
  test step may use loopback networking only. Promotion targets `gate/trial` only; `main` does
  not move through the gate yet (ADR 0020).
- **Web UI:** any holder of the owner bearer sees every session. That is the design of ADR 0051:
  one owner, many tabs.
- **`aim mcp --stdio`** gives its client aim's local authority (sessions, board, memory, programs).
  That is the documented intent (ADR 0045).
- **MCP servers located in the workspace and using HTTP** are refused until aimx can proxy HTTP
  from the workspace host.
- **Linux:** the plugin worker runs without an OS sandbox, although its guest has no ambient access.
  bubblewrap profiles come with W31.
- **Tests under heavy load:** at a load average of about 30 (six agents building), the W30 worker
  saw two flakes. The `bench:wire` temp-directory cleanup raced the codex peer, and a
  `rev7_daemon` connect was refused. Neither reproduced on rerun.
- **The W22 benchmark numbers need care:**
  - The W4 "prefix stability" ratio measured JSON key order, not caching (REV15).
  - The codex peer's usage was lost by the recorder on four requests.
  - `bench:wire` cannot yet fail on a regression.

  W26 fixes all three.

## 6. Decisions waiting for the maintainer

1. **Tighten the locked `dedup::dedup_next`** (REV14 F3). Today the at-most-once theorem holds for
   any begin table. A one-line change to the locked spec makes the proof depend on the table. It
   needs your approval and a superseding ADR.
2. **The compiled-plugin disk cache (ADR 0057).** Wasmtime's `.cwasm` deserialization is `unsafe`,
   which aim forbids. The choices are to wait for a safe upstream API, or to approve one narrowly
   audited exception with an integrity check. Until then plugins compile in their worker, off the
   startup path.
3. **`aim login codex`** (browser, or `--device`). It is implemented but was never run, because the
   agents share `~/.codex/auth.json` read-only.
4. **How many agents to run in parallel.** Six codex and up to five Claude workers pushed the
   machine to a load average of about 30.
5. **Upcoming, when W31 lands:** the default `exec.spawn` profile for interactive sessions. It stays
   `none` for now; `baseline` would deny reads of `~/.ssh` and credential directories.
6. **Upcoming, when W28 lands:** whether `main` merges move to the gate (ADR 0020), and whether
   the gate gets a dedicated OS user.

## 7. How to resume

- **Worker panes:** the codex workers run in the herdr tab for agents, named `jev`, `ssh-fix`,
  `kernel`, `rev-lead2`, `aimx-ssh` and `daemon`. They are idle, with their context intact.
- **Worktrees:** under `/Users/tomas/projects/aim-wt/`, one per branch.
- **Lead scratchpad:** briefs (`tasks/`), reports, reviews, questions and answers, and each
  worker's stop status (`status/`). It lives in the session's scratchpad directory under
  `/private/tmp/claude-501/…`, which is **temporary**. Everything needed to resume is on this page
  and on the pushed branches.
- **Process** (`CLAUDE.md`):
  - one worktree and branch per worker;
  - a cross-model review before any merge;
  - `mise run check` green on the merged tree;
  - `mise run verify` whenever the kernel changes;
  - live smoke tests for every integration;
  - `LOCKED` specs changed only with the maintainer and a superseding ADR.

- **Open worker questions awaiting the lead:**
  - `questions/rev-lead2.md`: W33's board dependency semantics. Option A is implemented.
  - `questions/daemon.md`, item 4: W31's profile transport for the board session.
- **Resuming a worker:** prompt its pane with "resume <task>: read `status/<name>.md` and the brief". Or start a fresh worker on the branch with the brief in `tasks/`.