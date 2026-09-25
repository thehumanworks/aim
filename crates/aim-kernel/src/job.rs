//! Blackboard job lifecycle ("fiverr for agents").
//!
//! Status: DRAFT decision (seeded from the Verus lab, docs/research/verus.md). The swarm milestone
//! extends it with attempt generations and claim tokens (docs/research/swarm.md) and then marks
//! [`next`] as LOCKED with its ADR.
//!
//! Decision, stated once as the spec function [`next`]:
//! `Open -> Claimed(w) -> Running(w) -> Done(w)`; `Fail(w)`/`Expire` send a held job back to
//! `Open` while retry budget remains, else to `Failed`; `Cancel` ends any live job; terminal
//! states (`Done`, `Failed`, `Cancelled`) absorb every event.
use vstd::prelude::*;

verus! {

/// Identity of a worker agent on the blackboard.
pub type WorkerId = u64;

/// Lifecycle state of one job.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobState {
    /// Posted and waiting for a worker.
    Open,
    /// Claimed by `worker`, not started yet.
    Claimed {
        /// The claimant; the only worker that may start the job.
        worker: WorkerId,
    },
    /// Being worked on by `worker`.
    Running {
        /// The holder; the only worker that may complete or fail the job.
        worker: WorkerId,
    },
    /// Completed by `worker` (terminal).
    Done {
        /// The worker that completed the job.
        worker: WorkerId,
    },
    /// Permanently failed after exhausting its retries (terminal).
    Failed,
    /// Cancelled by the lead (terminal).
    Cancelled,
}

/// Something that happens to a job.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// `worker` claims an open job.
    Claim {
        /// The claiming worker.
        worker: WorkerId,
    },
    /// The claimant starts working.
    Start {
        /// The worker that must hold the claim.
        worker: WorkerId,
    },
    /// The holder delivers the job.
    Complete {
        /// The worker that must hold the job.
        worker: WorkerId,
    },
    /// The holder reports failure; retried while budget remains.
    Fail {
        /// The worker that must hold the job.
        worker: WorkerId,
    },
    /// The holder's lease timed out (worker vanished); retried while budget remains.
    Expire,
    /// The lead cancels the job.
    Cancel,
}

/// Why an event was rejected (the job is left unchanged).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LifecycleError {
    /// The job is finished; terminal states absorb every event.
    Terminal,
    /// The event names a worker that does not hold the job.
    NotHolder,
    /// The event is not valid in the job's current state.
    WrongState,
}

/// Abstract view of a job. All public specifications are stated over this.
pub struct JobView {
    /// Current lifecycle state.
    pub state: JobState,
    /// Retries consumed so far.
    pub retries: nat,
    /// Retry budget fixed when the job was posted.
    pub max_retries: nat,
}

/// Finished states: they absorb every later event.
pub open spec fn is_terminal(s: JobState) -> bool {
    s is Done || s is Failed || s is Cancelled
}

/// The worker currently holding the job, if any (claimed or running).
pub open spec fn holder(s: JobState) -> Option<WorkerId> {
    match s {
        JobState::Claimed { worker } => Some(worker),
        JobState::Running { worker } => Some(worker),
        _ => None,
    }
}

/// `v` with its lifecycle state replaced by `s`.
pub open spec fn with_state(v: JobView, s: JobState) -> JobView {
    JobView { state: s, retries: v.retries, max_retries: v.max_retries }
}

/// A held job is lost (failure or expiry): retry while budget remains, else fail permanently.
pub open spec fn retry_or_fail(v: JobView) -> JobView {
    if v.retries < v.max_retries {
        JobView { state: JobState::Open, retries: v.retries + 1, max_retries: v.max_retries }
    } else {
        with_state(v, JobState::Failed)
    }
}

/// DRAFT(swarm) decision: the complete transition function. `None` = rejected, job unchanged.
pub open spec fn next(pre: JobView, ev: Event) -> Option<JobView> {
    if is_terminal(pre.state) {
        None
    } else {
        match ev {
            Event::Claim { worker } => if pre.state is Open {
                Some(with_state(pre, JobState::Claimed { worker }))
            } else {
                None
            },
            Event::Start { worker } => if pre.state == (JobState::Claimed { worker }) {
                Some(with_state(pre, JobState::Running { worker }))
            } else {
                None
            },
            Event::Complete { worker } => if pre.state == (JobState::Running { worker }) {
                Some(with_state(pre, JobState::Done { worker }))
            } else {
                None
            },
            Event::Fail { worker } => if pre.state == (JobState::Running { worker }) {
                Some(retry_or_fail(pre))
            } else {
                None
            },
            Event::Expire => if holder(pre.state) is Some {
                Some(retry_or_fail(pre))
            } else {
                None
            },
            Event::Cancel => Some(with_state(pre, JobState::Cancelled)),
        }
    }
}

/// Well-formed: the retry counter never exceeds the budget.
pub open spec fn wf(v: JobView) -> bool {
    v.retries <= v.max_retries
}

/// Events a job can still accept before the end of its current attempt.
pub open spec fn phase_rank(s: JobState) -> nat {
    match s {
        JobState::Open => 3,
        JobState::Claimed { .. } => 2,
        JobState::Running { .. } => 1,
        _ => 0,
    }
}

/// Upper bound on how many more events this job can accept.
pub open spec fn remaining_steps(v: JobView) -> int {
    (v.max_retries - v.retries) * 4 + phase_rank(v.state)
}

// ---------------------------------------------------------------------------------------------
// Theorems about the decision itself (erased at compile time; they document *why* it is safe).
// ---------------------------------------------------------------------------------------------
/// Terminal states are absorbing.
pub proof fn lemma_terminal_absorbing(pre: JobView, ev: Event)
    requires
        is_terminal(pre.state),
    ensures
        next(pre, ev) is None,
{
}

/// Only the worker holding the job may start, complete or fail it, and `Done` records that worker.
pub proof fn lemma_only_holder_progresses(pre: JobView, ev: Event)
    requires
        next(pre, ev) is Some,
    ensures
        ev matches Event::Start { worker } ==> holder(pre.state) == Some(worker),
        ev matches Event::Complete { worker } ==> holder(pre.state) == Some(worker),
        ev matches Event::Fail { worker } ==> holder(pre.state) == Some(worker),
        next(pre, ev)->0.state matches JobState::Done { worker } ==> pre.state == (
        JobState::Running { worker }),
{
}

/// The holder can only change by claiming an unheld (Open) job: a job is never claimed by two
/// workers at once, and no worker can take over another's claim.
pub proof fn lemma_holder_changes_only_by_claim(pre: JobView, ev: Event)
    requires
        next(pre, ev) is Some,
        holder(next(pre, ev)->0.state) is Some,
        holder(next(pre, ev)->0.state) != holder(pre.state),
    ensures
        pre.state is Open,
        ev == (Event::Claim { worker: holder(next(pre, ev)->0.state)->0 }),
{
}

/// Every accepted event keeps the retry budget bounded and strictly consumes progress.
pub proof fn lemma_step_bounded(pre: JobView, ev: Event)
    requires
        wf(pre),
        next(pre, ev) is Some,
    ensures
        wf(next(pre, ev)->0),
        next(pre, ev)->0.max_retries == pre.max_retries,
        next(pre, ev)->0.retries <= pre.retries + 1,
        0 <= remaining_steps(next(pre, ev)->0) < remaining_steps(pre),
{
}

/// Replays an event sequence; returns the final view and how many events were accepted.
pub open spec fn run(v: JobView, evs: Seq<Event>) -> (JobView, nat)
    decreases evs.len(),
{
    if evs.len() == 0 {
        (v, 0)
    } else {
        match next(v, evs[0]) {
            Some(post) => {
                let (fin, n) = run(post, evs.drop_first());
                (fin, n + 1)
            },
            None => run(v, evs.drop_first()),
        }
    }
}

/// Retries are bounded, therefore every job accepts at most `4 * max_retries + 3` events in any
/// history, whatever the workers and the lead do.
pub proof fn theorem_bounded_lifecycle(v: JobView, evs: Seq<Event>)
    requires
        wf(v),
    ensures
        wf(run(v, evs).0),
        run(v, evs).1 <= remaining_steps(v),
    decreases evs.len(),
{
    if evs.len() > 0 {
        match next(v, evs[0]) {
            Some(post) => {
                lemma_step_bounded(v, evs[0]);
                theorem_bounded_lifecycle(post, evs.drop_first());
            },
            None => {
                theorem_bounded_lifecycle(v, evs.drop_first());
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Executable implementation, proven to refine `next`.
// ---------------------------------------------------------------------------------------------
/// A job on the blackboard. Fields are private; the type invariant bounds the retry counter.
pub struct Job {
    state: JobState,
    retries: u32,
    max_retries: u32,
}

impl View for Job {
    type V = JobView;

    closed spec fn view(&self) -> JobView {
        JobView {
            state: self.state,
            retries: self.retries as nat,
            max_retries: self.max_retries as nat,
        }
    }
}

impl Job {
    #[verifier::type_invariant]
    spec fn inv(self) -> bool {
        self.retries <= self.max_retries
    }

    /// The view of a freshly posted job.
    pub open spec fn new_view(max_retries: u32) -> JobView {
        JobView { state: JobState::Open, retries: 0, max_retries: max_retries as nat }
    }

    /// Posts a fresh open job with a retry budget of `max_retries`.
    #[must_use]
    pub const fn new(max_retries: u32) -> (job: Self)
        ensures
            job@ == Self::new_view(max_retries),
    {
        Self { state: JobState::Open, retries: 0, max_retries }
    }

    /// The job's current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> (s: JobState)
        ensures
            s == self@.state,
    {
        self.state
    }

    /// Retries consumed so far (never above the budget).
    #[must_use]
    pub const fn retries(&self) -> (r: u32)
        ensures
            r as nat == self@.retries,
            wf(self@),
    {
        proof {
            use_type_invariant(self);
        }
        self.retries
    }

    /// Private fast path: its `requires` is an assumption about the caller, so it must never be
    /// reachable from unverified code. Public callers go through [`Job::apply`].
    const fn complete_unchecked(&mut self, worker: WorkerId)
        requires
            old(self)@.state == (JobState::Running { worker }),
        ensures
            final(self)@ == with_state(old(self)@, JobState::Done { worker }),
    {
        proof {
            use_type_invariant(&*self);
        }
        self.state = JobState::Done { worker };
    }

    const fn retry_or_fail(&mut self)
        requires
            holder(old(self)@.state) is Some,
        ensures
            final(self)@ == retry_or_fail(old(self)@),
    {
        proof {
            use_type_invariant(&*self);
        }
        if self.retries < self.max_retries {
            // No overflow: the type invariant plus this branch give retries + 1 <= max_retries.
            self.retries += 1;
            self.state = JobState::Open;
        } else {
            self.state = JobState::Failed;
        }
    }

    /// The only mutation entry point. Total: no `requires`, so unverified callers cannot break it.
    ///
    /// # Errors
    /// The event is rejected (and the job left unchanged) exactly when [`next`] returns `None`.
    pub const fn apply(&mut self, ev: Event) -> (r: Result<(), LifecycleError>)
        ensures
            match next(old(self)@, ev) {
                Some(post) => r is Ok && final(self)@ == post,
                None => r is Err && final(self)@ == old(self)@,
            },
    {
        proof {
            use_type_invariant(&*self);
        }
        match self.state {
            JobState::Done { .. } | JobState::Failed | JobState::Cancelled => {
                return Err(LifecycleError::Terminal);
            },
            _ => {},
        }
        match ev {
            Event::Claim { worker } => match self.state {
                JobState::Open => {
                    self.state = JobState::Claimed { worker };
                    Ok(())
                },
                _ => Err(LifecycleError::WrongState),
            },
            Event::Start { worker } => match self.state {
                JobState::Claimed { worker: h } => if h == worker {
                    self.state = JobState::Running { worker };
                    Ok(())
                } else {
                    Err(LifecycleError::NotHolder)
                },
                _ => Err(LifecycleError::WrongState),
            },
            Event::Complete { worker } => match self.state {
                JobState::Running { worker: h } => if h == worker {
                    self.complete_unchecked(worker);
                    Ok(())
                } else {
                    Err(LifecycleError::NotHolder)
                },
                _ => Err(LifecycleError::WrongState),
            },
            Event::Fail { worker } => match self.state {
                JobState::Running { worker: h } => if h == worker {
                    self.retry_or_fail();
                    Ok(())
                } else {
                    Err(LifecycleError::NotHolder)
                },
                _ => Err(LifecycleError::WrongState),
            },
            Event::Expire => match self.state {
                JobState::Claimed { .. } | JobState::Running { .. } => {
                    self.retry_or_fail();
                    Ok(())
                },
                _ => Err(LifecycleError::WrongState),
            },
            Event::Cancel => {
                self.state = JobState::Cancelled;
                Ok(())
            },
        }
    }
}

} // verus!
