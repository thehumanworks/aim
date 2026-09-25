# ADR 0025: Cut transcripts without separating tool exchanges

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0007
- Scope: Pure selection of a transcript compaction cut and keep-mask; not summary quality, token estimates, provider behavior, or storage.

## Context

The context engine compacts old transcript items while preserving a verbatim tail
(`docs/architecture.md`, §6.4). Parallel calls may be interleaved with their results, so adjacency
cannot identify an exchange. The provider-neutral transcript carries a `call_id` on both calls and
results (`crates/aim-proto/src/conversation.rs`). A simple cut can preserve pinned messages before
it, but a pinned tool item would require an extra closure step over every matching exchange.

## Decision

Give each distinct host call id a `u64` identifier in the kernel representation. Do not require ids
to be unique: every earlier call and later result with the same id form a pair. A cut `k` summarizes
`[0, k)` and leaves `[k, len)` verbatim, while pinned messages before the cut remain kept. The cut
must remove at least one item, put both ends of every pair on the same side, and keep the verbatim
tail within its `u64` token budget. Reject pinned tool items; they cannot be handled by this simple
cut rule. The budget here is for the verbatim tail; the host must account separately for preserved
pinned messages and the replacement summary or provider compaction item.

Choose the smallest valid cut. Summarizing everything at `k = len` is a valid fallback for any
nonempty transcript without pinned tool items and a nonnegative budget. The implementation checks
token addition, and scans matching calls by id without assuming an id appears only once. `plan_ok`
and `cut_ok` are the locked decision specs in `crates/aim-kernel/src/compaction.rs`.

## Consequences

Parallel exchanges stay intact across compaction. The planner remains total: empty input and pinned
tool items have explicit errors; other inputs produce a cut, even when no verbatim tail fits. A
simple quadratic scan is acceptable for infrequent compaction on transcripts of thousands of items.
The host is responsible for mapping strings to stable, distinct `u64` ids and for estimating tokens.

## Verification

`theorem_cut_yields_plan` proves a valid cut produces a keep-mask preserving pins and pairs.
`theorem_tail_monotone` proves moving a cut right cannot increase the tail cost.
`theorem_all_summarized` proves the fallback cut exists. `plan_cut` ensures the smallest valid cut
and `kept_mask` ensures the defined keep-mask under Verus `--no-cheating`.
`crates/aim-kernel/tests/compaction.rs` exercises the erased executable boundary cases, including
parallel calls and a 5,000-item transcript. These proofs are conditional on input ids and token
counts; they do not prove the host's mapping, estimation, or external compaction effects
(`docs/architecture.md`, §13; `docs/adr/0005`).
