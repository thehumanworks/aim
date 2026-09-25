//! Admission and shared turn accounting for native child sessions (ADR 0070).
//!
//! The host serializes updates to a parent's live-child count and remaining turn credit. A child
//! receives a local cap, while each of its turns also spends one turn of its parent's credit.
use vstd::prelude::*;

verus! {

/// Maximum root-to-child edges under the default policy.
pub const DEFAULT_MAX_DEPTH: u32 = 2;

/// Maximum live children of one session under the default policy.
pub const DEFAULT_MAX_CONCURRENT_CHILDREN: u32 = 4;

/// Maximum turns granted to one child under the default policy.
pub const DEFAULT_CHILD_TURN_CAP: u32 = 8;

/// Admission failure; the parent remains able to continue its turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnError {
    /// The proposed child would exceed the maximum depth.
    DepthLimit,
    /// This parent already has the maximum number of live children.
    ConcurrencyLimit,
    /// The parent has no turn credit to share with a child.
    ParentBudgetExhausted,
}

/// The child's admitted depth and initial local turn credit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChildBudget {
    /// Root sessions are at depth zero; a child is one deeper than its parent.
    pub depth: u32,
    /// A child turn must also debit the parent's remaining turn credit.
    pub turns_left: u32,
}

/// LOCKED(ADR-0070): depth and concurrency bounds take precedence over the parent's empty budget.
/// The child receives no more than eight turns or the parent's currently available turn credit.
pub open spec fn admit_child_spec(
    parent_depth: u32,
    active_children: u32,
    parent_turns_left: u32,
) -> Result<ChildBudget, SpawnError> {
    if parent_depth >= 2 {
        Err(SpawnError::DepthLimit)
    } else if active_children >= 4 {
        Err(SpawnError::ConcurrencyLimit)
    } else if parent_turns_left == 0 {
        Err(SpawnError::ParentBudgetExhausted)
    } else {
        Ok(
            ChildBudget {
                depth: (parent_depth as int + 1) as u32,
                turns_left: if parent_turns_left < 8 {
                    parent_turns_left
                } else {
                    8
                },
            },
        )
    }
}

/// LOCKED(ADR-0070): each child turn debits both its local cap and the parent's remaining credit.
/// A zero balance on either side refuses another turn and leaves both counters unchanged.
pub open spec fn charge_child_turn_spec(parent_turns_left: u32, child_turns_left: u32) -> Option<
    (u32, u32),
> {
    if parent_turns_left == 0 || child_turns_left == 0 {
        None
    } else {
        Some(((parent_turns_left as int - 1) as u32, (child_turns_left as int - 1) as u32))
    }
}

/// Admitted children are inside both structural limits and carry bounded, nonzero turn credit.
pub proof fn theorem_admitted_bounds(
    parent_depth: u32,
    active_children: u32,
    parent_turns_left: u32,
)
    ensures
        match admit_child_spec(parent_depth, active_children, parent_turns_left) {
            Ok(child) => {
                child.depth <= DEFAULT_MAX_DEPTH && parent_depth < child.depth && active_children
                    < DEFAULT_MAX_CONCURRENT_CHILDREN && 0 < child.turns_left && child.turns_left
                    <= DEFAULT_CHILD_TURN_CAP && child.turns_left <= parent_turns_left
            },
            Err(_) => true,
        },
{
}

/// Refusing a spawn at any limit never grants a child budget.
pub proof fn theorem_spawn_limits(parent_depth: u32, active_children: u32, parent_turns_left: u32)
    ensures
        parent_depth >= DEFAULT_MAX_DEPTH ==> admit_child_spec(
            parent_depth,
            active_children,
            parent_turns_left,
        ) is Err,
        active_children >= DEFAULT_MAX_CONCURRENT_CHILDREN ==> admit_child_spec(
            parent_depth,
            active_children,
            parent_turns_left,
        ) is Err,
        parent_turns_left == 0 ==> admit_child_spec(
            parent_depth,
            active_children,
            parent_turns_left,
        ) is Err,
{
}

/// A successful child turn always decreases the parent's balance and the child's balance by one.
pub proof fn theorem_child_turn_counts_toward_parent(parent_turns_left: u32, child_turns_left: u32)
    ensures
        match charge_child_turn_spec(parent_turns_left, child_turns_left) {
            Some((parent_after, child_after)) => {
                parent_after as int + 1 == parent_turns_left as int && child_after as int + 1
                    == child_turns_left as int
            },
            None => parent_turns_left == 0 || child_turns_left == 0,
        },
{
}

/// Decide whether a parent may start another child. The caller must reserve the slot atomically
/// with the live-child count; a later refusal is a tool error, not a parent-session failure.
///
/// # Errors
/// Returns a limit error when depth, live-child capacity, or parent turn credit is exhausted.
pub fn admit_child(parent_depth: u32, active_children: u32, parent_turns_left: u32) -> (result:
    Result<ChildBudget, SpawnError>)
    ensures
        result == admit_child_spec(parent_depth, active_children, parent_turns_left),
{
    if parent_depth >= DEFAULT_MAX_DEPTH {
        Err(SpawnError::DepthLimit)
    } else if active_children >= DEFAULT_MAX_CONCURRENT_CHILDREN {
        Err(SpawnError::ConcurrencyLimit)
    } else if parent_turns_left == 0 {
        Err(SpawnError::ParentBudgetExhausted)
    } else {
        Ok(
            ChildBudget {
                depth: parent_depth + 1,
                turns_left: parent_turns_left.min(DEFAULT_CHILD_TURN_CAP),
            },
        )
    }
}

/// Charge a completed child turn to its own cap and to the parent's shared turn budget.
/// `None` means neither balance was changed; the host must stop the child before another turn.
#[must_use]
pub fn charge_child_turn(parent_turns_left: u32, child_turns_left: u32) -> (result: Option<
    (u32, u32),
>)
    ensures
        result == charge_child_turn_spec(parent_turns_left, child_turns_left),
{
    if parent_turns_left == 0 || child_turns_left == 0 {
        None
    } else {
        Some((parent_turns_left - 1, child_turns_left - 1))
    }
}

} // verus!
