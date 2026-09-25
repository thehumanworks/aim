//! Code mode's exposure (ADR 0076): which tools a model is offered when code mode is off, on, or
//! the only way to act.
//!
//! The shell parses `AIM_CODE_MODE` into a [`CodeModeRequest`] and learns whether the
//! `aim-coderun` worker was found, whether this platform can sandbox it, and whether the session's
//! tool ceiling permits `run_code`. [`decide`] turns those facts into an [`Exposure`]: the effective
//! mode, whether the code and program tools are offered, which direct tools stay visible, and why
//! a requested mode fell back to `Off`. [`direct_tools`] then selects the visible direct tools by
//! host-assigned name ids, so the compact set is always a subset of the full one.
#[cfg(verus_only)]
use crate::agent_tools::has_id;
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// A code-mode setting: requested, default, or effective.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// No code or program tools; every direct tool is offered.
    Off,
    /// The code and program tools beside a compact set of direct tools.
    On,
    /// Only the code and program tools; every other tool is reachable inside a cell.
    Only,
}

/// The default (ADR 0076 §6): the maintainer's decision that code mode is the preferred default,
/// taken knowing T4b's benchmark rule found no efficiency gain over `Off` (`bench/plans/code-mode.md`).
/// `AIM_CODE_MODE=off` opts out.
pub const DEFAULT_MODE: Mode = Mode::On;

/// What `AIM_CODE_MODE` asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeModeRequest {
    /// Not set: the default applies.
    Unset,
    /// Set to a value that names no mode: code mode is off (it fails closed).
    Invalid,
    /// Set to a mode.
    Set(Mode),
}

/// Why a requested code mode fell back to `Off`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fallback {
    /// This platform cannot sandbox the worker.
    PlatformUnsupported,
    /// The `aim-coderun` worker was not found.
    WorkerMissing,
    /// The session's tool ceiling does not permit `run_code`.
    NotPermitted,
}

/// Which direct (non-code) tools the model sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direct {
    /// Every direct tool.
    Full,
    /// Every direct tool except the shell's hidden set.
    Compact,
    /// No direct tool.
    Hidden,
}

/// What a session's model is offered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exposure {
    /// The effective mode.
    pub mode: Mode,
    /// Whether the code tool (`run_code`, or codex's `exec`/`wait`) is offered.
    pub code: bool,
    /// Whether the saved-program tools are offered.
    pub programs: bool,
    /// Which direct tools are offered.
    pub direct: Direct,
    /// Why a requested mode fell back to `Off`; `None` when nothing fell back.
    pub fallback: Option<Fallback>,
}

/// DRAFT(ADR-0076): an unset request means the default, and an invalid one means `Off`: a value
/// that names no mode never enables code mode.
pub open spec fn wanted_mode(requested: CodeModeRequest, default: Mode) -> Mode {
    match requested {
        CodeModeRequest::Unset => default,
        CodeModeRequest::Invalid => Mode::Off,
        CodeModeRequest::Set(mode) => mode,
    }
}

/// DRAFT(ADR-0076): what keeps code mode from running, checked in this order: the platform, the
/// worker, then the session's tool ceiling.
pub open spec fn code_blocker(worker: bool, platform: bool, permitted: bool) -> Option<Fallback> {
    if !platform {
        Some(Fallback::PlatformUnsupported)
    } else if !worker {
        Some(Fallback::WorkerMissing)
    } else if !permitted {
        Some(Fallback::NotPermitted)
    } else {
        None
    }
}

/// DRAFT(ADR-0076): what each effective mode offers.
pub open spec fn mode_exposure(mode: Mode, fallback: Option<Fallback>) -> Exposure {
    match mode {
        Mode::Off => Exposure {
            mode,
            code: false,
            programs: false,
            direct: Direct::Full,
            fallback,
        },
        Mode::On => Exposure {
            mode,
            code: true,
            programs: true,
            direct: Direct::Compact,
            fallback,
        },
        Mode::Only => Exposure {
            mode,
            code: true,
            programs: true,
            direct: Direct::Hidden,
            fallback,
        },
    }
}

/// DRAFT(ADR-0076): the wanted mode applies when nothing blocks code mode; otherwise the session
/// falls back to `Off`, and says why unless `Off` was what it wanted.
pub open spec fn code_mode_decision(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
) -> Exposure {
    let mode = wanted_mode(requested, default);
    if mode == Mode::Off {
        mode_exposure(Mode::Off, None)
    } else {
        match code_blocker(worker, platform, permitted) {
            Some(reason) => mode_exposure(Mode::Off, Some(reason)),
            None => mode_exposure(mode, None),
        }
    }
}

/// DRAFT(ADR-0076): whether a direct tool is shown: all in `Full`, all but the hidden ones in
/// `Compact`, none in `Hidden`.
pub open spec fn shows_direct(direct: Direct, hidden: Seq<u64>, id: u64) -> bool {
    match direct {
        Direct::Full => true,
        Direct::Compact => !has_id(hidden, id),
        Direct::Hidden => false,
    }
}

/// The predicate [`shows_direct`] as a function over ids.
pub open spec fn shown_direct(direct: Direct, hidden: Seq<u64>) -> spec_fn(u64) -> bool {
    |id: u64| shows_direct(direct, hidden, id)
}

/// DRAFT(ADR-0076): the direct tools offered, in the workspace's order.
pub open spec fn direct_view(direct: Direct, hidden: Seq<u64>, tools: Seq<u64>) -> Seq<u64> {
    tools.filter(shown_direct(direct, hidden))
}

/// Decides code mode's exposure: total, and exactly [`code_mode_decision`].
#[must_use]
pub fn decide(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
) -> (out: Exposure)
    ensures
        out == code_mode_decision(requested, default, worker, platform, permitted),
{
    let mode = match requested {
        CodeModeRequest::Unset => default,
        CodeModeRequest::Invalid => Mode::Off,
        CodeModeRequest::Set(mode) => mode,
    };
    let fallback = if !platform {
        Some(Fallback::PlatformUnsupported)
    } else if !worker {
        Some(Fallback::WorkerMissing)
    } else if !permitted {
        Some(Fallback::NotPermitted)
    } else {
        None
    };
    let (mode, fallback) = match (mode, fallback) {
        (Mode::Off, _) => (Mode::Off, None),
        (_, Some(reason)) => (Mode::Off, Some(reason)),
        (mode, None) => (mode, None),
    };
    match mode {
        Mode::Off => Exposure {
            mode,
            code: false,
            programs: false,
            direct: Direct::Full,
            fallback,
        },
        Mode::On => Exposure {
            mode,
            code: true,
            programs: true,
            direct: Direct::Compact,
            fallback,
        },
        Mode::Only => Exposure {
            mode,
            code: true,
            programs: true,
            direct: Direct::Hidden,
            fallback,
        },
    }
}

fn has_hidden(ids: &[u64], id: u64) -> (found: bool)
    ensures
        found == has_id(ids@, id),
{
    for i in 0..ids.len()
        invariant
            forall|j: int| 0 <= j < i ==> ids@[j] != id,
    {
        if ids[i] == id {
            return true;
        }
    }
    false
}

/// Selects the direct tools `direct` offers from `tools` (name ids in the workspace's order),
/// given the shell's `hidden` ids: total, and exactly [`direct_view`].
#[must_use]
pub fn direct_tools(direct: Direct, tools: &[u64], hidden: &[u64]) -> (out: Vec<u64>)
    ensures
        out@ == direct_view(direct, hidden@, tools@),
{
    let mut out: Vec<u64> = Vec::new();
    for i in 0..tools.len()
        invariant
            out@ == tools@.subrange(0, i as int).filter(shown_direct(direct, hidden@)),
    {
        let id = tools[i];
        let keep = match direct {
            Direct::Full => true,
            Direct::Compact => !has_hidden(hidden, id),
            Direct::Hidden => false,
        };
        proof {
            let pred = shown_direct(direct, hidden@);
            assert(tools@.subrange(0, i + 1) =~= tools@.subrange(0, i as int).push(id));
            tools@.subrange(0, i as int).lemma_filter_push(id, pred);
            assert(keep == pred(id));
        }
        if keep {
            out.push(id);
        }
    }
    proof {
        assert(tools@.subrange(0, tools@.len() as int) =~= tools@);
    }
    out
}

proof fn lemma_direct_filter_within(s: Seq<u64>, pred: spec_fn(u64) -> bool)
    ensures
        forall|i: int|
            0 <= i < s.filter(pred).len() ==> s.contains(#[trigger] s.filter(pred)[i]) && pred(
                s.filter(pred)[i],
            ),
    decreases s.len(),
{
    reveal(Seq::filter);
    if s.len() > 0 {
        let rest = s.drop_last();
        lemma_direct_filter_within(rest, pred);
        let out = s.filter(pred);
        assert forall|i: int| 0 <= i < out.len() implies s.contains(#[trigger] out[i]) && pred(
            out[i],
        ) by {
            if pred(s.last()) && i == out.len() - 1 {
                assert(s[s.len() - 1] == out[i]);
            } else {
                assert(out[i] == rest.filter(pred)[i]);
                let j = choose|j: int| 0 <= j < rest.len() && rest[j] == rest.filter(pred)[i];
                assert(s[j] == out[i]);
            }
        }
    }
}

proof fn lemma_direct_filter_all(s: Seq<u64>, pred: spec_fn(u64) -> bool)
    requires
        forall|id: u64| #[trigger] pred(id),
    ensures
        s.filter(pred) == s,
    decreases s.len(),
{
    reveal(Seq::filter);
    if s.len() > 0 {
        lemma_direct_filter_all(s.drop_last(), pred);
        assert(s.drop_last().push(s.last()) =~= s);
    }
}

proof fn lemma_direct_filter_none(s: Seq<u64>, pred: spec_fn(u64) -> bool)
    requires
        forall|id: u64| !#[trigger] pred(id),
    ensures
        s.filter(pred) == Seq::<u64>::empty(),
    decreases s.len(),
{
    reveal(Seq::filter);
    if s.len() > 0 {
        lemma_direct_filter_none(s.drop_last(), pred);
    }
}

/// An unset request is exactly a request for the default.
pub proof fn theorem_unset_means_default(
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
)
    ensures
        code_mode_decision(CodeModeRequest::Unset, default, worker, platform, permitted)
            == code_mode_decision(
            CodeModeRequest::Set(default),
            default,
            worker,
            platform,
            permitted,
        ),
{
}

/// An invalid request fails closed: no code or program tools and every direct tool, whatever the
/// default, and it is not reported as a fallback (the shell warns about the value itself).
pub proof fn theorem_invalid_means_off(default: Mode, worker: bool, platform: bool, permitted: bool)
    ensures
        code_mode_decision(CodeModeRequest::Invalid, default, worker, platform, permitted)
            == mode_exposure(Mode::Off, None),
{
}

/// Code and program tools are offered only when the worker was found, the platform sandboxes
/// it, and the session's ceiling permits `run_code`.
pub proof fn theorem_code_needs_worker_platform_and_permission(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
)
    ensures
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            (out.code || out.programs) ==> worker && platform && permitted
        }),
{
}

/// Code mode never widens a tool ceiling: a session whose ceiling does not permit `run_code` is
/// offered no code or program tools and keeps its full direct set, whatever was requested
/// (so `Only` never applies to it).
pub proof fn theorem_never_widens_a_ceiling(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
)
    ensures
        ({
            let out = code_mode_decision(requested, default, worker, platform, false);
            !out.code && !out.programs && out.direct == Direct::Full && out.mode == Mode::Off
        }),
{
}

/// `Off` offers no code or program tools, and every direct tool.
pub proof fn theorem_off_offers_no_code(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
)
    ensures
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            out.mode == Mode::Off <==> !out.code
        }),
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            out.mode == Mode::Off ==> !out.programs && out.direct == Direct::Full
        }),
        wanted_mode(requested, default) == Mode::Off ==> code_mode_decision(
            requested,
            default,
            worker,
            platform,
            permitted,
        ) == mode_exposure(Mode::Off, None),
{
}

/// `Only` never leaves the model without tools: whenever the direct tools are hidden, the code
/// tool is offered. A request for `Only` that cannot run falls back to `Off` with every direct
/// tool and names what blocked it.
pub proof fn theorem_only_is_never_empty(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
)
    ensures
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            out.direct == Direct::Hidden ==> out.code && out.mode == Mode::Only
        }),
        wanted_mode(requested, default) == Mode::Only && code_blocker(
            worker,
            platform,
            permitted,
        ) is Some ==> code_mode_decision(requested, default, worker, platform, permitted)
            == mode_exposure(Mode::Off, code_blocker(worker, platform, permitted)),
{
}

/// The wanted mode applies exactly when nothing blocks code mode; a fallback always names its
/// cause, and only a mode that was not `Off` can fall back.
pub proof fn theorem_fallback_names_its_cause(
    requested: CodeModeRequest,
    default: Mode,
    worker: bool,
    platform: bool,
    permitted: bool,
)
    ensures
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            code_blocker(worker, platform, permitted) is None ==> out.mode == wanted_mode(
                requested,
                default,
            ) && out.fallback is None
        }),
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            out.fallback is Some <==> (wanted_mode(requested, default) != Mode::Off && code_blocker(
                worker,
                platform,
                permitted,
            ) is Some)
        }),
        ({
            let out = code_mode_decision(requested, default, worker, platform, permitted);
            out.fallback is Some ==> out.fallback == code_blocker(worker, platform, permitted)
                && out.mode == Mode::Off
        }),
{
}

/// The full direct set is every tool, in order; the hidden set is empty.
pub proof fn theorem_full_and_hidden_direct_sets(hidden: Seq<u64>, tools: Seq<u64>)
    ensures
        direct_view(Direct::Full, hidden, tools) == tools,
        direct_view(Direct::Hidden, hidden, tools) == Seq::<u64>::empty(),
{
    lemma_direct_filter_all(tools, shown_direct(Direct::Full, hidden));
    lemma_direct_filter_none(tools, shown_direct(Direct::Hidden, hidden));
}

/// Every direct tool offered in any mode is in the full direct set, and the compact set never
/// offers a hidden tool: `On`'s direct set is a subset of `Off`'s.
pub proof fn theorem_direct_sets_narrow(direct: Direct, hidden: Seq<u64>, tools: Seq<u64>)
    ensures
        forall|i: int|
            0 <= i < direct_view(direct, hidden, tools).len() ==> direct_view(
                Direct::Full,
                hidden,
                tools,
            ).contains(#[trigger] direct_view(direct, hidden, tools)[i]),
        forall|i: int|
            0 <= i < direct_view(Direct::Compact, hidden, tools).len() ==> !has_id(
                hidden,
                #[trigger] direct_view(Direct::Compact, hidden, tools)[i],
            ),
        direct_view(direct, hidden, tools).len() <= tools.len(),
{
    theorem_full_and_hidden_direct_sets(hidden, tools);
    lemma_direct_filter_within(tools, shown_direct(direct, hidden));
    lemma_direct_filter_within(tools, shown_direct(Direct::Compact, hidden));
    tools.lemma_filter_len(shown_direct(direct, hidden));
}

} // verus!
