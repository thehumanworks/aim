# Personalizing the TUI

Create `~/.aim/tui.json` (or `$AIM_HOME/tui.json` when using a custom aim home), then restart the TUI:

```json
{
  "fullscreen": false,
  "plain": false,
  "status": ["limits", "workspace", "tokens"],
  "commands": {
    "review": {
      "description": "Review a change for correctness and missing tests",
      "output": "agent",
      "text": "Review $ARGUMENTS. Check correctness, regressions and missing tests. Do not modify files."
    },
    "checklist": {
      "description": "Show my release checklist privately",
      "output": "user",
      "text": "Release $1 checklist:\n- Run checks\n- Review the diff\n- Obtain approval before publishing"
    }
  }
}
```

Type `/review the current diff` to send the expanded prompt to the agent. If a turn is running,
it becomes steering, just like ordinary input. Type `/checklist v2` to print a local notice:
neither that invocation nor its output is sent to the agent or saved to prompt history.
Both commands appear in slash completion and `/help`.

## Templates and visibility

- `output` is required: `agent` sends a prompt; `user` displays only in this TUI.
- `$ARGUMENTS` inserts all arguments; `$1`, `$2`, … insert shell-style quoted arguments, counted
  from one; `$$` inserts a dollar sign. Missing positional arguments expand to empty text.
- Expansion is text substitution, **not shell execution**. Backticks and shell snippets stay
  literal. Expanded text starting with `/` is not dispatched again.
- Prompt templates can instruct the agent to follow a routine or use tools through its normal
  authority. Direct harness-tool execution without an agent turn is not implemented here.
- Command names contain ASCII letters, digits, `-` or `_`, without the leading slash.
  Built-ins cannot be replaced. Descriptions appear in help and completion.
- Commands are user-wide, including when attached to a remote workspace. Project files do not
  override them. This file is separate from the existing resource catalog's Markdown prompts.
- Invalid configuration stops startup with an error rather than silently changing output
  visibility. The file limit is 64 KiB. Do not put credentials in templates.

User-only notices are terminal-local: they are not restored after reattachment and do not enter
conversation search, compaction, or model context. They still appear in terminal scrollback
(and any terminal recording you enable).

## Presentation

All keys are optional except fields within a command definition:

| Key | Default | Effect |
| --- | --- | --- |
| `fullscreen` | `false` | Initial fullscreen layout; `--fullscreen` also enables it; `/fullscreen` toggles it. |
| `plain` | `false` | Use the plain theme rather than automatic terminal theme detection. |
| `status` | `["tokens", "limits", "workspace"]` | Optional status-line segments, in display order. Use `[]` to hide them all. |
| `commands` | `{}` | Named command definitions. |

Essential state, model and privacy indicators remain visible. Settings apply to both inline and
fullscreen layouts; agent-authored declarative UI surfaces continue to render independently.

## `/status`: ChatGPT / Codex subscription usage

In a Codex session, `/status` prints **the latest subscription limits reported by the provider**:
each metered window's percentage used, duration in minutes, and reset time in Unix seconds.
These are subscription usage rates, not conversation token counts.

This command is always user-only. It does not send a prompt, run a model request, or refresh an
account endpoint. Before a turn has reported limits (including after a fresh attachment), it says
no data is available; in another provider's session it explains that Codex is required. The output
explicitly says it is not a live refresh, so older reported data cannot be mistaken for a current
account lookup.
