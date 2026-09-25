//! Bounded workflow decisions over a topologically ordered graph (ADR 0071).
//!
//! The loader resolves manifest ids to positions and sorts arbitrary DAGs before admission.
//! Every admitted dependency points to a lower position, so a cycle cannot be admitted.
#![expect(clippy::match_like_matches_macro, reason = "Verus proves explicit enum matches against scheduler predicates")]
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// Maximum admitted workflow size and scheduler scan length.
pub const MAX_STEPS: usize = 128;

/// Stable id, normalized dependency positions, and retry policy for one step.
pub struct Step {
    /// Durable step id.
    pub id: u64,
    /// Positions of prerequisite steps in topological order.
    pub depends_on: Vec<usize>,
    /// Retries allowed after the first attempt.
    pub max_retries: u32,
}

/// Durable execution status of a step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepStatus {
    /// Waiting for dependencies.
    Pending,
    /// An attempt is in flight.
    Running,
    /// Completed successfully.
    Succeeded,
    /// An attempt failed; a retry may remain.
    Failed,
    /// Explicitly or transitively cancelled.
    Cancelled,
}

/// Malformed graph or refused decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkflowError {
    /// Too many steps.
    TooManySteps,
    /// A stable id occurs twice.
    DuplicateId,
    /// A dependency does not precede its step.
    InvalidDependency,
    /// Status count or requested position is invalid.
    InvalidSnapshot,
    /// The step is not failed or its retry budget is exhausted.
    RetryUnavailable,
}

/// LOCKED(ADR-0071): a bounded DAG has unique ids and each dependency points backward.
/// Every path strictly decreases its position, which excludes cycles.
pub open spec fn well_formed(steps: Seq<Step>) -> bool {
    &&& steps.len() <= MAX_STEPS
    &&& forall|i: int, j: int| 0 <= i < j < steps.len() ==> steps[i].id != steps[j].id
    &&& forall|i: int, d: int|
        0 <= i < steps.len() && 0 <= d < steps[i].depends_on@.len() ==> steps[i].depends_on@[d] < i
}

/// LOCKED(ADR-0071): only a pending step with all dependencies succeeded is ready.
pub open spec fn ready_at(steps: Seq<Step>, states: Seq<StepStatus>, index: int) -> bool {
    &&& 0 <= index < steps.len()
    &&& states.len() == steps.len()
    &&& states[index] == StepStatus::Pending
    &&& forall|d: int|
        0 <= d < steps[index].depends_on@.len() ==> states[steps[index].depends_on@[d] as int]
            == StepStatus::Succeeded
}

/// LOCKED(ADR-0071): a retry spends one allowance only after failure and below the cap.
pub open spec fn retry_next(status: StepStatus, used: u32, maximum: u32) -> Option<nat> {
    if status == StepStatus::Failed && used < maximum {
        Some(used as nat + 1)
    } else {
        None
    }
}

/// LOCKED(ADR-0071): cancellation of a step or any prerequisite cancels an unfinished step.
/// Apply in topological order to propagate through all descendants.
pub open spec fn cancel_next(
    steps: Seq<Step>,
    states: Seq<StepStatus>,
    index: int,
    requested: bool,
) -> bool {
    0 <= index < steps.len() && states.len() == steps.len() && states[index]
        != StepStatus::Succeeded && (requested || (exists|d: int|
        0 <= d < steps[index].depends_on@.len() && states[steps[index].depends_on@[d] as int]
            == StepStatus::Cancelled))
}

fn is_pending(status: StepStatus) -> (b: bool)
    ensures
        b == (status == StepStatus::Pending),
{
    match status {
        StepStatus::Pending => true,
        _ => false,
    }
}

fn is_succeeded(status: StepStatus) -> (b: bool)
    ensures
        b == (status == StepStatus::Succeeded),
{
    match status {
        StepStatus::Succeeded => true,
        _ => false,
    }
}

fn is_cancelled(status: StepStatus) -> (b: bool)
    ensures
        b == (status == StepStatus::Cancelled),
{
    match status {
        StepStatus::Cancelled => true,
        _ => false,
    }
}

fn is_failed(status: StepStatus) -> (b: bool)
    ensures
        b == (status == StepStatus::Failed),
{
    match status {
        StepStatus::Failed => true,
        _ => false,
    }
}

/// Admit a normalized graph. The loader must first topologically order manifest steps.
///
/// # Errors
/// Returns the first size, identity, or dependency violation.
pub fn validate(steps: &[Step]) -> (r: Result<(), WorkflowError>)
    ensures
        r is Ok ==> well_formed(steps@),
{
    if steps.len() > MAX_STEPS {
        return Err(WorkflowError::TooManySteps);
    }
    let mut i: usize = 0;
    while i < steps.len()
        invariant
            i <= steps.len(),
            steps.len() <= MAX_STEPS,
            forall|a: int, b: int| 0 <= a < b < i ==> steps@[a].id != steps@[b].id,
            forall|a: int, d: int|
                0 <= a < i && 0 <= d < steps@[a].depends_on@.len() ==> steps@[a].depends_on@[d] < a,
        decreases steps.len() - i,
    {
        let mut j: usize = 0;
        while j < i
            invariant
                j <= i,
                i < steps.len(),
                forall|k: int| 0 <= k < j ==> steps@[k].id != steps@[i as int].id,
            decreases i - j,
        {
            if steps[j].id == steps[i].id {
                return Err(WorkflowError::DuplicateId);
            }
            j += 1;
        }
        let mut d: usize = 0;
        while d < steps[i].depends_on.len()
            invariant
                d <= steps@[i as int].depends_on@.len(),
                i < steps.len(),
                forall|k: int| 0 <= k < d ==> steps@[i as int].depends_on@[k] < i,
            decreases steps@[i as int].depends_on@.len() - d,
        {
            if steps[i].depends_on[d] >= i {
                return Err(WorkflowError::InvalidDependency);
            }
            d += 1;
        }
        i += 1;
    }
    Ok(())
}

/// Test readiness against a complete durable status snapshot.
///
/// # Errors
/// Refuses malformed graphs, a status count mismatch, or an absent step.
pub fn is_ready(steps: &[Step], states: &[StepStatus], index: usize) -> (r: Result<
    bool,
    WorkflowError,
>)
    ensures
        r is Ok ==> r->Ok_0 == ready_at(steps@, states@, index as int),
{
    validate(steps)?;
    if states.len() != steps.len() || index >= steps.len() {
        return Err(WorkflowError::InvalidSnapshot);
    }
    if !is_pending(states[index]) {
        return Ok(false);
    }
    let mut d: usize = 0;
    while d < steps[index].depends_on.len()
        invariant
            d <= steps@[index as int].depends_on@.len(),
            index < steps.len(),
            states.len() == steps.len(),
            well_formed(steps@),
            states@[index as int] == StepStatus::Pending,
            forall|k: int|
                0 <= k < d ==> states@[steps@[index as int].depends_on@[k] as int]
                    == StepStatus::Succeeded,
        decreases steps@[index as int].depends_on@.len() - d,
    {
        if !is_succeeded(states[steps[index].depends_on[d]]) {
            return Ok(false);
        }
        d += 1;
    }
    Ok(true)
}

/// Spend one retry after failure, if the declared allowance remains.
///
/// # Errors
/// Returns `RetryUnavailable` when no retry is permitted.
pub fn retry(status: StepStatus, used: u32, maximum: u32) -> (r: Result<u32, WorkflowError>)
    ensures
        match r {
            Ok(n) => retry_next(status, used, maximum) == Some(n as nat),
            Err(_) => retry_next(status, used, maximum) is None,
        },
{
    if is_failed(status) && used < maximum {
        Ok(used + 1)
    } else {
        Err(WorkflowError::RetryUnavailable)
    }
}

/// Decide cancellation from the current snapshot. Apply in topological order, updating
/// earlier statuses before querying descendants.
///
/// # Errors
/// Refuses malformed graphs, a status count mismatch, or an absent step.
pub fn should_cancel(steps: &[Step], states: &[StepStatus], index: usize, requested: bool) -> (r:
    Result<bool, WorkflowError>)
    ensures
        r is Ok ==> r->Ok_0 == cancel_next(steps@, states@, index as int, requested),
{
    validate(steps)?;
    if states.len() != steps.len() || index >= steps.len() {
        return Err(WorkflowError::InvalidSnapshot);
    }
    if is_succeeded(states[index]) {
        return Ok(false);
    }
    if requested {
        return Ok(true);
    }
    let mut d: usize = 0;
    while d < steps[index].depends_on.len()
        invariant
            d <= steps@[index as int].depends_on@.len(),
            index < steps.len(),
            states.len() == steps.len(),
            well_formed(steps@),
            states@[index as int] != StepStatus::Succeeded,
            forall|k: int|
                0 <= k < d ==> states@[steps@[index as int].depends_on@[k] as int]
                    != StepStatus::Cancelled,
        decreases steps@[index as int].depends_on@.len() - d,
    {
        if is_cancelled(states[steps[index].depends_on[d]]) {
            return Ok(true);
        }
        d += 1;
    }
    Ok(false)
}

/// Every admitted edge has a lower position; a self dependency is impossible.
pub proof fn lemma_dependency_precedes(steps: Seq<Step>, index: int, dependency: int)
    requires
        well_formed(steps),
        0 <= index < steps.len(),
        0 <= dependency < steps[index].depends_on@.len(),
    ensures
        steps[index].depends_on@[dependency] < index,
{
}

/// A ready step has a successful state for each prerequisite.
pub proof fn lemma_ready_requires_success(
    steps: Seq<Step>,
    states: Seq<StepStatus>,
    index: int,
    dependency: int,
)
    requires
        ready_at(steps, states, index),
        0 <= dependency < steps[index].depends_on@.len(),
    ensures
        states[steps[index].depends_on@[dependency] as int] == StepStatus::Succeeded,
{
}

/// A retry at or beyond its cap is refused.
pub proof fn lemma_retry_bounded(status: StepStatus, used: u32, maximum: u32)
    requires
        used >= maximum,
    ensures
        retry_next(status, used, maximum) is None,
{
}

/// A cancelled prerequisite forces cancellation of its dependent.
pub proof fn lemma_cancel_propagates(
    steps: Seq<Step>,
    states: Seq<StepStatus>,
    index: int,
    dependency: int,
)
    requires
        0 <= index < steps.len(),
        states.len() == steps.len(),
        states[index] != StepStatus::Succeeded,
        0 <= dependency < steps[index].depends_on@.len(),
        states[steps[index].depends_on@[dependency] as int] == StepStatus::Cancelled,
    ensures
        cancel_next(steps, states, index, false),
{
}

} // verus!
