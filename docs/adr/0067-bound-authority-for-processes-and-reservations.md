# ADR 0067: Bind processes and reservations to their creating authority; journal live reservations

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0027, 0046, 0053, 0054
- Scope: how aimx authorizes later calls on processes and file reservations, which content
  comparisons need read authority, what `max_output_bytes` bounds, and how abandoned reservation
  markers are removed. No wire type changes. The pure scope decisions stay in ADR 0027's kernel.

## Context

Two cross-model reviews found gaps in ADR 0046's per-call authority and ADR 0054's reservations.

- **REV14 F2.** `fs.finalize` and `fs.cancel` checked the call's scope only lexically, then wrote
  through the backend view captured at reserve time. An unscoped reservation of
  `allowed/link/out` (`link -> ../private`) was finalized under the scope `{roots:[allowed]}` and
  wrote `private/out`. The same happened after a session narrowed its ceiling. The review's
  scratch probe printed `private/out = Ok("WRITTEN-UNDER-SCOPE")`.
- **REV14 F8.** Process control (`exec.read`, `wait`, `write_stdin`, `resize`, `signal`,
  `release`, `BashOutput`, `KillShell`) re-checked the *lexical* cwd string against the current
  scope. ADR 0046 promises that control is "judged under its bound process authority", but
  nothing bound one.
- **REV14 F5.** A scope with `write` but not `read` could read a file byte by byte. It could
  guess with `fs.edit {old: g, new: g}` (success, `not found`, or an occurrence count) or with
  `IfHash`, whose `precondition_failed` detail carried the whole-file hash. ADR 0027's op mask
  treats `read` and `write` as independent.
- **REV14 F7.** `max_output_bytes` was documented as "maximum retained output bytes". It was
  enforced per response, and pushed `exec.output` ignored it.
- **REV13a M5.** `HarnessClient::shutdown` killed `aimx serve --stdio` right after closing the
  connection, before aimx's EOF shutdown could cancel live markers. The resume-TTL reaper never
  ran either. ADR 0054's cleanup claim did not hold on the default local path. The review's probe
  printed `art/sun.png = Ok("aim-reservation")` after a normal shutdown.
- **REV13a L1 and L2.** A failed finalize or cancel kept its slot among the session's 64. A
  reserve racing `Session::close` could add a claim to a closed session.
- **REV13a M6.** The agentless SSH backend did not support reservations. `generate_image` then
  failed there, where it had worked before ADR 0054.

## Decision

1. **Bound authority.** A process and a reservation record the effective scope of the call that
   created them (`authz::Bound`, from `Grant::bound`). A later call on them is permitted only when
   its own effective scope (principal ∩ session ceiling ∩ call scope) **narrows** the bound one
   (`authz::may_act_within`, which uses the verified `policy::Scope::narrows`). A later call can
   therefore never widen the creating authority. A wider caller, such as an unscoped parent of a
   delegated child, passes the creating scope or a narrower one. When a session ends, it still
   releases every process and reservation.
2. **Resolved targets.** Control is also checked against the target resolved when the object was
   created, never against a lexical spelling:
   - The local backend records a process's **real cwd** at spawn. Every scoped view checks `Exec`
     there on each control call.
   - `fs.finalize` and `fs.cancel` write through the **current call's view**. That view re-walks
     the marker path and checks the current grant on the resolved target, so a symlink planted
     after the reservation cannot redirect the write.
   - The reserve-time view is kept only for the session-end cleanup, which removes a marker only
     while its hash matches.
   - A refused `exec.release` or `KillShell` keeps the process owned.
3. **Content comparisons need read.**
   - `fs.edit` and the `Edit` tool need `read` and `write`. The backend checks both on the
     resolved target.
   - A client-supplied `IfHash` on `fs.write` needs `read`. A write-only scope may still create
     files or blindly replace them.
   - The `current` hash in a `precondition_failed` detail is included only when the backend view
     may read the resolved target. This covers the server-held `IfHash` of `fs.finalize` under a
     write-only scope, which stays permitted.
4. **`max_output_bytes`** bounds the output payload of one response or one pushed `exec.output`
   batch.
   - A process spawned under a scope with a limit keeps its output in chunks of at most that many
     bytes. The chunk size never goes below 1 KiB and never above the 32 KiB read block, and a
     sequence number is never split.
   - The pushed batch is clamped to the limit bound at spawn.
   - A later, narrower limit is honored for chunks already kept (REV19 B4). An `exec.read`
     whose first chunk is larger than the call's limit (or the 1 KiB floor) is refused
     (`denied`), rather than returned or split, because a sequence number is never split. The
     forwarder skips such a chunk once the session ceiling has narrowed, which leaves a `seq` gap.
     The pure check is `workspace::chunk_fits`.
   - The `aim-proto` doc comment says this now.
5. **Reservation lifecycle.**
   - A finalize or cancel that fails because the marker is gone, changed or replaced
     (`precondition_failed`, `not_found`, `conflict`) ends the reservation and frees its slot.
     Any other failure, such as a refusal or an unavailable backend, keeps it for a retry.
   - A session marks itself closed under its reservation lock. A reserve that completes after
     that gets its claim back and removes its own marker.
6. **Durable reservation journal.** A new persisted format:
   - **Location.** `<canonical home>/.aim/aimx/reservations/`. The directory has mode 0700, is
     owned by the user, and is added to the default protected paths.
   - **Trusted storage (REV19 B1).** Writing and sweeping both open the directory component by
     component from `/` with `O_NOFOLLOW`, so no component may be a symlink. The directory must be
     owned by the user; a looser mode is tightened to 0700. Every entry is then reached relative to
     that held descriptor. Storage that fails these checks is neither written nor swept, and
     nothing is deleted from it. The home is canonicalized once when the path is built.
   - **Contents.** One entry per live reservation, `<tag>-<random>.json`, mode 0600, where `<tag>`
     is 16 hex digits of the SHA-256 of its root:
     `{"version":1,"root":<canonical workspace root>,"path":<absolute marker path>,"hash":"sha256:…"}`.
     There is also `.lock`, mode 0600, the admission lock.
   - **Write order.** Under an exclusive `flock` on `.lock` (REV19 B3), the recorder counts the
     entries and then creates its own entry, so threads and aimx processes cannot overshoot the
     bound together. The entry is created as `.tmp-<tag>-<random>`, `flock`ed, written,
     `fsync`ed and renamed into place **before** its marker is created. The owning server keeps it
     open and locked while the reservation lives, and deletes it when the reservation ends.
   - **Sweep.** Each `workspace.open` of a root sweeps that root's entries whose lock is free,
     meaning their server exited or was killed. The sweep removes the marker only while it still
     has the recorded hash, then deletes the entry.
   - **Concurrent servers.** A live reservation of another aimx is never swept, because its lock
     is held.
   - **Bounds.** The journal holds at most 4096 entry files besides `.lock`. A sweep reads every
     name (at most 4160), opens only its own root's entries, and cleans up at most 256 of them. It
     starts at a random point of its root's sorted entries, so repeated sweeps reach every
     abandoned entry even when some cleanups keep failing (REV19 B2).
   - **Failure handling.** A reservation that cannot be journaled is refused (fail closed), since
     its marker could otherwise outlive a crash. A read-only server does not sweep. Temporary
     entries left by a writer that died are removed once unlocked and older than 60 s.
   - **Where it is enabled.** `aimx serve` and the resident server enable the journal. An
     embedded `Server` without `ServerConfig::reservation_journal` keeps none.
7. **Graceful harness shutdown.** `HarnessClient::shutdown` closes the connection. It then gives a
   spawned aimx up to 2 s to finish its EOF shutdown (session close cancels its unused markers)
   before it kills the process. A dropped or cancelled `generate_image` future cancels its
   reservation on a spawned task (a drop guard). The guard stays armed until the harness answers
   an explicit cancel, so a future dropped while that cancel is pending still cancels (REV19 B5).
   The retry reuses the idempotency key, so aimx runs the cancel at most once.
8. **Agentless SSH** supports reservations. `cancel_if_hash` compares the hash and removes the
   file in one remote script under the backend's mutation lock, and never follows a marker that
   became a symlink.

## Consequences

- To control a process or finalize a reservation created under a narrower scope, a caller must
  present that scope. aim today sends no per-call scope, so its behaviour is unchanged. A child
  delegated a narrower scope cannot act on its parent's wider processes. It can still act on a
  parent's process whose resolved cwd its own scope permits for `exec`, because the check only
  needs its scope to narrow the parent's. Such a child could spawn its own process there anyway.
- Write-only scopes lose `fs.edit` and `IfHash`. This is intended: both are reads.
- A limit below 1 KiB still delivers process output in chunks of up to 1 KiB. This avoids
  per-chunk overhead in the output ring.
- ADR 0054's cleanup now holds on the default local path, in three ways:
  - A normal shutdown cancels unused markers.
  - A killed or crashed aimx leaves a journal entry that the next open of the root sweeps.
  - Only a marker whose root is never opened again stays on disk, and its entry records it.
  One limit remains from ADR 0054: a same-user writer replacing a marker with identical bytes.
- `RemoteHarness::shutdown` (`crates/aim/src/remote.rs`) still kills its SSH transport right after
  closing the protocol, so over SSH the markers are removed only by the remote server's own
  cleanup. The agentless server keeps no journal. **Unverified** for agentless markers after a
  kill; see the FIX17 report.

## Verification

- **Pure decisions.** The pure decisions are small functions with unit tests. They are listed
  for the kernel in the FIX17 report:
  - `authz::may_act_within` (`later_calls_act_only_within_the_bound_authority`);
  - `authz::tree_guard` (`tree_guard_refuses_root_protected_and_denied_descendants`);
  - `authz::canonical_scope_path` and `authz::inherit_limits`
    (`scope_paths_and_limits_canonicalize`).
  They build on ADR 0027's verified `Scope::narrows`. None is a Verus proof yet.
- **RPC boundary.** `crates/aimx/tests/conformance/authority.rs`:
  - `rev14_finalize_rechecks_the_call_scope_on_the_resolved_target` (the REV14 probe as a
    regression);
  - `finalize_after_the_ceiling_narrows_is_judged_by_the_narrower_ceiling`;
  - `a_symlink_planted_after_the_reservation_cannot_redirect_the_finalize`;
  - `a_reservation_cannot_be_finalized_with_wider_authority`;
  - `rev14_write_only_scope_is_not_a_read_oracle`;
  - `precondition_detail_needs_read_and_a_lost_marker_ends_the_reservation`;
  - `failed_cancels_do_not_exhaust_reservation_slots`;
  - `process_control_is_judged_on_the_resolved_cwd`;
  - `a_process_is_controlled_only_within_its_spawning_authority`;
  - `scoped_process_output_is_bounded_by_the_output_limit`.
  Nine of these fail on the pre-ADR code. The symlink-planted case already held, because the
  reserve-time view was scoped too.
- **Journal.** Real `aimx serve --stdio` processes: `reservation.rs`
  (`a_killed_servers_marker_is_swept_at_the_next_open`,
  `a_concurrent_servers_live_reservation_is_not_swept` and
  `stdio_eof_cancels_unused_markers_before_exit`), plus the unit tests in `server/journal.rs`.
- **Session race.** `server::session::tests::a_reservation_added_after_close_is_refused`.
- **REV19 fixes.** Each has a regression test that fails on the code before the fix:
  - B1: `reservation.rs::a_symlinked_journal_is_never_swept` (the review's RPC probe, with a
    trusted-storage control) and `journal::tests::untrusted_storage_is_neither_written_nor_swept`;
  - B2: `journal::tests::sweeps_reach_every_abandoned_entry`;
  - B3: `journal::tests::concurrent_admission_keeps_the_bound` (the review's 32-thread probe);
  - B4: `authority.rs::a_later_narrower_output_limit_refuses_larger_existing_chunks` (the
    review's probe) and `a_narrowed_ceiling_bounds_output_pushed_later`;
  - B5: `media::dispatcher::tests::dropping_during_a_pending_cancel_still_cancels`.
- **Live.** The live evidence (ADR 0022) is recorded in the FIX17 report:
  - aim's real `generate_image` on a local workspace, and one cancelled mid-generation;
  - agentless SSH reservations over the user-space sshd fixture.
