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
  were stopped as well; their partial reviews carry a `PARTIAL` first line and a provisional verdict. No daemon or test
  server from the agents is left running.
- **`main` is at `eaee693` or later.** The last merges were W30 (UI surfaces), FIX15 (board, with three new locks), FIX19, W29 (MCP) and the REV13b web fixes.
- **Not on `main`:** ten branches, listed in §4. None of them is needed for `main` to work.

## 2. Build and run

### 2.1 Build (once, and after every `git pull`)

```sh
cd ~/projects/aim
git pull                                   # main is 5618108 or later
mise install                               # pinned toolchain; a no-op when already installed
cargo build --release --locked -p aim -p aimx -p aim-coderun     # about 3 min
export PATH="$PWD/target/release:$PATH"    # aim finds aimx and aim-coderun next to itself
aim daemon stop 2>/dev/null                # so the next `aim` starts a daemon from this build
```

- Release binaries are small: `aim` 19.5 MB, `aimx` 7.7 MB, `aim-coderun` 3.7 MB.
- The three binaries always go together:
  - `aim`: the CLI, TUI and daemon;
  - `aimx`: the tool harness that every file, shell and search call goes through;
  - `aim-coderun`: the code-mode sandbox worker, macOS only.
- After rebuilding, always run `aim daemon stop`. Persistent sessions live in a background daemon
  that the TUI starts on demand, and a daemon left over from an old build would keep serving the
  old code.

### 2.2 Credentials

| Provider (`-p`) | Needs | Notes |
|---|---|---|
| `codex` (default) | `~/.codex/auth.json` from the Codex CLI | aim only reads it. `aim login codex` exists but has never been run |
| `openrouter` | `OPENROUTER_API_KEY` | e.g. `-m openai/gpt-4.1-mini` (cheap) or any OpenRouter model id |
| `ai-gateway` | `AI_GATEWAY_API_KEY` | Vercel AI Gateway model ids |
| `acp:claude` | Claude Code signed in (`aim login claude` if not) | Claude Code runs on aim's tools, under aim's authority |

### 2.3 Run

```sh
aim                                  # TUI, inline, codex, in the current directory
aim -p openrouter -m openai/gpt-4.1-mini -C ~/some/repo
aim --fullscreen                     # or toggle with /fullscreen
aim --ephemeral                      # private: nothing written to disk
aim run "fix the failing test"       # headless, one turn; --json prints every event
aim run --ssh myhost -C /srv/app "…" # every tool call runs on myhost (see the gap below)
aim sessions                         # list sessions; `aim --session <id>` re-attaches
```

- **TUI keys:** `Enter` sends. `Ctrl-C` cancels a running turn, clears the composer when idle, and
  quits when pressed twice. `/` opens the commands: `/model`, `/effort`, `/new`, `/sessions`,
  `/cancel`, `/fullscreen`, `/help`, `/quit`. `/dictate` is listed but not wired in the TUI yet.
- **The web UI**, in its own terminal:

  ```sh
  mise run web:build                           # about 40 s; builds crates/aim-web/dist
  aim daemon stop 2>/dev/null
  aim daemon token create                      # prints a bearer once; copy it
  aim daemon --web 127.0.0.1:8080              # foreground; the TUI shares this daemon
  ```

  Open **exactly** `http://127.0.0.1:8080`. `localhost` is a different Origin and is refused.
  Paste the token when the page asks for it.
- **Logs:** `~/.aim/logs/daemon.log`. **Store:** `~/.aim/aim.db`.

### 2.4 Known gaps to expect

- **SSH from your Mac to a Linux host.** The resident mode would need a Linux `aimx` build, and
  aim does not cross-compile one. Until the fix in `main` after `5618108`, aim tried to install the
  macOS `aimx` there and failed; it now falls back to plain SSH commands (agentless mode). A
  Mac-to-Mac host with the same CPU gets the resident aimx.
- **The TUI has no `--ssh` flag.** Start an SSH session with `aim run --ssh <host> -C <dir> "…"`,
  then attach to it from the TUI with `/sessions`. `aim --remote wss://…` (a network aimx) does work
  in the TUI. Adding `--ssh` to the TUI is a small follow-up.
- Board workers are file-only (no shell) until W31 lands (§5).
- Code mode and the plugin worker sandbox are macOS only.

### 2.5 What I'd like you to test

These need a human: judgement of feel and looks, your real machines and accounts. They are in
priority order, and even the first three help a lot.

1. **Daily-drive the TUI on a real task, about 20 minutes.** Run `aim` in this repo or another
   repo you work on, with codex, and give it a genuine small change.
   - Is streaming smooth?
   - Are tool rows readable?
   - Does `Ctrl-C` cancel promptly?
   - Can you type the next prompt while a turn runs?
   - Do `/model` and `/effort` apply?
   - Quit, run `aim` again, and use `/sessions`: the conversation should come back intact.
   - Tell me what feels slow, ugly, confusing or missing compared with Codex CLI and Claude Code.
2. **Inline against fullscreen, in your terminal.** Try both layouts through a long answer, a big
   diff and a window resize. Screenshots of anything that renders wrong are ideal.
3. **SSH shadowing on a host you really use:** `aim run --ssh <host> -C <dir> "create hello.txt
   and list the directory"`.
   - The file must appear **on the host** and nowhere locally.
   - Then attach from the TUI with `/sessions` and continue the conversation.
   - aim installs a resident aimx on the host if it can, and falls back to plain SSH commands if
     not. Tell me the host's OS, and whether `~/.aim` appeared there.
4. **Claude Code through aim:** `aim -p acp:claude`, with one small edit. It should behave like
   Claude Code, but every tool call goes through aim.
5. **Private mode:** `aim --ephemeral`, then chat. Afterwards `aim sessions` must not list it, and
   your prompt history must not contain it.
6. **The web UI** (§2.3): open a session in the browser, send a prompt, and watch it stream. Open
   the same session in the TUI with `/sessions` and see that both follow it.
7. **Your existing MCP servers:** `aim mcp list` should show the servers from your Claude, Codex
   and Cursor configs as untrusted. Trust one with `aim mcp trust <name>`, and a new session
   should offer its tools as `mcp__<name>__…`.
8. **Search your history:** after a few sessions, `aim search-sessions "<something you discussed>"`.
9. **Spot checks:**
   - **Code mode (macOS):** in a session, ask it to "use run_code to count the lines of every .rs
     file under crates/aim/src".
   - **UI surfaces:** ask it to "use ui_show to show a table of the 5 largest files with a
     progress bar".
   - **Codex services:** `aim search "…"`, `aim image "…" -o out.png`.

**Please don't run yet:**
- `aim login codex`: it has not been tested against your shared codex auth.
- `mise run smoke`: it spends money and codex subscription runs across every provider. Individual
  tests are fine.

**What helps me most in a report:**
- what you did, what you expected, and what happened;
- the session id from `aim sessions`;
- a screenshot for anything visual;
- the tail of `~/.aim/logs/daemon.log` if something failed.

Rough notes are enough. I turn each one into a fix task with a regression test.

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
| `agent/kernel/fix18-integration` (`6198674`) | Crash-safe board integration, rescue refs, `aim board apply` (ADR 0068), and REV14 proof gaps | Done: 253 obligations, and the mutations fail as they should. Claude review REV22 was stopped partway, with a provisional **MERGE AFTER FIXES**. (1) **High:** one unreachable or bad recoverable row blocks all integration, including `discard-rescue`; it needs per-row reconcile. (2) Don't start a new result for a target that already has one waiting for apply. (3) Add a theorem that a job is finalized as Integrated only when the target equals the result, plus a theorem pinning the begin state, before locking | Fix 1–3, finish REV22 (mutations, board tests), merge, then lock `integration::{reconcile, next, cleanup_allowed}` |
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