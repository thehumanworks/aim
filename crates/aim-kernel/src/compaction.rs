//! Verified transcript cut for compaction (docs/adr/0025).
//!
//! Items before the cut are summarized; the tail and pinned messages are kept verbatim. Calls
//! and results are related by call id, including interleaved parallel calls. Repeated ids are
//! deliberately allowed: every matching call/result pair imposes the same boundary constraint.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// What a transcript item is, for the purpose of compaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemKind {
    /// Any message that is not part of a tool exchange.
    Message,
    /// A tool call, identified by its call id.
    ToolCall(u64),
    /// The result of the tool call with this call id.
    ToolResult(u64),
}

/// One transcript item as the compaction planner sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Item {
    /// What the item is.
    pub kind: ItemKind,
    /// Pinned items must survive every compaction.
    pub pinned: bool,
    /// Token cost of keeping the item.
    pub tokens: u32,
}

/// Why a transcript cannot be cut or a keep-mask cannot be measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// An empty transcript has no shrinking cut.
    Empty,
    /// Pinned tool items cannot be preserved by a simple transcript cut.
    PinnedToolItem,
    /// The keep-mask and transcript differ in length.
    LengthMismatch,
    /// The token total does not fit in a `u64`.
    Overflow,
}

/// A call at `i` and its result at `j`, identified by equal call ids, even if other items intervene.
pub open spec fn answers(items: Seq<Item>, i: int, j: int) -> bool {
    &&& 0 <= i < j < items.len()
    &&& match (items[i].kind, items[j].kind) {
        (ItemKind::ToolCall(c), ItemKind::ToolResult(r)) => c == r,
        _ => false,
    }
}

/// Token cost of the verbatim tail beginning at `k`.
pub open spec fn tail_tokens(items: Seq<Item>, k: int) -> int
    decreases items.len() - k,
{
    if k < 0 || k >= items.len() {
        0
    } else {
        items[k].tokens as int + tail_tokens(items, k + 1)
    }
}

/// Keep the tail and any pinned items before it.
pub open spec fn kept(items: Seq<Item>, k: int) -> Seq<bool> {
    Seq::new(items.len(), |i: int| i >= k || items[i].pinned)
}

/// LOCKED(ADR-0025): pinned items survive, and no call/result pair is separated.
pub open spec fn plan_ok(items: Seq<Item>, keep: Seq<bool>) -> bool {
    &&& keep.len() == items.len()
    &&& forall|i: int| 0 <= i < items.len() && #[trigger] items[i].pinned ==> keep[i]
    &&& forall|i: int, j: int| #[trigger] answers(items, i, j) ==> keep[i] == keep[j]
}

/// A pinned tool item at index `i`.
pub open spec fn pinned_tool(items: Seq<Item>, i: int) -> bool {
    &&& 0 <= i < items.len()
    &&& items[i].pinned
    &&& !(items[i].kind is Message)
}

/// LOCKED(ADR-0025): a shrinking, budgeted cut that preserves every call/result pair and
/// requires pins before the cut to be messages.
pub open spec fn cut_ok(items: Seq<Item>, k: int, budget: int) -> bool {
    &&& 1 <= k <= items.len()
    &&& tail_tokens(items, k) <= budget
    &&& forall|i: int, j: int| #[trigger] answers(items, i, j) && i < k ==> j < k
    &&& forall|i: int|
        0 <= i < k && #[trigger] items[i].pinned ==> items[i].kind == ItemKind::Message
}

/// A valid cut produces a keep-mask that preserves pins and matched tool exchanges.
pub proof fn theorem_cut_yields_plan(items: Seq<Item>, k: int, budget: int)
    ensures
        cut_ok(items, k, budget) ==> plan_ok(items, kept(items, k)),
{
    if cut_ok(items, k, budget) {
        assert forall|i: int, j: int| #[trigger] answers(items, i, j) implies kept(items, k)[i]
            == kept(items, k)[j] by {
            if i < k {
                assert(j < k);
                assert(!items[i].pinned);
                assert(!items[j].pinned);
            }
        }
    }
}

/// Moving the cut right cannot increase the token cost of its tail.
pub proof fn theorem_tail_monotone(items: Seq<Item>, k1: int, k2: int)
    requires
        0 <= k1 <= k2 <= items.len(),
    ensures
        tail_tokens(items, k2) <= tail_tokens(items, k1),
    decreases k2 - k1,
{
    if k1 < k2 {
        theorem_tail_monotone(items, k1 + 1, k2);
    }
}

/// Summarizing everything is always a valid cut for a nonempty transcript without pinned tools.
pub proof fn theorem_all_summarized(items: Seq<Item>, budget: int)
    requires
        items.len() >= 1,
        budget >= 0,
        forall|i: int| 0 <= i < items.len() ==> !pinned_tool(items, i),
    ensures
        cut_ok(items, items.len() as int, budget),
{
    assert forall|i: int| 0 <= i < items.len() && items[i].pinned implies items[i].kind
        == ItemKind::Message by {
        assert(!pinned_tool(items, i));
    }
    assert forall|i: int, j: int| answers(items, i, j) && i < items.len() implies j
        < items.len() by {
        assert(j < items.len());
    }
}

proof fn lemma_tail_excludes_earlier(items: Seq<Item>, k: int, best: int, budget: int)
    requires
        1 <= k < best <= items.len(),
        tail_tokens(items, k) > budget,
        forall|t: int| k < t < best ==> !cut_ok(items, t, budget),
    ensures
        forall|t: int| 1 <= t < best ==> !cut_ok(items, t, budget),
{
    assert forall|t: int| 1 <= t < best implies !cut_ok(items, t, budget) by {
        if t <= k {
            theorem_tail_monotone(items, t, k);
        }
    }
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

fn first_call(items: &[Item], j: usize) -> (m: usize)
    requires
        j < items.len(),
    ensures
        m <= j,
        m < j ==> answers(items@, m as int, j as int),
        forall|i: int| #[trigger] answers(items@, i, j as int) ==> m as int <= i,
{
    for i in 0..j
        invariant
            j < items.len(),
            forall|h: int| 0 <= h < i ==> !answers(items@, h, j as int),
    {
        let found = match (items[i].kind, items[j].kind) {
            (ItemKind::ToolCall(c), ItemKind::ToolResult(r)) => c == r,
            _ => false,
        };
        if found {
            return i;
        }
    }
    j
}

/// The smallest valid cut, preserving the longest verbatim tail within `budget`.
///
/// # Errors
/// [`PlanError::Empty`] for no items; [`PlanError::PinnedToolItem`] if any tool item is pinned.
pub fn plan_cut(items: &[Item], budget: u64) -> (r: Result<usize, PlanError>)
    ensures
        r matches Ok(k) ==> cut_ok(items@, k as int, budget as int) && forall|k2: int|
            1 <= k2 < k ==> !cut_ok(items@, k2, budget as int),
        r == Err::<usize, PlanError>(PlanError::Empty) <==> items.len() == 0,
        r == Err::<usize, PlanError>(PlanError::PinnedToolItem) ==> exists|i: int|
            pinned_tool(items@, i),
{
    let n = items.len();
    if n == 0 {
        return Err(PlanError::Empty);
    }
    for i in 0..n
        invariant
            n == items.len(),
            n > 0,
            forall|h: int| 0 <= h < i ==> !pinned_tool(items@, h),
    {
        if items[i].pinned {
            #[expect(clippy::single_match_else, reason = "variant matching exposes the Verus proof of a pinned tool item")]
            match items[i].kind {
                ItemKind::Message => {},
                _ => {
                    proof {
                        assert(pinned_tool(items@, i as int));
                        assert(exists|h: int| pinned_tool(items@, h));
                    }
                    return Err(PlanError::PinnedToolItem);
                },
            }
        }
        proof {
            assert(!pinned_tool(items@, i as int));
        }
    }
    proof {
        theorem_all_summarized(items@, budget as int);
    }
    let mut k = n;
    let mut best = n;
    let mut tail: u64 = 0;
    let mut earliest = n;
    while k > 1
        invariant
            n == items.len(),
            n > 0,
            1 <= k <= best <= n,
            tail as int == tail_tokens(items@, k as int),
            tail <= budget,
            cut_ok(items@, best as int, budget as int),
            forall|h: int| 0 <= h < n ==> !pinned_tool(items@, h),
            forall|t: int| k <= t < best ==> !cut_ok(items@, t, budget as int),
            earliest <= n,
            forall|i: int, j: int| #[trigger]
                answers(items@, i, j) && j >= k ==> earliest as int <= i,
            earliest < n ==> exists|i: int, j: int|
                answers(items@, i, j) && j >= k && i == earliest,
        decreases k,
    {
        k -= 1;
        let m = first_call(items, k);
        if m < k && m < earliest {
            earliest = m;
        }
        let next = tail.checked_add(u64::from(items[k].tokens));
        if let Some(sum) = next {
            if sum > budget {
                proof {
                    assert(tail_tokens(items@, k as int) == sum as int);
                    lemma_tail_excludes_earlier(items@, k as int, best as int, budget as int);
                }
                return Ok(best);
            }
            tail = sum;
        } else {
            proof {
                assert(tail_tokens(items@, k as int) == tail as int
                    + items@[k as int].tokens as int);
                lemma_tail_excludes_earlier(items@, k as int, best as int, budget as int);
            }
            return Ok(best);
        }
        if earliest >= k {
            best = k;
        }
    }
    Ok(best)
}

/// Keep-mask for a cut; for out-of-range `k`, only pinned items remain kept.
#[must_use]
pub fn kept_mask(items: &[Item], k: usize) -> (out: Vec<bool>)
    ensures
        k <= items.len() ==> out@ == kept(items@, k as int),
{
    let mut out: Vec<bool> = Vec::with_capacity(items.len());
    for i in 0..items.len()
        invariant
            out@.len() == i,
            forall|j: int| 0 <= j < i ==> #[trigger] out@[j] == (j >= k || items@[j].pinned),
    {
        out.push(i >= k || items[i].pinned);
    }
    out
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
