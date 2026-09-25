//! Resolving a requested configuration value against the values an agent advertises (ADR 0075).
//!
//! Status: DRAFT(ADR-0075). The shell (`aim-acp`, `config_options`) turns every advertised value
//! and the request into keys: shell-assigned `u64` ids for normalized byte strings (equal strings
//! share an id, distinct strings never do), a generation number and a variant id. Normalization
//! stays in the shell; this module only compares keys, so its proofs are about tiers and indices.
//!
//! A request matches an advertised value at one of four tiers, best first: the exact bytes; the
//! case-folded value or display name; a family the request names, with the same variant; a family
//! the request names when the request has no variant and the value has one. Only tiers down to a
//! caller-chosen depth are tried. The best tier with any match decides: one match resolves, two
//! or more are ambiguous (never a silent pick), none at all is unknown.
//!
//! Before the family tiers, the family must be unambiguous: a request whose words name two
//! advertised families, or that names no generation while the values of its family span two
//! generations, is a conflict and resolves to nothing at a family tier.
use vstd::prelude::*;

verus! {

/// How closely an advertised value matches a request, best first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// The same bytes.
    Exact,
    /// The same case-folded value, or the request equals the case-folded display name.
    Folded,
    /// The request names the value's family, and both have the same variant (or neither has one).
    Family,
    /// The request names the value's family and no variant; the value has a variant.
    FamilyAnyVariant,
}

/// The keys of one advertised value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Candidate {
    /// Id of the value's exact bytes.
    pub value: u64,
    /// Id of the case-folded value.
    pub folded: u64,
    /// Id of the case-folded display name, if the value has one.
    pub name: Option<u64>,
    /// Id of the value's family word, if it has one.
    pub family: Option<u64>,
    /// The value's generation, if it has one.
    pub generation: Option<u64>,
    /// Id of the value's variant (for example a context size), if it has one.
    pub variant: Option<u64>,
}

/// The keys of a request. The ids of the words it contains are passed beside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    /// Id of the request's exact bytes.
    pub value: u64,
    /// Id of the case-folded request.
    pub folded: u64,
    /// The generation the request names, if any.
    pub generation: Option<u64>,
    /// Id of the variant the request names, if any.
    pub variant: Option<u64>,
}

/// A request resolved to one advertised value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resolved {
    /// Index of the value among the advertised values.
    pub index: usize,
    /// The tier it matched at.
    pub tier: Tier,
}

/// Why a request did not resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unresolved {
    /// No advertised value matches at any tier tried.
    Unknown,
    /// Two or more advertised values match at the best tier with a match.
    Ambiguous {
        /// The tier of the tie.
        tier: Tier,
        /// Index of the first value in the tie.
        first: usize,
        /// Index of the second value in the tie.
        second: usize,
    },
    /// Nothing matches at a better tier, and the family tiers do not apply: the request names two
    /// advertised families, or names no generation while its family comes in two.
    Conflict {
        /// Index of the first conflicting value.
        first: usize,
        /// Index of the second conflicting value.
        second: usize,
    },
}

/// The first two matches among a prefix of the advertised values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// No match.
    None,
    /// Exactly one match, at this index.
    One(usize),
    /// Two or more matches; the first two indices.
    Two(usize, usize),
}

/// The order of the tiers: 0 is the best.
pub open spec fn rank(t: Tier) -> nat {
    match t {
        Tier::Exact => 0,
        Tier::Folded => 1,
        Tier::Family => 2,
        Tier::FamilyAnyVariant => 3,
    }
}

/// The tier of a rank; ranks above 3 name the last tier.
pub open spec fn tier_at(k: nat) -> Tier {
    if k == 0 {
        Tier::Exact
    } else if k == 1 {
        Tier::Folded
    } else if k == 2 {
        Tier::Family
    } else {
        Tier::FamilyAnyVariant
    }
}

/// Whether the request's words include `id`.
pub open spec fn has_word(words: Seq<u64>, id: u64) -> bool {
    exists|i: int| 0 <= i < words.len() && words[i] == id
}

/// DRAFT(ADR-0075): a request that names a generation accepts only values of that generation.
pub open spec fn generation_ok(r: Request, c: Candidate) -> bool {
    match r.generation {
        Some(g) => c.generation == Some(g),
        None => true,
    }
}

/// DRAFT(ADR-0075): the request names the value's family among its words.
pub open spec fn names_family(words: Seq<u64>, c: Candidate) -> bool {
    match c.family {
        Some(f) => has_word(words, f),
        None => false,
    }
}

/// DRAFT(ADR-0075): whether advertised value `c` matches the request at tier `t`.
pub open spec fn matches_at(r: Request, words: Seq<u64>, c: Candidate, t: Tier) -> bool {
    match t {
        Tier::Exact => c.value == r.value,
        Tier::Folded => generation_ok(r, c) && (c.folded == r.folded || c.name == Some(r.folded)),
        Tier::Family => generation_ok(r, c) && names_family(words, c) && c.variant == r.variant,
        Tier::FamilyAnyVariant => generation_ok(r, c) && names_family(words, c) && r.variant is None
            && c.variant is Some,
    }
}

/// Whether `c` matches the request at either family tier.
pub open spec fn family_match(r: Request, words: Seq<u64>, c: Candidate) -> bool {
    matches_at(r, words, c, Tier::Family) || matches_at(r, words, c, Tier::FamilyAnyVariant)
}

/// DRAFT(ADR-0075): two advertised values that make a family request unclear: the request's
/// words name both of their (different) families, or the request names no generation and both
/// match it at a family tier with different generations.
pub open spec fn conflicts(r: Request, words: Seq<u64>, a: Candidate, b: Candidate) -> bool {
    (names_family(words, a) && names_family(words, b) && a.family != b.family) || (
    r.generation is None && family_match(r, words, a) && family_match(r, words, b) && a.generation
        != b.generation)
}

/// The first value before index `i` that conflicts with value `j`.
pub open spec fn partner(r: Request, words: Seq<u64>, cs: Seq<Candidate>, j: int, i: int) -> Option<
    usize,
>
    decreases i,
{
    if i <= 0 {
        None
    } else {
        match partner(r, words, cs, j, i - 1) {
            Some(p) => Some(p),
            None => if conflicts(r, words, cs[i - 1], cs[j]) {
                Some((i - 1) as usize)
            } else {
                None
            },
        }
    }
}

/// The first conflicting pair among the first `n` values, ordered by its second index.
pub open spec fn conflict(r: Request, words: Seq<u64>, cs: Seq<Candidate>, n: int) -> Option<
    (usize, usize),
>
    decreases n,
{
    if n <= 0 {
        None
    } else {
        match conflict(r, words, cs, n - 1) {
            Some(pair) => Some(pair),
            None => match partner(r, words, cs, n - 1, n - 1) {
                Some(p) => Some((p, (n - 1) as usize)),
                None => None,
            },
        }
    }
}

/// The first two values among the first `n` that match at tier `t`.
pub open spec fn found(r: Request, words: Seq<u64>, cs: Seq<Candidate>, t: Tier, n: int) -> Found
    decreases n,
{
    if n <= 0 {
        Found::None
    } else {
        let prev = found(r, words, cs, t, n - 1);
        if matches_at(r, words, cs[n - 1], t) {
            match prev {
                Found::None => Found::One((n - 1) as usize),
                Found::One(i) => Found::Two(i, (n - 1) as usize),
                Found::Two(i, j) => Found::Two(i, j),
            }
        } else {
            prev
        }
    }
}

/// DRAFT(ADR-0075): the tiers from rank `k` down to `deepest`, in order; the first tier with a
/// match decides. On reaching the family tiers, a conflicting pair decides first.
pub open spec fn resolve_from(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
    k: nat,
) -> Result<Resolved, Unresolved>
    decreases 4 - k,
{
    if k > rank(deepest) {
        Err(Unresolved::Unknown)
    } else {
        match (if k == 2 {
            conflict(r, words, cs, cs.len() as int)
        } else {
            None
        }) {
            Some((first, second)) => Err(Unresolved::Conflict { first, second }),
            None => match found(r, words, cs, tier_at(k), cs.len() as int) {
                Found::None => resolve_from(r, words, cs, deepest, k + 1),
                Found::One(i) => Ok(Resolved { index: i, tier: tier_at(k) }),
                Found::Two(i, j) => Err(
                    Unresolved::Ambiguous { tier: tier_at(k), first: i, second: j },
                ),
            },
        }
    }
}

/// DRAFT(ADR-0075): the resolution of a request against the advertised values `cs`, trying the
/// tiers from exact down to `deepest`.
pub open spec fn resolve_spec(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
) -> Result<Resolved, Unresolved> {
    resolve_from(r, words, cs, deepest, 0)
}

proof fn lemma_found(r: Request, words: Seq<u64>, cs: Seq<Candidate>, t: Tier, n: int)
    requires
        0 <= n <= cs.len(),
        cs.len() <= usize::MAX,
    ensures
        match found(r, words, cs, t, n) {
            Found::None => forall|k: int| 0 <= k < n ==> !matches_at(r, words, cs[k], t),
            Found::One(i) => 0 <= i < n && matches_at(r, words, cs[i as int], t) && forall|k: int|
                0 <= k < n && k != i ==> !matches_at(r, words, cs[k], t),
            Found::Two(i, j) => 0 <= i < j < n && matches_at(r, words, cs[i as int], t)
                && matches_at(r, words, cs[j as int], t),
        },
    decreases n,
{
    if n > 0 {
        lemma_found(r, words, cs, t, n - 1);
    }
}

proof fn lemma_partner(r: Request, words: Seq<u64>, cs: Seq<Candidate>, j: int, i: int)
    requires
        0 <= i <= j < cs.len(),
        cs.len() <= usize::MAX,
    ensures
        match partner(r, words, cs, j, i) {
            None => forall|p: int| 0 <= p < i ==> !#[trigger] conflicts(r, words, cs[p], cs[j]),
            Some(p) => 0 <= p < i && conflicts(r, words, cs[p as int], cs[j]),
        },
    decreases i,
{
    if i > 0 {
        lemma_partner(r, words, cs, j, i - 1);
    }
}

proof fn lemma_conflict(r: Request, words: Seq<u64>, cs: Seq<Candidate>, n: int)
    requires
        0 <= n <= cs.len(),
        cs.len() <= usize::MAX,
    ensures
        match conflict(r, words, cs, n) {
            None => forall|p: int, q: int|
                0 <= p < q < n ==> !#[trigger] conflicts(r, words, cs[p], cs[q]),
            Some((p, q)) => 0 <= p < q < n && conflicts(r, words, cs[p as int], cs[q as int]),
        },
    decreases n,
{
    if n > 0 {
        lemma_conflict(r, words, cs, n - 1);
        lemma_partner(r, words, cs, n - 1, n - 1);
        if conflict(r, words, cs, n) is None {
            assert forall|p: int, q: int| 0 <= p < q < n implies !#[trigger] conflicts(
                r,
                words,
                cs[p],
                cs[q],
            ) by {
                if q == n - 1 {
                    assert(partner(r, words, cs, n - 1, n - 1) is None);
                }
            }
        }
    }
}

proof fn lemma_partner_stable(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    j: int,
    m: int,
    n: int,
)
    requires
        0 <= m <= n,
        partner(r, words, cs, j, m) is Some,
    ensures
        partner(r, words, cs, j, n) == partner(r, words, cs, j, m),
    decreases n - m,
{
    if n > m {
        lemma_partner_stable(r, words, cs, j, m, n - 1);
    }
}

proof fn lemma_conflict_stable(r: Request, words: Seq<u64>, cs: Seq<Candidate>, m: int, n: int)
    requires
        0 <= m <= n,
        conflict(r, words, cs, m) is Some,
    ensures
        conflict(r, words, cs, n) == conflict(r, words, cs, m),
    decreases n - m,
{
    if n > m {
        lemma_conflict_stable(r, words, cs, m, n - 1);
    }
}

/// A conflicting pair anywhere in the list, in either order, means the scan finds one.
proof fn lemma_some_conflict(r: Request, words: Seq<u64>, cs: Seq<Candidate>, i: int, j: int)
    requires
        cs.len() <= usize::MAX,
        0 <= i < cs.len(),
        0 <= j < cs.len(),
        conflicts(r, words, cs[i], cs[j]),
    ensures
        conflict(r, words, cs, cs.len() as int) is Some,
{
    lemma_conflict(r, words, cs, cs.len() as int);
    assert(conflicts(r, words, cs[j], cs[i]));
    assert(i != j);
    if i < j {
        assert(conflicts(r, words, cs[i], cs[j]));
    } else {
        assert(conflicts(r, words, cs[j], cs[i]));
    }
}

proof fn lemma_ranks()
    ensures
        forall|t: Tier| #[trigger] tier_at(rank(t)) == t && rank(t) <= 3,
        forall|k: nat| k <= 3 ==> #[trigger] rank(tier_at(k)) == k,
{
}

proof fn lemma_resolve_from(r: Request, words: Seq<u64>, cs: Seq<Candidate>, deepest: Tier, k: nat)
    requires
        cs.len() <= usize::MAX,
    ensures
        match resolve_from(r, words, cs, deepest, k) {
            Ok(res) => {
                &&& k <= rank(res.tier) <= rank(deepest)
                &&& found(r, words, cs, res.tier, cs.len() as int) == Found::One(res.index)
                &&& forall|m: nat|
                    k <= m < rank(res.tier) ==> #[trigger] found(
                        r,
                        words,
                        cs,
                        tier_at(m),
                        cs.len() as int,
                    ) == Found::None
                &&& (k <= 2 <= rank(res.tier) ==> conflict(r, words, cs, cs.len() as int) is None)
            },
            Err(Unresolved::Ambiguous { tier, first, second }) => {
                &&& k <= rank(tier) <= rank(deepest)
                &&& found(r, words, cs, tier, cs.len() as int) == Found::Two(first, second)
                &&& forall|m: nat|
                    k <= m < rank(tier) ==> #[trigger] found(
                        r,
                        words,
                        cs,
                        tier_at(m),
                        cs.len() as int,
                    ) == Found::None
                &&& (k <= 2 <= rank(tier) ==> conflict(r, words, cs, cs.len() as int) is None)
            },
            Err(Unresolved::Conflict { first, second }) => {
                &&& k <= 2 <= rank(deepest)
                &&& conflict(r, words, cs, cs.len() as int) == Some((first, second))
                &&& forall|m: nat|
                    k <= m < 2 ==> #[trigger] found(r, words, cs, tier_at(m), cs.len() as int)
                        == Found::None
            },
            Err(Unresolved::Unknown) => {
                &&& forall|m: nat|
                    k <= m <= rank(deepest) ==> #[trigger] found(
                        r,
                        words,
                        cs,
                        tier_at(m),
                        cs.len() as int,
                    ) == Found::None
                &&& (k <= 2 <= rank(deepest) ==> conflict(r, words, cs, cs.len() as int) is None)
            },
        },
    decreases 4 - k,
{
    lemma_ranks();
    if k <= rank(deepest) {
        lemma_resolve_from(r, words, cs, deepest, k + 1);
    }
}

/// A resolved index is in bounds and matches at its tier, which is within `deepest`; it is the
/// only match at that tier, nothing matches at a better tier, and a family-tier answer has no
/// conflicting pair.
pub proof fn theorem_resolved_is_the_unique_best(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
)
    requires
        cs.len() <= usize::MAX,
    ensures
        match resolve_spec(r, words, cs, deepest) {
            Ok(res) => {
                &&& res.index < cs.len()
                &&& matches_at(r, words, cs[res.index as int], res.tier)
                &&& rank(res.tier) <= rank(deepest)
                &&& forall|k: int|
                    0 <= k < cs.len() && k != res.index ==> !matches_at(r, words, cs[k], res.tier)
                &&& forall|k: int, t: Tier|
                    0 <= k < cs.len() && rank(t) < rank(res.tier) ==> !matches_at(
                        r,
                        words,
                        cs[k],
                        t,
                    )
                &&& (rank(res.tier) >= 2 ==> forall|p: int, q: int|
                    0 <= p < q < cs.len() ==> !#[trigger] conflicts(r, words, cs[p], cs[q]))
            },
            _ => true,
        },
{
    lemma_ranks();
    lemma_resolve_from(r, words, cs, deepest, 0);
    lemma_conflict(r, words, cs, cs.len() as int);
    if let Ok(res) = resolve_spec(r, words, cs, deepest) {
        lemma_found(r, words, cs, res.tier, cs.len() as int);
        assert forall|k: int, t: Tier|
            0 <= k < cs.len() && rank(t) < rank(res.tier) implies !matches_at(
            r,
            words,
            cs[k],
            t,
        ) by {
            assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            lemma_found(r, words, cs, t, cs.len() as int);
        }
    }
}

/// An ambiguity is a real tie: two distinct in-bounds values match at its tier, which is within
/// `deepest`, and nothing matches at a better tier.
pub proof fn theorem_ambiguity_is_a_tie_at_the_best_tier(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
)
    requires
        cs.len() <= usize::MAX,
    ensures
        match resolve_spec(r, words, cs, deepest) {
            Err(Unresolved::Ambiguous { tier, first, second }) => {
                &&& first < second < cs.len()
                &&& matches_at(r, words, cs[first as int], tier)
                &&& matches_at(r, words, cs[second as int], tier)
                &&& rank(tier) <= rank(deepest)
                &&& forall|k: int, t: Tier|
                    0 <= k < cs.len() && rank(t) < rank(tier) ==> !matches_at(r, words, cs[k], t)
            },
            _ => true,
        },
{
    lemma_ranks();
    lemma_resolve_from(r, words, cs, deepest, 0);
    if let Err(Unresolved::Ambiguous { tier, first, second }) = resolve_spec(
        r,
        words,
        cs,
        deepest,
    ) {
        lemma_found(r, words, cs, tier, cs.len() as int);
        assert forall|k: int, t: Tier|
            0 <= k < cs.len() && rank(t) < rank(tier) implies !matches_at(r, words, cs[k], t) by {
            assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            lemma_found(r, words, cs, t, cs.len() as int);
        }
    }
}

/// A conflict is real: two distinct in-bounds values conflict for the request, the family tiers
/// were within `deepest`, and nothing matches at a better tier.
pub proof fn theorem_conflict_is_real(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
)
    requires
        cs.len() <= usize::MAX,
    ensures
        match resolve_spec(r, words, cs, deepest) {
            Err(Unresolved::Conflict { first, second }) => {
                &&& first < second < cs.len()
                &&& conflicts(r, words, cs[first as int], cs[second as int])
                &&& 2 <= rank(deepest)
                &&& forall|k: int, t: Tier|
                    0 <= k < cs.len() && rank(t) < 2 ==> !matches_at(r, words, cs[k], t)
            },
            _ => true,
        },
{
    lemma_ranks();
    lemma_resolve_from(r, words, cs, deepest, 0);
    lemma_conflict(r, words, cs, cs.len() as int);
    if let Err(Unresolved::Conflict { first, second }) = resolve_spec(r, words, cs, deepest) {
        assert forall|k: int, t: Tier| 0 <= k < cs.len() && rank(t) < 2 implies !matches_at(
            r,
            words,
            cs[k],
            t,
        ) by {
            assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            lemma_found(r, words, cs, t, cs.len() as int);
        }
    }
}

/// A request that names no generation never resolves at a family tier while two values it
/// matches there differ in generation (`opus` with both Opus 4.5 and Opus 5.5 offered).
pub proof fn theorem_unqualified_family_never_picks_a_generation(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
    i: int,
    j: int,
)
    requires
        cs.len() <= usize::MAX,
        0 <= i < cs.len(),
        0 <= j < cs.len(),
        r.generation is None,
        family_match(r, words, cs[i]),
        family_match(r, words, cs[j]),
        cs[i].generation != cs[j].generation,
    ensures
        match resolve_spec(r, words, cs, deepest) {
            Ok(res) => rank(res.tier) < 2,
            Err(Unresolved::Ambiguous { tier, .. }) => rank(tier) < 2,
            _ => true,
        },
{
    lemma_some_conflict(r, words, cs, i, j);
    lemma_resolve_from(r, words, cs, deepest, 0);
}

/// A request whose words name two different advertised families never resolves at a family
/// tier (`opus sonnet`).
pub proof fn theorem_two_families_never_resolve(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
    i: int,
    j: int,
)
    requires
        cs.len() <= usize::MAX,
        0 <= i < cs.len(),
        0 <= j < cs.len(),
        names_family(words, cs[i]),
        names_family(words, cs[j]),
        cs[i].family != cs[j].family,
    ensures
        match resolve_spec(r, words, cs, deepest) {
            Ok(res) => rank(res.tier) < 2,
            Err(Unresolved::Ambiguous { tier, .. }) => rank(tier) < 2,
            _ => true,
        },
{
    lemma_some_conflict(r, words, cs, i, j);
    lemma_resolve_from(r, words, cs, deepest, 0);
}

/// `Unknown` means no advertised value matches at any tier tried.
pub proof fn theorem_unknown_means_no_match(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
)
    requires
        cs.len() <= usize::MAX,
    ensures
        resolve_spec(r, words, cs, deepest) == Err::<Resolved, Unresolved>(Unresolved::Unknown)
            ==> forall|k: int, t: Tier|
            0 <= k < cs.len() && rank(t) <= rank(deepest) ==> !matches_at(r, words, cs[k], t),
{
    lemma_ranks();
    lemma_resolve_from(r, words, cs, deepest, 0);
    if resolve_spec(r, words, cs, deepest) == Err::<Resolved, Unresolved>(Unresolved::Unknown) {
        assert forall|k: int, t: Tier|
            0 <= k < cs.len() && rank(t) <= rank(deepest) implies !matches_at(
            r,
            words,
            cs[k],
            t,
        ) by {
            assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            lemma_found(r, words, cs, t, cs.len() as int);
        }
    }
}

/// Resolution depends only on which values match at which tier, not on their order: a value that
/// is the only match at the best tier with any match (within `deepest`) is the answer, provided
/// no pair conflicts when that tier is a family tier.
pub proof fn theorem_unique_best_is_resolved(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
    k: int,
    t: Tier,
)
    requires
        cs.len() <= usize::MAX,
        0 <= k < cs.len(),
        rank(t) <= rank(deepest),
        matches_at(r, words, cs[k], t),
        forall|j: int| 0 <= j < cs.len() && j != k ==> !matches_at(r, words, cs[j], t),
        forall|j: int, u: Tier|
            0 <= j < cs.len() && rank(u) < rank(t) ==> !matches_at(r, words, cs[j], u),
        rank(t) >= 2 ==> forall|p: int, q: int|
            0 <= p < q < cs.len() ==> !#[trigger] conflicts(r, words, cs[p], cs[q]),
    ensures
        resolve_spec(r, words, cs, deepest) == Ok::<Resolved, Unresolved>(
            Resolved { index: k as usize, tier: t },
        ),
{
    lemma_ranks();
    lemma_resolve_from(r, words, cs, deepest, 0);
    lemma_found(r, words, cs, t, cs.len() as int);
    lemma_conflict(r, words, cs, cs.len() as int);
    match resolve_spec(r, words, cs, deepest) {
        Ok(res) => {
            lemma_found(r, words, cs, res.tier, cs.len() as int);
            if rank(t) < rank(res.tier) {
                assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            }
            assert(rank(res.tier) == rank(t));
            assert(res.tier == t);
        },
        Err(Unresolved::Ambiguous { tier, first, second }) => {
            lemma_found(r, words, cs, tier, cs.len() as int);
            if rank(t) < rank(tier) {
                assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            }
            assert(rank(tier) == rank(t));
            assert(tier == t);
        },
        Err(Unresolved::Conflict { first, second }) => {
            if rank(t) < 2 {
                assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
            } else {
                assert(conflicts(r, words, cs[first as int], cs[second as int]));
            }
        },
        Err(Unresolved::Unknown) => {
            assert(found(r, words, cs, tier_at(rank(t)), cs.len() as int) == Found::None);
        },
    }
}

/// An exact match always wins: when some advertised value has the request's exact bytes,
/// resolution ends at the exact tier, and a unique exact value is the answer.
pub proof fn theorem_exact_wins(
    r: Request,
    words: Seq<u64>,
    cs: Seq<Candidate>,
    deepest: Tier,
    k: int,
)
    requires
        cs.len() <= usize::MAX,
        0 <= k < cs.len(),
        cs[k].value == r.value,
    ensures
        match resolve_spec(r, words, cs, deepest) {
            Ok(res) => res.tier == Tier::Exact && cs[res.index as int].value == r.value,
            Err(Unresolved::Ambiguous { tier, .. }) => tier == Tier::Exact,
            Err(Unresolved::Conflict { .. }) => false,
            Err(Unresolved::Unknown) => false,
        },
        (forall|j: int| 0 <= j < cs.len() && j != k ==> cs[j].value != r.value) ==> resolve_spec(
            r,
            words,
            cs,
            deepest,
        ) == Ok::<Resolved, Unresolved>(Resolved { index: k as usize, tier: Tier::Exact }),
{
    lemma_ranks();
    lemma_found(r, words, cs, Tier::Exact, cs.len() as int);
    theorem_resolved_is_the_unique_best(r, words, cs, deepest);
    theorem_ambiguity_is_a_tie_at_the_best_tier(r, words, cs, deepest);
    theorem_conflict_is_real(r, words, cs, deepest);
    theorem_unknown_means_no_match(r, words, cs, deepest);
    assert(matches_at(r, words, cs[k], Tier::Exact));
    if forall|j: int| 0 <= j < cs.len() && j != k ==> cs[j].value != r.value {
        theorem_unique_best_is_resolved(r, words, cs, deepest, k, Tier::Exact);
    }
}

/// A value whose generation differs from the one the request names never matches beyond the
/// exact tier, so it is never resolved to and never part of a tie.
pub proof fn theorem_generation_is_respected(r: Request, words: Seq<u64>, c: Candidate, t: Tier)
    ensures
        t != Tier::Exact && r.generation is Some && matches_at(r, words, c, t) ==> c.generation
            == r.generation,
{
}

/// A request that names a variant never matches a value of another variant at a family tier.
pub proof fn theorem_variant_is_respected(r: Request, words: Seq<u64>, c: Candidate, t: Tier)
    ensures
        (t == Tier::Family || t == Tier::FamilyAnyVariant) && r.variant is Some && matches_at(
            r,
            words,
            c,
            t,
        ) ==> c.variant == r.variant,
{
}

fn has_word_exec(words: &[u64], id: u64) -> (named: bool)
    ensures
        named == has_word(words@, id),
{
    let mut i: usize = 0;
    while i < words.len()
        invariant
            i <= words.len(),
            forall|j: int| 0 <= j < i ==> words@[j] != id,
        decreases words.len() - i,
    {
        if words[i] == id {
            return true;
        }
        i += 1;
    }
    false
}

fn same(a: Option<u64>, b: Option<u64>) -> (equal: bool)
    ensures
        equal == (a == b),
{
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        (None, None) => true,
        _ => false,
    }
}

fn generation_ok_exec(r: Request, c: Candidate) -> (ok: bool)
    ensures
        ok == generation_ok(r, c),
{
    match r.generation {
        Some(g) => same(c.generation, Some(g)),
        None => true,
    }
}

fn names_family_exec(words: &[u64], c: Candidate) -> (named: bool)
    ensures
        named == names_family(words@, c),
{
    match c.family {
        Some(f) => has_word_exec(words, f),
        None => false,
    }
}

fn matches(r: Request, words: &[u64], c: Candidate, t: Tier) -> (m: bool)
    ensures
        m == matches_at(r, words@, c, t),
{
    match t {
        Tier::Exact => c.value == r.value,
        Tier::Folded => generation_ok_exec(r, c) && (c.folded == r.folded || same(
            c.name,
            Some(r.folded),
        )),
        Tier::Family => generation_ok_exec(r, c) && names_family_exec(words, c) && same(
            c.variant,
            r.variant,
        ),
        Tier::FamilyAnyVariant => generation_ok_exec(r, c) && names_family_exec(words, c)
            && r.variant.is_none() && c.variant.is_some(),
    }
}

fn conflicts_exec(r: Request, words: &[u64], a: Candidate, b: Candidate) -> (c: bool)
    ensures
        c == conflicts(r, words@, a, b),
{
    let families = names_family_exec(words, a) && names_family_exec(words, b) && !same(
        a.family,
        b.family,
    );
    let a_family = matches(r, words, a, Tier::Family) || matches(
        r,
        words,
        a,
        Tier::FamilyAnyVariant,
    );
    let b_family = matches(r, words, b, Tier::Family) || matches(
        r,
        words,
        b,
        Tier::FamilyAnyVariant,
    );
    families || (r.generation.is_none() && a_family && b_family && !same(
        a.generation,
        b.generation,
    ))
}

fn find_conflict(r: Request, words: &[u64], cs: &[Candidate]) -> (out: Option<(usize, usize)>)
    ensures
        out == conflict(r, words@, cs@, cs@.len() as int),
{
    let mut j: usize = 0;
    while j < cs.len()
        invariant
            j <= cs.len(),
            conflict(r, words@, cs@, j as int) is None,
        decreases cs.len() - j,
    {
        let mut i: usize = 0;
        while i < j
            invariant
                i <= j < cs.len(),
                conflict(r, words@, cs@, j as int) is None,
                partner(r, words@, cs@, j as int, i as int) is None,
            decreases j - i,
        {
            if conflicts_exec(r, words, cs[i], cs[j]) {
                proof {
                    assert(partner(r, words@, cs@, j as int, i + 1) == Some(i));
                    lemma_partner_stable(r, words@, cs@, j as int, i + 1, j as int);
                    assert(conflict(r, words@, cs@, j + 1) == Some((i, j)));
                    lemma_conflict_stable(r, words@, cs@, j + 1, cs@.len() as int);
                }
                return Some((i, j));
            }
            i += 1;
        }
        j += 1;
    }
    None
}

fn scan(r: Request, words: &[u64], cs: &[Candidate], t: Tier) -> (f: Found)
    ensures
        f == found(r, words@, cs@, t, cs@.len() as int),
{
    let mut f = Found::None;
    let mut i: usize = 0;
    while i < cs.len()
        invariant
            i <= cs.len(),
            f == found(r, words@, cs@, t, i as int),
        decreases cs.len() - i,
    {
        if matches(r, words, cs[i], t) {
            f =
            match f {
                Found::None => Found::One(i),
                Found::One(first) => Found::Two(first, i),
                Found::Two(first, second) => Found::Two(first, second),
            };
        }
        i += 1;
    }
    f
}

fn rank_of(t: Tier) -> (k: u8)
    ensures
        k as nat == rank(t),
{
    match t {
        Tier::Exact => 0,
        Tier::Folded => 1,
        Tier::Family => 2,
        Tier::FamilyAnyVariant => 3,
    }
}

fn tier_of(k: u8) -> (t: Tier)
    ensures
        t == tier_at(k as nat),
{
    if k == 0 {
        Tier::Exact
    } else if k == 1 {
        Tier::Folded
    } else if k == 2 {
        Tier::Family
    } else {
        Tier::FamilyAnyVariant
    }
}

/// Resolves a request against the advertised values `candidates`, trying the tiers from exact
/// down to `deepest`. `words` are the ids of the words in the request.
///
/// # Errors
/// [`Unresolved::Unknown`] when nothing matches, [`Unresolved::Ambiguous`] when the best tier with
/// a match has more than one, [`Unresolved::Conflict`] when the family tiers are reached with a
/// conflicting pair.
pub fn resolve(request: Request, words: &[u64], candidates: &[Candidate], deepest: Tier) -> (out:
    Result<Resolved, Unresolved>)
    ensures
        out == resolve_spec(request, words@, candidates@, deepest),
{
    let last = rank_of(deepest);
    let mut k: u8 = 0;
    while k <= last
        invariant
            last as nat == rank(deepest),
            last <= 3,
            k <= last + 1,
            resolve_spec(request, words@, candidates@, deepest) == resolve_from(
                request,
                words@,
                candidates@,
                deepest,
                k as nat,
            ),
        decreases last + 1 - k,
    {
        let tier = tier_of(k);
        let conflict = if k == 2 {
            find_conflict(request, words, candidates)
        } else {
            None
        };
        if let Some((first, second)) = conflict {
            return Err(Unresolved::Conflict { first, second });
        }
        match scan(request, words, candidates, tier) {
            Found::None => {},
            Found::One(index) => {
                return Ok(Resolved { index, tier });
            },
            Found::Two(first, second) => {
                return Err(Unresolved::Ambiguous { tier, first, second });
            },
        }
        k += 1;
    }
    Err(Unresolved::Unknown)
}

} // verus!
#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(value: u64, family: Option<u64>, generation: Option<u64>, variant: Option<u64>) -> Candidate {
        Candidate { value, folded: value + 100, name: None, family, generation, variant }
    }

    #[test]
    fn tiers_are_tried_best_first_and_ties_are_ambiguous() {
        let opus_1m = candidate(1, Some(50), Some(5005), Some(60));
        let sonnet = candidate(2, Some(51), Some(5000), None);
        let request = Request { value: 9, folded: 109, generation: None, variant: None };
        // `opus` names family 50 only: the 1M variant is the one opus.
        assert_eq!(
            resolve(request, &[50], &[opus_1m, sonnet], Tier::FamilyAnyVariant),
            Ok(Resolved { index: 0, tier: Tier::FamilyAnyVariant })
        );
        // Not tried when the depth stops earlier.
        assert_eq!(resolve(request, &[50], &[opus_1m, sonnet], Tier::Folded), Err(Unresolved::Unknown));
        // A plain opus beside it wins at the better family tier.
        let opus = candidate(3, Some(50), Some(5005), None);
        assert_eq!(
            resolve(request, &[50], &[opus_1m, sonnet, opus], Tier::FamilyAnyVariant),
            Ok(Resolved { index: 2, tier: Tier::Family })
        );
        // Two plain opus values tie.
        assert_eq!(
            resolve(request, &[50], &[opus, sonnet, opus], Tier::FamilyAnyVariant),
            Err(Unresolved::Ambiguous { tier: Tier::Family, first: 0, second: 2 })
        );
    }

    #[test]
    fn unclear_families_conflict_before_the_family_tiers() {
        let opus_1m = candidate(1, Some(50), Some(5005), Some(60));
        let sonnet = candidate(2, Some(51), Some(5000), None);
        let old_opus = candidate(4, Some(50), Some(4005), None);
        let request = Request { value: 9, folded: 109, generation: None, variant: None };
        // `opus` while Opus 5.5 and Opus 4.5 are both offered: no silent pick of either.
        assert_eq!(
            resolve(request, &[50], &[opus_1m, sonnet, old_opus], Tier::FamilyAnyVariant),
            Err(Unresolved::Conflict { first: 0, second: 2 })
        );
        // Naming the generation settles it.
        let opus_5_5 = Request { generation: Some(5005), ..request };
        assert_eq!(
            resolve(opus_5_5, &[50], &[opus_1m, sonnet, old_opus], Tier::FamilyAnyVariant),
            Ok(Resolved { index: 0, tier: Tier::FamilyAnyVariant })
        );
        // `opus sonnet` names two families.
        assert_eq!(
            resolve(request, &[50, 51], &[opus_1m, sonnet], Tier::FamilyAnyVariant),
            Err(Unresolved::Conflict { first: 0, second: 1 })
        );
        // An exact value still wins over any conflict.
        let exact = Request { value: 2, ..request };
        assert_eq!(resolve(exact, &[50, 51], &[opus_1m, sonnet], Tier::FamilyAnyVariant), Ok(Resolved { index: 1, tier: Tier::Exact }));
    }
}
