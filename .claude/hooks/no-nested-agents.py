#!/usr/bin/python3
"""PreToolUse guard: sub-agents may not spawn sub-agents (maintainer policy, 2026-09-25).

Claude Code includes `agent_id` in a hook's input only when the tool call comes from inside a
sub-agent, so the lead session keeps the Agent tool and every worker loses it. Exit code 2
blocks the call and shows stderr to the model.
"""
import json
import sys

try:
    call = json.load(sys.stdin)
except ValueError:
    sys.exit(0)
if call.get("agent_id"):
    print("Sub-agents may not spawn sub-agents in this repository: do the work yourself.", file=sys.stderr)
    sys.exit(2)
sys.exit(0)
