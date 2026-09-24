# R3 — ACP (Agent Client Protocol) + MCP as aim's protocol substrate

Researched 2026-09-25. Sources are the scratchpad clones (`refs/…`), crate sources (crates.io tarballs),
the installed `claude-agent-acp` 0.81.2 (mise), and web pages. Every non-obvious claim has a citation;
`UNVERIFIED` marks what could not be checked. Opinions live only in *Implications for aim*.

## TL;DR

- **claude-agent-acp 0.81.2 routes nothing through ACP client `fs`/`terminal`.** This has been true since 0.18.0.
  Every Claude Code tool (Read/Edit/Bash/Glob/Grep/Web*/Task…) runs inside the adapter host's `claude` process.
  The ACP `terminal` content is display-only (§2.7).
- **SSH-shadowing Claude therefore means replacing its tools.** Set `_meta.claudeCode.options {tools:[…],
  toolAliases:{Bash→mcp__aim__bash,…}, strictMcpConfig:true, settingSources:[], allowedTools:["mcp__aim"]}` and
  inject an aim MCP server (stdio or HTTP; MCP-over-ACP entries are silently dropped).
- tny live-verified that recipe over real SSH on adapter 0.75.1. On 0.81.2 the code path is identical but a live
  run is `UNVERIFIED`, and open issue #883 reports stdio MCP servers not spawning.
- **ACP v2 (draft) deletes client `fs`/`terminal`.** Its stated replacement is "expose an MCP server to the
  agent", so MCP injection is the only future-proof way to route another agent's tools through aim (§3).
- **Claude login has no API-key method.** Only terminal auth: the client runs `<adapter> --cli auth login
  --claudeai|--console` in a TTY, or `--cli` → `/login` when SSH env vars are set. `authenticate` handles only
  `gateway`. Env credentials and `providers/set` also work. Subscriptions work unless `--hide-claude-auth` is
  passed. Raw `initialize` JSON is in §2.1.
- **Model, effort, mode and fast mode are all `session/set_config_option`** (ids `model`/`effort`/`mode`/`fast`).
  ACP has no `set_model` any more. Usage arrives as `usage_update{used,size,cost}`, plus per-turn
  `PromptResponse.usage` and `_meta.quota`.
- **The adapter churns.** Seven minors in three weeks, breaking changes, and open lifecycle bugs (fork, stdio MCP,
  stuck prompts). Pin it via mise and gate on a *capability* probe; tny's exact-version gate (`0.75.1`) is already
  stale.
- **Rust ACP SDK = `agent-client-protocol` 2.2.0** (schema =1.9.1; wire v1 stable, v2 behind a feature).
  It uses a role/builder API; `ClientSideConnection` is gone. It is executor-agnostic, needs `Send` handlers,
  and runs one task per connection. Siblings: `-http` (HTTP/SSE/WS), `-conductor`, `-polyfill`.
- **MCP's latest spec is 2026-07-28, a stateless clean break.** It removes `initialize`, sessions and
  server→client requests, and adds `server/discover`, MRTR, `subscriptions/listen` and `extensions`. Tasks became
  an extension. Shipping clients such as Claude Code still use `initialize`, so servers must speak both eras.
- **MCP gaps for a harness**: no streaming partial tool output (every proposal closed or dormant), no PTY, only
  coarse file watching, no webhooks. Cancellation, concurrency and versioning fit (§6).
- **rmcp 3.4.1** (the official Rust MCP SDK; codex pins 3.2.0) serves both eras, including MRTR, `listen` and
  tasks. It supports stdio, child process, AsyncRead/AsyncWrite and streamable HTTP, and is tokio-only. Gaps: no
  WebSocket, no server-side auth, no stdio line cap. `agent-client-protocol-rmcp` is still on rmcp ^2.
- **Other ACP agents**: codex-acp 1.13.1 (now TypeScript; delegates nothing) and gemini 0.61 (`--acp`; files
  only). goose, kimi and mistral-vibe delegate both files and shell. The registry lists 41 agents. **tny exposes
  no ACP agent**: its server was deleted 2026-09-19 (ADR 0152), and only the client plus an MCP bridge came back
  (ADR 0164).
- **Recommendation (opinion): harness.** The harness is a library plus a canonical **`aim-harness/1` JSON-RPC**
  (handles, seq-numbered streams, PTY, watch; codex's exec-server is the precedent), with an **MCP façade (both
  eras) in the same binary**. `ssh host aim-harness mcp --stdio` then works as the SSH-shadow endpoint for any
  MCP agent.
- **Recommendation (opinion): the rest.** Use an ACP-v2-shaped **`aim-daemon/1`** for UIs, with an ACP-agent
  façade later. ACP client, MCP client and MCP server live at the edges. One schema (serde+schemars), many
  transports (NDJSON over stdio/unix/ssh, WS). No gRPC.
- **Protocol overhead is not the bottleneck** (measured): 11/18/212 µs per JSON-RPC round trip (256 B/4 KB/64 KB)
  over a unix socket. An SSH round trip (~10–30 ms/call, tny ADR 0022) and model latency dominate.

## Findings

### 1. The Rust ACP SDK (`agent-client-protocol` 2.2.0)

**Versions** (`cargo search`/`cargo info`, 2026-09-25): `agent-client-protocol` **2.2.0** (2026-09-18,
MSRV 1.88, edition 2024, repo `agentclientprotocol/rust-sdk`), pinned to `agent-client-protocol-schema`
**=1.9.1**. Siblings: `-tokio` 0.11.1, `-rmcp` 3.1.1, `-conductor` 2.2.0 (plus `-http`, `-polyfill`,
`-derive`, `-trace-viewer`, named in the crate README).

**The task's API names are stale.** `ClientSideConnection`/`AgentSideConnection` and the `Agent`/`Client` traits are
gone. Changes: the "sacp" redesign in 0.11.0 (2026-04), tokio removed in 0.12.0, and API breaks in 2.0.0
(2026-07) with the same v1 wire (`CHANGELOG.md:160-235, 344-400`).
- Roles are the unit values `Client`, `Agent`, `Proxy` and `Conductor` (`src/lib.rs:145-148`).
  `Client.builder()` returns a `Builder`. Handlers attach with `.on_receive_request(async |req, responder, cx| …,
  on_receive_request!())` / `.on_receive_notification(…)`, then either `.connect_with(transport, async |cx| {…})`
  (drive + send) or `.connect_to(transport)` (serve only) (`src/jsonrpc.rs:1494, 1569, 1795, 1871`).
- Transports implement `ConnectTo<Role>`: `AcpAgent` (spawns a subprocess; `AcpAgentConfig{command,args,env}` is
  serde-(de)serializable, so it can live directly in aim config; `AcpAgent::from_args`/`FromStr` parse a
  shell-words command), `Stdio`, `ByteStreams` (any `futures::io::AsyncRead/AsyncWrite` pair, so unix
  sockets and `ssh` pipes work), `Lines` (NDJSON) and `Channel` (in-process frames) (`src/acp_agent.rs:37-78, 189,
  822, 882`; `src/lib.rs:88-98, 130-138, 174-179`).
- **Runtime**: executor-agnostic. The connection is a future the embedder polls. `cx.spawn(fut)` pushes
  `Send + 'static` futures into an in-connection task actor, not a runtime spawn
  (`src/jsonrpc/task_actor.rs`; `src/jsonrpc.rs:3238-3245`). Handlers must be `Send`
  (`src/jsonrpc.rs:1506, 1578`). **One connection is processed on one task**, and a handler that blocks
  stalls the connection (`src/jsonrpc.rs:552-566`). There is no `LocalSet`/`!Send` requirement, and tokio
  works (the examples use `#[tokio::main]`). Subprocess I/O uses `async-process` and `async-io` timers
  (their own reactor thread) (`src/acp_agent.rs:13, 280, 541`). WASI builds work; `wasm_js` targets
  browser wasm (`CHANGELOG.md:14-30`).
- **Protocol versions**: `ProtocolVersion::{V0, V1}`, where `LATEST = V1`. `V2` exists only with `unstable_protocol_v2`
  (schema `src/version.rs`). Draft v2 **drops the client `fs/*` and `terminal/*` methods entirely**. Its
  `ClientCapabilities` has only `auth`, `elicitation` and `nes` (schema `src/v2/client.rs:2195-2227`). Terminals
  become agent-owned `TerminalUpdate`/`TerminalOutputChunk` session updates (`src/v2/terminal.rs:187-192`).
  A prompt response only acknowledges acceptance; output and completion (`StateUpdate`) arrive as
  `session/update` (crate README). v2 also drops `session/load` and `session/set_mode` and adds `auth/login`/`auth/logout`.
- **Stable v1 methods** (schema `src/v1/agent.rs:4753-4789`, `client.rs:2691-2707`): `initialize`,
  `authenticate`, `logout`, `session/{new,load,list,delete,resume,close,prompt,cancel,set_mode,set_config_option}`,
  client-side `session/request_permission`, `fs/{read,write}_text_file`, `terminal/{create,output,
  wait_for_exit,kill,release}`, `elicitation/create`, and `$/cancel_request`. **There is no `session/set_model`
  any more**: models are config options (category `model`). Cargo **unstable features**: `unstable_session_fork`
  (`session/fork`), `unstable_llm_providers` (`providers/{list,set,disable}`), `unstable_mcp_over_acp`
  (`McpServer::Acp`, `mcpCapabilities.acp`, `mcp/{connect,message,disconnect}`), `unstable_end_turn_token_usage`
  (`PromptResponse.usage`), `unstable_session_compaction`, `unstable_session_notices` and
  `unstable_plan_operations`, plus `unstable_nes` (schema only) and `unstable_protocol_v2` (`Cargo.toml` features).
- **MCP-over-ACP**: the `mcp_server` module builds MCP servers. With `unstable_mcp_over_acp` it attaches them to
  a session (`with_mcp_server`) as `McpServer::Acp`, tunnelled as `mcp/*` messages over the ACP pipe. For agents
  that only speak HTTP MCP, the README says to put `agent-client-protocol-polyfill` "immediately before an
  HTTP-capable agent" (crate README, "MCP Server Attachment").

Minimal client, adapted from `examples/yolo_one_shot_client.rs` (APIs checked against the 2.2.0 source; not
compiled). The agent side is `Agent.builder().on_receive_request(…).connect_to(Stdio::new())`
(`examples/simple_agent.rs`).

```rust
Client.builder()
  .on_receive_notification(async |n: SessionNotification, _cx| { render(n.update); Ok(()) }, on_receive_notification!())
  .on_receive_request(async |r: RequestPermissionRequest, responder, _cx| responder.respond(decide(&r)), on_receive_request!())
  .connect_with(AcpAgent::new(AcpAgentConfig::new("claude-agent-acp")), async |cx: ConnectionTo<Agent>| {
      cx.send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
          ClientCapabilities::new().auth(AuthCapabilities::new().terminal(true)))).block_task().await?;
      let s = cx.send_request(NewSessionRequest::new("/abs/cwd")
          .mcp_servers(vec![McpServer::Stdio(McpServerStdio::new("aim", "/bin/aim-mcp-relay"))])
          .meta(claude_meta())).block_task().await?;             // serde_json::Map, §2.8
      cx.send_request(PromptRequest::new(s.session_id, vec![ContentBlock::Text(TextContent::new("hi"))]))
        .block_task().await?;
      Ok(())
  }).await?;
```

### 2. `claude-agent-acp` 0.81.2 in depth

Source: `refs/claude-agent-acp` at the `v0.81.2` release commit (`package.json:6`). It depends on
`@agentclientprotocol/sdk` 1.5.0 and `@anthropic-ai/claude-agent-sdk` 0.3.280 (`package.json:68-69`). The agent
runs the SDK's bundled **native `claude` binary** (platform optional dependency), which `CLAUDE_CODE_EXECUTABLE`
overrides (`src/acp-agent.ts:1634-1665, 8512`). `claude-agent-acp --cli …` just `exec`s that binary
(`src/index.ts:12-41`). The whole agent is one 10.8k-line file (`src/acp-agent.ts`).

#### 2.1 Raw `initialize` (captured 2026-09-25, no session, no prompt)

Scripted with `{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},
"terminal":true,"auth":{"terminal":true},"_meta":{"terminal-auth":true}}}`. Paths are shortened to `~`; email and
org are redacted:

```json
{"protocolVersion":1,"agentInfo":{"name":"@agentclientprotocol/claude-agent-acp","title":"Claude Agent","version":"0.81.2"},
 "agentCapabilities":{"_meta":{"claudeCode":{"promptQueueing":true},"authStatus":{}},
   "promptCapabilities":{"image":true,"embeddedContext":true},"mcpCapabilities":{"http":true,"sse":true},
   "auth":{"logout":{}},"providers":{},"loadSession":true,
   "sessionCapabilities":{"additionalDirectories":{},"close":{},"delete":{},"fork":{},"list":{},"resume":{},"subagents":{}}},
 "authMethods":[
   {"id":"claude-ai-login","name":"Claude Subscription","description":"Use Claude subscription ","type":"terminal",
    "args":["--cli","auth","login","--claudeai"],
    "_meta":{"terminal-auth":{"command":"~/.local/share/mise/installs/node/26.8.1/bin/node","args":["~/.local/share/mise/installs/npm-agentclientprotocol-claude-agent-acp/0.81.2/bin/claude-agent-acp","--cli","auth","login","--claudeai"],"label":"Claude Login"}}},
   {"id":"console-login","name":"Anthropic Console","description":"Use Anthropic Console (API usage billing)","type":"terminal",
    "args":["--cli","auth","login","--console"],"_meta":{"terminal-auth":{"command":"…node","args":["…claude-agent-acp","--cli","auth","login","--console"],"label":"Anthropic Console Login"}}}],
 "_meta":{"jetbrains":{"air":{"version":1,"capabilities":["sessionFailure","agentFileChangeReport","nativeSubagentSessions","asyncTasks","recommendedValue"]}},
          "steering":{"supported":true},"goal":{"version":1,"controlMethod":"_session/goal","actions":["set","clear"]}}}
← {"jsonrpc":"2.0","method":"_auth/status_update","params":{"authStatus":{"kind":"account","label":"Claude Max","account":{"plan":"max","email":"<redacted>","organization":"<redacted>"}}}}
```

Variants:
- With **empty client capabilities**, `authMethods` is `[]`. The `agentCapabilities` are byte-identical: they
  ignore client capabilities.
- When any of `NO_BROWSER`, `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY` or `CLAUDE_CODE_REMOTE` is set, the two
  methods become one: `{"id":"claude-login","name":"Log in with Claude","description":"Run \`claude /login\` in the
  terminal","type":"terminal","args":["--cli"]}`. The first probe ran inside an SSH session and got exactly this
  (`src/acp-agent.ts:2248-2276`). **aim launched over SSH hits this branch.**
- `gateway` / `gateway-bedrock` methods (`_meta.gateway.protocol` `anthropic`|`bedrock`) appear only when the
  client sends `clientCapabilities.auth._meta.gateway: true` (`:2217-2240`).
- `protocolVersion` is hard-coded to `1` and ignores the request (`:2333`). The agent answers `initialize`
  immediately and runs `claude auth status --json` in the background (5 s timeout). The result arrives as the
  `_auth/status_update` push (`:2213, 2590-2651`; `src/auth-status.ts:1-60`).

#### 2.2 Login: what an ACP client must do

- **No API-key auth method exists.** `authenticate` handles only `gateway`/`gateway-bedrock`, storing a
  base URL and headers. Any other `methodId` throws `"Method not implemented."` (`src/acp-agent.ts:2478-2506`).
- **Terminal auth is the only login path** (ACP v1 `AuthMethod::Terminal`: "Client runs the configured agent
  program as a separate interactive process, without passing this method to `authenticate`", args appended and
  `env` overrides; schema `v1/agent.rs:581-590, 709-739`). The client must advertise
  `clientCapabilities.auth.terminal=true` or legacy `_meta["terminal-auth"]=true` (`:2241-2242`). Then it runs
  `<adapter cmd> --cli auth login --claudeai` (subscription) or `… --console` (API billing) **in a real TTY**. Under the
  SSH env it runs `<adapter cmd> --cli`, which opens the full `claude` TUI, where the user types `/login`. Credentials land in the
  normal Claude Code store and are shared with a plain `claude` install.
- **Detecting "logged out"**: `session/new` does not check credentials (unless `--hide-claude-auth` is set). The turn fails
  with the ACP `authRequired` error when the CLI result says "Please run /login" (`:5581-5585`). The agent
  re-probes at the start of every prompt and on `logout`, and recreates signed-out queries at the next turn boundary
  (`:2863-2880, 3032-3045`). Client flow: prompt → `authRequired` → run the terminal method → retry the prompt.
- **Env credentials**: the Claude process env is `{...process.env, ..._meta.claudeCode.options.env,
  ...providerEnv}` (`:8452-8461`). So `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`),
  `ANTHROPIC_BASE_URL`, and Bedrock/Vertex variables work whether set in the adapter's environment or per session.
  `providers/set {providerId:"main", apiType: anthropic|bedrock|vertex, baseUrl, headers}` routes model traffic
  for later sessions (`:2724-2780`). It blanks every credential env var and injects `ANTHROPIC_CUSTOM_HEADERS`
  (`:9276-9300`). `logout` clears overrides and runs `claude auth logout` (`:2819-2860`).
- **Subscriptions** work by default. The opt-in argv flag `--hide-claude-auth` hides `claude-ai-login` and refuses
  subscription-billed turns with `authRequired`, `data.reason:"claude_subscription_not_supported"`
  (`src/hide-claude-auth.ts:1-60`). JetBrains' registry launches with that flag (issue #782 comments). Whether
  third-party use of a subscription is allowed by Anthropic's terms is a policy question (see *Open questions*).

#### 2.3 Sessions

- `session/new {cwd, mcpServers, additionalDirectories?, _meta?}` → `{sessionId, modes, configOptions}`
  (`:8893-8899`). `cwd` must be absolute **and exist on the machine running the adapter** (`validateCwd`,
  `:8190-8210`). MCP servers: `type:"http"|"sse"` → `{type,url,headers}`; entries with **no** `type` →
  stdio `{command,args,env}`. **Anything else, including `McpServer::Acp`, is silently dropped**
  (`:8282-8307`). The advertised `mcpCapabilities` are `{http:true, sse:true}`, without `acp`.
- `session/load` replays the full transcript as `session/update` and then answers. `session/resume` re-attaches
  without replay (`:2417-2446`). Both rebuild the Claude process if a fingerprint of `cwd`, `mcpServers` and
  `_meta` options changed (`:1216-1340`).
- `session/list {cwd?}` → `{sessionId,cwd,title,updatedAt}` from the SDK's on-disk store (`:2448-2462`).
  `session/close` tears down; `session/delete` also deletes the transcript (`:6867-6883`).
- `session/fork` (unstable) returns only `{sessionId}` and **does not register a live session**. The client must
  `session/load` or `resume` the new ID. This regressed in 0.71.0 (issue #1110; `src/fork-session.ts:187, 234`).
  A fork point can be passed via `_meta.jetbrains.air.fork.messageId`.
- `session/cancel` interrupts the query. A 30 s force-cancel backstop settles the turn as `cancelled`
  (`:346, 6497-6530`). `promptQueueing:true` means concurrent `session/prompt`s are queued (`:2948`).
  `_session/steering` injects text into a running turn (`:456, 3238-3270`).

#### 2.4 Modes, models, effort, fast mode

All of these go through `session/set_config_option`. `session/set_mode` is also served (`:10583-10620`).

| configId | category | values |
|---|---|---|
| `mode` | `mode` | `default` "Manual", `acceptEdits`, `plan`, `auto` ("Claude handles permission decisions"), `bypassPermissions` (only if allowed: not root, not disabled by settings, not opted out via `options.allowDangerouslySkipPermissions:false`); `dontAsk` is accepted but not listed (`src/session-mode.ts:300-350`) |
| `model` | `model` | SDK `supportedModels()` plus `default` (`src/session-model.ts:187-246`) |
| `effort` | `thought_level` | the model's `supportedEffortLevels` (plus `default`), present only if the model supports effort (`src/session-effort.ts:70-118`) |
| `fast` | — | boolean if the client advertises `session.configOptions.boolean`, else an on/off select, shown only when the model supports fast mode (`src/acp-agent.ts:9357, 9407-9425`) |

`MAX_THINKING_TOKENS` (env) sets the thinking budget and `CLAUDE_MODEL_CONFIG` (env) sets model overrides/allowlists
(`:8358-8361`). Open bug: `options.model` is overridden by `settings.model` (issue #1056).

#### 2.5 Prompt input, `session/update` output, usage

- **Prompt content** (`promptToClaude`, `:9591-9660`): `text` (with `/mcp:server:cmd` rewritten);
  `resource_link` (a markdown link only; Claude reads the file itself); text `resource` (appended as a
  `<context ref=…>` block; blobs ignored); `image` (base64 or http URI). `audio` is ignored.
- **Updates emitted.** Standard: `agent_message_chunk`, `agent_thought_chunk`, `user_message_chunk` (replay),
  `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update`, `config_option_update`,
  `session_info_update` (titles), `usage_update`. Unstable, capability-gated: `notice`, `compaction_update`,
  `compaction_summary_chunk`. JetBrains AIR extensions: `subagent_spawned`, `subagent_state_update`,
  `async_task_spawned`, `async_task_progress`, `async_task_state_update` (grep of `sessionUpdate: "` across `src/`).
  Extension notifications: `_auth/status_update`, and `_claude/sdkMessage` (raw SDK stream, opt-in via
  `_meta.claudeCode.emitRawSDKMessages: true|filters[]`, `:1376-1382, 4254`).
- **Usage**:
  - `usage_update {used, size, cost:{amount, currency:"USD"}, _meta:{"_claude/model"}}` after each result and
    each compaction (`:3573-3590, 4446, 5327-5345`). `size` is the context window, learned from
    `result.modelUsage` and guessed before the first turn (`:10647-10674`).
  - `PromptResponse.usage {input,output,cachedRead,cachedWrite,total}Tokens` covers the main loop only.
    `_meta.quota.model_usage[]` adds subagents and compaction, in the same shape as codex-acp (`:9026-9090`).
  - Rate limits arrive in `_meta["_claude/rateLimit"]` (`:6367`). Usage is per-turn, not cumulative as the RFD asks (#390).

#### 2.6 Permission flow

The SDK's `canUseTool` → `session/request_permission {sessionId, toolCall{toolCallId, name, kind, title, status:
"pending", rawInput, content, locations}, options[], _meta.permission{version:1, title, description?,
defaultToNo?}}`. The request is sent with a cancellation signal, which becomes `$/cancel_request` if the turn is
cancelled.

- **Option IDs**: `allow-once`, `allow-with-updates` (applies a snapshotted SDK `PermissionUpdate`: a rule, mode,
  or directories), `allow-skill-exact|prefix`, `exit-plan-{auto,bypass,accept-edits,default}`,
  `exit-plan-clear-*`, `reject`.
- Options are grouped `allow_once` < `allow_always` < reject kinds.
- `{outcome:"cancelled"}` aborts the tool, which is different from a reject.

Sources: `docs/permission-extension.md`, `src/permissions/`. `AskUserQuestion` and MCP elicitations use
ACP `elicitation/create`, and are disabled when the client lacks `elicitation.form` (`:8366-8374, 8492-8511`).

#### 2.7 Client-capability routing — the SSH crux (verified)

- **Nothing is routed to the client.** 0.18.0 changed this: "Switch over to built-in Claude tools. … it won't use
  client capabilities for files or terminals" (`CHANGELOG.md:952`). In 0.81.2, `fs/read_text_file` and
  `fs/write_text_file` exist only as an unused wrapper and pass-through (`:1911-1916, 7300-7308`). The only
  callers are test mocks (`src/tests/acp-agent.test.ts:475-476`). There is no `terminal/create` anywhere, and no
  in-process MCP server (`grep createSdkMcpServer` → 0 hits).
- **Every built-in tool executes inside the `claude` process on the adapter's host**: Read, Write, Edit, Bash,
  Glob, Grep, NotebookEdit, WebFetch, WebSearch, Task/Agent subagents, background shells, Monitor, and so on.
- The ACP `{type:"terminal", terminalId: <tool_use id>}` content plus `_meta.terminal_info` /
  `terminal_output(_delta)` / `terminal_exit` is **display-only**. The adapter streams Claude's own Bash output to
  clients that advertise `clientCapabilities._meta.terminal_output(_delta)` (`src/tools.ts:170-190, 905-955`;
  `src/acp-agent.ts:9975-9995`). The v2 draft makes this agent-owned-terminal model official (§1).
- **What the code does support for tool shadowing**: `_meta.claudeCode.options` passes through to the SDK almost
  verbatim (`:8345-8351`, merged at `:8471-8520`). The only exceptions are `cwd`, `permissionMode`, `canUseTool`,
  `includePartialMessages` and `executable` (ACP-managed), and `agent` (deleted). Recipe:
  - `tools: []` (or a whitelist such as `["WebSearch","WebFetch","TodoWrite","Task"]`). This is the SDK's base set,
    and `[]` disables every built-in (`sdk.d.ts:1563-1575`). Legacy form: `_meta.disableBuiltInTools: true` (`:8380-8382`).
  - `strictMcpConfig: true`, so only `mcpServers` from the request are loaded (`sdk.d.ts:2220-2228`).
  - `settingSources: []` for SDK isolation. This also drops CLAUDE.md, skills and settings; `'project'` is required
    for CLAUDE.md (`sdk.d.ts:2170-2179`).
  - **`toolAliases`**: "a host that runs Bash inside a remote sandbox via an MCP tool can set `{ Bash:
    'mcp__workspace__bash' }`" (`sdk.d.ts:1537-1562`). Aliased names keep the model's trained vocabulary.
    Built-in input schemas to mirror: `BashInput`, `FileReadInput`, `FileEditInput`, `FileWriteInput`, `GlobInput`
    and `GrepInput` (`sdk-tools.d.ts:788-956`).
  - `allowedTools: ["mcp__aim"]` so aim's own tools do not trigger ACP permission prompts; aim enforces policy
    inside its MCP server instead.
- **Prior art (user's own)**: tny sends exactly `_meta:{disableBuiltInTools:true, claudeCode:{options:{tools:[],
  settingSources:[], strictMcpConfig:true, maxTurns?}}}` plus a mandatory stdio MCP relay into its runtime
  (`refs/tny/src/backends/acp/acp_client.c:476-482`; ADR 0164). It **live-verified real-SSH tool routing on
  adapter 0.75.1**, with checks `only_tny_tools`, `remote_proof` and `local_untouched`, using tools
  `read_file`/`write_file`/`terminal`, including resume across processes
  (`refs/tny/docs/verification/acp-client/live-observation.json`). tny gates `--ssh` to that verified adapter
  version (ADR 0164 "Consequences").
- **Caveats**:
  1. `cwd` must exist locally, so shadowed sessions need a local scratch directory (tny does this).
  2. `tools: []` also removes WebSearch/WebFetch/Task/Todo unless re-listed.
  3. Open issue #883 reports that session-scoped **stdio** MCP servers are never spawned (0.59–0.70, standalone
     repro). tny's 0.75.1 evidence contradicts it for the strict/tools-only configuration. Status on 0.81.2 is
     `UNVERIFIED` (running a real session was out of scope), so this needs a conformance test.
  4. MCP-over-ACP cannot reach this adapter directly (dropped at `:8282-8307`). Use stdio, streamable HTTP
     (`mcpCapabilities.http`), or the Rust `-polyfill` proxy that turns `McpServer::Acp` into a localhost HTTP
     endpoint (`UNVERIFIED` against this adapter).
  5. Claude Code's MCP limits apply to replacement tools (https://code.claude.com/docs/en/mcp):
     - Output: it warns above 10k tokens and caps at `MAX_MCP_OUTPUT_TOKENS` (default 25,000). Over the cap, the
       result is saved to a file and replaced by its path. A tool can raise its own cap with
       `_meta["anthropic/maxResultSizeChars"]` (≤500,000) in `tools/list`.
     - Timeouts: `MCP_TIMEOUT` (startup) and `MCP_TOOL_TIMEOUT` / per-server `timeout` (execution).
     - Tool search is on by default, so MCP tools are deferred behind `ToolSearch`. `ENABLE_TOOL_SEARCH=false`
       (or a custom `ANTHROPIC_BASE_URL`) disables it.
     - aim sets these through `_meta.claudeCode.options.env`.

#### 2.8 `_meta` / extension surface (client → adapter)

| key | where | effect | source |
|---|---|---|---|
| `_meta.systemPrompt` | session/new,load,resume | string → replace; object → `{type:"preset",preset:"claude_code", append?, excludeDynamicSections?}` | `:8308-8328` |
| `_meta.claudeCode.options.*` | same | SDK `Options` passthrough: `tools`, `allowedTools`, `disallowedTools` (merged), `toolAliases`, `mcpServers` (merged), `hooks` (merged; callbacks can't cross JSON), `settingSources`, `strictMcpConfig`, `settings` (object or path), `env`, `model`, `effort`, `thinking`, `maxTurns`, `maxBudgetUsd`, `taskBudget`, `additionalDirectories`, `agents`, `plugins`, `skills`, `sandbox`, `systemPrompt`, `outputFormat`, `resume`, `persistSession`, `extraArgs`, `allowDangerouslySkipPermissions:false` | `:1216-1275, 8345-8520` |
| `_meta.claudeCode.emitRawSDKMessages` | same | `_claude/sdkMessage` raw stream | `:1376-1382` |
| `_meta.disableBuiltInTools` | same | legacy `tools: []` | `:8380-8382` |
| `_meta.additionalRoots` | same | legacy `additionalDirectories` | `:8393-8395` |
| `_meta.steering.idleBehavior` | `_session/steering` | `promptRequired` → don't auto-start a turn | `:3261` |
| `clientCapabilities._meta.terminal_output(_delta)` | initialize | Bash output as terminal display meta | `:9984-9986` |
| `clientCapabilities._meta["terminal-auth"]`, `.auth.terminal`, `.auth._meta.gateway` | initialize | auth methods offered | `:2217, 2241-2242` |
| `clientCapabilities.elicitation.{form,url}`, `.session.{notices,compaction,configOptions.boolean}`, `.subagents`, `_meta.jetbrains.air.capabilities` | initialize | AskUserQuestion, MCP elicitation, notices, compaction events, boolean options, native subagent sessions, AIR extensions | `:8363-8374`; `src/session-notices.ts:21`; `src/context-compaction.ts:42`; `src/acp-subagents.ts:102` |
| ext methods | — | `_session/steering`, `_session/async_task/stop`, `_session/goal {set|clear}` | `:456-459`; `src/goal-extension.ts:5` |
| env (adapter process) | spawn | `CLAUDE_CODE_EXECUTABLE`, `MAX_THINKING_TOKENS`, `CLAUDE_MODEL_CONFIG`, `CLAUDE_AGENT_LOGS` (log dir), creds and routing vars | `src/index.ts:63`; `:8358-8361, 8512` |

#### 2.9 Churn, limitations, known bugs

- **Churn**: seven minor releases from 0.75.1 (09-05) to 0.81.2 (09-24). 0.77.0 was breaking (removed
  `options.agent`) (`CHANGELOG.md:1-120`). Every push to `main` publishes a `preview` build (`AGENTS.md`).
- **Open issues** (2026-09-25):
  - Protocol and sessions: #883 (stdio MCP), #1110 (fork), #1056 (model overridden by settings), #1041
    (case-sensitive `session/list` cwd).
  - Lifecycle: #896 (response withheld until cancel), #1039/#1145 (steered or queued prompts never resolve),
    #1011 (orphaned `claude` children), #994 (cancel leaves background subagents running).
  - Slash commands: #642/#643 (`/usage`, `/context` output).
- **Unsupported slash commands**: `clear`, `cost`, `keybindings-help`, `login`, `logout`, `output-style:new`,
  `release-notes`, `todos` (`:9540-9549`).

### 3. ACP spec status, other agents, and tny

**Spec** (repo `agentclientprotocol/agent-client-protocol` @ d880573; RFDs at `agentclientprotocol.com/rfds/<slug>`):
- The wire protocol is **1**. The latest schema releases are `schema-v1.23.0` and `schema-v2.0.0-alpha.5` (both
  2026-09-18). The Rust SDK major version "2.x" is **not** protocol v2.
- The v1 stable/unstable split matches §1. `session/set_model` was removed as "never-stabilized" on 2026-06-01
  (`docs/rfds/updates.mdx:251-256`).
- **Completed RFDs**: session list, resume, close and delete; logout; terminal auth (2026-08-20); agent registry.
  The **`env_var` auth method was removed** because it "does not generalize to remote transports".
- **Draft RFDs**: session fork, providers, MCP-over-ACP, proxy chains, `auth/status`.
- **The v2 overview RFD is Active.**
  - `session/prompt` resolves on *insertion* with `{messageId}`, and turn state moves to
    `state_update{running|idle(stopReason)|requires_action}`.
  - Messages are upserted by ID.
  - `session/load` and `set_mode` are dropped.
  - MCP configs get a `type` tag, `session.mcp.stdio` becomes an explicit capability, and MCP-over-SSE is dropped.
  - **Client `fs/*`/`terminal/*` are removed** because they have "not been widely adopted".
    Replacement: "clients … can already expose a special MCP server to the agent"
    (`docs/rfds/v2/client-filesystem-terminal-capabilities.mdx:11-66`).
- **Remote transport**: the RFD `streamable-http-websocket-transport` is Active and additive to v1.
  - One `/acp` endpoint for POST, SSE GET streams per connection and per session, and a WebSocket upgrade.
  - HTTP/2 is required. There is no replay in v1.
  - Terminal auth must not be advertised over a remote transport.
  - Rust implementation: **`agent-client-protocol-http` 2.2.0**, with an axum `AcpHttpServer` (HTTP+SSE+WS) and a
    reqwest/tungstenite `HttpClient`.

**Registry** (`cdn.agentclientprotocol.com/registry/v1/latest/registry.json`, 41 agents, bumped hourly), notable
launch commands: `claude-acp` 0.81.2, `codex-acp` 1.13.1 (`npx @agentclientprotocol/…`), `gemini --acp`,
`copilot --acp`, `goose acp`, `opencode acp`, `kimi acp`, `cursor-agent acp`, `devin acp`, `pi-acp`, qwen-code,
cline, factory droid, grok.

**Who routes file or shell work through the ACP client** (this decides SSH shadowing per agent):

| agent | fs via client | shell via client | note |
|---|---|---|---|
| claude-agent-acp 0.81.2 | no | no | built-ins only; tool replacement via `_meta` (§2.7) |
| codex-acp 1.13.1 (TypeScript now; the Rust `zed-industries/codex-acp` repo is archived) | no | no (display-only `_meta.terminal_output`) | auth methods: `api-key`, `chat-gpt` (browser), `chat-gpt-device-code` (needs URL elicitation), `gateway`; MCP `http` only (`src/CodexAuthMethod.ts:17-80`, `src/CodexAcpServer.ts:343-398`) |
| gemini-cli 0.61.0 | partly: read/write/edit inside the session root | no | `packages/cli/src/acp/acpFileSystemService.ts:26-87`; qwen-code has the same pattern |
| goose 1.52.0 | yes | **yes** (`terminal/*`) | `crates/goose/src/acp/fs.rs:10-58` (Read/WriteTextFile + CreateTerminal… requests; `gh api`, main) |
| kimi-cli 1.52.0 | yes | **yes** | `src/kimi_cli/acp/kaos.py:165-229`; also has an SSH backend |
| mistral-vibe 2.25.8 | yes | yes | `vibe/acp/tool_io.py:21-116` (which tools consume them is `UNVERIFIED`) |
| opencode, pi-acp | no | no | pi-acp does not wire client MCP servers (README) |

**tny** (user's own, C11):
- **It exposes no ACP agent today.** `tny acp` prints "ACP server was removed" (`src/main.c:164-165`).
- History (`gh api`): an ACP server and client shipped in v0.1.0 (2026-08-19), with WebSocket added 08-22.
  **Both were deleted 2026-09-19** in the "simplification" ADR 0152 (tny owns its loop). **Only the client was
  restored** 2026-09-21 (ADR 0164, "not an ACP server"), without WebSocket.
- **Current client** (v1 only):
  - Advertises `fs:false, terminal:false`. Any agent request other than `request_permission` gets
    method-not-found (`src/backends/acp/acp_client.c:394-446`, `acp_events.c:297-333`).
  - Injects the stdio relay `tny --acp-mcp-bridge <0600 socket>`, and the owning runtime answers MCP itself
    (`initialize` → `2024-11-05`) (`acp_bridge.c:344-411, 495-513, 681-759`).
  - For verified Claude it sets `mode=bypassPermissions` (`:231-237`).
  - **`--ssh` requires `agentInfo.version == "0.75.1"`** (`:416-431`), a pin that is now stale.

### 4. MCP — latest spec is **2026-07-28**, a stateless clean break

Released versions: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25, **2026-07-28** (Current). The draft is
unchanged since release apart from editorial commits. The RC blog calls the release "a clean break"
([versioning](https://modelcontextprotocol.io/specification/versioning),
[changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog),
[blog](https://blog.modelcontextprotocol.io/posts/2026-07-28/)). tny already speaks both eras: it tries
`server/discover` and falls back to `initialize` (`refs/tny/docs/adr/0051-mcp-streamable-http.md`).

- **Removed**:
  - `initialize`/`initialized`, `Mcp-Session-Id` and sessions ("an open connection, such as a STDIO process, is
    not a conversation or session").
  - `ping`, `logging/setLevel`, and `resources/subscribe`.
  - The HTTP GET stream and SSE resume. A broken stream loses the request, which is then re-issued with a new ID.
  - **All server→client requests**. On stdio "the server MUST NOT write requests".
- **Added**:
  - Per-request `_meta` keys `io.modelcontextprotocol/{protocolVersion,clientCapabilities,clientInfo}`.
  - `server/discover`, which servers MUST implement. A version mismatch returns `-32022 {supported, requested}`.
  - **MRTR**: a result of `resultType:"input_required"` with `inputRequests{k: elicitation|sampling|roots}` and
    an integrity-protected `requestState`. The client re-calls with `inputResponses`.
  - `subscriptions/listen`: the only long-lived stream. Its filter is fixed (`toolsListChanged`,
    `resourcesListChanged`, `promptsListChanged`, `resourceSubscriptions`) plus fields that extensions add.
  - `resultType` on every result, and `ttlMs`/`cacheScope` on lists.
  - `capabilities.extensions{"<rev-dns>/<name>": settings}` (SEP-2133). Prefixes whose 2nd label is
    `modelcontextprotocol` or `mcp` are reserved.
  - OpenTelemetry `traceparent` in `_meta`.
  - Full JSON Schema 2020-12 in the tool schemas.
- **Deprecated** (removable ≥ 2027-07-28): Sampling, Roots, Logging, DCR, and 2024-11-05 HTTP+SSE.
- **Methods**: `server/discover`, `tools/list|call`, `resources/list|templates/list|read`, `prompts/list|get`,
  `completion/complete`, `subscriptions/listen`, and client `notifications/cancelled`. Server notifications:
  `progress`, `message`, `cancelled`, `resources/updated`, `*/list_changed`, `subscriptions/acknowledged`.
- **Tools**:
  - `Tool{name, title?, description?, icons?, inputSchema, outputSchema?, annotations{readOnlyHint=false,
    destructiveHint=true, idempotentHint=false, openWorldHint=true}, _meta?}`. The list MUST NOT vary per
    connection.
  - `tools/call` → `{content[text|image|audio|resource_link|resource], structuredContent?, isError?}` or
    `input_required`. Execution and validation failures come back as `isError:true`.
- **Progress and cancellation**:
  - `notifications/progress{progressToken, progress↑, total?, message?}`, only on the originating request's stream.
  - `notifications/cancelled{requestId}` on stdio. **On HTTP, closing the SSE stream is the cancel**, which is the
    opposite of 2025-11-25 SEP-1699.
- **Tasks**:
  - 2025-11-25: core and experimental. `params.task{ttl}`, then `tasks/get|result|list|cancel` and
    `notifications/tasks/status`. States: `working|input_required|completed|failed|cancelled`.
  - 2026-07-28: moved to the **extension** `io.modelcontextprotocol/tasks` (SEP-2663). The server may answer
    `tools/call` with `{resultType:"task", taskId, status, ttlMs, pollIntervalMs}`, followed by
    `tasks/get|update|cancel` and `notifications/tasks` via `listen{taskIds}`. `tasks/list`, `tasks/result`
    and progress-on-tasks are gone.
  - The client-support matrix has **no Tasks column** ([matrix](https://modelcontextprotocol.io/extensions/client-matrix)).
- **Transports**:
  - stdio: NDJSON, also recommended for unix sockets and TCP.
  - Streamable HTTP: POST-only; each response is JSON or a request-scoped SSE stream. Headers
    `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`; check `Origin`.
  - HTTP auth: OAuth 2.1 + PKCE S256, RFC 9728 resource metadata, RFC 8707 `resource`, CIMD preferred over the
    deprecated DCR ([authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)).
    stdio takes its credentials from the env.
- **Streaming partial tool output: not in the spec.**
  - SEP-2998 `notifications/tools/partial_result` was closed 2026-09-22.
  - SEP-2679 task partials: closed. SEP-2532 `resources/stream`: open draft with no sponsor.
  - The roadmap lists "results that stream" and a `tools/call` result redesign, with no SEP
    ([roadmap](https://modelcontextprotocol.io/development/roadmap)).
- **Webhooks and events**: a Working Group is at the "ideating" stage.
- **Official extensions**: `ui` (MCP Apps), `tasks`, `skills`, `oauth-client-credentials`,
  `enterprise-managed-authorization`.

### 5. `rmcp` (official Rust MCP SDK)

**Release and maturity**: **3.4.1** (2026-09-23). Majors: 1.0 (2026-03), 2.0 (06-29), 3.0 (07-28), then 11 releases
since. 29M downloads, MSRV 1.88, Apache-2.0, 18 open issues. codex pins `rmcp = "=3.2.0"`
(`refs/codex/codex-rs/Cargo.toml:443`). Paths below are in `scratchpad/r3/crates/rmcp-3.4.1/`.

**Runtime**:
- **tokio is a hard dependency.** Handlers are `Send + Sync` by default. The `local` feature relaxes that and uses
  `spawn_local`, but it is only compile-tested (`src/service.rs:20-38, 1328-1342`).

**Default features and transports**:
- Defaults are `base64`, `macros`, `server`. `client` and `transport-io` are opt-in (`Cargo.toml.orig:125-201`).
- **Available**: `stdio()`; `TokioChildProcess` (client); any `AsyncRead + AsyncWrite` (unix sockets, ssh
  pipes) or Sink/Stream; streamable HTTP client (reqwest or unix-socket hyper); and a streamable HTTP server,
  `StreamableHttpService`. The server is a tower `Service` with a 4 MiB body cap, loopback-only `allowed_hosts`,
  local or no session manager, and legacy-era `Last-Event-Id` replay.
- **Not available**: WebSocket (`ws.rs`: "Maybe we don't really need a ws implementation?"), legacy HTTP+SSE
  (removed in 0.11), and **server-side auth** (use axum middleware).
- Framing is NDJSON **with no line-length cap** (issue #1030, `src/transport/async_rw.rs:221-227`).

**Protocol versions**:
- Constants cover all 5 versions, but `LATEST = V_2025_11_25` (`src/model.rs:170-187`).
- A server accepts every era: the `initialize` flow never agrees to 2026-07-28, and 2026-07-28 arrives through
  `server/discover` (`service/server.rs:466-479`).
- A client uses `initialize` unless it opts into `ClientLifecycleMode::Discover|Auto` (`service/client.rs:636-657`).
- It implements MRTR, `subscriptions/listen`, the SEP-2663 tasks extension (`TaskManager`; `tasks/get|update|cancel`)
  and HMAC `requestState`.

**API shape**:
- Macros: `#[tool]`, `#[tool_router(server_handler)]`, `#[tool_handler]`, `#[prompt*]`. Output schemas are
  inferred from `Json<T>` returns.
- Tools can be added and removed at runtime (`ToolRouter`), `ServiceExt::serve(transport)` starts a service, and
  `Peer<RoleServer>` sends notifications.
- **Cancellation is cooperative**: handlers get a `CancellationToken` and are never aborted
  (`service.rs:1591-1624`).
- JSON is `serde_json::Value` everywhere, with untagged unions and no `RawValue`. There are no benchmarks.

**Auth and bugs**:
- Client auth (`auth` feature): PKCE, CIMD → DCR, RFC 9728 discovery, refresh, client-credentials JWT, and
  enterprise token exchange.
- Open bugs include #1272 (P0, duplicate SEP-2243 headers) and #1283.

**`agent-client-protocol-rmcp` 3.1.1 depends on `rmcp = "2.1.0"`** (`Cargo.toml:59-64`), so it cannot share types
with rmcp 3.x. Avoid it, or wait for a bump. `agent-client-protocol-tokio` 0.11.1 is obsolete, because core 2.2.0
now ships `AcpAgent`/`Stdio`.

**Comparison crates**: `jsonrpsee` 0.26.0 (2025-08), `tonic`/`tonic-prost` 0.14.6 (2026-05; repo moved to
`grpc/grpc-rust`), `prost` 0.14.4, `schemars` 1.2.2 (2026-07), `tarpc` 0.38.0 (about one release a year).

### 6. MCP as aim's harness ↔ agent protocol — fit matrix (2026-07-28 semantics)

| need | fits? | how, or the gap |
|---|---|---|
| (a) streaming tool output | **no** | Only `progress.message` strings. Workaround: `resource_link` + `subscriptions/listen` + `resources/read`. Needs `dev.aim/*` notifications tied to the request, or a listen-filter extension |
| (b) PTY / interactive | **no** | No byte streams and no server→client requests. Needs server-minted handles (the spec's own pattern for cross-call state) + an extension for write/resize/output |
| (c) cancellation | yes | `notifications/cancelled` (stdio); stream close (HTTP); `tasks/cancel` (cooperative) |
| (d) file watching | partial | `resourceSubscriptions` → `resources/updated{uri}`: no event kinds, no batching, no rename/delete semantics |
| (e) many concurrent clients | yes, by design | Stateless requests; HTTP scales horizontally. stdio is 1:1 per process |
| (f) blackboard submit → assign → notify → poll | partial | Tasks extension (get/update/cancel + listen pushes) models the async part, but client support is unknown. No webhooks, and assignment is application logic |
| (g) versioning / capabilities | yes, but churny | Per-request version, `-32022` negotiation, `extensions` map with fallback rules. Four breaking revisions in 20 months, the last a clean break |
| (h) per-call overhead | fine | One JSON-RPC exchange, plus required `_meta` version/capabilities on every request in 2026-07-28. Measured NDJSON round trip over a unix socket: 11 µs (256 B), 18 µs (4 KB), 212 µs (64 KB) (§Impl.) |
| server asks the client mid-call (permission, elicitation) | MRTR only | The harness can't originate requests. Policy prompts must live in the agent/daemon, not the harness |

Aim-specific gaps that need extensions (reverse-DNS prefix such as `dev.aim/`; the `io.modelcontextprotocol`
and `mcp` second labels are reserved): exec/PTY handles with seq-numbered output, a watch stream, binary/bulk
reads (the spec only has base64 `blob`), per-call timings and usage in `_meta`, and webhook registration for jobs.

### 7. Internal-protocol alternatives

| option | strengths | weaknesses | verdict (opinion) |
|---|---|---|---|
| MCP-native surface (+ `dev.aim/*` ext) | Day-one interop; rmcp; ecosystem tooling | Gaps (a)(b)(d) need extensions anyway. Stateless model: no server→client requests, no sessions. Clean-break churn, and the roadmap flags `tools/call` result redesign and "HTTP over stdio" | **Façade**, not the core |
| Custom JSON-RPC 2.0 over NDJSON/WS (hand-rolled codec or the ACP SDK engine; `jsonrpsee` is unnecessary) | Exact semantics: handles, seq streams, backpressure, resume. Aim owns versioning. Same envelope as ACP/MCP, so one tracing/replay stack | Must build our own conformance tests. Interop only through the façade | **Canonical harness wire** |
| gRPC (tonic, bidi streams) | Flow control, protobuf speed, codegen | `.proto` becomes a second source of truth next to the Rust/Verus types. No stdio/ssh-pipe story without custom plumbing; browsers need grpc-web; no ACP/MCP peers speak it | Later, only as a transport if a consumer demands it |
| One schema, many transports | Rust types → serde + schemars → JSON Schema (MCP schemas, docs, web TS); codex does this (`refs/codex/codex-rs/app-server-protocol/src/export.rs:27,123,202`) | Needs discipline: no hand-written JSON | **Adopt as the method** for all of the above |

**Transports** (opinion):
- NDJSON over stdio, unix sockets, and `ssh -T host aim-harness serve --stdio`. OpenSSH can also forward unix
  sockets (`-L local.sock:remote.sock`).
- WebSocket for remote and browser. The Rust ACP SDK has `-http` (WS/SSE) and codex's exec-server defaults to
  `ws://` (`refs/codex/codex-rs/exec-server/README.md`).
- MCP streamable HTTP only on the façade.
- Precedent: codex already split execution out as a custom JSON-RPC **exec-server**
  (`refs/codex/codex-rs/exec-server-protocol/src/protocol.rs:22-57`, `exec-server/README.md`). It has
  `process/start|read|write|signal|terminate` plus `process/output|exited|closed` notifications with `seq`,
  `fs/*`, `http/request` + `bodyDelta`, a WS transport, and a Noise relay with `seq/ack/resume` frames.

## Implications for aim (opinion — recommendations, not facts)

### Recommended architecture: aim-owned core protocols, standard protocols at the edges

```
 UI clients (TUI · web · CLI)   Zed/JetBrains (later)   external agents (codex CLI, Cursor, any MCP client)
          │ aim-daemon/1            │ `aim acp` façade            │ MCP (stdio | streamable HTTP)
          │ (JSON-RPC: unix|WS)     │ (ACP v1, Rust SDK)          │
          ▼                         ▼                             │
 ┌──────────────── aimd (agent daemon, SQLite) ───────────────────┐       │
 │ session engine; agents behind one trait:                        │       │
 │  • aim-native loop (codex Responses / openai-compat)  in-proc   │       │
 │  • ACP client → claude-agent-acp, codex-acp, gemini … (pinned)  │       │
 │ MCP client → user MCP servers (rmcp)                            │       │
 │ blackboard (jobs/webhooks), memory, session search              │       │
 └───────────────┬─────────────────────────────────────────────────┘       │
                 │ aim-harness/1 (JSON-RPC: handles, seq streams, pty)     │ MCP façade (both eras)
                 ▼                                                         ▼
      aim-harness  (library + binary: `serve --stdio | --unix P | --ws URL`, `mcp --stdio | --http`)
        local  |  remote via `ssh -T host aim-harness serve --stdio`  |  agentless SSH backend
```

1. **Execution layer (aim-harness)**: a Rust library with a typed `Harness` API, plus **two wire bindings in the
   same binary**. Both are thin.
   - **(i) Canonical `aim-harness/1`**: JSON-RPC 2.0, NDJSON or WS, `aim-proto` types, one `initialize` per
     connection with an integer version and a feature map. It has:
     - explicit handles (process, pty, watch) plus a resumable connection token;
     - **seq-numbered output** pushed as notifications *and* pullable with `afterSeq`/`waitMs`;
     - cancellation;
     - bulk/binary reads (length-prefixed side frames or base64 at first).
     - This copies codex's exec-server (`process/output {processId, seq, stream, chunk}`,
       `process/read {afterSeq, maxBytes, waitMs}`;
       `refs/codex/codex-rs/exec-server-protocol/src/protocol.rs:24-31, 444-471, 1113-1118`), which survives
       SSH/WS reconnects without replay logic in the agent.
   - **(ii) MCP façade**: a projection of the tool registry serving **both eras**: `initialize` (2025-06-18 and
     2025-11-25, which is what shipping clients such as Claude Code speak today) and 2026-07-28
     `server/discover`. It maps coarse live output onto `progress.message`, long jobs onto the tasks extension
     *and* plain submit/poll tools, and exposes `dev.aim/*` extensions to aim-aware clients.
   - **Why not MCP as the core** (§6/§7):
     - 2026-07-28 removed sessions and server→client requests, and still has no streaming output.
     - It was a clean break 8 months after 2025-11-25, and the roadmap already flags a `tools/call` redesign.
     - A façade confines that churn to one crate.
   - **The day-one interop goal still holds**: `ssh host aim-harness mcp --stdio` is the SSH-shadow endpoint for
     *any* MCP-capable agent.
   - **Verus** targets the pure core: path confinement to roots, exact-substring edit application, seq/ack
     monotonicity and resume, and handle/cancellation state machines. It does not target the codecs.
   - **Tradeoff**: two bindings to keep in conformance (mitigated because both are generated from one registry
     and one type crate), and aim-native clients get richer semantics than MCP clients.
2. **aim-agent ↔ harness**: the same connection type whether the harness is local or remote.
   - Remote default: harness over SSH stdio, auto-bootstrapping a static musl binary into `~/.aim/bin/<ver>`
     (the VS Code/Zed remote-server pattern).
   - Fallback: tny ADR 0022's agentless POSIX-over-ControlMaster mode (~10–30 ms/call), for hosts that forbid
     uploads.
   - Measured JSON-RPC round trip over a unix socket: **11/18/212 µs** at 256 B/4 KB/64 KB (tokio current-thread,
     `serde_json::Value`, M3 Ultra, 20k calls; `scratchpad/r3/rpcbench`).
3. **UI ↔ daemon**: an aim-native **`aim-daemon/1`** protocol (aim-proto types; JSON-RPC over unix socket or
   stdio locally, WS remotely).
   - **Shape its event model on ACP v2**, so that an ACP projection is mechanical: message upserts by
     `messageId`, `state_update{running|idle(stopReason)|requires_action}`, tool-call upserts, config options with
     categories `model`/`thought_level`/`mode` (Jev dynamic effort surfaces here), and `usage_update`.
   - Add what ACP lacks: multi-client attach with fan-out, ephemeral/private sessions, a daemon-wide session index
     and semantic search, the blackboard, plugin/WASM UI events, file/skill autocompletion, codex login, and
     dictation.
   - **Why not ACP itself**: tny deleted its ACP server and kept only the client (ADR 0152 → 0164). ACP is also
     mid-v2 churn, and most of aim's UI surface would be `_aim/*` anyway.
   - Tradeoff: editors don't get aim for free, only through the façade in (5).
4. **aim as ACP client (Claude Code etc.)**:
   - Use `agent-client-protocol` 2.2.0 with stable v1 only. Enable unstable features one at a time
     (`unstable_session_fork`, `unstable_end_turn_token_usage`, `unstable_llm_providers`) behind aim flags.
     Use `acp:NAME` profiles (tny ADR 0029).
   - For Claude, run **tools-authority mode** by default when aim policy or SSH matters:
     `_meta.claudeCode.options = {tools: [<non-fs built-ins aim wants to keep>], toolAliases: {Bash, Read,
     Edit, Write, Glob, Grep → mcp__aim__*}, strictMcpConfig: true, settingSources: [] (or ["project"] when
     local), allowedTools: ["mcp__aim"], maxTurns}`, plus a stdio MCP relay to aimd. The relay is aim's
     equivalent of `tny --acp-mcp-bridge` (ADR 0164).
   - Mirror Claude's built-in input schemas (`sdk-tools.d.ts`) in the aliased tools so skills and prompts that
     name `Bash`/`Read` keep working.
   - Set `mode` to `bypassPermissions` via config option so aim's policy is the only authority (tny does this,
     `acp_client.c:231-237`).
   - Pin the adapter in `mise.toml`. Gate the mode on a conformance probe (tools list = aim tools only, write
     lands remotely) at every adapter bump. tny's exact-version gate (`== "0.75.1"`) already broke on 0.81.2.
     Prefer a capability probe over a version string.
   - **Other agents**:
     - Serve v1 `fs` + `terminal` from the harness. That routes goose, kimi and mistral-vibe fully, and
       gemini/qwen for files only. v2 removes these, so treat the route as legacy; MCP injection stays primary.
     - Agents that can't give up their built-ins (codex-acp has no switch) run on the remote host
       (`ssh host <agent> acp`), or are refused under `--ssh`.
   - **Login**: advertise `auth.terminal`, then spawn the method's command (use `_meta["terminal-auth"]`
     `command`/`args` verbatim) in an aim PTY pane. Treat `authRequired` as the "log in" trigger.
5. **aim as ACP agent**: an optional edge crate, `aim acp`.
   - Uses the Rust SDK's `Agent` role over `Stdio` (`examples/simple_agent.rs`); for remote, add
     `agent-client-protocol-http`.
   - Projects `aim-daemon/1` sessions: `loadSession`/`resume`/`list`/`close`, config options, `usage_update`.
   - Never depend on client `fs`/`terminal`: v2 deletes them, and tools stay in the harness.
   - Build it when an editor integration is actually wanted (see *Open questions*). Until then, keep `aim-daemon/1`
     ACP-projectable by construction.
6. **aim as MCP server**:
   - One MCP façade, `aim mcp` (stdio) plus `/mcp` streamable HTTP on aimd.
   - Contents: harness tools, plus aim services: memory, semantic session search, codex-backend web search/image
     gen/dictation, and saved code-mode programs.
   - Blackboard: plain `board_submit` / `board_get` / `board_wait` tools, which every client supports. Add the
     `io.modelcontextprotocol/tasks` extension only for clients that declare it. Webhooks are an aim feature,
     because MCP has none (§4).
   - Auth: bearer or OAuth on HTTP (§4); 0600 socket/dir permissions locally (tny ADR 0164 model).
7. **aim as MCP client**:
   - Use `rmcp` 3.4.x, pinned exactly as codex does. Enable `client`, `transport-child-process` and
     `transport-streamable-http-client-reqwest`, `auth`, and `ClientLifecycleMode::Auto` so both eras work.
   - Put a length-capped framing wrapper in front of stdio servers (rmcp #1030).
   - Follow tny's warm-up and import patterns (ADRs 0049/0052/0068).
   - Imported tools go through the same policy path as harness tools.
   - For the MCP façade (6): rmcp's server, plus axum middleware for bearer/OAuth, because rmcp has no
     server-side auth.

### Cross-cutting rules

- **One schema, many transports.** A crate (`aim-proto`) holds serde + schemars Rust types for every `aim-daemon/1`,
  `aim-harness/1` and `dev.aim/*` payload. It generates JSON Schema (MCP `inputSchema`/`outputSchema`, docs, and TS for the web UI;
  codex does the same with schemars + ts-rs, `refs/codex/codex-rs/app-server-protocol/src/export.rs:27, 123, 202`).
  Framing: NDJSON on byte streams (stdio, unix, ssh) and WebSocket text frames remotely. **No gRPC in the MVP**:
  it adds a second source of truth (`.proto`), no stdio/ssh story, and no ecosystem peers. If needed later it
  is a transport, not a protocol.
- **Three engines, clean ownership**:
  - `aim-daemon/1` and `aim-harness/1` share one small hand-rolled JSON-RPC peer. It needs request-id
    correlation, cancellation, seq streams and resume tokens; it is Verus-friendly, and nothing here justifies
    `jsonrpsee`.
  - ACP edges use `agent-client-protocol`. Its runtime-agnostic, single-task-per-connection design fits a tokio
    daemon, provided handlers never block.
  - MCP edges use `rmcp` (§5).
- **Tolerant decoding on the wire**: `#[serde(default)]` and ACP-style `DefaultOnError` for optional
  capabilities (schema uses `serde_as(deserialize_as = "DefaultOnError")` throughout). Keep
  `deny_unknown_fields` for config files only.
- **Every tool call from every agent crosses the harness**. That gives one policy, hook, audit and SSH seam. An
  agent that cannot surrender its built-ins (generic ACP agents) either runs entirely on the remote host
  (`ssh host <agent-acp>` as the ACP command) or is refused for `--ssh`, as tny does.

## Open questions for the user

1. **Claude subscription via a third-party client.** Should aim offer the `claude-ai-login` (subscription) method,
   or Console/API-key billing only? The adapter allows both, while JetBrains launches it with `--hide-claude-auth`.
2. **Remote hosts under `--ssh`.** May aim upload and run a static `aim-harness` binary on the remote (full
   features: PTY, watch, fast search)? Or must SSH mode stay agentless, as in tny ADR 0022?
3. **Claude over SSH.** Is it acceptable to replace Claude Code's built-in tools with aim's aliased MCP tools
   (aim is the policy authority; some Claude-tuned behaviour is lost)? The alternative is running
   `claude-agent-acp` itself on the remote (needs node, the adapter, and Claude credentials there).
4. **Editors.** Is an ACP-agent surface for Zed/JetBrains wanted for the MVP? tny removed its ACP server.

<!-- REPORT COMPLETE -->
