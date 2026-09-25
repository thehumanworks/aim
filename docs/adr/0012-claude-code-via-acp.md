# ADR 0012: Run Claude Code through ACP with explicit tool authority

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0008, 0009
- Scope: Claude Code as an external ACP agent, not aim's native provider loop.

## Context

`claude-agent-acp` runs Claude's built-in tools inside its own process; its ACP client
`fs` and `terminal` methods do not intercept them (research/acp-mcp.md, §2.7, lines
262–313). Tool substitution through injected MCP was verified in tny on an earlier adapter,
but the pinned current adapter needs a live conformance run (research/acp-mcp.md, TL;DR,
lines 7–17). The adapter also changes rapidly, and exact version checks age poorly
(research/acp-mcp.md, §2.9, lines 330–342).

## Decision

- aim is the ACP v1 client using `agent-client-protocol` 2.2. Pin `claude-agent-acp` in mise
  and run a live capability probe at startup. A version string alone is not authority to
  enable a feature (research/acp-mcp.md, §1 and §2.9).
- Offer two explicit per-session authority modes. In **aim tools**, disable Claude built-ins
  with `_meta.claudeCode.options`, map tool aliases to `mcp__aim__*`, set
  `strictMcpConfig`, `settingSources` and `allowedTools`, and inject aim's MCP server with
  schemas compatible with Claude's expected tool shapes. Calls cross aim's dispatcher and
  aimx. This mode is required under `--ssh`; reject SSH if the probe cannot establish it.
- In **native tools**, Claude runs its built-ins locally. ACP updates are rendered and
  logged, but those calls bypass aim admission, hooks and SSH shadowing. Label this blind
  spot in the UI. Use native mode locally until the pinned adapter passes conformance,
  then make aim tools the local default.
- `aim login claude` runs the adapter's advertised terminal-auth command in the current
  terminal. The TUI runs it in a PTY pane. Do not send an API-key-style `authenticate`
  request for subscription login (research/acp-mcp.md, §2.2, lines 167–192).
- Set model, effort, mode and fast options through `session/set_config_option`, using the
  adapter's advertised options (research/acp-mcp.md, §2.4, lines 212–225). For a remote
  workspace, give the adapter an existing local scratch cwd while all workspace tool
  calls target remote aimx; the ACP session cwd must exist on the adapter host.
- Private and ephemeral sessions request `persistSession:false`; refuse them if a live
  adapter test cannot confirm no local transcript remains.

## Consequences

ACP keeps Claude's own loop and login behavior. The aim-tools path can enforce SSH
shadowing, while native mode exposes a clearly labelled authority gap. Adapter upgrades
must be retested for tool replacement, authentication, config and privacy behavior.

## Verification

- M2-llm `live_claude_acp_capabilities` and `live_claude_config_options` exercise the
  pinned adapter. M2b `live_claude_ssh_remote_changed_local_untouched` must cover Read,
  Edit, Bash, Glob and Grep via injected MCP before SSH is enabled.
- M2a `live_claude_private_no_transcript` checks the adapter's persistence promise.
