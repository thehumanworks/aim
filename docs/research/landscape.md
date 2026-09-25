# R2 — Competitive landscape, token efficiency and benchmarks

Scope: codex, pi, oh-my-pi (omp), unreal-agent, as of 2026-09-25. Sources: shallow clones under
`refs/` (codex `dbb875d` 2026-09-24, pi `5fd446c` 2026-09-25, omp `4a7b586` 2026-09-24, unreal-agent
`1b9f778` 2026-09-23), a local wire-level pilot (F1), and fetched web pages (F6). Token figures are
**chars/4** unless stated. `UNVERIFIED` = not confirmed against code or a fetched page. Code citations
are `refs/<repo>/<path>:<line>`; all 283 were checked to exist and be in range.

## TL;DR

- **Measured on the wire (F1):** static prefix per request is ~0.7k tokens for unreal-agent, ~1.5k for pi,
  ~7.1k for omp, ~9.7k for codex with gpt-5.5 and ~13.2k with gpt-6-astra. Spawn→first request / peak RSS:
  51 ms / 16 MB, 228 ms / 147 MB, 895 ms / 434 MB, 142 ms / 105 MB respectively.
- All four keep history append-only across a tool round trip, and all four can be pointed at a loopback
  Responses endpoint with subscription-style auth (F1, mock). A fair head-to-head is feasible now: the
  user's tny `harness_bench` already runs all four live through a recording proxy (F7).
- **Published:** Unreal Labs reports TB 4.0 at 57.9% for $1,428 vs Codex $2,350 and Pi 55.0% / $1,827
  (gpt-6-astra, xhigh). The saving is fewer turns (28 vs 44) and less input (1.73M vs 2.83M per trial), not
  output. Snorkel's TB 4.0 board has Codex + gpt-6-astra at 58.2% / $3.3k. Methods are only partly disclosed (F6).
- **codex** is the closest architectural precedent for aim: an app-server daemon over stdio/UDS/ws JSON-RPC,
  SQLite projections over JSONL rollouts, a separable `exec-server`, and an agent message board (a
  blackboard; off by default). For gpt-6/5.6 it sends "Responses lite": one JS `exec` tool wrapping shell/patch/MCP
  plus wait/ask/sleep/collab tools (11 sent), websocket deltas, `generate:false` prewarm, remote compaction v2 (F2).
- **Output bounding differs a lot** for a 589 KB command:
  - codex: 10k tokens, head+tail, no path.
  - pi: last 2,000 lines plus a log path.
  - omp: 20 KB + 20 KB plus `artifact://`.
  - unreal: 40k chars plus a path.
- **Compaction:**
  - codex: 90% of window, encrypted remote v2 or a local summary plus 20k user tokens.
  - pi: window − 16k, structured iterative summary, keeps the last 20k.
  - omp: a remote→snapcompact→handoff→shake→soft chain, started speculatively, with cache-aware pruning.
  - unreal: none; its `Report` type is unused.
- **omp** adds hashline edits (a per-file 4-hex tag plus line numbers, Lark-constrained), `xd://` lazy tools,
  an output minimizer, structural read summaries, and a per-turn effort judge whose chain starts with
  `typesafe/jev-latest` (F4).
- **pi:** 4 default tools, a 40-event in-process TypeScript extension API (the one aim's brief cites as inspiration), no
  date in the prompt, and cache warming. Its daemon (CBOR over UDS) and SQLite stores are experimental and
  unwired (F3).
- **unreal-agent**'s async-first design:
  - Tools run in the background as durable, versioned operations.
  - Running placeholders are appended rather than rewritten, and fast results are batched for 1 s.
  - New input preempts the in-flight model call; inboxes are deduplicated.
  - There are no edit tools, no compaction and no TUI (F5).
- None of the four has WASM extensions or SSH tool-call shadowing. omp's `ssh://` covers read/write/grep
  only; codex's remote `exec-server` is the closest precedent.
- Self-reported usage is not comparable: `codex exec` omits subagent tokens, omp logs judge calls separately,
  and pi rotates cache keys per `--no-session` run. Measure at the proxy.
- **Plan (I1, recommendation):** three tiers.
  - Tier 0: an offline wire microbench on every PR, gating prefix size, prefix stability, output bounds,
    latency and RSS.
  - Tier 1: nightly live runs on a port of tny `harness_bench`.
  - Tier 2: per-release TB 4.0, SWE-Atlas QnA, DeepSWE 1.1 and SWE-bench Verified via Harbor.
- **Top levers (I2, opinion):** fewer turns via async batched tools; a small per-request context (spill, summaries,
  cache-timed pruning); correct cache routing including a workspace-shared key; a Verus-checked append-only
  prefix; lazy tools; cache-preserving compaction.

## Findings

### F1. Measured head-to-head pilot (wire level, this machine, 2026-09-25)

Method (FACT): every harness ran headless in a fresh `HOME` and fresh git workspace against a zero-latency
loopback mock of the Responses SSE endpoint that recorded each request body. Configs mirror the user's own
cross-harness adapters (`refs/tny/tests/bench/harness_bench/adapters.py:137-308`); scratch scripts only.
Model string `gpt-5.5`, effort low. Versions: codex-cli 0.158.0-alpha.10, pi 0.87.1 (= ref HEAD version),
omp 18.3.0 (= ref HEAD; run with tny's `--no-skills --no-rules`), unreal-agent built from ref HEAD.
macOS 27 arm64. Latency/RSS: median of 5 warm runs (`/usr/bin/time -l`). Scenario B: the mock's first
reply calls the harness's shell tool with `seq 1 100000` (588,895 B of output), then answers "OK".

| Metric | codex | pi | omp | unreal-agent |
|---|---:|---:|---:|---:|
| 1st request JSON bytes | 38,798 | 6,187 (2,439 on wire, zstd) | 28,519 | 2,854 |
| ≈ static-prefix tokens | ~9.7k | ~1.5k | ~7.1k | ~0.7k |
| instructions + developer/system text (chars) | 21,299 + 5,515 | 2,670 | 9,902 + 429 | 1,566 |
| tools sent: count / JSON chars | 9 / 9,092 | 4 / 3,096 | 11 / 16,548 | 2 / 951 |
| largest tool schemas (chars) | update_goal 2,172; exec_command 1,564 | edit 1,238 | edit 2,871; eval 2,450; task 2,449 | Bash 669 |
| spawn → first request (ms) | 142 | 228 | 895 (727–1,155) | 51 |
| spawn → exit (ms) | 320 | 234 | 928 | 62 |
| max RSS (MB) | 105 | 147 | 434 | 16 |
| install footprint | 239 MB binary + 65 MB `codex-code-mode-host` | 75.6 MB binary | 933 MB `node_modules` (Bun) | 12.1 MB binary |
| B: model-visible output of 589 KB | 40,217 chars: head+tail, `…137224 tokens truncated…`, no spill path | 12,088 chars: last 2,000 lines + `Full output: /tmp/pi-bash-….log` | 51,369 chars: head 4,720 + tail 4,719 lines, `[…90562ln elided…]`, `artifact://0` | 40,310 chars: 20k head + 20k tail, `...548895 bytes truncated; complete output in <path>` |
| B: request 2 = request 1 + appended items (cache-safe) | yes | yes | yes | yes |

- FACT: with `-m gpt-6-astra` codex sends a different layout ("Responses lite"): no top-level
  `instructions`/`tools`; `input[0]` is an `additional_tools` developer item with 11 tools / 20,957 chars
  (`exec` code-mode tool alone 11,048; multi-agent tools `spawn_agent` 2,715 …), then developer messages
  of 28,005 chars; 52,765 B total (~13.2k tokens), `reasoning.context:"all_turns"`. Codex's bundled catalog
  marks every gpt-6/gpt-5.6 model `tool_mode: code_mode_only`, `use_responses_lite: true`, 272k context,
  `truncation_policy {tokens, 10000}` (`refs/codex/codex-rs/models-manager/models.json:4-35`;
  layout switch `refs/codex/codex-rs/core/src/client.rs:912-920`).
- FACT: headers observed — codex: `session-id`, `thread-id`, `originator: codex_exec`, a ~900-byte
  `x-codex-turn-metadata` JSON header, `prompt_cache_key` = thread id, `text.verbosity:"low"`; pi:
  `content-encoding: zstd`, `session-id`, `prompt_cache_key` = session id, `reasoning.summary:"auto"`;
  omp: `session-id` + `session_id` + `conversation_id` + a *different* `thread-id`; unreal: `session-id` =
  `prompt_cache_key` = sha-like session id.
- Caveats: one prompt, zero model latency, one OS; this measures harness overhead, not task quality. omp's
  very first run on this machine took 7.5 s to its first request (Bun transpile cache cold). Pilot scripts
  (scratch, not in the repo): `<scratchpad>/bench/{mock_responses.py,drive.py,check_cites.py}`.

### F2. codex (Rust; openai/codex `codex-rs/`)

- (a) Process: TUI and `codex exec` embed the **app-server in-process** (Rust types, JSON only at external
  boundaries) (`refs/codex/codex-rs/app-server-client/README.md:3-6,29-38`); the TUI has no `codex-core` dependency
  (`refs/codex/codex-rs/tui/Cargo.toml:32-42`) and targets `Embedded | LocalDaemon | Remote`, auto-connecting to a
  daemon socket within 50 ms (`refs/codex/codex-rs/tui/src/lib.rs:277-278,312-321,1031-1039`); `daemon_auto_start`
  is stable/default-on (`refs/codex/codex-rs/features/src/lib.rs:936`). App-server = JSON-RPC over
  `stdio | unix socket (0600) | ws://` — no gRPC/REST (`refs/codex/codex-rs/app-server-transport/src/transport/mod.rs:53-54,81-120`).
  Methods include `thread/start|resume|fork|read|list|compact/start|inject_items|queue/add`,
  `turn/start|steer|interrupt`, notifications `item/*`, `turn/*`, `thread/tokenUsage/updated`
  (`refs/codex/codex-rs/app-server-protocol/src/protocol/common.rs:551-1994`). The daemon survives TUI exit and
  records loaded/interrupted threads across restarts (`refs/codex/codex-rs/app-server-daemon/README.md:32-41,161-192`;
  `refs/codex/codex-rs/app-server-transport/src/daemon_recovery.rs:13-28`). **Tool execution is separable**:
  `exec-server` is a JSON-RPC process/filesystem/http server (default `ws://127.0.0.1:0`, Noise-encrypted
  relay for remote) selected with `CODEX_EXEC_SERVER_URL` (`refs/codex/codex-rs/exec-server/README.md:3-5,36-41`;
  `refs/codex/codex-rs/exec-server-protocol/src/protocol.rs:22-57`; `refs/codex/codex-rs/exec-server/src/environment.rs:56,79`).
- (a) Persistence: JSONL rollouts `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<thread>.jsonl` are the source
  of truth (`refs/codex/codex-rs/rollout/src/recorder.rs:1727-1731`; `refs/codex/codex-rs/rollout/src/rollout_file_name.rs:67-70`);
  SQLite (WAL, `synchronous=Normal`) holds projections: `state_5`, `thread_history_1`, `logs_2`,
  `goals_1`, `memories_1`, `queue_1` (`refs/codex/codex-rs/state/src/sqlite.rs:29-34,302-308`), no FTS. Fork =
  truncate rollout at a turn (`thread/fork` `last_turn_id`/`before_turn_id`)
  (`refs/codex/codex-rs/core/src/thread_rollout_truncation.rs:169`; `refs/codex/codex-rs/core/src/thread_manager.rs:188-206`).
- (b) Prompt: per-model `model_messages.instructions_template` in the bundled catalog (refreshed from
  `/models` with ETag, 300 s TTL) (`refs/codex/codex-rs/models-manager/models.json:75-76`,
  `refs/codex/codex-rs/models-manager/src/manager.rs:32,63`); default model gpt-6-astra (`refs/codex/codex-rs/models-manager/models.json:4,69-74`).
  Template sizes: gpt-6-astra 21,428 B (~5.4k tok), gpt-6-sol 18,998, gpt-5.6-* 17,766, gpt-5.5 21,473;
  the `core/*_prompt.md` files are dead code. Base instructions are frozen per thread
  (`refs/codex/codex-rs/core/src/session/mod.rs:738-742`); everything dynamic is a "world state" section
  (AGENTS.md ≤ 32 KiB, permissions, collaboration mode, `<environment_context>` with cwd/shell/**date**/
  timezone, apps, plugins; `refs/codex/codex-rs/core/src/session/world_state.rs:117-271`,
  `refs/codex/codex-rs/core/src/agents_md.rs:43-68`). After the first turn only **diffs are appended** (model switch,
  AGENTS.md "replace all previously provided", env fields that changed, effort as `configuration_update`
  items) against a `reference_context_item` baseline (`refs/codex/codex-rs/core/src/session/mod.rs:4632-4698`;
  `refs/codex/codex-rs/core/src/context/world_state/environment.rs:135-194`;
  `refs/codex/codex-rs/core/src/session/reasoning_effort.rs:1,77-90`). Under Responses Lite the tools item and the
  instructions item get UUIDv5 ids over thread id + payload so retries/resume stay byte-identical
  (`refs/codex/codex-rs/core/src/client.rs:912-943`). `prompt_cache_key` = root thread id, or
  `"{source}:{parent_thread_id}"` for internal sub-requests (`refs/codex/codex-rs/core/src/client.rs:585-599`). Skills list budget: 2% of
  window, default 8,000 chars (`refs/codex/codex-rs/ext/skills/src/render.rs:17-21`).
- (c) Tools: `tool_mode` Direct | CodeMode | CodeModeOnly; all gpt-6/5.6 models are `code_mode_only`
  (`refs/codex/codex-rs/models-manager/models.json:20`; `refs/codex/codex-rs/core/src/tools/mod.rs:74-84`). Code-mode-only hides every nested tool
  behind one freeform JS tool **`exec`** (V8 isolate; Lark grammar `pragma_source | plain_source`,
  `refs/codex/codex-rs/core/src/tools/code_mode/execute_spec.rs:18-26`); nested tools are described as TypeScript
  `declare const tools` (`refs/codex/codex-rs/code-mode-protocol/src/description.rs:417-429`) and **only what the
  script prints reaches the model** (10k-token cap; `refs/codex/codex-rs/core/src/tools/code_mode/mod.rs:286-326`).
  Direct mode (gpt-5.5): `exec_command`, `write_stdin` (unified exec, PTY optional), `apply_patch`
  (freeform only, grammar 578 B, `refs/codex/codex-rs/core/assets/tools/apply_patch.lark:1`), `view_image`,
  `request_user_input`, goal tools, `tool_search`, optional MCP resource tools and `multi_agent_v1`
  (F1 measured 9 tools / 9,092 chars). Unified exec: yield 250–30,000 ms, default 10,000 output tokens,
  1 MiB head/tail buffer, ≤ 64 processes with LRU eviction, forced `NO_COLOR=1 TERM=dumb PAGER=cat`
  (`refs/codex/codex-rs/core/src/unified_exec/mod.rs:73-82`, `refs/codex/codex-rs/core/src/unified_exec/process_manager.rs:93-104,1728-1799`).
  MCP tools are **deferred behind `tool_search`** (BM25, limit 8) whenever the model supports it — no
  count threshold (`refs/codex/codex-rs/core/src/mcp_tool_exposure.rs:90-94`; `refs/codex/codex-rs/tools/src/tool_discovery.rs:7`).
  Truncation: catalog `truncation_policy {tokens, 10000}` (bytes/4), 50/50 head/tail, marker
  `…N tokens truncated…`, re-truncated at 1.2× when recorded to history; no spill file
  (`refs/codex/codex-rs/utils/string/src/truncate.rs:4,126-137`; `refs/codex/codex-rs/core/src/context_manager/history.rs:449-452`).
  HEAD #47957 only bounds warehouse-only tool metadata to a 15 MiB wire budget
  (`refs/codex/codex-rs/core/src/client_tool_metadata.rs:2,10`), not model-visible output.
- (d) Compaction: auto at 90% of the window (244,800 of 272k), hard at 95%; checked pre-turn and
  mid-turn (`refs/codex/codex-rs/protocol/src/openai_models.rs:525-535`; `refs/codex/codex-rs/core/src/session/context_window.rs:84-110`;
  `refs/codex/codex-rs/core/src/session/turn.rs:598-621,1271-1283`). **Remote v2** (OpenAI/Azure/Bedrock providers):
  the normal streaming request plus a trailing `{"type":"compaction_trigger"}` item, same tools and
  instructions (prefix stays cached), returns `{"type":"compaction","encrypted_content"}`; afterwards keeps
  user messages + agent messages ≤ 10k tokens within a 64k-token budget, then the compaction item
  (`refs/codex/codex-rs/core/src/compact_remote_v2.rs:75-76,401-416,454`; `refs/codex/codex-rs/core/src/compact_remote_v2_attempt.rs:78-87`).
  Local fallback: prompt "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for
  another LLM that will resume the task…" (426 B, `refs/codex/codex-rs/prompts/templates/compact/prompt.md`), sent
  **without tools**; keeps ≤ 20,000 tokens of the newest user messages + `SUMMARY_PREFIX` ("Another language
  model started to solve this problem and produced a summary…") (`refs/codex/codex-rs/core/src/compact.rs:59,281-284,667-740`).
  No other pruning; old reasoning is kept; token estimate = last server total + ceil(bytes/4) of new items
  (`refs/codex/codex-rs/core/src/context_manager/history.rs:798-815`).
- (g-transport) zstd request bodies on the ChatGPT backend (`refs/codex/codex-rs/features/src/lib.rs:1272`;
  `refs/codex/codex-rs/core/src/client.rs:1627-1635`). WebSocket (`OpenAI-Beta: responses_websockets=2026-02-06`) sends
  `previous_response_id` + only new items when the request is otherwise identical, prewarms with
  `generate:false`, keeps `x-codex-turn-state` sticky routing (`refs/codex/codex-rs/core/src/client.rs:175,293-304,337-392,1390-1426,2010-2016`).
  HTTP never uses `previous_response_id`.
- (e) Extensibility: 12 hook events (PreToolUse … Stop, Interrupt) with command/MCP/prompt/agent
  handlers; hooks can block, rewrite tool input and MCP output; `additionalContext` capped at 2,500 tokens
  (`refs/codex/codex-rs/protocol/src/protocol.rs:1579-1601`; `refs/codex/codex-rs/hooks/src/schema.rs:238-272`;
  `refs/codex/codex-rs/hooks/src/output_spill.rs:12`). Plugins bundle skills + MCP servers + apps + hooks and also read
  `.claude-plugin`/`.cursor-plugin` manifests (`refs/codex/codex-rs/plugin/src/manifest.rs:19-24`;
  `refs/codex/codex-rs/utils/plugins/src/plugin_namespace.rs:134-135`). Skills: `SKILL.md` under `~/.agents/skills`,
  `.codex/skills`, and **`.agents/skills` in every dir from repo root to cwd**; metadata-only catalog,
  body read on demand (`refs/codex/codex-rs/ext/skills/src/host_roots.rs:86-171`; `refs/codex/codex-rs/ext/skills/src/fragments.rs:40-41`). MCP client
  rmcp 3.2 (`refs/codex/codex-rs/Cargo.toml:443`); codex is **not** an MCP server. Extensions are compiled-in Rust
  crates implementing contributor traits — no WASM (`refs/codex/codex-rs/ext/extension-api/src/lib.rs:46-85`). Code
  mode runs in a separate, lazily spawned `codex-code-mode-host` (V8 150.4, sandboxed), reachable over
  stdio or gRPC (`refs/codex/codex-rs/code-mode/src/remote_session.rs:34`; `refs/codex/codex-rs/code-mode-host/src/main.rs:21`).
- (f) Multi-agent: in-process threads, each with its own rollout; edges in SQLite `thread_spawn_edges`
  (`refs/codex/codex-rs/agent-graph-store/src/store.rs:13-43`). V1 tools `spawn_agent/send_input/resume_agent/wait_agent/
  close_agent`; V2 (namespace `collaboration`, chosen by the model catalog for gpt-6) adds `send_message`
  ("does not trigger a new turn"), `followup_task`, `list_agents`, `interrupt_agent`
  (`refs/codex/codex-rs/core/src/tools/handlers/multi_agents_spec.rs`; `refs/codex/codex-rs/core/src/config/mod.rs:1574-1616`).
  Limits: 6 threads, depth 1, V2 4 concurrent per session, wait 10 s–1 h (`refs/codex/codex-rs/core/src/config/mod.rs:253-263`).
  **A blackboard already exists but is off**: `agent_message_board` (UnderDevelopment) — per-tree board with
  channels, threads, `search_posts`, subscriptions, idempotent `post(request_id)`, SQLite
  `agent_message_board_1.sqlite`, 64 KiB posts; pushes only into *running* turns, idle agents must poll
  (`refs/codex/codex-rs/features/src/lib.rs:1331`; `refs/codex/codex-rs/ext/agent-message-board/src/api.rs:24-148`;
  `refs/codex/codex-rs/ext/agent-message-board/src/local.rs:1-89`; `refs/codex/codex-rs/core/src/agent_message_board.rs:159-199`).
- (g) Perf: ratatui 0.30 **inline viewport** — finished history is written into normal scrollback, alt
  screen only for overlays (`refs/codex/codex-rs/tui/src/tui.rs:431`; `refs/codex/codex-rs/tui/src/insert_history.rs:1-5`); 120 FPS
  cap with a frame-scheduler actor (`refs/codex/codex-rs/tui/src/tui/frame_rate_limiter.rs:4-13`); adaptive stream
  pacing (`refs/codex/codex-rs/tui/src/streaming/chunking.rs:85-116`). Startup prewarms MCP, shell snapshot and the
  websocket (`refs/codex/codex-rs/core/src/session/startup_prewarm.rs:1-3`; `refs/codex/codex-rs/core/src/session/mcp_prewarm.rs:1-4`). Release: thin
  LTO, 4 CGUs, jemalloc (`refs/codex/codex-rs/Cargo.toml:611-620`; `refs/codex/codex-rs/cli/src/main.rs:50-51`). OTEL metrics
  include `codex.turn.ttft.duration_ms`, `codex.websocket.continuation`, `codex.startup.phase.duration_ms`
  (`refs/codex/codex-rs/otel/src/metrics/names.rs:15-61`). F1: 142 ms to first request, 105 MB RSS, 239 MB binary.
- (h) Evals: only two divan micro-benches (`codex --help`, image prompts)
  (`refs/codex/codex-rs/cli/e2e_benches/codex_help.rs:14-25`); no TB/SWE-bench harness. `responses-api-proxy` forwards
  `POST /v1/responses` with an injected key and `--dump-dir` request/response pairs, no websocket
  (`refs/codex/codex-rs/responses-api-proxy/README.md:31-79`).
- Headless/endpoints: ChatGPT base `https://chatgpt.com/backend-api/codex`, API `https://api.openai.com/v1`
  (`refs/codex/codex-rs/model-provider-info/src/lib.rs:77,425-427`); custom `[model_providers.<id>]` with
  `wire_api="responses"` only — **Chat Completions removed** (`refs/codex/codex-rs/model-provider-info/src/lib.rs:96,126,140-191`). `codex exec --json`
  emits `turn.completed.usage{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens,
  reasoning_output_tokens}` = **cumulative main-thread totals; subagent tokens excluded**
  (`refs/codex/codex-rs/exec/src/exec_events.rs:60-73`; `refs/codex/codex-rs/exec/src/event_processor_with_jsonl_output.rs:118-127`;
  `refs/codex/codex-rs/exec/src/lib.rs:1636-1638`). Effort only via `-c model_reasoning_effort=…`.

### F3. pi (TypeScript; earendil-works/pi, coding-agent 0.87.1)

- (a) Process: released CLI = **one Node process**; modes TUI, `-p`, `--mode json`, `--mode rpc` (LF-framed
  JSONL on stdio, 33 commands incl. `prompt|steer|follow_up|abort|compact|fork|get_session_stats`; ~30 event
  types) (`refs/pi/packages/coding-agent/docs/cli.md:44-47`; `refs/pi/packages/coding-agent/docs/rpc.md:3,37-54`;
  `refs/pi/packages/coding-agent/docs/rpc-commands.md:7-782`; `refs/pi/packages/coding-agent/docs/json.md:54-193`). Extensions and SDK run in-process
  (`refs/pi/packages/coding-agent/docs/how-pi-works.md:39-43`). An **experimental daemon** exists behind `PI_EXPERIMENTAL=1`: server +
  one session-worker process per session + presentation clients, length-prefixed **CBOR over a Unix socket**
  (protocol v8, 16 MiB frames), outbound WebSocket only to a hosted relay; not in published entrypoints
  (`refs/pi/packages/coding-agent/src/core/experimental.ts:2`; `refs/pi/packages/protocol/README.md:13-41`;
  `refs/pi/packages/protocol/src/protocol.ts:5`; `refs/pi/packages/server/src/transports/unix/listener.ts:57`;
  `refs/pi/packages/agent/docs/plugins.md:16-31`). `AgentHarness` spec: durable intent/settlement records around
  every provider request and tool call, entries + values + append-only usage ledger (`refs/pi/packages/agent/docs/harness.md:21-54`).
- (a) Persistence: append-only **JSONL tree** (`id`/`parentId`, v3) at `~/.pi/agent/sessions/--<path>--/<ts>_<uuidv7>.jsonl`;
  `/tree` moves the leaf in-file, `/fork` copies to a new file, `/clone` copies the branch; one
  `appendFileSync` per entry, no fsync, nothing written before the first assistant message
  (`refs/pi/packages/coding-agent/docs/session-format.md:3-231`; `refs/pi/packages/coding-agent/docs/sessions.md:26-28`;
  `refs/pi/packages/coding-agent/src/core/session-manager.ts:41,1160-1187`). Two **SQLite schemas exist but are unwired**:
  `session-backends/sqlite-node` (entries/values/usage_ledger, WAL) and `durable` (conversations, tasks with
  `background` + owner subtree, scoped documents with base/delta revisions, WAL + `synchronous=NORMAL`)
  (`refs/pi/packages/session-backends/sqlite-node/src/sqlite/migrations/001_initial.sql:1-121`;
  `refs/pi/packages/durable/src/storage/sqlite/migrations.ts:10-77`; `refs/pi/packages/durable/src/types.ts:129-151`).
- (b) Prompt: XML-sectioned (`<tools>` one-line snippets, `<rules>`, `<docs>`, `<project_context>`,
  `<skills>`, `<cwd>`) (`refs/pi/packages/coding-agent/src/core/system-prompt.ts:121-180`); ~2.6k chars (~650 tokens),
  47% of it pi self-docs; **no date** since v0.80.7 "Fixed system prompt cache invalidation across dates"
  (`refs/pi/packages/coding-agent/CHANGELOG.md:955`). AGENTS.md/CLAUDE.md per directory; skills metadata-only
  (`refs/pi/packages/coding-agent/src/core/resource-loader.ts:127`; `refs/pi/packages/coding-agent/src/core/skills.ts:362-380`). Mid-session prompt/tool changes are
  appended as section patches only where the model supports mid-conversation system messages; otherwise
  prompt changes rebuild the leading prompt and **invalidate the cache** (`refs/pi/packages/ai/src/utils/transcript.ts:105-120`;
  `refs/pi/packages/coding-agent/docs/extensions.md:150`).
- (c) Tools: 8 built-ins, default **read, bash, edit, write** (+ grep/find/ls/powershell opt-in)
  (`refs/pi/packages/coding-agent/src/core/tools/index.ts:95-105`; `refs/pi/packages/coding-agent/src/core/sdk.ts:258`); measured 3,096 chars on
  the wire (F1). Parallel tool execution by default (`refs/pi/packages/agent/src/agent.ts:253`). Edit =
  `{path, edits:[{oldText,newText}]}`, each unique against the original, normalization-only fuzzing (NFKC,
  quotes, dashes), result is one line "Successfully replaced N block(s)" (`refs/pi/packages/coding-agent/src/core/tools/edit.ts:21-51,205-210`;
  `refs/pi/packages/coding-agent/src/core/tools/edit-diff.ts:34-54,328-350`). Read: no line numbers, 2,000 lines/50 KB head + "Use offset=Z to
  continue" (`refs/pi/packages/coding-agent/src/core/tools/read.ts:156-169`). Bash: `spawn` with pipes, **no PTY, no background jobs**, optional
  timeout, **tail** 2,000 lines/50 KB, full log `/tmp/pi-bash-<hex>.log` only when truncated
  (`refs/pi/packages/coding-agent/src/core/tools/truncate.ts:11-13`; `refs/pi/packages/coding-agent/src/core/tools/bash.ts:38-46,96-120,239,333`; `refs/pi/packages/coding-agent/src/core/tools/output-accumulator.ts:19-22`).
- (d) Context: compaction when `contextTokens > contextWindow − reserveTokens` (16,384), keeping the newest
  ~20,000 tokens verbatim; checked between turns and on overflow (one compact-and-retry)
  (`refs/pi/packages/coding-agent/src/core/compaction/compaction.ts:148-152,289-291`; `refs/pi/packages/coding-agent/docs/compaction.md:37-96`).
  Summary prompt: "Create a structured context checkpoint summary that another LLM will use to continue the
  work" with `## Goal / Constraints & Preferences / Progress (Done/In Progress/Blocked) / Key Decisions /
  Next Steps / Critical Context`, "Preserve exact file paths, function names, and error messages";
  iterative `<previous-summary>` update ("PRESERVE all existing information"); tool results cut to 2,000
  chars in the serialized transcript; `<read-files>`/`<modified-files>` lists appended
  (`refs/pi/packages/coding-agent/src/core/compaction/compaction.ts:529-601`; `refs/pi/packages/coding-agent/src/core/compaction/utils.ts:29-150`). No automatic pruning; `context_edit` entries
  can omit/replace earlier items (extensions only) (`refs/pi/packages/coding-agent/docs/session-format.md:137-145`). Cache:
  Anthropic 3–4 `cache_control` breakpoints; OpenAI `prompt_cache_key` = session UUIDv7 (so every
  `--no-session` print run gets a fresh key), `prompt_cache_retention:"24h"` when long
  (`refs/pi/packages/ai/src/api/anthropic-messages.ts:1087-1109`; `refs/pi/packages/ai/src/api/openai-responses.ts:83-100`;
  `refs/pi/packages/coding-agent/src/main.ts:363-364`). **Cache warming**: replays the request with a 1-token cap at
  90% of the TTL when expected savings ≥ $0.05, default "streaming" (`refs/pi/packages/coding-agent/src/core/cache-warmer.ts:15-58`).
- (e) Extensibility (the model aim's brief cites): TS modules loaded by jiti, in-process, full user rights
  (`refs/pi/packages/coding-agent/docs/extensions.md:5-38`). **40 events** incl. `context`, `context_with_system`,
  `before_provider_request` (payload replace), `tool_call` (block/mutate), `tool_result` (replace),
  `message_end`, `turn_end`/`agent_before_settle` (append + continue), `cache_warming_decision`
  (`refs/pi/packages/coding-agent/src/core/extensions/types.ts:1222-1436`). Registration: tools, commands, shortcuts,
  flags, message/entry renderers, markdown transformers, providers, `setActiveTools`, `setModel`,
  `setThinkingLevel` (`refs/pi/packages/coding-agent/src/core/extensions/types.ts:1443-1640`). UI: dialogs, status/widgets/header/footer, `custom()` overlays,
  editor replacement, autocomplete providers, themes (JSON, hot reload in agent dir)
  (`refs/pi/packages/coding-agent/src/core/extensions/types.ts:145-293`; `refs/pi/packages/coding-agent/docs/themes.md:40-47`). Packages via npm/git (`refs/pi/packages/coding-agent/docs/packages.md:12-14`).
- (f) Multi-agent: none built in; example `subagent` extension spawns `pi --mode json -p --no-session`
  processes (≤ 8 tasks, 4 concurrent) (`refs/pi/packages/coding-agent/examples/extensions/subagent/index.ts:33-34,300-346`).
  `durable` models background tasks and owner subtrees but is unused.
- (g) Perf: Node ≥ 22 esbuild bundle or `bun build --compile` binary (`refs/pi/scripts/build-binaries.sh:131-133`); lazy
  provider modules (`refs/pi/packages/ai/src/api/anthropic-messages.lazy.ts:4`). TUI: main-screen (keeps scrollback)
  **differential rendering** — redraw from first changed line, full redraw on width change or change above
  viewport, all inside synchronized output `CSI ?2026h/l` (`refs/pi/packages/tui/README.md:3,61-62,741-749`).
  F1: 228 ms to first request, 147 MB RSS, 75.6 MB binary, zstd request bodies on the codex backend.
- (h) Evals: `vitest-evals` "documentation-lift" suites in Docker (`with_docs` vs `without_docs`), reporting
  pass-rate lift, input/output/cacheRead/cacheWrite tokens, tool calls, estimated $; no TB/SWE-bench
  (`refs/pi/packages/evals/README.md:3-103`; `refs/pi/packages/evals/src/report.ts:22-53`). Session analysers
  (`refs/pi/scripts/session-context-stats.mjs`, `refs/pi/scripts/edit-tool-stats.mjs`) mine real sessions.
- Providers/headless: `openai-codex` → `https://chatgpt.com/backend-api` + `/codex/responses`, transport
  auto (WebSocket first, SSE fallback), OAuth browser + device code (`refs/pi/packages/ai/src/providers/openai-codex.ts:9-17`;
  `refs/pi/packages/ai/src/api/openai-codex-responses.ts:294-370,641-654`); OpenAI-compatible via `models.json`
  `api: "openai-completions" | "openai-responses"`, a `baseUrl`-only provider entry reroutes built-ins
  (`refs/pi/packages/coding-agent/docs/models.md:12-64`; `refs/pi/packages/coding-agent/src/core/provider-composer.ts:306-311`). Usage per assistant
  message `{input, output, cacheRead, cacheWrite, reasoning?, cost}` (`refs/pi/packages/ai/src/types.ts:420-441`).
  With Anthropic OAuth pi injects a Claude Code identity block and renames tools
  (`refs/pi/packages/ai/src/api/anthropic-messages.ts:86-100`).

### F4. oh-my-pi / omp (TypeScript on Bun + Rust N-API natives; fork of pre-daemon pi)

- (a) Process: one Bun process for TUI, `-p`, `--mode json|rpc|acp|rpc-ui`, SDK; natives in-process ("No
  fork/exec on the hot path"); subagents run on the main thread (`refs/oh-my-pi/README.md:452,498`; `refs/oh-my-pi/docs/cli-reference.md:155,198-202`;
  `refs/oh-my-pi/packages/coding-agent/src/task/executor.ts:2-4`). RPC = NDJSON on stdio, 1 MiB frames (v2 chunking to 64 MiB),
  hosts can register their own tools/URI schemes (`refs/oh-my-pi/docs/rpc.md:3,35-55,138-139`). **No agent daemon**; side
  services: per-project `launch` broker supervising long-lived processes and sharing LSP servers, auth broker
  (SQLite vault) + auth gateway proxy, collab relay (`refs/oh-my-pi/packages/coding-agent/src/launch/broker.ts:38`;
  `refs/oh-my-pi/docs/auth-broker-gateway.md:5-6`; `refs/oh-my-pi/docs/collab.md:3`). Sessions: JSONL tree (v3, `id`/`parentId` + leaf)
  with images in a content-addressed blob store and full tool/subagent outputs in per-session artifact dirs;
  SQLite only for history/title/recap indexes; `/tree`, `/fork`, `branch`, `handoff`; imports Claude Code and
  Codex sessions (`refs/oh-my-pi/docs/session.md:10,37-63,400-553`; `refs/oh-my-pi/docs/blob-artifact-architecture.md:9-10`;
  `refs/oh-my-pi/docs/cli-reference.md:76-77`).
- (b) Prompt: Handlebars templates, dense RFC-2119 register — `system-prompt.md` 13,122 B + project 2,498 B +
  personality 899 B ≤ ~4.1k tokens before context files/rules/skills
  (`refs/oh-my-pi/packages/coding-agent/src/prompts/system/system-prompt.md:1-246`); F1 measured 9,902 + 429 chars. With
  native tool calls the prompt lists only tool **names** (`refs/oh-my-pi/packages/coding-agent/src/system-prompt.ts:886`).
  **Date and cwd live in a first-user-turn reminder appended only on change** "so the bytes are stable"
  (`refs/oh-my-pi/packages/coding-agent/src/session/date-cwd-reminder.ts:1-42`); append-only context mode freezes
  system+tools (`refs/oh-my-pi/packages/agent/src/append-only-context.ts:1-15`); withdrawn tools are re-declared
  byte-identically (`refs/oh-my-pi/packages/agent/src/sent-tool-definitions.ts:3-8`); model id is in the prompt by default.
  `priority.json` holds per-role model fallback chains; the **judge** chain starts with `typesafe/jev-latest`
  (`refs/oh-my-pi/packages/coding-agent/src/priority.json:116-122`).
- (c) Tools: 30 registry names; 15 enabled in the TUI, but `tools.xdev` (default on) **moves "discoverable"
  tools out of the tools array** into `xd://<tool>` devices driven via read/write, so the request carries 11
  (read, write, edit, bash, eval, glob, grep, task, wait, todo, web_search) (`refs/oh-my-pi/packages/coding-agent/src/tools/builtin-names.ts:1-36`;
  `refs/oh-my-pi/packages/coding-agent/src/tools/settings.ts:868-877`; `refs/oh-my-pi/packages/coding-agent/src/tools/xdev.ts:1-58`). Edit default
  = **hashline**: read returns `[path#TAG]` + `N:TEXT`, TAG = 4-hex xxh32 of the whole normalized file (not
  per-line); edits are `{input}` text with `PUT N.=M:` / `PUT N*:` (tree-sitter block) / inserts / `CUT` / `MV`,
  `+TEXT` bodies only, lines referencing the read snapshot; Lark-constrained on OpenAI; edits allowed only on
  lines the model has seen; per-model fallback to `replace` (Kimi, MiniMax, DeepSeek, …)
  (`refs/oh-my-pi/crates/pi-edit/src/store.rs:69-80`; `refs/oh-my-pi/crates/pi-edit/prompts/hashline.md:1-31`;
  `refs/oh-my-pi/packages/coding-agent/src/edit/settings.ts:9-136`; `refs/oh-my-pi/packages/coding-agent/src/utils/edit-mode.ts:20-43`). Shell: embedded
  brush with in-process builtins, persistent sessions, PTY only when asked and a UI exists, `async:true` jobs
  and auto-background after 60 s (≤ 100 jobs), 300 s default timeout (`refs/oh-my-pi/docs/bash-tool-runtime.md:93-127`;
  `refs/oh-my-pi/packages/coding-agent/src/exec/settings.ts:110-113,252-255`). An **output minimizer** (23 Rust filters + 67 TOML
  defs) rewrites verbose git/npm/cargo output, raw kept as `artifact://` (`refs/oh-my-pi/docs/bash-tool-runtime.md:217-219`).
  Generic spill: results > 50 KB keep first 20 KB + last 20 KB inline, full text in `artifact://<id>`; bash has
  its own split (F1 measured ≈22 KB head + ≈28 KB tail inline)
  (`refs/oh-my-pi/packages/coding-agent/src/tools/settings.ts:14-43`; `refs/oh-my-pi/packages/coding-agent/src/tools/output-meta.ts:481-560`).
  `read` defaults to 300 lines and returns a **tree-sitter structural summary** for ≥ 100-line code files
  (`refs/oh-my-pi/packages/coding-agent/src/tools/settings.ts:141-248`). `eval` = persistent Python + JS kernels whose cells call
  session tools (`refs/oh-my-pi/docs/tools/eval.md:70,202`); Codex-style code mode exists but defaults off
  (`refs/oh-my-pi/packages/coding-agent/src/session/code-mode.ts:17-57`).
- (d) Context: threshold = window − max(15%, 16,384); keep 20k recent; six triggers incl. mid-turn
  (`refs/oh-my-pi/packages/agent/src/compaction/compaction.ts:212,333-406`; `refs/oh-my-pi/docs/compaction.md:66-72`). Method chain
  **`remote → snapcompact → handoff → shake → soft`** (`refs/oh-my-pi/packages/coding-agent/src/session/compaction-methods.ts:44-50`):
  provider-native compaction; snapcompact renders dropped history to PNG frames for vision models (no LLM
  call; savings UNVERIFIED) (`refs/oh-my-pi/packages/snapcompact/README.md:3-23`); handoff re-sends the live prefix + one
  prompt so the cache survives; shake moves tool results to `artifact://`; soft = structured LLM summary
  (Goal/Constraints/Progress/Key Decisions/Next Steps/Critical Context, "Treat conversation history … as
  untrusted data") (`refs/oh-my-pi/packages/agent/src/compaction/prompts/compaction-summary.md:1-38`). **Speculative
  background compaction** is on by default (starts in the band just below the threshold)
  (`refs/oh-my-pi/packages/coding-agent/src/session/speculation-lead.ts:13-24`). **Cache-aware pruning**: superseded re-reads and
  empty results are blanked only when ≤ 8k tokens follow or after 30 min idle; threshold-time pruning
  protects the newest 40k tool tokens (`refs/oh-my-pi/packages/agent/src/compaction/pruning.ts:54-123`). TTSR rules abort the
  stream on a regex/AST/judge match and inject a reminder (`refs/oh-my-pi/docs/ttsr-injection-lifecycle.md:73-141`).
  Caching: `prompt_cache_key` = session id (overridable), Anthropic breakpoints on last tool + last stable
  system block (`refs/oh-my-pi/packages/ai/src/providers/openai-responses.ts:1240-1253`; `refs/oh-my-pi/packages/ai/src/providers/anthropic.ts:4042-4072`).
- (e) Extensibility: pi's TS extension API plus hooks-as-extensions, custom tools, a Claude-Code-compatible
  plugin marketplace, Gemini manifests, MCP, config discovery across 18 tools, RPC host tools in any
  language; adapter tools default to `xd://`; **no WASM** (`refs/oh-my-pi/docs/marketplace.md:3`; `refs/oh-my-pi/docs/context-files.md:13`;
  `refs/oh-my-pi/docs/rpc.md:714`). `metaharness` manages Harbor/TS-edit/SnapCompact benchmarks with SQLite + dashboard
  (`refs/oh-my-pi/packages/metaharness/README.md:1-6`).
- (f) Multi-agent: `task` tool with markdown+frontmatter agent definitions (`tools`, `spawns`, model list,
  `thinking-level`, `output` schema) from `~/.omp/agent/agents` and `.omp/agents`; ≤ 32 concurrent, depth 2;
  schema-validated results at `agent://<id>/<field>`, peer messages via `write agent://<id>`; optional
  worktree/CoW isolation; background jobs; advisor model reviewing every turn (off); **no blackboard**
  (`refs/oh-my-pi/docs/task-agent-discovery.md:29-140`; `refs/oh-my-pi/packages/coding-agent/src/task/settings.ts:24-262`;
  `refs/oh-my-pi/packages/coding-agent/src/prompts/tools/task.md:5-19`; `refs/oh-my-pi/docs/advisor-watchdog.md:3-5`).
- (g) Perf: `--thinking auto` asks a judge model once per user turn to pick low…xhigh ("If torn between
  levels, choose the lower one"), 4 s timeout, never re-judged mid-turn (`refs/oh-my-pi/packages/coding-agent/src/auto-thinking/classifier.ts:1-67`;
  `refs/oh-my-pi/packages/coding-agent/src/session/model-controls.ts:593-665`). Speculative execution of validated local reads
  as soon as arguments finish streaming (off; `refs/oh-my-pi/packages/coding-agent/src/tools/settings.ts:795-811`;
  `refs/oh-my-pi/packages/agent/src/agent-loop.ts:2320-2325`). SSH = `ssh://host/path` for read/write/grep only (UTF-8, ≤ 1 MiB,
  ControlMaster), shell via `ssh` in bash — **no tool-call shadowing** (`refs/oh-my-pi/packages/coding-agent/src/internal-urls/ssh-protocol.ts:1-46`;
  `refs/oh-my-pi/packages/coding-agent/src/ssh/connection-manager.ts:268`). TUI diff-renders per frame with synchronized output
  (`refs/oh-my-pi/docs/tui-core-renderer.md:6-77`). F1: 895 ms to first request, 434 MB RSS.
- (h) Evals: 15 micro-benches, `omp bench` (TTFT, prefill/decode, cache), `omp if-bench`, and `metaharness`
  with a Harbor runner (TB 2.0 default), a KVM-microVM TB 2.1 runner and a **`pi_upstream` adapter for
  side-by-side omp vs pi** (`refs/oh-my-pi/packages/metaharness/README.md:82-132`; `refs/oh-my-pi/packages/metaharness/agent/pi_upstream.py:1-4`).
  `typescript-edit-benchmark` results (6 OpenRouter models, e.g. haiku-4.5 90.0%) record no edit mode; the
  README's 6.7%→68.3% etc. trace only to the blog (`refs/oh-my-pi/packages/typescript-edit-benchmark/all_models_results.json:3-60`;
  `refs/oh-my-pi/README.md:117-127`). No TB scores committed.
- Providers/headless: `openai-codex` → `${baseUrl}/codex/responses` for any configured host; websocket only if
  `PI_CODEX_WEBSOCKET` or discovery `prefer_websockets` (`refs/oh-my-pi/packages/ai/src/providers/openai-codex-responses.ts:4586-4592`;
  `refs/oh-my-pi/packages/ai/src/providers/openai-codex-transport.ts:5-18`); OpenAI-compatible custom providers and `baseUrl`
  overrides in `models.yml` (`refs/oh-my-pi/docs/models.md:142-192`). Usage `{input (uncached only), output (incl.
  reasoning), cacheRead, cacheWrite, reasoningTokens?, cost}`; side calls (auto-thinking judge) are logged as
  separate `model_usage` entries (`refs/oh-my-pi/packages/catalog/src/types.ts:145-207`;
  `refs/oh-my-pi/packages/coding-agent/src/session/session-manager.ts:2819-2840`).

### F5. unreal-agent (Go library + headless runner; "async-first")

- (a) Process: one Go process, **no TUI, no daemon**. `unreal-agent-runner` takes one JSON request
  (stdin, positional, or `-p`), streams persisted session items to stdout as JSONL, exits when idle
  (`refs/unreal-agent/cmd/unreal-agent-runner/README.md:3-4`; it submits a `StopWhenIdle` control input,
  `refs/unreal-agent/cmd/internal/agentrunner/run.go:400-408`). Components: inbox, coordinator, session
  store, context builder, LLM adapter, tool registry, operation manager (`refs/unreal-agent/README.md:31-40`).
  The only "protocol" is `inbox.Input{ID, Kind: external|control|crash, Payload}`
  (`refs/unreal-agent/harness/inbox/inbox.go:17-27`); control modes `hard`, `when_idle`, `heartbeat`,
  `settings` (reasoning effort can change mid-session) (`refs/unreal-agent/harness/inbox/inbox.go:50-59`).
- (a) Persistence: `<dir>/<session>.session.jsonl`, append-only, `formatVersion = 2`, v1 refused
  (`refs/unreal-agent/harness/sessionstore/localfile/store.go:22`, `refs/unreal-agent/harness/sessionstore/localfile/codec.go:15,114-120`);
  item kinds `fork|input|turn|model_response|tool_call_status`
  (`refs/unreal-agent/harness/sessionstore/sessionstore.go:23-29`); operations saved latest-wins
  (`refs/unreal-agent/harness/sessionstore/sessionstore.go:100-101`); `Fork(id, parentID, previousTurnID)` (`refs/unreal-agent/harness/sessionstore/sessionstore.go:103-108`), with a
  known gap: "Forks leave inherited calls without results" (`refs/unreal-agent/harness/coordinator/loop.go:552`).
- (b) Prompt: embedded `preamble.md` (1,371 B: turn model, async tool semantics, heartbeat, "I believe in
  you!") + runner default system prompt (`refs/unreal-agent/cmd/internal/agentrunner/run.go:45-50`) = 1,566 chars (~390 tokens), fully static, no
  date/cwd/env (`refs/unreal-agent/harness/contextbuilder/prompts/preamble.md:1-11`). Skills are appended
  once as `<available_skills>` XML (name, description, location) (`refs/unreal-agent/harness/contextbuilder/skills.go:16-44`),
  discovered from `<workspace>/.harness/skills/*/SKILL.md` (`refs/unreal-agent/cmd/internal/agentrunner/run.go:323-326`).
- (c) Tools: exactly `Bash`, `ViewImage`, `SkillUse` (`refs/unreal-agent/harness/tool/registry.go:15-19`,
  `refs/unreal-agent/harness/tool/static.go:39-88`); **no edit/read/write tools** — edits go through the shell. Bash runs every
  command in the background as a durable operation: non-PTY pipes (`refs/unreal-agent/harness/primitives/process.go:54-58`),
  process group recorded for crash recovery, stdout/stderr spilled to `<opdir>/<opID>/out|err`
  (`refs/unreal-agent/harness/operation/shell.go:19-20,466-477`). `max_output_length` default 40,000 chars, max 1,000,000
  (`refs/unreal-agent/harness/operation/output.go:9-10`), head = limit/2, tail = rest, marker `...N bytes truncated; complete
  output in <path>...` (`refs/unreal-agent/harness/operation/output.go:29-36`). ViewImage caps 2000×2000 px, 5 MB−1 KB base64
  (`refs/unreal-agent/harness/tool/viewimage/viewimage.go:15-17`).
- (d) Context: builder = `committedPrefix` + `stagedSuffix`, append-only (`refs/unreal-agent/harness/contextbuilder/builder.go:23-29,125-137`).
  `Build()` returns `Result{Request, Report{Changes[]{Kind omitted|truncated|compacted, Source, Reason}}}`
  (`refs/unreal-agent/harness/contextbuilder/contextbuilder.go:9-30`), but **nothing populates `Report` yet** (`refs/unreal-agent/harness/contextbuilder/builder.go:136`;
  test asserts empty, `refs/unreal-agent/harness/contextbuilder/builder_test.go:181`). A `compaction` turn type is persisted and skipped by the
  coordinator (`refs/unreal-agent/harness/session/session.go:14`; `refs/unreal-agent/harness/coordinator/loop.go:467,610`), yet no code creates one: **no compaction,
  no pruning today**. Cache: `prompt_cache_key` = session id plus `session-id` header on the ChatGPT
  backend (`refs/unreal-agent/harness/llm/clients/openaicodex/client.go:62`; `refs/unreal-agent/harness/coordinator/loop.go:375-377`); OpenRouter gets `x-session-id`
  and top-level `cache_control {ephemeral, ttl 1h}` (`refs/unreal-agent/harness/llm/clients/openrouter/client.go:47-58`). Requests are
  deterministic JSON, `store:false`, encrypted reasoning included, summary `auto`
  (`refs/unreal-agent/harness/llm/responsesapi/request.go:27-57`).
- (e) Extensibility = swap Go interface implementations (store, builder, adapter, operation manager); the
  README's example is a proxy manager forwarding serialized operations to a manager inside a remote
  sandbox (`refs/unreal-agent/README.md:44-55`). No plugin/extension runtime, no MCP.
- (f) Async-first, concretely: a tool call is translated synchronously into durable operations
  (`refs/unreal-agent/harness/coordinator/loop.go:780-814`); the model immediately sees `"Tool call is still running. Its result arrives in a
  later turn…"` (`refs/unreal-agent/harness/contextbuilder/builder.go:16`). If results land before the placeholder was sent, it is replaced in the
  staged suffix; otherwise the real result is **appended later as a second tool result for the same
  call_id**, so history stays append-only (`refs/unreal-agent/harness/contextbuilder/builder.go:103-123`; `refs/unreal-agent/harness/contextbuilder/builder_test.go:200-243`). After a
  response with tool calls the coordinator waits up to 1 s (`toolCallRunGracePeriod`) to batch fast
  results into one turn (`refs/unreal-agent/harness/coordinator/loop.go:28,247-252`); inputs are slurped with 1 ms idle / 100 items
  (`refs/unreal-agent/harness/coordinator/loop.go:23-26,429-451`). New external input cancels the in-flight model request and restarts the turn
  with it (`refs/unreal-agent/harness/coordinator/loop.go:346-351,420-425`). While only tools are pending, a heartbeat user message fires after
  `-tool-heartbeat-interval` (default 10 min) (`refs/unreal-agent/harness/coordinator/loop.go:118-124,282-304`; `refs/unreal-agent/cmd/internal/agentrunner/run.go:174`). Inputs carry
  caller-supplied IDs deduplicated per session (`refs/unreal-agent/harness/inbox/local.go:59-86`). Operations are versioned,
  serializable state machines with checkpoints (`refs/unreal-agent/harness/operation/operation.go:30-65`); `RemoteJobHandler`s
  (plan type+version) let a manager proxy work elsewhere (`refs/unreal-agent/harness/operation/remote_job.go:26-32`); none ship.
  No subagents.
- (g) Perf: 12.1 MB static binary, 51 ms to first request, 16 MB RSS (F1). No websocket transport, no
  prewarm (no `websocket`/`previous_response_id` in source). Every session append re-opens, truncates to
  the committed size, writes and `fsync`s the log (`refs/unreal-agent/harness/sessionstore/localfile/store.go:423-431`).
- (h) In-repo eval: `benchmarks/harbor` = Harbor 0.22.0 installed-agent adapter
  (`refs/unreal-agent/benchmarks/harbor/pyproject.toml:6`); uploads a checksummed binary bundle, runs the
  runner with Bash+ViewImage (SkillUse disallowed), converts JSONL to ATIF `trajectory.json` with
  prompt/cached/completion/reasoning/cache-write totals; cost left `None`
  (`refs/unreal-agent/benchmarks/harbor/src/harness_harbor/agent.py:124-210`). Accepts only `openai/`, `openrouter/`,
  `fireworks_ai/` models — not the subscription provider (`refs/unreal-agent/benchmarks/harbor/src/harness_harbor/agent.py:45-52`). README documents a TB 4.0
  run: `-d terminal-bench/terminal-bench@4.0.0 -e modal -m openai/gpt-6-astra --ak thinking_level=max
  -k 5 -n 40` (`refs/unreal-agent/benchmarks/harbor/README.md:47-53`).
- Providers: all five clients speak only `/responses` (OpenAI, `openai-codex`, OpenRouter, Fireworks,
  Ollama) (`refs/unreal-agent/cmd/internal/agentrunner/providers.go:11-58`); the codex client reads `~/.codex/auth.json` or env
  tokens, never logs in or refreshes, and accepts a base-URL override **only for loopback IPs**
  (`refs/unreal-agent/harness/llm/clients/openaicodex/client.go:17,90-103`, `refs/unreal-agent/harness/llm/clients/openaicodex/credentials.go:21-47,71-72`).

### F6. Published results and vendor claims (web, fetched 2026-09-25)

- Terminal-Bench is now at **4.0**: `harbor run -d terminal-bench/terminal-bench@4.0.0`, flat 8 h agent
  timeout, recalibrated resources ([tbench.ai/news/terminal-bench-4-0](https://www.tbench.ai/news/terminal-bench-4-0));
  66 tasks per secondary sources (UNVERIFIED count). tbench.ai and Harbor Hub leaderboards render
  client-side and could not be read.
- Snorkel TB 4.0 leaderboard: Codex + GPT-6 Astra **58.2% ±2.8, $3.3k** (2026-09-03); Claude Code + Fable
  5.1 57.9% ±3.8, $6.2k; Claude Code + Opus 5 53.9%, $6.1k; Codex + GPT-5.6 Sol 37.3%, $2.5k. No pi, omp or
  unreal-agent rows ([snorkel.ai/leaderboard/terminal-bench-4-0](https://snorkel.ai/leaderboard/terminal-bench-4-0/)).
- Unreal Labs launch post (2026-09-22), GPT-6 Astra, xhigh ([unreallabs.ai/blog/unreal-agent](https://unreallabs.ai/blog/unreal-agent/)):

  | Suite | unreal-agent | Codex | Pi |
  |---|---|---|---|
  | TB 4.0 pass / total $ | 57.9% / $1,428 | 57.9% / $2,350 ("leaderboard") | 55.0% / $1,827 |
  | TB 4.0 in / out per trial, turns, tool calls | 1.73M / 32k, 28, 37 | — | 2.83M / 35k, 44, 57 |
  | SWE-Atlas QnA pass / $ (in per trial, turns) | 65.8% / $936 (898k, 16) | 63.3% / $1,303 (1.69M, 22) | 64.0% / $1,033 (1.29M, 24) |
  | DeepSWE 1.1 pass / $ (in per trial, turns) | 72.4% / $1,367 (1.60M, 26) | 69.0% / $1,633 (2.19M, 30) | 69.6% / $1,584 (2.21M, 40) |
  | ALE-CLI full pass / mean / $ | 30.0% / 59.7 / $217 | 29.0% / 58.1 / $292 | 29.0% / 59.2 / $262 |

  Trials per task, harness versions, pricing and cache split are not disclosed; its Codex TB 4.0 row
  ($2,350) disagrees with Snorkel's ($3.3k, 58.2%). Output tokens are nearly equal across harnesses; the
  savings are **input tokens via fewer turns** (28 vs 44).
- oh-my-pi "The Harness Problem" ([stencil.so/blog/the-harness-problem](https://stencil.so/blog/the-harness-problem),
  2026-02-12): React edit benchmark, 16 models × 180 tasks × 3 runs; hashline beats patch in 14/16 models,
  +15 pts average; Grok Code Fast 1 6.7% → 68.3%; Grok 4 Fast output tokens −61%. Format:
  `1:a3|function hello() {` — edits address `line:hash` anchors, stale hashes are rejected. (Current omp code
  uses one per-file tag plus line numbers instead; F4 c.)
- pi: author reports Terminal-Bench 2.0, Opus 4.5, 5 trials/task, "8th place", system prompt + tool
  definitions "below 1000 tokens"; no MCP, no background bash, no todos
  ([mariozechner.at 2025-11-30](https://mariozechner.at/posts/2025-11-30-pi-coding-agent/)); exact score UNVERIFIED.
- Codex TB 2.0 numbers (e.g. GPT-5.4 + Codex CLI 76.0%) appear only on aggregators — UNVERIFIED.
- OpenAI WebSocket mode ([developers.openai.com/api/docs/guides/websocket-mode](https://developers.openai.com/api/docs/guides/websocket-mode)):
  continue with `previous_response_id` + only new input items; state is a connection-local in-memory
  cache (with `store=false`, a miss returns `previous_response_not_found`); `response.create` with
  `generate: false` preloads state (prewarm); connections ≤ 60 min; ≤ 16 in-flight responses per
  connection; "up to roughly 40% faster end-to-end" for 20+ tool calls. InfoQ (2026-05-07): Codex
  "migrated most Responses API traffic to WebSocket mode" ([infoq](https://www.infoq.com/news/2026/05/openai-websocket-responses-api/)).
- "Unrolling the Codex agent loop" (openai.com) returned HTTP 403; its exact-prefix-caching and
  `/responses/compact` claims are known only from search snippets — UNVERIFIED here (see F2 for code).
- Harbor pre-integrated agents include `codex`, `claude-code`, `pi`, `terminus-2`, `opencode`, `hermes`,
  `fx`; **not** omp or unreal-agent ([docs.harborframework.com](https://docs.harborframework.com/core-concepts/agents/pre-integrated-agents.md)).
  Harbor-format suites: SWE-bench Verified (500) on Harbor Hub; SWE-Atlas QnA (124 tasks, Harbor format,
  [scaleapi/SWE-Atlas](https://github.com/scaleapi/SWE-Atlas)); DeepSWE 1.1 (113 tasks; separate verifier
  needs Pier ≥ 0.3, [datacurve-ai/deep-swe](https://github.com/datacurve-ai/deep-swe)); ALE-CLI
  ([rdi-berkeley/agents-last-exam](https://github.com/rdi-berkeley/agents-last-exam)).

### F7. Prior art in the user's own repo (tny) — directly reusable

- `refs/tny/docs/benchmarks/harness-efficiency.md:10-33`: objective = price-weighted cost per *completed*
  task at no loss of success; rules "Do not truncate data away. Spill it to a file", "Keep the cached
  prefix byte-stable", "Never drop reasoning items". Cost in input-token equivalents (ITE; gpt-6: uncached
  1.0, cached 0.1, cache write 1.25, output 5.0) (`refs/tny/docs/benchmarks/harness-efficiency.md:42-62`); a history rewrite pays back only after
  ≈ `11.5 × A / R` requests (`refs/tny/docs/benchmarks/harness-efficiency.md:62`). Fairness: same model/effort, default prompts/tools, isolated HOME,
  **one recording proxy reading usage from `response.completed`; self-reports not trusted** (`refs/tny/docs/benchmarks/harness-efficiency.md:98-107`);
  ≥3 reps, Wilson CIs, paired non-inferiority δ = 8 pp (`refs/tny/docs/benchmarks/harness-efficiency.md:120-126`).
- tny's real-usage baseline: 97.3% cache hit, **110.8k mean input tokens/request**; cached input = 66% of
  cost; shell output = 79% of tool-result bytes (`refs/tny/docs/benchmarks/harness-efficiency.md:70-86`). I.e. once caching works,
  **context size re-read per request** dominates cost.
- `refs/tny/tests/bench/harness_bench/adapters.py:13,110-354`: working proxy configs for tny, codex, pi,
  omp, hermes, opencode, fx and unreal-agent against `chatgpt.com/backend-api/codex/responses`;
  multi-turn resume only for tny and codex (`refs/tny/docs/benchmarks/harness-efficiency.md:14`). Proxy computes instructions/tools/history/tool-output
  sizes per request, including the `additional_tools` layout (`refs/tny/tests/bench/harness_bench/proxy.py:43-96`).
- ADR 0077/0078: vs Codex CLI 0.154, same model — conversation warm cache hit 86.8% (tny) vs 98.4% (codex),
  request bytes 399k vs 1.13M (`refs/tny/docs/adr/0077-openai-prompt-cache-routing.md:111-118`); fresh tasks
  with a workspace-shared `prompt_cache_key` + `session-id` routing group: 78.1% vs 30.3% cached
  (`refs/tny/docs/adr/0078-workspace-shared-prompt-cache.md:60-68`); sharing only the JSON key gave zero hits;
  WebSocket continuation "did not produce a superior cache-hit result" (`refs/tny/docs/adr/0078-workspace-shared-prompt-cache.md:94-100`); the ChatGPT
  backend sends an `x-codex-turn-state` affinity token to replay within a turn (`refs/tny/docs/adr/0077-openai-prompt-cache-routing.md:23-41`).

### F8. Comparison table (facts from F1–F7)

| Dimension | codex | pi | omp | unreal-agent |
|---|---|---|---|---|
| Runtime / footprint | Rust; 239 MB + 65 MB V8 host | TS on Node/Bun; 75.6 MB binary | TS on Bun + Rust N-API natives; 933 MB install | Go; 12.1 MB |
| Process & UI↔core | in-process app-server; optional daemon; JSON-RPC over stdio/UDS/ws; separable `exec-server` | single process; JSONL RPC on stdio; CBOR/UDS daemon experimental | single Bun process; NDJSON RPC/ACP on stdio; no agent daemon (process broker only) | single headless process; inbox inputs; JSONL out |
| Session store / fork | JSONL rollouts + SQLite projections; fork = truncate at turn | JSONL id/parentId tree; `/tree` `/fork` `/clone`; SQLite unwired | JSONL tree + blob/artifact stores; `/tree` `/fork` `branch` `handoff` | versioned JSONL (fsync/append); fork at turn |
| Static prefix, 1st request (F1) | ~9.7k tok (gpt-5.5); ~13.2k (gpt-6-astra) | ~1.5k tok | ~7.1k tok | ~0.7k tok |
| Dynamic prompt parts | frozen base; diffs appended (env/date, AGENTS.md, effort, model) | no date; section patches only on capable models | date/cwd in first user turn, appended on change; model id in prompt | none |
| Default tools | code-mode `exec` (+wait, collab) on gpt-6; else exec_command/write_stdin/apply_patch/view_image/… | read, bash, edit, write | 11 sent (read, write, edit, bash, eval, glob, grep, task, wait, todo, web_search); rest behind `xd://` | Bash, ViewImage (+SkillUse) |
| Edit format | freeform `apply_patch` (Lark) | multi `oldText/newText` | hashline (file tag + line numbers, Lark-constrained); per-model fallback | none (shell) |
| Shell | unified exec, PTY optional, 64 procs, yield/poll | spawn+pipes, no PTY, no background | embedded brush, persistent sessions, async jobs, auto-background 60 s | every call backgrounded, durable, pipes |
| Output bound (F1 W2, 589 KB) | 10k tok head/tail, no path | tail 2,000 lines/50 KB + log path | generic >50 KB: 20 KB + 20 KB + `artifact://` (bash measured ≈51 KB); minimizer; read = structural summary | 40k chars head/tail + path |
| Compaction | 90%/95% of window; remote v2 encrypted or local summary + 20k user msgs | window − 16,384; keep 20k recent; structured summary; iterative | window − max(15%, 16k); remote→snapcompact→handoff→shake→soft; speculative; cache-aware pruning | none (type reserved) |
| Cache keying | `prompt_cache_key`=thread, session/thread ids, turn-state, zstd, ws delta | `prompt_cache_key`=session; cache warming | `prompt_cache_key`=session; byte-stable tool re-declaration; append-only mode | `prompt_cache_key`=session; OpenRouter 1h TTL |
| Extensibility | hooks (12 events), plugins, skills, MCP (deferred via tool_search), compiled Rust ext | 40-event TS extension API, tools/commands/UI/themes, packages | pi API + marketplace (Claude format), MCP, RPC host tools, metaharness | swap Go interfaces |
| Multi-agent | V1/V2 subagents (6 threads, depth 1); message board (off) | example extension only | `task` subagents in-process (32, depth 2), typed results, peer messages; no blackboard | none; async tools |
| TUI | ratatui inline viewport, 120 FPS cap | differential rendering, sync output | per-frame diff + sync output | none |
| Latency tricks | websocket + `previous_response_id` delta, `generate:false` prewarm | websocket-first, zstd, cache warming | speculative reads (off), speculative compaction, auto-thinking judge | 1 s batching grace |
| Spawn→1st req / RSS (F1) | 142 ms / 105 MB | 228 ms / 147 MB | 895 ms / 434 MB | 51 ms / 16 MB |
| ChatGPT backend / OpenAI-compatible | yes / Responses-only custom providers | yes / completions + responses | yes / custom providers + `baseUrl` overrides | yes (loopback override only) / Responses-only |
| In-repo evals; published | divan micro-benches; Snorkel TB4 58.2% | docs-lift evals; TB2 "8th" (UNVERIFIED score) | metaharness (Harbor, TB 2.1 microVM, pi_upstream); blog edit-bench claims | Harbor adapter; TB4 57.9% @ −39% $ |

## Implications for aim (opinion — recommendations, not facts)

### I1. Benchmark plan

**Headline metric.** ITE (or $) per *passed* task at non-inferior pass rate, same model, same effort, same
upstream — exactly tny's definition (F7). Secondary: turns/task, tool calls/task, token-weighted cache-hit rate,
static-prefix tokens, TTFT per request (first provider event, as tny ADR 0077 measures), p50 wall,
spawn→first-request, idle/peak RSS. Always report input / cached / cache-write / output /
reasoning separately. Never report best-of-N; publish per-task flip tables.

**Tier 0 — offline wire microbench, every PR (< 2 min, no credentials).** Turn the F1 pilot into `bench/wire/`:
a scripted Responses mock (SSE and websocket) replays canned trajectories; competitors pinned in
`mise.toml` (codex, pi, omp via bun, unreal-agent at a pinned commit) and aim at HEAD.

| id | scripted scenario | measured | proposed aim gate |
|---|---|---|---|
| W1 | "Reply OK" | 1st-request bytes split (instructions/dev/tools), spawn→1st request, exit, peak RSS | prefix ≤ 2k tok, ≤ 30 ms, ≤ 40 MB |
| W2 | shell prints 589 KB | model-visible chars, marker, spill path | ≤ 10k chars, path present |
| W3 | read a 5,000-line file | chars returned, continuation hint | ≤ budget, hint present |
| W4 | 20 tool steps | Σ request bytes; per-step harness overhead ms; **prefix stability** (req n+1 extends req n) | 100% stable |
| W5 | 5 parallel calls in one response | response end → next request | ≈ max(tool), not Σ |
| W6 | cross the compaction threshold | compaction request bytes, post-compaction request, prefix reuse | documented budget |
| W7 | resume/fork a 200-item session | resume ms, bytes re-sent | ≤ 50 ms |
| W8 | daemon idle 60 s, prewarmed socket | idle RSS, prewarm frames | ≤ 25 MB |

Store one JSON per run and a summary in `bench/history/`; fail CI on regressions beyond measured noise.

**Tier 1 — live, small, nightly.** Port tny's `harness_bench` (12 short + 3 long + 2 multi-turn tasks, hidden
verifiers, isolated HOME, loopback recording proxy, ITE, Wilson CIs, paired non-inferiority δ = 8 pp) and
add an `aim` adapter; it already drives codex, pi, omp and unreal-agent (F7). omp's `metaharness`
`pi_upstream` adapter is a second reference for side-by-side runs (F4 h). Cheap model/effort nightly; the
headline model for release candidates.

**Tier 2 — public suites via Harbor, per release.** Terminal-Bench 4.0, SWE-Atlas QnA (124), DeepSWE 1.1 (via
Pier), a stratified 100-task SWE-bench Verified subset, optionally ALE-CLI Near-Term — the suites Unreal
Labs published against Codex and Pi (F6), so aim lands directly on that table. Ship an aim Harbor
installed-agent adapter that writes ATIF with prompt/cached/completion/reasoning/cache-write totals
(pattern: `refs/unreal-agent/benchmarks/harbor/src/harness_harbor/{agent,trajectory}.py`). Harbor's built-in
`codex` and `pi` agents exist; omp and unreal-agent need their own adapters (F6).

**Fairness recipe (configs exercised against the mock in F1; tny runs them live, F7).** All four reach the
ChatGPT subscription backend through one loopback proxy:
- codex: `-c model_providers.bench={base_url="http://127.0.0.1:P/v1",wire_api="responses",requires_openai_auth=true,supports_websockets=false}`
  `-c model_provider="bench" -c model_reasoning_effort=E --ignore-user-config --ignore-rules
  --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check --json` (a differently named provider
  disables zstd and websockets, F2; F1 used `env_key` against the mock). If codex uses the built-in
  ChatGPT provider instead: mount the proxy at `…/backend-api/codex`, never point `chatgpt_base_url` at it
  (routing rewrites back to the real origin), handle the websocket upgrade, decode zstd (F2 agent notes;
  `refs/codex/codex-rs/model-provider-info/src/lib.rs:604-610`).
- pi: `models.json {"providers":{"openai-codex":{"baseUrl":"http://127.0.0.1:P"}}}`, `settings.json {"transport":"sse"}`,
  `--provider openai-codex --model M --thinking E --mode json --print`; pass `--session-id` per task or each
  run gets a fresh cache key (F3 d).
- omp: `models.yml providers.openai-codex.baseUrl`, `PI_CODEX_WEBSOCKET=0`, `--model openai-codex/M --thinking E
  --mode json --print --auto-approve`.
- unreal-agent: `UNREAL_HARNESS_LLM_PROVIDER=openai-codex UNREAL_HARNESS_LLM_BASE_URL=http://127.0.0.1:P/backend-api/codex`
  (loopback only) and a JSON request with `thinking_level`.
Rules: token comparisons over HTTP/SSE (websocket measured separately, for latency); proxy decodes zstd and
reads usage from the provider's terminal event, because self-reports differ (codex exec omits subagent
tokens; omp logs judge calls separately; F2, F4); isolated HOME; default prompts/tools; hosted web search
off everywhere or on everywhere; rotate harness order; ≤ 3 concurrent runs; record binary hashes. For
non-OpenAI models the only common wire is an **OpenAI Responses-compatible** endpoint (e.g. OpenRouter
`/responses`): codex and unreal-agent cannot speak Chat Completions (F2, F5).

### I2. Levers where aim can win (ranked: expected saving × confidence ÷ cost)

1. **Fewer model turns: async, batched tool execution.** unreal-agent vs Pi on TB 4.0: 28 vs 44 turns, 1.73M
   vs 2.83M input tokens per trial, similar output (F6). Every avoided turn skips a full context re-read.
   Adopt background-by-default tools, running placeholders that are appended, never rewritten (F5 f), a short
   batching grace, and a prompt that says "go wide".
2. **Keep per-request context small.** Once caching works, re-reading context dominates cost (tny: 110.8k
   input tokens/request, cached input 66% of cost; shell output 79% of tool bytes; F7). Combine spill-to-file
   with a small inline budget and an explicit path (codex gives no path; pi keeps only the tail; F1 W2),
   omp-style structural read summaries and a command-output minimizer (F4 c), minimal tool results (pi's edit
   returns one line, no diff echo; F3 c), and threshold-time pruning
   of old tool results timed so it does not break the cache (omp: only when ≤ 8k tokens follow; F4 d).
3. **Correct cache routing.** `prompt_cache_key` plus `session-id`/`thread-id`, turn-scoped
   `x-codex-turn-state`, and a **workspace-shared routing group** for fresh tasks and subagents (78% vs 30%
   cached on fresh tasks vs codex; F7). Cheap, and large for swarms.
4. **A byte-stable, append-only prefix as a verified invariant.** Deterministic serialization (unreal),
   deterministic ids for synthetic items and appended diffs for settings/AGENTS.md/effort/model (codex F2 b),
   volatile data after the prefix (omp's date/cwd reminder, F4 b), byte-identical tool re-declaration (omp).
   State "request n+1 extends request n unless a recorded compaction/edit event says otherwise" as a
   Verus-checked property of the context builder and test it in W4.
5. **A small static prefix with lazy tool exposure.** 0.7k (unreal) and 1.5k (pi) vs 7.1k (omp) and 9.7–13.2k
   (codex) tokens (F1). Defer rarely used tools (codex: all MCP tools behind BM25 `tool_search`; omp:
   `xd://` devices) and list skills as metadata only. At ~40 turns a 10k-token prefix costs ~400k cached
   tokens (~40k ITE) per task.
6. **Compaction that keeps the cache.** On the ChatGPT backend use codex-style remote compaction v2 (same
   prefix + `compaction_trigger`, encrypted result, 64k retained-message budget; F2 d); elsewhere reuse the
   live prefix for the summary call (omp handoff) and start it speculatively below the threshold (omp).
   Return and persist unreal-style `Report{omitted|truncated|compacted}` records so compaction is auditable.
   Batch history edits (tny's 11.5 × A/R payback rule).
7. **Edit format per model family.** Hashline-style anchored edits with a seen-lines guard lift weak models
   and cut retry loops (published −61% output tokens on Grok 4 Fast, F6); freeform `apply_patch` for GPT
   (codex). Make the edit tool pluggable per model and decide with Tier 1 data.
8. **Code mode, measured before defaulting.** Codex made one JS `exec` tool the only top-level tool for every
   gpt-6/5.6 model, and only printed output enters context (F2 c) — but its description alone is 11k chars
   (F1). Compare aim code mode vs direct tools on Tier 1.
9. **Transport for latency, not tokens.** Websocket continuation (`previous_response_id` + new items) and
   `generate:false` prewarm give "up to ~40%" faster long rollouts (F6) but showed no cache-hit gain in tny's
   test (F7). Also zstd request bodies (codex, pi).
10. **Process cost.** A Rust daemon with prewarmed connections should undercut unreal-agent (51 ms, 16 MB)
    and clearly beat codex (142 ms, 105 MB), pi (228 ms, 147 MB) and omp (895 ms, 434 MB) (F1). Prefer SQLite
    WAL + `synchronous=NORMAL` (codex, pi durable) to an fsync per appended item (unreal).
11. **Dynamic reasoning effort as an appended control item.** Effort can change mid-session without breaking
    the prefix (codex `configuration_update` items, unreal `settings` input); omp's per-user-turn judge
    already lists `typesafe/jev-latest` first (F4 b, g). Output (including reasoning) weighs 5× in ITE, so this
    is the Jev router's direct lever; aim can re-judge mid-turn, which omp does not.

Not worth copying: preempting in-flight model calls on every new input (unreal) pays for discarded partial
requests; cache warming by replaying requests (pi) spends tokens and only pays off when a human is idle.

## Open questions for the user

1. Budget: may nightly live runs use the ChatGPT subscription (rate limits, fair use), or should Tier 1/2
   use API keys? Published TB 4.0 runs cost $1.4k–3.3k each at gpt-6-astra xhigh (F6).
2. Headline configuration for "beat": which model and effort (Unreal used gpt-6-astra xhigh), and is the win
   condition "same pass rate, lower ITE per passed task" or "higher pass rate"?
3. Default tool surface: start minimal (unreal/pi-like, lazy extras) or rich (omp-like)? It changes the
   static prefix by ~5–10k tokens per request.
4. Publish results (Harbor Hub / leaderboard submissions, blog), or keep them internal?

<!-- REPORT COMPLETE -->
