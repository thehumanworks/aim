//! Verified promotion decisions for the independent self-improvement gate (ADR 0060).
//!
//! The shell validates signatures, SHA-256 digests, refs, filesystem artifacts, and ledger I/O.
//! It passes those observations to this total state machine. A proof of a transition is
//! conditional on the shell reporting those observations truthfully.
use vstd::prelude::*;

verus! {

/// The step at which a promotion failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureStep {
    /// Independent candidate evaluation.
    Evaluate,
    /// Base compare-and-swap and merge.
    Merge,
    /// Remote push and reference readback.
    Push,
    /// Exact artifact deployment.
    Deploy,
    /// Offline canary.
    Canary,
    /// Activation pointer swap.
    Activate,
}

/// Promotion phase, including terminal exits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// Candidate exists but has no trusted receipt.
    Proposed,
    /// A signed, passing receipt is bound to the pinned evaluator.
    Evaluated,
    /// Candidate tree merged after a base compare-and-swap.
    Merged,
    /// The remote reference was read back at the merged SHA.
    Pushed,
    /// The exact SHA and runtime digests were installed.
    Deployed,
    /// Offline canary passed.
    Canary,
    /// The new artifact is active.
    Active,
    /// Evaluation or policy refused the candidate.
    Rejected,
    /// The previous runnable artifact was restored.
    RolledBack,
    /// An operational step failed and may require rollback.
    Failed(FailureStep),
}

/// Trusted observations for one promotion step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The shell verified the signed receipt, its bindings, and the gate result.
    Evaluate {
        /// Ed25519 signature verified against the gate's trusted key.
        signature_valid: bool,
        /// Receipt evaluator digest equals the gate's pinned evaluator digest.
        evaluator_pinned: bool,
        /// Candidate SHA/tree and baseline SHA match this proposal.
        candidate_bound: bool,
        /// Independent evaluation passed every required gate.
        passed: bool,
    },
    /// The target base still matched and the resulting tree is the evaluated tree.
    Merge {
        /// Target ref matched the receipt's baseline SHA at the atomic update.
        base_cas: bool,
        /// Merge result tree matches the evaluated candidate tree.
        tree_matches: bool,
    },
    /// The pushed remote ref was read back at the expected merged SHA.
    Push {
        /// Remote reference readback matched exactly.
        remote_readback: bool,
    },
    /// The deploy was read back and a runnable predecessor was retained.
    Deploy {
        /// Deployed commit matches the pushed SHA.
        exact_sha: bool,
        /// Binary, configuration, and schema digests match the recorded artifacts.
        digests_match: bool,
        /// The predecessor remains executable against the current store schema.
        predecessor_runnable: bool,
        /// The retained predecessor is the proposal's original rollback artifact.
        predecessor_matches: bool,
    },
    /// Offline canary result.
    Canary {
        /// The canary passed.
        passed: bool,
    },
    /// Swap the active pointer after the canary, then read it back.
    Activate {
        /// Active pointer readback matched the deployed artifact.
        pointer_readback: bool,
    },
    /// Reject an unpromoted candidate.
    Reject,
    /// Restore the retained runnable predecessor and read it back.
    Rollback {
        /// Active pointer readback matched the retained predecessor.
        predecessor_readback: bool,
    },
    /// Record a failed operational step.
    Fail {
        /// Step that failed.
        at: FailureStep,
    },
}

/// Authority and protected-set observations after a proposed transition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Boundary {
    /// Allowed capability bits after this step; only removal is permitted.
    pub ceiling: u64,
    /// Opaque digest identity for the protected set, established by the trusted shell.
    pub protected_digest: u64,
}

/// Small serializable projection of gate promotion state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GateView {
    /// Current phase.
    pub stage: Stage,
    /// A passing receipt with the pinned evaluator and matching candidate was verified.
    pub receipt_pinned: bool,
    /// A previous runnable artifact is retained for rollback.
    pub rollback_runnable: bool,
    /// Nonzero abstract identity of that artifact, mapped to a full SHA by the trusted shell.
    pub rollback_target: u64,
    /// Current capability ceiling.
    pub ceiling: u64,
    /// Protected-set digest fixed at proposal time.
    pub protected_digest: u64,
}

/// LOCKED(ADR-0060): a valid gate state retains rollback and cannot claim evaluation without
/// a verified pinned receipt.
pub open spec fn wf(v: GateView) -> bool {
    v.rollback_runnable && v.rollback_target != 0 && match v.stage {
        Stage::Proposed => !v.receipt_pinned,
        Stage::Evaluated
        | Stage::Merged
        | Stage::Pushed
        | Stage::Deployed
        | Stage::Canary
        | Stage::Active
        | Stage::RolledBack => v.receipt_pinned,
        Stage::Rejected | Stage::Failed(_) => true,
    }
}

fn valid(v: GateView) -> (yes: bool)
    ensures
        yes == wf(v),
{
    v.rollback_runnable && v.rollback_target != 0 && match v.stage {
        Stage::Proposed => !v.receipt_pinned,
        Stage::Evaluated
        | Stage::Merged
        | Stage::Pushed
        | Stage::Deployed
        | Stage::Canary
        | Stage::Active
        | Stage::RolledBack => v.receipt_pinned,
        Stage::Rejected | Stage::Failed(_) => true,
    }
}

/// LOCKED(ADR-0060): an initial proposal exists only with a runnable rollback artifact.
pub open spec fn initial(
    ceiling: u64,
    protected_digest: u64,
    rollback_target: u64,
    rollback_runnable: bool,
) -> Option<GateView> {
    if rollback_runnable && rollback_target != 0 {
        Some(
            GateView {
                stage: Stage::Proposed,
                receipt_pinned: false,
                rollback_runnable,
                rollback_target,
                ceiling,
                protected_digest,
            },
        )
    } else {
        None
    }
}

/// LOCKED(ADR-0060): one complete promotion transition or refusal. Every successful step
/// appends only a narrowing authority observation and leaves the protected digest unchanged.
pub open spec fn next(pre: GateView, ev: Event, boundary: Boundary) -> Option<GateView> {
    if !wf(pre) || boundary.ceiling & pre.ceiling != boundary.ceiling || boundary.protected_digest
        != pre.protected_digest {
        None
    } else {
        let stage = match ev {
            Event::Evaluate { signature_valid, evaluator_pinned, candidate_bound, passed } => {
                if pre.stage == Stage::Proposed && signature_valid && evaluator_pinned
                    && candidate_bound && passed {
                    Some(Stage::Evaluated)
                } else {
                    None
                }
            },
            Event::Merge { base_cas, tree_matches } => {
                if pre.stage == Stage::Evaluated && base_cas && tree_matches {
                    Some(Stage::Merged)
                } else {
                    None
                }
            },
            Event::Push { remote_readback } => {
                if pre.stage == Stage::Merged && remote_readback {
                    Some(Stage::Pushed)
                } else {
                    None
                }
            },
            Event::Deploy {
                exact_sha,
                digests_match,
                predecessor_runnable,
                predecessor_matches,
            } => {
                if pre.stage == Stage::Pushed && exact_sha && digests_match && predecessor_runnable
                    && predecessor_matches {
                    Some(Stage::Deployed)
                } else {
                    None
                }
            },
            Event::Canary { passed } => {
                if pre.stage == Stage::Deployed && passed {
                    Some(Stage::Canary)
                } else {
                    None
                }
            },
            Event::Activate { pointer_readback } => {
                if pre.stage == Stage::Canary && pointer_readback {
                    Some(Stage::Active)
                } else {
                    None
                }
            },
            Event::Reject => {
                if pre.stage == Stage::Proposed || pre.stage == Stage::Evaluated {
                    Some(Stage::Rejected)
                } else {
                    None
                }
            },
            Event::Rollback { predecessor_readback } => {
                if pre.stage == Stage::Deployed || pre.stage == Stage::Canary || pre.stage
                    == Stage::Active || (pre.stage is Failed && pre.receipt_pinned) {
                    if predecessor_readback {
                        Some(Stage::RolledBack)
                    } else {
                        None
                    }
                } else {
                    None
                }
            },
            Event::Fail { at } => {
                if (pre.stage == Stage::Proposed && at == FailureStep::Evaluate) || (pre.stage
                    == Stage::Evaluated && at == FailureStep::Merge) || (pre.stage == Stage::Merged
                    && at == FailureStep::Push) || (pre.stage == Stage::Pushed && at
                    == FailureStep::Deploy) || (pre.stage == Stage::Deployed && at
                    == FailureStep::Canary) || (pre.stage == Stage::Canary && at
                    == FailureStep::Activate) {
                    Some(Stage::Failed(at))
                } else {
                    None
                }
            },
        };
        match stage {
            Some(stage) => Some(
                GateView {
                    stage,
                    receipt_pinned: pre.receipt_pinned || stage == Stage::Evaluated,
                    rollback_runnable: pre.rollback_runnable,
                    rollback_target: pre.rollback_target,
                    ceiling: boundary.ceiling,
                    protected_digest: boundary.protected_digest,
                },
            ),
            None => None,
        }
    }
}

impl GateView {
    /// Constructs a proposal only when the prior artifact is runnable.
    #[must_use]
    pub fn proposed(
        ceiling: u64,
        protected_digest: u64,
        rollback_target: u64,
        rollback_runnable: bool,
    ) -> (out: Option<Self>)
        ensures
            out == initial(ceiling, protected_digest, rollback_target, rollback_runnable),
    {
        if rollback_runnable && rollback_target != 0 {
            Some(
                Self {
                    stage: Stage::Proposed,
                    receipt_pinned: false,
                    rollback_runnable,
                    rollback_target,
                    ceiling,
                    protected_digest,
                },
            )
        } else {
            None
        }
    }
}

fn same_stage(left: Stage, right: Stage) -> (yes: bool)
    ensures
        yes == (left == right),
{
    match (left, right) {
        (Stage::Proposed, Stage::Proposed)
        | (Stage::Evaluated, Stage::Evaluated)
        | (Stage::Merged, Stage::Merged)
        | (Stage::Pushed, Stage::Pushed)
        | (Stage::Deployed, Stage::Deployed)
        | (Stage::Canary, Stage::Canary)
        | (Stage::Active, Stage::Active)
        | (Stage::Rejected, Stage::Rejected)
        | (Stage::RolledBack, Stage::RolledBack) => true,
        (Stage::Failed(a), Stage::Failed(b)) => same_failure(a, b),
        _ => false,
    }
}

#[expect(clippy::match_like_matches_macro, reason = "explicit variants relate Rust equality to Verus structural equality")]
fn same_failure(left: FailureStep, right: FailureStep) -> (yes: bool)
    ensures
        yes == (left == right),
{
    match (left, right) {
        (FailureStep::Evaluate, FailureStep::Evaluate)
        | (FailureStep::Merge, FailureStep::Merge)
        | (FailureStep::Push, FailureStep::Push)
        | (FailureStep::Deploy, FailureStep::Deploy)
        | (FailureStep::Canary, FailureStep::Canary)
        | (FailureStep::Activate, FailureStep::Activate) => true,
        _ => false,
    }
}

fn is_failed(stage: Stage) -> (yes: bool)
    ensures
        yes == (stage is Failed),
{
    matches!(stage, Stage::Failed(_))
}

/// Applies a trusted observation. Invalid state, order, digest, authority or evidence refuses
/// without a state change.
#[must_use]
#[expect(clippy::manual_map, reason = "Verus proves this explicit Option case split against the transition spec")]
pub fn advance(pre: GateView, ev: Event, boundary: Boundary) -> (out: Option<GateView>)
    ensures
        out == next(pre, ev, boundary),
{
    if !valid(pre) || boundary.ceiling & pre.ceiling != boundary.ceiling
        || boundary.protected_digest != pre.protected_digest {
        return None;
    }
    let stage = match ev {
        Event::Evaluate { signature_valid, evaluator_pinned, candidate_bound, passed } => {
            if same_stage(pre.stage, Stage::Proposed) && signature_valid && evaluator_pinned
                && candidate_bound && passed {
                Some(Stage::Evaluated)
            } else {
                None
            }
        },
        Event::Merge { base_cas, tree_matches } => {
            if same_stage(pre.stage, Stage::Evaluated) && base_cas && tree_matches {
                Some(Stage::Merged)
            } else {
                None
            }
        },
        Event::Push { remote_readback } => {
            if same_stage(pre.stage, Stage::Merged) && remote_readback {
                Some(Stage::Pushed)
            } else {
                None
            }
        },
        Event::Deploy { exact_sha, digests_match, predecessor_runnable, predecessor_matches } => {
            if same_stage(pre.stage, Stage::Pushed) && exact_sha && digests_match
                && predecessor_runnable && predecessor_matches {
                Some(Stage::Deployed)
            } else {
                None
            }
        },
        Event::Canary { passed } => {
            if same_stage(pre.stage, Stage::Deployed) && passed {
                Some(Stage::Canary)
            } else {
                None
            }
        },
        Event::Activate { pointer_readback } => {
            if same_stage(pre.stage, Stage::Canary) && pointer_readback {
                Some(Stage::Active)
            } else {
                None
            }
        },
        Event::Reject => {
            if same_stage(pre.stage, Stage::Proposed) || same_stage(pre.stage, Stage::Evaluated) {
                Some(Stage::Rejected)
            } else {
                None
            }
        },
        Event::Rollback { predecessor_readback } => {
            if same_stage(pre.stage, Stage::Deployed) || same_stage(pre.stage, Stage::Canary)
                || same_stage(pre.stage, Stage::Active) || (is_failed(pre.stage)
                && pre.receipt_pinned) {
                if predecessor_readback {
                    Some(Stage::RolledBack)
                } else {
                    None
                }
            } else {
                None
            }
        },
        Event::Fail { at } => {
            if (same_stage(pre.stage, Stage::Proposed) && same_failure(at, FailureStep::Evaluate))
                || (same_stage(pre.stage, Stage::Evaluated) && same_failure(at, FailureStep::Merge))
                || (same_stage(pre.stage, Stage::Merged) && same_failure(at, FailureStep::Push))
                || (same_stage(pre.stage, Stage::Pushed) && same_failure(at, FailureStep::Deploy))
                || (same_stage(pre.stage, Stage::Deployed) && same_failure(at, FailureStep::Canary))
                || (same_stage(pre.stage, Stage::Canary) && same_failure(
                at,
                FailureStep::Activate,
            )) {
                Some(Stage::Failed(at))
            } else {
                None
            }
        },
    };
    match stage {
        Some(stage) => Some(
            GateView {
                stage,
                receipt_pinned: pre.receipt_pinned || same_stage(stage, Stage::Evaluated),
                rollback_runnable: pre.rollback_runnable,
                rollback_target: pre.rollback_target,
                ceiling: boundary.ceiling,
                protected_digest: boundary.protected_digest,
            },
        ),
        None => None,
    }
}

/// LOCKED(ADR-0060): the durable ledger projection grows by exactly one event; prior entries
/// are never rewritten or removed by a transition.
pub open spec fn ledger_append<T>(prior: Seq<T>, entry: T) -> Seq<T> {
    prior.push(entry)
}

/// LOCKED(ADR-0060): a successful state transition contributes exactly its new stage to the
/// append-only ledger view; a refused transition contributes nothing.
pub open spec fn history_next(
    pre: GateView,
    prior: Seq<Stage>,
    ev: Event,
    boundary: Boundary,
) -> Option<(GateView, Seq<Stage>)> {
    match next(pre, ev, boundary) {
        Some(post) => Some((post, ledger_append(prior, post.stage))),
        None => None,
    }
}

/// The constructor establishes a well-formed proposed state.
pub proof fn lemma_initial_wf(ceiling: u64, digest: u64, target: u64, runnable: bool)
    ensures
        initial(ceiling, digest, target, runnable) matches Some(v) ==> wf(v),
{
}

/// A successful step preserves receipt and rollback invariants.
pub proof fn lemma_next_wf(pre: GateView, ev: Event, boundary: Boundary)
    requires
        wf(pre),
    ensures
        next(pre, ev, boundary) matches Some(post) ==> wf(post),
{
}

/// Activation requires the independently verified pinned evaluator receipt.
pub proof fn activation_requires_pinned_receipt(pre: GateView, ev: Event, boundary: Boundary)
    ensures
        match next(pre, ev, boundary) {
            Some(post) => post.stage == Stage::Active ==> post.receipt_pinned,
            None => true,
        },
{
}

/// A successful step cannot increase the authority bitset.
pub proof fn authority_never_widens(pre: GateView, ev: Event, boundary: Boundary)
    ensures
        next(pre, ev, boundary) matches Some(post) ==> post.ceiling & pre.ceiling == post.ceiling,
{
}

/// A successful step cannot change the protected-set digest.
pub proof fn protected_digest_unchanged(pre: GateView, ev: Event, boundary: Boundary)
    ensures
        next(pre, ev, boundary) matches Some(post) ==> post.protected_digest
            == pre.protected_digest,
{
}

/// Every state reachable by a successful transition keeps its runnable predecessor.
pub proof fn rollback_target_retained(pre: GateView, ev: Event, boundary: Boundary)
    requires
        wf(pre),
    ensures
        next(pre, ev, boundary) matches Some(post) ==> post.rollback_runnable
            && post.rollback_target == pre.rollback_target && post.rollback_target != 0,
{
}

/// Appending one ledger event keeps the complete earlier ledger as an exact prefix.
pub proof fn ledger_append_only<T>(prior: Seq<T>, entry: T)
    ensures
        ledger_append(prior, entry).subrange(0, prior.len() as int) == prior,
{
}

/// Any successful promotion preserves every prior ledger event in order.
pub proof fn promotion_ledger_append_only(
    pre: GateView,
    prior: Seq<Stage>,
    ev: Event,
    boundary: Boundary,
)
    ensures
        history_next(pre, prior, ev, boundary) matches Some(result) ==> result.1.subrange(
            0,
            prior.len() as int,
        ) == prior,
{
}

} // verus!
