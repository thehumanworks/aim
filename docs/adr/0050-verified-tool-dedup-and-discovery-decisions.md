# ADR 0050: Verify tool ceilings, mutation replay, and discovery admission

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0005, 0008, 0033, 0038
- Scope: Pure decisions used by named-agent tool ceilings, aimx's mutation dedup table, and resource-read budgets. The shell still owns authenticated identity, exact name encoding, UUIDv7 parsing, clocks, I/O and recorded outcomes.

## Context

Architecture §1 puts policies and budgets in the verified functional core. ADR 0038 requires the named-agent ceiling retained by a resumed session to narrow its tools, and its resource discovery fix admits every fixed and listed file against one budget before reading. ADR 0008 requires idempotent mutation outcomes; FIX4/FIX10 distinguished an admission refusal before execution from an attempted mutation and retained keys after result eviction. The implementations and example tests existed in shells, where a later branch change would not invalidate any proof.

The tool policy's names are unbounded UTF-8 strings. Encoding them with a hash would make a collision capable of changing an allow or deny decision. Instead, each host call builds a sorted union of its participating names, assigns sequential `u64` ids, and keeps the exact reverse map. This is a collision-free bijection for the names in that call; the kernel receives only ids. The shell checks the mapping round-trip in tests. The kernel proof assumes the host faithfully maps the same name to the same id within the call.

## Decision

`aim-kernel::agent_tools` owns tool-name membership, deny precedence, unrestricted status, and intersection. A finite intersection materializes only names both inputs permit; its denial list can then be empty. When both allowlists are absent, it unions denials. This may normalize the serialized `SessionAgent` allow/deny fields differently, but preserves permission for every name. The shell keeps the same wire and persisted types and uses the kernel result to construct them.

`aim-kernel::dedup` owns the decision for an absent, in-flight, completed, or tombstoned key. A pre-execution admission refusal discards an in-flight reservation; any attempted outcome, including a failure, becomes Done. Evicting Done leaves a tombstone until the horizon. A stale UUIDv7 key with no retained record, or any tombstone, yields `unknown_outcome`, never Execute. The shell still parses UUIDv7, checks future skew, computes its stale flag, and stores bounded keyed tombstones. A key without a mint time is indistinguishable from a new key after its horizon and may execute again; this states ADR 0008's opaque-key limit explicitly. The trace theorem covers attempts before `HorizonElapsed`.

`aim-kernel::discovery` owns a file and byte credit counter. A batch reserves worst-case bytes for each admitted file before I/O. Settlement charges bytes returned even if the parser later discards the file, refunds a missing file, and charges the whole cap for a failed read. No reservation or settlement can exceed the original limits. Zero per-file cap admits no files, preserving the prior shell behavior. The `Files` backend remains responsible for returning at most its advertised per-file prefix and one result per requested path.

## Consequences

The shell has small, auditable translations between exact names and ids, table rows and per-key phases, and read outcomes and byte charges. Kernelization does not prove those translations, UUID parsing, concurrent table locking, backend byte limits, or actual external effects. The existing integration and live smoke tests cover those boundaries. Policy intersection now allocates a temporary registry; agent definitions and resumed ceilings are small and intersection occurs at session setup, while each tool membership check constructs a registry from its current policy.

## Verification

- `agent_tools::tool_intersection_ok`, `theorem_deny_overrides`, `theorem_intersection_commutative`, and `theorem_intersection_narrows` prove the permission relation. Host tests check Unicode and prefix-distinct name/id roundtrips and recorded ceiling behavior.
- `dedup::theorem_execute_requires_fresh_absent` proves that every Execute decision in the locked begin table has an absent, non-stale key. `theorem_refusal_and_retention` checks representative result states, and `theorem_never_executes_twice_within_horizon` proves the trace rule; that trace currently also gates Begin on Absent independently of the table. aimx's property and conformance tests exercise the shell's records, eviction and UUIDv7 parsing.
- `discovery::theorem_budget_bounds` proves, over every prefix of a trace starting at `Budget::new`, that total admitted files stay within the file limit and settled charges plus pending reservations stay within the byte limit. The executable reserve and settle postconditions match the trace transitions; settle now also guarantees `Err` exactly when no reservation is pending. Existing resource tests exercise fixed/listed reads and precedence; kernel boundary tests cover failures, missing reads, zero cap, and `u64::MAX`.
- The W23 scratchpad report records the integrated `mise run check`, `mise run verify`, and applicable live smoke results.
