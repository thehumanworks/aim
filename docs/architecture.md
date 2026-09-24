# aim architecture

> Status: v0.1 (2026-09-25). Source of truth for how aim is shaped. Decisions are recorded in
> [`adr/`](adr/); evidence lives in [`research/`](research/). When this document and an accepted
> ADR disagree, the ADR wins and this document is wrong — fix it.

aim ("Agent I am") is a harness for all agents, built and updated by the agents that use it
([vision](vision.md)). This document describes the shape that makes that possible: an execution
layer and an agent layer that are separate applications, a verified functional core, protocols
defined once as Rust types, and every agent-mutable thing expressed as versioned data.

---

## 1. Principles

1. **Functional core, imperative shell.** Every *decision* — state transitions, policies,
   planners, budgets, protocol negotiation — is a pure function in a Verus-verified kernel. I/O
   code (network, disk, processes, UI) is a thin shell that asks the kernel what to do and does it.
   Proofs are preferred over example tests; the proofs are the documentation of locked decisions.
2. **One authority per concern.** One tool dispatcher routes every tool call from every source
   (model, code mode, plugin, program, hook). One session log per session. One job ledger for the
   swarm. One permission engine. One evolution gate. Everything else is a projection.
3. **One schema, many transports.** Every wire message is a Rust type in `aim-proto` (serde +
   schemars). JSON Schema, MCP tool schemas, the web UI's types and the docs are generated from it.
   Transports are interchangeable: in-process, stdio, unix socket, SSH, WebSocket, HTTP.
4. **aim-owned core protocols, standard protocols at the edges.** `aim-harness/1` and
   `aim-daemon/1` are aim's own. MCP (server and client), ACP (client, later agent) and A2A
   (federation) are façades that project the core — churn in a standard is confined to one crate.
5. **Capabilities are data.** Models, reasoning-effort ladders, service tiers, tools, workspace
   capabilities and provider quirks come from catalogs and manifests, never hardcoded enums
   (the live catalog already exposes `low…ultra` efforts that codex's own UI does not —
   [live probes](research/live-probes.md)).
6. **Everything an agent may change is versioned data.** Instructions, prompts, skills, agent
   definitions, tool descriptions, policies, themes, UI surfaces, workflows, programs, plugins —
   and aim's own source. Changes are hot-reloaded, recorded in a ledger and reversible.
7. **Evidence over assertion.** Every integration ships live smoke tests (required — they run for
   real). Every performance claim has a benchmark. Every locked decision has an ADR, and a proof
   when it is logic.

---

## 2. Applications and processes

aim is two applications, each usable without the other:

| App | Binary | Role | Runs where |
| --- | --- | --- | --- |
| **Execution layer** | `aimx` | Workspaces (fs, exec, PTY, search, watch), built-in tools, tool-level policy, MCP façade. Owns no conversations and no LLM credentials. | Locally, or on any remote host (static musl build, auto-bootstrapped over SSH) |
| **Agent layer** | `aim` | Agent daemon (`aim daemon`), CLI, TUI, web UI server, MCP/ACP edges. Owns sessions, agents, providers, credentials, memory, swarm, plugins, evolution. | The user's machine (or any server they choose) |

```
  TUI · CLI · web (Leptos) · editors(later)        external agents (codex CLI, Claude, any MCP client)
          │ aim-daemon/1 (unix | WS)                          │ MCP (stdio | streamable HTTP)
          ▼                                                   │
 ┌─────────────────────────── aim daemon (agent layer) ──────┼────────────────────────────┐
 │ sessions (event-sourced, SQLite) · agent loops · providers (codex, openai-compat)       │
 │ ACP client → claude-agent-acp …   · MCP client → user MCP servers                       │
 │ ONE tool dispatcher ─ policy · hooks · audit · provenance                               │
 │ context engine & compaction · Jev decisions · blackboard ledger · memory · search       │
 │ plugin host (wasmtime) · code-mode workers · program library · evolution controller     │
 │ media/search services (codex transcribe · images · web search)   · MCP façade `aim mcp` │
 └────────────────────────────┬────────────────────────────────────────────────────────────┘
                              │ aim-harness/1 (JSON-RPC; unix | stdio | SSH | WS)
                              ▼
 ┌────────────── aimx (execution layer) ──────────────┐     `aimx mcp` façade (harness tools only)
 │ tool registry · tool policy · process/PTY handles   │
 │ Workspace backends:                                 │
 │   local │ ssh→remote `aimx proxy` │ ssh agentless    │
 │   (later: container, cloud bucket via OpenDAL)       │
 └─────────────────────────────────────────────────────┘
```

**Local topology.** `aim` (TUI/CLI) auto-spawns `aim daemon` (one per user, unix socket, 0700
directory), which auto-spawns a local `aimx serve` (unix socket). The TUI can exit and reattach;
sessions keep running in the daemon.

**Ephemeral.** `aim --ephemeral` runs the agent loop and the harness *in process*, with an
in-memory store, and never touches the daemon, the database, history files or indexes
([§5.4](#54-ephemeral-and-private-sessions)).

**SSH.** `aim --ssh host` (or `/ssh host`) keeps inference, credentials, sessions and UI local and
binds the session's workspace to the remote host: the local `aimx` connects to a remote
`aimx proxy` over one OpenSSH ControlMaster channel, uploading a sha256-verified static binary on
first contact; when that is impossible, it falls back to agentless mode (sftp + POSIX `sh` over
the same master). Every workspace tool call — read, write, edit, shell, PTY, search, watch, git —
executes on the remote. Details in [§9](#9-ssh-shadowing).

**Standalone harness.** `aimx serve --unix PATH | --stdio | --ws ADDR` exposes the harness to any
client; `aimx mcp --stdio | --http ADDR` exposes its tools to any MCP agent (so
`ssh host aimx mcp --stdio` shadows *any* MCP-capable agent's tool calls onto that host).

---

## 3. Crate map

Crates are small and single-purpose; dependencies point downward only. `(V)` marks Verus-verified
crates.

| Layer | Crate | Responsibility |
| --- | --- | --- |
| kernel | `aim-kernel` (V) | Pure decision logic with Verus specs and proofs: seq/ack streams, path confinement, exact edits, turn FSM, context plans, job ledger, grants/policy, effort controller, evolution gate, version negotiation, backoff. No I/O, no async, no allocation-heavy types. |
| contract | `aim-proto` | Every wire type: `aim-harness/1`, `aim-daemon/1`, events, UI surfaces, errors, schema export. |
| contract | `aim-rpc` | Hand-rolled JSON-RPC 2.0 peer: framing (NDJSON / length-prefixed), request correlation, `$/cancel`, seq-numbered streams, resume tokens, transports. |
| exec | `aim-workspace` | `Workspace`/`Fs`/`Exec`/`Search`/`Watch` traits, `Caps`, `WsPath`. |
| exec | `aim-workspace-local` | The **only** crate allowed `std::fs` / `std::process` / `tokio::fs` / `tokio::process` (clippy `disallowed-methods` elsewhere). |
| exec | `aim-workspace-ssh` | OpenSSH connection manager, askpass bridge, bootstrap, agentless emulation. |
| exec | `aim-tools` | Built-in harness tools written once against `Workspace`. |
| exec | `aim-harness` | Harness server: tool registry & policy, handle tables, `aim-harness/1` service, MCP façade. |
| exec | `aimx` (bin) | Execution-layer binary. |
| agent | `aim-config` | `~/.aim/config.toml`, `.agents/` discovery (skills, agents, prompts, rules, MCP, hooks, instructions), foreign-format import with provenance and trust. |
| agent | `aim-auth` | Credential store, ChatGPT OAuth (browser PKCE + device code), read-only codex import, Claude terminal-auth orchestration, API-key references. |
| agent | `aim-llm` | `ModelProvider` trait, normalized stream events, catalogs, usage accounting. |
| agent | `aim-llm-codex` | ChatGPT codex backend: Responses (SSE, later WS), catalog, V2 compaction, rate limits. |
| agent | `aim-llm-openai` | OpenAI-compatible chat/responses with provider profiles and quirks-as-data. |
| agent | `aim-services` | Media & search services: transcription, image generation, web search. |
| agent | `aim-acp` | ACP client for external agents (claude-agent-acp first); later the `aim acp` agent façade. |
| agent | `aim-mcp` | MCP client (rmcp) for user servers; façade helpers shared with `aim-harness`. |
| agent | `aim-store` | SQLite (WAL, one DB actor): session logs, job ledger, grants, search index, evolution ledger; in-memory store for ephemeral. |
| agent | `aim-agent` | Agent loop, dispatcher, context engine, compaction, subagents, steering. |
| agent | `aim-jev` | Decision pipeline: effort, routing, relevance; Jev + deterministic fallback. |
| agent | `aim-board` | Blackboard service over the job ledger; webhooks; A2A later. |
| agent | `aim-memory` | File-based memory (Markdown canonical, git history, SQLite index). |
| agent | `aim-search` | Conversation search: FTS5 + model2vec vectors (sqlite-vec) + Jev rerank. |
| agent | `aim-plugins` | wasmtime component host for `aim:plugin@0.1`, grants, hot reload. |
| agent | `aim-coderun` | Code-mode runtimes (rquickjs JS/TS first; Monty Python later) in a crash-isolated worker. |
| agent | `aim-programs` | Saved-program library: git repos, manifests, retrieval, GitHub sync. |
| agent | `aim-evolve` | Evolution controller: proposals, protected evaluation, gate, ledger, activation, rollback. |
| agent | `aim-daemon` | Session manager, `aim-daemon/1` service, client fan-out, web server. |
| ui | `aim-tui` | ratatui renderer: inline + fullscreen, composer, completion, surfaces, themes. |
| ui | `aim-web` | Leptos (wasm) web client, served by the daemon; private mode. |
| bin | `aim` | CLI + TUI + `aim daemon` / `aim mcp` / `aim login` … |

Milestone M0–M2 need only: `aim-kernel`, `aim-proto`, `aim-rpc`, `aim-workspace{,-local,-ssh}`,
`aim-tools`, `aim-harness`, `aimx`, `aim-config`, `aim-auth`, `aim-llm{,-codex,-openai}`,
`aim-acp`, `aim-store`, `aim-agent`, `aim-daemon`, `aim`.

---

## 4. Protocols

### 4.1 `aim-harness/1` (agent layer ↔ execution layer)

JSON-RPC 2.0. NDJSON on byte streams (stdio, unix socket, SSH channel), text frames on WebSocket.
One `initialize {proto_gen, min_gen, build, caps, resume?}` per connection; compatibility is by
**protocol generation**, not exact version. Precedent: codex `exec-server`, herdr, Zed
([acp-mcp](research/acp-mcp.md), [infra](research/infra.md)).

Method families (typed in `aim-proto::harness`):

- `workspace.open {root, backend}` → `WorkspaceId` + `Caps {exec, pty, watch, native_search,
  atomic_rename, max_concurrency, os, arch, shell}`.
- `fs.stat | read | read_many | write | edit | list | mkdir | remove | rename | copy`. Writes carry
  a precondition (`Any | IfAbsent | IfHash(h)`) so a model never overwrites a file that changed
  under it. `edit` applies exact-substring edits remotely and atomically.
- `exec.spawn {argv | shell, cwd, env, pty?}` → `ProcId`; output streamed as
  `exec.output {proc, seq, stream, chunk}` notifications **and** pullable with
  `exec.read {proc, after_seq, max_bytes, wait_ms}`; `exec.write_stdin | resize | signal | wait`.
- `search.grep | glob` (streamed, bounded), `watch.start | stop` (+ `watch.event`).
- `tools.list | tools.call` — the high-level tool surface (what MCP projects).
- `session.resume {token, last_seq per stream}` — reconnect after SSH/WS drops without replay
  logic in the agent. Server-side ring buffers are bounded.
- `$/cancel`, `$/progress`.

Seq/ack monotonicity, bounded replay and path confinement are kernel proofs.

### 4.2 `aim-daemon/1` (clients ↔ agent daemon)

JSON-RPC over the unix socket locally, WebSocket remotely (web UI, remote TUI). Its event model is
**shaped like ACP v2** so an ACP projection is mechanical: message upserts by id, tool-call upserts,
`state_update {running | idle(stop_reason) | requires_action}`, config options (`model`,
`effort`, `mode`), `usage_update`. It adds what ACP lacks: multi-client attach with fan-out, a
daemon-wide session index and search, the blackboard, plugin UI surfaces, completion, login flows,
dictation, evolution.

### 4.3 Edges

| Edge | Implementation | Notes |
| --- | --- | --- |
| MCP server | `aimx mcp` (harness tools), `aim mcp` (all aim tools incl. memory, search, board, programs) | Serves both MCP eras: `initialize` (2025-06/11) and 2026-07-28 `server/discover`; long jobs as plain submit/poll tools plus the tasks extension when declared. |
| MCP client | `rmcp` 3.4.x, pinned | User servers; length-capped stdio framing; foreign configs imported per-source opt-in. |
| ACP client | `agent-client-protocol` 2.2 (v1 stable) | Claude Code via `claude-agent-acp` (pinned in mise, gated by a capability probe, not a version string). |
| ACP agent | later, `aim acp` | Projects `aim-daemon/1`; not in the MVP. |
| A2A | later, `aim-board` | Federation of the blackboard with remote aims/foreign agents. |
| gRPC | not in the MVP | If needed, a transport adapter over the same types — never a second schema. |

---

## 5. Sessions: event-sourced state

### 5.1 The log

A session is an append-only log of typed `SessionEvent`s (`aim-proto::event`), each with
`{schema, session_id, seq, turn_id, ts}`: user/assistant messages, reasoning (opaque provider
items preserved byte-exact), tool calls and results, steering, compaction markers, model/effort
changes, usage, errors, UI surfaces, plugin entries. `seq` is strictly monotonic. The durable
transcript is **lossless**; what the model sees is *derived* by the context engine.

Provider-native items (e.g. codex reasoning items with `encrypted_content`, hosted search items)
are stored next to the normalized form so replay is exact.

### 5.2 Forks

A fork is a new session `{parent, fork_seq}` that shares the parent's immutable prefix. Proved:
forks never observe events appended to the parent after `fork_seq`.

### 5.3 Storage

`~/.aim/aim.db` (SQLite WAL, owned by one DB actor thread in the daemon) holds session logs, the
job ledger, grants, the search index and the evolution ledger. Large blobs (tool outputs,
artifacts, images) live content-addressed under `~/.aim/blobs/`.

### 5.4 Ephemeral and private sessions

`--ephemeral` (CLI/TUI) and **private** (web) sessions use `MemoryStore`. This is a *type-level*
guarantee: the ephemeral session constructor only accepts the memory store, the search indexer
and memory writer take a `Persistent` session witness, and the storage router's "ephemeral ⇒
never persisted" property is a kernel proof. Provider no-store flags are sent where defined
(`store:false`). Disclosed caveat: services with server-side retention (codex dictation keeps audio
for 30 days — [live probes](research/live-probes.md)) are marked in the UI.

---

## 6. The agent layer

### 6.1 Agent backends

A session is driven by one `AgentBackend`, which emits normalized events:

- **Native loop** — aim's own loop over a `ModelProvider` (codex backend, OpenAI-compatible, later
  Anthropic API). Every tool call goes through aim's dispatcher.
- **ACP agent** — an external agent (Claude Code via `claude-agent-acp`, later codex-acp, gemini, …)
  with its own loop. aim is the ACP client: renders its updates, answers permission requests,
  sets config options. Tool authority:
  - *native tools* (default locally): the agent's built-ins run in its own process;
  - *aim tools* (forced under `--ssh`, opt-in locally): built-ins disabled via
    `_meta.claudeCode.options {tools, toolAliases → mcp__aim__*, strictMcpConfig, settingSources,
    allowedTools}`, and aim's MCP server injected — schemas mirror Claude Code's own tool shapes
    so skills and prompts keep working. Agents that cannot surrender built-ins are refused under
    `--ssh` rather than silently running locally.

### 6.2 The native loop

```
inbox (idempotent inputs: prompt, steer, tool results, job events, webhooks)
  → kernel turn FSM decides the next effect
  → context engine builds the model view (pure) → provider stream → normalized events
  → complete tool calls → dispatcher (parallel where allowed) → results → loop
  → settle: exactly one terminal outcome per turn
```

Kernel-proved turn invariants: an incomplete tool call is never dispatched; every dispatched call
receives exactly one result (cancellation settles pending calls with a cancelled result); a steer
is either delivered before the next provider request or returned to the queue — never lost
(tny ADR 0011/0013 lesson).

### 6.3 The dispatcher

Every tool call — from the model, a code-mode cell, a plugin, a saved program or a hook — enters
one dispatcher with a `source` (`model | code(cell) | plugin(id) | program(id@sha) | hook(id)`):

1. **policy** (kernel `policy`: grants ∩ agent ceiling ∩ session policy; deny overrides allow;
   protected paths);
2. **pre-hooks** (plugins, imported hooks; tny ADR 0028 fold precedence: cancel/deny beat rewrite);
3. **route**: harness tool → `aimx`; agent tool (subagent, board, memory, search, programs, ui) →
   daemon internals; plugin tool → plugin host; MCP tool → MCP client;
4. **post-hooks**, output shaping (large outputs become handles — the model sees head/tail plus a
   `read_output(handle, range)` affordance), event emission, audit.

This single path is what makes permissions, hooks, SSH shadowing, telemetry and replay uniform.

### 6.4 Context engine and compaction

The context engine is a pure function: `(transcript, budget, policy) → (model input, record of
what was omitted, truncated or compacted)` (unreal-agent's context builder, made verifiable).
Budgets come from the catalog (272k windows today). Prompt-cache discipline: a stable prefix
(instructions + tools) that only changes on explicit reconfiguration; append-only growth between
compactions; `prompt_cache_key` per workspace × tool profile × remote target (tny ADR 0078
measured a 78% cached fraction with this).

Compaction is layered, cheapest first:

1. **Output handles** — oversized tool outputs never enter the context in full.
2. **Relevance elision** — stale tool results are stubbed when a batched Jev `Noul` ("is this item
   still needed for the current goal?") says so; computed off the critical path.
3. **Remote compaction** — codex V2 (`compaction_trigger` → encrypted `compaction` item) when the
   provider supports it.
4. **Structured local summary** — a handoff-style summary (goal, decisions, files, open items,
   facts) produced by a model call when (3) is unavailable.
5. **Retrieval** — nothing is lost: compacted content stays searchable via conversation search
   (`search_sessions`, `read_session`).

Kernel-proved plan invariants: a tool call and its result are never separated; pinned items are
always kept; the plan fits the budget; compaction strictly shrinks; token arithmetic cannot
overflow.

### 6.5 Providers

`ModelProvider::{catalog, stream}` with normalized events and provider-native sidecars.

- **Codex (ChatGPT subscription)** — `https://chatgpt.com/backend-api/codex`: `POST /responses`
  (SSE; WebSocket `responses_websockets=2026-02-06` with `previous_response_id` input elision
  later, behind a benchmark); `GET /models?client_version=…` catalog with ETag; V2 compaction;
  rate-limit headers (`x-codex-primary-*`, `-secondary-*`, credits) surfaced in the status line;
  `x-codex-turn-state` echoed for affinity; `usage.attribution` recorded for token-efficiency work.
  Contract: [codex-backend](research/codex-backend.md) + [live probes](research/live-probes.md).
- **OpenAI-compatible** — named profiles `{base_url, key_env, wire: chat | responses, quirks}`,
  presets for OpenRouter and Vercel AI Gateway. Quirks are data (e.g. AI Gateway's
  `min_output_tokens = 16`), not code branches.

### 6.6 Auth

- **ChatGPT**: aim's own OAuth grant (browser PKCE on `127.0.0.1:1455`, fallback 1457; device code
  flow), stored in the OS keyring with a 0600-file fallback. `~/.codex/auth.json` is a read-only
  fallback source and is **never refreshed by aim** (refresh-token rotation would log the codex CLI
  out). One refresher per daemon (lease) avoids rotation races between sessions.
- **Claude**: `claude-agent-acp` owns Claude credentials. `aim login claude` runs the adapter's
  terminal-auth command (`--cli auth login --claudeai | --console`) in an aim PTY pane; `authRequired`
  from the adapter triggers it.
- **API keys**: referenced by env-var name (never persisted unless the user stores them in the
  keyring).

### 6.7 Jev decisions

`aim-jev` exposes a `Decider` trait with a Jev implementation (`typesafe-jev`, called on
`spawn_blocking`) and a deterministic fallback. Jev answers many calibrated questions about one
state in one ~0.6 s request, for ~$0.04/M input tokens, so aim batches them:

- **Per-step bundle** (one request, issued while tools execute, deadline-bound, applied to the next
  provider request): effort `Score` over the *selected model's catalog ladder*; `Noul`s for
  "stuck/looping", "making progress", "would past sessions help"; relevance of candidate skills.
- **Router**: deterministic capability filter (context size, tools, images, tier) → Jev `Choice`
  among eligible configured models (described by strengths/cost) → sticky per task; switching
  accounts for prompt-cache loss.
- **Relevance**: compaction elision, memory recall, skill/program suggestions, search rerank.

The **effort controller** is a kernel function: bounds from the user/agent definition, catalog
support, hysteresis (no thrashing), explicit user override wins. Every decision is logged with
inputs and outputs so the evolution loop can tune thresholds offline. Private/ephemeral sessions
never send content to Jev.

### 6.8 Memory, skills, agents, instructions

`.agents/` is aim's native project home; `~/.aim/` the user's
([agents-conventions](research/agents-conventions.md)):

```
repo/AGENTS.md                    portable instructions (walked root → cwd, byte-capped)
repo/.agents/instructions.md      aim-specific instructions
repo/.agents/rules/*.md           path-scoped rules
repo/.agents/skills/<n>/SKILL.md  Agent Skills (open spec; also read by codex and pi)
repo/.agents/agents/<n>.md        agent definitions (YAML frontmatter, schema aim.agent/v1)
repo/.agents/prompts/<n>.md       prompt templates / slash commands
repo/.agents/programs/            saved code-mode programs (git)
repo/.agents/plugins/             project plugins (need hash-pinned trust)
repo/.agents/mcp.json, hooks.toml native MCP servers and hooks (explicit trust)
repo/.agents/memory/              optional shared project memory (never auto-committed)
~/.aim/{config.toml, agents/, skills/, prompts/, plugins/, programs/, memory/, mcp.json, hooks.toml}
```

Foreign formats (Claude `.claude/{skills,agents,commands}`, `CLAUDE.md`, Codex TOML agents and
`[mcp_servers]`, pi, oh-my-pi, tny, OpenCode) are **read** as typed descriptors with provenance
`{kind, path, scope, parser_version, trust, hash}`. Discovery never executes anything; foreign MCP
servers and hooks need a per-source opt-in (tny ADR 0052). Skills load metadata-first (catalog
budgeted to a small share of context), full bodies on activation; explicit mentions inject the
body into the user turn so the cached system prefix never changes (tny ADR 0056).

Memory is Markdown (canonical) with a short index loaded at start and topic files retrieved on
demand (exact, semantic and Jev-ranked). A memory can suggest, never grant permissions.

### 6.9 Conversation search

Chunks: user messages, final assistant messages, tool-call *digests* (never raw outputs), and every
compaction summary. Index: FTS5 (BM25) + model2vec `potion-retrieval-32M` vectors in sqlite-vec,
fused with reciprocal-rank fusion, optionally reranked by one Jev request. Tools:
`search_sessions`, `read_session`. Automatic surfacing only when calibrated gates pass (≥ 0.7 that
the request benefits, ≥ 0.8 relevance). Budgets: < 50 ms p50 locally at 100k chunks; +1–2 s with
Jev ([infra](research/infra.md)).

---

## 7. The swarm (blackboard)

The SQLite **job/attempt ledger is the only execution authority**; messages, claims, notifications,
the board view and A2A tasks are projections ([swarm](research/swarm.md), tny ADRs 0143–0162).

- Entities: `Run` (namespace/owner) → `Job` (contract: deliverable, acceptance, requirements,
  dependencies, budget, workspace policy) → `Attempt` (fenced generation, claim token hash, lease,
  heartbeat) → `Message` (typed, bounded mailbox) → `Artifact` (immutable, hashed evidence) →
  `Review` (acceptance is separate from execution success) → `Subscription`/`Outbox`
  (notifications never change job truth).
- API: `post | assign | claim | heartbeat | message | complete | fail | cancel | retry | watch |
  poll | subscribe | review | integrate` — the same service in-process, over the unix socket, and
  over HTTP/WS; polling is the reconciliation read, push is a hint.
- Workers: native sub-sessions (agent definitions), ACP agents, remote aims (A2A, later). Remote
  identities need one-time approval before their first claim.
- Matching: deterministic eligibility and admission first; Jev `Choice` only ranks eligible agents
  when ambiguous. A probability is never a permission.
- Isolation: editing jobs get a git worktree by default; integration is a serialized queue that
  preserves conflicts.
- Notifications: in-process and unix-socket watch streams first; HTTP webhooks with Standard
  Webhooks signing and an outbox next; A2A push later.

Kernel-proved: one current attempt per job generation; stale claim tokens cannot mutate; a job
starts only when its dependencies' evidence matches; terminal execution never implies acceptance;
admission never exceeds capacity; delegation only narrows permissions.

---

## 8. Extensibility

### 8.1 Plugins (WebAssembly components)

wasmtime (LTS line, patched promptly — 28 advisories in 2026), components only, one `Engine` per
daemon, pooling allocator, precompiled `.cwasm` cache (3–5 µs per instance measured). The ABI is the
WIT package `aim:plugin@0.1.0`: a *synchronous* guest world (guest toolchains for WASI p3 are not
stable yet; the host awaits internally on fibers) with interfaces `host`, `kv`, `tools`, `session`,
`ui`, `bus`, `wasi:http` (egress allowlisted) and an exported `plugin`
(`init → registration`, `on-event → list<action>`, `call-tool`, `run-command`, `complete`,
`on-ui-action`, `render`, `shutdown`). Semver matching lets 0.1.x hosts and guests link; records are
frozen per track, growth goes through additive functions and an `ext` action; 0.2 moves to WASI p3
async streams (and model-provider plugins).

Capabilities are granted per plugin hash (`tools.provide`, `tools.call:<glob>`,
`ui.surface:<placement>`, `session.*`, `kv`, `bus:<topic>`, `net.http:<host>` …); ungranted imports
link to deny stubs. Global plugins load by default; project plugins need hash-pinned trust; an
agent can never self-grant beyond its session ceiling. Two authoring paths: compiled components
(Rust SDK first, any component language) and script plugins (TS/JS source run by a prebuilt
QuickJS component — hot-loaded in milliseconds, no toolchain). The pi feature map is in
[extensibility](research/extensibility.md).

### 8.2 UI protocol

Because UIs are separate processes, plugin- and agent-authored UI must be data. aim adopts the
**A2UI v1.0 shape** (surfaces, flat id-keyed components, JSON-Pointer data updates, actions flowing
back) with an aim catalog (`aim/terminal@1`) of terminal-first components (Text, Markdown, Code,
Diff, Row/Column/Box, List/Table/Tree/KeyValue, Progress/Spinner/Badge/Sparkline/Log, Image,
Button/TextField/Select/CheckBox, and a `Cells` escape hatch) and **placements** (`status.*`,
`widget.*`, `panel.side`, `overlay`, `dialog`, `transcript`, `tool(call_id)`, `toast`, `title`).
aim's own built-in UI (status line, tool rows, dialogs) uses the same protocol, so TUI and web stay
at parity by construction and every built-in is replaceable. Unknown components render their
fallback. Themes are DTCG token files with a terminal extension (ANSI 256/16 fallbacks, attributes,
glyph sets). Agents emit surfaces through `ui.show | update | close` tools.

### 8.3 Code mode and programs

A `CodeRuntime` trait, hosted in a crash-isolated `aim-coderun` worker process under the OS
sandbox profile with no network. **JS/TS first** (rquickjs/QuickJS-ng: +1 MB, 0.1 ms cold,
10.7 µs per awaited host call, exact deadlines and memory limits; TypeScript stripped with oxc),
**Python second** (Monty, for Claude-family models). Codex models get codex's own `exec`/`wait`
contract (so catalog `tool_mode` templates work verbatim; aim follows the catalog's `tool_mode`
hint); others get `run_code`. Every `tools.*` call returns to the dispatcher. Typed `.d.ts`/`.pyi`
surfaces are generated from tool schemas; the prompt carries only a compact index plus
`describe`/`search`.

Saved programs live in git repos (`~/.aim/programs`, synced to the user's private GitHub remote;
`.agents/programs` in projects) with a `program.toml` (params/returns schemas, tools used, grant
snapshot, provenance). Retrieval is FTS5 + vectors + one Jev rerank. A program runs with its saved
grants ∩ current policy — never more.

---

## 9. SSH shadowing

- **Transport**: the system OpenSSH binary (full `ssh_config`, agents, ProxyJump, FIDO, known_hosts).
  aim starts its own ControlMaster (`~/.aim/ssh/%C`) with `SSH_ASKPASS` pointing at aim, so
  password/2FA/host-key prompts reach the TUI or web UI even from the headless daemon; host-key
  checking is never weakened.
- **Bootstrap** (default): `uname -sm` probe → pick `aimx-g<gen>-<ver>-<target>` from the local
  cache or release assets → stream it over the master's stdin, verify sha256, `exec … proxy`.
  Targets: `x86_64`/`aarch64-unknown-linux-musl`, `aarch64`/`x86_64-apple-darwin`.
  Per-host `bootstrap = never` forces agentless.
- **Agentless fallback**: sftp + `sh -c` over the master, atomic temp→rename writes, bytes on
  stdin, `rg --json` when present; reduced `Caps` (no watch, PTY via `ssh -tt`).
- **What stays local**: inference, credentials, sessions, memory, MCP, plugins. The remote never
  sees a provider key. Remote `AGENTS.md` is loaded and labelled as remote (tny ADR 0040); the
  model's preamble states the workspace is remote.
- **Enforcement**: tool crates cannot reach `std::fs`/`std::process` (clippy
  `disallowed-methods`), so a tool cannot accidentally run locally (pi's SSH example greps
  locally — the failure this prevents).

---

## 10. Recursive self-improvement

The maintainer's policy is **autonomous, gate-only**: any change — instructions, skills, tool
descriptions, agent definitions, prompts, policies, plugins, programs, and aim's own Rust source —
that passes the evaluation gate is activated (and for source: merged and deployed) automatically.
That makes the gate the most safety-critical component in aim
([self-improvement](research/self-improvement.md)).

- **Loop**: observe (typed session telemetry, including provider token attribution) → propose (a
  candidate artifact with a hypothesis and a predicted effect) → evaluate (protected suites in
  isolated worktrees/sandboxes: deterministic validators, paired comparisons, a held-out set,
  live smoke) → activate (versioned, hot-reloaded or rebuilt+restarted) → monitor (regression
  detection, automatic rollback).
- **Protected set** — never writable by any candidate or agent: the evaluator and its suites, the
  permission engine, the evolution ledger, the gate itself. Enforced by the dispatcher's
  protected-path policy and by process separation (the evaluator runs outside the candidate's
  sandbox and write authority).
- **Ledger**: append-only chain of `{proposal, hypothesis, prediction, artifact digest, evaluator
  digest, results, decision, activation, rollback}`.
- **Kernel-proved**: activation requires a passing evaluation whose evaluator digest equals the
  protected evaluator's; the ledger is append-only; a rollback point always exists; no evolution
  step widens a permission ceiling or touches the protected set.
- **Swarm**: the lead posts "improve X" jobs; workers propose; evaluators score — candidates never
  see the scorekeeper.
- Private and ephemeral sessions never contribute telemetry.

---

## 11. User interfaces

- **TUI** (`aim-tui`, ratatui + crossterm): inline by default — chat lives in native scrollback,
  with a pinned bottom region (streaming partial, completion popups, queue, status, composer);
  alternate screen for pickers, search, dashboards, diff inspection and plugin panels;
  `--fullscreen` for a canvas layout. One semantic transcript model feeds both. One frame
  scheduler, synchronized output, dirty-line diffing; keystrokes render immediately, streams are
  coalesced. Composer: multiline, bracketed paste with collapsed large pastes, history search,
  optional vim/external editor, visible steer/queue outcomes, `/dictate`. A single async
  completion broker (files and dirs via `ignore` + `nucleo`, slash commands with argument
  completion, skills, agents, sessions, programs; later cloud buckets) with cancellation and stale
  -result fencing ([tui-ux](research/tui-ux.md)).
- **CLI**: `aim run "…"` (headless, streamed JSON events or text), `--ephemeral`, `aim login`,
  `aim sessions`, `aim board`, `aim evolve`, `aim mcp`.
- **Web**: Leptos (wasm) app served by the daemon, speaking `aim-daemon/1` over WebSocket; renders
  the same UI protocol and themes; **private** sessions are ephemeral.

---

## 12. Policy and security

- Default permission mode is **yolo** (the maintainer's standing preference, tny ADRs 0001/0159),
  as an explicit user-level policy — with typed ceilings that only narrow: subagents, remote
  agents, plugins and programs can never exceed their parent's or grant's ceiling.
- Deny always overrides allow; hooks can deny, never silently authorize.
- Paths are confined to workspace roots (proved); protected paths are enforced at the dispatcher.
- Secrets never enter logs, events, hooks or indexes; the remote side never sees credentials.
- OS sandboxing (Seatbelt/bubblewrap) wraps code-mode workers and, optionally, `aimx`.

---

## 13. Verification strategy

- **Verus kernels** (`aim-kernel`): every item marked "kernel-proved" above. Proof functions are
  named after the decision they lock and are cited from the ADR's `Verification` section.
- **Shells call the kernel**: a shell cannot make an illegal transition because the only way to
  compute a new state is the kernel's transition function; the shell persists what the kernel
  returns.
- **Where proofs cannot reach** (codecs, I/O, UI, external services): property/model-based tests
  against the kernel model, fixture-based contract tests, PTY screen tests for the TUI, and
  **live smoke tests** for every provider and service (required).
- **Static rules**: strict clippy (workspace lints), `unsafe_code = forbid` (narrow, documented
  exceptions only in platform crates), `disallowed-methods` for I/O outside the backend crate.

---

## 14. Performance and token efficiency

Targets are measured, never asserted ([landscape](research/landscape.md)). Levers, in order of
expected value: cache-stable prompt prefix and cache-routing keys; compact tool schemas with lazy
discovery; output handles instead of raw dumps; code mode for multi-step tool work; Jev-driven
effort (cheap steps stay cheap) and routing; remote compaction; WebSocket incremental input;
startup without provider I/O (first paint budget < 10 ms). Metrics: tokens by kind (input, cached,
output, reasoning; plus codex attribution by field), cache-hit rate, cost per task, success rate,
TTFT, wall time, tool calls, cold start, idle RSS.

---

## 15. Milestones

| M | Scope | Exit criterion |
| --- | --- | --- |
| M0 | mise-pinned toolchain (Rust, Verus, …), workspace, lints, `aim-kernel` pipeline, checks, ADRs, AGENTS.md | `mise run check` and `mise run verify` green |
| M1 | `aim-proto`, `aim-rpc`, `aim-workspace{,-local,-ssh}`, `aim-tools`, `aim-harness`, `aimx` | `aimx` serves core tools over unix/stdio; same tools over SSH (bootstrap + agentless); `aimx mcp` works with an MCP client |
| M2 | `aim-store`, `aim-auth`, `aim-llm{,-codex,-openai}`, `aim-acp`, `aim-agent`, `aim-daemon`, `aim run` | live smoke green for codex, OpenRouter, AI Gateway, Claude; **aim completes a real change on its own repo** |
| M3 | `aim-tui` (inline + fullscreen, composer, completion, surfaces, themes) | daily-drivable |
| M4 | context engine + compaction, skills, agents, memory, instructions import | long-running sessions without context failure |
| M5 | blackboard (local), worktrees, subagents | lead + workers deliver a multi-job change |
| M6 | Jev effort/router/relevance | measured token/latency win vs static policy |
| M7 | plugins + UI protocol + code mode + programs | a plugin and an agent-authored surface render in the TUI |
| M8 | conversation search, dictation, images, web search | — |
| M9 | web UI (Leptos), private mode | — |
| M10 | benchmarks vs codex/pi/unreal-agent; evolution loop live | aim improves itself through the gate |

From M2 on, aim builds aim: milestones are posted as blackboard jobs and executed by aim agents
(codex and Claude), with cross-model review.
