//! Exact tool ceilings over host-assigned, collision-free name ids (ADR 0050).
//!
//! The shell builds one sorted union of all names for an operation and assigns each distinct
//! UTF-8 name a sequential `u64`. No hash is used; the reverse map reconstructs policy fields.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// A tool policy as name-id lists; duplicates do not affect membership.
pub struct ToolPolicyView {
    /// `None` admits every id before denials.
    pub allow: Option<Seq<u64>>,
    /// Ids always refused.
    pub deny: Seq<u64>,
}

/// Whether an id occurs in a list.
pub open spec fn has_id(ids: Seq<u64>, id: u64) -> bool {
    exists|i: int| 0 <= i < ids.len() && ids[i] == id
}

proof fn lemma_has_push(ids: Seq<u64>, item: u64, id: u64)
    ensures
        has_id(ids.push(item), id) <==> (has_id(ids, id) || item == id),
{
    if has_id(ids.push(item), id) {
        let i = choose|i: int| 0 <= i < ids.push(item).len() && ids.push(item)[i] == id;
        if i < ids.len() {
            assert(has_id(ids, id));
        } else {
            assert(item == id);
        }
    }
    if has_id(ids, id) {
        let i = choose|i: int| 0 <= i < ids.len() && ids[i] == id;
        assert(ids.push(item)[i] == id);
        assert(has_id(ids.push(item), id));
    }
    if item == id {
        assert(ids.push(item)[ids.len() as int] == id);
        assert(has_id(ids.push(item), id));
    }
}

proof fn lemma_has_concat(a: Seq<u64>, b: Seq<u64>, id: u64)
    ensures
        has_id(a + b, id) <==> (has_id(a, id) || has_id(b, id)),
{
    if has_id(a + b, id) {
        let i = choose|i: int| 0 <= i < (a + b).len() && (a + b)[i] == id;
        if i < a.len() {
            assert(has_id(a, id));
        } else {
            assert(has_id(b, id));
        }
    }
    if has_id(a, id) {
        let i = choose|i: int| 0 <= i < a.len() && a[i] == id;
        assert((a + b)[i] == id);
        assert(has_id(a + b, id));
    }
    if has_id(b, id) {
        let i = choose|i: int| 0 <= i < b.len() && b[i] == id;
        assert((a + b)[a.len() + i] == id);
        assert(has_id(a + b, id));
    }
}

/// LOCKED(ADR-0050): deny overrides both an absent and an explicit allowlist.
pub open spec fn tool_permits(policy: ToolPolicyView, id: u64) -> bool {
    !has_id(policy.deny, id) && (policy.allow is None || has_id(policy.allow->0, id))
}

/// LOCKED(ADR-0050): only a missing allowlist and empty denylist are unrestricted.
pub open spec fn unrestricted(policy: ToolPolicyView) -> bool {
    policy.allow is None && policy.deny.len() == 0
}

/// LOCKED(ADR-0050): intersection permits exactly what both input policies permit.
pub open spec fn tool_intersection_ok(
    a: ToolPolicyView,
    b: ToolPolicyView,
    out: ToolPolicyView,
) -> bool {
    forall|id: u64| tool_permits(out, id) <==> (tool_permits(a, id) && tool_permits(b, id))
}

/// An owned kernel tool ceiling.
#[derive(Debug)]
pub struct ToolPolicy {
    allow: Option<Vec<u64>>,
    deny: Vec<u64>,
}

impl View for ToolPolicy {
    type V = ToolPolicyView;

    closed spec fn view(&self) -> ToolPolicyView {
        ToolPolicyView {
            allow: match &self.allow {
                Some(ids) => Some(ids@),
                None => None,
            },
            deny: self.deny@,
        }
    }
}

fn contains(ids: &[u64], id: u64) -> (found: bool)
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

fn finite_intersection(candidates: &[u64], a: &ToolPolicy, b: &ToolPolicy) -> (out: Vec<u64>)
    ensures
        forall|id: u64|
            has_id(out@, id) <==> (has_id(candidates@, id) && tool_permits(a@, id) && tool_permits(
                b@,
                id,
            )),
{
    let mut out: Vec<u64> = Vec::new();
    for i in 0..candidates.len()
        invariant
            forall|id: u64|
                has_id(out@, id) <==> ((exists|j: int| 0 <= j < i && candidates@[j] == id)
                    && tool_permits(a@, id) && tool_permits(b@, id)),
    {
        let candidate = candidates[i];
        let keep = a.permits(candidate) && b.permits(candidate);
        let ghost before = out@;
        if keep {
            out.push(candidate);
        }
        proof {
            assert forall|id: u64|
                has_id(out@, id) <==> ((exists|j: int| 0 <= j < i + 1 && candidates@[j] == id)
                    && tool_permits(a@, id) && tool_permits(b@, id)) by {
                if keep {
                    lemma_has_push(before, candidate, id);
                }
                if candidate == id {
                    assert(keep == (tool_permits(a@, id) && tool_permits(b@, id)));
                }
            }
        }
    }
    proof {
        assert forall|id: u64|
            has_id(out@, id) <==> (has_id(candidates@, id) && tool_permits(a@, id) && tool_permits(
                b@,
                id,
            )) by {}
    }
    out
}

fn union_ids(a: &[u64], b: &[u64]) -> (out: Vec<u64>)
    ensures
        forall|id: u64| has_id(out@, id) <==> (has_id(a@, id) || has_id(b@, id)),
{
    let mut out = Vec::new();
    for i in 0..a.len()
        invariant
            out@ == a@.subrange(0, i as int),
    {
        out.push(a[i]);
        proof {
            assert(out@ =~= a@.subrange(0, (i + 1) as int));
        }
    }
    for i in 0..b.len()
        invariant
            out@ == a@ + b@.subrange(0, i as int),
    {
        out.push(b[i]);
        proof {
            assert(out@ =~= a@ + b@.subrange(0, (i + 1) as int));
        }
    }
    proof {
        assert(b@.subrange(0, b@.len() as int) =~= b@);
        assert(out@ =~= a@ + b@);
        assert forall|id: u64| has_id(out@, id) <==> (has_id(a@, id) || has_id(b@, id)) by {
            lemma_has_concat(a@, b@, id);
        }
    }
    out
}

impl ToolPolicy {
    /// Stores a policy expressed in one operation's host name registry.
    #[must_use]
    pub fn new(allow: Option<Vec<u64>>, deny: Vec<u64>) -> (out: Self)
        ensures
            out@.allow == (match allow {
                Some(ids) => Some(ids@),
                None => None,
            }),
            out@.deny == deny@,
    {
        Self { allow, deny }
    }

    /// Decides whether one registered name id is permitted.
    #[must_use]
    pub fn permits(&self, id: u64) -> (yes: bool)
        ensures
            yes == tool_permits(self@, id),
    {
        if contains(&self.deny, id) {
            return false;
        }
        match &self.allow {
            Some(ids) => contains(ids, id),
            None => true,
        }
    }

    /// Whether this policy excludes no names.
    #[must_use]
    pub fn is_unrestricted(&self) -> (yes: bool)
        ensures
            yes == unrestricted(self@),
    {
        self.allow.is_none() && self.deny.is_empty()
    }

    /// Narrows to the names permitted by both policies.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> (out: Self)
        ensures
            tool_intersection_ok(self@, other@, out@),
    {
        match (&self.allow, &other.allow) {
            (Some(candidates), _) | (_, Some(candidates)) => {
                let allow = finite_intersection(candidates, self, other);
                Self { allow: Some(allow), deny: Vec::new() }
            },
            (None, None) => Self { allow: None, deny: union_ids(&self.deny, &other.deny) },
        }
    }

    /// Returns id lists for reconstruction through the host's reverse registry.
    #[must_use]
    pub fn into_parts(self) -> (out: (Option<Vec<u64>>, Vec<u64>))
        ensures
            match out.0 {
                Some(ids) => Some(ids@),
                None => None,
            } == self@.allow,
            out.1@ == self@.deny,
    {
        (self.allow, self.deny)
    }
}

/// A named denial always wins.
pub proof fn theorem_deny_overrides(policy: ToolPolicyView, id: u64)
    requires
        has_id(policy.deny, id),
    ensures
        !tool_permits(policy, id),
{
}

/// Intersection is commutative in observable permission decisions.
pub proof fn theorem_intersection_commutative(
    a: ToolPolicyView,
    b: ToolPolicyView,
    ab: ToolPolicyView,
    ba: ToolPolicyView,
)
    requires
        tool_intersection_ok(a, b, ab),
        tool_intersection_ok(b, a, ba),
    ensures
        forall|id: u64| tool_permits(ab, id) == tool_permits(ba, id),
{
}

/// Intersection never widens either operand.
pub proof fn theorem_intersection_narrows(a: ToolPolicyView, b: ToolPolicyView, out: ToolPolicyView)
    requires
        tool_intersection_ok(a, b, out),
    ensures
        forall|id: u64| tool_permits(out, id) ==> tool_permits(a, id) && tool_permits(b, id),
{
}

} // verus!
