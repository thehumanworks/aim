//! The native agent loop's turn state machine (docs/architecture.md §6.2).
//!
//! Status: DRAFT(M2a) decision. It becomes LOCKED with its ADR once the native loop has run
//! against every provider.
//!
//! One *turn* is everything between a user input and the model's final answer: possibly several
//! model requests, each of which may ask for tool calls, with user *steering* arriving at any time.
//! The agent loop feeds this machine what happened and does only what it allows. The decision,
//! stated as [`next`]:
//!
//! - a tool call is *dispatched* only when the provider delivered it complete, at most once per id;
//! - a result is accepted only for a dispatched call that has none yet;
//! - the next model request is sent only when every dispatched call has its result, and it
//!   delivers every queued steer;
//! - a response without new calls ends the turn — unless steers are queued, which continue it;
//! - a response the model may not continue (e.g. filtered), or a cancellation, *winds the turn
//!   down*: queued steers are returned to the caller, and every outstanding call must still receive
//!   a (cancelled) result before the turn settles — so the transcript never holds a call without a
//!   result event, and no steer is ever lost;
//! - a settled turn accepts nothing.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// Identity of one tool call within a turn (the loop maps provider call ids to these).
pub type CallId = u64;

/// Where the turn is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// A model response is streaming (complete calls are dispatched as they arrive).
    Streaming,
    /// The response finished; dispatched tool calls are still running.
    AwaitingResults,
    /// Every call has its result; the next model request may be sent.
    Ready,
    /// Winding down: outstanding calls must still receive their (cancelled) results.
    Cancelling,
    /// The turn is over. Absorbs every event.
    Settled,
}

/// Something that happened during a turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The provider delivered a complete tool call.
    CallComplete {
        /// The call.
        id: CallId,
    },
    /// The model response finished streaming.
    ResponseDone {
        /// The model may be asked again (false after e.g. a content filter stop).
        may_continue: bool,
    },
    /// A dispatched call produced its result (in `Cancelling`: its cancelled result).
    Result {
        /// The call.
        id: CallId,
    },
    /// The loop starts the next model request (delivering every queued steer).
    NextRequest,
    /// The user steers the running turn.
    Steer,
    /// The user (or the system) cancels the turn.
    Cancel,
}

/// Why an event was rejected (the turn is left unchanged).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TurnError {
    /// The turn is settled.
    Settled,
    /// The event is not valid in the current phase.
    WrongPhase,
    /// A call with this id was already dispatched in this turn.
    DuplicateCall,
    /// No dispatched call with this id is waiting for a result.
    UnknownOrAnswered,
}

/// Abstract view of a turn; all specifications are stated over it.
pub struct TurnView {
    /// Current phase.
    pub phase: Phase,
    /// Dispatched calls of the whole turn, in dispatch order.
    pub calls: Seq<CallId>,
    /// `answered[i]` holds when `calls[i]` has its result.
    pub answered: Seq<bool>,
    /// Number of calls dispatched before the current model request started.
    pub request_start: nat,
    /// Steers accepted so far.
    pub steers: nat,
    /// Steers waiting for the next request.
    pub queued: nat,
    /// Steers delivered with a request.
    pub delivered: nat,
    /// Steers handed back to the caller when the turn wound down.
    pub returned: nat,
}

/// Every dispatched call has its result.
pub open spec fn all_answered(v: TurnView) -> bool {
    forall|i: int| 0 <= i < v.answered.len() ==> #[trigger] v.answered[i]
}

/// Well-formed: one flag per call, no call dispatched twice, calls of earlier requests answered,
/// `Ready`/`Settled` only when every call is answered, and every steer accounted for.
pub open spec fn wf(v: TurnView) -> bool {
    &&& v.answered.len() == v.calls.len()
    &&& v.calls.no_duplicates()
    &&& v.request_start <= v.calls.len()
    &&& forall|i: int| 0 <= i < v.request_start ==> #[trigger] v.answered[i]
    &&& (v.phase is Ready || v.phase is Settled) ==> all_answered(v)
    &&& v.queued + v.delivered + v.returned == v.steers
    &&& (v.phase is Cancelling || v.phase is Settled) ==> v.queued == 0
}

/// A new turn: the first model request is streaming and nothing is dispatched.
pub open spec fn start() -> TurnView {
    TurnView {
        phase: Phase::Streaming,
        calls: Seq::empty(),
        answered: Seq::empty(),
        request_start: 0,
        steers: 0,
        queued: 0,
        delivered: 0,
        returned: 0,
    }
}

/// `v` in phase `p`.
pub open spec fn in_phase(v: TurnView, p: Phase) -> TurnView {
    TurnView { phase: p, ..v }
}

/// Winding down: queued steers go back to the caller; wait for outstanding results if any.
pub open spec fn wind_down(v: TurnView) -> TurnView {
    TurnView {
        phase: if all_answered(v) {
            Phase::Settled
        } else {
            Phase::Cancelling
        },
        queued: 0,
        returned: v.returned + v.queued,
        ..v
    }
}

/// DRAFT(M2a) decision: the complete transition function. `None` = rejected, turn unchanged.
pub open spec fn next(pre: TurnView, ev: Event) -> Option<TurnView> {
    match ev {
        Event::CallComplete { id } => if pre.phase is Streaming && !pre.calls.contains(id) {
            Some(TurnView { calls: pre.calls.push(id), answered: pre.answered.push(false), ..pre })
        } else {
            None
        },
        Event::ResponseDone { may_continue } => if !(pre.phase is Streaming) {
            None
        } else if !may_continue {
            Some(wind_down(pre))
        } else if pre.calls.len() == pre.request_start {
            // No new calls: the final answer — unless steers are queued, which continue the turn.
            if pre.queued > 0 {
                Some(in_phase(pre, Phase::Ready))
            } else {
                Some(in_phase(pre, Phase::Settled))
            }
        } else if all_answered(pre) {
            Some(in_phase(pre, Phase::Ready))
        } else {
            Some(in_phase(pre, Phase::AwaitingResults))
        },
        Event::Result { id } => if (pre.phase is Streaming || pre.phase is AwaitingResults
            || pre.phase is Cancelling) && exists|i: int|
            0 <= i < pre.calls.len() && pre.calls[i] == id && !pre.answered[i] {
            let i = choose|i: int|
                0 <= i < pre.calls.len() && pre.calls[i] == id && !pre.answered[i];
            let post = TurnView { answered: pre.answered.update(i, true), ..pre };
            if pre.phase is Streaming {
                Some(post)
            } else if all_answered(post) {
                Some(
                    in_phase(
                        post,
                        if pre.phase is Cancelling {
                            Phase::Settled
                        } else {
                            Phase::Ready
                        },
                    ),
                )
            } else {
                Some(post)
            }
        } else {
            None
        },
        Event::NextRequest => if pre.phase is Ready {
            Some(
                TurnView {
                    phase: Phase::Streaming,
                    request_start: pre.calls.len(),
                    queued: 0,
                    delivered: pre.delivered + pre.queued,
                    ..pre
                },
            )
        } else {
            None
        },
        Event::Steer => if pre.phase is Streaming || pre.phase is AwaitingResults
            || pre.phase is Ready {
            Some(TurnView { steers: pre.steers + 1, queued: pre.queued + 1, ..pre })
        } else {
            None
        },
        Event::Cancel => if pre.phase is Streaming || pre.phase is AwaitingResults
            || pre.phase is Ready {
            Some(wind_down(pre))
        } else {
            None
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Theorems about the decision.
// ---------------------------------------------------------------------------------------------
/// A new turn is well-formed.
pub proof fn lemma_start_wf()
    ensures
        wf(start()),
{
}

/// A settled turn accepts nothing.
pub proof fn lemma_settled_absorbs(pre: TurnView, ev: Event)
    requires
        pre.phase is Settled,
    ensures
        next(pre, ev) is None,
{
}

/// Every accepted event keeps the turn well-formed: no call is ever dispatched twice,
/// `Ready`/`Settled` always mean every call has its result, and every steer is accounted for.
pub proof fn lemma_step_wf(pre: TurnView, ev: Event)
    requires
        wf(pre),
        next(pre, ev) is Some,
    ensures
        wf(next(pre, ev)->0),
{
    let post = next(pre, ev)->0;
    match ev {
        Event::CallComplete { id } => {
            assert forall|a: int, b: int| 0 <= a < b < post.calls.len() implies post.calls[a]
                != post.calls[b] by {
                if b == post.calls.len() - 1 {
                    assert(pre.calls[a] == post.calls[a]);
                    assert(!pre.calls.contains(id));
                } else {
                    assert(pre.calls[a] == post.calls[a] && pre.calls[b] == post.calls[b]);
                }
            }
            assert forall|i: int|
                0 <= i < post.request_start implies #[trigger] post.answered[i] by {
                assert(pre.answered[i] == post.answered[i]);
            }
        },
        Event::ResponseDone { may_continue } => {
            if may_continue && pre.calls.len() == pre.request_start {
                assert forall|i: int|
                    0 <= i < post.answered.len() implies #[trigger] post.answered[i] by {
                    assert(i < pre.request_start);
                }
            }
        },
        Event::Result { id } => {
            let i = choose|i: int|
                0 <= i < pre.calls.len() && pre.calls[i] == id && !pre.answered[i];
            assert forall|j: int|
                0 <= j < post.request_start implies #[trigger] post.answered[j] by {
                if j != i {
                    assert(post.answered[j] == pre.answered[j]);
                }
            }
        },
        _ => {},
    }
}

/// The next model request is never sent while a dispatched call lacks its result.
pub proof fn lemma_no_request_with_outstanding_calls(pre: TurnView)
    requires
        wf(pre),
        next(pre, Event::NextRequest) is Some,
    ensures
        all_answered(pre),
{
}

/// A result is accepted only for a dispatched call that had none, and only that call changes.
pub proof fn lemma_result_answers_exactly_one_call(pre: TurnView, id: CallId)
    requires
        wf(pre),
        next(pre, Event::Result { id }) is Some,
    ensures
        exists|i: int|
            0 <= i < pre.calls.len() && pre.calls[i] == id && !pre.answered[i] && (#[trigger] next(
                pre,
                Event::Result { id },
            )->0).answered == pre.answered.update(i, true),
{
}

/// Cancellation never answers a call by itself: the flags are untouched, so every outstanding call
/// must still receive a `Result` event before the turn can settle.
pub proof fn lemma_cancel_answers_nothing(pre: TurnView)
    requires
        wf(pre),
        next(pre, Event::Cancel) is Some,
    ensures
        next(pre, Event::Cancel)->0.answered == pre.answered,
{
}

/// However a turn ends, every dispatched call has its result and every steer was either
/// delivered with a request or returned to the caller — never lost.
pub proof fn lemma_settled_means_all_answered_and_no_lost_steer(pre: TurnView, ev: Event)
    requires
        wf(pre),
        next(pre, ev) is Some,
        next(pre, ev)->0.phase is Settled,
    ensures
        all_answered(next(pre, ev)->0),
        next(pre, ev)->0.delivered + next(pre, ev)->0.returned == next(pre, ev)->0.steers,
{
    lemma_step_wf(pre, ev);
}

// ---------------------------------------------------------------------------------------------
// Executable implementation, proven to refine `next`.
// ---------------------------------------------------------------------------------------------
/// A turn in progress. Fields are private; the type invariant is [`wf`].
pub struct Turn {
    phase: Phase,
    calls: Vec<CallId>,
    answered: Vec<bool>,
    request_start: usize,
    steers: u64,
    queued: u64,
    delivered: u64,
    returned: u64,
}

impl View for Turn {
    type V = TurnView;

    closed spec fn view(&self) -> TurnView {
        TurnView {
            phase: self.phase,
            calls: self.calls@,
            answered: self.answered@,
            request_start: self.request_start as nat,
            steers: self.steers as nat,
            queued: self.queued as nat,
            delivered: self.delivered as nat,
            returned: self.returned as nat,
        }
    }
}

impl Default for Turn {
    fn default() -> Self {
        Self::new()
    }
}

/// `flags` with position `i` set.
fn with_set(flags: &[bool], i: usize) -> (out: Vec<bool>)
    requires
        i < flags@.len(),
    ensures
        out@ == flags@.update(i as int, true),
{
    let mut out: Vec<bool> = Vec::with_capacity(flags.len());
    let n = flags.len();
    for k in 0..n
        invariant
            n == flags@.len(),
            i < n,
            out@.len() == k,
            forall|j: int| 0 <= j < k ==> #[trigger] out@[j] == flags@.update(i as int, true)[j],
    {
        out.push(
            if k == i {
                true
            } else {
                flags[k]
            },
        );
    }
    proof {
        assert(out@ =~= flags@.update(i as int, true));
    }
    out
}

/// `v` with `x` appended (a fresh vector: vstd's `Vec::push` may unwind, which a field of a
/// type-invariant struct must not do).
fn pushed<T: Copy>(v: &[T], x: T) -> (out: Vec<T>)
    ensures
        out@ == v@.push(x),
{
    let mut out: Vec<T> = Vec::new();
    let n = v.len();
    for k in 0..n
        invariant
            n == v@.len(),
            out@.len() == k,
            forall|j: int| 0 <= j < k ==> #[trigger] out@[j] == v@[j],
    {
        out.push(v[k]);
    }
    out.push(x);
    proof {
        assert(out@ =~= v@.push(x));
    }
    out
}

/// A copy of `v` (fields of a type-invariant struct are replaced whole, never mutated in place).
fn copied<T: Copy>(v: &[T]) -> (out: Vec<T>)
    ensures
        out@ == v@,
{
    let mut out: Vec<T> = Vec::with_capacity(v.len());
    let n = v.len();
    for k in 0..n
        invariant
            n == v@.len(),
            out@.len() == k,
            forall|j: int| 0 <= j < k ==> #[trigger] out@[j] == v@[j],
    {
        out.push(v[k]);
    }
    proof {
        assert(out@ =~= v@);
    }
    out
}

/// Whether every flag is set.
fn all_true(flags: &[bool]) -> (b: bool)
    ensures
        b == forall|i: int| 0 <= i < flags@.len() ==> #[trigger] flags@[i],
{
    let n = flags.len();
    for k in 0..n
        invariant
            n == flags@.len(),
            forall|i: int| 0 <= i < k ==> #[trigger] flags@[i],
    {
        if !flags[k] {
            proof {
                assert(!flags@[k as int]);
            }
            return false;
        }
    }
    true
}

const fn is_streaming(p: Phase) -> (b: bool)
    ensures
        b == (p is Streaming),
{
    matches!(p, Phase::Streaming)
}

const fn is_awaiting(p: Phase) -> (b: bool)
    ensures
        b == (p is AwaitingResults),
{
    matches!(p, Phase::AwaitingResults)
}

const fn is_ready(p: Phase) -> (b: bool)
    ensures
        b == (p is Ready),
{
    matches!(p, Phase::Ready)
}

const fn is_cancelling(p: Phase) -> (b: bool)
    ensures
        b == (p is Cancelling),
{
    matches!(p, Phase::Cancelling)
}

/// `id` is a dispatched call of `v` that has no result yet.
pub open spec fn index_of_unanswered(v: TurnView, id: CallId) -> bool {
    exists|i: int| 0 <= i < v.calls.len() && v.calls[i] == id && !v.answered[i]
}

impl Turn {
    #[verifier::type_invariant]
    spec fn inv(self) -> bool {
        wf(self@)
    }

    /// A new turn whose first model request is streaming.
    #[must_use]
    pub const fn new() -> (t: Self)
        ensures
            t@ == start(),
    {
        let t = Self {
            phase: Phase::Streaming,
            calls: Vec::new(),
            answered: Vec::new(),
            request_start: 0,
            steers: 0,
            queued: 0,
            delivered: 0,
            returned: 0,
        };
        proof {
            assert(t@.calls =~= Seq::<CallId>::empty());
            assert(t@.answered =~= Seq::<bool>::empty());
        }
        t
    }

    /// The current phase.
    #[must_use]
    pub const fn phase(&self) -> (p: Phase)
        ensures
            p == self@.phase,
    {
        self.phase
    }

    /// Steers waiting for the next request.
    #[must_use]
    pub const fn queued_steers(&self) -> (n: u64)
        ensures
            n == self@.queued,
    {
        self.queued
    }

    /// Steers handed back to the caller so far.
    #[must_use]
    pub const fn returned_steers(&self) -> (n: u64)
        ensures
            n == self@.returned,
    {
        self.returned
    }

    /// Ids of dispatched calls still waiting for their result, in dispatch order.
    #[must_use]
    pub fn outstanding(&self) -> (out: Vec<CallId>)
        ensures
            forall|j: int| 0 <= j < out@.len() ==> #[trigger] index_of_unanswered(self@, out@[j]),
            forall|i: int|
                0 <= i < self@.calls.len() && !self@.answered[i] ==> out@.contains(
                    #[trigger] self@.calls[i],
                ),
    {
        proof {
            use_type_invariant(self);
        }
        let mut out: Vec<CallId> = Vec::new();
        let ghost mut idx: Seq<int> = Seq::empty();
        let n = self.calls.len();
        for k in 0..n
            invariant
                n == self@.calls.len(),
                n == self@.answered.len(),
                idx.len() == out@.len(),
                forall|j: int|
                    0 <= j < out@.len() ==> 0 <= #[trigger] idx[j] < n && self@.calls[idx[j]]
                        == out@[j] && !self@.answered[idx[j]],
                forall|i: int|
                    0 <= i < k && !self@.answered[i] ==> out@.contains(#[trigger] self@.calls[i]),
        {
            if !self.answered[k] {
                let ghost before = out@;
                out.push(self.calls[k]);
                proof {
                    idx = idx.push(k as int);
                    let last = out@.len() - 1;
                    assert(out@[last] == self@.calls[k as int]);
                    assert forall|j: int| 0 <= j < out@.len() implies 0 <= #[trigger] idx[j] < n
                        && self@.calls[idx[j]] == out@[j] && !self@.answered[idx[j]] by {
                        if j < last {
                            assert(out@[j] == before[j]);
                        }
                    }
                    assert forall|i: int|
                        0 <= i < k + 1 && !self@.answered[i] implies out@.contains(
                        #[trigger] self@.calls[i],
                    ) by {
                        if i == k {
                            assert(out@[last] == self@.calls[i]);
                        } else {
                            assert(before.contains(self@.calls[i]));
                            let w = choose|w: int|
                                0 <= w < before.len() && before[w] == self@.calls[i];
                            assert(out@[w] == before[w]);
                        }
                    }
                }
            }
        }
        proof {
            assert forall|j: int| 0 <= j < out@.len() implies #[trigger] index_of_unanswered(
                self@,
                out@[j],
            ) by {
                let i = idx[j];
                assert(0 <= i < self@.calls.len() && self@.calls[i] == out@[j]
                    && !self@.answered[i]);
            }
        }
        out
    }

    fn position(&self, id: CallId) -> (r: Option<usize>)
        requires
            self@.answered.len() == self@.calls.len(),
        ensures
            r is None ==> !self@.calls.contains(id),
            r matches Some(i) ==> i < self@.calls.len() && self@.calls[i as int] == id,
    {
        let n = self.calls.len();
        for k in 0..n
            invariant
                n == self@.calls.len(),
                forall|i: int| 0 <= i < k ==> #[trigger] self@.calls[i] != id,
        {
            if self.calls[k] == id {
                return Some(k);
            }
        }
        proof {
            if self@.calls.contains(id) {
                let w = choose|w: int| 0 <= w < self@.calls.len() && self@.calls[w] == id;
                assert(self@.calls[w] != id);
            }
        }
        None
    }

    /// The only mutation entry point. Total: no `requires`, so unverified callers cannot break it.
    /// (Counters are `u64`; an event that would overflow one — 2^64 steers — is rejected.)
    ///
    /// # Errors
    /// The event is rejected (and the turn left unchanged) exactly when [`next`] returns `None`
    /// or a counter would overflow.
    #[expect(clippy::too_many_lines, reason = "one arm per event, mirroring `next` arm by arm")]
    pub fn apply(&mut self, ev: Event) -> (r: Result<(), TurnError>)
        ensures
            r is Ok ==> next(old(self)@, ev) == Some(final(self)@),
            r is Err ==> final(self)@ == old(self)@,
            next(old(self)@, ev) is None ==> r is Err,
    {
        proof {
            use_type_invariant(&*self);
        }
        let ghost pre = self@;
        match ev {
            Event::CallComplete { id } => {
                if !is_streaming(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                if self.position(id).is_some() {
                    return Err(TurnError::DuplicateCall);
                }
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self =
                Self {
                    calls: pushed(&self.calls, id),
                    answered: pushed(&self.answered, false),
                    phase: self.phase,
                    request_start: self.request_start,
                    steers: self.steers,
                    queued: self.queued,
                    delivered: self.delivered,
                    returned: self.returned,
                };
                Ok(())
            },
            Event::ResponseDone { may_continue } => {
                if !is_streaming(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                let all = all_true(&self.answered);
                if !may_continue {
                    if self.returned > u64::MAX - self.queued {
                        return Err(TurnError::WrongPhase);
                    }
                    proof {
                        lemma_step_wf(pre, ev);
                    }
                    *self =
                    Self {
                        phase: if all {
                            Phase::Settled
                        } else {
                            Phase::Cancelling
                        },
                        calls: copied(&self.calls),
                        answered: copied(&self.answered),
                        request_start: self.request_start,
                        steers: self.steers,
                        queued: 0,
                        delivered: self.delivered,
                        returned: self.returned + self.queued,
                    };
                    return Ok(());
                }
                let phase = if self.calls.len() == self.request_start {
                    if self.queued > 0 {
                        Phase::Ready
                    } else {
                        Phase::Settled
                    }
                } else if all {
                    Phase::Ready
                } else {
                    Phase::AwaitingResults
                };
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self =
                Self {
                    phase,
                    calls: copied(&self.calls),
                    answered: copied(&self.answered),
                    request_start: self.request_start,
                    steers: self.steers,
                    queued: self.queued,
                    delivered: self.delivered,
                    returned: self.returned,
                };
                Ok(())
            },
            Event::Result { id } => {
                if !is_streaming(self.phase) && !is_awaiting(self.phase) && !is_cancelling(
                    self.phase,
                ) {
                    return Err(TurnError::WrongPhase);
                }
                let Some(i) = self.position(id) else {
                    return Err(TurnError::UnknownOrAnswered);
                };
                proof {
                    assert forall|j: int| 0 <= j < pre.calls.len() && pre.calls[j] == id implies j
                        == i as int by {
                        if j != i as int {
                            assert(pre.calls[j] != pre.calls[i as int]);
                        }
                    }
                }
                if self.answered[i] {
                    return Err(TurnError::UnknownOrAnswered);
                }
                let answered = with_set(&self.answered, i);
                let done = all_true(&answered);
                let phase = if is_streaming(self.phase) {
                    Phase::Streaming
                } else if done {
                    if is_cancelling(self.phase) {
                        Phase::Settled
                    } else {
                        Phase::Ready
                    }
                } else {
                    self.phase
                };
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self =
                Self {
                    phase,
                    calls: copied(&self.calls),
                    answered,
                    request_start: self.request_start,
                    steers: self.steers,
                    queued: self.queued,
                    delivered: self.delivered,
                    returned: self.returned,
                };
                Ok(())
            },
            Event::NextRequest => {
                if !is_ready(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                if self.delivered > u64::MAX - self.queued {
                    return Err(TurnError::WrongPhase);
                }
                proof {
                    lemma_step_wf(pre, ev);
                }
                let calls = copied(&self.calls);
                let request_start = calls.len();
                *self =
                Self {
                    phase: Phase::Streaming,
                    calls,
                    answered: copied(&self.answered),
                    request_start,
                    steers: self.steers,
                    queued: 0,
                    delivered: self.delivered + self.queued,
                    returned: self.returned,
                };
                Ok(())
            },
            Event::Steer => {
                if !is_streaming(self.phase) && !is_awaiting(self.phase) && !is_ready(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                if self.steers == u64::MAX {
                    return Err(TurnError::WrongPhase);
                }
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self =
                Self {
                    phase: self.phase,
                    calls: copied(&self.calls),
                    answered: copied(&self.answered),
                    request_start: self.request_start,
                    steers: self.steers + 1,
                    queued: self.queued + 1,
                    delivered: self.delivered,
                    returned: self.returned,
                };
                Ok(())
            },
            Event::Cancel => {
                if !is_streaming(self.phase) && !is_awaiting(self.phase) && !is_ready(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                if self.returned > u64::MAX - self.queued {
                    return Err(TurnError::WrongPhase);
                }
                let all = all_true(&self.answered);
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self =
                Self {
                    phase: if all {
                        Phase::Settled
                    } else {
                        Phase::Cancelling
                    },
                    calls: copied(&self.calls),
                    answered: copied(&self.answered),
                    request_start: self.request_start,
                    steers: self.steers,
                    queued: 0,
                    delivered: self.delivered,
                    returned: self.returned + self.queued,
                };
                Ok(())
            },
        }
    }
}

} // verus!
