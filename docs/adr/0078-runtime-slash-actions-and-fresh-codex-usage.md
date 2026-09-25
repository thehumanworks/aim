# ADR 0078: Execute slash actions at runtime and fetch fresh account limits

- Status: Accepted
- Date: 2026-09-25
- Supersedes: 0077
- Baseline: 0008, 0009, 0010, 0014, 0046
- Scope: User-configured executable TUI commands, their output lifecycle, and fresh Codex account usage.

## Context

ADR 0077 incorrectly restricted the requested slash-command feature to text templates and
cached subscription windows. The maintainer clarified that commands must execute dynamically
at invocation time and `/status` must pull fresh limits. Merely asking an agent to execute a
tool is not equivalent: user-only command output must stay out of model turns and session history.

Codex's account backend supports GET `/backend-api/wham/usage`, independently of the Responses
API. The upstream [backend client](https://github.com/openai/codex/blob/main/codex-rs/backend-client/src/client.rs)
and [rate-limit tests](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/suite/v2/rate_limits.rs)
describe primary/secondary windows and additional metered families. This internal API is not
an OpenAI API-key usage endpoint; live verification is required for its schema.

## Decision

Keep the user-owned `tui.json` presentation settings, help/completion registry, and explicit
`output: agent | user`. Extend commands with:

- `run: [program, arg, …]`: execute an argv vector through the invoking workspace's Bash tool.
  Expand aim argument placeholders in each element, then shell-quote each whole element.
- `tool: {name, arguments}`: call a workspace harness tool directly. Expand string values,
  not object keys, in the structured JSON arguments. Unknown tools fail at the harness.
- `text`: without an executable action, keep the original prompt-template behavior. With an
  action, optionally wrap its result using `{{output}}`. Insert runtime output verbatim after
  expanding the template's arguments, without recursively interpreting its contents.
- `timeout_ms`: positive and at most 600000, default 30000, including workspace connection
  and tool I/O. Exactly one of `run` and `tool` may be set; a command must have an action or text.

Execute only on explicit user invocation, never while discovering configuration. No project
configuration is auto-executed. `Options::workspaces` injects the normal workspace factory: each
invocation connects a dedicated harness using the attached session's captured root and location,
including SSH/network workspaces. These are **user** actions, under that connection's authenticated
principal and aimx enforcement, not delegated model calls or a means for an agent to acquire tools
outside its ceiling. The native model/ACP backend is not called. No output is broadcast into the
session event log; only a successful `agent` result becomes an ordinary prompt/steering message.

Only one action runs at a time. Ctrl+C and `/cancel` cancel it before cancelling an agent turn.
Switching/reattaching sessions and exiting also cancel it. Bind completion to the invocation id,
session id and attach generation; discard stale completion. Failed actions, connection errors,
timeouts, truncated/non-text results and output over 64 KiB are local errors, never agent prompts.
Do not retry executable actions. Close the dedicated harness on completion/cancellation to release
processes and handles; background Bash actions are refused rather than claiming to start a job
that would immediately be released. New actions cannot overlap a worker still cleaning up.

`/status` is an async, always-user-only action that **fetches on every invocation**. It uses this
client's Codex credentials and the configured Codex base URL's sibling `../wham/usage` endpoint,
on any active provider and before any model turn. Apply the provider's request deadline and a
256 KiB body limit. Normalize primary/secondary, code-review and additional-family windows, but
discard account identifiers, email and opaque native metadata. Do not display HTTP error bodies,
follow redirects, reuse cached usage, or send a model request as a fallback. Codex HTTP clients
now refuse redirects so custom authentication headers cannot follow them to another origin.

## Consequences

Users can run scripts and tools directly, with either a private local result or an expanded
agent prompt, without an agent request to orchestrate execution. Every command sees current
workspace state. The existing harness owns filesystem/process access and remote shadowing.
Command-specific processes/handles do not outlive the invocation; persistent background jobs
and tools outside the workspace harness are not part of this contract.

Configured executables have the user's authority and may mutate data. Installing a native user
command is deliberate trust in its executable/script; shell scripts explicitly passed to
`sh -c` or Bash still interpret shell syntax. See [the TUI guide](../tui.md).

## Verification

- `aim_kernel::slash::delivery` and `no_unintended_agent_output` prove that stale, failed and
  user-only results cannot be routed to an agent; `timeout` proves accepted deadlines are bounded.
- TUI app tests fence cancellation, errors and session switches and route successful results.
- `runtime_commands_execute_each_time_and_route_real_tool_output` runs real commands and
  a direct Read tool through aimx, observes changed data, and checks prompt-history isolation.
- `cancelling_a_runtime_command_releases_its_real_process` tests actual process cancellation.
- `status_fetches_each_time_without_any_agent_turn_or_history` drives the real TUI against a
  local account endpoint, checks two fresh GETs with distinct percentages and no history.
- Provider offline tests cover authorization, no cache/model calls, invalid data, response
  bounds, HTTP errors, redirects and timeout.
- `live_codex_account_limits` passed on 2026-09-25: two fresh calls to the real authenticated
  account service, normalized windows returned, no credentials or account metadata printed.
