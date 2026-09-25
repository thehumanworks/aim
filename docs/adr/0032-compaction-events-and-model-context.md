# ADR 0032: Record compaction as events; rebuild the model's context from the log

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0010, 0025
- Scope: the provider compaction capability, the `Compacted` update and event, and how readers rebuild the model's context; not the cut decision (ADR 0025) or the trigger thresholds (policy defaults in `agent::compact`).

## Context

Long sessions outgrow the model's window (architecture §6.4).
- codex offers remote compaction: a trailing `compaction_trigger` returns one encrypted `compaction` item (docs/research/codex-backend.md §7; W01's `live_compaction_v2`).
- The OpenAI-compatible gateways offer none, so aim summarizes locally.
- The session log is append-only (ADR 0007) and must keep everything, because conversation search and the user's scrollback need the full history (§6.9). The model, however, must continue from the compacted context, also after a restart.

## Decision

**Provider capability.** `ModelProvider::compact(request) -> Option<Item>` is additive, and its default is `None`, meaning unsupported. The returned provider-native item is valid only for the provider that made it. codex implements it with V2 compaction.

**Contract.** The additive contract is:
- `SessionUpdate::Compacted { replaced, items, method, tokens_before, tokens_after }`;
- `EventBody::Compacted { replaced, items }`.

The first `replaced` items **of the model's context as rebuilt so far** were replaced by `items`: the compaction item or local summary, followed by any pinned items kept from before the cut.

**Two views of one log.** `host::model_items_of(events)` folds item events and applies each `Compacted` in order; it is the model's context on resume. `items_of(events)` keeps every item; it is the user's transcript returned by `attach`. Old readers preserve `compacted` as an unknown event kind.

**Summary items.** A local summary is a `User` item opening with a fixed heading (`agent::compact::SUMMARY_HEADING`). It is requested with the conversation's own instructions and tools, so the provider's prompt cache still covers the prefix. Its usage is accounted like any request.

## Consequences

- Nothing is lost: search, scrollback and audits read the full log.
- Every consumer that rebuilds model context must use the fold, never a plain item list.
- A session cannot switch providers after a remote compaction, because the item is provider-native. The provider is fixed per session today.
- A second remote compaction whose prefix already starts with a codex compaction item has not been exercised live yet (unverified).

## Verification

- `crates/aim/tests/compaction.rs`: local summary, remote compaction, no compaction below the threshold, overflow retried once, a second overflow failing, and `the_model_context_is_rebuilt_from_the_log_with_compactions_applied`.
- Live: `live_compaction_codex_remote_keeps_the_facts` (~16,250 → ~2,045 estimated tokens, 5.9 s) and `live_compaction_local_summary_keeps_the_facts` (OpenRouter, ~16,250 → ~1,811, 8.3 s). Both still answer a fact stated only at the start (commit 55b885b).
- Cut safety: ADR 0025's locked kernel specs.
