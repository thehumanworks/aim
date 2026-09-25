//! Compaction-plan invariants.
//!
//! Status: DRAFT decision. This model pairs a tool call with the item immediately after it; the
//! context-engine milestone generalises pairing to call ids (parallel tool calls produce
//! `call₁ call₂ result₁ result₂`) and then marks [`plan_ok`] as LOCKED with its ADR.
//!
//! Decision, stated as [`plan_ok`]: a plan keeps every pinned item, and never keeps a tool
//! call without its result (or vice versa). Token totals are computed without overflow.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// What a transcript item is, for the purpose of compaction.
pub enum ItemKind {
    /// Any message that is not part of a tool exchange.
    Message,
    /// A tool call, identified by its call id.
    ToolCall(u64),
    /// The result of the tool call with this call id.
    ToolResult(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// One transcript item as the compaction planner sees it.
pub struct Item {
    /// What the item is.
    pub kind: ItemKind,
    /// Pinned items must survive every compaction.
    pub pinned: bool,
    /// Token cost of keeping the item.
    pub tokens: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// Why a plan could not be repaired or measured.
pub enum PlanError {
    /// The keep-mask and the transcript differ in length.
    LengthMismatch,
    /// The token total does not fit in a `u64`.
    Overflow,
}

/// Items `i` and `i + 1` are a tool call immediately followed by its own result.
pub open spec fn is_pair(items: Seq<Item>, i: int) -> bool {
    &&& 0 <= i
    &&& i + 1 < items.len()
    &&& items[i].kind matches ItemKind::ToolCall(c)
    &&& items[i + 1].kind matches ItemKind::ToolResult(r)
    &&& c == r
}

/// DRAFT(context-engine) decision: what every compaction plan (keep-mask) must satisfy.
pub open spec fn plan_ok(items: Seq<Item>, keep: Seq<bool>) -> bool {
    &&& keep.len() == items.len()
    &&& forall|i: int| 0 <= i < items.len() && #[trigger] items[i].pinned ==> keep[i]
    &&& forall|i: int| #[trigger] is_pair(items, i) ==> keep[i] == keep[i + 1]
}

/// The planner asked to keep item `i`, or it is pinned.
pub open spec fn base_keep(items: Seq<Item>, proposed: Seq<bool>, i: int) -> bool {
    proposed[i] || items[i].pinned
}

/// Closed form of the repaired plan: requested or pinned, or the partner of such an item.
pub open spec fn repaired(items: Seq<Item>, proposed: Seq<bool>, i: int) -> bool {
    ||| base_keep(items, proposed, i)
    ||| (is_pair(items, i - 1) && base_keep(items, proposed, i - 1))
    ||| (is_pair(items, i) && base_keep(items, proposed, i + 1))
}

/// Total tokens of the kept items among the first `n`.
pub open spec fn kept_tokens_spec(items: Seq<Item>, keep: Seq<bool>, n: int) -> int
    decreases n,
{
    if n <= 0 {
        0
    } else {
        kept_tokens_spec(items, keep, n - 1) + if keep[n - 1] {
            items[n - 1].tokens as int
        } else {
            0
        }
    }
}

proof fn lemma_kept_tokens_monotone(items: Seq<Item>, keep: Seq<bool>, a: int, b: int)
    requires
        0 <= a <= b,
    ensures
        kept_tokens_spec(items, keep, a) <= kept_tokens_spec(items, keep, b),
    decreases b - a,
{
    if a < b {
        lemma_kept_tokens_monotone(items, keep, a, b - 1);
    }
}

const fn is_pair_exec(items: &[Item], i: usize) -> (b: bool)
    ensures
        b == is_pair(items@, i as int),
{
    if i >= items.len() || i + 1 >= items.len() {
        return false;
    }
    match (items[i].kind, items[i + 1].kind) {
        (ItemKind::ToolCall(c), ItemKind::ToolResult(r)) => c == r,
        _ => false,
    }
}

/// Repairs a planner's proposal (e.g. produced by a model or heuristic) into a valid plan.
/// Only ever adds items: nothing the planner wanted kept is dropped.
///
/// # Errors
/// `LengthMismatch` if `proposed` and `items` differ in length.
pub fn repair_plan(items: &[Item], proposed: &[bool]) -> (r: Result<Vec<bool>, PlanError>)
    ensures
        r is Err <==> proposed.len() != items.len(),
        r matches Ok(keep) ==> plan_ok(items@, keep@) && forall|i: int|
            0 <= i < items.len() && #[trigger] proposed[i] ==> keep[i],
{
    if proposed.len() != items.len() {
        return Err(PlanError::LengthMismatch);
    }
    let n = items.len();
    let mut keep: Vec<bool> = Vec::with_capacity(n);
    for i in 0..n
        invariant
            n == items.len(),
            n == proposed.len(),
            keep.len() == i,
            forall|j: int| 0 <= j < i ==> #[trigger] keep[j] == repaired(items@, proposed@, j),
    {
        let base = proposed[i] || items[i].pinned;
        let prev = i > 0 && is_pair_exec(items, i - 1) && (proposed[i - 1] || items[i - 1].pinned);
        let next = is_pair_exec(items, i) && (proposed[i + 1] || items[i + 1].pinned);
        keep.push(base || prev || next);
    }
    Ok(keep)
}

/// Total tokens of the kept items. Never overflows.
///
/// # Errors
/// `LengthMismatch` on differing lengths; `Overflow` iff the true total exceeds `u64::MAX`.
pub fn kept_tokens(items: &[Item], keep: &[bool]) -> (r: Result<u64, PlanError>)
    ensures
        keep.len() != items.len() ==> r == Err::<u64, PlanError>(PlanError::LengthMismatch),
        keep.len() == items.len() ==> match r {
            Ok(t) => t as int == kept_tokens_spec(items@, keep@, items.len() as int),
            Err(e) => e == PlanError::Overflow && kept_tokens_spec(
                items@,
                keep@,
                items.len() as int,
            ) > u64::MAX,
        },
{
    if keep.len() != items.len() {
        return Err(PlanError::LengthMismatch);
    }
    let n = items.len();
    let mut acc: u64 = 0;
    for i in 0..n
        invariant
            n == items.len(),
            n == keep.len(),
            acc as int == kept_tokens_spec(items@, keep@, i as int),
    {
        if keep[i] {
            if let Some(s) = acc.checked_add(u64::from(items[i].tokens)) {
                acc = s;
            } else {
                proof {
                    lemma_kept_tokens_monotone(items@, keep@, i + 1, n as int);
                }
                return Err(PlanError::Overflow);
            }
        }
    }
    Ok(acc)
}

} // verus!
