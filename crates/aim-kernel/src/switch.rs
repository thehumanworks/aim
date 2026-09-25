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

/// DRAFT(ADR-0074): whether the shell sends a requested effort for a model whose ladder is
/// `ladder`. `auto` always goes; a level goes when it is on the ladder. An empty ladder is not
/// known (options not arrived, or the model reports none): the backend decides, as before.
pub open spec fn effort_sent_spec(ladder: Seq<u64>, request: EffortRequest) -> bool {
    match request {
        EffortRequest::Auto => true,
        EffortRequest::Level(id) => ladder.len() == 0 || ladder.contains(id),
    }
}

/// With a known ladder, only `auto` or a level on it is sent.
pub proof fn effort_sent_only_if_offered(ladder: Seq<u64>, request: EffortRequest)
    ensures
        effort_sent_spec(ladder, request) && ladder.len() > 0 ==> match request {
            EffortRequest::Auto => true,
            EffortRequest::Level(id) => ladder.contains(id),
        },
        !effort_sent_spec(ladder, request) ==> ladder.len() > 0 && request is Level,
{
}

/// DRAFT(ADR-0074): the efforts offered for a ladder: each level once, and `auto`, nothing else.
pub open spec fn effort_candidates_spec(ladder: Seq<u64>, auto: u64, out: Seq<u64>) -> bool {
    out.no_duplicates() && forall|id: u64| out.contains(id) <==> (ladder.contains(id) || id == auto)
}

/// The candidates keep the ladder's order and put `auto` last (unless the ladder has it).
pub proof fn effort_candidates_are_the_ladder_and_auto(ladder: Seq<u64>, auto: u64, out: Seq<u64>)
    requires
        effort_candidates_spec(ladder, auto, out),
    ensures
        out.contains(auto),
        forall|i: int| 0 <= i < ladder.len() ==> out.contains(#[trigger] ladder[i]),
        forall|i: int| 0 <= i < out.len() ==> ladder.contains(#[trigger] out[i]) || out[i] == auto,
        forall|i: int, j: int|
            0 <= i < out.len() && 0 <= j < out.len() && i != j ==> out[i] != out[j],
{
    assert(ladder.contains(auto) || auto == auto);
    assert forall|i: int| 0 <= i < ladder.len() implies out.contains(#[trigger] ladder[i]) by {
        assert(ladder.contains(ladder[i]));
    }
    assert forall|i: int| 0 <= i < out.len() implies ladder.contains(#[trigger] out[i]) || out[i]
        == auto by {
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

/// Whether the shell sends `request` for a model whose ladder is `ladder`.
#[must_use]
pub fn effort_sent(ladder: &[u64], request: EffortRequest) -> (sent: bool)
    ensures
        sent == effort_sent_spec(ladder@, request),
{
    match request {
        EffortRequest::Auto => true,
        EffortRequest::Level(id) => ladder.is_empty() || contains(ladder, id),
    }
}

/// The efforts to offer: the ladder's levels in order, each once, then `auto` unless the ladder
/// already has it.
#[must_use]
pub fn effort_candidates(ladder: &[u64], auto: u64) -> (out: Vec<u64>)
    ensures
        effort_candidates_spec(ladder@, auto, out@),
{
    let mut out: Vec<u64> = Vec::new();
    for i in 0..ladder.len()
        invariant
            out@.no_duplicates(),
            forall|id: u64| out@.contains(id) <==> (exists|j: int| 0 <= j < i && ladder@[j] == id),
    {
        let id = ladder[i];
        let ghost before = out@;
        let found = contains(&out, id);
        if !found {
            out.push(id);
            proof {
                lemma_push_fresh(before, id);
            }
        }
        proof {
            assert forall|x: u64|
                out@.contains(x) <==> (exists|j: int| 0 <= j < i + 1 && ladder@[j] == x) by {
                // The loop invariant, at `x`, for the list before this step.
                assert(before.contains(x) <==> (exists|j: int| 0 <= j < i && ladder@[j] == x));
                if found {
                    assert(out@ == before);
                } else {
                    assert(out@.contains(x) <==> (before.contains(x) || x == id));
                }
                if x == id {
                    assert(ladder@[i as int] == x);
                }
                if exists|j: int| 0 <= j < i + 1 && ladder@[j] == x {
                    let j = choose|j: int| 0 <= j < i + 1 && ladder@[j] == x;
                    if j < i {
                        assert(exists|k: int| 0 <= k < i && ladder@[k] == x);
                    } else {
                        assert(x == id);
                    }
                }
                if exists|j: int| 0 <= j < i && ladder@[j] == x {
                    let j = choose|j: int| 0 <= j < i && ladder@[j] == x;
                    assert(0 <= j < i + 1 && ladder@[j] == x);
                }
            }
        }
    }
    proof {
        assert forall|id: u64| out@.contains(id) <==> ladder@.contains(id) by {
            if ladder@.contains(id) {
                let k = choose|k: int| 0 <= k < ladder@.len() && ladder@[k] == id;
                assert(exists|j: int| 0 <= j < ladder@.len() && ladder@[j] == id);
            }
        }
    }
    let ghost before = out@;
    if !contains(&out, auto) {
        out.push(auto);
        proof {
            lemma_push_fresh(before, auto);
        }
    }
    out
}

} // verus!
