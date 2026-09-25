# ADR 0009: Shadow workspace operations over SSH

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0002, 0006, 0008
- Scope: Remote workspaces and their execution boundary; provider traffic stays with aim.

## Context

`--ssh` must make every workspace operation target the remote host, including search and
third-party tools. A partial override can silently touch local files: pi's SSH example still
greps and finds locally (research/infra.md, §1, lines 82–90). Existing remote editors use the
system OpenSSH client and a resident server with a per-connection bridge (research/infra.md,
§1, lines 106–153). Agentless SSH costs extra round trips and cannot offer durable process
resume (research/infra.md, TL;DR, lines 13–35).

## Decision

- Use system OpenSSH so `ssh_config`, agents, ProxyJump, FIDO and known-host checks work.
  Start a ControlMaster at `~/.aim/ssh/%C`; route password, 2FA and host-key prompts to the
  CLI, TUI or web UI through `SSH_ASKPASS`. Never weaken host-key checking.
- Auto-bootstrap by default. Probe `uname -sm`; select a versioned `aimx` for the target
  from local cache, local build or release assets. The release manifest is signed by an
  ed25519 key compiled into aim. Verify artifact sha256 locally before upload, then hash
  installed bytes again using an independent remote utility where present. Use the local
  build's computed hash for development. Key the cache by digest and protocol generation.
- Run detached `aimx serve` on a remote user's 0700-directory unix socket, with pid file
  and flock. Each SSH connection runs `aimx proxy` as a bridge. Keep bounded process output
  rings (initially 8 MiB per process), a 30-minute default resume TTL, 5-second heartbeat,
  1-second to 2-minute reconnect backoff, and idle shutdown only with no clients or live
  processes. Resume tokens remain scoped to principal and harness instance.
- On signature, digest, target or execution failure, fall back to agentless sftp plus
  `sh -c` over the master. `bootstrap = never` forces this path. Send file bytes on stdin
  and use atomic temp-to-rename writes. Advertise reduced capabilities: no watch, PTY via
  `ssh -tt`, and no resume across drops. Never claim resident guarantees in this mode.
- Keep inference, credentials, sessions, memory, plugins and local-only MCP servers local.
  Read remote `AGENTS.md` and `.agents/` through `Workspace`, label their origin remote,
  and state the remote workspace in the model preamble. No provider key crosses SSH.
- Every workspace tool, including MCP, hooks, plugins, programs and code mode, must route
  through the session's `Workspace`/aimx target. Only the backends (`aimx::workspace::local`,
  `aimx::ssh`) and server plumbing may call `std::fs` or `std::process`; the `cargo xtask check`
  path rule guards this boundary (amended 2026-09-25; see ADR 0004).

## Consequences

Remote aimx gives one protocol, pipelined calls, search, watch and reconnectable processes.
Bootstrap requires signed release metadata and target builds. Agentless hosts remain usable
with visibly weaker guarantees; the caller must handle capability differences.

## Verification

- M1b live `remote_changed_local_untouched` conformance runs each tool source under SSH.
- M1b fault tests cover reconnect within/after TTL, replay bounds, tampered manifest,
  local and remote hash mismatch, failed execution, and agentless capability reporting.
- `cargo xtask check` rejects OS access under `crates/aimx/src` outside the backends and
  server plumbing (in force from M1a).
