//! Pure crash-recovery decisions for board integration (ADR 0068).
//!
//! OIDs and scratch paths are exact byte strings supplied by the Git/ledger shell. The shell
//! verifies their syntax, owns checkout coordination, and performs durable writes and Git effects.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// Durable integration phase.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Accepted work is waiting for the integrator.
    Queued,
    /// The pinned intent is durable and external Git work may be in progress.
    Integrating,
    /// The result is the observed target head.
    Integrated,
    /// Integration stopped; a saved result may still need explicit application.
    Failed,
    /// A merge conflict needs human resolution.
    Conflict,
}

/// One recovery or forward-progress instruction to the Git/ledger shell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// No transition is needed.
    None,
    /// Persist a selected pinned intent before creating scratch or moving any ref.
    Begin,
    /// Clear an unfinished intent and queue it again while all refs remain pinned.
    RetryQueued,
    /// Inspect the recorded, owned scratch worktree and continue its merge/check.
    ResumeScratch,
    /// Pin the persisted result under the owned rescue ref before cleanup.
    EnsureRescue,
    /// Try switching the owned scratch checkout to the target branch.
    AcquireOwnedCheckout,
    /// Attempt a checkout-aware fast-forward under the integrator's ownership.
    AdvanceOwned,
    /// Record that the target already points to the persisted result.
    FinalizeIntegrated,
    /// Record a terminal failed outcome.
    FinalizeFailed,
    /// Record a terminal merge conflict.
    FinalizeConflict,
    /// The user explicitly applied a saved result; replace Failed with Integrated.
    MarkApplied,
}

/// A detected merge/check failure that the shell has already recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// No failure has been recorded.
    None,
    /// Git reported unresolved merge paths.
    Conflict,
    /// A check or Git operation failed without a merge conflict.
    Failed,
}

/// Abstract persisted intent, including exact pinned OIDs and scratch path.
pub struct IntentView {
    /// Durable phase.
    pub phase: Phase,
    /// Pinned target OID.
    pub target: Seq<u8>,
    /// Accepted source OID.
    pub source: Seq<u8>,
    /// Recorded scratch path.
    pub scratch: Seq<u8>,
    /// Checked merge result, durable before any target movement.
    pub result: Option<Seq<u8>>,
    /// A rescue ref is still expected for a saved failed result.
    pub rescue_expected: bool,
}

/// Exact Git observations made under the integrator's serialization boundary.
#[expect(clippy::struct_excessive_bools, reason = "independent observed Git and user-request facts")]
pub struct GitView {
    /// Observed target ref OID, if it exists.
    pub target: Option<Seq<u8>>,
    /// Observed source branch OID, if it exists.
    pub source: Option<Seq<u8>>,
    /// Observed registered scratch path, if present.
    pub scratch: Option<Seq<u8>>,
    /// The scratch worktree is owned by this integration.
    pub scratch_owned: bool,
    /// The integrator exclusively owns the target branch checkout.
    pub target_checkout_owned: bool,
    /// A checkout-aware attempt to acquire the target branch was refused.
    pub checkout_attempt_failed: bool,
    /// Observed owned rescue ref OID, if it exists.
    pub rescue: Option<Seq<u8>>,
    /// Durable failure evidence from the merge or check.
    pub failure: Failure,
    /// The user explicitly requested applying a saved failed result.
    pub apply_requested: bool,
}

/// Persisted values read from the board ledger.
#[derive(Debug)]
pub struct Intent {
    /// Durable phase.
    pub phase: Phase,
    /// Pinned target OID as validated Git hex bytes.
    pub target: Vec<u8>,
    /// Accepted source OID as validated Git hex bytes.
    pub source: Vec<u8>,
    /// Recorded scratch path as bytes on the workspace host.
    pub scratch: Vec<u8>,
    /// Checked merge result, if durably recorded.
    pub result: Option<Vec<u8>>,
    /// A rescue ref is still expected for a saved failed result.
    pub rescue_expected: bool,
}

impl View for Intent {
    type V = IntentView;

    open spec fn view(&self) -> IntentView {
        IntentView {
            phase: self.phase,
            target: self.target@,
            source: self.source@,
            scratch: self.scratch@,
            result: match &self.result {
                Some(oid) => Some(oid@),
                None => None,
            },
            rescue_expected: self.rescue_expected,
        }
    }
}

/// Git state observed for one persisted integration intent.
#[derive(Debug)]
#[expect(clippy::struct_excessive_bools, reason = "independent observed Git and user-request facts")]
pub struct GitObservation {
    /// Observed target ref OID, if it exists.
    pub target: Option<Vec<u8>>,
    /// Observed source branch OID, if it exists.
    pub source: Option<Vec<u8>>,
    /// Observed registered scratch path, if present.
    pub scratch: Option<Vec<u8>>,
    /// The registered scratch worktree is owned by this integration.
    pub scratch_owned: bool,
    /// The integrator exclusively owns the target branch checkout.
    pub target_checkout_owned: bool,
    /// A checkout-aware attempt to acquire the target branch was refused.
    pub checkout_attempt_failed: bool,
    /// Observed owned rescue ref OID, if it exists.
    pub rescue: Option<Vec<u8>>,
    /// Durable merge/check failure evidence.
    pub failure: Failure,
    /// The user explicitly requested applying a saved failed result.
    pub apply_requested: bool,
}

impl View for GitObservation {
    type V = GitView;

    open spec fn view(&self) -> GitView {
        GitView {
            target: match &self.target {
                Some(oid) => Some(oid@),
                None => None,
            },
            source: match &self.source {
                Some(oid) => Some(oid@),
                None => None,
            },
            scratch: match &self.scratch {
                Some(path) => Some(path@),
                None => None,
            },
            scratch_owned: self.scratch_owned,
            target_checkout_owned: self.target_checkout_owned,
            checkout_attempt_failed: self.checkout_attempt_failed,
            rescue: match &self.rescue {
                Some(oid) => Some(oid@),
                None => None,
            },
            failure: self.failure,
            apply_requested: self.apply_requested,
        }
    }
}

/// LOCKED(ADR-0068): reconcile the durable intent against current Git observations. A moved
/// target is finalized before retry; a saved result is rescued before any attempted advance.
pub open spec fn reconcile(intent: IntentView, git: GitView) -> Decision {
    if intent.phase is Failed {
        if git.apply_requested && intent.result is Some && git.target == intent.result {
            Decision::MarkApplied
        } else if intent.rescue_expected && intent.result is Some && git.rescue != intent.result {
            Decision::EnsureRescue
        } else {
            Decision::None
        }
    } else if intent.phase == Phase::Queued {
        if intent.result is None && git.target == Some(intent.target) && git.source == Some(
            intent.source,
        ) && git.scratch is None && git.rescue is None {
            Decision::Begin
        } else {
            Decision::None
        }
    } else if intent.phase != Phase::Integrating {
        Decision::None
    } else if intent.result is Some && git.target == intent.result {
        Decision::FinalizeIntegrated
    } else if git.target != Some(intent.target) {
        Decision::FinalizeFailed
    } else if intent.result is Some {
        if git.rescue != intent.result {
            Decision::EnsureRescue
        } else if git.source != Some(intent.source) {
            Decision::FinalizeFailed
        } else if git.scratch != Some(intent.scratch) || !git.scratch_owned {
            Decision::FinalizeFailed
        } else if git.target_checkout_owned {
            Decision::AdvanceOwned
        } else if git.checkout_attempt_failed {
            Decision::FinalizeFailed
        } else {
            Decision::AcquireOwnedCheckout
        }
    } else if git.source != Some(intent.source) {
        Decision::FinalizeFailed
    } else if git.failure == Failure::Conflict {
        Decision::FinalizeConflict
    } else if git.failure == Failure::Failed {
        Decision::FinalizeFailed
    } else if git.scratch == Some(intent.scratch) && git.scratch_owned {
        Decision::ResumeScratch
    } else if git.scratch is None && git.rescue is None {
        Decision::RetryQueued
    } else {
        Decision::FinalizeFailed
    }
}

/// LOCKED(ADR-0068): only reconciliation-approved events change the persisted phase; explicit
/// apply is the sole route from a saved Failed result to Integrated.
pub open spec fn next(intent: IntentView, git: GitView, decision: Decision) -> Option<Phase> {
    if decision == reconcile(intent, git) {
        match decision {
            Decision::Begin => Some(Phase::Integrating),
            Decision::RetryQueued => Some(Phase::Queued),
            Decision::FinalizeIntegrated | Decision::MarkApplied => Some(Phase::Integrated),
            Decision::FinalizeFailed => Some(Phase::Failed),
            Decision::FinalizeConflict => Some(Phase::Conflict),
            _ => None,
        }
    } else {
        None
    }
}

/// LOCKED(ADR-0068): a saved result remains reachable before scratch cleanup unless the target
/// already names that result. An unowned or mismatched scratch path is never removed.
pub open spec fn cleanup_allowed(intent: IntentView, git: GitView) -> bool {
    &&& git.scratch == Some(intent.scratch)
    &&& git.scratch_owned
    &&& (intent.result is None || git.rescue == intent.result || git.target == intent.result)
}

/// One intent with only its phase replaced.
pub open spec fn with_phase(intent: IntentView, phase: Phase) -> IntentView {
    IntentView { phase, ..intent }
}

/// Whether an instruction records an initial terminal outcome.
pub open spec fn is_finalize(decision: Decision) -> bool {
    decision == Decision::FinalizeIntegrated || decision == Decision::FinalizeFailed || decision
        == Decision::FinalizeConflict
}

fn bytes_eq(a: &[u8], b: &[u8]) -> (same: bool)
    ensures
        same == (a@ == b@),
{
    if a.len() != b.len() {
        return false;
    }
    for k in 0..a.len()
        invariant
            a@.len() == b@.len(),
            forall|j: int| 0 <= j < k ==> #[trigger] a@[j] == b@[j],
    {
        if a[k] != b[k] {
            return false;
        }
    }
    proof {
        assert(a@ =~= b@);
    }
    true
}

#[expect(clippy::ref_option, reason = "the Verus postcondition models borrowed option contents directly")]
fn option_bytes_eq(a: &Option<Vec<u8>>, b: &Option<Vec<u8>>) -> (same: bool)
    ensures
        same == ((match a {
            Some(bytes) => Some(bytes@),
            None => None,
        }) == (match b {
            Some(bytes) => Some(bytes@),
            None => None,
        })),
{
    match (a, b) {
        (Some(left), Some(right)) => bytes_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

#[expect(clippy::ref_option, reason = "the Verus postcondition models borrowed option contents directly")]
fn option_matches(a: &Option<Vec<u8>>, b: &[u8]) -> (same: bool)
    ensures
        same == ((match a {
            Some(bytes) => Some(bytes@),
            None => None,
        }) == Some(b@)),
{
    match a {
        Some(bytes) => bytes_eq(bytes, b),
        None => false,
    }
}

/// Reconcile an observed Git snapshot with a persisted intent.
#[must_use]
pub fn decide_reconcile(intent: &Intent, git: &GitObservation) -> (decision: Decision)
    ensures
        decision == reconcile(intent@, git@),
{
    proof {
        reveal_with_fuel(reconcile, 1);
    }
    match intent.phase {
        Phase::Failed => {
            if let Some(result) = &intent.result {
                if git.apply_requested && option_matches(&git.target, result) {
                    return Decision::MarkApplied;
                }
                if intent.rescue_expected && !option_matches(&git.rescue, result) {
                    return Decision::EnsureRescue;
                }
            }
            return Decision::None;
        },
        Phase::Queued => {
            if intent.result.is_none() && option_matches(&git.target, &intent.target)
                && option_matches(&git.source, &intent.source) && git.scratch.is_none()
                && git.rescue.is_none() {
                return Decision::Begin;
            }
            return Decision::None;
        },
        Phase::Integrating => {},
        _ => return Decision::None,
    }
    #[expect(clippy::collapsible_if, reason = "the Verus proof tracks the option witness across branches")]
    if let Some(result) = &intent.result {
        if option_matches(&git.target, result) {
            return Decision::FinalizeIntegrated;
        }
    }
    if !option_matches(&git.target, &intent.target) {
        return Decision::FinalizeFailed;
    }
    if let Some(result) = &intent.result {
        if !option_matches(&git.rescue, result) {
            return Decision::EnsureRescue;
        }
        if !option_matches(&git.source, &intent.source) {
            return Decision::FinalizeFailed;
        }
        if !option_matches(&git.scratch, &intent.scratch) || !git.scratch_owned {
            return Decision::FinalizeFailed;
        }
        return if git.target_checkout_owned {
            Decision::AdvanceOwned
        } else if git.checkout_attempt_failed {
            Decision::FinalizeFailed
        } else {
            Decision::AcquireOwnedCheckout
        };
    }
    if !option_matches(&git.source, &intent.source) {
        return Decision::FinalizeFailed;
    }
    match git.failure {
        Failure::Conflict => return Decision::FinalizeConflict,
        Failure::Failed => return Decision::FinalizeFailed,
        Failure::None => {},
    }
    if option_matches(&git.scratch, &intent.scratch) && git.scratch_owned {
        return Decision::ResumeScratch;
    }
    if git.scratch.is_none() && git.rescue.is_none() {
        Decision::RetryQueued
    } else {
        Decision::FinalizeFailed
    }
}

/// Calculate the allowed phase change from one reconciled instruction.
#[must_use]
pub fn decide_next(intent: &Intent, git: &GitObservation, decision: Decision) -> (phase: Option<
    Phase,
>)
    ensures
        phase == next(intent@, git@, decision),
{
    proof {
        reveal_with_fuel(next, 1);
    }
    match (decision, decide_reconcile(intent, git)) {
        (Decision::Begin, Decision::Begin) => Some(Phase::Integrating),
        (Decision::RetryQueued, Decision::RetryQueued) => Some(Phase::Queued),
        (Decision::FinalizeIntegrated, Decision::FinalizeIntegrated)
        | (Decision::MarkApplied, Decision::MarkApplied) => Some(Phase::Integrated),
        (Decision::FinalizeFailed, Decision::FinalizeFailed) => Some(Phase::Failed),
        (Decision::FinalizeConflict, Decision::FinalizeConflict) => Some(Phase::Conflict),
        _ => None,
    }
}

/// Whether the shell may remove this recorded scratch path.
#[must_use]
pub fn decide_cleanup(intent: &Intent, git: &GitObservation) -> (allowed: bool)
    ensures
        allowed == cleanup_allowed(intent@, git@),
{
    option_matches(&git.scratch, &intent.scratch) && git.scratch_owned && (intent.result.is_none()
        || option_bytes_eq(&git.rescue, &intent.result) || option_bytes_eq(
        &git.target,
        &intent.result,
    ))
}

/// A moved target always selects a recorded outcome, never a retry or another ref move.
pub proof fn theorem_moved_target_has_outcome(intent: IntentView, git: GitView)
    requires
        intent.phase == Phase::Integrating,
        git.target != Some(intent.target),
    ensures
        reconcile(intent, git) == Decision::FinalizeIntegrated || reconcile(intent, git)
            == Decision::FinalizeFailed,
{
}

/// A finalization cannot be repeated; explicit apply is a separate, evidence-bound update.
pub proof fn theorem_finalize_at_most_once(intent: IntentView, git: GitView, next_git: GitView)
    requires
        next(intent, git, reconcile(intent, git)) is Some,
        is_finalize(reconcile(intent, git)),
    ensures
        !is_finalize(
            reconcile(with_phase(intent, next(intent, git, reconcile(intent, git))->0), next_git),
        ),
{
}

/// An automatic ref move needs the still-pinned accepted source and rescued checked result.
pub proof fn theorem_advance_uses_verified_source(intent: IntentView, git: GitView)
    ensures
        reconcile(intent, git) == Decision::AdvanceOwned ==> intent.phase == Phase::Integrating
            && intent.result is Some && git.source == Some(intent.source) && git.target == Some(
            intent.target,
        ) && git.rescue == intent.result && git.scratch == Some(intent.scratch) && git.scratch_owned
            && git.target_checkout_owned,
{
}

/// A retry only occurs before any observed target or rescue-ref movement.
pub proof fn theorem_retry_precedes_ref_movement(intent: IntentView, git: GitView)
    ensures
        reconcile(intent, git) == Decision::RetryQueued ==> intent.phase == Phase::Integrating
            && intent.result is None && git.target == Some(intent.target) && git.source == Some(
            intent.source,
        ) && git.rescue is None && git.scratch is None,
{
}

/// Explicit apply can update a failed row only after Git names its persisted result.
pub proof fn theorem_apply_requires_observed_result(intent: IntentView, git: GitView)
    ensures
        reconcile(intent, git) == Decision::MarkApplied ==> intent.phase == Phase::Failed
            && intent.result is Some && git.target == intent.result && git.apply_requested,
{
}

/// A result that was not advanced remains reachable under the owned rescue ref at cleanup.
pub proof fn theorem_cleanup_preserves_unadvanced_result(intent: IntentView, git: GitView)
    requires
        intent.result is Some,
        git.target != intent.result,
    ensures
        cleanup_allowed(intent, git) ==> git.scratch == Some(intent.scratch) && git.scratch_owned
            && git.rescue == intent.result,
{
}

} // verus!
