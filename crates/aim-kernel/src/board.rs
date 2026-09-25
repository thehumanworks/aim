//! In-memory blackboard index: many jobs, one event at a time. Every job obeys `job::next`.
use crate::job::{Event, Job, JobView, LifecycleError};
// Spec-only items are erased from the plain build: import them only when Verus runs.
#[cfg(verus_only)]
use crate::job::{next, wf};
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// Why the board rejected an event (the board is left unchanged).
pub enum BoardError {
    /// No job has that id.
    NoSuchJob,
    /// The job's lifecycle rejected the event.
    Rejected(LifecycleError),
}

/// All jobs on the blackboard, addressed by id.
pub struct Board {
    jobs: Vec<Job>,
}

impl View for Board {
    type V = Seq<JobView>;

    closed spec fn view(&self) -> Seq<JobView> {
        self.jobs@.map_values(|j: Job| j@)
    }
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    /// An empty board.
    #[must_use]
    pub const fn new() -> (b: Self)
        ensures
            b@ == Seq::<JobView>::empty(),
    {
        let b = Self { jobs: Vec::new() };
        assert(b@ =~= Seq::<JobView>::empty());
        b
    }

    /// Posts a job; returns its id (index). Never touches existing jobs.
    pub fn post(&mut self, max_retries: u32) -> (id: usize)
        ensures
            id == old(self)@.len(),
            final(self)@ == old(self)@.push(Job::new_view(max_retries)),
    {
        let id = self.jobs.len();
        self.jobs.push(Job::new(max_retries));
        assert(self@ =~= old(self)@.push(Job::new_view(max_retries)));
        id
    }

    /// Applies one event to one job. Total. Only job `id` can change, and only as `next` allows.
    ///
    /// # Errors
    /// `NoSuchJob` for an unknown id; `Rejected` when `next` rejects the event.
    pub fn apply(&mut self, id: usize, ev: Event) -> (r: Result<(), BoardError>)
        ensures
            id >= old(self)@.len() ==> r == Err::<(), BoardError>(BoardError::NoSuchJob)
                && final(self)@ == old(self)@,
            id < old(self)@.len() ==> match next(old(self)@[id as int], ev) {
                Some(post) => r is Ok && final(self)@ == old(self)@.update(id as int, post),
                None => r is Err && final(self)@ == old(self)@,
            },
    {
        if id >= self.jobs.len() {
            return Err(BoardError::NoSuchJob);
        }
        let res = self.jobs[id].apply(ev);
        assert(self@ =~= old(self)@.update(id as int, self.jobs@[id as int]@));
        match res {
            Ok(()) => Ok(()),
            Err(e) => {
                assert(self@ =~= old(self)@);
                Err(BoardError::Rejected(e))
            },
        }
    }
}

/// Board-level corollary: every job on the board stays well-formed across any accepted event.
pub proof fn lemma_board_step_wf(pre: Seq<JobView>, id: int, ev: Event)
    requires
        forall|i: int| 0 <= i < pre.len() ==> wf(#[trigger] pre[i]),
        0 <= id < pre.len(),
        next(pre[id], ev) is Some,
    ensures
        forall|i: int|
            0 <= i < pre.len() ==> wf(#[trigger] pre.update(id, next(pre[id], ev)->0)[i]),
{
    crate::job::lemma_step_bounded(pre[id], ev);
}

} // verus!
