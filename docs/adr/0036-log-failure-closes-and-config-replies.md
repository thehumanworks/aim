# ADR 0036: A session stops at its first log-write failure; config changes answer their requester

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0031
- Scope: how the session host reacts to a failed append to a persistent session's log, and what `set_config` promises its caller; not store durability itself (SQLite, ADR 0007) or ephemeral sessions (their memory store cannot fail).

## Context

The codex review of the lead's code (REV6, `scratchpad/reviews/REV6-lead-code.md`) found two problems in the session host.

- **Findings 1 (major):** `publish` only warned when an append failed, then mirrored and broadcast the update anyway. A live turn could therefore complete while resume rebuilt a different context. Worse, a later append that succeeded left an event silently missing from a gap-free log (ADR 0007's `seq` check cannot see a skipped event, because `seq` does not advance on failure).
- **Findings 3 and 5 (majors):** `SessionClient::set_config` returned success before anything was applied. A refused change, such as an effort the model does not offer, or a partial ACP change, was only logged, and deferred changes overwrote each other.

## Decision

**Log failures.** A persistent session stops recording at its **first** failed append. It then:
1. stops the running turn (the turn is cancelled and winds down as usual);
2. reports the turn's single terminal event as `TurnFailed { message: "store: …" }`, whatever the backend ended with;
3. closes the session (`StateChanged { closed }`).

If `TurnStarted` itself cannot be recorded, the turn does not run and `TurnFailed` is its only event. Nothing is appended after the failure, so the log ends cleanly where it failed. On resume, repair answers any call left without a result, as for a crash.

**Config replies.** `set_config` answers its requester.
- **When idle:** the change is validated and applied at once, and a refusal is the call's error (`invalid_params`). Model and effort are checked against the provider catalog (native) or the agent's advertised options (ACP) **before** anything changes, so a refused value never leaves a partial change behind.
- **While a turn runs:** the change is accepted, merged field by field with earlier pending changes, and applied as the turn ends, before any later control message. A refusal at that point is logged.
- **In every case:** `ConfigChanged` announces the state actually in force.

A new session records its initial configuration (`Backend::set_config(None, None)` reports it), so a resumed session keeps its model and effort.

## Consequences

- A client never sees a successful turn whose log is incomplete, and it learns why the session closed.
- A full disk ends a session instead of silently diverging it. The user resumes the session after fixing the store, and repair answers any interrupted call.
- UIs get immediate feedback on invalid settings when idle. A change made mid-turn is only known to have applied once `ConfigChanged` arrives.

## Verification

`crates/aim/tests/host.rs`:
- `a_log_that_cannot_be_written_fails_the_turn_and_closes_the_session`;
- `an_effort_the_model_does_not_offer_is_refused_when_idle`;
- `config_changes_during_a_turn_merge_field_by_field`;
- `a_resumed_session_keeps_its_initial_effort`;
- `shutdown_waits_for_a_starting_session_and_refuses_new_ones` (REV6 finding 8).
