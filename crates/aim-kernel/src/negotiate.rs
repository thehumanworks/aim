//! Protocol-generation negotiation for the `aim-harness/1` and `aim-daemon/1` handshakes.
//!
//! Peers are compatible by *protocol generation*, not by exact build (docs/architecture.md §4.1):
//! each side advertises the inclusive range of generations it can speak, and both then speak the
//! single generation given by [`agreed`].
use vstd::prelude::*;

verus! {

/// Inclusive range of protocol generations that one peer can speak. Never empty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Generations {
    min: u32,
    max: u32,
}

/// Abstract view of a [`Generations`] range; all specifications are stated over it.
pub struct GenerationsView {
    /// Oldest generation the peer can speak.
    pub min: u32,
    /// Newest generation the peer can speak.
    pub max: u32,
}

impl View for Generations {
    type V = GenerationsView;

    closed spec fn view(&self) -> GenerationsView {
        GenerationsView { min: self.min, max: self.max }
    }
}

/// A peer advertising `r` can speak generation `g`.
pub open spec fn supports(r: GenerationsView, g: u32) -> bool {
    r.min <= g && g <= r.max
}

/// LOCKED(ADR-0005): two peers speak the newest generation that both support; when their ranges
/// do not overlap there is no common generation and the connection is refused.
pub open spec fn agreed(a: GenerationsView, b: GenerationsView) -> Option<u32> {
    let lo = if a.min >= b.min {
        a.min
    } else {
        b.min
    };
    let hi = if a.max <= b.max {
        a.max
    } else {
        b.max
    };
    if lo <= hi {
        Some(hi)
    } else {
        None
    }
}

/// Negotiation does not depend on which side initiates.
pub proof fn theorem_agreed_is_symmetric(a: GenerationsView, b: GenerationsView)
    ensures
        agreed(a, b) == agreed(b, a),
{
}

/// The agreed generation is spoken by both peers, and no newer generation is.
pub proof fn theorem_agreed_is_newest_common(a: GenerationsView, b: GenerationsView)
    ensures
        agreed(a, b) matches Some(g) ==> supports(a, g) && supports(b, g),
        agreed(a, b) matches Some(g) ==> forall|h: u32|
            #![trigger supports(a, h), supports(b, h)]
            supports(a, h) && supports(b, h) ==> h <= g,
{
}

/// Refusal happens exactly when the peers share no generation at all.
pub proof fn theorem_refusal_iff_disjoint(a: GenerationsView, b: GenerationsView)
    requires
        a.min <= a.max,
        b.min <= b.max,
    ensures
        agreed(a, b) is None <==> forall|h: u32|
            #![trigger supports(a, h), supports(b, h)]
            !(supports(a, h) && supports(b, h)),
{
    if agreed(a, b) is None {
        assert forall|h: u32| !(supports(a, h) && supports(b, h)) by {}
    } else {
        let g = agreed(a, b)->0;
        assert(supports(a, g) && supports(b, g));
    }
}

impl Generations {
    #[verifier::type_invariant]
    spec fn inv(self) -> bool {
        self.min <= self.max
    }

    /// Builds the range `min..=max`; `None` when it would be empty (`min > max`).
    #[must_use]
    pub const fn new(min: u32, max: u32) -> (r: Option<Self>)
        ensures
            r is None <==> min > max,
            r matches Some(g) ==> g@ == (GenerationsView { min, max }),
    {
        if min <= max {
            Some(Self { min, max })
        } else {
            None
        }
    }

    /// The range holding exactly one generation.
    #[must_use]
    pub const fn exactly(generation: u32) -> (r: Self)
        ensures
            r@ == (GenerationsView { min: generation, max: generation }),
    {
        Self { min: generation, max: generation }
    }

    /// Oldest supported generation.
    #[must_use]
    pub const fn min(&self) -> (m: u32)
        ensures
            m == self@.min,
    {
        self.min
    }

    /// Newest supported generation.
    #[must_use]
    pub const fn max(&self) -> (m: u32)
        ensures
            m == self@.max,
            self@.min <= self@.max,
    {
        proof {
            use_type_invariant(self);
        }
        self.max
    }
}

/// Negotiates the generation to speak: total, and exactly [`agreed`].
#[must_use]
pub const fn negotiate(ours: Generations, theirs: Generations) -> (r: Option<u32>)
    ensures
        r == agreed(ours@, theirs@),
{
    let lo = if ours.min >= theirs.min {
        ours.min
    } else {
        theirs.min
    };
    let hi = if ours.max <= theirs.max {
        ours.max
    } else {
        theirs.max
    };
    if lo <= hi {
        Some(hi)
    } else {
        None
    }
}

} // verus!
