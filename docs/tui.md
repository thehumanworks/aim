# Personalizing the TUI

Create `~/.aim/tui.json` (or `$AIM_HOME/tui.json`), then restart the TUI:

```json
{
  "fullscreen": false,
  "plain": false,
  "status": ["limits", "workspace", "tokens"],
  "commands": {
    "reviewdiff": {
      "description": "Run git diff and send the result for review",
      "output": "agent",
      "run": ["git", "diff", "--"],
      "text": "Review this freshly collected diff for correctness and regressions:\n{{output}}"
    },
    "branch": {
      "description": "Show the current branch privately",
      "output": "user",
      "run": ["git", "branch", "--show-current"]
    },
    "inspect": {
      "description": "Read a workspace file and ask the agent to inspect it",
      "output": "agent",
      "tool": {
        "name": "Read",
        "arguments": {"file_path": "$1"}
      },
      "text": "Inspect $1:\n{{output}}"
    },
    "review": {
      "description": "A plain prompt template",
      "output": "agent",
      "text": "Review $ARGUMENTS. Do not modify files."
    }
  }
}
```

`/reviewdiff` **executes git at invocation time**, then sends its actual result to the agent.
`/branch` executes git too, but displays the result only to you. `/inspect src/main.rs` executes
the harness's Read tool before expanding its result into a prompt. No model turn is used to
orchestrate these actions. Commands appear in slash completion and `/help`.

## Runtime actions and arguments

A command requires `description`, `output`, and either `run`, `tool`, or `text`.
`run` and `tool` are mutually exclusive. `text` may wrap either runtime action:

- `run` is a nonempty argv array. It runs through aimx's Bash tool in the **attached workspace**,
  including remote/SSH workspaces. Each expanded argument is shell-quoted as a whole; typing
  shell metacharacters into an ordinary argument does not execute them.
- `tool` invokes a workspace harness tool by name with structured JSON arguments. Only string
  values are template-expanded; keys, numbers and booleans retain their types.
- `text` alone is a prompt or local notice. With a runtime action, it is an optional wrapper:
  `{{output}}` inserts the actual tool result verbatim. Without a wrapper, use the result itself.
- `$ARGUMENTS` inserts all invocation arguments as one string. `$1`, `$2`, … select shell-style
  quoted arguments, counted from one. `$$` produces a literal dollar sign. Missing positional
  arguments become empty strings. Output is never recursively expanded or dispatched as a slash.
- `timeout_ms` defaults to 30000 and accepts 1–600000, covering connection and tool execution.
- Background Bash jobs are not supported: invocation-specific processes and output handles are
  released when the command finishes. Use a foreground command. Output must be text, complete,
  and at most 64 KiB including its wrapper; otherwise the TUI shows an error.

For a script, use e.g. `"run": ["python3", "scripts/my-routine.py", "$ARGUMENTS"]`.
You may explicitly configure `sh -c` or a Bash tool call, but **those scripts interpret shell
syntax**; do not interpolate untrusted text into a shell script. Existing shell dollar signs in
a template need escaping as `$$`.

These are user-invoked actions under your workspace harness credentials and permissions, not
agent tool calls. Executables may change files or contact services. Trust the commands/scripts
you configure. No project config is loaded or executed automatically, and no executable runs
merely because it was discovered. Do not put credentials in configuration or printed output.

## Output visibility and lifecycle

- `output: "agent"`: successful output becomes an ordinary prompt (steering if a turn is
  running), with ordinary session and history rules.
- `output: "user"`: display only in this TUI. Neither invocation nor result enters model
  context, prompt history, stored conversation events, compaction, or conversation search.
- Failures and timeouts are shown only to the user, regardless of the declared output channel.
  The harness never automatically retries a command that might have side effects.
- One runtime action runs at a time. **Ctrl+C** or **`/cancel`** cancels it before cancelling an
  agent turn. Changing sessions or exiting also cancels it; late results cannot reach another
  session. If cleanup is still running, wait before retrying another command.
- User-only results are terminal-local and not restored on reattachment. They still appear in
  terminal scrollback and any terminal recording you enable.

Command names contain ASCII letters, digits, `-` or `_`, without the slash. Built-ins cannot
be replaced. Configuration is user-wide and loaded at TUI startup, separately from Markdown
resource-catalog prompts. Unknown fields, conflicting actions, invalid deadlines/names and
files larger than 64 KiB stop startup with an error rather than silently changing visibility.

## Presentation

| Key | Default | Effect |
| --- | --- | --- |
| `fullscreen` | `false` | Initial fullscreen layout; `--fullscreen` also enables it; `/fullscreen` toggles it. |
| `plain` | `false` | Plain theme rather than automatic theme detection. |
| `status` | `["tokens", "limits", "workspace"]` | Optional status-line segments in display order; `[]` hides them. |
| `commands` | `{}` | User-defined command definitions. |

Essential state, model and privacy indicators remain visible. Settings apply to inline and
fullscreen layouts; agent-authored UI surfaces continue to render independently.

## `/status`: fresh ChatGPT / Codex subscription limits

Every invocation performs an authenticated account-usage GET, even before your first model turn
and when the active session uses a different provider. It uses **this TUI client's Codex login**
(`aim login codex`, or the Codex CLI's credentials read-only). The configured
`AIM_CODEX_BASE_URL` determines the same-origin sibling `../wham/usage` endpoint.

The command prints fresh percentage-used values, durations in minutes and reset times in Unix
seconds for all returned metered windows, including additional families and code-review quotas.
These are subscription limits, not the current conversation's token counts. A provider window
length that is not a whole minute is rounded up for display.

`/status` is always user-only and never spends a model request. It does **not** fall back to
cached values on error. Authentication failures, HTTP errors, invalid responses and timeouts
are reported locally. Account identifiers, email and opaque response metadata are discarded.
