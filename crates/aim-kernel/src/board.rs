//! Deterministic claim admission over observations from the ledger's write transaction.
//!
//! The service computes dependency acceptance and capacity holds under one SQLite writer lock,
//! then applies this total gate before `Job::apply(Event::Claim)`. The ledger remains the authority.
use vstd::prelude::*;

verus! {

/// Why an otherwise valid claim cannot be admitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardError {
    /// At least one dependency lacks accepted evidence.
    DependencyNotAccepted,
    /// The worker has no free slot, including uncertain cleanup holds.
    CapacityExceeded,
}

/// DRAFT(M5a): dependency acceptance and capacity are both required for admission.
pub open spec fn may_claim(dependencies_ready: bool, held: usize, capacity: usize) -> bool {
    dependencies_ready && held < capacity
}

/// Admit a claim from observations made in the same ledger transaction.
///
/// # Errors
/// Returns `DependencyNotAccepted` or `CapacityExceeded` without side effects.
pub fn check_admission(dependencies_ready: bool, held: usize, capacity: usize) -> (r: Result<
    (),
    BoardError,
>)
    ensures
        r is Ok <==> may_claim(dependencies_ready, held, capacity),
{
    if !dependencies_ready {
        return Err(BoardError::DependencyNotAccepted);
    }
    if held >= capacity {
        return Err(BoardError::CapacityExceeded);
    }
    Ok(())
}

/// A successful claim cannot exceed the worker's declared capacity after one slot is reserved.
pub proof fn lemma_admitted_capacity(dependencies_ready: bool, held: usize, capacity: usize)
    requires
        may_claim(dependencies_ready, held, capacity),
    ensures
        held + 1 <= capacity,
{
}

/// A finished dependency alone does not authorize admission: explicit acceptance is required.
pub proof fn lemma_unaccepted_dependency_blocks(held: usize, capacity: usize)
    ensures
        !may_claim(false, held, capacity),
{
}

} // verus!
