# ADR 0038: Record a session's agent and effort source; report every config outcome

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0007, 0013, 0021, 0028, 0031, 0033, 0036
- Amends: 0028 (effort provenance), 0033 (agents on resume, bounds), 0036 (config outcomes)
- Scope: what a persistent session records so a resume keeps its authority and effort policy (`SessionMeta.agent`, `ConfigChanged.effort_source`); the outcome of every `session.set_config` (deferred, partial, storage failure, close); the services the native backend factory is given; how discovery's bounds are admitted. It does not cover ACP `session/load`, the Jev controller (0013, 0028), or aimx's read semantics.

## Context

The codex review REV8 (`scratchpad/reviews/REV8.md`) and Claude's review REV9 (`scratchpad/reviews/REV9-jev-media.md`) found these gaps in the session host:

- **REV8-2 (blocker).** `SessionMeta` did not record a session's named agent, and `live_or_resume` passed `agent: None`. A session created with a read-only agent therefore resumed with every tool, including Bash, Write and the credential-local media tools. ADR 0033 listed this as a known gap.
- **REV8-14 and REV9-M1.** A persistent session with no explicit effort recorded its starting level as a plain `ConfigChanged`. Resume read it back as an explicit effort, so Jev never advised the session again. REV9's live probe measured one decision before the resume and none after. `set_config` also had no way back to automatic.
- **REV8-3.** ACP applies model and effort as two requests, and the effort options can change with the model. A failed second step left the model changed, while the host reported "nothing changed" and announced nothing.
- **REV8-4 and REV8-17.** A change accepted during a turn and refused at its end was only logged (`tracing`). A pending change was also still applied after a close was requested.
- **REV8-5.** An idle `set_config` replied `Ok` and broadcast `ConfigChanged` even when that event could not be appended. The session then closed, and a resume restored the old configuration.
- **REV8-9.** The fixed files of a discovery scope (the `AGENTS.md` walk, `MEMORY.md`) bypassed the file and byte budgets. A batch of 32 was also started whenever any budget remained.
- **REV9-M4.** The native factory read the machine's codex credentials and `TYPESAFE_API_KEY` for every session, tests included. No test could show that a private session never reaches Jev (ADR 0013).

## Decision

**A session's agent is part of its record.**
- At creation the host stores `SessionMeta.agent = { name, allow, deny }`, the named agent and the tool ceiling in force. The field is additive and absent for the default agent and for older logs.
- A resume passes the name back as `SessionSpec.agent`, with the recorded ceiling (`BackendRequest.recorded`). The native factory discovers the definition again:
  - if it is missing or not importable, the resume is **refused** (`not_found` / `invalid_params`) and the session stays stored. It never resumes without its ceiling;
  - otherwise the ceiling is the current definition's policy **intersected** with the recorded one: allowlists intersect and denylists unite. An edited definition can narrow a resumed session, never widen it. The agent's instructions and prompt-cache key are applied as at creation.
- ACP sessions refuse a named agent (`unavailable`): the ACP bridge cannot enforce its tool ceiling yet.

**Effort has a recorded source.** `EventBody::ConfigChanged` and `SessionUpdate::ConfigChanged` carry `effort_source: explicit | auto`. It is additive, and a missing value reads as `explicit`, so older logs resume exactly as before.
- An effort that is not set is `auto`: a new session whose spec and agent definition name none, and any record without a level. A set effort is `explicit` for a new session. On resume it keeps its recorded source, so older logs resume as they did.
- Jev decisions (0028) announce `auto`. A `set_config` with an effort announces `explicit`.
- `set_config` with `effort: "auto"` (`aim_proto::daemon::AUTO_EFFORT`, reserved) returns a native session to `auto`. The level in force is where Jev starts. ACP agents offer no automatic effort and refuse it, unless the agent itself advertises such a level.
- A resume restores the source. For `auto`, the recorded level is the starting point, and the decider is attached again when the session is persistent and one is configured.
- An advised `auto` session that switches models starts from the new model's ladder: its catalog default, else its lowest level (REV9-m1). Requests and decision records then agree on the index.

**Every config change has an outcome the requester can see** (amends 0036's "a refusal at that point is logged").
- **Idle.** The change is applied at once. A refusal is the call's `invalid_params` error. If its `ConfigChanged` cannot be appended, the call fails with `internal`, nothing is broadcast, and the session closes (0036).
- **During a turn.** The change is accepted and merged field by field (0036). When the turn ends, the change produces exactly one of:
  - `ConfigChanged`: it applied;
  - `ConfigRejected { model, effort, message }`: the backend refused it;
  - `ConfigRejected` with a "cancelled" message: the session is closing or its log has failed, so the change is not applied and the actor does not wait on the backend.

  `ConfigRejected` is not recorded, because a rejection changes no state.
- **Partial changes.** `Backend::set_config` may fail after changing something, for example ACP's model step followed by a refused effort step. After any refusal the host asks the backend what is in force (`set_config(None, None)`). If that differs from what was last announced, the host records and publishes `ConfigChanged` before it reports the refusal. The ACP backend has no read method, so it reports the options the agent last returned: those of the successful model step, plus any `config_option_update` notifications that arrived since.

**Shutdown is bounded.** `SessionHost::shutdown_within(deadline)` (`shutdown` is 10 s) refuses new starts at once. Its deadline covers waiting for starts in flight, closing sessions, and their workspace shutdowns. A start that finishes after shutdown began tears itself down and returns `unavailable`, never going live (REV8-7).

**Services are injected.** `native_backends_with` takes `NativeServices { media, decider }`.
- Tests pass none or fakes.
- `providers::services()` wires production:
  - codex media tools when codex credentials exist;
  - `JevDecider` when `TYPESAFE_API_KEY` is set.
- A decider is attached only to persistent sessions (0013). Media tools are composed with `Dispatcher::with_policy`, allowed for persistent sessions only. Private and ephemeral sessions have no per-session opt-in yet, so they get none (0042). The skill budget uses the context window of the model the session will actually run, so an agent that selects another model is budgeted for that model (REV8-15).

**Discovery admits before it reads** (amends 0033's bounds). Every file requested counts against `max_files`: the fixed files, then the listed ones. Before a batch is sent, it is sized to the budget left:
- at most `READ_BATCH` files;
- at most the files left;
- at most `⌊bytes left / per-file cap⌋` files, where the per-file cap is `min(max_file_bytes, bytes left)`.

Bytes read are charged afterwards, including a `CLAUDE.md` that loses to `AGENTS.md` and the memory index. A failed read is charged its whole cap. A FIFO or device never blocks discovery: user files are opened `O_NONBLOCK` and must be regular files. A user directory listing stops at its entry limit.

## Consequences

- A resumed session keeps the authority it was created with, and deleting or loosening its agent file cannot widen it. The cost is that a session whose agent file is gone cannot be resumed until the file is restored.
- Automatic effort survives restarts. `"auto"` is reserved in `session.set_config`, so a catalog level with that name cannot be pinned through it. None of the catalogs aim reads has one (codex `gpt-6-sol`: `minimal … xhigh`; docs/research/live-probes.md).
- UIs learn the outcome of every deferred change from the update stream. Idle requesters get it as the reply.
- `Backend::set_config` returns an `InForce { model, effort, effort_source }` value. Its error contract is now "may have changed something; ask again".
- Discovery can read slightly less than before (a partly spent budget stops a batch early). In exchange the advertised bounds hold.
- **Open, owned by aimx:** `fs.read` and `fs.read_many` still read and hash a whole file to return a bounded prefix (`crates/aimx/src/workspace/local/fs.rs`, `read`). The protocol promises a whole-file hash. Truly bounded project reads need an additive prefix read without the whole-file hash; this is proposed, not decided here.

## Verification

Each test below failed against the code before its fix: run red first (REV8-6), or checked by reverting the fix and re-running.

- `crates/aim-proto/tests/contract.rs`: `adr_0038_fields_are_additive_and_old_records_read_as_explicit`.
- `crates/aim/tests/resources.rs`:
  - `a_resumed_named_agent_keeps_its_tool_ceiling`: after a restart, Write and a media tool are neither offered nor reachable, and a widened definition does not widen the session;
  - `a_resumed_agent_session_is_refused_when_its_definition_is_gone` (missing, and not importable);
  - `the_skill_budget_follows_the_agent_model`;
  - `fixed_and_listed_files_share_one_admission_budget`;
  - `a_fifo_memory_index_never_blocks_discovery_or_exit`;
  - `real_aimx_keeps_a_project_instruction_split_at_the_byte_limit`.
- `crates/aim/tests/host.rs`:
  - `automatic_effort_survives_a_restart_and_explicit_effort_can_return_to_auto`;
  - `a_log_from_before_adr_0038_resumes_as_it_did`;
  - `an_automatic_session_switching_models_starts_from_the_new_ladder`;
  - `a_private_session_never_calls_the_decider`;
  - `without_media_services_only_workspace_tools_are_offered`;
  - `a_deferred_config_refusal_reaches_the_stream`;
  - `a_partial_config_change_is_reconciled_and_announced`;
  - `an_idle_config_change_the_log_cannot_keep_is_an_error`;
  - `a_pending_config_is_cancelled_when_the_session_closes`;
  - `shutdown_is_bounded_while_a_backend_is_still_starting`.
- `crates/aim/src/acp.rs`: `a_failed_effort_step_reports_the_changed_model` (a scripted ACP agent whose effort options depend on the model). `crates/aim/tests/acp_bridge.rs`: `acp_sessions_require_authority_and_persistence_gates` covers the named-agent refusal.
- `crates/aim/src/resources/agents.rs`: `intersected_policies_permit_exactly_what_both_permit`, an enumeration over policies and tools. `crates/aim/src/resources/files.rs`: `harness_reads_keep_the_text_before_a_split_character`, `listing_stops_at_its_limit` and `a_fifo_is_refused_without_blocking`.
- Live, run for this change: `acp_bridge`, `compaction` and `resources` (`-- --ignored live_`).
