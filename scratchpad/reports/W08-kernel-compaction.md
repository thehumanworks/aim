# W08 — verified kernel compaction planner

Branch: `agent/kernel/compaction` (pushed to `origin`); implementation commit `5d419bb`.
Worktree: `/Users/tomas/projects/aim-wt/kernel-compaction`.

## Built

- `crates/aim-kernel/src/compaction.rs`: replaced adjacency repair with the total
  `plan_cut(&[Item], u64) -> Result<usize, PlanError>` API. It selects the smallest cut whose
  verbatim tail fits the budget and whose matched call/result items stay together. Parallel and
  repeated call ids are handled without a uniqueness assumption. The implementation scans for the
  earliest matching call per result and sweeps candidate cuts downward, O(n²) time and O(1)
  additional space. It checks `u64` addition before proceeding.
- Added `kept_mask(&[Item], usize) -> Vec<bool>`. Retained `kept_tokens` and its checked accounting;
  removed adjacency-specific `repair_plan`, `is_pair`, `base_keep`, and `repaired` (no external
  callers). `PlanError` now has `Empty` and `PinnedToolItem` in addition to the existing variants.
- `crates/aim-kernel/tests/compaction.rs`: ten executable boundary cases, including sequential,
  parallel, repeated-id, straddling, exact-budget, pinned, empty, all-summarized, and 5,000-item
  cases.
- `docs/adr/0025-compaction-plan-invariants.md` and `docs/adr/README.md`: recorded the cut
  contract and its limits. The budget applies to the verbatim tail; the host must account for
  preserved pinned messages and the replacement summary/compaction item separately.

## Specs and proofs

- `answers(items, i, j)`: an earlier `ToolCall(id)` and later `ToolResult(id)`.
- `tail_tokens(items, k)`: mathematical tail token sum; `kept(items, k)`: tail plus pins.
- `plan_ok` and `cut_ok`: both marked `LOCKED(ADR-0025)`; exactly the pairing, pinning, shrinking,
  and tail-budget conditions in the W08 brief.
- `theorem_cut_yields_plan`: `cut_ok => plan_ok(kept)`.
- `theorem_tail_monotone`: moving the cut right cannot increase tail tokens.
- `theorem_all_summarized`: a nonempty transcript without pinned tool items has a valid cut at
  `len` for every nonnegative budget.
- Executable `plan_cut` proves validity and smallest-cut minimality; `kept_mask` proves its result
  equals `kept` for in-range cuts.

The only specification detail beyond the brief is that `tail_tokens` returns zero for an invalid
negative index; the cut spec accepts only indices from 1 through `len`. No requested invariant was
weakened. No `aim-proto` or `aim-llm` contract was changed. Pure decision logic is already in the
kernel; none remains to move there.

## Verification

- Post-rebase `mise run verify`: **97 kernel obligations verified, 0 errors** under
  `--no-cheating`. The initial uncached full run also verified 2,059 vstd obligations.
- Post-rebase `mise run check`: rustfmt, verusfmt, workspace Clippy with `-D warnings`, and all
  workspace tests passed. The final `cargo xtask check` step reports only that
  `compaction::plan_ok` and `compaction::cut_ok` are absent from
  `crates/aim-kernel/LOCKED.toml`. W08 explicitly protects that manifest and assigns
  `mise run locked:update` to the lead; the file was never edited here. Thus the aggregate
  `mise run check` command is **not yet green**. The lead must record the two digests and rerun it
  before merging into `main`.
- `cargo test -p aim-kernel --test compaction -- --nocapture`: **10 passed, 0 failed**. The
  5,000-item interleaved transcript took **3.24 ms** in that single debug test run; an earlier run
  measured **6.04 ms**. These are local timings, not a benchmark distribution.
- Live smoke: not applicable to this pure kernel change; no external integration was added.
- `git diff --check` passed before the commit; the branch was rebased on `origin/main` and pushed.

## Open handoff

The protected `LOCKED.toml` update and the resulting green aggregate quality gate remain with the
lead. No other known implementation issue remains.
