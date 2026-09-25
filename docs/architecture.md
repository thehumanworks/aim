# aim architecture

> Status: v0.2 (2026-09-25) — revised after the first cross-model review
> (19 findings; all blockers addressed). Source of truth for how aim is shaped. Decisions are
> recorded in [`adr/`](adr/); evidence lives in [`research/`](research/). When this document and an
> accepted ADR disagree, the ADR wins and this document is wrong — fix it.

aim ("Agent I am") is a harness for all agents, built and updated by the agents that use it
([vision](vision.md)). This document describes the shape that makes that possible: an execution
layer and an agent layer that are separate applications, a verified functional core, protocols
defined once as Rust types, and every agent-mutable thing expressed as versioned data.

---

## 1. Principles

1. **Functional core, imperative shell.** Every *decision* — state transitions, policies,
   planners, budgets, protocol negotiation — is a pure function in the Verus-verified `aim-kernel`.
   I/O code (network, disk, processes, UI) is a thin shell that asks the kernel what to do and does
   it. Proofs are preferred over example tests and are the documentation of locked decisions
   ([§13](#13-verification-strategy) says exactly what a proof does and does not cover).
2. **One authority per concern.** One tool dispatcher per agent daemon routes every tool call
   *aim controls*. One execution authority per harness (`aimx` enforces its own policy, whoever
   calls it). One session log per session. One job ledger for the swarm. One evolution gate, which
   is a separate trusted service. Everything else is a projection.
3. **One schema, many transports.** Every wire message is a Rust type in `aim-proto` (serde +
   schemars; no I/O, compiles to wasm for the web client). JSON Schema, MCP tool schemas, the web
   UI's types and the docs are generated from it. Transports are interchangeable: in-process,
   stdio, unix socket, SSH channel, WebSocket, HTTP.
4. **aim-owned core protocols, standard protocols at the edges.** `aim-harness/1` and
   `aim-daemon/1` are aim's own. MCP (server and client), ACP (client, later agent), A2A
   (federation) and gRPC are adapters that project the core — churn in a standard is confined to
   one module.
5. **Capabilities are data.** Models, reasoning-effort ladders, service tiers, tools, workspace
   capabilities and provider quirks come from catalogs and manifests, never hardcoded enums
   (the live catalog already exposes `low…ultra` efforts that codex's own UI does not —
   [live probes](research/live-probes.md)).
6. **Everything an agent may change is versioned data.** Instructions, prompts, skills, agent
   definitions, tool descriptions, policies, themes, UI surfaces, workflows, programs, plugins —
   and aim's own source. Changes are hot-reloaded or rebuilt, recorded in a ledger and reversible.
7. **Evidence over assertion.** Every integration ships live smoke tests (required — they run for
   real). Every performance claim has a benchmark. Every locked decision has an ADR, and a proof
   when it is logic.

---

## 2. Applications and processes

aim is two applications, each usable without the other, plus a small trusted gate service:

| App | Binary | Role | Runs where |
| --- | --- | --- | --- |
| **Execution layer** | `aimx` | Workspaces (fs, exec, PTY, search, watch), built-in tools, **its own enforcement** (principals, workspace grants, path confinement, protected paths, limits), MCP adapter. Holds no conversations and no LLM credentials. | Locally, or on any remote host (static musl build, auto-bootstrapped over SSH) |
| **Agent layer** | `aim` | Agent daemon (`aim daemon`), CLI, TUI, web UI server, MCP/ACP edges. Owns sessions, agents, providers, credentials, memory, swarm, plugins, the evolution *controller*. | The user's machine (or any server they choose) |
| **Gate** | `aim-gate` | The evolution gate: evaluates candidates with protected suites, signs receipts, promotes (merge, push, deploy). Built from a pinned, protected commit; never runs candidate code with its own credentials. | The user's machine, as a separate OS identity/sandbox ([§10](#10-recursive-self-improvement)) |

```
  TUI · CLI · web (Leptos) · editors(later)        external agents (codex CLI, Claude, any MCP client)
          │ aim-daemon/1 (unix | WS)                          │ MCP (stdio | streamable HTTP)
          ▼                                                   │
 ┌─────────────────────────── aim daemon (agent layer) ──────┼────────────────────────────┐
 │ sessions (event-sourced, SQLite) · agent loops · providers (codex, openai-compat)       │
 │ ACP client → claude-agent-acp …   · MCP client → user MCP servers                       │
 │ dispatcher (aim-controlled calls): admission · hooks · audit · provenance · routing     │
 │ context engine & compaction · Jev decisions · blackboard ledger · memory · search       │
 │ plugin host (wasmtime) · code-mode workers · program library · evolution controller     │
 │ media/search services (transcribe · images · web search) · MCP adapter `aim mcp`       │
 └────────────────────────────┬────────────────────────────────────────────────────────────┘
                              │ aim-harness/1 (JSON-RPC; unix | stdio | SSH | WS | HTTP)
                              ▼
 ┌────────────── aimx (execution layer) ──────────────┐     `aimx mcp` adapter (harness tools only)
 │ principals & grants · enforcement (kernel policy)   │
 │ tool registry · process/PTY handles · dedup table   │
 │ Workspace backends:                                 │
 │   local │ ssh → remote resident `aimx serve`        │
 │         │ ssh agentless (sftp + sh)                  │
 │   (later: container, cloud bucket via OpenDAL)       │
 └─────────────────────────────────────────────────────┘
```

**Local topology.** `aim` (TUI/CLI) auto-spawns `aim daemon` (one per user, unix socket in a 0700
directory), which auto-spawns a local `aimx serve` (unix socket). The TUI can exit and reattach;
sessions keep running in the daemon.

**Ephemeral.** `aim --ephemeral` runs the agent loop and the harness *in process* with an in-memory
store, and never touches the daemon, the database, history files or indexes
([§5.4](#54-ephemeral-and-private-sessions)).

**SSH.** `aim --ssh host` (or `/ssh host`) keeps inference, credentials, sessions and UI local and
binds the session's workspace to the remote host ([§9](#9-ssh-shadowing)).

**Standalone harness.** `aimx serve --unix PATH | --stdio | --ws ADDR | --http ADDR` exposes the
harness to any client; `aimx mcp --stdio | --http ADDR` exposes its tools to any MCP agent (so
`ssh host aimx mcp --stdio` shadows *any* MCP-capable agent's tool calls onto that host). In every
mode aimx authenticates the caller and enforces its grants itself ([§12](#12-policy-and-security)) —
aim's dispatcher is an *admission* layer on top, never the only guard.

---

## 3. Crate map

Crates exist only at true boundaries (a separately deployable app, a contract shared across apps,
or a dependency-heavy adapter a worker can own alone). Everything else starts as a module and is
split when it earns it.

| Crate | Kind | Responsibility |
| --- | --- | --- |
| `aim-kernel` | lib (Verus) | Pure decisions with specs and proofs. vstd is its only dependency; `no_std`. |
| `aim-proto` | lib | Wire and durable types for `aim-harness/1`, `aim-daemon/1`, session events and UI surfaces; error codes; schema export. No I/O (compiles to wasm). Lossless conversions to kernel types. |
| `aim-rpc` | lib | JSON-RPC 2.0 peer on tokio: framing, correlation, `$/cancel`, idempotency, seq streams, resume; transports (stdio, unix, WS, HTTP, in-process). |
| `aimx` | lib + bin | The execution layer: modules `workspace` (traits + local backend — the only code allowed `std::fs`/`std::process`), `ssh` (connection manager, askpass, bootstrap, agentless), `tools`, `authz` (principals, grants, enforcement), `server`, `mcp`. |
| `aim-llm` | lib | `ModelProvider` trait, normalized stream events, catalogs, usage accounting. |
| `aim-llm-codex` | lib | ChatGPT codex backend: OAuth (PKCE + device), Responses (SSE; WS later), catalog, V2 compaction, rate limits, and the codex media endpoints. |
| `aim-llm-openai` | lib | OpenAI-compatible chat/responses with provider profiles and quirks-as-data. |
| `aim-acp` | lib | ACP client for external agents (claude-agent-acp first); later the `aim acp` agent adapter. |
| `aim` | lib + bin | The agent layer: modules `agent` (loop, dispatcher, context engine), `store`, `config` (`.agents/` + foreign imports), `auth`, `daemon`, `cli`; later `board`, `memory`, `search`, `jev`, `plugins`, `coderun`, `programs`, `evolve`, `tui`. |
| `aim-gate` | bin | The trusted evolution gate ([§10](#10-recursive-self-improvement)). |
| `aim-web` | wasm | Leptos web client (M9). |
| `xtask` | bin | Repository invariants: ADR hygiene, kernel API rules, LOCKED digests. |

Candidates to split out of `aim` when they grow or need an isolated owner: `aim-tui`, `aim-jev`,
`aim-plugins` (wasmtime is heavy), `aim-coderun` (runs as a separate worker process anyway).

---

## 4. Protocols

### 4.1 `aim-harness/1` (agent layer ↔ execution layer)

JSON-RPC 2.0. NDJSON on byte streams (stdio, unix socket, SSH channel), text frames on WebSocket,
and request/response + SSE on HTTP. One `initialize {generations, build, caps, principal proof,
resume?}` per connection; compatibility is by **protocol generation** (the `LOCKED` negotiation in
`aim-kernel::negotiate`), not exact version. Precedent: codex `exec-server`, herdr, Zed
([acp-mcp](research/acp-mcp.md), [infra](research/infra.md)).

Method families (typed in `aim-proto::harness`):

- `workspace.open {root, backend}` → `WorkspaceId` + `Caps {exec, pty, watch, native_search,
  atomic_rename, max_concurrency, os, arch, shell}`.
- `fs.stat | read | read_many | write | edit | list | mkdir | remove | rename | copy`. Writes carry
  a precondition (`Any | IfAbsent | IfHash(h)`) so a model never overwrites a file that changed
  under it. `edit` applies exact-substring edits in the harness, atomically.
- `exec.spawn {argv | shell, cwd, env, pty?}` → `ProcId`; output streamed as
  `exec.output {proc, seq, stream, chunk}` notifications **and** pullable with
  `exec.read {proc, after_seq, max_bytes, wait_ms}`; `exec.write_stdin | resize | signal | wait`.
- `search.grep | glob` (streamed, bounded), `watch.start | stop` (+ `watch.event`).
- `tools.list | tools.call` — the high-level tool surface (what MCP projects).
- Resume: a reconnecting client sends `initialize {resume: token}` within the session's TTL to
  re-attach its processes and streams, then catches each stream up by pulling
  `exec.read {after_seq}` — there is no separate resume method.
- `$/cancel`, `$/progress`.

**Mutation safety.** Every mutating request (`fs.write | edit | remove | rename | copy | mkdir`,
`exec.spawn`, `tools.call` of a mutating tool) carries a client-generated **idempotency key**.
aimx keeps a bounded dedup table per session: a retried key returns the recorded outcome instead of
re-executing. Request acknowledgement is separate from stream sequence. If the dedup window has
expired, the answer is an explicit `unknown_outcome` error, never a silent re-execution.

### 4.2 `aim-daemon/1` (clients ↔ agent daemon)

JSON-RPC over the unix socket locally, WebSocket remotely (web UI, remote TUI). Its event model is
**shaped like ACP v2** so an ACP projection is mechanical: message upserts by id, tool-call upserts,
`state_update {running | idle(stop_reason) | requires_action}`, config options (`model`,
`effort`, `mode`), `usage_update`. It adds what ACP lacks: multi-client attach with fan-out, a
daemon-wide session index and search, the blackboard, plugin UI surfaces, completion, login flows,
dictation, evolution.

**Hosting** (`aim::host`). Serving sessions is not tied to the transport. A `SessionHost` runs
each live session as an actor that alone owns the session's agent and recorder. A prompt while the
session is idle starts a turn. A prompt during a turn steers it, and steering that arrives as the
turn ends is handed back (`steers_returned`). `set_config` applies at once when idle, otherwise from
the next turn. Each update is recorded, mirrored into the transcript and broadcast under one lock.
`session.attach` therefore returns a snapshot plus a subscription with no finished item missed or
repeated. Attaching to a stored session resumes it exactly once and continues its log without gaps.

UIs program against the `SessionClient` trait. The in-process host implements it (`--ephemeral`,
tests), and so does the daemon client over this protocol, so a UI cannot tell which one it has.

Providers and workspaces are injected as factories, so SSH workspaces and test fakes plug in
without changes to the host.

### 4.3 Edges

| Edge | Implementation | Notes |
| --- | --- | --- |
| MCP server | `aimx mcp` (harness tools), `aim mcp` (all aim tools incl. memory, search, board, programs, media) | Serves both MCP eras: `initialize` (2025-06/11) and 2026-07-28 `server/discover`; long jobs as plain submit/poll tools plus the tasks extension when declared. |
| MCP client | `rmcp` 3.4.x, pinned | User servers; length-capped stdio framing; foreign configs imported per-source opt-in. |
| ACP client | `agent-client-protocol` 2.2 (v1 stable) | Claude Code via `claude-agent-acp` (pinned in mise, gated by a live capability probe, not a version string). |
| ACP agent | later, `aim acp` | Projects `aim-daemon/1`; not in the MVP. |
| A2A | M5+ | Federation of the blackboard with remote aims/foreign agents. |
| gRPC | scheduled after M2 | The brief lists http/grpc/websockets as remote options; the MVP ships HTTP and WS. The gRPC adapter maps the same `aim-proto` types onto tonic services (server-streaming for output/watch, cancellation → `$/cancel`, status codes ↔ error codes, resume tokens in metadata) — a transport, never a second schema. |

### 4.4 Failure semantics and compatibility (frozen with `/1`)

Before either `/1` protocol is declared stable:

- **Errors**: a closed set of machine codes (`invalid_params`, `not_found`, `denied`,
  `precondition_failed`, `conflict`, `unknown_outcome`, `unavailable`, `timeout`, `cancelled`,
  `limit_exceeded`, `internal`) with a human message and an optional typed `data`.
- **Binary data**: base64 in JSON in `/1`; length-prefixed binary side frames are a negotiated
  capability later. Per-message and per-stream size limits are advertised in `initialize`.
- **IDs**: request ids per connection; stable ids for workspaces, processes, streams, sessions,
  events; idempotency keys for mutations.
- **Resume**: resume tokens are scoped to (principal, harness instance), carry a TTL, and survive
  reconnects within it; process ownership and ring buffers are bounded and released on expiry.
- **Unknown data**: unknown fields are ignored on the wire (`#[serde(default)]`); unknown *events*
  are preserved in storage and forwarded, never dropped.
- **Stored vs wire compatibility**: session events carry their own `schema` version, independent of
  the protocol generation; the store migrates forward only, with expand/contract migrations so the
  previous binary can still read the database during rollback ([§10](#10-recursive-self-improvement)).
- **Blobs**: content-addressed and reference-counted from events (forks share); GC removes
  unreachable blobs after a grace period; backup = the database + the blob directory.

---

## 5. Sessions: event-sourced state

### 5.1 The log

A session is an append-only log of typed `SessionEvent`s (`aim-proto::event`), each with
`{schema, session_id, seq, turn_id, ts}`: user/assistant messages, reasoning (opaque provider
items preserved byte-exact), tool calls and results, steering, compaction markers, model/effort
changes, usage, errors, UI surfaces, plugin entries. The durable transcript is **lossless**; what
the model sees is *derived* by the context engine. Provider-native items (e.g. codex reasoning items
with `encrypted_content`, hosted search items) are stored next to the normalized form so replay is
exact.

### 5.2 Forks

A fork is a new session `{parent, fork_seq}` that shares the parent's immutable prefix.

### 5.3 Storage

`~/.aim/aim.db` (SQLite WAL, owned by one DB actor thread in the daemon) holds session logs, the job
ledger, grants, the search index and the controller's view of the evolution ledger. Large blobs
(tool outputs, artifacts, images) live content-addressed under `~/.aim/blobs/`. The raw store is
local and 0600; projections (hooks, indexes, audit, telemetry) are redacted — credential values
never enter them. A lossless transcript can contain secrets the *user or a tool* produced; those
are protected by local file permissions (optional encryption at rest later), not by a claim that
they cannot occur.

### 5.4 Ephemeral and private sessions

`--ephemeral` (CLI/TUI) and **private** (web) sessions use `MemoryStore`. Inside aim this is a
type-level guarantee: the ephemeral session constructor only accepts the memory store, and the
indexer, memory writer and telemetry take a `Persistent` session witness that ephemeral sessions
cannot produce. Outside aim, privacy is per backend and stated honestly:

- codex / OpenAI-compatible: `store:false` where defined; provider retention is outside aim.
- Claude via ACP: sessions start with `persistSession: false`; a live smoke test asserts the adapter
  leaves no transcript on disk. If an adapter version cannot guarantee it, private/ephemeral mode is
  **refused** for that agent rather than silently degraded.
- Services with server-side retention (codex dictation keeps audio 30 days —
  [live probes](research/live-probes.md)) are labelled in the UI before use.
- Private/ephemeral sessions never send content to Jev, embeddings or the evolution loop.

---

## 6. The agent layer

### 6.1 Agent backends

A session is driven by one `AgentBackend`, which emits normalized events:

- **Native loop** — aim's own loop over a `ModelProvider` (codex backend, OpenAI-compatible, later
  Anthropic API). Every tool call goes through aim's dispatcher.
- **ACP agent** — an external agent (Claude Code via `claude-agent-acp`, later codex-acp, gemini,
  goose, …) with its own loop. aim is the ACP client: renders its updates, answers permission
  requests, sets config options. Tool authority is explicit per session:
  - *aim tools* — built-ins disabled via `_meta.claudeCode.options {tools, toolAliases →
    mcp__aim__*, strictMcpConfig, settingSources, allowedTools}` and aim's MCP server injected,
    with schemas mirroring Claude Code's own tool shapes so skills and prompts keep working. Every
    tool call then crosses aim's dispatcher and aimx. **Required under `--ssh`** (and the local
    default once its conformance gate has passed on the pinned adapter).
  - *native tools* — the agent's built-ins run in its own process. aim sees them only through ACP
    updates: they are rendered and logged, but **not** admitted, hooked or shadowed by aim. The UI
    labels this mode. Agents that cannot surrender their built-ins are refused under `--ssh`.
- Claude login does not need the TUI: `aim login claude` runs the adapter's terminal-auth command
  (`--cli auth login --claudeai | --console`) in the current terminal; the TUI offers the same in a
  PTY pane. Remote-workspace Claude sessions get a local scratch cwd (the adapter needs an existing
  local directory) while every tool call executes remotely through aim's tools.

### 6.2 The native loop

```
inbox (idempotent inputs: prompt, steer, tool results, job events, webhooks)
  → kernel turn state machine decides the next effect
  → context engine builds the model view (pure) → provider stream → normalized events
  → complete tool calls → dispatcher (parallel where allowed) → results → loop
  → settle: exactly one terminal outcome per turn
```

The turn state machine is a kernel spec with theorems: an incomplete tool call is never dispatched;
every dispatched call is settled by exactly one result event (cancellation settles pending calls
with a cancelled result); a steer is either delivered before the next provider request or returned
to the queue — never lost (tny ADR 0011/0013 lesson). The shell's conformance tests replay recorded
provider streams and fault injections against it.

### 6.3 The dispatcher

Every tool call aim controls — from the model, a code-mode cell, a plugin, a saved program or a
hook — enters one dispatcher with a `source` (`model | code(cell) | plugin(id) | program(id@sha) |
hook(id)`) and a declared **location** (`workspace` — must execute on the session's bound aimx
target; `local-service` — runs where credentials live, labelled as local):

1. **admission** (kernel `policy`: session grants ∩ agent ceiling ∩ source ceiling; deny overrides
   allow);
2. **pre-hooks** (plugins, imported hooks; tny ADR 0028 fold precedence: cancel/deny beat rewrite);
3. **route**:
   - harness tool → `aimx` (which enforces again — [§12](#12-policy-and-security));
   - agent tool (subagent, board, memory, sessions search, programs, ui) → daemon internals;
   - service tool (`transcribe`, `generate_image`, `web_search`) → media/search services
     (credentials local; outputs become blobs);
   - plugin tool → plugin host (plugins have no ambient fs/net; their effects go back through the
     dispatcher);
   - MCP tool → MCP client. Under `--ssh`, MCP servers declared `location: workspace` are spawned on
     the remote through aimx; local-only MCP servers stay available but are labelled local;
4. **post-hooks**, output shaping (large outputs become handles — the model sees head/tail plus a
   `read_output(handle, range)` affordance), event emission, audit.

A live conformance test — "remote changed, local untouched" — runs for each tool source (built-in,
MCP, plugin, hook, program, code mode) under `--ssh`.

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
5. **Retrieval** — nothing is lost: compacted content stays searchable (`search_sessions`,
   `read_session`).

The plan's kernel spec: a tool call and its result (matched by **call id**, so parallel calls
`call₁ call₂ result₁ result₂` are handled) are never separated; pinned items are always kept; the
plan fits the budget; compaction strictly shrinks; token arithmetic cannot overflow.

### 6.5 Providers

`ModelProvider::{catalog, stream}` with normalized events and provider-native sidecars.

- **Codex (ChatGPT subscription)** — `https://chatgpt.com/backend-api/codex`: `POST /responses`
  (SSE; WebSocket `responses_websockets=2026-02-06` with `previous_response_id` input elision
  later, behind a benchmark); `GET /models?client_version=…` catalog with ETag; V2 compaction;
  rate-limit headers (`x-codex-primary-*`, `-secondary-*`, credits) surfaced in the status line;
  `x-codex-turn-state` echoed for affinity; `usage.attribution` recorded for token-efficiency work.
  Contract: [codex-backend](research/codex-backend.md) + [live probes](research/live-probes.md).
- **OpenAI-compatible** (`aim-llm-openai`, [ADR 0011](adr/0011-openai-compatible-profiles.md)) —
  named profiles `{id, base_url, api_key_env, wire: chat | responses, quirks, headers, models}`,
  presets for OpenRouter and Vercel AI Gateway. Only `chat` is implemented. Quirks are data, not
  code branches:
  - Output caps: `min_output_tokens` (AI Gateway 16) and `max_output_tokens_field`.
  - Billed cost: `cost_pointer` into the streamed usage (`/cost` on OpenRouter, `/gateway_cost` on
    AI Gateway, which includes surcharges); the raw usage object is kept in `Usage.native`.
  - Caching and affinity: `extra_body` (OpenRouter `cache_control`, AI Gateway
    `providerOptions.gateway.caching = "auto"`), `session_header` for `Request.session_id`
    (`x-session-id` / `x-session-affinity`), `cache_key_field` for `Request.cache_key`.
  - Wire capabilities: `supports_parallel_tool_calls`, `supports_stream_usage` (usage is then
    required: a turn without it is a `Protocol` error, never zero usage), `reasoning_param`,
    `replay_reasoning_details`, `tool_result_images` (only to models whose catalog entry accepts
    images; an unseen model is looked up once), `idle_timeout_secs`.
  - `headers` holds extra request headers: literal non-secret values, or `{ env = "NAME" }` for
    secrets, read per request and never serialized. `models` is a static catalog for endpoints
    without discovery.

### 6.6 Auth

- **ChatGPT**: aim's own OAuth grant (browser PKCE on `127.0.0.1:1455`, fallback 1457; device code
  flow), stored in the OS keyring with a 0600-file fallback. `~/.codex/auth.json` is a read-only
  fallback source and is **never refreshed by aim** (refresh-token rotation would log the codex CLI
  out). One refresher per daemon (lease) avoids rotation races between sessions.
- **Claude**: `claude-agent-acp` owns Claude credentials; see [§6.1](#61-agent-backends).
- **API keys**: referenced by env-var name (never persisted unless the user stores them in the
  keyring).

### 6.7 Jev decisions

`aim-jev` (a module first) exposes a `Decider` trait with a Jev implementation (`typesafe-jev`,
called on `spawn_blocking`) and a deterministic fallback. Jev answers many calibrated questions
about one state in one ~0.6 s request, for ~$0.04/M input tokens, so aim batches them:

- **Per-step bundle** (one request, issued while tools execute, deadline-bound, applied to the next
  provider request): effort `Score` over the *selected model's catalog ladder*; `Noul`s for
  "stuck/looping", "making progress", "would past sessions help"; relevance of candidate skills.
- **Router**: deterministic capability filter (context size, tools, images, tier) → Jev `Choice`
  among eligible configured models (described by strengths/cost) → sticky per task; switching
  accounts for prompt-cache loss.
- **Relevance**: compaction elision, memory recall, skill/program suggestions, search rerank.

Jev's probabilities are **quantized** at the kernel boundary (validated, clamped, fixed-point
basis points) before any kernel decision: the effort controller is a kernel spec over integers —
bounds from the user/agent definition and the catalog, at most one step per decision, hysteresis
(no flip-flop within a window), explicit user override wins. Proofs are about the controller, never
about model or Jev quality. Every decision is logged with inputs and outputs so the evolution loop
can tune thresholds offline. Private/ephemeral sessions never send content to Jev.

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
repo/.agents/workflows/<n>/       workflow manifests + entrypoints (§8.4)
repo/.agents/programs/            saved code-mode programs (git)
repo/.agents/plugins/             project plugins (need hash-pinned trust)
repo/.agents/mcp.json, hooks.toml native MCP servers and hooks (explicit trust)
repo/.agents/memory/              optional shared project memory (never auto-committed)
~/.aim/{config.toml, agents/, skills/, prompts/, plugins/, programs/, memory/, mcp.json, hooks.toml}
```

Project resources are read **through the Workspace trait**, so under `--ssh` the remote project's
`AGENTS.md` and `.agents/` apply (labelled as remote, tny ADR 0040). Foreign formats (Claude
`.claude/{skills,agents,commands}`, `CLAUDE.md`, Codex TOML agents and `[mcp_servers]`, pi,
oh-my-pi, tny, OpenCode) are **read** as typed descriptors with provenance `{kind, path, scope,
parser_version, trust, hash}`. Discovery never executes anything; foreign MCP servers and hooks need
a per-source opt-in (tny ADR 0052). Skills load metadata-first (catalog budgeted to a small share of
context), full bodies on activation; explicit mentions inject the body into the user turn so the
cached system prefix never changes (tny ADR 0056).

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

Kernel specs (conditional on the ledger being the only writer): one current attempt per job
generation; stale claim tokens cannot mutate; a job starts only when its dependencies' evidence
matches; terminal execution never implies acceptance; admission never exceeds capacity; delegation
only narrows permissions. Crash/replay and concurrency are covered by shell fault-injection tests.

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

### 8.4 Programmable workflows

A workflow is a versioned directory `.agents/workflows/<name>/` with `workflow.toml` (typed
params/results schemas, triggers — manual, slash command, schedule, event, job — capability ceiling,
budget) and an entrypoint: either a **code-mode program** (imperative) or a **step DAG** (declarative:
steps = tool calls, agent turns or blackboard jobs, with dependencies). Every step goes through the
dispatcher; each has a durable step id so a workflow resumes after a crash; retries, cancellation and
compensation are declared per step. Workflows compile onto the same job ledger as the swarm (tny ADR
0143: one scheduler, one authority). Code-mode workflows land with M7; DAG workflows with M5.

---

## 9. SSH shadowing

- **Transport**: the system OpenSSH binary (full `ssh_config`, agents, ProxyJump, FIDO, known_hosts).
  aim starts its own ControlMaster (`~/.aim/ssh/%C`) with `SSH_ASKPASS` pointing at aim, so
  password/2FA/host-key prompts reach the CLI, TUI or web UI even from the headless daemon; host-key
  checking is never weakened.
- **Remote aimx is resident**: `aimx serve` runs detached on the remote (setsid, pid file, flock),
  listening only on a 0700-directory unix socket, owned by the remote user. Each SSH connection runs
  `aimx proxy`, which only bridges the channel to that socket. Processes and output ring buffers
  (bounded, e.g. 8 MiB per process) survive network drops; a reconnect presents its resume token
  within the TTL (default 30 min); the daemon exits after an idle period with no clients and no live
  processes. Heartbeat every 5 s; reconnect with exponential backoff (1 s → 2 min).
- **Bootstrap** (default): `uname -sm` probe → select `aimx-g<gen>-<ver>-<target>` from the local
  cache, a local build, or release assets. **Trust root**: release artifacts are listed in a signed
  manifest whose ed25519 public key is compiled into `aim`; the artifact's sha256 is verified
  locally *before* upload; after upload the installed bytes are re-hashed with an independent remote
  utility (`sha256sum`/`shasum -a 256`) when present (a binary reporting its own hash is not
  evidence). Dev builds use the locally computed hash of the local build. The cache key includes
  the verified digest and protocol generation. Targets: `x86_64`/`aarch64-unknown-linux-musl`,
  `aarch64`/`x86_64-apple-darwin`. Per-host `bootstrap = never` forces agentless.
- **Agentless fallback**: sftp + `sh -c` over the master, atomic temp→rename writes, bytes on
  stdin, `rg --json` when present; reduced `Caps` (no watch, PTY via `ssh -tt`, no resume across
  drops — reported honestly). Used on signature, digest, target or execution failures.
- **What stays local**: inference, credentials, sessions, memory, plugins, local-only MCP servers.
  The remote never sees a provider key. Remote `AGENTS.md` is loaded and labelled as remote
  (tny ADR 0040); the model's preamble states the workspace is remote.
- **Enforcement**: inside `aimx`, only the backends (`workspace::local`, `ssh`) and the server
  plumbing (`server`, the binary entry point) may touch `std::fs` / `std::process` /
  `tokio::fs` / `tokio::process`; `cargo xtask check` rejects them anywhere else under
  `crates/aimx/src` (clippy's `disallowed-methods` is crate-wide and cannot express a module
  boundary), so a tool cannot accidentally run locally — pi's SSH example greps locally; this is the failure it prevents. Plus the live
  "remote changed, local untouched" conformance suite ([§6.3](#63-the-dispatcher)).

---

## 10. Recursive self-improvement

The maintainer's policy is **autonomous, gate-only**: any change — instructions, skills, tool
descriptions, agent definitions, prompts, policies, plugins, programs, and aim's own Rust source —
that passes the evaluation gate is activated (for source: merged, pushed and deployed)
automatically, without a human approval step. That makes the gate the most safety-critical
component in aim ([self-improvement](research/self-improvement.md)).

### 10.1 Trust boundary

A path list inside the agent process cannot confine a yolo shell, a build script, a proc macro, a
plugin or a modified dispatcher. So the boundary is an **OS boundary**:

- **`aim-gate` is a separate trusted service** with its own identity: built from a pinned commit on a
  protected ref, running as a separate OS user (or, at minimum, outside every candidate sandbox),
  holding the receipt-signing key and the push credentials for the protected branch. Candidate code
  never runs with gate credentials.
- **Candidates run confined**: candidate agents, their builds (`build.rs`, proc macros), tests,
  code-mode cells and plugins run under an OS sandbox (Seatbelt on macOS, bubblewrap on Linux) that
  can write only the candidate worktree and has no host credentials; live provider smoke is brokered
  through the gate rather than done with the candidate's own access.
- **Protected set** — never writable from inside any candidate sandbox, and write-denied even for
  normal yolo sessions by aim's baseline sandbox profile: the gate binary and its config, the
  evaluator and its suites (including hidden cases and baselines), the receipt key, the evolution
  ledger, the permission engine's policy files, and every `LOCKED` spec in `aim-kernel` together
  with `crates/aim-kernel/LOCKED.toml` (the specs are the evaluator's definition of correctness: a
  candidate that weakened a spec to something trivially true would still verify, so the proof would
  lock nothing). Agents may freely change proofs and implementations; a diff touching a `LOCKED`
  spec or the manifest is rejected by the gate and needs the maintainer plus a superseding ADR.

### 10.2 Evaluation

The loop: observe (typed session telemetry, including provider token attribution) → propose (a
candidate commit/artifact with a hypothesis and a predicted effect) → evaluate → promote → monitor.
The gate:

- inspects the **complete** diff and dependency closure (Cargo.lock, `.cargo/`, build scripts, CI,
  scripts, generated code, toolchain pins, mise config) and rejects protected-set changes, deleted or
  disabled tests, and unreviewed new dependencies (supply chain);
- runs protected validators **on final artifacts after the candidate has exited**, never trusting
  agent logs;
- compares paired baseline/candidate repetitions against predeclared thresholds (safety first, then
  quality, then latency and cost), and uses a fresh holdout once per campaign;
- issues a **signed receipt** bound to the candidate, evaluator, environment and baseline digests.

### 10.3 Promotion state machine

`proposed → evaluated(receipt) → merged(base SHA CAS) → pushed(remote ref read back) → deployed(exact
SHA; binary, config and schema digests recorded) → canary → active`, with `rejected`,
`rolled_back` and `failed(step)` exits. Nothing is reported active unless every step's readback
matched. The previous runnable artifact is kept; database migrations are expand/contract so the
previous binary still reads the store; external effects are not claimed reversible. The ledger is
append-only and hash-chained, and is **anchored externally** by committing checkpoints to the private
GitHub repository, so a same-host writer cannot silently rewrite history.

Kernel specs (over the gate's own state, conditional on its inputs): activation requires a receipt
whose evaluator digest equals the pinned evaluator's; the ledger view is append-only; a runnable
rollback target always exists in the gate's state; no transition widens a permission ceiling or
changes a protected digest. Swarms help by parallelising candidates and evaluators — candidates
never see the scorekeeper. Private and ephemeral sessions never contribute telemetry.

---

## 11. User interfaces

- **TUI** (module first, `aim-tui` later; ratatui + crossterm): inline by default — chat lives in
  native scrollback, with a pinned bottom region (streaming partial, completion popups, queue,
  status, composer); alternate screen for pickers, search, dashboards, diff inspection and plugin
  panels; `--fullscreen` for a canvas layout. One semantic transcript model feeds both. One frame
  scheduler, synchronized output, dirty-line diffing; keystrokes render immediately, streams are
  coalesced. Composer: multiline, bracketed paste with collapsed large pastes, history search,
  optional vim/external editor, visible steer/queue outcomes, `/dictate`. A single async completion
  broker (files and dirs via `ignore` + `nucleo`, slash commands with argument completion, skills,
  agents, sessions, programs; later cloud buckets) with cancellation and stale-result fencing
  ([tui-ux](research/tui-ux.md)).
- **CLI**: `aim run "…"` (headless, streamed JSON events or text), `--ephemeral`, `aim login
  codex|claude`, `aim sessions`, `aim board`, `aim evolve`, `aim mcp`.
- **Web**: Leptos (wasm) app served by the daemon, speaking `aim-daemon/1` over WebSocket; renders
  the same UI protocol and themes; **private** sessions are ephemeral.

---

## 12. Policy and security

- **Two enforcement points, one policy kernel.** aim's dispatcher *admits* calls (session grants ∩
  agent ceiling ∩ source ceiling). aimx *enforces* on every request regardless of caller: the
  connection's authenticated principal carries a grant (workspace roots, operation classes, limits);
  a client may narrow it per call (e.g. a read-only subagent), never widen it. Both evaluate the same
  `aim-kernel` policy spec.
- **Principals**: local unix-socket peers are identified by peer credentials (same OS user) and
  receive the grants configured for them; network clients must present a scoped bearer token (or
  mTLS) issued by `aimx`'s owner; the daemon authenticates web clients the same way.
- **Listeners**: loopback or unix sockets only by default. A non-loopback listener requires
  authentication and TLS (or a declared, protected reverse proxy); browser-reachable WS/HTTP checks
  `Origin` and uses CSRF-safe tokens; request, body and concurrency limits are enforced. Negative
  tests cover unauthenticated fs/exec and cross-workspace access.
- **Default mode is yolo** (the maintainer's standing preference, tny ADRs 0001/0159): no prompts,
  as an explicit user-level policy. Two things yolo never relaxes: ceilings only narrow (subagents,
  remote agents, plugins and programs never exceed their parent's or grant's ceiling), and the
  protected set stays write-denied by aim's baseline sandbox profile ([§10.1](#101-trust-boundary)).
- Deny always overrides allow; hooks can deny, never silently authorize.
- Credential values never enter logs, events, hooks, indexes or telemetry; the remote side never
  sees credentials.
- OS sandboxing (Seatbelt/bubblewrap) wraps code-mode workers, candidates, and optionally `aimx`.

---

## 13. Verification strategy

**What a kernel proof means.** A Verus proof in `aim-kernel` establishes a property of a *pure
function over its inputs*. It never, by itself, establishes an effect on a filesystem, a database,
a network or a model. Every claim in this document of the form "the kernel proves X" is shorthand
for a conditional theorem — e.g. "given an authenticated principal and a normalized path with no
symlink escape, `policy::permits` allows only in-root targets"; "for each accepted state-machine
event, exactly one terminal result event is emitted"; "given a receipt whose digests match the
pinned evaluator, the transition to `active` is permitted". The shells then carry the assumptions
with evidence of a different kind: conformance and fault-injection tests (crash/replay, symlink
races, disconnects), adversarial sandbox tests, and live smoke tests.

- **Kernel rules** (enforced by `cargo xtask check` and `mise run verify`):
  - pinned mise + Rust + Verus + vstd toolchain, bumped together;
  - verify only `aim-kernel`, with `--no-cheating` on the kernel (vstd's own axioms are trusted),
    in a cleaned target so flags are never skipped by cache;
  - no public exec function with a `requires` clause (a precondition is an assumption unverified
    callers can break): public APIs are total, with private fields, type invariants and
    `Result`/`Option`;
  - no `assume`/`admit`/`external_body`/`assume_specification`; time, randomness and I/O results
    enter as plain arguments; no floats (Jev scores are quantized first);
  - each decision is one spec fn marked `LOCKED(ADR-NNNN)` (or `DRAFT(<milestone>)` until final),
    with theorems about the decision itself; `LOCKED.toml` records a digest of each locked spec's
    transitive closure (other spec fns and types it references), and the gate rejects changes;
  - kernel types are separate from serde wire types (`aim-proto`), with tested conversions.
- **Where proofs cannot reach** (codecs, I/O, UI, external services): property/model-based tests
  against the kernel model, fixture-based contract tests, PTY screen tests for the TUI, and
  **live smoke tests** for every provider and service (required).
- **Static rules**: strict clippy (workspace lints, `-D warnings`), `unsafe_code = forbid`
  (narrow, documented exceptions only in platform code), and an xtask path rule confining OS
  access in the execution layer to its backends and server plumbing ([§9](#9-ssh-shadowing)).

---

## 14. Performance and token efficiency

Measured head-to-head on a zero-latency mock ([landscape](research/landscape.md)): unreal-agent's
first request is ~0.7k tokens with 51 ms to first request and 16 MB peak RSS; pi ~1.5k / 228 ms /
147 MB; oh-my-pi ~7.1k / 895 ms / 434 MB; codex ~9.7k / 142 ms / 105 MB. Levers, ranked: fewer turns
(background and batched tools, code mode), smaller per-request context (output handles, summarised
reads, cache-safe pruning), correct cache routing (workspace-shared keys), an append-only prefix,
lazy tool loading, cache-preserving compaction, Jev-driven effort and routing, WebSocket incremental
input, startup without provider I/O (first paint < 10 ms).

**Benchmark manifest** (the pass condition for "beat"): identical tasks, model/provider, tool
permissions, hardware, local/remote topology, cache state and repetitions for every harness; token
usage read at a recording proxy (self-reported usage is unreliable); quality graded independently
first. Tiers: an offline wire benchmark on every PR (fixed-prompt size, prefix stability, output
bounds, latency, memory); a nightly live run (a port of tny's `harness_bench`); per-release runs
through Harbor on Terminal-Bench 4.0, SWE-Atlas QnA, DeepSWE 1.1 and a SWE-bench Verified subset.
M10 requires a measured win on predeclared dimensions at no material quality loss — or reports
exactly where aim does not yet beat a peer.

---

## 15. Milestones

Slices are cut so self-hosting comes first and independent work runs in parallel once the
contract exists.

| M | Scope | Exit criterion |
| --- | --- | --- |
| **M0** | mise-pinned Rust + Verus + verusfmt, workspace, strict lints, `aim-kernel` pipeline (`--no-cheating`), xtask invariants, AGENTS.md, ADRs | `mise run check` and `mise run verify` green |
| **M1-proto** | minimal frozen `aim-proto` tool/stream/session contract + `aim-rpc` (lead) | round-trip + schema tests; ADR |
| **M1a** | local standalone `aimx`: read, list, exact edit, write, exec (+PTY), grep/glob over unix/stdio; principals/grants; idempotency | conformance suite green locally |
| **M1b** (∥) | SSH: ControlMaster, resident remote aimx + proxy + bootstrap with trust root, agentless fallback; `aimx mcp` | "remote changed, local untouched" live |
| **M2-llm** (∥) | codex OAuth + Responses streaming + catalog; OpenAI-compatible profiles; ACP client for Claude (capability probe, login) | live smoke green per adapter |
| **M2a** | `aim` agent loop + dispatcher + store + headless `aim run` (+ `--ephemeral`) on local aimx + codex | **aim completes a real change on its own repo** |
| **M2b** | SSH sessions, OpenAI-compatible and Claude sessions end to end | live smoke green for all three over local and SSH |
| M3 | TUI (inline + fullscreen, composer, completion, UI surfaces, themes) | daily-drivable |
| M4 | context engine + layered compaction, skills, agents, instructions import, memory | long sessions without context failure |
| M5 | blackboard (local), worktrees, subagents, DAG workflows, webhooks | lead + workers deliver a multi-job change |
| M6 | Jev effort/router/relevance | measured token/latency win vs static policy |
| M7 | plugins + UI protocol + code mode + programs + code-mode workflows | a plugin and an agent-authored surface render in the TUI |
| M8 | conversation search, dictation, images, web search | — |
| M9 | web UI (Leptos), private mode; gRPC adapter | — |
| M10 | benchmarks vs codex/pi/unreal-agent; `aim-gate` + evolution loop live | aim improves itself through the gate |

M2–M4 are self-hosted with ordinary `aim run` sessions (codex and Claude workers in herdr panes,
cross-model review). From M5, milestones are posted as blackboard jobs and executed by aim agents.
