//! The native agent loop's turn state machine (docs/architecture.md §6.2).
//!
//! Status: DRAFT(M2a) decision. It becomes LOCKED with its ADR once the native loop has run
//! against every provider.
//!
//! One *turn* is everything between a user input and the model's final answer: possibly several
//! model requests, each of which may ask for tool calls. The agent loop feeds this machine what
//! happened and does only what it allows. The decision, stated as [`next`]:
//!
//! - a tool call is *dispatched* only when the provider delivered it complete, and at most once
//!   per call id;
//! - a result is accepted only for a dispatched call that has no result yet;
//! - the next model request is sent only when every dispatched call has its result;
//! - a response without new calls is the final answer and settles the turn;
//! - cancellation settles the turn, and the loop answers every outstanding call with a cancelled
//!   result — so the transcript never holds a call without its result;
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
    /// The turn is over (final answer or cancellation). Absorbs every event.
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
    ResponseDone,
    /// A dispatched call produced its result.
    Result {
        /// The call.
        id: CallId,
    },
    /// The loop starts the next model request.
    NextRequest,
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
}

/// Every dispatched call has its result.
pub open spec fn all_answered(v: TurnView) -> bool {
    forall|i: int| 0 <= i < v.answered.len() ==> #[trigger] v.answered[i]
}

/// Well-formed: one flag per call, no call dispatched twice, calls of earlier requests all
/// answered, and `Ready`/`Settled` only when every call is answered.
pub open spec fn wf(v: TurnView) -> bool {
    &&& v.answered.len() == v.calls.len()
    &&& v.calls.no_duplicates()
    &&& v.request_start <= v.calls.len()
    &&& forall|i: int| 0 <= i < v.request_start ==> #[trigger] v.answered[i]
    &&& (v.phase is Ready || v.phase is Settled) ==> all_answered(v)
}

/// A new turn: the first model request is streaming and nothing is dispatched.
pub open spec fn start() -> TurnView {
    TurnView {
        phase: Phase::Streaming,
        calls: Seq::empty(),
        answered: Seq::empty(),
        request_start: 0,
    }
}

/// `id` is a dispatched call of `v` that has no result yet.
pub open spec fn index_of_unanswered(v: TurnView, id: CallId) -> bool {
    exists|i: int| 0 <= i < v.calls.len() && v.calls[i] == id && !v.answered[i]
}

/// `v` in phase `p`.
pub open spec fn in_phase(v: TurnView, p: Phase) -> TurnView {
    TurnView { phase: p, calls: v.calls, answered: v.answered, request_start: v.request_start }
}

/// Phase once a response is done or a result arrives outside streaming: wait while anything is
/// outstanding, else ready for the next request.
pub open spec fn after_progress(v: TurnView) -> Phase {
    if all_answered(v) {
        Phase::Ready
    } else {
        Phase::AwaitingResults
    }
}

/// DRAFT(M2a) decision: the complete transition function. `None` = rejected, turn unchanged.
/// On `Cancel`, the loop must answer every call that was not yet answered with a cancelled result.
pub open spec fn next(pre: TurnView, ev: Event) -> Option<TurnView> {
    if pre.phase is Settled {
        None
    } else {
        match ev {
            Event::CallComplete { id } => if pre.phase is Streaming && !pre.calls.contains(id) {
                Some(
                    TurnView {
                        phase: Phase::Streaming,
                        calls: pre.calls.push(id),
                        answered: pre.answered.push(false),
                        request_start: pre.request_start,
                    },
                )
            } else {
                None
            },
            Event::ResponseDone => if !(pre.phase is Streaming) {
                None
            } else if pre.calls.len() == pre.request_start {
                Some(in_phase(pre, Phase::Settled))
            } else {
                Some(in_phase(pre, after_progress(pre)))
            },
            Event::Result { id } => if (pre.phase is Streaming || pre.phase is AwaitingResults)
                && exists|i: int|
                0 <= i < pre.calls.len() && pre.calls[i] == id && !pre.answered[i] {
                let i = choose|i: int|
                    0 <= i < pre.calls.len() && pre.calls[i] == id && !pre.answered[i];
                let post = TurnView {
                    phase: pre.phase,
                    calls: pre.calls,
                    answered: pre.answered.update(i, true),
                    request_start: pre.request_start,
                };
                if pre.phase is Streaming {
                    Some(post)
                } else {
                    Some(in_phase(post, after_progress(post)))
                }
            } else {
                None
            },
            Event::NextRequest => if pre.phase is Ready {
                Some(
                    TurnView {
                        phase: Phase::Streaming,
                        calls: pre.calls,
                        answered: pre.answered,
                        request_start: pre.calls.len(),
                    },
                )
            } else {
                None
            },
            Event::Cancel => Some(
                TurnView {
                    phase: Phase::Settled,
                    calls: pre.calls,
                    answered: Seq::new(pre.calls.len(), |i: int| true),
                    request_start: pre.request_start,
                },
            ),
        }
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

/// Every accepted event keeps the turn well-formed — so no call is ever dispatched twice, and
/// `Ready`/`Settled` always mean every call has its result.
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
        Event::ResponseDone => {
            if pre.calls.len() == pre.request_start {
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
        Event::NextRequest => {},
        Event::Cancel => {},
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

/// However a turn ends, every dispatched call has its result.
pub proof fn lemma_settled_means_all_answered(pre: TurnView, ev: Event)
    requires
        wf(pre),
        next(pre, ev) is Some,
        next(pre, ev)->0.phase is Settled,
    ensures
        all_answered(next(pre, ev)->0),
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
}

impl View for Turn {
    type V = TurnView;

    closed spec fn view(&self) -> TurnView {
        TurnView {
            phase: self.phase,
            calls: self.calls@,
            answered: self.answered@,
            request_start: self.request_start as nat,
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

const fn is_settled(p: Phase) -> (b: bool)
    ensures
        b == (p is Settled),
{
    matches!(p, Phase::Settled)
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

/// A copy of `v` (fields of a type-invariant struct are replaced whole, never mutated in place).
fn pushed_none<T: Copy>(v: &[T]) -> (out: Vec<T>)
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

/// `n` set flags.
fn all_set(n: usize) -> (out: Vec<bool>)
    ensures
        out@ == Seq::new(n as nat, |i: int| true),
{
    let mut out: Vec<bool> = Vec::with_capacity(n);
    for _k in 0..n
        invariant
            out@.len() == _k,
            forall|j: int| 0 <= j < _k ==> #[trigger] out@[j],
    {
        out.push(true);
    }
    proof {
        assert(out@ =~= Seq::new(n as nat, |i: int| true));
    }
    out
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
        // Witness indices: `out[j] == calls[idx[j]]` with that call unanswered.
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

    fn all_answered_exec(&self) -> (b: bool)
        requires
            self@.answered.len() == self@.calls.len(),
        ensures
            b == all_answered(self@),
    {
        let n = self.answered.len();
        for k in 0..n
            invariant
                n == self@.answered.len(),
                forall|i: int| 0 <= i < k ==> #[trigger] self@.answered[i],
        {
            if !self.answered[k] {
                proof {
                    assert(!self@.answered[k as int]);
                }
                return false;
            }
        }
        true
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
    ///
    /// # Errors
    /// The event is rejected (and the turn left unchanged) exactly when [`next`] returns `None`.
    #[expect(clippy::too_many_lines, reason = "one arm per event, mirroring `next` arm by arm")]
    pub fn apply(&mut self, ev: Event) -> (r: Result<(), TurnError>)
        ensures
            match next(old(self)@, ev) {
                Some(post) => r is Ok && final(self)@ == post,
                None => r is Err && final(self)@ == old(self)@,
            },
    {
        proof {
            use_type_invariant(&*self);
        }
        let ghost pre = self@;
        if is_settled(self.phase) {
            return Err(TurnError::Settled);
        }
        match ev {
            Event::CallComplete { id } => {
                if !is_streaming(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                if self.position(id).is_some() {
                    return Err(TurnError::DuplicateCall);
                }
                let calls = pushed(&self.calls, id);
                let answered = pushed(&self.answered, false);
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self =
                Self {
                    phase: Phase::Streaming,
                    calls,
                    answered,
                    request_start: self.request_start,
                };
                Ok(())
            },
            Event::ResponseDone => {
                if !is_streaming(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                let phase = if self.calls.len() == self.request_start {
                    Phase::Settled
                } else if self.all_answered_exec() {
                    Phase::Ready
                } else {
                    Phase::AwaitingResults
                };
                proof {
                    lemma_step_wf(pre, ev);
                }
                let calls = pushed_none(&self.calls);
                let answered = pushed_none(&self.answered);
                *self = Self { phase, calls, answered, request_start: self.request_start };
                Ok(())
            },
            Event::Result { id } => {
                if !is_streaming(self.phase) && !is_awaiting(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                let Some(i) = self.position(id) else {
                    return Err(TurnError::UnknownOrAnswered);
                };
                proof {
                    // Call ids are unique, so `i` is the only index holding `id`.
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
                let calls = pushed_none(&self.calls);
                let ghost post = TurnView {
                    phase: pre.phase,
                    calls: pre.calls,
                    answered: pre.answered.update(i as int, true),
                    request_start: pre.request_start,
                };
                let phase = if is_awaiting(self.phase) {
                    if all_true(&answered) {
                        Phase::Ready
                    } else {
                        Phase::AwaitingResults
                    }
                } else {
                    Phase::Streaming
                };
                proof {
                    lemma_step_wf(pre, ev);
                }
                *self = Self { phase, calls, answered, request_start: self.request_start };
                Ok(())
            },
            Event::NextRequest => {
                if !is_ready(self.phase) {
                    return Err(TurnError::WrongPhase);
                }
                proof {
                    lemma_step_wf(pre, ev);
                }
                let calls = pushed_none(&self.calls);
                let answered = pushed_none(&self.answered);
                let request_start = calls.len();
                *self = Self { phase: Phase::Streaming, calls, answered, request_start };
                Ok(())
            },
            Event::Cancel => {
                proof {
                    lemma_step_wf(pre, ev);
                }
                let calls = pushed_none(&self.calls);
                let answered = all_set(self.calls.len());
                *self =
                Self { phase: Phase::Settled, calls, answered, request_start: self.request_start };
                Ok(())
            },
        }
    }
}

} // verus!
