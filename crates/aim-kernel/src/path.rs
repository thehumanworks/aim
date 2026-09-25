//! Workspace path confinement (docs/architecture.md §12, docs/adr/0008).
//!
//! The harness only ever touches paths that are *lexically* inside a workspace root. The shell
//! splits a user path at `/` into [`Segment`]s (empty segments dropped); this module decides where
//! it leads. Symlinks are invisible to a lexical check: backends must still refuse to follow a
//! link out of the root (docs/adr/0008) — that is conformance-tested, not proved here.
//!
//! The decision, stated as [`confined`]: a relative path is resolved against the root and may
//! never climb above it; an absolute path must resolve to the root or below it. The result is a
//! sequence of plain names (no `.`, `..`, `/` or NUL) that starts with the root.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// One segment of a path as the user wrote it.
#[derive(Clone, PartialEq, Eq, Debug)]
#[verifier::allow(autoderive_clone_without_spec)]
pub enum Segment {
    /// `.`
    Current,
    /// `..`
    Parent,
    /// Any other name (its bytes).
    Name(Vec<u8>),
}

/// Why a path was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathError {
    /// The path leads outside the workspace root.
    Escapes,
    /// A name is empty or contains `/` or NUL (the shell split it wrongly or the input is hostile).
    InvalidName,
}

/// Spec-level segment.
pub enum Seg {
    /// `.`
    Current,
    /// `..`
    Parent,
    /// A name.
    Name(Seq<u8>),
}

/// A name the filesystem can take as one path component.
pub open spec fn valid_name(n: Seq<u8>) -> bool {
    &&& n.len() > 0
    &&& forall|i: int| 0 <= i < n.len() ==> #[trigger] n[i] != 0u8 && n[i] != 47u8
}

/// Every name of a resolved path is valid.
pub open spec fn all_valid(p: Seq<Seq<u8>>) -> bool {
    forall|i: int| 0 <= i < p.len() ==> #[trigger] valid_name(p[i])
}

/// Resolves `segs` on top of `stack`, never popping below `floor` entries.
/// `None` = an attempt to climb above the floor or an invalid name.
pub open spec fn walk(stack: Seq<Seq<u8>>, segs: Seq<Seg>, floor: nat) -> Option<Seq<Seq<u8>>>
    decreases segs.len(),
{
    if segs.len() == 0 {
        Some(stack)
    } else {
        match segs[0] {
            Seg::Current => walk(stack, segs.drop_first(), floor),
            Seg::Parent => if stack.len() > floor {
                walk(stack.drop_last(), segs.drop_first(), floor)
            } else {
                None
            },
            Seg::Name(n) => if valid_name(n) {
                walk(stack.push(n), segs.drop_first(), floor)
            } else {
                None
            },
        }
    }
}

/// LOCKED(ADR-0008): where a path leads, confined to `root`. A relative path starts at the root and
/// may not climb above it; an absolute path starts at `/` and must end at the root or below it.
pub open spec fn confined(root: Seq<Seq<u8>>, segs: Seq<Seg>, absolute: bool) -> Option<
    Seq<Seq<u8>>,
> {
    if !all_valid(root) {
        None
    } else if absolute {
        match walk(Seq::empty(), segs, 0) {
            Some(p) => if p.len() >= root.len() && p.subrange(0, root.len() as int) == root {
                Some(p)
            } else {
                None
            },
            None => None,
        }
    } else {
        walk(root, segs, root.len())
    }
}

// ---------------------------------------------------------------------------------------------
// Theorems about the decision.
// ---------------------------------------------------------------------------------------------
/// Walking never goes below its floor, keeps the stack's first `floor` entries, and only adds
/// valid names.
pub proof fn lemma_walk_keeps_floor(stack: Seq<Seq<u8>>, segs: Seq<Seg>, floor: nat)
    requires
        floor <= stack.len(),
        all_valid(stack),
    ensures
        walk(stack, segs, floor) matches Some(p) ==> p.len() >= floor && p.subrange(0, floor as int)
            == stack.subrange(0, floor as int) && all_valid(p),
    decreases segs.len(),
{
    if segs.len() > 0 {
        match segs[0] {
            Seg::Current => lemma_walk_keeps_floor(stack, segs.drop_first(), floor),
            Seg::Parent => if stack.len() > floor {
                let next = stack.drop_last();
                assert(next.subrange(0, floor as int) == stack.subrange(0, floor as int));
                lemma_walk_keeps_floor(next, segs.drop_first(), floor);
            },
            Seg::Name(n) => if valid_name(n) {
                let next = stack.push(n);
                assert(next.subrange(0, floor as int) == stack.subrange(0, floor as int));
                assert forall|i: int| 0 <= i < next.len() implies #[trigger] valid_name(
                    next[i],
                ) by {
                    if i < stack.len() {
                        assert(next[i] == stack[i]);
                    }
                }
                lemma_walk_keeps_floor(next, segs.drop_first(), floor);
            },
        }
    }
}

/// A confined path always starts with the workspace root and holds only valid names.
pub proof fn theorem_confined_stays_inside(root: Seq<Seq<u8>>, segs: Seq<Seg>, absolute: bool)
    ensures
        confined(root, segs, absolute) matches Some(p) ==> p.len() >= root.len() && p.subrange(
            0,
            root.len() as int,
        ) == root && all_valid(p),
{
    if all_valid(root) {
        if absolute {
            lemma_walk_keeps_floor(Seq::empty(), segs, 0);
        } else {
            lemma_walk_keeps_floor(root, segs, root.len());
            assert(root.subrange(0, root.len() as int) == root);
        }
    }
}

/// Spec-level form of the executable segments.
pub open spec fn seg_view(s: Segment) -> Seg {
    match s {
        Segment::Current => Seg::Current,
        Segment::Parent => Seg::Parent,
        Segment::Name(n) => Seg::Name(n@),
    }
}

// ---------------------------------------------------------------------------------------------
// Executable implementation, proven to compute `confined`.
// ---------------------------------------------------------------------------------------------
fn name_is_valid(n: &[u8]) -> (b: bool)
    ensures
        b == valid_name(n@),
{
    if n.is_empty() {
        return false;
    }
    for k in 0..n.len()
        invariant
            n@.len() > 0,
            forall|i: int| 0 <= i < k ==> #[trigger] n@[i] != 0u8 && n@[i] != 47u8,
    {
        if n[k] == 0 || n[k] == 47 {
            proof {
                assert(!(n@[k as int] != 0u8 && n@[k as int] != 47u8));
            }
            return false;
        }
    }
    true
}

fn bytes_eq(a: &[u8], b: &[u8]) -> (r: bool)
    ensures
        r == (a@ == b@),
{
    if a.len() != b.len() {
        return false;
    }
    for k in 0..a.len()
        invariant
            a@.len() == b@.len(),
            forall|i: int| 0 <= i < k ==> #[trigger] a@[i] == b@[i],
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

/// View of a vector of names as a sequence of byte sequences.
pub open spec fn names_view(v: Seq<Vec<u8>>) -> Seq<Seq<u8>> {
    v.map_values(|n: Vec<u8>| n@)
}

fn copy_name(n: &[u8]) -> (out: Vec<u8>)
    ensures
        out@ == n@,
{
    let mut out: Vec<u8> = Vec::with_capacity(n.len());
    for k in 0..n.len()
        invariant
            out@.len() == k,
            k <= n@.len(),
            forall|i: int| 0 <= i < k ==> #[trigger] out@[i] == n@[i],
    {
        out.push(n[k]);
    }
    proof {
        assert(out@ =~= n@);
    }
    out
}

/// Resolves `segs` confined to `root` (a list of names). Total: every input gets an answer.
/// Most of its length is proof steps linking each exit to [`confined`].
///
/// # Errors
/// [`PathError::Escapes`] when the path leads outside the root, [`PathError::InvalidName`] when a
/// name (of the path or the root) is empty or contains `/` or NUL.
#[expect(clippy::too_many_lines, reason = "the body is mostly proof steps tying each exit to `confined`")]
pub fn confine(root: &[Vec<u8>], segs: &[Segment], absolute: bool) -> (r: Result<
    Vec<Vec<u8>>,
    PathError,
>)
    ensures
        match confined(names_view(root@), segs@.map_values(|s: Segment| seg_view(s)), absolute) {
            Some(p) => r matches Ok(v) && names_view(v@) == p,
            None => r is Err,
        },
{
    let ghost root_v = names_view(root@);
    let ghost segs_v = segs@.map_values(|s: Segment| seg_view(s));
    // The root itself must consist of valid names.
    for k in 0..root.len()
        invariant
            root_v == names_view(root@),
            forall|i: int| 0 <= i < k ==> #[trigger] valid_name(root_v[i]),
    {
        proof {
            assert(root_v[k as int] == root@[k as int]@);
        }
        if !name_is_valid(&root[k]) {
            proof {
                assert(!all_valid(root_v));
            }
            return Err(PathError::InvalidName);
        }
    }
    proof {
        assert(all_valid(root_v));
    }
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let floor: usize;
    if absolute {
        floor = 0;
        proof {
            assert(names_view(stack@) =~= Seq::<Seq<u8>>::empty());
        }
    } else {
        for k in 0..root.len()
            invariant
                stack@.len() == k,
                k <= root@.len(),
                forall|i: int| 0 <= i < k ==> #[trigger] stack@[i]@ == root@[i]@,
        {
            stack.push(copy_name(&root[k]));
        }
        floor = root.len();
        proof {
            assert(names_view(stack@) =~= root_v);
        }
    }
    let ghost start = names_view(stack@);
    proof {
        assert(segs_v.subrange(0, segs_v.len() as int) =~= segs_v);
        if absolute {
            assert(confined(root_v, segs_v, absolute) == (match walk(start, segs_v, 0) {
                Some(p) => if p.len() >= root_v.len() && p.subrange(0, root_v.len() as int)
                    == root_v {
                    Some(p)
                } else {
                    None
                },
                None => None,
            }));
        } else {
            assert(confined(root_v, segs_v, absolute) == walk(start, segs_v, floor as nat));
        }
    }
    for k in 0..segs.len()
        invariant
            floor <= stack@.len(),
            all_valid(root_v),
            root_v == names_view(root@),
            absolute ==> start == Seq::<Seq<u8>>::empty() && floor == 0,
            !absolute ==> start == root_v && floor as nat == root_v.len(),
            segs_v == segs@.map_values(|s: Segment| seg_view(s)),
            segs_v.len() == segs@.len(),
            walk(start, segs_v, floor as nat) == walk(
                names_view(stack@),
                segs_v.subrange(k as int, segs_v.len() as int),
                floor as nat,
            ),
    {
        let ghost rest = segs_v.subrange(k as int, segs_v.len() as int);
        let ghost before = names_view(stack@);
        proof {
            assert(rest.len() > 0);
            assert(rest.drop_first() =~= segs_v.subrange(k + 1, segs_v.len() as int));
            assert(rest[0] == seg_view(segs@[k as int]));
        }
        match &segs[k] {
            Segment::Current => {},
            Segment::Parent => {
                if stack.len() > floor {
                    stack.pop();
                    proof {
                        assert(names_view(stack@) =~= before.drop_last());
                    }
                } else {
                    proof {
                        assert(walk(before, rest, floor as nat) is None);
                    }
                    return Err(PathError::Escapes);
                }
            },
            Segment::Name(n) => {
                if !name_is_valid(n) {
                    proof {
                        assert(walk(before, rest, floor as nat) is None);
                    }
                    return Err(PathError::InvalidName);
                }
                stack.push(copy_name(n));
                proof {
                    assert(names_view(stack@) =~= before.push(n@));
                }
            },
        }
    }
    proof {
        assert(segs_v.subrange(segs_v.len() as int, segs_v.len() as int) =~= Seq::<Seg>::empty());
        assert(walk(start, segs_v, floor as nat) == Some(names_view(stack@)));
    }
    if absolute {
        if stack.len() < root.len() {
            return Err(PathError::Escapes);
        }
        for k in 0..root.len()
            invariant
                absolute,
                stack@.len() >= root@.len(),
                root_v == names_view(root@),
                all_valid(root_v),
                start == Seq::<Seq<u8>>::empty() && floor == 0,
                segs_v == segs@.map_values(|s: Segment| seg_view(s)),
                walk(start, segs_v, floor as nat) == Some(names_view(stack@)),
                forall|i: int| 0 <= i < k ==> #[trigger] stack@[i]@ == root@[i]@,
        {
            if !bytes_eq(&stack[k], &root[k]) {
                proof {
                    let prefix = names_view(stack@).subrange(0, root@.len() as int);
                    assert(prefix[k as int] == stack@[k as int]@);
                    assert(root_v[k as int] == root@[k as int]@);
                    assert(prefix != root_v);
                }
                return Err(PathError::Escapes);
            }
        }
        proof {
            assert(names_view(stack@).subrange(0, root@.len() as int) =~= root_v);
        }
    }
    Ok(stack)
}

} // verus!
