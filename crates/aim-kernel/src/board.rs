//! Admission over restored dependency and worker-job states from one ledger write transaction.
//!
//! The caller supplies complete job sequences; the kernel derives acceptance and held capacity
//! from their verified views. A claim goes through this module so admission cannot be bypassed by
//! calling a public lifecycle transition directly.
use vstd::prelude::*;

use crate::job::{Event, Job, WorkerId};

verus! {

/// Why an otherwise valid claim cannot be admitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardError {
    /// At least one dependency lacks accepted evidence.
    DependencyNotAccepted,
    /// The worker has no free slot, including uncertain cleanup holds.
    CapacityExceeded,
    /// The job lifecycle rejected the claim itself.
    LifecycleRejected,
}

/// Number of jobs in a prefix that still reserve this worker's capacity.
pub open spec fn held_prefix(jobs: Seq<Job>, count: nat, worker: WorkerId) -> nat
    recommends
        count <= jobs.len(),
    decreases count,
{
    if count == 0 {
        0
    } else {
        held_prefix(jobs, (count - 1) as nat, worker) + if crate::job::holds_capacity(
            jobs[count - 1]@,
            worker,
        ) {
            1nat
        } else {
            0nat
        }
    }
}

/// DRAFT(M5b): dependency evidence and worker capacity from actual restored job states.
pub open spec fn may_claim(
    deps: Seq<Job>,
    others: Seq<Job>,
    worker: WorkerId,
    capacity: nat,
) -> bool {
    (forall|i: int| 0 <= i < deps.len() ==> crate::job::accepted(deps[i]@)) && held_prefix(
        others,
        others.len(),
        worker,
    ) < capacity
}

/// Admit a claim from complete job observations made in the same ledger write transaction.
///
/// # Errors
/// Returns a dependency or capacity refusal without mutating the job.
pub fn check_admission(deps: &[Job], others: &[Job], worker: WorkerId, capacity: usize) -> (r:
    Result<(), BoardError>)
    ensures
        r is Ok <==> may_claim(deps@, others@, worker, capacity as nat),
{
    let mut i: usize = 0;
    while i < deps.len()
        invariant
            i <= deps.len(),
            forall|j: int| 0 <= j < i ==> crate::job::accepted(deps@[j]@),
        decreases deps.len() - i,
    {
        if !deps[i].is_accepted() {
            return Err(BoardError::DependencyNotAccepted);
        }
        i += 1;
    }
    let mut held: usize = 0;
    let mut j: usize = 0;
    while j < others.len()
        invariant
            j <= others.len(),
            held <= j,
            held as nat == held_prefix(others@, j as nat, worker),
        decreases others.len() - j,
    {
        if others[j].holds_capacity(worker) {
            held += 1;
        }
        j += 1;
    }
    if held >= capacity {
        Err(BoardError::CapacityExceeded)
    } else {
        Ok(())
    }
}

/// Admission and the lifecycle Claim are one public operation.
///
/// # Errors
/// A dependency, capacity, or lifecycle refusal leaves the target unchanged.
#[expect(clippy::too_many_arguments, reason = "admission receives one transaction's complete observations and claim identity")]
pub fn claim_admitted(
    job: &mut Job,
    deps: &[Job],
    others: &[Job],
    worker: WorkerId,
    capacity: usize,
    claim_id: u64,
    lease_until: u64,
    now: u64,
) -> (r: Result<(), BoardError>)
    ensures
        r is Ok ==> may_claim(deps@, others@, worker, capacity as nat),
        r is Err ==> final(job)@ == old(job)@,
{
    check_admission(deps, others, worker, capacity)?;
    match job.apply(Event::Claim { worker, claim_id, lease_until }, now) {
        Ok(()) => Ok(()),
        Err(_) => Err(BoardError::LifecycleRejected),
    }
}

/// One more successful claim cannot put this worker above its declared capacity.
pub proof fn theorem_admission_respects_capacity(
    deps: Seq<Job>,
    others: Seq<Job>,
    worker: WorkerId,
    capacity: nat,
)
    requires
        may_claim(deps, others, worker, capacity),
    ensures
        held_prefix(others, others.len(), worker) + 1 <= capacity,
{
}

/// Any unaccepted dependency blocks a claim regardless of spare capacity.
pub proof fn lemma_unaccepted_dependency_blocks(
    deps: Seq<Job>,
    others: Seq<Job>,
    worker: WorkerId,
    capacity: nat,
    index: int,
)
    requires
        0 <= index < deps.len(),
        !crate::job::accepted(deps[index]@),
    ensures
        !may_claim(deps, others, worker, capacity),
{
}

} // verus!
