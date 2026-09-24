# Swarm coordination: durable blackboard, federation and delivery

Research date: 2026-09-25. `refs/<repo>/...:line` citations refer to the supplied read-only snapshots. External links cite primary protocol specifications, project documentation or papers. The design recommendations are proposals for aim, not claims about working aim code. No authenticated endpoint or paid model was called.

## TL;DR

- **FACT:** A2A 1.0 (latest released specification in this review) supplies remote agent discovery, tasks, polling, SSE, push callbacks, cancellation and multi-turn input. It is governed under the Linux Foundation's Agentic AI Foundation. [A2A specification](https://a2a-protocol.org/latest/specification/); [A2A governance announcement](https://a2a-protocol.org/latest/blog/2026/08/27/a-new-chapter-for-a2a-joining-the-agentic-ai-foundation/).
- **FACT:** A2A push uses configured callback authentication and recipient verification; it does not mandate Standard Webhooks body signatures. Push and SSE are delivery hints: `GetTask` is the reconciliation read. [A2A push security](https://a2a-protocol.org/latest/specification/#132-push-notification-security); [A2A update delivery](https://a2a-protocol.org/latest/specification/).
- **FACT:** MCP Tasks moved from experimental core to the `io.modelcontextprotocol/tasks` extension in the 2026-07-28 release. They make long-running *tool calls* asynchronous; A2A is a better agent-to-agent federation contract. [MCP Tasks release](https://blog.modelcontextprotocol.io/posts/2026-07-28/); [Tasks specification](https://tasks.extensions.modelcontextprotocol.io/specification/draft/tasks).
- **FACT:** tny's first-party decisions already establish one lifecycle authority, durable DAG jobs, fenced attempts, bounded typed mailboxes, explicit worktree ownership and separate review/acceptance. `refs/tny/docs/adr/0143-durable-dag-over-jobs.md:5-25`; `refs/tny/docs/adr/0146-durable-team-mailbox.md:15-65`; `refs/tny/docs/adr/0160-swarm-contribution-contracts.md:25-81`.
- **FACT:** tny explicitly rejects exactly-once reasoning/external effects, PID-based lease reclamation, stale-attempt message reuse, implicit merge and execution-success-as-acceptance. `refs/tny/docs/adr/0093-durable-native-jobs-and-verified-retry.md:7-17`; `refs/tny/docs/adr/0146-durable-team-mailbox.md:37-79`; `refs/tny/docs/adr/0145-managed-task-workspaces.md:42-53`.
- **FACT:** Claude agent teams already offer lead/teammate assignment, claims, shared tasks and peer messaging, but are experimental and have coordination/token cost. Codex has spawn/wait/close and a TUI picker. [Claude agent teams](https://code.claude.com/docs/en/agent-teams); `refs/codex/codex-rs/core/src/tools/handlers/multi_agents/spawn.rs:52-125`.
- **FACT:** Blackboard papers report task-specific accuracy and token gains, including a 2025 controlled-ablation token reduction and a 2026 data-discovery gain; neither establishes durable correctness for code changes. [Han & Zhang 2025](https://arxiv.org/html/2507.01701v1); [Salemi et al. 2026](https://arxiv.org/abs/2510.01285).
- **RECOMMENDATION:** Make the SQLite-backed job/attempt ledger aim's sole execution authority. Treat messages, claims, events, notifications and A2A views as projections of that ledger, never independent schedulers.
- **RECOMMENDATION:** Use deterministic eligibility and admission first; add Jev `Choice` only to rank eligible agents when the decision is ambiguous. A probability is not a permission or capacity grant.
- **RECOMMENDATION:** Ship local post/claim/heartbeat/complete/fail/cancel/watch/poll, bounded inbox and evidence packets in the MVP; add webhook and A2A federation after local recovery and idempotency are proven.

## Findings

### 1. A2A as a federation protocol

- **FACT:** The current A2A specification labels protocol version `1.0` (release 1.0.0) and defines equivalent JSON-RPC, gRPC and HTTP+JSON bindings. Google donated the project into Linux Foundation governance; the 2026-08-27 announcement says it joined the LF-directed Agentic AI Foundation as a Growth Stage project. [A2A specification](https://a2a-protocol.org/latest/specification/); [governance announcement](https://a2a-protocol.org/latest/blog/2026/08/27/a-new-chapter-for-a2a-joining-the-agentic-ai-foundation/).
- **FACT:** The public Agent Card is discoverable at `/.well-known/agent-card.json`; it describes provider/name/skills, supported interfaces with URL/binding/protocol version, input/output modes, capabilities (`streaming`, `pushNotifications`, extensions, extended card) and `securitySchemes`/requirements. An authenticated extended card can expose more information. [Agent Card specification](https://a2a-protocol.org/latest/specification/#8-agent-discovery-the-agent-card).
- **FACT:** A2A `SendMessage` may return a direct `Message` or a server-created `Task`. A `Task` has ID/context ID, status, artifacts and optional history. Message parts support text, raw/file bytes or URI, and structured data; artifacts have stable IDs and parts. [A2A data model](https://a2a-protocol.org/latest/specification/#4-protocol-data-model).
- **FACT:** Task states are `submitted`, `working`, `completed`, `failed`, `canceled`, `input_required`, `rejected`, `auth_required` and `unspecified`. `completed`, `failed`, `canceled`, `rejected` are terminal; `input_required` and `auth_required` suspend for another interaction. [A2A TaskState](https://a2a-protocol.org/latest/specification/#413-taskstate).
- **FACT:** `GetTask` is a polling/reconciliation read; `ListTasks` filters and paginates; `CancelTask` requests cancellation but an agent may already have finished or be unable to cancel. `SendStreamingMessage` and `SubscribeToTask` require the advertised streaming capability; HTTP delivers an initial task/message and status/artifact updates through SSE until a terminal update. [A2A operations](https://a2a-protocol.org/latest/specification/#3-a2a-protocol-operations).
- **FACT:** REST mapping includes `POST /message:send`, `POST /message:stream`, `GET /tasks/{id}`, `GET /tasks`, `POST /tasks/{id}:cancel`, and `POST /tasks/{id}:subscribe`; push config operations live below `/tasks/{id}/pushNotificationConfigs`. JSON-RPC/gRPC operation names include `SendMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, and push-config CRUD. [A2A method mapping](https://a2a-protocol.org/latest/specification/#53-method-mapping-reference).
- **FACT:** The push config contains callback URL and authentication information. Agent sends task stream event payloads to the callback with configured credentials; receiver must validate authenticity, should match expected task ID, respond 2xx and idempotently handle duplicate events. Server should validate callback URLs against SSRF, set timeouts and retry transient delivery. This is credential-based callback authentication, not a required canonical-body signature. [A2A push security](https://a2a-protocol.org/latest/specification/#132-push-notification-security).
- **FACT:** A2A notes streamed updates may be missed after disconnect; consumers should fetch `GetTask` to reconcile critical state. Push is likewise an asynchronous notification path, not a substitute for authoritative task state. [A2A streaming/update delivery](https://a2a-protocol.org/latest/specification/).
- **FACT:** A2A extensions have URI identifiers declared in Agent Cards and opted into through the `A2A-Extensions` header; incompatible changes get a new URI. TSC governance controls standard extension graduation. [A2A extensions](https://a2a-protocol.org/latest/specification/#46-extensions); [extension governance](https://a2a-protocol.org/latest/topics/extension-and-binding-governance/).
- **FACT:** The official Rust repository advertises A2A v1 support for REST, JSON-RPC, gRPC and SSE. Crates.io queries on 2026-09-25 reported `a2a-lf` 0.3.1, `a2a-client-lf` 0.2.5, `a2a-server-lf` 0.4.4, `a2a-grpc` 0.3.7 and `a2a-cli` 0.2.1. The uneven pre-1.0 crate versions mean API stability and exact interoperability require pinned integration tests. [Official Rust SDK](https://github.com/a2aproject/a2a-rs); [a2a-lf crate](https://crates.io/crates/a2a-lf); [a2a-server-lf crate](https://crates.io/crates/a2a-server-lf).
- **RECOMMENDATION:** Use A2A at the aim↔foreign-agent boundary. Map one externally visible A2A task to an aim job or a specified task attempt with a stable mapping table, and expose only authorized artifacts. Do not force aim's internal job schema into A2A's relatively small state/part model.
- **RECOMMENDATION:** A2A's `input_required`/`auth_required` should pause the external task and create an aim follow-up requirement; a webhook or SSE event alone must not unblock a dependency until `GetTask` confirms the authoritative state.

### 2. MCP Tasks overlap

- **FACT:** The 2026-07-28 MCP release moved Tasks from experimental core into the `io.modelcontextprotocol/tasks` extension; the published specification page still labels itself Draft. [MCP release](https://blog.modelcontextprotocol.io/posts/2026-07-28/); [Tasks specification](https://tasks.extensions.modelcontextprotocol.io/specification/draft/tasks).
- **FACT:** A client opts in per request. A server may return a `resultType:"task"` from a long-running `tools/call`; `tasks/get` polls, `tasks/update` answers an input request, and `tasks/cancel` is cooperative. Optional `notifications/tasks` over `subscriptions/listen` reduce polling. [MCP Tasks specification](https://tasks.extensions.modelcontextprotocol.io/specification/draft/tasks).
- **FACT:** Task IDs must be unguessable and authorization checked on every task operation; the extension intentionally omits `tasks/list` because a broad listing could reveal another caller's work. [MCP Tasks specification](https://tasks.extensions.modelcontextprotocol.io/specification/draft/tasks).
- **UNVERIFIED:** Numeric error-code examples differ between the extension overview and detailed draft. Do not hard-code those codes until an exact released extension version and test fixture are selected.
- **RECOMMENDATION:** Let aim's harness MCP server return an MCP Task for a long-running *tool operation* such as a remote build or search. Use A2A for a remote *agent* that owns a conversation, artifacts and multi-turn work. Both adapt to the same internal job ledger; neither should become the ledger.

### 3. Blackboard architectures and assignment research

- **FACT:** Han and Zhang's 2025 blackboard MAS uses shared/private blackboard spaces, a control unit that chooses participating agents from board state each round, and bounded stopping. Its reported ablation without the controller used about 18.83M versus 5.07M tokens on MMLU, 13.33M versus 2.98M on GPQA, and 13.86M versus 4.72M on MATH at similar accuracy in that setup. These are paper workloads, not aim predictions. [Paper](https://arxiv.org/html/2507.01701v1).
- **FACT:** Salemi et al.'s 2026 data-discovery blackboard has agents volunteer/respond to posted requests; across their task benchmarks they report 13–57% relative end-to-end success improvement and up to 9% relative F1 gain over baselines. The task domain and comparators matter. [Paper](https://arxiv.org/abs/2510.01285).
- **FACT:** Amayuelas et al. compare planner and per-step orchestration in CuisineWorld; explicit worker capabilities change task allocation, and the planner used concurrent capacity more efficiently in their tested setup. [Paper](https://arxiv.org/html/2504.02051v2).
- **FACT:** Agora's 2026 auction design scores task units on competence/cost/confidence and dependencies, with benchmark comparisons rather than a claim that raw agent self-confidence is reliable. [Paper](https://arxiv.org/html/2607.09600v2).
- **FACT:** MultiAgentBench is an interactive collaboration/competition benchmark, and scaling studies evaluate communication topologies. They are useful for testing swarm behavior but do not prove filesystem ownership, retry fences or webhook delivery. [MultiAgentBench](https://aclanthology.org/2025.acl-long.421/); [ICLR 2025 scaling paper](https://proceedings.iclr.cc/paper_files/paper/2025/hash/66a026c0d17040889b50f0dfa650e5e0-Abstract-Conference.html).
- **RECOMMENDATION:** Separate *selection policy* from *scheduling truth*. A control model, auction or Jev classifier can propose an agent and explain tradeoffs; a deterministic transaction must still check eligibility, capacity, lease, permission ceiling, dependency evidence and deadlines before launch.
- **RECOMMENDATION:** End collective work on an explicit acceptance rule, budget/deadline, or lead decision. “Agents reached consensus” is a claim requiring review, not a completion state.

### 4. Existing harness patterns

- **FACT:** Claude Code agent teams are experimental and enabled with `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`. A lead coordinates independently running teammates through a shared task list, assignment/self-claim and peer messages. Teammates have separate contexts, cost more tokens than subagents, and spawning is limited in noninteractive/SDK contexts. [Claude agent teams](https://code.claude.com/docs/en/agent-teams).
- **FACT:** Claude subagents are focused delegates with separate contexts whose result returns to the parent, unlike a team with shared tasks and peer messages. [Claude agent teams](https://code.claude.com/docs/en/agent-teams).
- **FACT:** Codex's `spawn_agent` handler enforces depth and resolves role/model/effort before spawn; `wait_agent` resolves IDs and bounded timeout; `close_agent` closes a local runtime agent. TUI has `/subagents` picker and switch rendering. `refs/codex/codex-rs/core/src/tools/handlers/multi_agents/spawn.rs:52-125`; `refs/codex/codex-rs/core/src/tools/handlers/multi_agents/wait.rs:55-103`; `refs/codex/codex-rs/core/src/tools/handlers/multi_agents/close_agent.rs:31-100`; `refs/codex/codex-rs/tui/src/multi_agents.rs:1-45`.
- **FACT:** OMP task agents discover native Markdown definitions, bounded recursion and per-agent model/thinking/tool settings; they can produce asynchronous task completion consumed by wait. `refs/oh-my-pi/docs/task-agent-discovery.md:25-47,138-169,211-297`.
- **FACT:** pi base is extension-oriented. Its agent loop can execute independent tool calls in one assistant message concurrently, but the supplied base does not establish a durable shared swarm ledger. `refs/pi/packages/coding-agent/docs/extensions.md:121-123`. **UNVERIFIED:** Installed third-party pi swarm extensions.
- **FACT:** unreal-agent assigns stable unique input IDs, deduplicates an inbox in memory, persists append-only session state, translates synchronous tool calls into serializable async operations, and atomically stores operation status through a swappable actor manager. `refs/unreal-agent/README.md:9-55`.
- **RECOMMENDATION:** An unreal-agent coordinator can submit an aim job as one durable external operation, then resume when an aim callback/poll supplies the result. Its operation manager is an integration point, not a second authority over aim's job attempts.
- **FACT:** OpenAI Agents SDK distinguishes `Agent.as_tool()` (manager retains control) from `handoff()` (specialist becomes active); handoff uses model-exposed tools and can filter input. Guardrails apply at different boundaries, so a handoff is not a durable job claim. [OpenAI Agents SDK multi-agent](https://openai.github.io/openai-agents-python/multi_agent/); [handoffs](https://openai.github.io/openai-agents-python/handoffs/).
- **FACT:** AutoGen documents RoundRobin, Selector, MagenticOne and Swarm teams, save/load state, termination and cancellation; its docs warn about parallel calls to stateful agent/team tools. [AutoGen teams](https://microsoft.github.io/autogen/dev/user-guide/agentchat-user-guide/tutorial/teams.html); [agents](https://microsoft.github.io/autogen/dev/user-guide/agentchat-user-guide/tutorial/agents.html).
- **FACT:** LangChain distinguishes subagents, handoffs, router and skills; a published illustrative three-domain comparison counts roughly 5 calls/9K tokens for parallel subagents/router versus 7+ calls/14K+ for sequential handoffs. This is an example, not a universal benchmark. [LangChain multi-agent patterns](https://docs.langchain.com/oss/python/langchain/multi-agent).
- **FACT:** CrewAI hierarchical process requires a manager model or agent and allows it to plan, delegate and validate unassigned tasks; sequential process is the simpler alternative. [CrewAI processes](https://docs.crewai.com/en/concepts/processes).

### 5. First-party tny lessons to preserve

- **FACT:** tny's durable DAG uses job ID as run, item index as task, and `(job,item,item_attempt)` as execution identity; job attempt is a retry generation. Dependencies are validated for duplicates, indexes and cycles before effects, and launch waits for integrity-checked successful predecessors. `refs/tny/docs/adr/0143-durable-dag-over-jobs.md:5-25`.
- **FACT:** Its retry validates immutable task definitions, direct dependency bindings, result/log hashes and clean Git revision; a failed/cancelled predecessor blocks descendants without provider spend. It never treats execution zero or hash match as accepted verification. `refs/tny/docs/adr/0143-durable-dag-over-jobs.md:45-77`.
- **FACT:** tny mailbox authenticates trusted caller identity separately from public message arguments. Stable message ID exact retry returns one receipt; changed payload/endpoint conflicts. Run-global sequence orders commits; queued, delivered, acked and retired are distinct. Old-attempt messages never become new-attempt context. `refs/tny/docs/adr/0146-durable-team-mailbox.md:15-52`.
- **FACT:** Mailbox bounds are 16-KiB payload, 64 outstanding per recipient, 256 retained per run, at most 16 messages/64 KiB per inbox read. Ack frees outstanding quota but retained IDs remain for dedup; exhaustion backpressures visibly. `refs/tny/docs/adr/0146-durable-team-mailbox.md:54-79`.
- **FACT:** Message insertion into an agent transcript happens only at a safe pre-request boundary, with stable ID persisted before mark-delivered. Unacked records replay and are deduplicated; message content remains untrusted user context. `refs/tny/docs/adr/0146-durable-team-mailbox.md:60-66`; `refs/tny/docs/adr/0149-native-team-boundaries.md:29-44`.
- **FACT:** Collective mode fans out atomically to a captured recipient set and uses directory-watch notifications followed by a durable snapshot; watcher events are hints and timeouts are not cancellations. It does not serialize a changing board into the system prompt. `refs/tny/docs/adr/0156-collective-swarm-mode.md:23-51`.
- **FACT:** File-defined purposeful swarms are validated and compiled into one bounded DAG, not recursive coordinators. The manifest is snapshotted for deterministic resume; current bounds include 16 participants/depth four. `refs/tny/docs/adr/0157-purposeful-file-defined-swarms.md:18-54`.
- **FACT:** Contribution contracts add deliverable, acceptance criteria, explicit dependency names and workspace policy. They inject bounded direct-predecessor evidence with artifact identity/hashes; no implicit group/coordinator dependency is inferred. `refs/tny/docs/adr/0160-swarm-contribution-contracts.md:25-81`.
- **FACT:** Typed named `swarm_message` is an adapter over the same mailbox, with canonical envelope, attempt-scoped deterministic ID and exact retry/conflict behavior. It does not add authority or automatic ack. `refs/tny/docs/adr/0161-typed-swarm-messages.md:18-64`.
- **FACT:** Review packets are bounded immutable observations of terminal successful contributions plus separate untrusted reviewer claims. They do not execute claimed checks, accept work or auto-merge. `refs/tny/docs/adr/0162-durable-swarm-review-continuity.md:20-43,56-68`.
- **FACT:** Managed worktrees derive private reservation/branch from run/task/attempt, never adopt unrelated existing work, record base/branch/tree identity, and require explicit integration into a clean quiescent launch branch. Conflict preserves both sides; cancellation never deletes user files. `refs/tny/docs/adr/0145-managed-task-workspaces.md:17-53`.
- **FACT:** tny shared admission treats a permit as one launch, not tokens/cost/provider calls. It does not reclaim capacity merely on lease expiry or missing PID; uncertain cleanup is an explicit hold. `refs/tny/docs/adr/0147-shared-admission.md:13-51`.
- **RECOMMENDATION:** Adapt these invariants to SQLite transactions in aim instead of copying tny's C/JSON storage. Keep a single scheduler and explicit separate verification/integration status.

### 6. Webhooks and durable notification semantics

- **FACT:** Standard Webhooks 1.0.0 defines `webhook-id`, `webhook-timestamp`, `webhook-signature`; the signed input is exact `id.timestamp.raw_payload` bytes. It specifies HMAC-SHA256 `v1` and Ed25519 `v1a`, freshness checks, constant-time verification and dedup by stable ID. [Standard Webhooks spec](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md).
- **FACT:** A2A push's configured callback auth is independent of this signature convention. Aim can support both: A2A-required authentication plus an aim/Standard-Webhooks signing profile negotiated for aim peers. [A2A push security](https://a2a-protocol.org/latest/specification/#132-push-notification-security); [Standard Webhooks spec](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md).
- **RECOMMENDATION:** The state transaction that changes a job must append an outbox row with stable event ID and monotonic job sequence in the same SQLite transaction. Delivery workers may send duplicate HTTP callbacks; receivers dedup by event ID and fetch current state by job ID/version.
- **RECOMMENDATION:** Use exponential backoff with jitter, per-attempt timeout, Retry-After when meaningful, a bounded retry horizon and a dead-letter state visible to operators. Never block job completion on HTTP availability; a failed notification stays in the outbox.
- **RECOMMENDATION:** Sign raw serialized bytes immediately before sending, attach timestamp and stable delivery ID, rotate per-subscriber secrets without logging them, enforce HTTPS and callback host/IP policy to prevent SSRF. Re-resolve/validate redirects and private-address targets.
- **RECOMMENDATION:** Poll and webhook share one truth: `GET job`/`watch from cursor`. A webhook means “something changed; reconcile at this version,” not “trust this as the only copy of result.” Local in-process/Unix-socket subscribers use the same event ID/sequence without HTTP signing.
- **RECOMMENDATION:** A slow subscriber may fall behind and get `cursor_expired`; return current snapshot plus a new cursor instead of pretending lossless infinite retention. Keep webhook dead letters separate from job/artifact retention.

### 7. Matching, budget and work isolation

- **FACT:** TypeSafe Jev `Choice::new` accepts ordered `(name,description)` candidates and `ChoiceAnswer` returns selected option, confidence and per-candidate probabilities. These are model outputs, not calibrated assignment guarantees by themselves. `refs/jevgrep/crates/typesafe-jev/src/question.rs:123-156`; `refs/jevgrep/crates/typesafe-jev/src/answer.rs:135-145`.
- **RECOMMENDATION:** Eligibility filter first: required skills/tools, model access, data locality, SSH environment, permission ceiling, concurrency slots, deadline and budget. Then compare eligible candidates by deterministic score/cost; invoke Jev Choice only when uncertainty could change the outcome, recording candidate descriptions and probabilities for audit.
- **RECOMMENDATION:** Support three allocation modes: lead assignment (predictable), worker claim among eligible posted jobs (low coordinator cost), and optional sealed bid/auction (useful for heterogeneous remote agents). The auction score should use observed quality/cost/latency, not an agent's unverified self-estimate.

Assignment comparison (**RECOMMENDATION**):

| Mechanism | Useful when | Failure mode | Required ledger check |
| --- | --- | --- | --- |
| Lead assigns named agent | Few known workers; accountable ownership | Lead bottleneck or stale capability knowledge | Candidate eligibility and current capacity. |
| First eligible claim | Homogeneous workers; plentiful independent jobs | Fast worker can monopolize; starvation | Fair queue, claim TTL and per-agent cap. |
| Capability-score dispatch | Heterogeneous models/tools/locality | Stale self-reported skills | Verified profile and recent observed outcomes. |
| Jev Choice ranking | Close alternatives where semantics matter | Uncalibrated probability or prompt sensitivity | Filter first; snapshot candidates and decision. |
| Contract-net bid | Remote agents with distinct price/deadlines | Strategic or unverifiable bids; auction cost | Bid deadline, capacity reservation and observed SLA. |

- **RECOMMENDATION:** A bid is an offer, never a claim. Run `post -> eligible invitations -> bounded bids -> deterministic score -> provisional assignment -> atomic claim`, then start only after the claimant proves identity and still meets the contract.
- **RECOMMENDATION:** If the auction deadline passes with no eligible bid, leave the job posted or fall back to the lead's declared policy. Do not let a model invent capacity or use a self-reported confidence as an acceptance score.
- **RECOMMENDATION:** Record matching features and rejected reasons without recording private prompts/credentials: required tool IDs, model family, environment, estimated cost, data locality, current queue and deadline. This makes dispatch reviewable.
- **RECOMMENDATION:** Prevent coordinator deadlock by reserving room for evidence-producing workers or flattening a bounded manifest into one scheduler, as tny ADR 0157 does. `refs/tny/docs/adr/0157-purposeful-file-defined-swarms.md:28-34,60-82`.
- **RECOMMENDATION:** A claim reserves execution for an agent/attempt with a lease and heartbeat. Lease expiry makes the attempt *suspect*, never automatically frees a still-running local process or authorizes duplicate external side effects. Requeue only after ownership/cleanup proof or a task's explicit retry policy.
- **RECOMMENDATION:** Separate `max_turns`, `max_tokens`, `max_cost`, wall deadline and shared run concurrency. Admission reserves a slot; usage accounting enforces soft/hard budget policy after each request; cancellation propagates to provider requests, tools, subagents and remote job adapters, recording uncertain cleanup honestly.
- **RECOMMENDATION:** Give each editing attempt a uniquely owned Git worktree and branch at a pinned base commit. Worker publishes commit/tree/diff and test evidence. Integrator serializes merges onto a recorded target HEAD; if target moves, revalidate/rebase under policy. Conflicts create a review state, not automatic overwrite.
- **RECOMMENDATION:** Keep read-only parallel jobs on shared checkout only when the tool layer enforces read-only. Worktrees isolate files but do not isolate credentials/processes or make edits accepted. A worker result must state source SHA, commands, exit codes, artifact hashes and gaps; an independent reviewer/operator records acceptance separately.

Crash and retry cases (**RECOMMENDATION**, extending tny's source-derived fences):

| Failure point | Persisted evidence | Recovery action |
| --- | --- | --- |
| Client times out after `post` commit | Idempotency key + job row | Return original job on retry. |
| Claim committed, process not started | Launch intent, no owned process | Reconcile under same attempt or fail before replacement. |
| Process started, acknowledgment lost | Owned process handle/attempt row | Observe existing worker; never launch another on blind retry. |
| Heartbeat stops | Last heartbeat plus process ownership | Mark suspect, inspect/reap; retain slot meanwhile. |
| Worker result file written, DB not committed | Content-hashed orphan artifact | Reconcile exact attempt or quarantine; do not accept by filename. |
| `complete` committed, webhook fails | Job event + outbox row | Retry delivery; polling already sees result. |
| Cancel races completion | Generation/version compare-and-swap | One terminal outcome; retain actual observed cleanup. |
| Integrator dies during merge | Integration intent, target HEAD, worker branch | Inspect Git state under repository lock; preserve both trees. |
| Worker attempt is retried | Old IDs/receipts retained | New attempt token; stale callbacks/messages denied. |

- **RECOMMENDATION:** Treat a lease as a liveness hint, not ownership proof. Local process handles, remote cancel acknowledgments, persisted operation IDs and tool cleanup receipts provide the evidence needed to release admission capacity.
- **RECOMMENDATION:** For remote agents, a disconnected A2A task is still running until `GetTask` or an explicit remote cancellation/reconciliation result says otherwise. If the remote cannot be reached, mark cleanup uncertain and keep the attempt fenced.

## Implications for aim

### Proposed internal blackboard model (**RECOMMENDATION**)

```text
Run          id, owner_session, workspace, state, version, budget, created_at
Job          id, run_id, kind, title, contract, priority, deadline, state, version,
             dependency_ids, requested_capabilities, assignment_policy, acceptance_state
Attempt      id, job_id, generation, assignee, claim_token_hash, lease_deadline,
             heartbeat_seq, state, execution_ref, cleanup_state, usage, started/ended_at
Claim        job_id, attempt_id, claimant, eligibility_snapshot_hash, reserved_slot
Message      id, run_id, thread_id, sender_attempt, recipient, kind, topic, body_ref,
             committed_seq, delivery_state, ack_state, created_at
Artifact     id, producing_attempt, media_type, uri, size, sha256, provenance, visibility
Subscription id, principal, scope, target, transport, secret_ref, cursor, state
Event        id, run_id, job_id, version, seq, type, payload_ref, committed_at
Outbox       event_id, subscription_id, attempt_count, next_at, last_status, state
Review       id, job_id, attempt_id, evidence_refs, reviewer_claims, checks, decision
```

- **RECOMMENDATION:** Use typed closed enums for lifecycle, delivery and review states; use versioned extension payloads for future message kinds, auction bids and remote protocols. Store large bodies/artifacts outside hot SQLite rows with content hash and confined path/object URI.
- **RECOMMENDATION:** A run is the blackboard namespace and owner; jobs are work contracts; attempts are executions; messages/knowledge are append-only observations; artifacts are immutable evidence. The board is a materialized view, not a mutable shared prompt.
- **RECOMMENDATION:** Put `run_id,job_id,attempt_id,principal` in every API authorization decision. Public caller-supplied IDs locate records but never grant membership. Current attempt capability is distinct from session ID; hash it at rest.
- **RECOMMENDATION:** Keep job, attempt, event and outbox mutation in a single SQLite transaction where possible. If an external process/worktree must be started, commit a launch intent first, perform the effect outside the transaction, then reconcile ownership. Never hold a DB transaction during provider, Git or webhook I/O.

Proposed contract example (**RECOMMENDATION**, exact aim schema still to be designed):

```json
{
  "title": "Audit the parser",
  "deliverable": "A review with reproducible findings",
  "acceptance": ["Each finding cites a source location", "Checks state their exact command"],
  "requires": {
    "skills": ["rust-review"],
    "tools": ["read", "search"],
    "environment": "local",
    "permission_ceiling": "read_only"
  },
  "depends_on": [],
  "assignment": {"mode": "lead_or_eligible_claim"},
  "budget": {"max_turns": 20, "deadline": "<timestamp>"},
  "workspace": {"mode": "shared_read_only", "base_commit": "<sha>"},
  "notify": {"subscriber_ids": ["lead"]}
}
```

- **RECOMMENDATION:** Canonicalize this contract before hashing/claiming so a retry compares semantic identity, not whitespace or field order. A later edit creates a new contract version or job, never silently changes an active attempt.
- **RECOMMENDATION:** Dependencies name exact job and required artifact/version, not merely a task title. Passing a dependency means verified availability and integrity of its output, not human acceptance unless the contract explicitly requests accepted input.

State machine (**RECOMMENDATION**):

```text
job: posted -> eligible -> claimed -> running -> succeeded | failed | canceled | interrupted
                 |            |           |          -> input_required (paused)
                 |            |           +-> cancellation_requested -> cleanup_pending
                 |            +-> claim_expired_suspect -> cleanup_pending
                 +-> blocked_by_dependency -> failed (without model spend)
retry: terminal failure/cancel/interruption + verified cleanup + policy -> new attempt
review: unreviewed -> reviewing -> accepted | changes_requested | rejected
merge: not_requested -> queued -> integrating -> integrated | conflict | failed
message: queued -> delivered -> acked; stale attempt -> retired (receipt retained)
notification: pending -> delivered | retry_due -> dead_letter; never changes job truth
```

Verus targets (**RECOMMENDATION**):

1. At most one current attempt per job generation, with monotonically increasing generation.
2. Exactly one durable owner claim for a launch; stale claim tokens cannot mutate current attempt.
3. A job starts only if all declared dependencies are successful and referenced evidence matches their attempts/hashes.
4. Terminal execution state never implies review acceptance or merge integration.
5. Message sequence is monotonic; ack requires delivery; old-attempt messages cannot be delivered as new-attempt context.
6. Duplicate message/event IDs with identical canonical bytes reconcile; different bytes conflict.
7. Admission count never exceeds configured capacity when every active/uncertain cleanup hold is counted.
8. Cancellation and retry cannot both authorize a new launch for one generation.
9. Outbox event version/sequence corresponds to a committed job transition; notification delivery cannot mutate it.
10. Visibility and permission ceiling can only narrow through delegation; a role label never grants authority.

These are logical model invariants. Process ownership, fsync durability, Git identity, HTTP signature verification, clock behavior and actual provider cleanup still need runtime tests and OS-specific evidence.

### API surface (**RECOMMENDATION**)

| Operation | Request essentials | Durable result / concurrency rule |
| --- | --- | --- |
| `post` | contract, deps, requirements, idempotency key | Validate DAG; create posted job + event atomically. |
| `assign` | job, target profile, expected version | Lead-only; assignment does not launch. |
| `claim` | job, agent identity, expected version | Check eligibility/capacity; create fenced attempt + token. |
| `heartbeat` | attempt, token, sequence, progress | Monotonic sequence; no ownership resurrection. |
| `message` | run, recipient, kind, stable ID, body | Persist before receipt; bounded quota, attempt fence. |
| `complete` | attempt token, artifact refs, usage, result | Validate hashes/provenance; execution state only. |
| `fail` | attempt token, safe error class, cleanup state | Keep uncertain cleanup as hold. |
| `cancel` | job, expected generation/version | Record intent; signal owned tree; reconcile. |
| `retry` | terminal job, expected version | Verify cleanup/evidence; new generation. |
| `watch` | run/job, cursor | Snapshot + ordered events; reconnect from cursor. |
| `poll` / `get` | job/task ID and auth | Authoritative current snapshot. |
| `subscribe` | scope, endpoint/local handle, auth | Outbox-backed delivery; no hidden state. |
| `review` | attempt, observed checks, decision | Separate acceptance record; never inferred. |
| `integrate` | attempt artifact, target HEAD | Serialized worktree merge; conflict preserved. |

- **RECOMMENDATION:** Serve these operations in-process, over a Unix socket, and via HTTP/gRPC using the same application service and identity checks. SSE/WebSocket watch transports subscribe to committed events; remote SSH should run against the remote authority rather than shadow only some calls.
- **RECOMMENDATION:** A2A server maps `SendMessage` into `post`/follow-up input, `GetTask` into `get`, `CancelTask` into `cancel`, SSE/push into `watch`/outbox. Preserve A2A context/task IDs, but keep internal attempt fencing hidden. MCP Tasks adapter maps a long tool call into the same job ID with scoped task auth.

### MVP boundary (**RECOMMENDATION**)

1. One local SQLite owner daemon, bounded job/DAG contract, deterministic lead assignment or claim, per-attempt identity and status.
2. Local worker execution with inherited permissions/budgets, cancel and honest uncertain-cleanup state; no automatic retry of external effects.
3. Bounded typed mailbox, persist-before-deliver safe-boundary ingestion, event cursor plus Unix-socket/in-process watch, authoritative poll.
4. Immutable artifact/evidence packet and explicit verification/acceptance state; isolated worktree option with serialized manual integration.
5. Capability profiles and deterministic matching; log assignment reasons and measured usage.
6. After the local contract is stable: HTTP webhook outbox and Standard Webhooks signing; then A2A 1.0 federation and MCP Tasks adapter; then Jev/auction routing based on benchmarked outcomes.

- **RECOMMENDATION:** Benchmark against a single-agent baseline and Codex/Claude/pi on identical tasks, models, prompts, workspace state and budgets. Record wall time, model calls, tokens, tool calls, merge conflicts, acceptance success and human review time. More parallelism is useful only when it improves these outcomes.
- **RECOMMENDATION:** Preserve every failed/uncertain attempt and dead-letter notification for operator diagnosis; retention/archival is an explicit policy. No hidden tombstone eviction that permits old idempotency keys to execute again.

## Open questions for the user

- Should remote agents be allowed to claim jobs automatically from the user's machine, or should the lead approve every new remote agent identity and capability profile before its first claim?
- For the MVP, should isolated worktrees be the default for editing jobs, or should the lead choose isolation per posted job? The user's tny ADR makes isolation opt-in for some modes but records shared-write risks.

<!-- REPORT COMPLETE -->
