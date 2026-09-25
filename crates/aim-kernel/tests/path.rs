//! Concrete confinement cases over the erased build (the proofs cover every input).
use aim_kernel::path::{PathError, Segment, confine};

fn segs(path: &str) -> Vec<Segment> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s {
            "." => Segment::Current,
            ".." => Segment::Parent,
            name => Segment::Name(name.as_bytes().to_vec()),
        })
        .collect()
}

fn root() -> Vec<Vec<u8>> {
    vec![b"home".to_vec(), b"me".to_vec(), b"repo".to_vec()]
}

fn show(r: Result<Vec<Vec<u8>>, PathError>) -> Result<String, PathError> {
    r.map(|v| format!("/{}", v.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>().join("/")))
}

#[test]
fn relative_paths_resolve_under_the_root_and_cannot_climb_out() {
    assert_eq!(show(confine(&root(), &segs("src/./lib.rs"), false)), Ok("/home/me/repo/src/lib.rs".into()));
    assert_eq!(show(confine(&root(), &segs("src/../Cargo.toml"), false)), Ok("/home/me/repo/Cargo.toml".into()));
    assert_eq!(show(confine(&root(), &segs(""), false)), Ok("/home/me/repo".into()));
    assert_eq!(show(confine(&root(), &segs(".."), false)), Err(PathError::Escapes));
    assert_eq!(show(confine(&root(), &segs("a/../../etc/passwd"), false)), Err(PathError::Escapes));
}

#[test]
fn absolute_paths_must_land_inside_the_root() {
    assert_eq!(show(confine(&root(), &segs("/home/me/repo/x"), true)), Ok("/home/me/repo/x".into()));
    assert_eq!(show(confine(&root(), &segs("/home/me/other/x"), true)), Err(PathError::Escapes));
    assert_eq!(show(confine(&root(), &segs("/home/me"), true)), Err(PathError::Escapes));
    assert_eq!(show(confine(&root(), &segs("/home/me/repo/../repo2"), true)), Err(PathError::Escapes));
    assert_eq!(show(confine(&root(), &segs("/.."), true)), Err(PathError::Escapes));
}

#[test]
fn hostile_names_are_refused() {
    let nul = vec![Segment::Name(b"a\0b".to_vec())];
    assert_eq!(confine(&root(), &nul, false), Err(PathError::InvalidName));
    let slash = vec![Segment::Name(b"a/b".to_vec())];
    assert_eq!(confine(&root(), &slash, false), Err(PathError::InvalidName));
    let empty = vec![Segment::Name(Vec::new())];
    assert_eq!(confine(&root(), &empty, false), Err(PathError::InvalidName));
}
