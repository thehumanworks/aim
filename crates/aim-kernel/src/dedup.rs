//! Per-key mutation replay decisions, with admission refusal distinct from execution (ADR 0050).
//!
//! The shell owns keys, keyed tombstone hashes, recorded results, and clocks. This model receives
//! the observed phase and a parsed `UUIDv7` mint time; it cannot parse key bytes itself.
use vstd::prelude::*;

verus! {

/// A key's retention phase, projected from the shell's records and tombstones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// No remembered reservation or result.
    Absent,
    /// A request reserved the key but has not reported an attempt outcome.
    InFlight,
    /// An attempted mutation has a recorded outcome.
    Done,
    /// The result was evicted, but the key remains remembered.
    Tombstone,
}

/// How the shell must answer one begin request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BeginDecision {
    /// Reserve the key and execute once.
    Execute,
    /// Wait for the in-flight attempt.
    InFlight,
    /// Replay the recorded outcome.
    Replay,
    /// Return `unknown_outcome`; no execution.
    UnknownOutcome,
    /// The same key names different parameters.
    Mismatch,
    /// Key retention capacity is full.
    Full,
    /// Concurrent attempt capacity is full.
    Busy,
}

/// LOCKED(ADR-0055): a timestamped key is stale once its mint time plus the horizon is no
/// later than now. Untimestamped keys cannot be classified stale after their tombstone expires.
pub open spec fn minted_too_old_spec(minted: Option<u64>, now: u64, horizon_ms: u64) -> bool {
    match minted {
        Some(time) => (time as nat) + (horizon_ms as nat) <= now as nat,
        None => false,
    }
}

/// Classifies a shell-parsed `UUIDv7` mint time without overflowing the clock arithmetic.
#[must_use]
pub fn minted_too_old(minted: Option<u64>, now: u64, horizon_ms: u64) -> (stale: bool)
    ensures
        stale == minted_too_old_spec(minted, now, horizon_ms),
{
    match minted {
        Some(time) => time <= now && horizon_ms <= now - time,
        None => false,
    }
}

/// LOCKED(ADR-0050): a retained attempt replays or waits; an evicted or stale absent key returns
/// unknown outcome; only an unstale, unremembered key with free capacity may execute.
pub open spec fn begin_decision(
    phase: Phase,
    same_request: bool,
    stale: bool,
    full: bool,
    busy: bool,
) -> BeginDecision {
    match phase {
        Phase::InFlight => if same_request {
            BeginDecision::InFlight
        } else {
            BeginDecision::Mismatch
        },
        Phase::Done => if same_request {
            BeginDecision::Replay
        } else {
            BeginDecision::Mismatch
        },
        Phase::Tombstone => BeginDecision::UnknownOutcome,
        Phase::Absent => if stale {
            BeginDecision::UnknownOutcome
        } else if full {
            BeginDecision::Full
        } else if busy {
            BeginDecision::Busy
        } else {
            BeginDecision::Execute
        },
    }
}

/// One total begin decision from the observed table phase and capacity flags.
#[must_use]
#[expect(clippy::fn_params_excessive_bools, reason = "the four independent table observations mirror the locked begin spec")]
pub fn decide_begin(
    phase: Phase,
    same_request: bool,
    stale: bool,
    full: bool,
    busy: bool,
) -> (decision: BeginDecision)
    ensures
        decision == begin_decision(phase, same_request, stale, full, busy),
{
    match phase {
        Phase::InFlight => if same_request {
            BeginDecision::InFlight
        } else {
            BeginDecision::Mismatch
        },
        Phase::Done => if same_request {
            BeginDecision::Replay
        } else {
            BeginDecision::Mismatch
        },
        Phase::Tombstone => BeginDecision::UnknownOutcome,
        Phase::Absent => if stale {
            BeginDecision::UnknownOutcome
        } else if full {
            BeginDecision::Full
        } else if busy {
            BeginDecision::Busy
        } else {
            BeginDecision::Execute
        },
    }
}

/// Every execution decision is for an absent key whose mint time is still within its horizon.
pub proof fn theorem_execute_requires_fresh_absent(
    phase: Phase,
    same_request: bool,
    stale: bool,
    full: bool,
    busy: bool,
)
    ensures
        begin_decision(phase, same_request, stale, full, busy) == BeginDecision::Execute ==> phase
            == Phase::Absent && !stale,
{
}

/// LOCKED(ADR-0050): a pre-execution refusal forgets its reservation, while any attempted
/// mutation, including a failed one, retains a replayable outcome.
pub open spec fn finish_phase(phase: Phase, attempted: bool) -> Option<Phase> {
    match phase {
        Phase::InFlight => Some(
            if attempted {
                Phase::Done
            } else {
                Phase::Absent
            },
        ),
        _ => None,
    }
}

/// The state after the shell reports an attempt or a pre-execution admission refusal.
#[must_use]
pub fn decide_finish(phase: Phase, attempted: bool) -> (next: Option<Phase>)
    ensures
        next == finish_phase(phase, attempted),
{
    match phase {
        Phase::InFlight => Some(
            if attempted {
                Phase::Done
            } else {
                Phase::Absent
            },
        ),
        _ => None,
    }
}

/// LOCKED(ADR-0050): eviction drops the result but retains a tombstone until the horizon;
/// only horizon expiry may forget that tombstone.
pub open spec fn eviction_phase(phase: Phase, horizon_elapsed: bool) -> Phase {
    match phase {
        Phase::Done | Phase::Tombstone => if horizon_elapsed {
            Phase::Absent
        } else {
            Phase::Tombstone
        },
        _ => phase,
    }
}

/// Decides whether an evicted key stays as a tombstone.
#[must_use]
pub fn decide_evict(phase: Phase, horizon_elapsed: bool) -> (next: Phase)
    ensures
        next == eviction_phase(phase, horizon_elapsed),
{
    match phase {
        Phase::Done | Phase::Tombstone => if horizon_elapsed {
            Phase::Absent
        } else {
            Phase::Tombstone
        },
        _ => phase,
    }
}

/// Events in a one-key execution trace. `HorizonElapsed` is excluded by the within-horizon theorem.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceEvent {
    /// One retry or first request.
    Begin {
        /// The parsed key's mint time is beyond the retention horizon.
        stale: bool,
        /// Retained-key capacity is full.
        full: bool,
        /// In-flight capacity is full.
        busy: bool,
    },
    /// Refusal before an external operation started.
    AdmissionRefused,
    /// The operation was attempted, whether it succeeded or failed.
    Attempted,
    /// Drop the result but retain the key.
    Evict,
    /// Forget a tombstone after the key's retention horizon.
    HorizonElapsed,
}

/// One key's phase and number of attempted operations in a trace.
pub struct TraceState {
    /// Remembered phase.
    pub phase: Phase,
    /// Attempt count used only in the proof, never stored by the shell.
    pub attempts: nat,
}

/// LOCKED(ADR-0050): an admission refusal can reopen Absent; an attempted result cannot reopen
/// a key before its horizon. `Begin` against Done/Tombstone leaves the phase unchanged.
pub open spec fn dedup_next(pre: TraceState, event: TraceEvent) -> Option<TraceState> {
    match event {
        TraceEvent::Begin { stale, full, busy } => if pre.phase == Phase::Absent && begin_decision(
            pre.phase,
            true,
            stale,
            full,
            busy,
        ) == BeginDecision::Execute {
            Some(TraceState { phase: Phase::InFlight, ..pre })
        } else {
            Some(pre)
        },
        TraceEvent::AdmissionRefused => if pre.phase == Phase::InFlight {
            Some(TraceState { phase: Phase::Absent, ..pre })
        } else {
            None
        },
        TraceEvent::Attempted => if pre.phase == Phase::InFlight {
            Some(TraceState { phase: Phase::Done, attempts: pre.attempts + 1 })
        } else {
            None
        },
        TraceEvent::Evict => if pre.phase == Phase::Done {
            Some(TraceState { phase: Phase::Tombstone, ..pre })
        } else {
            None
        },
        TraceEvent::HorizonElapsed => if pre.phase == Phase::Tombstone {
            Some(TraceState { phase: Phase::Absent, ..pre })
        } else {
            None
        },
    }
}

/// Within a key's horizon, any attempted result is held as Done or Tombstone.
pub open spec fn trace_wf(v: TraceState) -> bool {
    v.attempts <= 1 && (v.attempts == 1 ==> (v.phase == Phase::Done || v.phase == Phase::Tombstone))
}

/// One accepted non-expiry step preserves the at-most-once invariant.
pub proof fn lemma_step_at_most_once(pre: TraceState, event: TraceEvent)
    requires
        trace_wf(pre),
        dedup_next(pre, event) is Some,
        !(event is HorizonElapsed),
    ensures
        trace_wf(dedup_next(pre, event)->0),
{
}

/// A sequence starts with no execution and follows accepted transitions.
pub open spec fn dedup_valid_trace(states: Seq<TraceState>, events: Seq<TraceEvent>) -> bool {
    states.len() == events.len() + 1 && states[0].phase == Phase::Absent && states[0].attempts == 0
        && (forall|i: int|
        0 <= i < events.len() ==> dedup_next(states[i], events[i]) == Some(states[i + 1])) && (
    forall|i: int| 0 <= i < events.len() ==> !(events[i] is HorizonElapsed))
}

proof fn lemma_trace_prefix(states: Seq<TraceState>, events: Seq<TraceEvent>, n: nat)
    requires
        dedup_valid_trace(states, events),
        n <= events.len(),
    ensures
        trace_wf(states[n as int]),
    decreases n,
{
    if n > 0 {
        lemma_trace_prefix(states, events, (n - 1) as nat);
        lemma_step_at_most_once(states[(n - 1) as int], events[(n - 1) as int]);
    }
}

/// An attempted mutation executes at most once before the key's horizon, across any valid
/// begin/refusal/finish/retry/eviction sequence.
pub proof fn theorem_never_executes_twice_within_horizon(
    states: Seq<TraceState>,
    events: Seq<TraceEvent>,
)
    requires
        dedup_valid_trace(states, events),
    ensures
        states[events.len() as int].attempts <= 1,
{
    lemma_trace_prefix(states, events, events.len());
}

/// A refused reservation may be retried, but a retained attempted result may not execute.
pub proof fn theorem_refusal_and_retention()
    ensures
        finish_phase(Phase::InFlight, false) == Some(Phase::Absent),
        begin_decision(Phase::Absent, true, false, false, false) == BeginDecision::Execute,
        finish_phase(Phase::InFlight, true) == Some(Phase::Done),
        begin_decision(Phase::Done, true, false, false, false) == BeginDecision::Replay,
        begin_decision(Phase::Tombstone, true, false, false, false)
            == BeginDecision::UnknownOutcome,
        begin_decision(Phase::Absent, true, true, false, false) == BeginDecision::UnknownOutcome,
{
}

} // verus!
