# Architecture decision records

Numbered and append-only. A decision that changes an earlier one gets a **new** ADR that names what
it supersedes (`Supersedes: 0007`); the old file stays, and its status line gains
`Superseded by NNNN`. Code, proofs and docs reference decisions as `docs/adr/NNNN`.

Rules (enforced by `mise run check`):

- File name: `NNNN-kebab-slug.md`. **Numbers are unique** — tny accumulated eleven duplicated
  numbers, and tooling that indexes by number alone then silently drops decisions. The check fails
  on any duplicate.
- Header block, in this order: `Status` (Proposed | Accepted | Superseded by NNNN | Rejected),
  `Date` (ISO), optional `Supersedes`, optional `Baseline` (ADRs this one builds on), `Scope`.
- Sections: `Context`, `Decision`, `Consequences`. Add `Verification` when a Verus proof or a live
  smoke test locks the decision — name the proof function or test so the ADR and the evidence
  point at each other.
- Evidence beats assertion: cite research reports, live probes, benchmarks or proofs.
- Agents write ADRs. Any change that alters an architectural contract (a protocol, a public trait,
  a persisted format, a policy default, a verified invariant) lands with its ADR in the same
  commit.

| ADR | Decision |
| --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record numbered, append-only architecture decisions with verification evidence. |
| [0002](0002-two-applications-and-a-gate.md) | Keep aimx and aim independent, with a separate trusted evolution gate. |
| [0003](0003-tooling-pinned-through-mise.md) | Pin Rust, Verus, and other tools through mise. |
| [0004](0004-strict-lint-policy.md) | Enforce strict workspace Clippy and Rust lint policy. |
| [0005](0005-verified-kernel.md) | Keep pure decisions in a Verus-verified kernel and lock protocol negotiation. |
| [0006](0006-core-protocols-and-edges.md) | Own core protocols and adapt standard protocols at the edges. |
| [0007](0007-event-sourced-sessions.md) | Persist typed session events with private in-memory sessions. |
| [0008](0008-execution-enforcement-in-aimx.md) | Admit tool calls in aim and enforce every call in aimx. |
| [0009](0009-ssh-shadowing.md) | Shadow workspace tools over SSH with a resident remote aimx and agentless fallback. |
| [0010](0010-codex-chatgpt-backend.md) | Implement a narrow ChatGPT Codex backend with independent OAuth. |
| [0011](0011-openai-compatible-profiles.md) | Configure OpenAI-compatible providers through named profiles and data quirks. |
| [0012](0012-claude-code-via-acp.md) | Run Claude Code through the pinned ACP client and explicit tool authority modes. |
| [0013](0013-jev-decisions.md) | Batch Jev advice and verify bounded integer effort decisions. |
| [0014](0014-dot-agents-layout-and-imports.md) | Use .agents and ~/.aim layouts with provenance and opt-in executable imports. |
| [0015](0015-tui-screen-model.md) | Use inline scrollback, overlay screens, and one semantic transcript. |
| [0016](0016-wasm-component-plugins.md) | Host capability-limited WebAssembly component plugins. |
| [0017](0017-declarative-ui-protocol-and-themes.md) | Render built-in and authored UI through one declarative protocol. |
| [0018](0018-code-mode.md) | Run code mode in sandboxed workers through the tool dispatcher. |
| [0019](0019-blackboard-ledger.md) | Make the SQLite job and attempt ledger the swarm's sole authority. |
| [0020](0020-gate-only-self-improvement.md) | Promote autonomous changes only through the isolated evolution gate. |
| [0021](0021-yolo-with-narrowing-ceilings.md) | Default to yolo while preserving narrowing ceilings and protected paths. |
| [0022](0022-live-smoke-tests-required.md) | Require live smoke evidence for every integration. |
| [0023](0023-rust-web-ui-leptos.md) | Build the web UI in Rust with Leptos and shared protocol types. |
| [0024](0024-benchmark-manifest.md) | Define comparable benchmark tiers and a quality-first pass condition. |
| [0025](0025-compaction-plan-invariants.md) | Cut transcripts without separating tool exchanges. |
| [0026](0026-daemon-stream-lifecycle-and-stored-summaries.md) | Order daemon attachments, report stream termination, and summarize stored sessions. |
| [0027](0027-policy-kernel.md) | Intersect authority scopes in the verified kernel. |
| [0028](0028-jev-decision-event.md) | Record bounded Jev decisions as typed session events |
| [0029](0029-media-services-and-audio-retention.md) | Expose media services and require explicit consent for private audio retention. |
| [0030](0030-board-daemon-contract.md) | Expose the board ledger through typed daemon methods. |
| [0031](0031-session-host-and-agent-backends.md) | Host sessions as actors over pluggable agent backends. |
| [0032](0032-compaction-events-and-model-context.md) | Record compaction as events; rebuild the model's context from the log. |
| [0033](0033-resource-catalog-and-activation.md) | Discover resources into one bounded catalog and activate skills in the user turn. |
| [0034](0034-inline-tui-writer-history-and-completion-sources.md) | Paint the inline TUI with a relative block writer; keep scrollback terminal-owned. |
| [0035](0035-conversation-search-projection.md) | Index persistent conversation events as redacted chunks. |
| [0036](0036-log-failure-closes-and-config-replies.md) | A session stops at its first log-write failure; config changes answer their requester. |
| [0037](0037-bounded-rpc-notification-backlog.md) | Bound ordered RPC notifications without blocking control frames. |
| [0038](0038-session-authority-effort-source-and-config-outcomes.md) | Record a session's agent and effort source; report every config outcome. |
| [0040](0040-daemon-recovery-and-paged-attachments.md) | Recover daemon attachments and page large session histories. |
| [0041](0041-indexed-session-summaries.md) | Index durable session summaries. |
| [0042](0042-bound-media-services-and-private-opt-in.md) | Bound media output and require private-session opt-in. |
| [0045](0045-trusted-mcp-edges.md) | Bind MCP imports to source hash and run location; serve aim tools over both MCP lifecycles. |
| [0046](0046-per-call-harness-authority.md) | Carry narrowing authority on harness calls and sessions. |
| [0047](0047-remote-harness-websocket-and-http.md) | Carry the harness protocol over bounded WebSocket and HTTP sessions. |
| [0048](0048-board-workers-and-integration.md) | Run fenced board attempts in isolated worktrees and integrate accepted evidence serially. |
| [0050](0050-verified-tool-dedup-and-discovery-decisions.md) | Verify named-agent tool ceilings, mutation replay, and resource-read budgets. |
| [0051](0051-daemon-web-listener-and-browser-client.md) | Serve an authenticated Leptos browser client from the daemon's bounded WebSocket listener. |
| [0052](0052-remote-workspace-location.md) | Persist remote harness URLs as workspace locations without credentials. |
| [0053](0053-network-token-call-scope-ceilings.md) | Bind network bearer scopes to harness sessions. |
| [0054](0054-reserve-image-destination-before-generation.md) | Reserve image destinations before paid generation. |
| [0055](0055-board-attempt-identity-and-worker-boundary.md) | Bind cleanup to an attempt and narrow worker authority. |
| [0056](0056-startup-catalog-and-model-output-budget.md) | Reuse startup capabilities and bound model-visible shell output. |
| [0063](0063-compose-native-mcp-and-board-tools.md) | Compose trusted MCP and durable board tools, with a private last-known MCP catalog. |
| [0064](0064-ui-surfaces-envelope-events-and-agent-tools.md) | Carry agent UI surfaces as validated, logged A2UI-shaped messages. |
| [0065](0065-locked-digests-resolve-names-by-module.md) | LOCKED digests resolve kernel names by module. |
| [0066](0066-code-cell-lifecycle-provenance-and-limits.md) | Bind code cells to the turn that observes them, record their nested calls, and bound their output. |
| [0074](0074-session-options-and-tui-session-switches.md) | Sessions advertise what they can switch to; the TUI derives new sessions in the kernel. |
| [0075](0075-resolve-acp-config-values-to-advertised-values.md) | Resolve requested ACP model and effort values to advertised values by verified tiers; list the values on refusal. |
| [0076](0076-code-mode-setting-and-acp-relay.md) | Select code mode with `AIM_CODE_MODE`, decide its exposure in the kernel, and serve it to Claude through aim's relay. |
