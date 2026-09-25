# FIX6 SSH repair report

## Result

The SSH slice fixes both REV5 blockers, all 11 majors, and the correctness and security minors covered by the task. The implementation is on `agent/aimx/ssh-fixes`; it has not been merged into `main`.

## Changes

- `crates/aimx/src/ssh/agentless.rs`: serialized mutations; atomic `IfAbsent`; preserved write modes and confined symlink targets; precise not-found, conflict, and transport errors; bounded transient channels and six live process slots; permits released at child exit; separate bounded stdin writes; random process markers, process-group signals, timeout and drop cleanup; shared output ring and byte edits; accurate root aliases, pagination, and PTY size; bounded search and batched directory metadata. Agentless `exec` is advertised only when the remote has Perl or `setsid` for process-group isolation.
- `crates/aimx/src/ssh/conn.rs`: private owned ControlPath, stale-socket recovery without detached master leaks, no direct-connection fallback, disabled inherited agent/X11/TTY/forwarding settings, bounded authenticated askpass relay, destination validation, cancellation cleanup, and shell-neutral command encoding tested under `sh`, `csh`, and `tcsh`.
- `crates/aimx/src/ssh/bootstrap.rs`: hashes file contents via stdin, verifies an upload before publishing it, and checks ownership and permissions of trusted directories.
- `crates/aimx/src/ssh/forward.rs`, `reconnect.rs`, `resident.rs`: safe askpass script creation, bounded relay I/O and heartbeat, conservative RPC replay, reserved ID handling, resident startup cleanup and retry, and a remote digest check in each proxy launch script immediately before `exec`.
- `crates/aimx/src/ssh/live_tests.rs` and `sandbox_probe.rs`: REV5 reproductions became `live_ssh_*` regressions. A private user-space sshd uses a separate remote tree and temporary keys. The local `aimx` and `aim` conformance processes run under a macOS sandbox that denies reads and writes of that tree; a child probe proves local Rust filesystem access fails at the exact absolute path that succeeds through SSH.

Existing public Rust API, protocol types, persisted formats, and policy defaults were unchanged. No `aim-proto` or `aim-llm` contract change, kernel edit, or ADR was needed. No files outside the assigned SSH scope and this report were changed. `~/.ssh` and `~/.codex/auth.json` were untouched.

## Verification

- The new live tests reproduced B1, B2, M1–M9 and the relevant minors against the original slice before implementation. The sandbox probe also demonstrated that the prior local/remote assertion was not a real filesystem boundary.
- `mise run check`: passed after integration and rebase (rustfmt, verusfmt, workspace Clippy with `-D warnings`, workspace tests, `cargo xtask check`).
- `mise exec -- cargo test -p aimx -- --ignored live_`: **33 passed, 0 failed** (31 SSH smoke tests, one MCP smoke test from current `main`, and one binary conformance test). This includes a real OpenRouter remote edit/exec path, resident reconnect, agentless fallback, askpass, tampering, and filesystem isolation. A separate launch test refuses a replaced resident binary before execution.
- Localhost SSH performance test: 4 MiB read **165 ms**, listing 100 files **54 ms**. The review's original probes measured 4.84 s and 5.33 s on their respective fixtures; these are local measurements, not a network RTT claim.
- An intermediate smoke attempt found a test-filter bug in the sandbox child probe, which was fixed. Another attempt saw one temporary sshd startup failure under parallel load; its targeted retry and the final full smoke run passed. The test harness uses isolated temporary configuration and keys.

## Remaining limits

- The agentless mutation mutex prevents races within one workspace instance. Two independent clients can still race an `IfHash` check against publication; the resident server is the stronger cross-client option.
- Rehash and `exec` now occur in one remote script, but a malicious process with the same remote user privileges could still replace the path between those two shell operations. An executable file-descriptor design would be needed to remove that last seam.
- Agentless fallback grep uses POSIX extended regular expressions, which do not implement every ripgrep extension. Non-POSIX shell transport encoding grows script arguments substantially and may reach the OS argument limit for unusually large scripts.
- A resident proxy failure after client stdio has begun reconnects or returns an error; it cannot switch that established session to agentless mode transparently.

## Kernel candidates

Confinement prefix decisions, replay eligibility, and the six-process/three-transient-channel capacity rule are pure policy logic worth considering for `aim-kernel`. Output retention and edit application already reuse the shared pure `OutputRing` and `apply_edits` implementations; no new verified invariant was introduced here.
