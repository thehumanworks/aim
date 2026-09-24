# R2 — Competitive landscape, token efficiency and benchmarks

Scope: codex, pi, oh-my-pi (omp), unreal-agent, as of 2026-09-25. Sources: shallow clones under
`refs/` (codex `dbb875d` 2026-09-24, pi `5fd446c` 2026-09-25, omp `4a7b586` 2026-09-24, unreal-agent
`1b9f778` 2026-09-23), a local wire-level pilot (F1), and fetched web pages (F6). Token figures are
**chars/4** unless stated. `UNVERIFIED` = not confirmed against code or a fetched page. Citation
convention: inside a harness section, a path without a `refs/<repo>/` prefix is relative to that repo's root.

## TL;DR

(filled last)

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
- Caveats: one prompt, zero model latency, one OS; this measures harness overhead, not task quality.

### F2. codex (Rust; openai/codex `codex-rs/`)

- (a) Process: TUI and `codex exec` embed the **app-server in-process** (Rust types, JSON only at external
  boundaries) (`codex-rs/app-server-client/README.md:3-6,29-38`); the TUI has no `codex-core` dependency
  (`codex-rs/tui/Cargo.toml:32-42`) and targets `Embedded | LocalDaemon | Remote`, auto-connecting to a
  daemon socket within 50 ms (`codex-rs/tui/src/lib.rs:277-278,312-321,1031-1039`); `daemon_auto_start`
  is stable/default-on (`codex-rs/features/src/lib.rs:936`). App-server = JSON-RPC over
  `stdio | unix socket (0600) | ws://` — no gRPC/REST (`codex-rs/app-server-transport/src/transport/mod.rs:53-54,81-120`).
  Methods include `thread/start|resume|fork|read|list|compact/start|inject_items|queue/add`,
  `turn/start|steer|interrupt`, notifications `item/*`, `turn/*`, `thread/tokenUsage/updated`
  (`codex-rs/app-server-protocol/src/protocol/common.rs:551-1994`). The daemon survives TUI exit and
  records loaded/interrupted threads across restarts (`codex-rs/app-server-daemon/README.md:32-41,161-192`;
  `codex-rs/app-server-transport/src/daemon_recovery.rs:13-28`). **Tool execution is separable**:
  `exec-server` is a JSON-RPC process/filesystem/http server (default `ws://127.0.0.1:0`, Noise-encrypted
  relay for remote) selected with `CODEX_EXEC_SERVER_URL` (`codex-rs/exec-server/README.md:3-5,36-41`;
  `codex-rs/exec-server-protocol/src/protocol.rs:22-57`; `codex-rs/exec-server/src/environment.rs:56,79`).
- (a) Persistence: JSONL rollouts `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<thread>.jsonl` are the source
  of truth (`codex-rs/rollout/src/recorder.rs:1727-1731`; `codex-rs/rollout/src/rollout_file_name.rs:67-70`);
  SQLite (WAL, `synchronous=Normal`) holds projections: `state_5`, `thread_history_1`, `logs_2`,
  `goals_1`, `memories_1`, `queue_1` (`codex-rs/state/src/sqlite.rs:29-34,302-308`), no FTS. Fork =
  truncate rollout at a turn (`thread/fork` `last_turn_id`/`before_turn_id`)
  (`codex-rs/core/src/thread_rollout_truncation.rs:169`; `codex-rs/core/src/thread_manager.rs:188-206`).
- (b) Prompt: per-model `model_messages.instructions_template` in the bundled catalog (refreshed from
  `/models` with ETag, 300 s TTL) (`codex-rs/models-manager/models.json:75-76`,
  `codex-rs/models-manager/src/manager.rs:32,63`); default model gpt-6-astra (`codex-rs/models-manager/models.json:4,69-74`).
  Template sizes: gpt-6-astra 21,428 B (~5.4k tok), gpt-6-sol 18,998, gpt-5.6-* 17,766, gpt-5.5 21,473;
  the `core/*_prompt.md` files are dead code. Base instructions are frozen per thread
  (`codex-rs/core/src/session/mod.rs:738-742`); everything dynamic is a "world state" section
  (AGENTS.md ≤ 32 KiB, permissions, collaboration mode, `<environment_context>` with cwd/shell/**date**/
  timezone, apps, plugins; `codex-rs/core/src/session/world_state.rs:117-271`,
  `codex-rs/core/src/agents_md.rs:43-68`). After the first turn only **diffs are appended** (model switch,
  AGENTS.md "replace all previously provided", env fields that changed, effort as `configuration_update`
  items) against a `reference_context_item` baseline (`codex-rs/core/src/session/mod.rs:4632-4698`;
  `codex-rs/core/src/context/world_state/environment.rs:135-194`;
  `codex-rs/core/src/session/reasoning_effort.rs:1,77-90`). Under Responses Lite the tools item and the
  instructions item get UUIDv5 ids over thread id + payload so retries/resume stay byte-identical
  (`codex-rs/core/src/client.rs:912-943`). `prompt_cache_key` = root thread id, or
  `"{source}:{parent_thread_id}"` for internal sub-requests (`codex-rs/core/src/client.rs:585-599`). Skills list budget: 2% of
  window, default 8,000 chars (`codex-rs/ext/skills/src/render.rs:17-21`).
- (c) Tools: `tool_mode` Direct | CodeMode | CodeModeOnly; all gpt-6/5.6 models are `code_mode_only`
  (`codex-rs/models-manager/models.json:20`; `codex-rs/core/src/tools/mod.rs:74-84`). Code-mode-only hides every nested tool
  behind one freeform JS tool **`exec`** (V8 isolate; Lark grammar `pragma_source | plain_source`,
  `codex-rs/core/src/tools/code_mode/execute_spec.rs:18-26`); nested tools are described as TypeScript
  `declare const tools` (`codex-rs/code-mode-protocol/src/description.rs:417-429`) and **only what the
  script prints reaches the model** (10k-token cap; `codex-rs/core/src/tools/code_mode/mod.rs:286-326`).
  Direct mode (gpt-5.5): `exec_command`, `write_stdin` (unified exec, PTY optional), `apply_patch`
  (freeform only, grammar 578 B, `codex-rs/core/assets/tools/apply_patch.lark:1`), `view_image`,
  `request_user_input`, goal tools, `tool_search`, optional MCP resource tools and `multi_agent_v1`
  (F1 measured 9 tools / 9,092 chars). Unified exec: yield 250–30,000 ms, default 10,000 output tokens,
  1 MiB head/tail buffer, ≤ 64 processes with LRU eviction, forced `NO_COLOR=1 TERM=dumb PAGER=cat`
  (`codex-rs/core/src/unified_exec/mod.rs:73-82`, `codex-rs/core/src/unified_exec/process_manager.rs:93-104,1728-1799`).
  MCP tools are **deferred behind `tool_search`** (BM25, limit 8) whenever the model supports it — no
  count threshold (`codex-rs/core/src/mcp_tool_exposure.rs:90-94`; `codex-rs/tools/src/tool_discovery.rs:7`).
  Truncation: catalog `truncation_policy {tokens, 10000}` (bytes/4), 50/50 head/tail, marker
  `…N tokens truncated…`, re-truncated at 1.2× when recorded to history; no spill file
  (`codex-rs/utils/string/src/truncate.rs:4,126-137`; `codex-rs/core/src/context_manager/history.rs:449-452`).
  HEAD #47957 only bounds warehouse-only tool metadata to a 15 MiB wire budget
  (`codex-rs/core/src/client_tool_metadata.rs:2,10`), not model-visible output.
- (d) Compaction: auto at 90% of the window (244,800 of 272k), hard at 95%; checked pre-turn and
  mid-turn (`codex-rs/protocol/src/openai_models.rs:525-535`; `codex-rs/core/src/session/context_window.rs:84-110`;
  `codex-rs/core/src/session/turn.rs:598-621,1271-1283`). **Remote v2** (OpenAI/Azure/Bedrock providers):
  the normal streaming request plus a trailing `{"type":"compaction_trigger"}` item, same tools and
  instructions (prefix stays cached), returns `{"type":"compaction","encrypted_content"}`; afterwards keeps
  user messages + agent messages ≤ 10k tokens within a 64k-token budget, then the compaction item
  (`codex-rs/core/src/compact_remote_v2.rs:75-76,401-416,454`; `codex-rs/core/src/compact_remote_v2_attempt.rs:78-87`).
  Local fallback: prompt "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for
  another LLM that will resume the task…" (426 B, `codex-rs/prompts/templates/compact/prompt.md`), sent
  **without tools**; keeps ≤ 20,000 tokens of the newest user messages + `SUMMARY_PREFIX` ("Another language
  model started to solve this problem and produced a summary…") (`codex-rs/core/src/compact.rs:59,281-284,667-740`).
  No other pruning; old reasoning is kept; token estimate = last server total + ceil(bytes/4) of new items
  (`codex-rs/core/src/context_manager/history.rs:798-815`).
- (g-transport) zstd request bodies on the ChatGPT backend (`codex-rs/features/src/lib.rs:1272`;
  `codex-rs/core/src/client.rs:1627-1635`). WebSocket (`OpenAI-Beta: responses_websockets=2026-02-06`) sends
  `previous_response_id` + only new items when the request is otherwise identical, prewarms with
  `generate:false`, keeps `x-codex-turn-state` sticky routing (`codex-rs/core/src/client.rs:175,293-304,337-392,1390-1426,2010-2016`).
  HTTP never uses `previous_response_id`.
- (e) Extensibility: 12 hook events (PreToolUse … Stop, Interrupt) with command/MCP/prompt/agent
  handlers; hooks can block, rewrite tool input and MCP output; `additionalContext` capped at 2,500 tokens
  (`codex-rs/protocol/src/protocol.rs:1579-1601`; `codex-rs/hooks/src/schema.rs:238-272`;
  `codex-rs/hooks/src/output_spill.rs:12`). Plugins bundle skills + MCP servers + apps + hooks and also read
  `.claude-plugin`/`.cursor-plugin` manifests (`codex-rs/plugin/src/manifest.rs:19-24`;
  `codex-rs/utils/plugins/src/plugin_namespace.rs:134-135`). Skills: `SKILL.md` under `~/.agents/skills`,
  `.codex/skills`, and **`.agents/skills` in every dir from repo root to cwd**; metadata-only catalog,
  body read on demand (`codex-rs/ext/skills/src/host_roots.rs:86-171`; `codex-rs/ext/skills/src/fragments.rs:40-41`). MCP client
  rmcp 3.2 (`codex-rs/Cargo.toml:443`); codex is **not** an MCP server. Extensions are compiled-in Rust
  crates implementing contributor traits — no WASM (`codex-rs/ext/extension-api/src/lib.rs:46-85`). Code
  mode runs in a separate, lazily spawned `codex-code-mode-host` (V8 150.4, sandboxed), reachable over
  stdio or gRPC (`codex-rs/code-mode/src/remote_session.rs:34`; `codex-rs/code-mode-host/src/main.rs:21`).
- (f) Multi-agent: in-process threads, each with its own rollout; edges in SQLite `thread_spawn_edges`
  (`codex-rs/agent-graph-store/src/store.rs:13-43`). V1 tools `spawn_agent/send_input/resume_agent/wait_agent/
  close_agent`; V2 (namespace `collaboration`, chosen by the model catalog for gpt-6) adds `send_message`
  ("does not trigger a new turn"), `followup_task`, `list_agents`, `interrupt_agent`
  (`codex-rs/core/src/tools/handlers/multi_agents_spec.rs`; `codex-rs/core/src/config/mod.rs:1574-1616`).
  Limits: 6 threads, depth 1, V2 4 concurrent per session, wait 10 s–1 h (`codex-rs/core/src/config/mod.rs:253-263`).
  **A blackboard already exists but is off**: `agent_message_board` (UnderDevelopment) — per-tree board with
  channels, threads, `search_posts`, subscriptions, idempotent `post(request_id)`, SQLite
  `agent_message_board_1.sqlite`, 64 KiB posts; pushes only into *running* turns, idle agents must poll
  (`codex-rs/features/src/lib.rs:1331`; `codex-rs/ext/agent-message-board/src/api.rs:24-148`;
  `codex-rs/ext/agent-message-board/src/local.rs:1-89`; `codex-rs/core/src/agent_message_board.rs:159-199`).
- (g) Perf: ratatui 0.30 **inline viewport** — finished history is written into normal scrollback, alt
  screen only for overlays (`codex-rs/tui/src/tui.rs:431`; `codex-rs/tui/src/insert_history.rs:1-5`); 120 FPS
  cap with a frame-scheduler actor (`codex-rs/tui/src/tui/frame_rate_limiter.rs:4-13`); adaptive stream
  pacing (`codex-rs/tui/src/streaming/chunking.rs:85-116`). Startup prewarms MCP, shell snapshot and the
  websocket (`codex-rs/core/src/session/startup_prewarm.rs:1-3`; `codex-rs/core/src/session/mcp_prewarm.rs:1-4`). Release: thin
  LTO, 4 CGUs, jemalloc (`codex-rs/Cargo.toml:611-620`; `codex-rs/cli/src/main.rs:50-51`). OTEL metrics
  include `codex.turn.ttft.duration_ms`, `codex.websocket.continuation`, `codex.startup.phase.duration_ms`
  (`codex-rs/otel/src/metrics/names.rs:15-61`). F1: 142 ms to first request, 105 MB RSS, 239 MB binary.
- (h) Evals: only two divan micro-benches (`codex --help`, image prompts)
  (`codex-rs/cli/e2e_benches/codex_help.rs:14-25`); no TB/SWE-bench harness. `responses-api-proxy` forwards
  `POST /v1/responses` with an injected key and `--dump-dir` request/response pairs, no websocket
  (`codex-rs/responses-api-proxy/README.md:31-79`).
- Headless/endpoints: ChatGPT base `https://chatgpt.com/backend-api/codex`, API `https://api.openai.com/v1`
  (`codex-rs/model-provider-info/src/lib.rs:77,425-427`); custom `[model_providers.<id>]` with
  `wire_api="responses"` only — **Chat Completions removed** (`codex-rs/model-provider-info/src/lib.rs:96,126,140-191`). `codex exec --json`
  emits `turn.completed.usage{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens,
  reasoning_output_tokens}` = **cumulative main-thread totals; subagent tokens excluded**
  (`codex-rs/exec/src/exec_events.rs:60-73`; `codex-rs/exec/src/event_processor_with_jsonl_output.rs:118-127`;
  `codex-rs/exec/src/lib.rs:1636-1638`). Effort only via `-c model_reasoning_effort=…`.

### F3. pi (TypeScript; earendil-works/pi, coding-agent 0.87.1)

- (a) Process: released CLI = **one Node process**; modes TUI, `-p`, `--mode json`, `--mode rpc` (LF-framed
  JSONL on stdio, 33 commands incl. `prompt|steer|follow_up|abort|compact|fork|get_session_stats`; ~30 event
  types) (`packages/coding-agent/docs/cli.md:44-47`; `packages/coding-agent/docs/rpc.md:3,37-54`;
  `packages/coding-agent/docs/rpc-commands.md:7-782`; `packages/coding-agent/docs/json.md:54-193`). Extensions and SDK run in-process
  (`packages/coding-agent/docs/how-pi-works.md:39-43`). An **experimental daemon** exists behind `PI_EXPERIMENTAL=1`: server +
  one session-worker process per session + presentation clients, length-prefixed **CBOR over a Unix socket**
  (protocol v8, 16 MiB frames), outbound WebSocket only to a hosted relay; not in published entrypoints
  (`packages/coding-agent/src/core/experimental.ts:2`; `packages/protocol/README.md:13-41`;
  `packages/protocol/src/protocol.ts:5`; `packages/server/src/transports/unix/listener.ts:57`;
  `packages/agent/docs/plugins.md:16-31`). `AgentHarness` spec: durable intent/settlement records around
  every provider request and tool call, entries + values + append-only usage ledger (`packages/agent/docs/harness.md:21-54`).
- (a) Persistence: append-only **JSONL tree** (`id`/`parentId`, v3) at `~/.pi/agent/sessions/--<path>--/<ts>_<uuidv7>.jsonl`;
  `/tree` moves the leaf in-file, `/fork` copies to a new file, `/clone` copies the branch; one
  `appendFileSync` per entry, no fsync, nothing written before the first assistant message
  (`packages/coding-agent/docs/session-format.md:3-231`; `packages/coding-agent/docs/sessions.md:26-28`;
  `packages/coding-agent/src/core/session-manager.ts:41,1160-1187`). Two **SQLite schemas exist but are unwired**:
  `session-backends/sqlite-node` (entries/values/usage_ledger, WAL) and `durable` (conversations, tasks with
  `background` + owner subtree, scoped documents with base/delta revisions, WAL + `synchronous=NORMAL`)
  (`packages/session-backends/sqlite-node/src/sqlite/migrations/001_initial.sql:1-121`;
  `packages/durable/src/storage/sqlite/migrations.ts:10-77`; `packages/durable/src/types.ts:129-151`).
- (b) Prompt: XML-sectioned (`<tools>` one-line snippets, `<rules>`, `<docs>`, `<project_context>`,
  `<skills>`, `<cwd>`) (`packages/coding-agent/src/core/system-prompt.ts:121-180`); ~2.6k chars (~650 tokens),
  47% of it pi self-docs; **no date** since v0.80.7 "Fixed system prompt cache invalidation across dates"
  (`packages/coding-agent/CHANGELOG.md:955`). AGENTS.md/CLAUDE.md per directory; skills metadata-only
  (`packages/coding-agent/src/core/resource-loader.ts:127`; `packages/coding-agent/src/core/skills.ts:362-380`). Mid-session prompt/tool changes are
  appended as section patches only where the model supports mid-conversation system messages; otherwise
  prompt changes rebuild the leading prompt and **invalidate the cache** (`packages/ai/src/utils/transcript.ts:105-120`;
  `packages/coding-agent/docs/extensions.md:150`).
- (c) Tools: 8 built-ins, default **read, bash, edit, write** (+ grep/find/ls/powershell opt-in)
  (`packages/coding-agent/src/core/tools/index.ts:95-105`; `packages/coding-agent/src/core/sdk.ts:258`); measured 3,096 chars on
  the wire (F1). Parallel tool execution by default (`packages/agent/src/agent.ts:253`). Edit =
  `{path, edits:[{oldText,newText}]}`, each unique against the original, normalization-only fuzzing (NFKC,
  quotes, dashes), result is one line "Successfully replaced N block(s)" (`packages/coding-agent/src/core/tools/edit.ts:21-51,205-210`;
  `packages/coding-agent/src/core/tools/edit-diff.ts:34-54,328-350`). Read: no line numbers, 2,000 lines/50 KB head + "Use offset=Z to
  continue" (`packages/coding-agent/src/core/tools/read.ts:156-169`). Bash: `spawn` with pipes, **no PTY, no background jobs**, optional
  timeout, **tail** 2,000 lines/50 KB, full log `/tmp/pi-bash-<hex>.log` only when truncated
  (`packages/coding-agent/src/core/tools/truncate.ts:11-13`; `packages/coding-agent/src/core/tools/bash.ts:38-46,96-120,239,333`; `packages/coding-agent/src/core/tools/output-accumulator.ts:19-22`).
- (d) Context: compaction when `contextTokens > contextWindow − reserveTokens` (16,384), keeping the newest
  ~20,000 tokens verbatim; checked between turns and on overflow (one compact-and-retry)
  (`packages/coding-agent/src/core/compaction/compaction.ts:148-152,289-291`; `packages/coding-agent/docs/compaction.md:37-96`).
  Summary prompt: "Create a structured context checkpoint summary that another LLM will use to continue the
  work" with `## Goal / Constraints & Preferences / Progress (Done/In Progress/Blocked) / Key Decisions /
  Next Steps / Critical Context`, "Preserve exact file paths, function names, and error messages";
  iterative `<previous-summary>` update ("PRESERVE all existing information"); tool results cut to 2,000
  chars in the serialized transcript; `<read-files>`/`<modified-files>` lists appended
  (`packages/coding-agent/src/core/compaction/compaction.ts:529-601`; `packages/coding-agent/src/core/compaction/utils.ts:29-150`). No automatic pruning; `context_edit` entries
  can omit/replace earlier items (extensions only) (`packages/coding-agent/docs/session-format.md:137-145`). Cache:
  Anthropic 3–4 `cache_control` breakpoints; OpenAI `prompt_cache_key` = session UUIDv7 (so every
  `--no-session` print run gets a fresh key), `prompt_cache_retention:"24h"` when long
  (`packages/ai/src/api/anthropic-messages.ts:1087-1109`; `packages/ai/src/api/openai-responses.ts:83-100`;
  `packages/coding-agent/src/main.ts:363-364`). **Cache warming**: replays the request with a 1-token cap at
  90% of the TTL when expected savings ≥ $0.05, default "streaming" (`packages/coding-agent/src/core/cache-warmer.ts:15-58`).
- (e) Extensibility (the model aim's brief cites): TS modules loaded by jiti, in-process, full user rights
  (`packages/coding-agent/docs/extensions.md:5-38`). **40 events** incl. `context`, `context_with_system`,
  `before_provider_request` (payload replace), `tool_call` (block/mutate), `tool_result` (replace),
  `message_end`, `turn_end`/`agent_before_settle` (append + continue), `cache_warming_decision`
  (`packages/coding-agent/src/core/extensions/types.ts:1222-1436`). Registration: tools, commands, shortcuts,
  flags, message/entry renderers, markdown transformers, providers, `setActiveTools`, `setModel`,
  `setThinkingLevel` (`packages/coding-agent/src/core/extensions/types.ts:1443-1640`). UI: dialogs, status/widgets/header/footer, `custom()` overlays,
  editor replacement, autocomplete providers, themes (JSON, hot reload in agent dir)
  (`packages/coding-agent/src/core/extensions/types.ts:145-293`; `packages/coding-agent/docs/themes.md:40-47`). Packages via npm/git (`packages/coding-agent/docs/packages.md:12-14`).
- (f) Multi-agent: none built in; example `subagent` extension spawns `pi --mode json -p --no-session`
  processes (≤ 8 tasks, 4 concurrent) (`packages/coding-agent/examples/extensions/subagent/index.ts:33-34,300-346`).
  `durable` models background tasks and owner subtrees but is unused.
- (g) Perf: Node ≥ 22 esbuild bundle or `bun build --compile` binary (`scripts/build-binaries.sh:131-133`); lazy
  provider modules (`packages/ai/src/api/anthropic-messages.lazy.ts:4`). TUI: main-screen (keeps scrollback)
  **differential rendering** — redraw from first changed line, full redraw on width change or change above
  viewport, all inside synchronized output `CSI ?2026h/l` (`packages/tui/README.md:3,61-62,741-749`).
  F1: 228 ms to first request, 147 MB RSS, 75.6 MB binary, zstd request bodies on the codex backend.
- (h) Evals: `vitest-evals` "documentation-lift" suites in Docker (`with_docs` vs `without_docs`), reporting
  pass-rate lift, input/output/cacheRead/cacheWrite tokens, tool calls, estimated $; no TB/SWE-bench
  (`packages/evals/README.md:3-103`; `packages/evals/src/report.ts:22-53`). Session analysers
  (`scripts/session-context-stats.mjs`, `scripts/edit-tool-stats.mjs`) mine real sessions.
- Providers/headless: `openai-codex` → `https://chatgpt.com/backend-api` + `/codex/responses`, transport
  auto (WebSocket first, SSE fallback), OAuth browser + device code (`packages/ai/src/providers/openai-codex.ts:9-17`;
  `packages/ai/src/api/openai-codex-responses.ts:294-370,641-654`); OpenAI-compatible via `models.json`
  `api: "openai-completions" | "openai-responses"`, a `baseUrl`-only provider entry reroutes built-ins
  (`packages/coding-agent/docs/models.md:12-64`; `packages/coding-agent/src/core/provider-composer.ts:306-311`). Usage per assistant
  message `{input, output, cacheRead, cacheWrite, reasoning?, cost}` (`packages/ai/src/types.ts:420-441`).
  With Anthropic OAuth pi injects a Claude Code identity block and renames tools
  (`packages/ai/src/api/anthropic-messages.ts:86-100`).

(F4 omp: pending)

### F5. unreal-agent (Go library + headless runner; "async-first")

- (a) Process: one Go process, **no TUI, no daemon**. `unreal-agent-runner` takes one JSON request
  (stdin, positional, or `-p`), streams persisted session items to stdout as JSONL, exits when idle
  (`refs/unreal-agent/cmd/unreal-agent-runner/README.md:3-4`; it submits a `StopWhenIdle` control input,
  `refs/unreal-agent/cmd/internal/agentrunner/run.go:400-408`). Components: inbox, coordinator, session
  store, context builder, LLM adapter, tool registry, operation manager (`refs/unreal-agent/README.md:31-40`).
  The only "protocol" is `inbox.Input{ID, Kind: external|control|crash, Payload}`
  (`refs/unreal-agent/harness/inbox/inbox.go:17-27`); control modes `hard`, `when_idle`, `heartbeat`,
  `settings` (reasoning effort can change mid-session) (`harness/inbox/inbox.go:50-59`).
- (a) Persistence: `<dir>/<session>.session.jsonl`, append-only, `formatVersion = 2`, v1 refused
  (`refs/unreal-agent/harness/sessionstore/localfile/store.go:22`, `harness/sessionstore/localfile/codec.go:15,114-120`);
  item kinds `fork|input|turn|model_response|tool_call_status`
  (`refs/unreal-agent/harness/sessionstore/sessionstore.go:23-29`); operations saved latest-wins
  (`harness/sessionstore/sessionstore.go:100-101`); `Fork(id, parentID, previousTurnID)` (`harness/sessionstore/sessionstore.go:103-108`), with a
  known gap: "Forks leave inherited calls without results" (`refs/unreal-agent/harness/coordinator/loop.go:552`).
- (b) Prompt: embedded `preamble.md` (1,371 B: turn model, async tool semantics, heartbeat, "I believe in
  you!") + runner default system prompt (`cmd/internal/agentrunner/run.go:45-50`) = 1,566 chars (~390 tokens), fully static, no
  date/cwd/env (`refs/unreal-agent/harness/contextbuilder/prompts/preamble.md:1-11`). Skills are appended
  once as `<available_skills>` XML (name, description, location) (`harness/contextbuilder/skills.go:16-44`),
  discovered from `<workspace>/.harness/skills/*/SKILL.md` (`cmd/internal/agentrunner/run.go:323-326`).
- (c) Tools: exactly `Bash`, `ViewImage`, `SkillUse` (`refs/unreal-agent/harness/tool/registry.go:15-19`,
  `harness/tool/static.go:39-88`); **no edit/read/write tools** — edits go through the shell. Bash runs every
  command in the background as a durable operation: non-PTY pipes (`harness/primitives/process.go:54-58`),
  process group recorded for crash recovery, stdout/stderr spilled to `<opdir>/<opID>/out|err`
  (`harness/operation/shell.go:19-20,466-477`). `max_output_length` default 40,000 chars, max 1,000,000
  (`harness/operation/output.go:9-10`), head = limit/2, tail = rest, marker `...N bytes truncated; complete
  output in <path>...` (`harness/operation/output.go:29-36`). ViewImage caps 2000×2000 px, 5 MB−1 KB base64
  (`harness/tool/viewimage/viewimage.go:15-17`).
- (d) Context: builder = `committedPrefix` + `stagedSuffix`, append-only (`harness/contextbuilder/builder.go:23-29,125-137`).
  `Build()` returns `Result{Request, Report{Changes[]{Kind omitted|truncated|compacted, Source, Reason}}}`
  (`harness/contextbuilder/contextbuilder.go:9-30`), but **nothing populates `Report` yet** (`harness/contextbuilder/builder.go:136`;
  test asserts empty, `harness/contextbuilder/builder_test.go:181`). A `compaction` turn type is persisted and skipped by the
  coordinator (`harness/session/session.go:14`; `harness/coordinator/loop.go:467,610`), yet no code creates one: **no compaction,
  no pruning today**. Cache: `prompt_cache_key` = session id plus `session-id` header on the ChatGPT
  backend (`harness/llm/clients/openaicodex/client.go:62`; `harness/coordinator/loop.go:375-377`); OpenRouter gets `x-session-id`
  and top-level `cache_control {ephemeral, ttl 1h}` (`harness/llm/clients/openrouter/client.go:47-58`). Requests are
  deterministic JSON, `store:false`, encrypted reasoning included, summary `auto`
  (`harness/llm/responsesapi/request.go:27-57`).
- (f) Async-first, concretely: a tool call is translated synchronously into durable operations
  (`harness/coordinator/loop.go:780-814`); the model immediately sees `"Tool call is still running. Its result arrives in a
  later turn…"` (`harness/contextbuilder/builder.go:16`). If results land before the placeholder was sent, it is replaced in the
  staged suffix; otherwise the real result is **appended later as a second tool result for the same
  call_id**, so history stays append-only (`harness/contextbuilder/builder.go:103-123`; `harness/contextbuilder/builder_test.go:200-243`). After a
  response with tool calls the coordinator waits up to 1 s (`toolCallRunGracePeriod`) to batch fast
  results into one turn (`harness/coordinator/loop.go:28,247-252`); inputs are slurped with 1 ms idle / 100 items
  (`harness/coordinator/loop.go:23-26,429-451`). New external input cancels the in-flight model request and restarts the turn
  with it (`harness/coordinator/loop.go:346-351,420-425`). While only tools are pending, a heartbeat user message fires after
  `-tool-heartbeat-interval` (default 10 min) (`harness/coordinator/loop.go:118-124,282-304`; `cmd/internal/agentrunner/run.go:174`). Inputs carry
  caller-supplied IDs deduplicated per session (`harness/inbox/local.go:59-86`). Operations are versioned,
  serializable state machines with checkpoints (`harness/operation/operation.go:30-65`); `RemoteJobHandler`s
  (plan type+version) let a manager proxy work elsewhere (`harness/operation/remote_job.go:26-32`); none ship.
  No subagents.
- (g) Perf: 12.1 MB static binary, 51 ms to first request, 16 MB RSS (F1). No websocket transport, no
  prewarm (no `websocket`/`previous_response_id` in source). Every session append re-opens, truncates to
  the committed size, writes and `fsync`s the log (`harness/sessionstore/localfile/store.go:423-431`).
- (e) Extensibility = swap Go interface implementations (store, builder, adapter, operation manager); the
  README's example is a proxy manager forwarding serialized operations to a manager inside a remote
  sandbox (`refs/unreal-agent/README.md:44-55`). No plugin/extension runtime, no MCP.
- (h) In-repo eval: `benchmarks/harbor` = Harbor 0.22.0 installed-agent adapter
  (`refs/unreal-agent/benchmarks/harbor/pyproject.toml:6`); uploads a checksummed binary bundle, runs the
  runner with Bash+ViewImage (SkillUse disallowed), converts JSONL to ATIF `trajectory.json` with
  prompt/cached/completion/reasoning/cache-write totals; cost left `None`
  (`benchmarks/harbor/src/harness_harbor/agent.py:124-210`). Accepts only `openai/`, `openrouter/`,
  `fireworks_ai/` models — not the subscription provider (`benchmarks/harbor/src/harness_harbor/agent.py:45-52`). README documents a TB 4.0
  run: `-d terminal-bench/terminal-bench@4.0.0 -e modal -m openai/gpt-6-astra --ak thinking_level=max
  -k 5 -n 40` (`benchmarks/harbor/README.md:47-53`).
- Providers: all five clients speak only `/responses` (OpenAI, `openai-codex`, OpenRouter, Fireworks,
  Ollama) (`cmd/internal/agentrunner/providers.go:11-58`); the codex client reads `~/.codex/auth.json` or env
  tokens, never logs in or refreshes, and accepts a base-URL override **only for loopback IPs**
  (`harness/llm/clients/openaicodex/client.go:17,90-103`, `harness/llm/clients/openaicodex/credentials.go:21-47,71-72`).

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
  `1:a3|function hello() {` — edits address `line:hash` anchors, stale hashes are rejected.
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
  1.0, cached 0.1, cache write 1.25, output 5.0) (`:42-62`); a history rewrite pays back only after
  ≈ `11.5 × A / R` requests (`:62`). Fairness: same model/effort, default prompts/tools, isolated HOME,
  **one recording proxy reading usage from `response.completed`; self-reports not trusted** (`:98-107`);
  ≥3 reps, Wilson CIs, paired non-inferiority δ = 8 pp (`:120-126`).
- tny's real-usage baseline: 97.3% cache hit, **110.8k mean input tokens/request**; cached input = 66% of
  cost; shell output = 79% of tool-result bytes (`refs/tny/docs/benchmarks/harness-efficiency.md:70-86`). I.e. once caching works,
  **context size re-read per request** dominates cost.
- `refs/tny/tests/bench/harness_bench/adapters.py:13,110-354`: working proxy configs for tny, codex, pi,
  omp, hermes, opencode, fx and unreal-agent against `chatgpt.com/backend-api/codex/responses`;
  multi-turn resume only for tny and codex (`:14`). Proxy computes instructions/tools/history/tool-output
  sizes per request, including the `additional_tools` layout (`refs/tny/tests/bench/harness_bench/proxy.py:43-96`).
- ADR 0077/0078: vs Codex CLI 0.154, same model — conversation warm cache hit 86.8% (tny) vs 98.4% (codex),
  request bytes 399k vs 1.13M (`refs/tny/docs/adr/0077-openai-prompt-cache-routing.md:111-118`); fresh tasks
  with a workspace-shared `prompt_cache_key` + `session-id` routing group: 78.1% vs 30.3% cached
  (`refs/tny/docs/adr/0078-workspace-shared-prompt-cache.md:60-68`); sharing only the JSON key gave zero hits;
  WebSocket continuation "did not produce a superior cache-hit result" (`0078…:94-100`); the ChatGPT
  backend sends an `x-codex-turn-state` affinity token to replay within a turn (`0077…:23-41`).
