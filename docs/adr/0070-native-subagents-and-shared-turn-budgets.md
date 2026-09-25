# ADR 0070: Run bounded native child sessions with narrowed authority

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0005, 0027, 0031, 0038, 0050
- Scope: Native parent-to-child session delegation, its authority and lifecycle, and verified depth, concurrency, and turn limits. ACP-hosted parents are excluded.

## Context

The dispatcher already reserves agent-tool routing for subagents (`docs/architecture.md`, §6.3), and named agents carry allow and deny ceilings across resume (ADR 0038). Research on delegated agents distinguishes child contexts from peer team members and records both the usefulness and the cost of concurrency (`docs/research/agents-conventions.md`, §3; `docs/research/swarm.md`, §§1–2). A recursive delegate needs explicit depth, concurrency, and usage bounds so a model cannot create unbounded descendants or spend outside its parent's turn budget.

## Decision

The native `agent` tool starts a child session from a `{ prompt, agent?, model?, effort?, description }` request. It returns the child's final assistant message within a bounded response, with the child session id as a recovery handle for `read_session`. Calls in one parent turn may run concurrently. A child inherits its parent's workspace location, root, and persistence class. Its tool ceiling is the intersection of the parent's effective ceiling and its named definition's ceiling, using ADR 0050's verified `agent_tools` decision. Child metadata records the parent session; additive `subagent.start` and `subagent.stop` updates carry the parent session id and originating call id. Closing or cancelling a parent cancels live children. A crashed child becomes that call's tool error; a refused spawn does not fail the parent session. ACP-hosted parents refuse delegation because the ACP bridge does not enforce the native ceiling (ADR 0038).

The default maximum depth is **2** edges from a root session, whose depth is zero. Each session may have at most **4** live direct children. A child receives at most **8** turns, further capped by its parent's remaining shared turn credit. Every child turn debits both the child's local credit and the parent's shared credit. A new child is refused if depth, concurrency, or parent credit is exhausted. The host atomically reserves live-child slots and serializes charges to a shared parent counter; the kernel receives observed integer counts and returns the admission or charge decision. The kernel does not itself schedule or cancel sessions.

## Consequences

The limits stop recursive and per-parent fanout by default. Sibling and descendant child turns compete for the root's shared child-turn credit; each child turn debits the child and every ancestor's balance. A root's own turns do not spend child-turn credit. Concurrent children can exhaust credit another child expected to use, so a child's initial cap is an upper bound, not a reserved allocation. A finished or cancelled child releases its live slot. Credits are live-session state and reset when a root session resumes; child sessions remain readable but refuse standalone resume without their live parent's ceiling and credit. These counters bound delegated turns, while provider tokens and money remain subject to separately configured budgets.

## Verification

`aim-kernel::subagents::theorem_admitted_bounds` and `theorem_spawn_limits` prove admission respects depth, live-child, and initial turn limits. `theorem_child_turn_counts_toward_parent` proves each admitted child turn spends exactly one unit from both balances. The executable `admit_child` and `charge_child_turn` functions refine the `LOCKED(ADR-0070)` specs. Host tests exercise the serialization boundary, ceiling narrowing, concurrent children, persistence inheritance, and cancellation; a live OpenRouter turn exercises two parallel children including a read-only definition. Kernel proofs do not establish that the host supplies accurate counts, releases slots, or actually cancels provider work.
