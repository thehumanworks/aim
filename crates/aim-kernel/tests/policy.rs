//! Executable edge cases for the verified scope policy.
use aim_kernel::policy::{Limits, Op, OpSet, Root, Scope, effective};

#[expect(clippy::expect_used, reason = "all test fixtures use normalized literal paths")]
fn path(s: &str) -> Root {
    Root::new(s.split('/').filter(|part| !part.is_empty()).map(|part| part.as_bytes().to_vec()).collect())
        .expect("test path has valid normalized segments")
}

fn limits(processes: u32, output: u64) -> Limits {
    Limits { max_processes: processes, max_output_bytes: output }
}

fn scope(roots: &[&str], ops: OpSet, denies: &[&str], limits: Limits) -> Scope {
    Scope::new(roots.iter().map(|root| path(root)).collect(), ops, denies.iter().map(|root| path(root)).collect(), limits)
}

#[test]
fn nested_and_disjoint_roots() {
    let outer = scope(&["/workspace"], OpSet::all(), &[], limits(9, 900));
    let inner = scope(&["/workspace/project"], OpSet::all(), &[], limits(8, 800));
    let overlap = outer.intersect(&inner);
    assert!(overlap.permits(Op::Read, &path("/workspace/project/src")));
    assert!(!overlap.permits(Op::Read, &path("/workspace/other")));
    assert!(overlap.narrows(&outer));
    assert!(overlap.narrows(&inner));

    let elsewhere = scope(&["/elsewhere"], OpSet::all(), &[], limits(8, 800));
    let disjoint = outer.intersect(&elsewhere);
    assert!(!disjoint.permits(Op::Read, &path("/workspace/project")));
    assert!(!disjoint.permits(Op::Read, &path("/elsewhere")));
}

#[test]
fn denies_inside_and_outside_the_allowed_roots() {
    let grant = scope(&["/workspace"], OpSet::all(), &["/workspace/private", "/elsewhere"], limits(4, 400));
    assert!(grant.permits(Op::Read, &path("/workspace/private/file")));
    assert!(!grant.permits(Op::Write, &path("/workspace/private/file")));
    assert!(grant.permits(Op::Write, &path("/workspace/public/file")));
    assert!(!grant.permits(Op::Write, &path("/elsewhere/file")));
}

#[test]
fn empty_ops_deny_every_class() {
    let empty = scope(&["/workspace"], OpSet::none(), &[], limits(4, 400));
    for op in [Op::Read, Op::Write, Op::Exec] {
        assert!(!empty.permits(op, &path("/workspace")));
    }
}

#[test]
fn intersection_takes_both_limit_minima() {
    let a = scope(&["/workspace"], OpSet::all(), &[], limits(2, 900));
    let b = scope(&["/workspace"], OpSet::all(), &[], limits(8, 100));
    assert_eq!(a.intersect(&b).limits(), limits(2, 100));
}

#[test]
fn four_level_effective_grant_intersects_every_operand() {
    let scopes = [
        scope(&["/workspace"], OpSet::all(), &[], limits(20, 20_000)),
        scope(&["/workspace/project"], OpSet::all(), &["/workspace/project/secret"], limits(10, 10_000)),
        scope(&["/workspace/project/src"], OpSet::new(true, false, true), &[], limits(4, 4_000)),
        scope(&["/workspace/project/src"], OpSet::new(true, true, false), &[], limits(1, 1_000)),
    ];
    let grant = effective(&scopes);
    assert!(grant.permits(Op::Read, &path("/workspace/project/src/lib.rs")));
    assert!(!grant.permits(Op::Write, &path("/workspace/project/src/lib.rs")));
    assert!(!grant.permits(Op::Exec, &path("/workspace/project/src")));
    assert!(!grant.permits(Op::Read, &path("/workspace/project/tests")));
    assert_eq!(grant.limits(), limits(1, 1_000));
    for operand in &scopes {
        assert!(grant.narrows(operand));
    }
}

#[test]
fn delegation_cannot_widen() {
    let parent = scope(&["/workspace/project"], OpSet::new(true, false, false), &[], limits(2, 200));
    let requested = scope(&["/workspace"], OpSet::all(), &[], limits(20, 2_000));
    assert!(!requested.narrows(&parent));
    let child = requested.intersect(&parent);
    assert!(child.narrows(&parent));
    assert!(child.permits(Op::Read, &path("/workspace/project/a")));
    assert!(!child.permits(Op::Write, &path("/workspace/project/a")));
    assert!(!child.permits(Op::Exec, &path("/workspace/project")));
}

#[test]
fn deny_overrides_allow() {
    let protected = scope(&["/workspace"], OpSet::all(), &["/workspace/locked"], limits(10, 1_000));
    let broad = scope(&["/"], OpSet::all(), &[], limits(20, 2_000));
    let effective = protected.intersect(&broad);
    assert!(!effective.permits(Op::Write, &path("/workspace/locked/item")));
    assert!(effective.permits(Op::Read, &path("/workspace/locked/item")));
}

#[test]
fn narrowing_is_semantic_even_when_denies_empty_a_root() {
    let child = scope(&["/workspace"], OpSet::new(false, true, false), &["/workspace"], limits(1, 100));
    let parent = scope(&["/elsewhere"], OpSet::none(), &[], limits(2, 200));
    assert!(child.narrows(&parent));
    assert!(!child.permits(Op::Write, &path("/workspace/file")));
}

#[test]
fn parent_deny_boundary_exposes_a_widening_child() {
    let parent = scope(&["/workspace"], OpSet::all(), &["/workspace/secret"], limits(4, 400));
    let child = scope(&["/workspace"], OpSet::all(), &["/workspace/secret/sub"], limits(4, 400));
    assert!(!child.narrows(&parent));
    assert!(child.intersect(&parent).narrows(&parent));
}

#[test]
fn empty_fold_is_the_intersection_identity() {
    let grant = effective(&[]);
    for op in [Op::Read, Op::Write, Op::Exec] {
        assert!(grant.permits(op, &path("/anywhere")));
    }
    assert_eq!(grant.limits(), limits(u32::MAX, u64::MAX));
}

#[test]
fn exec_is_judged_at_the_cwd() {
    let grant = scope(&["/workspace/run"], OpSet::new(false, false, true), &[], limits(1, 100));
    assert!(grant.permits(Op::Exec, &path("/workspace/run")));
    assert!(!grant.permits(Op::Exec, &path("/workspace/other")));
}

#[test]
fn invalid_segments_cannot_become_roots() {
    assert!(Root::new(vec![Vec::new()]).is_none());
    assert!(Root::new(vec![b"a/b".to_vec()]).is_none());
    assert!(Root::new(vec![b"a\0b".to_vec()]).is_none());
    assert!(Root::new(vec![b".".to_vec()]).is_none());
    assert!(Root::new(vec![b"..".to_vec()]).is_none());
}
