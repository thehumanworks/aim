//! Fenced job attempts and independent result review.
//!
//! DRAFT(M5a): the lead will lock this decision after reviewing the ledger contract.
//! Lease expiry fences an attempt but does not prove its process stopped.
#![expect(clippy::match_like_matches_macro, reason = "Verus proves these explicit pattern matches against enum predicates")]
use vstd::prelude::*;

verus! {

/// Worker identity supplied by the service.
pub type WorkerId = u64;

/// Execution state; acceptance is separate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobState {
    /// Awaiting a claim.
    Posted,
    /// Claimed, awaiting launch.
    Claimed,
    /// Executing.
    Running,
    /// Result delivered, awaiting review.
    Succeeded,
    /// Attempt failed or expired.
    Failed,
    /// Cancelled by the lead.
    Cancelled,
}

/// Independent review decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewState {
    /// No review decision.
    Pending,
    /// Evidence accepted.
    Accepted,
    /// Result rejected.
    Rejected,
}

/// Process ownership after an attempt changes state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CleanupState {
    /// No process remains.
    Confirmed,
    /// Current attempt owns a live slot.
    Active,
    /// The old process may still run; its slot remains reserved.
    Pending,
}

/// Current or last attempt identity. The service authenticates the secret token separately.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Claim {
    /// Worker that owns the attempt.
    pub worker: WorkerId,
    /// Nonsecret identifier bound to the hashed token in the ledger.
    pub claim_id: u64,
    /// Exclusive lease deadline in service-supplied time.
    pub lease_until: u64,
}

/// A job transition request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// Reserve this generation.
    Claim {
        /// Claiming worker.
        worker: WorkerId,
        /// Nonsecret claim identity.
        claim_id: u64,
        /// Exclusive lease deadline.
        lease_until: u64,
    },
    /// Launch the current attempt.
    Start {
        /// Current generation.
        generation: u32,
        /// Current claim identity.
        claim_id: u64,
    },
    /// Extend the current lease.
    Heartbeat {
        /// Current generation.
        generation: u32,
        /// Current claim identity.
        claim_id: u64,
        /// Extended exclusive lease deadline.
        lease_until: u64,
    },
    /// Deliver a result without accepting it.
    Complete {
        /// Current generation.
        generation: u32,
        /// Current claim identity.
        claim_id: u64,
    },
    /// Fail and report whether cleanup was confirmed.
    Fail {
        /// Current generation.
        generation: u32,
        /// Current claim identity.
        claim_id: u64,
        /// Whether the old process was confirmed stopped.
        cleanup_confirmed: bool,
    },
    /// Fence an attempt whose lease expired.
    Expire {
        /// Current generation.
        generation: u32,
    },
    /// Confirm an old process has stopped.
    ConfirmCleanup,
    /// Open a new generation within the retry budget.
    Retry,
    /// Cancel an open or active job.
    Cancel,
    /// Review a delivered result.
    Review {
        /// Reviewer's decision.
        accepted: bool,
        /// Evidence reference was validated.
        evidence_present: bool,
    },
}

/// Why an event or persisted snapshot was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LifecycleError {
    /// Invalid in this execution state.
    WrongState,
    /// An older or different attempt.
    StaleClaim,
    /// Missing or nonadvancing lease.
    InvalidLease,
    /// No retry remains.
    RetryExhausted,
    /// The previous process may still run.
    CleanupPending,
    /// Acceptance requires evidence.
    MissingEvidence,
    /// Persisted fields do not form a valid state.
    InvalidSnapshot,
}

/// Abstract persisted job state.
pub struct JobView {
    /// Execution state.
    pub state: JobState,
    /// Review decision.
    pub review: ReviewState,
    /// Attempt generation, initially zero.
    pub generation: nat,
    /// Maximum retry count.
    pub max_retries: nat,
    /// Current or last attempt identity.
    pub claim: Option<Claim>,
    /// Process cleanup status.
    pub cleanup: CleanupState,
    /// Review evidence was present.
    pub evidence_present: bool,
}

/// Valid persisted state.
pub open spec fn wf(v: JobView) -> bool {
    &&& v.generation <= v.max_retries
    &&& (v.state == JobState::Posted ==> v.claim is None && v.cleanup == CleanupState::Confirmed
        && v.review == ReviewState::Pending && !v.evidence_present)
    &&& ((v.state == JobState::Claimed || v.state == JobState::Running) ==> v.claim is Some
        && v.cleanup == CleanupState::Active && v.review == ReviewState::Pending
        && !v.evidence_present)
    &&& (v.state == JobState::Succeeded ==> v.claim is Some && v.cleanup == CleanupState::Confirmed)
    &&& ((v.state == JobState::Failed || v.state == JobState::Cancelled) ==> v.review
        == ReviewState::Pending && (v.claim is Some || v.cleanup == CleanupState::Confirmed))
    &&& (v.review == ReviewState::Accepted ==> v.state == JobState::Succeeded && v.evidence_present)
    &&& (v.review == ReviewState::Rejected ==> v.state == JobState::Succeeded)
}

/// Fresh posted state.
pub open spec fn fresh(max_retries: u32) -> JobView {
    JobView {
        state: JobState::Posted,
        review: ReviewState::Pending,
        generation: 0,
        max_retries: max_retries as nat,
        claim: None,
        cleanup: CleanupState::Confirmed,
        evidence_present: false,
    }
}

/// Current, unexpired attempt identity.
pub open spec fn claim_is_current(v: JobView, generation: u32, claim_id: u64, now: u64) -> bool {
    v.generation == generation as nat && v.claim is Some && v.claim->0.claim_id == claim_id && now
        < v.claim->0.lease_until
}

/// Whether this job reserves a worker slot, including uncertain cleanup.
pub open spec fn holds_capacity(v: JobView, worker: WorkerId) -> bool {
    v.claim is Some && v.claim->0.worker == worker && (v.cleanup == CleanupState::Active
        || v.cleanup == CleanupState::Pending)
}

/// Accepted evidence available to dependent jobs.
pub open spec fn accepted(v: JobView) -> bool {
    v.state == JobState::Succeeded && v.review == ReviewState::Accepted && v.evidence_present
}

/// DRAFT(M5a): a complete transition or `None` when rejected.
pub open spec fn next(pre: JobView, ev: Event, now: u64) -> Option<JobView> {
    match ev {
        Event::Claim { worker, claim_id, lease_until } => if pre.state == JobState::Posted && now
            < lease_until {
            Some(
                JobView {
                    state: JobState::Claimed,
                    claim: Some(Claim { worker, claim_id, lease_until }),
                    cleanup: CleanupState::Active,
                    ..pre
                },
            )
        } else {
            None
        },
        Event::Start { generation, claim_id } => if pre.state == JobState::Claimed
            && claim_is_current(pre, generation, claim_id, now) {
            Some(JobView { state: JobState::Running, ..pre })
        } else {
            None
        },
        Event::Heartbeat { generation, claim_id, lease_until } => if (pre.state == JobState::Claimed
            || pre.state == JobState::Running) && claim_is_current(pre, generation, claim_id, now)
            && lease_until > pre.claim->0.lease_until {
            Some(JobView { claim: Some(Claim { lease_until, ..pre.claim->0 }), ..pre })
        } else {
            None
        },
        Event::Complete { generation, claim_id } => if pre.state == JobState::Running
            && claim_is_current(pre, generation, claim_id, now) {
            Some(JobView { state: JobState::Succeeded, cleanup: CleanupState::Confirmed, ..pre })
        } else {
            None
        },
        Event::Fail { generation, claim_id, cleanup_confirmed } => if pre.state == JobState::Running
            && claim_is_current(pre, generation, claim_id, now) {
            Some(
                JobView {
                    state: JobState::Failed,
                    cleanup: if cleanup_confirmed {
                        CleanupState::Confirmed
                    } else {
                        CleanupState::Pending
                    },
                    ..pre
                },
            )
        } else {
            None
        },
        Event::Expire { generation } => if (pre.state == JobState::Claimed || pre.state
            == JobState::Running) && pre.generation == generation as nat && pre.claim is Some && now
            >= pre.claim->0.lease_until {
            Some(JobView { state: JobState::Failed, cleanup: CleanupState::Pending, ..pre })
        } else {
            None
        },
        Event::ConfirmCleanup => if (pre.state == JobState::Failed || pre.state
            == JobState::Cancelled) && pre.cleanup == CleanupState::Pending {
            Some(JobView { cleanup: CleanupState::Confirmed, ..pre })
        } else {
            None
        },
        Event::Retry => if (pre.state == JobState::Failed || pre.state == JobState::Cancelled || (
        pre.state == JobState::Succeeded && pre.review == ReviewState::Rejected)) && pre.cleanup
            == CleanupState::Confirmed && pre.generation < pre.max_retries {
            Some(
                JobView {
                    state: JobState::Posted,
                    review: ReviewState::Pending,
                    generation: pre.generation + 1,
                    claim: None,
                    evidence_present: false,
                    ..pre
                },
            )
        } else {
            None
        },
        Event::Cancel => if pre.state == JobState::Posted || pre.state == JobState::Claimed
            || pre.state == JobState::Running {
            Some(
                JobView {
                    state: JobState::Cancelled,
                    cleanup: if pre.state == JobState::Posted {
                        CleanupState::Confirmed
                    } else {
                        CleanupState::Pending
                    },
                    ..pre
                },
            )
        } else {
            None
        },
        Event::Review { accepted: decision, evidence_present } => if pre.state
            == JobState::Succeeded && pre.review == ReviewState::Pending && (!decision
            || evidence_present) {
            Some(
                JobView {
                    review: if decision {
                        ReviewState::Accepted
                    } else {
                        ReviewState::Rejected
                    },
                    evidence_present,
                    ..pre
                },
            )
        } else {
            None
        },
    }
}

/// Every accepted transition preserves the snapshot invariant.
pub proof fn lemma_step_wf(pre: JobView, ev: Event, now: u64)
    requires
        wf(pre),
        next(pre, ev, now) is Some,
    ensures
        wf(next(pre, ev, now)->0),
{
}

/// Completing work alone cannot accept it.
pub proof fn lemma_completion_needs_review(pre: JobView, generation: u32, claim_id: u64, now: u64)
    requires
        wf(pre),
        next(pre, Event::Complete { generation, claim_id }, now) is Some,
    ensures
        next(pre, Event::Complete { generation, claim_id }, now)->0.review is Pending,
{
}

/// An old generation, wrong claim id, or expired lease cannot mutate an active attempt.
pub proof fn lemma_stale_attempt_fenced(pre: JobView, generation: u32, claim_id: u64, now: u64)
    requires
        wf(pre),
        !claim_is_current(pre, generation, claim_id, now),
    ensures
        next(pre, Event::Start { generation, claim_id }, now) is None,
        next(pre, Event::Heartbeat { generation, claim_id, lease_until: u64::MAX }, now) is None,
        next(pre, Event::Complete { generation, claim_id }, now) is None,
        next(pre, Event::Fail { generation, claim_id, cleanup_confirmed: true }, now) is None,
{
}

/// A second claim cannot succeed within the same generation.
pub proof fn lemma_claim_exclusive(
    pre: JobView,
    first_worker: WorkerId,
    first_id: u64,
    first_lease: u64,
    second_worker: WorkerId,
    second_id: u64,
    second_lease: u64,
    first_now: u64,
    second_now: u64,
)
    requires
        wf(pre),
        next(
            pre,
            Event::Claim { worker: first_worker, claim_id: first_id, lease_until: first_lease },
            first_now,
        ) is Some,
    ensures
        next(
            next(
                pre,
                Event::Claim { worker: first_worker, claim_id: first_id, lease_until: first_lease },
                first_now,
            )->0,
            Event::Claim { worker: second_worker, claim_id: second_id, lease_until: second_lease },
            second_now,
        ) is None,
{
}

/// Retrying advances exactly one generation, never past the fixed bound.
pub proof fn lemma_retry_bounded(pre: JobView, now: u64)
    requires
        wf(pre),
        next(pre, Event::Retry, now) is Some,
    ensures
        next(pre, Event::Retry, now)->0.generation == pre.generation + 1,
        next(pre, Event::Retry, now)->0.generation <= pre.max_retries,
        next(pre, Event::Retry, now)->0.claim is None,
{
}

/// An uncertain cleanup blocks retry.
pub proof fn lemma_pending_cleanup_fences_retry(pre: JobView, now: u64)
    requires
        wf(pre),
        pre.cleanup == CleanupState::Pending,
    ensures
        next(pre, Event::Retry, now) is None,
{
}

fn is_posted(s: JobState) -> (b: bool)
    ensures
        b == (s == JobState::Posted),
{
    match s {
        JobState::Posted => true,
        _ => false,
    }
}

fn is_claimed(s: JobState) -> (b: bool)
    ensures
        b == (s == JobState::Claimed),
{
    match s {
        JobState::Claimed => true,
        _ => false,
    }
}

fn is_running(s: JobState) -> (b: bool)
    ensures
        b == (s == JobState::Running),
{
    match s {
        JobState::Running => true,
        _ => false,
    }
}

fn is_succeeded(s: JobState) -> (b: bool)
    ensures
        b == (s == JobState::Succeeded),
{
    match s {
        JobState::Succeeded => true,
        _ => false,
    }
}

fn is_failed(s: JobState) -> (b: bool)
    ensures
        b == (s == JobState::Failed),
{
    match s {
        JobState::Failed => true,
        _ => false,
    }
}

fn is_cancelled(s: JobState) -> (b: bool)
    ensures
        b == (s == JobState::Cancelled),
{
    match s {
        JobState::Cancelled => true,
        _ => false,
    }
}

fn is_review_pending(s: ReviewState) -> (b: bool)
    ensures
        b == (s == ReviewState::Pending),
{
    match s {
        ReviewState::Pending => true,
        _ => false,
    }
}

fn is_review_accepted(s: ReviewState) -> (b: bool)
    ensures
        b == (s == ReviewState::Accepted),
{
    match s {
        ReviewState::Accepted => true,
        _ => false,
    }
}

fn is_review_rejected(s: ReviewState) -> (b: bool)
    ensures
        b == (s == ReviewState::Rejected),
{
    match s {
        ReviewState::Rejected => true,
        _ => false,
    }
}

fn is_cleanup_confirmed(s: CleanupState) -> (b: bool)
    ensures
        b == (s == CleanupState::Confirmed),
{
    match s {
        CleanupState::Confirmed => true,
        _ => false,
    }
}

fn is_cleanup_active(s: CleanupState) -> (b: bool)
    ensures
        b == (s == CleanupState::Active),
{
    match s {
        CleanupState::Active => true,
        _ => false,
    }
}

fn is_cleanup_pending(s: CleanupState) -> (b: bool)
    ensures
        b == (s == CleanupState::Pending),
{
    match s {
        CleanupState::Pending => true,
        _ => false,
    }
}

/// A job with private fields maintaining `wf`.
pub struct Job {
    state: JobState,
    review: ReviewState,
    generation: u32,
    max_retries: u32,
    claim: Option<Claim>,
    cleanup: CleanupState,
    evidence_present: bool,
}

impl View for Job {
    type V = JobView;

    closed spec fn view(&self) -> JobView {
        JobView {
            state: self.state,
            review: self.review,
            generation: self.generation as nat,
            max_retries: self.max_retries as nat,
            claim: self.claim,
            cleanup: self.cleanup,
            evidence_present: self.evidence_present,
        }
    }
}

impl Job {
    /// Post a fresh job with a fixed retry budget.
    #[must_use]
    pub const fn new(max_retries: u32) -> (job: Self)
        ensures
            job@ == fresh(max_retries),
    {
        Self {
            state: JobState::Posted,
            review: ReviewState::Pending,
            generation: 0,
            max_retries,
            claim: None,
            cleanup: CleanupState::Confirmed,
            evidence_present: false,
        }
    }

    /// Rehydrate a persisted snapshot after checking its structure.
    ///
    /// # Errors
    /// Returns `InvalidSnapshot` for inconsistent fields.
    pub fn restore(
        state: JobState,
        review: ReviewState,
        generation: u32,
        claim: Option<Claim>,
        cleanup: CleanupState,
        max_retries: u32,
        evidence_present: bool,
    ) -> (r: Result<Self, LifecycleError>)
        ensures
            match r {
                Ok(job) => wf(job@) && job@.state == state && job@.review == review
                    && job@.generation == generation as nat && job@.claim == claim && job@.cleanup
                    == cleanup && job@.max_retries == max_retries as nat && job@.evidence_present
                    == evidence_present,
                Err(_) => true,
            },
    {
        proof {
            reveal(<Job as View>::view);
        }
        if generation > max_retries {
            return Err(LifecycleError::InvalidSnapshot);
        }
        match state {
            JobState::Posted => {
                if claim.is_some() || !is_cleanup_confirmed(cleanup) || !is_review_pending(review)
                    || evidence_present {
                    return Err(LifecycleError::InvalidSnapshot);
                }
            },
            JobState::Claimed | JobState::Running => {
                if claim.is_none() || !is_cleanup_active(cleanup) || !is_review_pending(review)
                    || evidence_present {
                    return Err(LifecycleError::InvalidSnapshot);
                }
            },
            JobState::Succeeded => {
                if claim.is_none() || !is_cleanup_confirmed(cleanup) || (is_review_accepted(review)
                    && !evidence_present) {
                    return Err(LifecycleError::InvalidSnapshot);
                }
            },
            JobState::Failed | JobState::Cancelled => {
                if !is_review_pending(review) || (claim.is_none() && !is_cleanup_confirmed(
                    cleanup,
                )) {
                    return Err(LifecycleError::InvalidSnapshot);
                }
            },
        }
        Ok(Self { state, review, generation, max_retries, claim, cleanup, evidence_present })
    }

    /// Execution state.
    #[must_use]
    pub const fn state(&self) -> (s: JobState)
        ensures
            s == self@.state,
    {
        self.state
    }

    /// Review state.
    #[must_use]
    pub const fn review(&self) -> (s: ReviewState)
        ensures
            s == self@.review,
    {
        self.review
    }

    /// Current generation.
    #[must_use]
    pub const fn generation(&self) -> (g: u32)
        ensures
            g as nat == self@.generation,
    {
        self.generation
    }

    /// Fixed retry budget.
    #[must_use]
    pub const fn max_retries(&self) -> (g: u32)
        ensures
            g as nat == self@.max_retries,
    {
        self.max_retries
    }

    /// Current or last claim.
    #[must_use]
    pub const fn claim(&self) -> (c: Option<Claim>)
        ensures
            c == self@.claim,
    {
        self.claim
    }

    /// Cleanup status.
    #[must_use]
    pub const fn cleanup(&self) -> (c: CleanupState)
        ensures
            c == self@.cleanup,
    {
        self.cleanup
    }

    /// Whether review evidence was present.
    #[must_use]
    pub const fn evidence_present(&self) -> (e: bool)
        ensures
            e == self@.evidence_present,
    {
        self.evidence_present
    }

    /// Accepted evidence may unblock dependent jobs.
    #[must_use]
    pub fn is_accepted(&self) -> (b: bool)
        ensures
            b == accepted(self@),
    {
        match self.state {
            JobState::Succeeded => match self.review {
                ReviewState::Accepted => self.evidence_present,
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether this worker still has a capacity hold.
    #[must_use]
    pub fn holds_capacity(&self, worker: WorkerId) -> (b: bool)
        ensures
            b == holds_capacity(self@, worker),
    {
        match self.claim {
            Some(c) => match self.cleanup {
                CleanupState::Active | CleanupState::Pending => c.worker == worker,
                CleanupState::Confirmed => false,
            },
            None => false,
        }
    }

    /// Whether a message or other attempt-scoped write has a live claim.
    #[must_use]
    pub fn authorizes_attempt(&self, generation: u32, claim_id: u64, now: u64) -> (b: bool)
        ensures
            b == claim_is_current(self@, generation, claim_id, now),
    {
        self.check_claim(generation, claim_id, now)
    }

    /// Apply an event, preserving the snapshot on rejection.
    ///
    /// # Errors
    /// Invalid state, claim, lease, retry, cleanup, or review.
    #[expect(
        clippy::too_many_lines,
        clippy::needless_return,
        clippy::nonminimal_bool,
        reason = "one independently proved return path per lifecycle event"
    )]
    pub fn apply(&mut self, ev: Event, now: u64) -> (r: Result<(), LifecycleError>)
        ensures
            r is Ok ==> next(old(self)@, ev, now) == Some(final(self)@),
            r is Err ==> final(self)@ == old(self)@,
            next(old(self)@, ev, now) is None ==> r is Err,
    {
        proof {
            reveal(<Job as View>::view);
        }
        match ev {
            Event::Claim { worker, claim_id, lease_until } => {
                if !is_posted(self.state) {
                    proof {
                        assert(old(self)@.state != JobState::Posted);
                    }
                    return Err(LifecycleError::WrongState);
                }
                if now >= lease_until {
                    return Err(LifecycleError::InvalidLease);
                }
                self.state = JobState::Claimed;
                self.claim = Some(Claim { worker, claim_id, lease_until });
                self.cleanup = CleanupState::Active;
                return Ok(());
            },
            Event::Start { generation, claim_id } => {
                if !is_claimed(self.state) {
                    return Err(LifecycleError::WrongState);
                }
                if !self.check_claim(generation, claim_id, now) {
                    return Err(LifecycleError::StaleClaim);
                }
                self.state = JobState::Running;
                return Ok(());
            },
            Event::Heartbeat { generation, claim_id, lease_until } => {
                if !is_claimed(self.state) && !is_running(self.state) {
                    return Err(LifecycleError::WrongState);
                }
                if !self.check_claim(generation, claim_id, now) {
                    return Err(LifecycleError::StaleClaim);
                }
                if let Some(c) = self.claim {
                    if lease_until <= c.lease_until {
                        return Err(LifecycleError::InvalidLease);
                    }
                    self.claim = Some(Claim { lease_until, ..c });
                }
                return Ok(());
            },
            Event::Complete { generation, claim_id } => {
                if !is_running(self.state) {
                    return Err(LifecycleError::WrongState);
                }
                if !self.check_claim(generation, claim_id, now) {
                    return Err(LifecycleError::StaleClaim);
                }
                self.state = JobState::Succeeded;
                self.cleanup = CleanupState::Confirmed;
                return Ok(());
            },
            Event::Fail { generation, claim_id, cleanup_confirmed } => {
                if !is_running(self.state) {
                    return Err(LifecycleError::WrongState);
                }
                if !self.check_claim(generation, claim_id, now) {
                    return Err(LifecycleError::StaleClaim);
                }
                self.state = JobState::Failed;
                self.cleanup = if cleanup_confirmed {
                    CleanupState::Confirmed
                } else {
                    CleanupState::Pending
                };
                return Ok(());
            },
            Event::Expire { generation } => {
                if !is_claimed(self.state) && !is_running(self.state) {
                    return Err(LifecycleError::WrongState);
                }
                if self.generation != generation {
                    return Err(LifecycleError::StaleClaim);
                }
                if let Some(c) = self.claim {
                    if now < c.lease_until {
                        return Err(LifecycleError::InvalidLease);
                    }
                } else {
                    return Err(LifecycleError::StaleClaim);
                }
                self.state = JobState::Failed;
                self.cleanup = CleanupState::Pending;
                return Ok(());
            },
            Event::ConfirmCleanup => {
                if (!is_failed(self.state) && !is_cancelled(self.state)) || !is_cleanup_pending(
                    self.cleanup,
                ) {
                    return Err(LifecycleError::WrongState);
                }
                self.cleanup = CleanupState::Confirmed;
                return Ok(());
            },
            Event::Retry => {
                if !is_failed(self.state) && !is_cancelled(self.state) && !(is_succeeded(self.state)
                    && is_review_rejected(self.review)) {
                    return Err(LifecycleError::WrongState);
                }
                if !is_cleanup_confirmed(self.cleanup) {
                    return Err(LifecycleError::CleanupPending);
                }
                if self.generation >= self.max_retries {
                    return Err(LifecycleError::RetryExhausted);
                }
                self.generation += 1;
                self.state = JobState::Posted;
                self.review = ReviewState::Pending;
                self.claim = None;
                self.evidence_present = false;
                return Ok(());
            },
            Event::Cancel => {
                match self.state {
                    JobState::Posted => {
                        self.cleanup = CleanupState::Confirmed;
                    },
                    JobState::Claimed | JobState::Running => {
                        self.cleanup = CleanupState::Pending;
                    },
                    _ => return Err(LifecycleError::WrongState),
                }
                self.state = JobState::Cancelled;
                return Ok(());
            },
            Event::Review { accepted, evidence_present } => {
                if !is_succeeded(self.state) || !is_review_pending(self.review) {
                    return Err(LifecycleError::WrongState);
                }
                if accepted && !evidence_present {
                    return Err(LifecycleError::MissingEvidence);
                }
                self.review = if accepted {
                    ReviewState::Accepted
                } else {
                    ReviewState::Rejected
                };
                self.evidence_present = evidence_present;
                return Ok(());
            },
        }
    }

    fn check_claim(&self, generation: u32, claim_id: u64, now: u64) -> (ok: bool)
        ensures
            ok == claim_is_current(self@, generation, claim_id, now),
    {
        if generation != self.generation {
            return false;
        }
        match self.claim {
            Some(c) => c.claim_id == claim_id && now < c.lease_until,
            None => false,
        }
    }
}

} // verus!
