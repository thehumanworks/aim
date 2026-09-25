//! The context engine's compaction (docs/architecture.md §6.4).
//!
//! **When.** Before each model request, the engine estimates the context size. The estimate
//! combines the provider-measured size of the last request plus response with a byte estimate
//! of the items added since. When it passes [`COMPACT_AT_PERCENT`] of the model's window, the
//! engine compacts. It also compacts, once, when the provider reports a context overflow.
//!
//! **What.** The transcript is **cut**:
//! - items before the cut are replaced by one provider compaction item (remote, e.g. codex V2)
//!   or, when the provider has none, by a local handoff summary;
//! - items after the cut stay verbatim;
//! - pinned items stay too (the running turn's own prompt is pinned).
//!
//! The cut keeps the longest tail that fits [`KEEP_PERCENT`] of the window and never separates
//! a tool call from its result, matched by call id so parallel calls are handled. That decision
//! is the verified kernel's `compaction::plan_cut` (ADR 0025, LOCKED).
//!
//! **Nothing is lost.** The session log keeps every item. The model's context is rebuilt by
//! applying the recorded `Compacted` events.

use std::collections::HashMap;

use aim_kernel::compaction::{Item as PlanItem, ItemKind};
use aim_proto::conversation::{Item, Part};

/// Compaction starts when the estimated context passes this share of the window.
pub const COMPACT_AT_PERCENT: u64 = 85;
/// The verbatim tail may use this share of the window.
pub const KEEP_PERCENT: u64 = 20;

/// Heading of a local summary, so people and models recognise it.
pub const SUMMARY_HEADING: &str = "[aim: summary of the earlier conversation — the full history is in the session log]";

/// Instructions for a local summary.
pub const SUMMARY_INSTRUCTIONS: &str = "You are compacting a coding agent's conversation so the agent can continue \
from your summary alone. Write a handoff: the user's goal and constraints; decisions made and why; files read or \
changed (paths) and their state; commands run and their outcomes; open problems and next steps; facts learned that \
are still needed. Be concrete and complete but terse. Do not call tools. Do not address the user.";

/// The request that asks for the summary, appended after the items being summarized.
pub const SUMMARY_REQUEST: &str = "Summarize the conversation above as a handoff, as instructed.";

/// Estimated tokens of an item: its JSON size / 4, rounded up, at least 1.
#[must_use]
pub fn estimate(item: &Item) -> u64 {
    let bytes = serde_json::to_vec(item).map_or(0, |v| v.len());
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX).max(1)
}

/// Estimated tokens of some text.
#[must_use]
pub fn estimate_text(text: &str) -> u64 {
    u64::try_from(text.len().div_ceil(4)).unwrap_or(u64::MAX)
}

/// The planner's view of a transcript. Call ids become dense numbers; `pinned[i]` marks items
/// that must survive.
#[must_use]
pub fn plan_items(items: &[Item], pinned: &[bool]) -> Vec<PlanItem> {
    let mut ids: HashMap<&str, u64> = HashMap::new();
    let mut planned = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let kind = match item {
            Item::ToolCall { call_id, .. } => ItemKind::ToolCall(id_of(&mut ids, call_id)),
            Item::ToolResult { call_id, .. } => ItemKind::ToolResult(id_of(&mut ids, call_id)),
            _ => ItemKind::Message,
        };
        let tokens = u32::try_from(estimate(item)).unwrap_or(u32::MAX);
        planned.push(PlanItem { kind, pinned: pinned.get(i).copied().unwrap_or(false), tokens });
    }
    planned
}

fn id_of<'a>(ids: &mut HashMap<&'a str, u64>, call_id: &'a str) -> u64 {
    let next = u64::try_from(ids.len()).unwrap_or(u64::MAX);
    *ids.entry(call_id).or_insert(next)
}

/// The smallest valid cut `k` (keep the longest verbatim tail within `budget`): `1 <= k <= n`, no
/// call/result pair straddles `k`, the tail `[k, n)` fits, and no pinned tool item lies before it.
/// `None` for an empty transcript or a pinned tool item. The decision is the verified kernel's
/// (`aim_kernel::compaction::plan_cut`, ADR 0025).
#[must_use]
pub fn plan_cut(items: &[PlanItem], budget: u64) -> Option<usize> {
    aim_kernel::compaction::plan_cut(items, budget).ok()
}

/// The kept items before the cut (pinned ones), in order.
#[must_use]
pub fn pinned_before(items: &[Item], pinned: &[bool], cut: usize) -> Vec<Item> {
    items.iter().zip(pinned).take(cut).filter(|(_, p)| **p).map(|(i, _)| i.clone()).collect()
}

/// A local summary as a conversation item.
#[must_use]
pub fn summary_item(text: &str) -> Item {
    Item::User { parts: vec![Part::Text { text: format!("{SUMMARY_HEADING}\n\n{}", text.trim()) }] }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(tokens: u32) -> PlanItem {
        PlanItem { kind: ItemKind::Message, pinned: false, tokens }
    }
    fn call(id: u64) -> PlanItem {
        PlanItem { kind: ItemKind::ToolCall(id), pinned: false, tokens: 1 }
    }
    fn result(id: u64) -> PlanItem {
        PlanItem { kind: ItemKind::ToolResult(id), pinned: false, tokens: 1 }
    }

    #[test]
    fn keeps_the_longest_tail_that_fits() {
        let items = [msg(10), msg(10), msg(3), msg(3)];
        assert_eq!(plan_cut(&items, 6), Some(2));
        assert_eq!(plan_cut(&items, 5), Some(3));
        assert_eq!(plan_cut(&items, 0), Some(4), "everything summarized");
        assert_eq!(plan_cut(&[], 10), None);
    }

    #[test]
    fn never_separates_parallel_calls_from_their_results() {
        // user, call1, call2, result1, result2, answer
        let items = [msg(5), call(1), call(2), result(1), result(2), msg(1)];
        // A budget for the last three items would cut between call2 and result1: not allowed.
        let k = plan_cut(&items, 3).unwrap_or_default();
        assert!(k == 1 || k >= 5, "cut {k} separates a call from its result");
        assert_eq!(plan_cut(&items, 100), Some(1));
    }
}
