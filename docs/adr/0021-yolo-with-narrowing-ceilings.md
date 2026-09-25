# ADR 0021: Default to yolo within narrowing ceilings

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0008, 0020
- Scope: Default tool approval policy and delegation; not the gate's promotion criteria.

## Context

The maintainer's standing preference is a prompt-free yolo default, carried forward from tny
ADRs 0001 and 0159 (`docs/architecture.md`, §12, lines 642–646;
`docs/research/tny-lessons.md`, §Agent defaults and ACP, lines 207–214). A prompt-free default cannot grant
a subagent, plugin, program, remote principal, or hook more authority than its parent grant.
The protected evaluator and LOCKED specs cannot depend on an in-process deny list alone.

## Decision

Make yolo an explicit user-level policy: routine permitted calls run without interactive
approval prompts. Effective authority is the intersection of the authenticated principal's
grant, session policy, agent/source ceiling, and any per-call narrowing. Delegation may only
reduce scope or limits. Deny overrides allow; hooks may deny but cannot silently authorize.

Apply the same `aim-kernel` policy decision at aim's dispatcher admission and aimx's independent
enforcement for every caller (`docs/architecture.md`, §12, lines 628–647). This covers direct
aimx clients as well as the native agent, MCP, plugins, code mode, and programs.

Keep the protected set write-denied by the baseline OS sandbox even in yolo. Candidate sandboxes
cannot write the gate, evaluator, policy files, LOCKED specs, or manifest. A request to broaden
authority needs a new explicit user policy/grant, not a hook result or child request
(`docs/architecture.md`, §§10.1, 12, lines 567–575 and 642–649).

## Consequences

Permitted work avoids approval latency while policy remains bounded. The UI must show the active
grant and denial reason clearly; callers cannot interpret yolo as unrestricted host access.

## Verification

In M1a, add `yolo_no_prompt_within_grant`, `deny_overrides_allow`,
`delegation_cannot_widen`, and `hook_cannot_authorize` conformance tests at both dispatcher
and aimx boundaries. In M10, test protected-path writes from an ordinary yolo session and
from a candidate sandbox. Kernel policy proofs are conditional on authenticated principals,
normalized paths, and the OS sandbox actually enforcing its profile.
