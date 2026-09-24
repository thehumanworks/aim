# tny lessons for aim (source snapshot, 2026-09-25)

## TL;DR

- **FACT:** tny's current architecture is one native OpenAI-compatible HTTP agent loop, with Codex as a ChatGPT
  Responses profile and optional external ACP clients. The original Codex app-server loop was removed because it
  made permissions, SSH, tools, and extensions inconsistent. `refs/tny/docs/architecture.md:9-37`;
  `refs/tny/docs/adr/0065-codex-chatgpt-responses-backend.md:16-64`.
- **FACT:** tny's `--ssh` keeps inference/session/UI local and routes native workspace tools through a persistent
  OpenSSH connection. A later ACP decision also supports a pinned, verified Claude adapter with built-ins disabled;
  other versions fail early. Remote project instructions follow the remote workspace.
  `refs/tny/docs/adr/0022-ssh-execution-boundary.md:16-66`; `refs/tny/docs/adr/0040-ssh-agents-md.md:24-46`;
  `refs/tny/docs/adr/0164-optional-acp-clients.md:74-79`.
- **FACT:** tny has independent Codex-backed speech, dictation, image, and search services. They resolve credentials
  independently from the selected chat provider. `refs/tny/docs/adr/0070-provider-independent-speech.md:14-24`;
  `refs/tny/docs/adr/0074-extensible-image-service.md:13-35`;
  `refs/tny/docs/adr/0079-provider-independent-dictation.md:13-46`;
  `refs/tny/docs/adr/0109-provider-independent-codex-search.md:7-40`.
- **FACT:** Jev currently serves only explicit `tny score` and `tny choose` calls. Automatic reasoning effort, model
  routing, and memory decisions were excluded from ADR 0165; no policy, measured latency, or fallback is
  established. `refs/tny/docs/adr/0165-tnyjev-decision-engine.md:8-49`; `refs/tny/src/cli/cmd_jev.c:170`.
- **FACT:** tny's swarm is a bounded, durable DAG over existing jobs, with attempt-fenced mailboxes and named typed
  messages. Success and hashes are evidence, not acceptance. `refs/tny/docs/adr/0143-durable-dag-over-jobs.md:5-45`;
  `refs/tny/docs/adr/0146-durable-team-mailbox.md:15-66`;
  `refs/tny/docs/adr/0160-swarm-contribution-contracts.md:25-82`.
- **FACT:** `sdk/schema/events.json` is the canonical 14-event public vocabulary with a versioned envelope;
  extension lifecycle hooks are a broader, capability-gated contract.
  `refs/tny/docs/adr/0030-public-event-schema.md:14-60`;
  `refs/tny/docs/adr/0028-extension-parity-contract.md:61-95`.
- **FACT:** tny compacts after eight completed turns, keeping four verbatim; full local transcript remains.
  `semantic_search` is lexical, and `memory` is an explicit JSON store rather than semantic conversation retrieval.
  `refs/tny/docs/features/sessions.md:351-353`; `refs/tny/docs/features/mcp-and-skills.md:31-43,107`.
- **FACT:** tny persists native sessions as files with detached per-turn runners, not a single SQLite daemon. It has
  CLI/TUI/SDK and a wasm landing terminal; a standalone web chat app is not established by these sources.
  `refs/tny/docs/architecture.md:54-90`; `refs/tny/AGENTS.md:101-103`.
- **FACT:** ADR 0150 supersedes binary-size ceilings and competitor size targets. It prioritizes effective context,
  reliability, authority, reversibility, and measured speed; aim's token/performance target should retain this
  distinction. `refs/tny/docs/adr/0150-agent-first-harness-and-measured-footprint.md:25-74`.
- **RECOMMENDATION:** Make the harness tool service independent of all agent adapters, and use one authority model
  for local, SSH, and remote calls. An ACP agent's built-in tools cannot be assumed to cross that boundary.
  `refs/tny/docs/adr/0022-ssh-execution-boundary.md:59-62`; `refs/tny/docs/adr/0164-optional-acp-clients.md:5-43`.
- **RECOMMENDATION:** Use Rust types and Verus to prove closed state transitions, identity/attempt fencing,
  path/authority constraints, bounded admission, and single terminal settlement; keep protocol behavior and
  empirical performance under integration tests. `refs/tny/docs/adr/0146-durable-team-mailbox.md:23-66`;
  `refs/tny/docs/adr/0117-allocation-free-provider-oom-settlement.md:1-8`.

## Findings

### Scope, current source, and status convention

**FACT:** This digest treats the cloned `refs/tny` source and its accepted ADRs as first-party design evidence.
“Implemented” below means a source or current architecture/documented runtime path was found, not a live end-to-end
run. “Partial” means an explicit capability limit or pending integration is documented. “Planned” means the aim idea
has no corresponding completed tny implementation in inspected sources. The ADR directory has duplicate numbers
(0030, 0045, 0087, 0114–0118, 0136, 0153, 0154, 0166); cite the slug, never a number alone.
`refs/tny/docs/adr/README.md:1-23`; `refs/tny/docs/adr/0065-codex-chatgpt-responses-backend.md:1-9`.

### Aim brief → tny decisions

| Aim topic | tny decision and rationale | Status / source |
| --- | --- | --- |
| Harness / agent split | 0023 **libtny embedding ABI**, 0032 **capability discovery**, 0036 **host services**, 0037 **ABI 1**, 0086 **standalone SDK toolkit**, 0152 **native HTTP only**, 0164 **optional ACP clients**: expose opaque runtime/session/event handles and standalone media/toolkit services; one owned native agent loop, external ACP loop optional. aim's separate independently deployable harness is a further split. | Partial analogue; `refs/tny/docs/architecture.md:9-52`. |
| Local socket / remote HTTP/gRPC/WebSockets | 0053 **forked turn isolation**, 0058 **session control channel**, 0166 **global sessions**: NDJSON Unix socket lets UI detach and tools communicate with a runner. No general remotely exposed harness service contract established. | Socket implemented; network service planned; `refs/tny/docs/architecture.md:54-68`; `refs/tny/docs/adr/0058-session-control-channel-roles-and-tool-ops.md:33-116`. |
| Codex Responses + login | 0065 **Codex ChatGPT Responses backend**, 0066 **native ChatGPT login**, 0170 **catalog discovery version**: direct HTTP avoids host-loop mismatch; separate credential sources and native PKCE/device flow remove Codex binary dependency. | Implemented in source; `refs/tny/docs/adr/0065-codex-chatgpt-responses-backend.md:38-105`; `refs/tny/docs/adr/0066-native-chatgpt-login-and-credential-sources.md:55-109`. |
| Claude login / ACP | 0019 **subscription logins** documents an earlier Claude OAuth compatible-profile route; current product docs say Claude models use configured gateways; 0029 **named ACP profiles**, 0030 **settings schema and ACP map**, 0164 **optional ACP clients** allow external Claude over stdio with owning-runtime MCP bridge. ACP adapter authentication remains external. | Partial; `refs/tny/docs/adr/0019-subscription-logins-claude-grok.md:30-36,56-90`; `refs/tny/docs/product.md:54-59`; `refs/tny/docs/adr/0164-optional-acp-clients.md:64-79`. |
| OpenAI-compatible providers | 0016 **Responses API default wire**, 0018 **provider setup stored keys**, 0152 **native HTTP only**, 0153 **environment keys and OAuth credentials**: named profiles configure URL, env-key name, model, wire; never persist key values. | Implemented; `refs/tny/docs/architecture.md:9-13,77-93`. |
| SSH shadows workspace tools | 0022 **SSH execution boundary**, 0040 **SSH AGENTS.md**, 0136 **terminal background completion**: keep local control plane; route tool I/O via SSH; ensure instruction source and tool workspace agree. 0164 gates SSH ACP to pinned Claude 0.75.1 with built-ins disabled and strict MCP config. | Implemented for native tools and verified Claude ACP version; `refs/tny/docs/adr/0022-ssh-execution-boundary.md:16-76`; `refs/tny/docs/adr/0040-ssh-agents-md.md:24-59`; `refs/tny/docs/adr/0164-optional-acp-clients.md:74-79`. |
| Token efficiency / performance | 0004 **TTFT**, 0057 **shell-first native loop**, 0062 **tool profiles**, 0076 **structured prompt**, 0077 **prompt-cache routing**, 0078 **workspace shared cache**, 0150 **agent-first footprint**: benchmark exact workloads, preserve required context, exploit provider cache. | Implemented/benchmarked in stated historical workloads; `refs/tny/docs/adr/0077-openai-prompt-cache-routing.md:99-145`; `refs/tny/docs/adr/0078-workspace-shared-prompt-cache.md:44-113`; `refs/tny/docs/adr/0150-agent-first-harness-and-measured-footprint.md:25-74`. |
| Auto compaction | 0020 **ephemeral sessions**, 0028 **extension parity** (pre/post/failed hooks): eight completed turns trigger summary, four stay verbatim, disk transcript intact. This is fixed-threshold, not demonstrated intelligent compaction. | Implemented fixed policy; `refs/tny/docs/features/sessions.md:351-353`; `refs/tny/docs/adr/0028-extension-parity-contract.md:73-85`. |
| Semantic past-conversation search | `semantic_search` is lexical workspace search; memory is explicit named JSON data. No verified semantic index of saved conversations. | Planned for aim; `refs/tny/docs/features/mcp-and-skills.md:31-43,107`; `refs/tny/src/core/tools.c:137`. |
| Swarm / blackboard | 0087 **explicit subagent contract**, 0093 **durable native jobs**, 0143 **durable DAG**, 0145 **managed task workspaces**, 0146 **team mailbox**, 0147 **shared admission**, 0148 **team control**, 0149 **team boundaries**, 0156 **collective mode**, 0157 **file-defined swarms**, 0160 **contribution contracts**, 0161 **typed messages**, 0162 **review continuity**: reuse one scheduler, bounded capacity, explicit identities/receipts; no webhook job marketplace. | Core implemented per ADRs; webhook marketplace planned; `refs/tny/docs/adr/0156-collective-swarm-mode.md:5-51`; `refs/tny/docs/adr/0162-durable-swarm-review-continuity.md:20-56`. |
| Jev effort / model router | 0009 **reasoning effort**, 0010 **fast tier**, 0015 **default effort**, 0139 **subagent provider/model/effort**, 0163 **SDK effort**, 0165 **tnyjev**: effort is canonicalized per provider; Jev Noul/Choice is explicit CLI only. | Manual effort implemented; automatic Jev routing planned; `refs/tny/docs/adr/0009-reasoning-effort.md:28-60`; `refs/tny/docs/adr/0165-tnyjev-decision-engine.md:8-49`. |
| Dictation / speech | 0070 **provider-independent speech**, 0071 **ephemeral host playback**, 0079 **provider-independent dictation**: standalone services use Codex credentials, not selected conversation provider; recording result enters draft before submission. | Implemented service; `refs/tny/docs/adr/0079-provider-independent-dictation.md:13-47`; `refs/tny/docs/adr/0070-provider-independent-speech.md:14-42`. |
| Image gen/edit | 0074 **extensible image service**, 0075 **image CLI and agent tools**, 0084 **Codex image defaults**, 0088–0098 **dimensions, manifests, preview, exports**: adapter registry, explicit artifacts, bounded bytes, no accidental replay. | Implemented service with platform/account caveats; `refs/tny/docs/adr/0074-extensible-image-service.md:13-64`. |
| Web search | 0055 **web-search gating**, 0106 **native hosted search and DuckDuckGo**, 0109 **provider-independent Codex search**: inline hosted tool only for eligible Codex Responses; independent bounded search service for other callers; explicit override wins. | Implemented; `refs/tny/docs/adr/0106-native-web-search-and-duckduckgo.md:7-40`; `refs/tny/docs/adr/0109-provider-independent-codex-search.md:7-76`. |
| Skills and completion | 0049 **MCP background warmup**, 0056 **skill mention injection**, 0136 **repository agent discovery**, 0167 **workspace dashboard navigation**: cached catalog/mention context and TUI navigation; `@` file and `$` skill picker. | Implemented subset; cloud bucket completion planned; `refs/tny/docs/product.md:43-50`; `refs/tny/docs/features/mcp-and-skills.md:365-382`. |
| Code mode / saved programs | 0057 **shell-first verbs**, 0063 **in-process first-party intercept**, 0047 **scriptable workflow DAGs**, 0143 **durable DAG**: shell and SDK workflows are scriptable; no verified first-class GitHub-synced agent-authored program registry or Jev suggestions. | CLI/workflow implemented; program registry planned; `refs/tny/docs/adr/0057-shell-first-native-loop.md:29-65`; `refs/tny/docs/adr/0047-scriptable-workflow-dags.md:19-57`. |
| Hackability / extensions | 0027 **Python event hooks**, 0028 **extension parity**, 0030 **public events**, 0038 **custom tools**, 0045 **system prompt flag**, 0048 **task presets**: capability-scoped hooks and explicit prompt/tool additions; contracts distinguish supported/unavailable/unsupported. | Implemented Python/native subset; wasm plugin runtime planned; `refs/tny/docs/adr/0028-extension-parity-contract.md:117-178`; `refs/tny/docs/architecture.md:30-32,81-90`. |
| UI plugins / WASM | 0017 **wasm browser parity**, 0028 **extension parity**: same core builds for browser; Python hooks are trusted global host processes, not user-loadable WASM UI components. | WASM build exists; UI plugin API planned; `refs/tny/AGENTS.md:94-103`; `refs/tny/docs/architecture.md:81-85`. |
| Daemon + SQLite | 0053 **detached runners**, 0166 **global sessions**: durable per-session files and process/socket ownership survive frontend exit; no SQLite state daemon. | Different architecture; `refs/tny/docs/architecture.md:54-90`. |
| File memory | 0020 **ephemeral sessions** and explicit `memory` tool use `~/.tny/memories.json`; do not inject all memory on every prompt. Git/SQLite memory backend is not verified. | JSON store implemented, richer system planned; `refs/tny/docs/features/mcp-and-skills.md:107`; `refs/tny/docs/architecture.md:77-90`. |
| Markdown agents / MCP / workflows | 0048 **runtime task presets** and 0136 **repository-scoped agent discovery** provide markdown-like task/agent discovery; 0049/0051/0052/0068 cover stdio/Streamable HTTP MCP warmup/import/schema; 0047/0143 cover scripted/durable DAGs. No evidence for aim's exact `.agents/skills` or YAML-frontmatter agent contract. | Partial; `refs/tny/docs/architecture.md:81-90`; `refs/tny/docs/adr/0047-scriptable-workflow-dags.md:19-57`. |
| TUI / headless CLI / web | 0002 **TUI prewarm**, 0053 **runner isolation**, 0166 **global sessions**, 0167 **dashboard navigation**; CLI JSON and TUI share runtime. Browser wasm landing terminal is not a complete persistent web-chat service. | CLI/TUI implemented; web chat planned; `refs/tny/AGENTS.md:39-49,101-103`. |
| Ephemeral / private | 0020 **ephemeral sessions**: in-process multi-turn; no local session/transcript/results persistence and no resume/import, provider no-store where defined. Browser private UI mode is not established. | CLI/TUI ephemeral implemented; web private planned; `refs/tny/docs/adr/0020-ephemeral-sessions.md:21-77`. |
| Recursive self-improvement | 0153 **bounded instruction evolution**, 0154 **default automatic workflow learning**, 0157–0162 **purposeful collaboration**: learn from typed recovery evidence within strict limits; no unbounded self-modifying agent. | Bounded learning implemented, broader vision planned; `refs/tny/docs/architecture.md:3-7`; `refs/tny/docs/adr/0154-default-automatic-workflow-learning.md:21-94`. |

### Priority decisions: transferable mechanics and amendments

**Codex and logins (0019, 0065, 0066). FACT:** ADR 0019 originally used app-server `account/login/start` and a
host-owned OAuth callback, and treated Claude/Grok as builtin OpenAI-compatible profiles whose explicit named user
profile can shadow the builtin. Codex login decision 1 and later CLI-login decision were superseded. ADR 0065
replaced `TNY_BK_CODEX` with a `codex` profile at `https://chatgpt.com/backend-api/codex`, using `POST /responses`,
bearer token, `chatgpt-account-id`, `OpenAI-Beta: responses=v1`, and
`{model,instructions,input:[...],stream:true,store:false}`. Its reason is ownership: app-server meant a second loop,
advisory permission, no SSH and missing tool/extension parity, protocol/process drift.
`refs/tny/docs/adr/0019-subscription-logins-claude-grok.md:3-11,22-29,45-85`;
`refs/tny/docs/adr/0065-codex-chatgpt-responses-backend.md:16-64`.

**FACT:** ADR 0066 then implemented native browser PKCE and device code. Credential precedence is
`--chatgpt-token`/account ID, `CHATGPT_ACCESS_TOKEN`/account ID, `~/.tny/codex-auth.json`, `$CODEX_HOME/auth.json`;
only winning file sources refresh, explicit flag/env tokens do not. Login uses 127.0.0.1:1455 (1457 fallback), state
verification, code exchange and a 15-minute device flow. Own store is private 0600 atomic JSON; logout deletes only
tny's store. These are current source contracts, but the ChatGPT backend/OAuth wire is an external compatibility
risk for aim. `refs/tny/docs/adr/0066-native-chatgpt-login-and-credential-sources.md:30-53,55-109`.

**SSH (0022, 0040, 0164). FACT:** The original whole-program remote exec required installing tny remotely and was
replaced. A ControlMaster socket (`~/.tny/ssh/%C`, 0700, `ControlPersist=600`) is established before TUI entry; tool
calls use `BatchMode=yes`. Remote commands run POSIX `sh` under resolved remote cwd, file bytes on stdin, atomic
temp→rename writes; no disabled host-key checking. Native workspace tools route remotely, while
memory/skills/subagents/MCP/web and questions stay local. Remote `AGENTS.md` or `CLAUDE.md` is loaded once at
connection; local launch ancestors are omitted, local global user policy is labeled. A tool-workspace path and
instruction source must describe the same tree. ADR 0022's host-loop refusal was amended by 0164 for verified Claude
ACP 0.75.1: empty built-in tools and settingSources, strict MCP config, scratch cwd; other versions fail before
session creation. `refs/tny/docs/adr/0164-optional-acp-clients.md:74-79`;
`refs/tny/docs/adr/0022-ssh-execution-boundary.md:1-81`; `refs/tny/docs/adr/0040-ssh-agents-md.md:9-59`.

**Tool surface and authority (0057–0062). FACT:** ADR 0057 proposes `all | terminal+edit | terminal` tool profiles,
retaining typed file tools and CLI verbs rather than deleting them. Its frozen 26-task, three-arm pilot is N=1 per
task, so a default change was explicitly deferred. ADR 0062 enforces the selected profile both in schema
advertisement and dispatch. ADR 0058 gives runner socket clients `owner`, `observer`, or `tool` roles: only an owner
can turn/steer/cancel/answer; a tool can ask_user/image_attach; observer can detach. Nested wait pumps control only,
avoiding backend reentry. `refs/tny/docs/adr/0057-shell-first-native-loop.md:29-79`;
`refs/tny/docs/adr/0062-native-tool-profiles-advertise-and-enforce.md:19-66`;
`refs/tny/docs/adr/0058-session-control-channel-roles-and-tool-ops.md:33-116`.

**FACT:** ADR 0059's tokenizer recognizes shell metacharacters, env prefixes, unbalanced quotes, truncation and
exec-capable options; complex commands cannot receive heuristic auto-approval or broad session grants. The tokenizer
is explicitly a UX classifier; ADR 0060 wraps local terminal children with macOS Seatbelt or Linux bubblewrap for
filesystem authority. These do not cover MCP, built-in file tools, extension code or external ACP built-ins. Default
`yolo` means opted-in `ask/auto` checks do not silently redefine user policy.
`refs/tny/docs/adr/0059-permission-tokeniser-metacharacters-fail-closed.md:7-75`;
`refs/tny/docs/adr/0060-os-sandbox-seatbelt-and-bubblewrap.md:22-58`; `refs/tny/AGENTS.md:39-46`.

**Steering, effort, and start (0011, 0009, 0004). FACT:** Mid-turn text steers only where a backend owns a safe
steering mechanism; otherwise it queues; rejection must return the text rather than lose it. Canonical effort levels
map to provider-specific wire values, avoiding a UI tied to one model. ADR 0004's Codex host registry speed path is
retired by 0065; treat its old TTFT data as historical, not a reason to recreate app-server plumbing.
`refs/tny/docs/adr/0011-mid-turn-input-steer-or-queue.md:1-72`; `refs/tny/docs/adr/0009-reasoning-effort.md:28-60`;
`refs/tny/docs/adr/0004-time-to-first-token.md:1-55`.

**Media and search (0070, 0074, 0079, 0106, 0109). FACT:** Speech uses a small adapter table and Codex OAuth
independently of active chat; dictation records into an editable composer draft via `/dictate`/Ctrl-R and CLI. Image
service has adapter registry, explicit output, bounded input/output, atomic artifact publication, cancellation, and
no automatic retry of paid generation. Codex hosted `web_search` items are provider-owned and normalized as tool
start/end without dispatching a local function. ADR 0109 amends 0106: explicit command then URL overrides win;
otherwise a usable ChatGPT subscription drives independent bounded search, and only *absence* of login falls back to
DuckDuckGo. Search request sends query plus fixed instructions, no conversation transcript.
`refs/tny/docs/adr/0070-provider-independent-speech.md:14-42`;
`refs/tny/docs/adr/0079-provider-independent-dictation.md:13-47`;
`refs/tny/docs/adr/0074-extensible-image-service.md:13-56`;
`refs/tny/docs/adr/0106-native-web-search-and-duckduckgo.md:7-40`;
`refs/tny/docs/adr/0109-provider-independent-codex-search.md:7-76`.

**Extensions and events (0028, 0030). FACT:** Extension parity is by named capability and provider state
(`supported`, `unsupported`, `unavailable`), not an undifferentiated promise. The event sequence is prompt
fold→turn→message/tool lifecycle→settlement; listeners have deterministic ordering and action precedence.
Cancellation/stop and explicit deny outrank rewrites; deny remains sticky through timeout/restart. Hook failures are
bounded diagnostics and fail open, while an already accepted deny stays closed. Secret-bearing provider
bodies/headers are excluded from hooks.
`refs/tny/docs/adr/0028-extension-parity-contract.md:61-114,117-178,199-248`.

**Workflows and durability (0047, 0143, 0146). FACT:** ADR 0047 provides bounded dependency DAGs in shell, Python
and TypeScript, with direct-output fan-in and blocked descendants, but each SDK originally owned its own
runtime/session. ADR 0143 moves durable execution onto native jobs; one authority owns identities, attempts,
admission, results and recovery. Mailboxes key run to job ID and member to item/attempt, authenticate sender from
private capability or lead ownership rather than model-supplied IDs, and use queued→delivered→acked transitions.
Exact retry reconciles; changed content or endpoint conflicts. Limits are 16 KiB payload, 64 outstanding/recipient,
256 retained/run; no eviction or exactly-once model reasoning claim.
`refs/tny/docs/adr/0047-scriptable-workflow-dags.md:19-57`; `refs/tny/docs/adr/0143-durable-dag-over-jobs.md:5-79`;
`refs/tny/docs/adr/0146-durable-team-mailbox.md:15-79`.

**Swarm refinement (0156, 0157, 0160–0162). FACT:** Collective mode (`--swarm[=n]`, 1–16) is opt-in over the same
job and mailbox; recipient set is atomically captured on publication, and filesystem watches wake waits rather than
polling the model. A versioned `schemas/swarm.schema.json` file supplies names, purposes and bounded nested groups;
nested coordinators flatten into one DAG so there is one admission/authority domain. Resume uses canonical saved
snapshot plus digest, never mutable source reread. Version 2 adds deliverable, acceptance, dependencies and
workspace policy; predecessor result integrity must pass before a dependent starts, but acceptance criteria remain
review inputs, not proof. `swarm_message` maps names to current attempt and sends a typed
`{version,kind,topic,body}` envelope; it does not invent a broker. Review packets retain bounded evidence/claims
separately and do not execute checks or confer acceptance. `refs/tny/docs/adr/0156-collective-swarm-mode.md:5-64`;
`refs/tny/docs/adr/0157-purposeful-file-defined-swarms.md:18-74`;
`refs/tny/docs/adr/0160-swarm-contribution-contracts.md:25-101`;
`refs/tny/docs/adr/0161-typed-swarm-messages.md:18-79`;
`refs/tny/docs/adr/0162-durable-swarm-review-continuity.md:20-56`.

**Instruction learning (0153, 0154). FACT:** ADR 0153 permits bounded, evidence-gated instruction evolution as an
optional controller. ADR 0154 enables automatic learning from typed native tool outcomes by default, constrained to
future prompts and without changing permission policy or task snapshots. The model's prose alone is not execution
evidence. This is intentionally narrower than recursive harness self-modification.
`refs/tny/docs/adr/0153-bounded-instruction-evolution.md:27-67`;
`refs/tny/docs/adr/0154-default-automatic-workflow-learning.md:21-94`; `refs/tny/docs/architecture.md:3-7`.

**Cache and ephemeral (0020, 0076–0078, 0082). FACT:** Ephemeral sessions retain process-local multi-turn state
without saved conversation artifacts; no-store applies where protocol supports it, and external provider retention
is outside this guarantee. Structured prompt keeps stable core instructions separate from dynamic environment. ADR
0077 adds session/turn cache routing, complete usage accounting including nullable cached token data; ADR 0078
groups routing by workspace/tool profile/SSH target while preserving distinct transcripts and actual prefix
matching. Its historical 24 fresh-task comparison reports 78.1% cached fraction versus 3.9% prior tny and 30.3%
Codex; it explicitly says that result does not establish superior warm continuous-conversation hits. ADR 0082 runs
prompt optimization in a separate ephemeral, read-only tool context and requires user submission of the returned
draft. `refs/tny/docs/adr/0020-ephemeral-sessions.md:21-77`;
`refs/tny/docs/adr/0076-structured-system-prompt.md:14-41`;
`refs/tny/docs/adr/0077-openai-prompt-cache-routing.md:29-66,99-168`;
`refs/tny/docs/adr/0078-workspace-shared-prompt-cache.md:6-26,44-113`;
`refs/tny/docs/adr/0082-prompt-optimisation.md:11-46`.

**Agent defaults and ACP (0001, 0150, 0159, 0056, 0136, 0029, 0164). FACT:** ADR 0001 sets yolo mode for all agents;
ADR 0159 supersedes earlier read-only worker/reviewer defaults and makes shared writable the default, with explicit
read-only ceilings. ADR 0150 removes fixed binary limits and competitor-size goals. Skill mentions are injected into
the user turn, not stable system prompt, and deduplicated while still in verbatim context. Repo-scoped dashboard
discovery finds saved agents across workspaces. ACP profiles are namespaced; optional ACP clients use an
owning-runtime MCP bridge, but an external agent's built-in tools remain outside tny's enforcement unless the
verified adapter disables/reroutes them. `refs/tny/docs/adr/0001-run-all-agents-in-yolo-mode.md:23-40`;
`refs/tny/docs/adr/0159-yolo-agent-defaults.md:1-35`;
`refs/tny/docs/adr/0150-agent-first-harness-and-measured-footprint.md:25-74`;
`refs/tny/docs/adr/0056-skill-mention-injection.md:33-86`;
`refs/tny/docs/adr/0136-repository-scoped-agent-discovery.md:14-35`;
`refs/tny/docs/adr/0029-named-acp-agent-profiles.md:21-66`; `refs/tny/docs/adr/0164-optional-acp-clients.md:5-88`.

### Jev: exact present contract versus future router

**FACT:** ADR 0165's phrase “later reasoning, model-routing and memory/reaction uses” is an explicit future
boundary. The only production call found for `tnyjev_evaluate` is the standalone CLI at `src/cli/cmd_jev.c:170`;
there is no auto effort/model policy to copy into aim. `tny score` is Jev **Noul** P(yes), whereas Jev's own Score
is ordinal and not implemented. `tny choose` is **Choice**, returning a chosen route plus probability distribution
and confidence. `refs/tny/docs/adr/0165-tnyjev-decision-engine.md:8-35`; `refs/tny/src/core/tnyjev.c:219-229`;
`refs/tny/src/cli/cmd_jev.c:170`.

**FACT:** Score question is supplied by caller, e.g. `Does this task need extended reasoning?`; no embedded
threshold. Choose's question defaults to `Which option best matches the state?`, or caller may supply `Which model
class best fits the task?`; example route keys `fast` and `reasoning` are illustrative, not an embedded policy.
State is a required UTF-8 text or structured JSON string/object/array via flag or stdin. Choice criteria are a JSON
object of 1–255 unique nonempty route keys with string/object/array/null descriptions; JSON numeric/boolean state
and descriptions are rejected. `refs/tny/docs/tnyjev.md:41-115`; `refs/tny/src/cli/cmd_jev.c:96-108,123-168`.

**FACT, exact request shape:** encoder builds
`{"model":...,"state":...,"questions":{"decision":{"type":"noul"|"choice","instructions":...,"criteria":{...}}}}`;
criteria appears only for Choice. It validates model/instructions, keys, request bound and response before
returning. `refs/tny/src/core/tnyjev.c:50-86`. The decisive code is:

```c
buf_appends(body, ",\"questions\":{\"decision\":{\"type\":");
jescape(body, r->kind == TNYJEV_SCORE ? "noul" : "choice");
```

**FACT:** Decoder requires returned model and unsigned input/output token usage. Noul probability must be finite in
[0,1]. Choice must include every route exactly once, sum to 1 within `1e-5`, choose an existing route with maximal
probability (ties allowed), and return confidence in [0,1]; invalid responses return protocol failure, never default
route. `refs/tny/src/core/tnyjev.c:110-175`.

```c
ok = fabs(sum - 1.0) <= 0.00001;
if (ok) ok = parsed.value.choose.probabilities[selected] + 1e-9 >= highest;
```

**FACT:** Default URL is `/v1/systemone`, default model `jev-latest`; `TYPESAFE_API_KEY` is CLI-owned and
independent of conversation credentials. Request/response max 1 MiB, default response timeout 60 seconds, maximum
300 seconds, separate transport connect/write deadlines. No retry, redirect, route execution or automatic fallback.
Mock tests establish protocol behavior, not latency, price, quality, live entitlement or CORS.
`refs/tny/src/core/tnyjev.h:15-18,42-89`; `refs/tny/docs/tnyjev.md:12-39,109-131`;
`refs/tny/docs/adr/0165-tnyjev-decision-engine.md:31-49`.

### Public event schema and extension distinction

**FACT:** `sdk/schema/events.json` v1 has fixed IDs 0–13: `text_delta`, `thinking`, `tool_start`, `tool_end`,
`permission_request`, `plan`, `usage`, `turn_end`, `error`, `status`, `steer_rejected`, `custom_message`,
`user_message`, `tool_progress`. Common envelope is `schema_version`, session-local `sequence`, monotonic
`timestamp_ms`, `provider`, `session_id`, `turn_id`, `type`. Tool events carry name/id/detail/success; usage carries
input/output/context/cost optionality; error carries text/code. Unknown events are preserved, unknown fields
ignored, optional additions minor-compatible. `refs/tny/sdk/schema/events.json:1-156`;
`refs/tny/docs/adr/0030-public-event-schema.md:14-60`.

**FACT:** The extension hook vocabulary additionally includes session/agent lifecycle, prompt submission, message
start/update/end, compaction lifecycle, model/effort/instruction/workspace changes, subagent, pre/post/batch tool
hooks and redacted provider request/response. That is not the same as the 14-event SDK schema. The generator checks
C/TypeScript/Python schema parity; C event view has borrowed lifetimes and append-only sized layout.
`refs/tny/docs/adr/0028-extension-parity-contract.md:61-95`; `refs/tny/docs/adr/0030-public-event-schema.md:32-77`.

### Lessons, pitfalls, and verification culture

- **FACT — supersession must be explicit:** 0019 app-server login → 0065 direct ChatGPT Responses → 0066 native
  login; 0106's DDG local-search default → 0109 independent Codex search; 0120/0121 fixed binary caps → 0150
  measured footprint; 0157 read-only assumptions → 0159 writable default. The ADR index has historical summaries
  that can lag later amendments, so read source ADR plus superseding ADR.
  `refs/tny/docs/adr/0019-subscription-logins-claude-grok.md:3-11`;
  `refs/tny/docs/adr/0109-provider-independent-codex-search.md:1-15`;
  `refs/tny/docs/adr/0150-agent-first-harness-and-measured-footprint.md:3-11`;
  `refs/tny/docs/adr/0159-yolo-agent-defaults.md:1-5`.
- **FACT — duplicate ADR IDs:** 0030, 0045, 0087, 0114–0118, 0136, 0153, 0154 and 0166 each have multiple slugs;
  automation that indexes by number alone will overwrite decisions.
  `refs/tny/docs/adr/0030-public-event-schema.md:1`; `refs/tny/docs/adr/0030-settings-schema-and-acp-map.md:1`;
  `refs/tny/docs/adr/0136-preserve-optional-tool-arguments-on-responses.md:1`;
  `refs/tny/docs/adr/0136-repository-scoped-agent-discovery.md:1`.
- **FACT — protocol/ownership hazards:** app-server drift drove 0065; shell prefix auto-allow was unsafe and
  prompted 0059; an interactive child asking its parent would deadlock without 0058's bounded control pump; a
  separate swarm scheduler would duplicate authority and create recovery races.
  `refs/tny/docs/adr/0065-codex-chatgpt-responses-backend.md:24-36`;
  `refs/tny/docs/adr/0059-permission-tokeniser-metacharacters-fail-closed.md:7-29`;
  `refs/tny/docs/adr/0058-session-control-channel-roles-and-tool-ops.md:15-31`;
  `refs/tny/docs/adr/0146-durable-team-mailbox.md:8-13`.
- **FACT — strict observation boundary:** tny's handoff says old worker/dirty-tree passes are not final
  combined-tree proof; preserve active processes/worktrees and bind evidence to exact source. It warns against
  treating PID existence as completion or a summary/hash as acceptance. `refs/tny/HANDOFF.md:42-90`;
  `refs/tny/AGENTS.md:64-92`; `refs/tny/docs/adr/0160-swarm-contribution-contracts.md:67-82`.
- **FACT — project conventions:** AGENTS.md and symlinked CLAUDE.md require decision ADRs, docs updates, no secret
  commits, startup without provider I/O, one event loop, user-selected yolo/writable defaults. `mise install`, `make
  test`, `make quality`, `make leaks`, strict gcc/clang CI, mutation tests for subtle boundaries, and before/after
  TTFT evidence are explicit gates. Local mocks use synthetic credentials; live inference requires explicit
  authorization. `refs/tny/AGENTS.md:1-5,21-49,64-74`.
- **RECOMMENDATION:** In Rust, represent session states, mailbox receipt states, attempt identities, capability
  scopes, path locality (`LocalWorkspace`/`RemoteWorkspace`), and authenticated principals as distinct types. Prove
  legal transitions, monotonic sequence/ack, bounded capacity, no authority escalation, and exactly one committed
  terminal result with Verus. A proof cannot establish external OAuth stability, live entitlement, provider tool
  behavior, model quality or filesystem power-loss durability; keep mock, fault, SSH, hosted and measured
  performance gates. Basis: `refs/tny/docs/adr/0146-durable-team-mailbox.md:37-88`;
  `refs/tny/docs/adr/0162-durable-swarm-review-continuity.md:20-56`;
  `refs/tny/docs/adr/0066-native-chatgpt-login-and-credential-sources.md:26-53`.
- **C/platform-specific, not aim architecture:** C11 ABI struct-size shims, private C++ migration, GCC/MSYS/Windows
  LTO and linker workarounds, Linux aarch64 binary-size cliffs, and `tny_poll`/Asyncify source-seam mechanics should
  be translated only where the same Rust target actually needs them. The underlying ownership and verification
  failures remain relevant. `refs/tny/docs/adr/0030-public-event-schema.md:32-62`;
  `refs/tny/docs/adr/0150-agent-first-harness-and-measured-footprint.md:3-11`; `refs/tny/AGENTS.md:94-99`.

## Implications for aim

1. **RECOMMENDATION:** Define a transport-neutral harness tool protocol and independent daemon before binding Codex,
   Claude ACP or generic HTTP agents. Make SSH a workspace transport in that harness protocol so *every* eligible
   file/shell call, including a Claude adapter call, has the same remote path and authority semantics. Tradeoff: a
   verified ACP tool bridge may need to disable unsupported external built-ins or reject SSH mode until it can
   shadow them. Basis: `refs/tny/docs/adr/0022-ssh-execution-boundary.md:59-66`;
   `refs/tny/docs/adr/0164-optional-acp-clients.md:5-43`.
2. **RECOMMENDATION:** Separate agent-event schema, extension lifecycle schema, and durable log schema while giving
   them stable IDs and one source of truth each. Preserve unknown events and publish capability truth per adapter.
   Tradeoff: versioning work up front, much less drift across TUI/web/CLI/SDK. Basis:
   `refs/tny/docs/adr/0030-public-event-schema.md:14-60`;
   `refs/tny/docs/adr/0028-extension-parity-contract.md:117-178`.
3. **RECOMMENDATION:** Carry over separate service adapters for image, dictation and search. Keep media/search
   requests bounded, cancellable, independently authenticated and explicitly reported; never silently switch service
   on an authentication or protocol error. Tradeoff: a small service registry and separate account diagnostics.
   Basis: `refs/tny/docs/adr/0074-extensible-image-service.md:13-56`;
   `refs/tny/docs/adr/0109-provider-independent-codex-search.md:7-40`.
4. **RECOMMENDATION:** Start Jev routing as a measured, typed optional decision pipeline: explicit candidate set,
   state schema, latency/usage logging, threshold policy, deterministic fallback, and offline comparison against
   static routing. Do not infer that `tny choose` already solved auto routing or that a 60-second deadline meets
   aim's TTFT goals. Tradeoff: routing may cost an extra request; cache/stability and result quality need
   workload-level measurement. Basis: `refs/tny/docs/adr/0165-tnyjev-decision-engine.md:34-49`;
   `refs/tny/docs/tnyjev.md:41-96`.
5. **RECOMMENDATION:** For swarms, persist one scheduler/authority ledger, attempt-fenced receipts, canonical
   manifest snapshots, bounded queues, explicit evidence references and operator review. A webhook can notify on a
   committed transition, but delivery/retry must reconcile against durable state. Tradeoff: quotas and idempotency
   semantics become visible API; they prevent silent replay or lost work. Basis:
   `refs/tny/docs/adr/0146-durable-team-mailbox.md:15-79`;
   `refs/tny/docs/adr/0157-purposeful-file-defined-swarms.md:18-74`;
   `refs/tny/docs/adr/0162-durable-swarm-review-continuity.md:20-56`.
6. **RECOMMENDATION:** Design compaction and conversation search around a lossless durable transcript plus a derived
   model view and retrieval index. Evaluate answer retention and privacy on long-running tasks before calling it
   intelligent. Tradeoff: SQLite/index maintenance and private-session exclusion need explicit transactional rules.
   Basis: `refs/tny/docs/features/sessions.md:351-353`; `refs/tny/docs/adr/0020-ephemeral-sessions.md:21-77`.
7. **RECOMMENDATION:** Retain tny's yolo and writable defaults only as an explicit user-level policy, then make
   capability ceilings and tool scopes typed and auditable across all transports. Rust/Verus should prove policy
   monotonicity, while OS sandboxing and integration tests enforce actual child behavior. Basis:
   `refs/tny/docs/adr/0001-run-all-agents-in-yolo-mode.md:23-40`;
   `refs/tny/docs/adr/0159-yolo-agent-defaults.md:1-35`;
   `refs/tny/docs/adr/0059-permission-tokeniser-metacharacters-fail-closed.md:31-75`.
8. **RECOMMENDATION:** Benchmark aim against Codex/pi/unreal on identical tasks, prompt/tool access, provider/model,
   cold/warm state, answer correctness, uncached tokens, TTFT and end-to-end time. Keep binary footprint measured
   without using a fixed size ceiling. Tradeoff: repeatable benchmarks cost time but prevent optimizing the wrong
   metric. Basis: `refs/tny/docs/adr/0077-openai-prompt-cache-routing.md:68-168`;
   `refs/tny/docs/adr/0078-workspace-shared-prompt-cache.md:44-113`;
   `refs/tny/docs/adr/0150-agent-first-harness-and-measured-footprint.md:25-74`.

## Open questions for the user

- Should aim's Claude integration require that *all* file/shell operations cross aim's verified harness tool bridge,
  even if that disables some Claude Code built-ins? This determines whether SSH shadowing can be promised uniformly.
  `refs/tny/docs/adr/0164-optional-acp-clients.md:5-43`.
- Does “yolo by default” from tny remain an explicit aim product default across remote HTTP/SSH and third-party
  agents, or should aim inherit the user's configured policy per workspace? This changes the authorization contract.
  `refs/tny/docs/adr/0001-run-all-agents-in-yolo-mode.md:23-40`;
  `refs/tny/docs/adr/0159-yolo-agent-defaults.md:1-35`.

<!-- REPORT COMPLETE -->
