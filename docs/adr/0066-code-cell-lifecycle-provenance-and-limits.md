# ADR 0066: Bind code cells to the turn that observes them, record their nested calls, and bound their output

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0018, 0022
- Scope: code mode's `run_code`, `exec`/`wait` and `run_program` tools: cell scheduling and
  lifetime, nested-call provenance on the wire and in the session log, output and store limits,
  the macOS worker sandbox, and where project programs are stored. It does not cover the
  model-visible `run_code` description or tool index (W26), nor image reservations (FIX17).

## Context

The cross-model review REV13a (`scratchpad/reviews/REV13a-coderun-reserve.md`) found that ADR 0018's
worker boundary held, but the machinery around it did not. Its probes are now regression tests.

- **Termination (H1).** `wait {terminate}` killed whichever cell was running. The terminated cell,
  if queued, still ran later with full tool authority.
- **Lifetime (H2).** `exec` cells outlived their turn and their session. They held the harness,
  so a session's aimx never shut down gracefully.
- **Sandbox (M1).** The Seatbelt profile was `(allow default)` with a few denials. The sandboxed
  worker could read `/private/tmp` and the per-user temp and cache directories, exec `/bin/sh`
  and `osascript`, make mach IPC, and read other processes' argv.
- **Output (M2, M3, M8).** Output was charged only by `text.len()`: empty `text('')` calls, and the
  separators between calls, were free. `notify('')` and `yield_control()` were unbounded.
  `max_output_tokens` failed a cell after its side effects instead of truncating it.
- **Provenance (M7).** Nested tool calls left no session event and no durable record, although
  ADR 0018 says to "record nested calls with cell provenance".
- **Admission (M9).** A `run_code` could queue silently behind an `exec` cell for 300 s, and
  `run_program` started a worker per call with no cap.
- **Lows.** L3 (the store was overwritten by a stale snapshot), L4 (leaked entries), L5 (project
  programs as a nested Git repository written with `std::fs`), L6, L8, L9 and L10.

Codex, whose `exec`/`wait` contract aim follows (ADR 0018), shows what the model expects.
References are to `codex-rs/core/src/tools/code_mode/mod.rs` in the reference clone:
- An interrupt terminates the session's active cells (`interrupt_active_cells`, line 157).
- Cells can outlive a turn; a broker holds their nested calls until a turn dispatches them
  (line 351).
- Each response is truncated to its token budget with a warning line
  (`truncate_code_mode_result`, line 311, test at line 536).

## Decision

### 1. Provenance: nested calls are child tool events, recorded durably

- **The shared context.** The agent loop runs every tool-call future inside a task-local
  `agent::tools::ToolCallContext {call_id, events, cancel}`: the provider call id, the turn's event
  sender, and the turn's cancel token. Code tools read it with `ToolCallContext::current()`.
  `ToolHost::call` is unchanged, and so are the wrappers between the loop and a tool host (none of
  them spawns). Native subagents (W32) use the same type to learn their parent call.
- **On the wire.** Each nested call emits `SessionUpdate::ToolStarted` and `ToolFinished` with
  `parent: Some(<run_code/exec/wait call id>)`. The field is additive: it is skipped when `None`,
  and old clients ignore it. A nested call's `call_id` is `<cell id>:<n>`.
- **In the log.** The recorder stores child events as the new `EventBody::NestedToolStarted` and
  `NestedToolFinished`. The model's own calls stay transcript items, as before. Builds older than
  this ADR read the new kinds as `EventBody::Unknown` and keep them verbatim
  (`aim-proto/tests/contract.rs`).
- **Idempotency keys.** A nested call's key is `<cell UUIDv7>:<n>`. It keeps the mint time that
  FIX4's dedup horizon parses (`aimx::dedup::minted_ms`); the old `code:` prefix hid it.

### 2. Lifetime: a cell runs only while a live turn observes it

- **Binding.** A cell is bound to the context of its `exec` (or `run_code`/`run_program`) call.
  A `wait` from a later turn rebinds it.
- **Interrupt.** Interrupting that turn ends the cell.
- **Session close.** Every cell ends and the worker is killed. The trigger is the drop of the last
  code tool host handle. `host.rs` also closes code mode explicitly, and waits at most 2 s for the
  cells to go before it shuts the workspace down. A cell can never keep the harness alive.
- **Normal turn end.** Cells survive a turn that ends normally, as in Codex. A nested call from a
  cell whose turn is over (its event channel is closed) is held, not dispatched, until a later
  `exec` or `wait` rebinds the cell. So no nested call ever runs unobserved.
- **Direct callers.** Callers outside the agent loop (tests, embedders) have no context. Their
  nested calls run without events.

### 3. Scheduling: one running cell per worker, a bounded queue, termination by cell

- **Queue.** Each session worker runs at most one cell. Up to 4 more wait in a FIFO queue. A cell's
  deadline starts at admission, so time spent queued counts against it.
- **`run_code`.** It waits at most 10 s for the running slot, then returns `LimitExceeded` with the
  running cell and the queue length. It returns at once if the queue is full.
- **`exec`.** With a full queue it is refused at once. Otherwise it returns its cell id as usual.
- **Termination.**
  - `wait {terminate}` ends exactly the named cell.
  - A queued cell leaves the queue and never runs.
  - The running cell is ended by killing the worker. The worker runs only that cell, so no other
    cell is affected. `$/cancel` alone cannot stop a cell that is busy in JavaScript.
- **Programs.** A session runs at most 2 `run_program` workers at once, with the same 10 s bounded
  wait.
- **Kernel candidate.** The scheduler is a small pure state machine, `coderun::scheduler`, and is
  listed for the kernel.

### 4. Output and store limits never fail a cell

- **Output budget** (`aim_coderun::budget`, applied by the worker and again by the parent):
  - `text` and `notify` cost their length plus one separator byte; a yield costs one byte.
  - Repeated markers coalesce.
  - A cell keeps at most 2048 events and its byte limit: 40 000 bytes for `run_code` and
    `run_program`, 64 000 for an `exec` cell.
  - Output beyond the budget is dropped and counted, never thrown, and a note tells the model.
  - The cell keeps running, and its `store` is kept.
- **Per-response truncation.** Each `exec` and `wait` response is truncated to `max_output_tokens`
  or `max_tokens` × 4 bytes: head and tail around an omission marker, after a warning line, as in
  Codex. The rest is not paged.
- **Store.** `store` is bounded at 4 MiB of JSON. The worker refuses the `store()` call that would
  exceed it, and the parent refuses a merge that would.
- **Store merge.** A cell's store changes merge key by key into the session store when it finishes
  (L3). A snapshot is taken when the cell starts running, not when it is queued.
- **Clearing.** `store(key, undefined)` removes a key.

### 5. The worker sandbox is deny-default

The macOS Seatbelt profile, from the pure builder `coderun::sandbox::profile`, allows only:
- `process-exec` and `file-read*` of the canonical worker path;
- `file-read*` of `/usr/lib` and the dyld shared-cache directories;
- `file-read-data` of `/`;
- `sysctl-read` of `hw.*`.

Everything else is denied: other files, other exec, fork, network, mach lookups, and other
processes' arguments. No temp directory is granted, because the worker writes nothing. JavaScript
`Date` therefore reports UTC.

### 6. Project programs are plain files in the workspace

Project programs live in `.agents/programs/<slug>/` as plain files. They are read and written
through the session's workspace, so aimx's scope, protected paths and deduplication apply. There
is no nested Git repository: the project's own version control tracks them. User programs stay a
local Git repository under `~/.aim/programs`, as ADR 0018 decided. Trust remains content-addressed
outside both.

## Consequences

- **Transcript.** The session log shows what a cell did, call by call, under the call that ran the
  cell. Clients that ignore `parent` show nested calls as ordinary tool rows.
- **Exec output.** A model that asks for small `max_output_tokens` loses the middle of long
  output, not the cell. Paging is gone.
- **Busy cells.** Parallel `run_code` calls, and calls behind a long `exec` cell, get a fast,
  explicit busy result instead of a silent wait.
- **Unobserved cells.** A cell left running when its turn ends cannot act until the model waits
  on it again. Its 300 s deadline still runs from admission.
- **Sandbox upkeep.** The profile must follow macOS's dyld cache location (the three known paths
  are listed). A future OS that moves the cache fails closed: the worker does not start.
- **Convergence.** W28's reusable Seatbelt module can adopt the profile builder: this one is its
  `readable` set plus an exec literal, with network off.

## Verification

- **Regression tests for every High and Medium finding**, in `crates/aim/tests/coderun.rs`,
  `crates/aim-coderun/tests/cells.rs`, `crates/aim/src/coderun/*`:
  - H1: terminating a queued cell;
  - H2: interrupt, close, harness release;
  - M1: sandbox probes, macOS only;
  - M2, M3: budget;
  - M7: child events and log records;
  - M8: truncation;
  - M9: busy.
- **ADR 0018's named tests.** `coderun_nested_call_uses_dispatcher`,
  `coderun_timeout_and_crash_isolation`, `codex_exec_wait_contract`, `program_grants_only_narrow`.
- **Live.** `crates/aim/tests/coderun_live.rs` (OpenRouter: a `run_code` turn with child events;
  codex: `exec`/`wait` with `terminate`).
