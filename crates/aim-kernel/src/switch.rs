//! Session switches and effort choices of the TUI (ADR 0074).
//!
//! The shell assigns every string one decision involves (provider, location, workspace, model and
//! effort) a `u64` from one registry, so equal ids mean equal strings; this module never sees
//! text. A session's provider is fixed at creation, so `/provider` means a new session, and a
//! model or effort chosen under the old provider must not travel into it.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// A session as the switch decisions see it: ids from the shell's registry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shape {
    /// Provider id.
    pub provider: u64,
    /// Where the workspace is (`local`, `ssh:<destination>`, `remote:<url>`).
    pub location: u64,
    /// Workspace root.
    pub workspace: u64,
    /// Kept (`true`) or ephemeral.
    pub persistent: bool,
    /// Model, when one is chosen (else the provider's default).
    pub model: Option<u64>,
    /// Effort, when one is chosen.
    pub effort: Option<u64>,
}

/// What the user asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Switch {
    /// `/new`: another session like this one, below the old output.
    New,
    /// `/clear`: another session like this one, on a cleared screen.
    Clear,
    /// `/provider <id>`: another session like this one on provider `id`.
    Provider(u64),
}

/// DRAFT(ADR-0074): what a switch starts from: the attached session, else the configured one.
pub open spec fn base_spec(attached: Option<Shape>, configured: Shape) -> Shape {
    match attached {
        Some(shape) => shape,
        None => configured,
    }
}

/// DRAFT(ADR-0074): the session a switch creates from `base`, or `None` when it changes nothing.
/// `/new` and `/clear` keep everything; `/provider` keeps the place and privacy but no model or
/// effort, and is a no-op for the provider already in use.
pub open spec fn switch_spec(base: Shape, switch: Switch) -> Option<Shape> {
    match switch {
        Switch::New => Some(base),
        Switch::Clear => Some(base),
        Switch::Provider(provider) => if provider == base.provider {
            None
        } else {
            Some(
                Shape {
                    provider,
                    location: base.location,
                    workspace: base.workspace,
                    persistent: base.persistent,
                    model: None,
                    effort: None,
                },
            )
        },
    }
}

/// A new session never carries a model or effort chosen under another provider: whatever it
/// carries comes from a base on the same provider.
pub proof fn provider_switch_resets_model_and_effort(base: Shape, switch: Switch)
    ensures
        match switch_spec(base, switch) {
            Some(out) => (out.provider != base.provider ==> out.model is None && out.effort is None)
                && (out.model is Some ==> out.provider == base.provider && out.model == base.model)
                && (out.effort is Some ==> out.provider == base.provider && out.effort
                == base.effort),
            None => true,
        },
{
}

/// `/provider` naming the provider in use changes nothing.
pub proof fn same_provider_switch_is_a_no_op(base: Shape)
    ensures
        switch_spec(base, Switch::Provider(base.provider)) is None,
        forall|provider: u64|
            provider != base.provider ==> #[trigger] switch_spec(
                base,
                Switch::Provider(provider),
            ) is Some,
{
}

/// `/new` and `/clear` create the same session as the one they start from.
pub proof fn new_and_clear_keep_the_session_shape(base: Shape)
    ensures
        switch_spec(base, Switch::New) == Some(base),
        switch_spec(base, Switch::Clear) == Some(base),
{
}

/// Every switch stays in the same workspace, place and privacy: an ephemeral session never begets
/// a kept one.
pub proof fn switches_keep_place_and_privacy(base: Shape, switch: Switch)
    ensures
        match switch_spec(base, switch) {
            Some(out) => out.location == base.location && out.workspace == base.workspace
                && out.persistent == base.persistent,
            None => true,
        },
{
}

/// Derives the session a switch creates: from the attached session when there is one, else from
/// the configured one. `None`: nothing to create.
#[must_use]
pub fn derive(attached: Option<Shape>, configured: Shape, switch: Switch) -> (out: Option<Shape>)
    ensures
        out == switch_spec(base_spec(attached, configured), switch),
{
    let base = match attached {
        Some(shape) => shape,
        None => configured,
    };
    match switch {
        Switch::New | Switch::Clear => Some(base),
        Switch::Provider(provider) => if provider == base.provider {
            None
        } else {
            Some(
                Shape {
                    provider,
                    location: base.location,
                    workspace: base.workspace,
                    persistent: base.persistent,
                    model: None,
                    effort: None,
                },
            )
        },
    }
}

/// A requested effort.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffortRequest {
    /// `auto`: hand the effort back to aim (ADR 0038).
    Auto,
    /// A level, by registry id.
    Level(u64),
}

/// Whether the session takes `auto`, as far as the shell knows (its options say, ADR 0074).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoEffort {
    /// The session has not said (its options have not arrived): the session decides.
    Unknown,
    /// The session takes `auto`.
    Taken,
    /// The session refuses `auto` (an agent that does not advertise it).
    Refused,
}

/// DRAFT(ADR-0074): whether the shell sends a requested effort for a model whose ladder is
/// `ladder`. `auto` goes unless the session refuses it; a level goes when it is on the ladder. An
/// empty ladder is not known (options not arrived, or the model reports none): the backend
/// decides, as before.
pub open spec fn effort_sent_spec(
    ladder: Seq<u64>,
    auto: AutoEffort,
    request: EffortRequest,
) -> bool {
    match request {
        EffortRequest::Auto => auto != AutoEffort::Refused,
        EffortRequest::Level(id) => ladder.len() == 0 || ladder.contains(id),
    }
}

/// With a known ladder, only a level on it is sent; `auto` is never sent to a session that refuses
/// it; anything refused is a level off a known ladder, or `auto` where it is refused.
pub proof fn effort_sent_only_if_offered(ladder: Seq<u64>, auto: AutoEffort, request: EffortRequest)
    ensures
        effort_sent_spec(ladder, auto, request) ==> match request {
            EffortRequest::Auto => auto != AutoEffort::Refused,
            EffortRequest::Level(id) => ladder.len() == 0 || ladder.contains(id),
        },
        !effort_sent_spec(ladder, auto, request) ==> match request {
            EffortRequest::Auto => auto == AutoEffort::Refused,
            EffortRequest::Level(id) => ladder.len() > 0 && !ladder.contains(id),
        },
{
}

/// `auto` never reaches a session that refuses it.
pub proof fn auto_is_never_sent_where_refused(ladder: Seq<u64>)
    ensures
        !effort_sent_spec(ladder, AutoEffort::Refused, EffortRequest::Auto),
{
}

/// The ladder's levels in their order, each at its first occurrence, without `auto_id` (an `auto`
/// on the ladder counts as `auto`, not as a level).
pub open spec fn distinct_levels(ladder: Seq<u64>, auto_id: u64) -> Seq<u64>
    decreases ladder.len(),
{
    if ladder.len() == 0 {
        Seq::empty()
    } else {
        let rest = distinct_levels(ladder.drop_last(), auto_id);
        let last = ladder.last();
        if last == auto_id || rest.contains(last) {
            rest
        } else {
            rest.push(last)
        }
    }
}

/// DRAFT(ADR-0074): the efforts offered for a ladder, in this order: its levels as
/// [`distinct_levels`] lists them, then `auto` exactly when the session takes it.
pub open spec fn effort_candidates_spec(
    ladder: Seq<u64>,
    auto_id: u64,
    auto: AutoEffort,
    out: Seq<u64>,
) -> bool {
    out == distinct_levels(ladder, auto_id) + if auto == AutoEffort::Taken {
        seq![auto_id]
    } else {
        Seq::empty()
    }
}

/// The request an offered id stands for.
pub open spec fn request_of(id: u64, auto_id: u64) -> EffortRequest {
    if id == auto_id {
        EffortRequest::Auto
    } else {
        EffortRequest::Level(id)
    }
}

proof fn lemma_push_contains(ids: Seq<u64>, item: u64)
    ensures
        forall|id: u64|
            #![trigger ids.push(item).contains(id)]
            ids.push(item).contains(id) <==> (ids.contains(id) || id == item),
{
    let pushed = ids.push(item);
    assert forall|id: u64| pushed.contains(id) <==> (ids.contains(id) || id == item) by {
        if pushed.contains(id) {
            let k = choose|k: int| 0 <= k < pushed.len() && pushed[k] == id;
            if k < ids.len() {
                assert(ids[k] == id);
            }
        }
        if ids.contains(id) {
            let k = choose|k: int| 0 <= k < ids.len() && ids[k] == id;
            assert(pushed[k] == id);
        }
        if id == item {
            assert(pushed[ids.len() as int] == id);
        }
    }
}

/// [`distinct_levels`] holds each level of the ladder once, and nothing else.
pub proof fn distinct_levels_are_the_ladder_once(ladder: Seq<u64>, auto_id: u64)
    ensures
        distinct_levels(ladder, auto_id).no_duplicates(),
        forall|id: u64|
            #![trigger distinct_levels(ladder, auto_id).contains(id)]
            distinct_levels(ladder, auto_id).contains(id) <==> (ladder.contains(id) && id
                != auto_id),
    decreases ladder.len(),
{
    if ladder.len() > 0 {
        let init = ladder.drop_last();
        let last = ladder.last();
        distinct_levels_are_the_ladder_once(init, auto_id);
        let rest = distinct_levels(init, auto_id);
        assert(ladder =~= init.push(last));
        lemma_push_contains(init, last);
        if !(last == auto_id || rest.contains(last)) {
            lemma_push_fresh(rest, last);
        }
        assert forall|id: u64|
            #![trigger distinct_levels(ladder, auto_id).contains(id)]
            distinct_levels(ladder, auto_id).contains(id) <==> (ladder.contains(id) && id
                != auto_id) by {
            assert(ladder.contains(id) <==> init.push(last).contains(id));
        }
    } else {
        assert forall|id: u64|
            #![trigger distinct_levels(ladder, auto_id).contains(id)]
            distinct_levels(ladder, auto_id).contains(id) <==> (ladder.contains(id) && id
                != auto_id) by {
            assert(distinct_levels(ladder, auto_id) =~= Seq::<u64>::empty());
        }
    }
}

/// The candidates: every level of the ladder once, in the ladder's order, then `auto` last and
/// only where the session takes it; each is one the shell sends (nothing offered is refused
/// locally).
pub proof fn effort_candidates_are_the_ladder_and_auto(
    ladder: Seq<u64>,
    auto_id: u64,
    auto: AutoEffort,
    out: Seq<u64>,
)
    requires
        effort_candidates_spec(ladder, auto_id, auto, out),
    ensures
        out.contains(auto_id) <==> auto == AutoEffort::Taken,
        auto == AutoEffort::Taken ==> out.last() == auto_id,
        forall|i: int|
            0 <= i < ladder.len() && ladder[i] != auto_id ==> out.contains(#[trigger] ladder[i]),
        forall|i: int|
            0 <= i < out.len() ==> effort_sent_spec(
                ladder,
                auto,
                request_of(#[trigger] out[i], auto_id),
            ),
        out.no_duplicates(),
{
    distinct_levels_are_the_ladder_once(ladder, auto_id);
    let levels = distinct_levels(ladder, auto_id);
    let tail: Seq<u64> = if auto == AutoEffort::Taken {
        seq![auto_id]
    } else {
        Seq::empty()
    };
    assert(out =~= levels + tail);
    assert forall|id: u64|
        out.contains(id) <==> (levels.contains(id) || (auto == AutoEffort::Taken && id
            == auto_id)) by {
        if out.contains(id) {
            let k = choose|k: int| 0 <= k < out.len() && out[k] == id;
            if k < levels.len() {
                assert(levels[k] == id);
            } else {
                assert(tail[k - levels.len()] == id);
            }
        }
        if levels.contains(id) {
            let k = choose|k: int| 0 <= k < levels.len() && levels[k] == id;
            assert(out[k] == id);
        }
        if auto == AutoEffort::Taken && id == auto_id {
            assert(out[levels.len() as int] == id);
        }
    }
    assert forall|i: int, j: int| 0 <= i < out.len() && 0 <= j < out.len() && i != j implies out[i]
        != out[j] by {
        if i < levels.len() && j < levels.len() {
            assert(out[i] == levels[i] && out[j] == levels[j]);
        } else if i < levels.len() {
            assert(levels.contains(levels[i]));
        } else if j < levels.len() {
            assert(levels.contains(levels[j]));
        }
    }
    assert forall|i: int| 0 <= i < ladder.len() && ladder[i] != auto_id implies out.contains(
        #[trigger] ladder[i],
    ) by {
        assert(ladder.contains(ladder[i]));
        assert(levels.contains(ladder[i]));
    }
    assert forall|i: int| 0 <= i < out.len() implies effort_sent_spec(
        ladder,
        auto,
        request_of(#[trigger] out[i], auto_id),
    ) by {
        assert(out.contains(out[i]));
    }
}

fn contains(ids: &[u64], id: u64) -> (found: bool)
    ensures
        found == ids@.contains(id),
{
    for i in 0..ids.len()
        invariant
            forall|j: int| 0 <= j < i ==> ids@[j] != id,
    {
        if ids[i] == id {
            assert(ids@[i as int] == id);
            return true;
        }
    }
    false
}

proof fn lemma_push_fresh(ids: Seq<u64>, item: u64)
    requires
        ids.no_duplicates(),
        !ids.contains(item),
    ensures
        ids.push(item).no_duplicates(),
        forall|id: u64|
            #![trigger ids.push(item).contains(id)]
            #![trigger ids.contains(id)]
            ids.push(item).contains(id) <==> (ids.contains(id) || id == item),
{
    let pushed = ids.push(item);
    assert forall|i: int, j: int|
        0 <= i < pushed.len() && 0 <= j < pushed.len() && i != j implies pushed[i] != pushed[j] by {
        if i < ids.len() && j < ids.len() {
            assert(pushed[i] == ids[i] && pushed[j] == ids[j]);
        } else if i < ids.len() {
            assert(pushed[i] == ids[i]);
            assert(ids.contains(ids[i]));
        } else {
            assert(pushed[j] == ids[j]);
            assert(ids.contains(ids[j]));
        }
    }
    assert forall|id: u64| pushed.contains(id) <==> (ids.contains(id) || id == item) by {
        if pushed.contains(id) {
            let k = choose|k: int| 0 <= k < pushed.len() && pushed[k] == id;
            if k < ids.len() {
                assert(ids[k] == id);
            }
        }
        if ids.contains(id) {
            let k = choose|k: int| 0 <= k < ids.len() && ids[k] == id;
            assert(pushed[k] == id);
        }
        if id == item {
            assert(pushed[ids.len() as int] == id);
        }
    }
}

/// Whether the shell sends `request` for a model whose ladder is `ladder`, to a session whose
/// `auto` support is `auto`.
#[must_use]
pub fn effort_sent(ladder: &[u64], auto: AutoEffort, request: EffortRequest) -> (sent: bool)
    ensures
        sent == effort_sent_spec(ladder@, auto, request),
{
    match request {
        EffortRequest::Auto => !matches!(auto, AutoEffort::Refused),
        EffortRequest::Level(id) => ladder.is_empty() || contains(ladder, id),
    }
}

/// The efforts to offer: the ladder's levels in order, each once, then `auto` when the session
/// takes it.
#[must_use]
pub fn effort_candidates(ladder: &[u64], auto_id: u64, auto: AutoEffort) -> (out: Vec<u64>)
    ensures
        effort_candidates_spec(ladder@, auto_id, auto, out@),
{
    let mut out: Vec<u64> = Vec::new();
    proof {
        assert(ladder@.subrange(0, 0) =~= Seq::<u64>::empty());
    }
    for i in 0..ladder.len()
        invariant
            out@ == distinct_levels(ladder@.subrange(0, i as int), auto_id),
    {
        let id = ladder[i];
        let ghost before = out@;
        let found = contains(&out, id);
        if id != auto_id && !found {
            out.push(id);
        }
        proof {
            let sub = ladder@.subrange(0, i + 1);
            assert(sub.drop_last() =~= ladder@.subrange(0, i as int));
            assert(sub.last() == id);
        }
    }
    proof {
        assert(ladder@.subrange(0, ladder@.len() as int) =~= ladder@);
        distinct_levels_are_the_ladder_once(ladder@, auto_id);
    }
    let ghost levels = out@;
    if matches!(auto, AutoEffort::Taken) {
        out.push(auto_id);
        proof {
            assert(out@ =~= levels + seq![auto_id]);
        }
    } else {
        proof {
            assert(out@ =~= levels + Seq::<u64>::empty());
        }
    }
    out
}

} // verus!
