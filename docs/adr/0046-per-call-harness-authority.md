# ADR 0046: Carry narrowing authority on harness calls and sessions

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0008, 0021, 0027
- Scope: `aim-harness/1` authority fields and bounded read semantics; not authentication, path confinement, OS sandboxing, or policy proof changes.

## Context

ADR 0008 requires aimx to enforce every operation independently of aim. ADR 0021 makes delegated ceilings narrowing, and ADR 0027 proves the pure intersection decision. The original wire contract has no way to carry a read-only delegated grant: a writable principal can make a call on the same connection without a per-call ceiling (REV4-A finding 4; W11 policy-kernel report, “Proposed `aim-harness/1` per-call shape”). FIX9 also identified whole-file reads and hashing when callers only need a bounded prefix. These are protocol and I/O boundary gaps; the kernel proof alone does not close them.

## Decision

Add `CallScope {roots, ops, deny_write, max_processes, max_output_bytes}`. `roots` and `ops` are required fields. Paths are normalized workspace-relative or target-host absolute prefixes; aimx rejects malformed paths. Operation strings are `read`, `write`, and `exec`; aimx rejects unknown values with `invalid_params`. Missing `deny_write` means no extra write-denied prefixes, and missing limits mean no extra limit. Explicitly empty roots or ops deny all operations. An omitted scope means no extra narrowing.

Every actionable `aim-harness/1` request parameter carries an optional `scope`. `initialize`, `workspace.open`, and `tools.list` do not. `workspace.open` instead accepts an optional `ceiling` bound to that opened workspace session. A later call cannot widen or omit this bound ceiling. aimx validates requested scopes, refuses a widening request, then evaluates `policy::effective([principal grant, session ceiling if present, per-call scope if present])` before dispatch. The authenticated principal is always the first operand: an empty fold is unrestricted in ADR 0027. This applies to direct RPC, tools, MCP, search, and Bash as `exec` at its cwd. Denied actions report `denied` with a reason. A process control request is judged under its bound process authority and the supplied call scope; omitting a per-call scope never erases the session ceiling.

A backend serving a scoped call must check the effective authority against the target resolved by the **same descriptor walk used for the operation**, including both endpoints of copy/rename, search starts, and process cwd. A separate pathname precheck is raceable. `Workspace::scoped` supplies a request-bound backend view; a backend unable to enforce the resolved target refuses the scoped call. The local backend shares its held root, process registry, and mutation lock across these views. Resident SSH calls are checked by the remote aimx; agentless SSH does not claim per-call subroot enforcement.

`fs.read` gains `hash`, defaulting to `true` for existing callers. When false, aimx returns only the requested or capped bytes and `FsReadResult.hash` is absent. `fs.read_many` gains `prefix_only`, defaulting to `false`; when true, aimx reads at most `max_bytes_per_file` (clamped to the harness limit) for each file, does not hash beyond the returned prefix, and omits the whole-file hash. `FsReadResult.hash` is optional to represent these cases. A whole-file hash, when present, still covers the whole file and remains suitable for `IfHash`; no prefix hash is passed off as a whole-file precondition. `truncated` reflects the returned byte cap.

## Consequences

Existing request JSON remains valid through defaults, and existing `fs.read` callers retain whole-file hashing. Typed Rust constructors must set the new optional fields. The nullable read hash requires callers needing `IfHash` to request hashing and handle its absence. Session ceilings protect delegated connections even if a later request omits `scope`; per-call narrowing protects individual requests. aimx remains responsible for path normalization, protected subtree mutations, symlink confinement, process ownership, and OS effects.

Root prefixes grant authority over resolved path names. A hard link inside an allowed prefix is still an allowed name for the same inode; this path policy does not isolate inodes across hard links. `Exec` authorizes the resolved cwd but is not a filesystem sandbox for commands run there. A separate OS sandbox is needed to confine a process's later file access (ADR 0027; `docs/architecture.md`, §13).

## Verification

ADR 0027's `policy::effective`, `Scope::narrows`, and `Scope::permits` proofs establish the pure scope decision. Wire tests check old JSON defaults, required roots/ops, explicit empty scopes, and optional hashes. aimx boundary tests named `delegation_cannot_widen`, `deny_overrides_allow`, and `yolo_no_prompt_within_grant` must establish admission and denial, including mutating tools. A measured large-file prefix test must establish that bytes actually read stay below the cap; stdio and SSH live smoke establish the remote boundary. None of these runtime observations are claimed by this ADR alone.

`scoped_prefix_cannot_cross_an_in_workspace_symlink` checks that a delegated subroot cannot read, search, write, or execute through an in-workspace symlink into another subtree. A local backend unit test checks the same resolved-target rule and shared process registry. These tests evidence the I/O shell assumption, not the Verus proof itself.
