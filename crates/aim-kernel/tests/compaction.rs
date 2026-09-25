//! Executable boundary cases for the verified cut planner.
use std::time::Instant;

use aim_kernel::compaction::{Item, ItemKind, PlanError, kept_mask, plan_cut};

const fn item(kind: ItemKind, pinned: bool, tokens: u32) -> Item {
    Item { kind, pinned, tokens }
}

#[test]
fn sequential_call_and_result_stay_together() {
    let items = [item(ItemKind::ToolCall(7), false, 1), item(ItemKind::ToolResult(7), false, 1), item(ItemKind::Message, false, 1)];
    assert_eq!(plan_cut(&items, 1), Ok(2));
    assert_eq!(kept_mask(&items, 2), [false, false, true]);
}

#[test]
fn parallel_calls_and_results_are_paired_by_id() {
    let items = [
        item(ItemKind::ToolCall(1), false, 1),
        item(ItemKind::ToolCall(2), false, 1),
        item(ItemKind::ToolResult(1), false, 1),
        item(ItemKind::ToolResult(2), false, 1),
        item(ItemKind::Message, false, 1),
    ];
    assert_eq!(plan_cut(&items, 5), Ok(4));
    assert_eq!(plan_cut(&items, 0), Ok(5));
}

#[test]
fn repeated_call_id_pairs_every_matching_call() {
    let items = [
        item(ItemKind::ToolCall(7), false, 1),
        item(ItemKind::Message, false, 1),
        item(ItemKind::ToolCall(7), false, 1),
        item(ItemKind::ToolResult(7), false, 1),
        item(ItemKind::Message, false, 1),
    ];
    assert_eq!(plan_cut(&items, 5), Ok(4));
}

#[test]
fn straddling_exchange_forces_a_later_cut() {
    let items = [
        item(ItemKind::Message, false, 1),
        item(ItemKind::ToolCall(5), false, 1),
        item(ItemKind::Message, false, 1),
        item(ItemKind::ToolResult(5), false, 1),
        item(ItemKind::Message, false, 1),
    ];
    assert_eq!(plan_cut(&items, 3), Ok(4));
}

#[test]
fn tail_on_budget_boundary_fits_exactly() {
    let items = [item(ItemKind::Message, false, 5), item(ItemKind::Message, false, 3), item(ItemKind::Message, false, 2)];
    assert_eq!(plan_cut(&items, 5), Ok(1));
    assert_eq!(plan_cut(&items, 4), Ok(2));
}

#[test]
fn pinned_message_before_cut_is_kept() {
    let items = [item(ItemKind::Message, true, 8), item(ItemKind::Message, false, 4), item(ItemKind::Message, false, 1)];
    assert_eq!(plan_cut(&items, 1), Ok(2));
    assert_eq!(kept_mask(&items, 2), [true, false, true]);
}

#[test]
fn pinned_tool_is_rejected_even_if_it_lies_in_the_tail() {
    let items = [item(ItemKind::Message, false, 1), item(ItemKind::ToolCall(1), true, 1), item(ItemKind::ToolResult(1), false, 1)];
    assert_eq!(plan_cut(&items, 2), Err(PlanError::PinnedToolItem));
}

#[test]
fn empty_transcript_has_no_shrinking_cut() {
    assert_eq!(plan_cut(&[], 10), Err(PlanError::Empty));
    assert!(kept_mask(&[], 0).is_empty());
}

#[test]
fn everything_can_be_summarized() {
    let items = [item(ItemKind::Message, false, 9)];
    assert_eq!(plan_cut(&items, 0), Ok(1));
    assert_eq!(kept_mask(&items, 1), [false]);
}

#[test]
fn five_thousand_interleaved_items() {
    let mut items = Vec::with_capacity(5_000);
    for id in 0..1_250 {
        items.extend([
            item(ItemKind::ToolCall(id), false, 1),
            item(ItemKind::ToolCall(id + 1_250), false, 1),
            item(ItemKind::ToolResult(id), false, 1),
            item(ItemKind::ToolResult(id + 1_250), false, 1),
        ]);
    }
    let start = Instant::now();
    assert_eq!(plan_cut(&items, 100), Ok(4_900));
    eprintln!("5,000-item compaction plan: {:?}", start.elapsed());
}
