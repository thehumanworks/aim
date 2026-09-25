//! Pure scope decisions for both admission and execution (docs/adr/0026).
//!
//! Paths use the normalized segment representation and `valid_name` predicate from `path`.
//! The shell must still enforce filesystem confinement and symlink safety at the I/O boundary.
#[cfg(verus_only)]
use crate::path::{names_view, valid_name};
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// A class of operation checked at a normalized path. `Exec` uses the process cwd.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Inspect files or metadata.
    Read,
    /// Mutate files or metadata.
    Write,
    /// Spawn or control a process.
    Exec,
}

/// Small operation bitmask: bit 0 = read, bit 1 = write, bit 2 = exec.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpSet {
    bits: u8,
}

/// Whether a bitmask contains an operation.
pub open spec fn has_op(bits: u8, op: Op) -> bool {
    match op {
        Op::Read => bits % 2 == 1,
        Op::Write => bits % 4 >= 2,
        Op::Exec => bits % 8 >= 4,
    }
}

impl View for OpSet {
    type V = Set<Op>;

    closed spec fn view(&self) -> Set<Op> {
        let s = Set::<Op>::empty();
        let s = if has_op(self.bits, Op::Read) {
            s.insert(Op::Read)
        } else {
            s
        };
        let s = if has_op(self.bits, Op::Write) {
            s.insert(Op::Write)
        } else {
            s
        };
        if has_op(self.bits, Op::Exec) {
            s.insert(Op::Exec)
        } else {
            s
        }
    }
}

impl OpSet {
    /// A mask from three operation flags.
    #[must_use]
    #[expect(clippy::cast_lossless, reason = "Verus proves the bounds of boolean-to-bitmask casts")]
    pub fn new(read: bool, write: bool, exec: bool) -> (out: Self)
        ensures
            out@.contains(Op::Read) == read,
            out@.contains(Op::Write) == write,
            out@.contains(Op::Exec) == exec,
    {
        let bits: u8 = (read as u8) + (write as u8) * 2 + (exec as u8) * 4;
        Self { bits }
    }

    /// All three operation classes.
    #[must_use]
    pub fn all() -> (out: Self)
        ensures
            forall|op: Op| out@.contains(op),
    {
        Self::new(true, true, true)
    }

    /// No operation classes.
    #[must_use]
    pub fn none() -> (out: Self)
        ensures
            forall|op: Op| !out@.contains(op),
    {
        Self::new(false, false, false)
    }

    /// Whether the mask contains `op`.
    #[must_use]
    pub fn contains(&self, op: Op) -> (yes: bool)
        ensures
            yes == self@.contains(op),
    {
        match op {
            Op::Read => self.bits % 2 == 1,
            Op::Write => self.bits % 4 >= 2,
            Op::Exec => self.bits % 8 >= 4,
        }
    }

    #[expect(clippy::trivially_copy_pass_by_ref, reason = "shared references make the Verus view contract explicit")]
    fn intersect(&self, other: &Self) -> (out: Self)
        ensures
            forall|op: Op| out@.contains(op) == (self@.contains(op) && other@.contains(op)),
    {
        Self::new(
            self.contains(Op::Read) && other.contains(Op::Read),
            self.contains(Op::Write) && other.contains(Op::Write),
            self.contains(Op::Exec) && other.contains(Op::Exec),
        )
    }
}

/// A normalized path, represented as the same sequence of byte segments used by `path::confined`.
/// An empty sequence denotes the filesystem root.
#[derive(Debug)]
pub struct Root {
    segments: Vec<Vec<u8>>,
}

impl View for Root {
    type V = Seq<Seq<u8>>;

    closed spec fn view(&self) -> Seq<Seq<u8>> {
        names_view(self.segments@)
    }
}

/// A normalized segment satisfies the path model's name rule and is not `.` or `..`.
pub open spec fn normalized_name(n: Seq<u8>) -> bool {
    &&& valid_name(n)
    &&& !(n.len() == 1 && n[0] == 46u8)
    &&& !(n.len() == 2 && n[0] == 46u8 && n[1] == 46u8)
}

/// All segments of a normalized path are normalized names.
pub open spec fn normalized_path(path: Seq<Seq<u8>>) -> bool {
    forall|i: int| 0 <= i < path.len() ==> #[trigger] normalized_name(path[i])
}

fn name_is_valid(n: &[u8]) -> (yes: bool)
    ensures
        yes == valid_name(n@),
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
            return false;
        }
    }
    true
}

fn bytes_eq(a: &[u8], b: &[u8]) -> (yes: bool)
    ensures
        yes == (a@ == b@),
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

/// `root` is a segment prefix of `path`.
pub open spec fn under(path: Seq<Seq<u8>>, root: Seq<Seq<u8>>) -> bool {
    root.len() <= path.len() && path.subrange(0, root.len() as int) == root
}

/// Prefix inclusion is transitive.
pub proof fn lemma_under_transitive(path: Seq<Seq<u8>>, middle: Seq<Seq<u8>>, root: Seq<Seq<u8>>)
    requires
        under(path, middle),
        under(middle, root),
    ensures
        under(path, root),
{
    assert(path.subrange(0, root.len() as int) =~= root);
}

/// Two roots containing one path are themselves comparable by prefix.
pub proof fn lemma_under_comparable(path: Seq<Seq<u8>>, a: Seq<Seq<u8>>, b: Seq<Seq<u8>>)
    requires
        under(path, a),
        under(path, b),
    ensures
        under(a, b) || under(b, a),
{
    if a.len() >= b.len() {
        assert(a.subrange(0, b.len() as int) =~= b);
    } else {
        assert(b.subrange(0, a.len() as int) =~= a);
    }
}

impl Root {
    /// Validates owned, already-normalized path segments. Invalid names are refused.
    #[must_use]
    pub fn new(segments: Vec<Vec<u8>>) -> (out: Option<Self>)
        ensures
            out matches Some(path) ==> path@ == names_view(segments@) && normalized_path(path@),
            out is None ==> !normalized_path(names_view(segments@)),
    {
        let ghost names = names_view(segments@);
        for i in 0..segments.len()
            invariant
                names == names_view(segments@),
                forall|j: int| 0 <= j < i ==> #[trigger] normalized_name(names[j]),
        {
            if !name_is_valid(&segments[i]) {
                proof {
                    assert(names[i as int] == segments@[i as int]@);
                    assert(!valid_name(names[i as int]));
                    assert(!normalized_name(names[i as int]));
                    assert(!normalized_path(names));
                }
                return None;
            }
            let n = &segments[i];
            if (n.len() == 1 && n[0] == 46) || (n.len() == 2 && n[0] == 46 && n[1] == 46) {
                proof {
                    assert(names[i as int] == n@);
                    assert(!normalized_name(names[i as int]));
                    assert(!normalized_path(names));
                }
                return None;
            }
            proof {
                assert(names[i as int] == n@);
                assert(normalized_name(names[i as int]));
            }
        }
        Some(Self { segments })
    }

    /// Whether this path is at or below `root`.
    #[must_use]
    pub fn under(&self, root: &Self) -> (yes: bool)
        ensures
            yes == under(self@, root@),
    {
        if root.segments.len() > self.segments.len() {
            return false;
        }
        for i in 0..root.segments.len()
            invariant
                root@.len() <= self@.len(),
                forall|j: int| 0 <= j < i ==> #[trigger] self@[j] == root@[j],
        {
            if !bytes_eq(&self.segments[i], &root.segments[i]) {
                proof {
                    let prefix = self@.subrange(0, root@.len() as int);
                    assert(prefix[i as int] == self@[i as int]);
                    assert(self@[i as int] == self.segments@[i as int]@);
                    assert(root@[i as int] == root.segments@[i as int]@);
                    assert(prefix != root@);
                }
                return false;
            }
        }
        proof {
            assert(self@.subrange(0, root@.len() as int) =~= root@);
        }
        true
    }

    fn copied(&self) -> (out: Self)
        ensures
            out@ == self@,
    {
        let mut segments: Vec<Vec<u8>> = Vec::with_capacity(self.segments.len());
        for i in 0..self.segments.len()
            invariant
                segments@.len() == i,
                forall|j: int| 0 <= j < i ==> #[trigger] segments@[j]@ == self.segments@[j]@,
        {
            segments.push(self.segments[i].clone());
        }
        let out = Self { segments };
        proof {
            assert(out@ =~= self@);
        }
        out
    }
}

fn common_root(a: &Root, b: &Root) -> (out: Option<Root>)
    ensures
        match out {
            Some(r) => forall|p: Seq<Seq<u8>>| #[trigger]
                under(p, r@) == (under(p, a@) && under(p, b@)),
            None => forall|p: Seq<Seq<u8>>| !(under(p, a@) && under(p, b@)),
        },
{
    if a.under(b) {
        let r = a.copied();
        proof {
            assert forall|p: Seq<Seq<u8>>| #[trigger]
                under(p, r@) == (under(p, a@) && under(p, b@)) by {
                if under(p, a@) {
                    lemma_under_transitive(p, a@, b@);
                }
            }
        }
        Some(r)
    } else if b.under(a) {
        let r = b.copied();
        proof {
            assert forall|p: Seq<Seq<u8>>| #[trigger]
                under(p, r@) == (under(p, a@) && under(p, b@)) by {
                if under(p, b@) {
                    lemma_under_transitive(p, b@, a@);
                }
            }
        }
        Some(r)
    } else {
        proof {
            assert forall|p: Seq<Seq<u8>>| !(under(p, a@) && under(p, b@)) by {
                if under(p, a@) && under(p, b@) {
                    lemma_under_comparable(p, a@, b@);
                }
            }
        }
        None
    }
}

/// Resource limits carried by one scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Maximum simultaneous processes.
    pub max_processes: u32,
    /// Maximum output bytes per request.
    pub max_output_bytes: u64,
}

/// Views a list of normalized roots as segment sequences.
pub open spec fn roots_view(roots: Seq<Root>) -> Seq<Seq<Seq<u8>>> {
    roots.map_values(|r: Root| r@)
}

/// The abstract scope used by the policy decision.
pub struct ScopeView {
    /// Allowed subtrees, interpreted as a union.
    pub roots: Seq<Seq<Seq<u8>>>,
    /// Allowed operation classes.
    pub ops: Set<Op>,
    /// Write-denied subtrees, overriding roots and operations.
    pub deny_write: Seq<Seq<Seq<u8>>>,
    /// Resource limits.
    pub limits: Limits,
}

/// A grant or ceiling. All fields are private; paths enter through `Root::new`.
#[derive(Debug)]
pub struct Scope {
    roots: Vec<Root>,
    ops: OpSet,
    deny_write: Vec<Root>,
    limits: Limits,
}

impl View for Scope {
    type V = ScopeView;

    closed spec fn view(&self) -> ScopeView {
        ScopeView {
            roots: roots_view(self.roots@),
            ops: self.ops@,
            deny_write: roots_view(self.deny_write@),
            limits: self.limits,
        }
    }
}

/// Whether a path lies under at least one of the roots.
pub open spec fn has_root(roots: Seq<Seq<Seq<u8>>>, path: Seq<Seq<u8>>) -> bool {
    exists|i: int| 0 <= i < roots.len() && #[trigger] under(path, roots[i])
}

/// Coverage by the cross product processed through row `i`, column `j`.
spec fn processed_pairs(
    a: Seq<Seq<Seq<u8>>>,
    b: Seq<Seq<Seq<u8>>>,
    i: int,
    j: int,
    path: Seq<Seq<u8>>,
) -> bool {
    exists|x: int, y: int|
        0 <= x < a.len() && 0 <= y < b.len() && (x < i || (x == i && y < j)) && #[trigger] under(
            path,
            a[x],
        ) && #[trigger] under(path, b[y])
}

proof fn lemma_pairs_step(
    a: Seq<Seq<Seq<u8>>>,
    b: Seq<Seq<Seq<u8>>>,
    i: int,
    j: int,
    path: Seq<Seq<u8>>,
)
    requires
        0 <= i < a.len(),
        0 <= j < b.len(),
    ensures
        processed_pairs(a, b, i, j + 1, path) == (processed_pairs(a, b, i, j, path) || (under(
            path,
            a[i],
        ) && under(path, b[j]))),
{
    if processed_pairs(a, b, i, j + 1, path) {
        let (x, y) = choose|x: int, y: int|
            0 <= x < a.len() && 0 <= y < b.len() && (x < i || (x == i && y < j + 1)) && under(
                path,
                a[x],
            ) && under(path, b[y]);
        if x == i && y == j {
        } else {
            assert(processed_pairs(a, b, i, j, path));
        }
    }
    if under(path, a[i]) && under(path, b[j]) {
        assert(processed_pairs(a, b, i, j + 1, path));
    }
    if processed_pairs(a, b, i, j, path) {
        let (x, y) = choose|x: int, y: int|
            0 <= x < a.len() && 0 <= y < b.len() && (x < i || (x == i && y < j)) && under(
                path,
                a[x],
            ) && under(path, b[y]);
        assert(processed_pairs(a, b, i, j + 1, path));
    }
}

proof fn lemma_pairs_row(a: Seq<Seq<Seq<u8>>>, b: Seq<Seq<Seq<u8>>>, i: int, path: Seq<Seq<u8>>)
    requires
        0 <= i < a.len(),
    ensures
        processed_pairs(a, b, i, b.len() as int, path) == processed_pairs(a, b, i + 1, 0, path),
{
}

proof fn lemma_pairs_complete(a: Seq<Seq<Seq<u8>>>, b: Seq<Seq<Seq<u8>>>, path: Seq<Seq<u8>>)
    ensures
        processed_pairs(a, b, a.len() as int, 0, path) == (has_root(a, path) && has_root(b, path)),
{
    if processed_pairs(a, b, a.len() as int, 0, path) {
        let (x, y) = choose|x: int, y: int|
            0 <= x < a.len() && 0 <= y < b.len() && x < a.len() && under(path, a[x]) && under(
                path,
                b[y],
            );
        assert(has_root(a, path));
        assert(has_root(b, path));
    }
    if has_root(a, path) && has_root(b, path) {
        let x = choose|x: int| 0 <= x < a.len() && under(path, a[x]);
        let y = choose|y: int| 0 <= y < b.len() && under(path, b[y]);
        assert(processed_pairs(a, b, a.len() as int, 0, path));
    }
}

proof fn lemma_has_root_push(roots: Seq<Seq<Seq<u8>>>, root: Seq<Seq<u8>>, path: Seq<Seq<u8>>)
    ensures
        has_root(roots.push(root), path) == (has_root(roots, path) || under(path, root)),
{
    if has_root(roots.push(root), path) {
        let i = choose|i: int| 0 <= i < roots.push(root).len() && under(path, roots.push(root)[i]);
        if i < roots.len() {
            assert(roots.push(root)[i] == roots[i]);
            assert(has_root(roots, path));
        } else {
            assert(i == roots.len());
            assert(roots.push(root)[i] == root);
        }
    }
    if has_root(roots, path) {
        let i = choose|i: int| 0 <= i < roots.len() && under(path, roots[i]);
        assert(roots.push(root)[i] == roots[i]);
        assert(has_root(roots.push(root), path));
    }
    if under(path, root) {
        assert(roots.push(root)[roots.len() as int] == root);
        assert(has_root(roots.push(root), path));
    }
}

fn intersect_roots(a: &[Root], b: &[Root]) -> (out: Vec<Root>)
    ensures
        forall|path: Seq<Seq<u8>>| #[trigger]
            has_root(roots_view(out@), path) == (has_root(roots_view(a@), path) && has_root(
                roots_view(b@),
                path,
            )),
{
    let ghost av = roots_view(a@);
    let ghost bv = roots_view(b@);
    let mut out: Vec<Root> = Vec::new();
    for i in 0..a.len()
        invariant
            av == roots_view(a@),
            bv == roots_view(b@),
            forall|path: Seq<Seq<u8>>| #[trigger]
                has_root(roots_view(out@), path) == processed_pairs(av, bv, i as int, 0, path),
    {
        for j in 0..b.len()
            invariant
                i < a.len(),
                av == roots_view(a@),
                bv == roots_view(b@),
                forall|path: Seq<Seq<u8>>| #[trigger]
                    has_root(roots_view(out@), path) == processed_pairs(
                        av,
                        bv,
                        i as int,
                        j as int,
                        path,
                    ),
        {
            let ghost old_roots = roots_view(out@);
            if let Some(root) = common_root(&a[i], &b[j]) {
                let ghost rv = root@;
                out.push(root);
                proof {
                    assert(roots_view(out@) =~= old_roots.push(rv));
                }
            }
            proof {
                assert forall|path: Seq<Seq<u8>>| #[trigger]
                    has_root(roots_view(out@), path) == processed_pairs(
                        av,
                        bv,
                        i as int,
                        j as int + 1,
                        path,
                    ) by {
                    lemma_pairs_step(av, bv, i as int, j as int, path);
                    if out@.len() > old_roots.len() {
                        lemma_has_root_push(
                            old_roots,
                            roots_view(out@)[old_roots.len() as int],
                            path,
                        );
                    }
                }
            }
        }
        proof {
            assert forall|path: Seq<Seq<u8>>| #[trigger]
                has_root(roots_view(out@), path) == processed_pairs(
                    av,
                    bv,
                    i as int + 1,
                    0,
                    path,
                ) by {
                lemma_pairs_row(av, bv, i as int, path);
            }
        }
    }
    proof {
        assert forall|path: Seq<Seq<u8>>| #[trigger]
            has_root(roots_view(out@), path) == (has_root(av, path) && has_root(bv, path)) by {
            lemma_pairs_complete(av, bv, path);
        }
    }
    out
}

spec fn prefix_covered(roots: Seq<Seq<Seq<u8>>>, n: int, path: Seq<Seq<u8>>) -> bool {
    exists|i: int| 0 <= i < n && #[trigger] under(path, roots[i])
}

proof fn lemma_prefix_step(roots: Seq<Seq<Seq<u8>>>, n: int, path: Seq<Seq<u8>>)
    requires
        0 <= n < roots.len(),
    ensures
        prefix_covered(roots, n + 1, path) == (prefix_covered(roots, n, path) || under(
            path,
            roots[n],
        )),
{
    if prefix_covered(roots, n + 1, path) {
        let i = choose|i: int| 0 <= i < n + 1 && under(path, roots[i]);
        if i < n {
            assert(prefix_covered(roots, n, path));
        }
    }
    if prefix_covered(roots, n, path) {
        let i = choose|i: int| 0 <= i < n && under(path, roots[i]);
        assert(prefix_covered(roots, n + 1, path));
    }
    if under(path, roots[n]) {
        assert(prefix_covered(roots, n + 1, path));
    }
}

fn concat_roots(a: &[Root], b: &[Root]) -> (out: Vec<Root>)
    ensures
        forall|path: Seq<Seq<u8>>| #[trigger]
            has_root(roots_view(out@), path) == (has_root(roots_view(a@), path) || has_root(
                roots_view(b@),
                path,
            )),
{
    let ghost av = roots_view(a@);
    let ghost bv = roots_view(b@);
    let mut out: Vec<Root> = Vec::new();
    for i in 0..a.len()
        invariant
            av == roots_view(a@),
            forall|path: Seq<Seq<u8>>| #[trigger]
                has_root(roots_view(out@), path) == prefix_covered(av, i as int, path),
    {
        let ghost old = roots_view(out@);
        let root = a[i].copied();
        let ghost rv = root@;
        out.push(root);
        proof {
            assert(roots_view(out@) =~= old.push(rv));
            assert forall|path: Seq<Seq<u8>>| #[trigger]
                has_root(roots_view(out@), path) == prefix_covered(av, i as int + 1, path) by {
                lemma_has_root_push(old, rv, path);
                lemma_prefix_step(av, i as int, path);
            }
        }
    }
    for j in 0..b.len()
        invariant
            av == roots_view(a@),
            bv == roots_view(b@),
            forall|path: Seq<Seq<u8>>| #[trigger]
                has_root(roots_view(out@), path) == (has_root(av, path) || prefix_covered(
                    bv,
                    j as int,
                    path,
                )),
    {
        let ghost old = roots_view(out@);
        let root = b[j].copied();
        let ghost rv = root@;
        out.push(root);
        proof {
            assert(roots_view(out@) =~= old.push(rv));
            assert forall|path: Seq<Seq<u8>>| #[trigger]
                has_root(roots_view(out@), path) == (has_root(av, path) || prefix_covered(
                    bv,
                    j as int + 1,
                    path,
                )) by {
                lemma_has_root_push(old, rv, path);
                lemma_prefix_step(bv, j as int, path);
            }
        }
    }
    out
}

/// LOCKED(ADR-0026): roots and operations allow a call, except that protected subtrees deny writes.
pub open spec fn permits(scope: ScopeView, op: Op, path: Seq<Seq<u8>>) -> bool {
    &&& scope.ops.contains(op)
    &&& has_root(scope.roots, path)
    &&& (op != Op::Write || !has_root(scope.deny_write, path))
}

/// LOCKED(ADR-0026): a child grants no path or operation the parent denies, and has no larger limit.
pub open spec fn narrows(child: ScopeView, parent: ScopeView) -> bool {
    &&& child.limits.max_processes <= parent.limits.max_processes
    &&& child.limits.max_output_bytes <= parent.limits.max_output_bytes
    &&& forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
        permits(child, op, path) ==> permits(parent, op, path)
}

proof fn lemma_root_reflexive(path: Seq<Seq<u8>>)
    ensures
        under(path, path),
{
    assert(path.subrange(0, path.len() as int) =~= path);
}

proof fn lemma_has_root_lift(roots: Seq<Seq<Seq<u8>>>, shorter: Seq<Seq<u8>>, longer: Seq<Seq<u8>>)
    requires
        under(longer, shorter),
        has_root(roots, shorter),
    ensures
        has_root(roots, longer),
{
    let i = choose|i: int| 0 <= i < roots.len() && under(shorter, roots[i]);
    lemma_under_transitive(longer, shorter, roots[i]);
    assert(has_root(roots, longer));
}

/// Every child root is a finite probe of authority inclusion for `op`.
pub open spec fn root_probes(child: ScopeView, parent: ScopeView, op: Op) -> bool {
    forall|i: int|
        0 <= i < child.roots.len() && #[trigger] permits(child, op, child.roots[i]) ==> permits(
            parent,
            op,
            child.roots[i],
        )
}

/// Parent write-deny roots are the other finite probes needed to decide exact inclusion.
pub open spec fn deny_probes(child: ScopeView, parent: ScopeView) -> bool {
    forall|i: int|
        0 <= i < parent.deny_write.len() && #[trigger] permits(
            child,
            Op::Write,
            parent.deny_write[i],
        ) ==> permits(parent, Op::Write, parent.deny_write[i])
}

/// Finite root and deny boundary probes decide containment of every path beneath them.
pub proof fn theorem_probes_complete(child: ScopeView, parent: ScopeView)
    requires
        root_probes(child, parent, Op::Read),
        root_probes(child, parent, Op::Write),
        root_probes(child, parent, Op::Exec),
        deny_probes(child, parent),
        child.limits.max_processes <= parent.limits.max_processes,
        child.limits.max_output_bytes <= parent.limits.max_output_bytes,
    ensures
        narrows(child, parent),
{
    assert forall|op: Op, path: Seq<Seq<u8>>| #[trigger] permits(child, op, path) implies permits(
        parent,
        op,
        path,
    ) by {
        let i = choose|i: int| 0 <= i < child.roots.len() && under(path, child.roots[i]);
        let root = child.roots[i];
        lemma_root_reflexive(root);
        assert(has_root(child.roots, root));
        if op == Op::Write && has_root(child.deny_write, root) {
            lemma_has_root_lift(child.deny_write, root, path);
            assert(false);
        }
        assert(permits(child, op, root));
        match op {
            Op::Read => {
                assert(root_probes(child, parent, Op::Read));
            },
            Op::Write => {
                assert(root_probes(child, parent, Op::Write));
            },
            Op::Exec => {
                assert(root_probes(child, parent, Op::Exec));
            },
        }
        assert(permits(parent, op, root));
        lemma_has_root_lift(parent.roots, root, path);
        if op == Op::Write && has_root(parent.deny_write, path) {
            let j = choose|j: int|
                0 <= j < parent.deny_write.len() && under(path, parent.deny_write[j]);
            let denied = parent.deny_write[j];
            lemma_under_comparable(path, root, denied);
            if under(root, denied) {
                lemma_has_root_lift(parent.deny_write, denied, root);
                assert(!permits(parent, Op::Write, root));
            } else {
                lemma_root_reflexive(denied);
                lemma_under_transitive(denied, root, root);
                assert(has_root(child.roots, denied));
                if has_root(child.deny_write, denied) {
                    lemma_has_root_lift(child.deny_write, denied, path);
                    assert(false);
                }
                assert(permits(child, Op::Write, denied));
                assert(deny_probes(child, parent));
                assert(permits(parent, Op::Write, denied));
                assert(has_root(parent.deny_write, denied));
                assert(false);
            }
        }
    }
}

proof fn lemma_narrows_requires_probes(child: ScopeView, parent: ScopeView)
    ensures
        narrows(child, parent) ==> root_probes(child, parent, Op::Read) && root_probes(
            child,
            parent,
            Op::Write,
        ) && root_probes(child, parent, Op::Exec) && deny_probes(child, parent),
{
}

/// A write under a protected subtree is refused even if a root and op allow it.
pub proof fn theorem_deny_overrides_allow(scope: ScopeView, path: Seq<Seq<u8>>)
    ensures
        has_root(scope.deny_write, path) ==> !permits(scope, Op::Write, path),
{
}

/// LOCKED(ADR-0026): an intersection permits exactly the overlap and takes each smaller limit.
pub open spec fn intersection_ok(a: ScopeView, b: ScopeView, out: ScopeView) -> bool {
    &&& forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
        permits(out, op, path) == (permits(a, op, path) && permits(b, op, path))
    &&& out.limits.max_processes == if a.limits.max_processes <= b.limits.max_processes {
        a.limits.max_processes
    } else {
        b.limits.max_processes
    }
    &&& out.limits.max_output_bytes == if a.limits.max_output_bytes <= b.limits.max_output_bytes {
        a.limits.max_output_bytes
    } else {
        b.limits.max_output_bytes
    }
}

/// Exact intersection grants no permission or limit beyond either operand.
pub proof fn theorem_intersection_narrows(a: ScopeView, b: ScopeView, out: ScopeView)
    requires
        forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
            permits(out, op, path) == (permits(a, op, path) && permits(b, op, path)),
        out.limits.max_processes == if a.limits.max_processes <= b.limits.max_processes {
            a.limits.max_processes
        } else {
            b.limits.max_processes
        },
        out.limits.max_output_bytes == if a.limits.max_output_bytes <= b.limits.max_output_bytes {
            a.limits.max_output_bytes
        } else {
            b.limits.max_output_bytes
        },
    ensures
        narrows(out, a),
        narrows(out, b),
{
}

/// Permissions shared by the first `n` scopes; an empty fold permits all paths and operations.
pub open spec fn shared_permit(scopes: Seq<Scope>, n: int, op: Op, path: Seq<Seq<u8>>) -> bool {
    forall|i: int| 0 <= i < n ==> #[trigger] permits(scopes[i]@, op, path)
}

/// Minimum process limit of the first `n` scopes, starting without a ceiling.
pub open spec fn min_processes(scopes: Seq<Scope>, n: int) -> u32
    decreases n,
{
    if n <= 0 {
        u32::MAX
    } else {
        let prior = min_processes(scopes, n - 1);
        if prior <= scopes[n - 1]@.limits.max_processes {
            prior
        } else {
            scopes[n - 1]@.limits.max_processes
        }
    }
}

/// Minimum output limit of the first `n` scopes, starting without a ceiling.
pub open spec fn min_output_bytes(scopes: Seq<Scope>, n: int) -> u64
    decreases n,
{
    if n <= 0 {
        u64::MAX
    } else {
        let prior = min_output_bytes(scopes, n - 1);
        if prior <= scopes[n - 1]@.limits.max_output_bytes {
            prior
        } else {
            scopes[n - 1]@.limits.max_output_bytes
        }
    }
}

proof fn lemma_shared_step(scopes: Seq<Scope>, n: int, op: Op, path: Seq<Seq<u8>>)
    requires
        0 <= n < scopes.len(),
    ensures
        shared_permit(scopes, n + 1, op, path) == (shared_permit(scopes, n, op, path) && permits(
            scopes[n]@,
            op,
            path,
        )),
{
    if !shared_permit(scopes, n + 1, op, path) && shared_permit(scopes, n, op, path) && permits(
        scopes[n]@,
        op,
        path,
    ) {
        let i = choose|i: int| 0 <= i < n + 1 && !permits(scopes[i]@, op, path);
        assert(false);
    }
}

/// LOCKED(ADR-0026): effective authority is the conjunction of all scope permissions and
/// the minimum of every resource limit; an empty fold is unrestricted.
pub open spec fn effective_ok(scopes: Seq<Scope>, out: ScopeView) -> bool {
    &&& forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
        permits(out, op, path) == shared_permit(scopes, scopes.len() as int, op, path)
    &&& out.limits.max_processes == min_processes(scopes, scopes.len() as int)
    &&& out.limits.max_output_bytes == min_output_bytes(scopes, scopes.len() as int)
}

/// Any effective fold is no broader than any one of its inputs.
pub proof fn theorem_effective_narrows(scopes: Seq<Scope>, out: ScopeView)
    requires
        forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
            permits(out, op, path) == shared_permit(scopes, scopes.len() as int, op, path),
        out.limits.max_processes == min_processes(scopes, scopes.len() as int),
        out.limits.max_output_bytes == min_output_bytes(scopes, scopes.len() as int),
    ensures
        forall|i: int| 0 <= i < scopes.len() ==> narrows(out, scopes[i]@),
{
    assert forall|i: int| 0 <= i < scopes.len() implies narrows(out, scopes[i]@) by {
        // Each minimum is at most each input term.
        lemma_min_processes_below(scopes, scopes.len() as int, i);
        lemma_min_output_below(scopes, scopes.len() as int, i);
        assert forall|op: Op, path: Seq<Seq<u8>>| #[trigger] permits(out, op, path) implies permits(
            scopes[i]@,
            op,
            path,
        ) by {}
    }
}

proof fn lemma_min_processes_below(scopes: Seq<Scope>, n: int, i: int)
    requires
        0 <= i < n <= scopes.len(),
    ensures
        min_processes(scopes, n) <= scopes[i].limits.max_processes,
    decreases n,
{
    if i < n - 1 {
        lemma_min_processes_below(scopes, n - 1, i);
    }
}

proof fn lemma_min_output_below(scopes: Seq<Scope>, n: int, i: int)
    requires
        0 <= i < n <= scopes.len(),
    ensures
        min_output_bytes(scopes, n) <= scopes[i].limits.max_output_bytes,
    decreases n,
{
    if i < n - 1 {
        lemma_min_output_below(scopes, n - 1, i);
    }
}

fn universal() -> (out: Scope)
    ensures
        forall|op: Op, path: Seq<Seq<u8>>| #[trigger] permits(out@, op, path),
        out@.limits.max_processes == u32::MAX,
        out@.limits.max_output_bytes == u64::MAX,
{
    let roots = alloc::vec![Root { segments: Vec::new() }];
    let out = Scope {
        roots,
        ops: OpSet::all(),
        deny_write: Vec::new(),
        limits: Limits { max_processes: u32::MAX, max_output_bytes: u64::MAX },
    };
    proof {
        assert forall|op: Op, path: Seq<Seq<u8>>| #[trigger] permits(out@, op, path) by {
            assert(path.subrange(0, 0) =~= Seq::<Seq<u8>>::empty());
            assert(under(path, Seq::<Seq<u8>>::empty()));
            assert(out@.roots.len() == 1);
            assert(out@.roots[0] =~= Seq::<Seq<u8>>::empty());
            assert(has_root(out@.roots, path));
        }
    }
    out
}

/// Intersects any number of scopes. The empty fold is the unrestricted scope.
#[must_use]
pub fn effective(scopes: &[Scope]) -> (out: Scope)
    ensures
        effective_ok(scopes@, out@),
        forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
            permits(out@, op, path) == shared_permit(scopes@, scopes.len() as int, op, path),
        out@.limits.max_processes == min_processes(scopes@, scopes.len() as int),
        out@.limits.max_output_bytes == min_output_bytes(scopes@, scopes.len() as int),
        forall|i: int| 0 <= i < scopes.len() ==> narrows(out@, scopes@[i]@),
{
    let mut out = universal();
    for i in 0..scopes.len()
        invariant
            forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
                permits(out@, op, path) == shared_permit(scopes@, i as int, op, path),
            out@.limits.max_processes == min_processes(scopes@, i as int),
            out@.limits.max_output_bytes == min_output_bytes(scopes@, i as int),
    {
        out = out.intersect(&scopes[i]);
        proof {
            assert forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
                permits(out@, op, path) == shared_permit(scopes@, i as int + 1, op, path) by {
                lemma_shared_step(scopes@, i as int, op, path);
            }
        }
    }
    proof {
        theorem_effective_narrows(scopes@, out@);
    }
    out
}

fn any_under(roots: &[Root], path: &Root) -> (yes: bool)
    ensures
        yes == has_root(roots_view(roots@), path@),
{
    let ghost views = roots_view(roots@);
    for i in 0..roots.len()
        invariant
            views == roots_view(roots@),
            forall|j: int| 0 <= j < i ==> !under(path@, views[j]),
    {
        if path.under(&roots[i]) {
            proof {
                assert(views[i as int] == roots@[i as int]@);
                assert(has_root(views, path@));
            }
            return true;
        }
        proof {
            assert(views[i as int] == roots@[i as int]@);
        }
    }
    false
}

impl Scope {
    /// Constructs a grant or ceiling from normalized path roots and a bitmask.
    #[must_use]
    pub fn new(roots: Vec<Root>, ops: OpSet, deny_write: Vec<Root>, limits: Limits) -> Self {
        Self { roots, ops, deny_write, limits }
    }

    /// Exact intersection: common root subtrees, common operations, unioned write denies,
    /// and the minimum of each resource limit.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> (out: Self)
        ensures
            intersection_ok(self@, other@, out@),
            forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
                permits(out@, op, path) == (permits(self@, op, path) && permits(other@, op, path)),
            out@.limits.max_processes == if self@.limits.max_processes
                <= other@.limits.max_processes {
                self@.limits.max_processes
            } else {
                other@.limits.max_processes
            },
            out@.limits.max_output_bytes == if self@.limits.max_output_bytes
                <= other@.limits.max_output_bytes {
                self@.limits.max_output_bytes
            } else {
                other@.limits.max_output_bytes
            },
            narrows(out@, self@),
            narrows(out@, other@),
    {
        let roots = intersect_roots(&self.roots, &other.roots);
        let ops = self.ops.intersect(&other.ops);
        let deny_write = concat_roots(&self.deny_write, &other.deny_write);
        let limits = Limits {
            max_processes: if self.limits.max_processes <= other.limits.max_processes {
                self.limits.max_processes
            } else {
                other.limits.max_processes
            },
            max_output_bytes: if self.limits.max_output_bytes <= other.limits.max_output_bytes {
                self.limits.max_output_bytes
            } else {
                other.limits.max_output_bytes
            },
        };
        let out = Self { roots, ops, deny_write, limits };
        proof {
            assert forall|op: Op, path: Seq<Seq<u8>>| #[trigger]
                permits(out@, op, path) == (permits(self@, op, path) && permits(
                    other@,
                    op,
                    path,
                )) by {
                if op == Op::Write {
                    theorem_deny_overrides_allow(out@, path);
                }
            }
            theorem_intersection_narrows(self@, other@, out@);
        }
        out
    }

    fn roots_probe(&self, parent: &Self, op: Op) -> (yes: bool)
        ensures
            yes == root_probes(self@, parent@, op),
    {
        for i in 0..self.roots.len()
            invariant
                forall|j: int|
                    0 <= j < i && #[trigger] permits(self@, op, self@.roots[j]) ==> permits(
                        parent@,
                        op,
                        self@.roots[j],
                    ),
        {
            if self.permits(op, &self.roots[i]) && !parent.permits(op, &self.roots[i]) {
                proof {
                    assert(self@.roots[i as int] == self.roots@[i as int]@);
                    assert(!root_probes(self@, parent@, op));
                }
                return false;
            }
        }
        true
    }

    fn denies_probe(&self, parent: &Self) -> (yes: bool)
        ensures
            yes == deny_probes(self@, parent@),
    {
        for i in 0..parent.deny_write.len()
            invariant
                forall|j: int|
                    0 <= j < i && #[trigger] permits(self@, Op::Write, parent@.deny_write[j])
                        ==> permits(parent@, Op::Write, parent@.deny_write[j]),
        {
            if self.permits(Op::Write, &parent.deny_write[i]) && !parent.permits(
                Op::Write,
                &parent.deny_write[i],
            ) {
                proof {
                    assert(parent@.deny_write[i as int] == parent.deny_write@[i as int]@);
                    assert(!deny_probes(self@, parent@));
                }
                return false;
            }
        }
        true
    }

    /// Whether every path and operation permitted by this scope is permitted by `parent`,
    /// with neither resource limit increased. This is an exact semantic check.
    #[must_use]
    pub fn narrows(&self, parent: &Self) -> (yes: bool)
        ensures
            yes == narrows(self@, parent@),
    {
        if self.limits.max_processes > parent.limits.max_processes || self.limits.max_output_bytes
            > parent.limits.max_output_bytes {
            return false;
        }
        proof {
            lemma_narrows_requires_probes(self@, parent@);
        }
        if !self.roots_probe(parent, Op::Read) {
            return false;
        }
        if !self.roots_probe(parent, Op::Write) {
            return false;
        }
        if !self.roots_probe(parent, Op::Exec) {
            return false;
        }
        if !self.denies_probe(parent) {
            return false;
        }
        proof {
            theorem_probes_complete(self@, parent@);
        }
        true
    }

    /// Limits on the grant.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether this scope permits an operation at a normalized path.
    #[must_use]
    pub fn permits(&self, op: Op, path: &Root) -> (yes: bool)
        ensures
            yes == permits(self@, op, path@),
    {
        if !self.ops.contains(op) || !any_under(&self.roots, path) {
            return false;
        }
        match op {
            Op::Write => !any_under(&self.deny_write, path),
            _ => true,
        }
    }
}

} // verus!
