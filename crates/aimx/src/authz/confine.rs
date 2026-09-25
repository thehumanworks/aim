//! Lexical path confinement (docs/adr/0008): the pure decision of which absolute path a
//! user-supplied path names inside a workspace root, or why it may not.
//!
//! Kernel candidate: this module does no I/O and uses only `&str`, so it can move into
//! `aim-kernel` with a Verus proof unchanged in meaning. Symlinks are invisible to it; the backend
//! resolves them afterwards and refuses real locations outside the root.
//!
//! Rules, for a root `R` and a user path `p`:
//! - `R` must be absolute; it is normalised first.
//! - `p` must not contain NUL.
//! - An empty `p` (or `.`) names `R` itself.
//! - A relative `p` is normalised on its own (`.` and empty segments dropped, `..` pops the
//!   previous segment); a `..` with nothing left to pop climbs above `R` → `denied`. The result is
//!   appended to `R`.
//! - An absolute `p` is normalised (`..` at `/` stays at `/`, as POSIX does) and must then be `R`
//!   or lie below it at a segment boundary (`/ab` is not inside `/a`) → otherwise `denied`.
//! - The result is normalised: absolute, no `.`/`..`/empty segments, no trailing `/` except for
//!   `/` itself.

use aim_proto::error::{ErrorCode, ProtoError};

/// Normalises an absolute POSIX path lexically. `None` when `path` is not absolute.
#[must_use]
pub fn normalize(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            name => segments.push(name),
        }
    }
    Some(join_absolute(&segments))
}

/// Normalises a relative path; `None` when a `..` climbs above its start.
fn normalize_relative(path: &str) -> Option<Vec<&str>> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            name => segments.push(name),
        }
    }
    Some(segments)
}

fn join_absolute(segments: &[&str]) -> String {
    let mut out = String::with_capacity(segments.iter().map(|s| s.len() + 1).sum::<usize>().max(1));
    for segment in segments {
        out.push('/');
        out.push_str(segment);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Whether the normalised absolute `path` is `base` or lies below it at a segment boundary.
#[must_use]
pub fn is_within(path: &str, base: &str) -> bool {
    if base == "/" {
        return path.starts_with('/');
    }
    path == base || path.strip_prefix(base).is_some_and(|rest| rest.starts_with('/'))
}

/// The path of `path` relative to `base` (`""` for `base` itself), when it lies within it.
#[must_use]
pub fn relative_to<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    if !is_within(path, base) {
        return None;
    }
    if base == "/" {
        return path.strip_prefix('/');
    }
    path.strip_prefix(base).map(|rest| rest.strip_prefix('/').unwrap_or(rest))
}

/// Confines `user_path` to `root`: the normalised absolute path it names, or `denied` when it
/// would leave the root (`invalid_params` for a relative root or a NUL byte).
///
/// # Errors
/// `denied` when the path escapes the root; `invalid_params` for malformed input.
pub fn confine(root: &str, user_path: &str) -> Result<String, ProtoError> {
    if user_path.contains('\0') || root.contains('\0') {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "path contains a NUL byte"));
    }
    let root = normalize(root).ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "workspace root must be absolute"))?;
    if user_path.starts_with('/') {
        let path = normalize(user_path).unwrap_or_default();
        if is_within(&path, &root) { Ok(path) } else { Err(denied(user_path)) }
    } else {
        let segments = normalize_relative(user_path).ok_or_else(|| denied(user_path))?;
        if segments.is_empty() {
            return Ok(root);
        }
        let mut out = if root == "/" { String::new() } else { root };
        for segment in segments {
            out.push('/');
            out.push_str(segment);
        }
        Ok(out)
    }
}

fn denied(user_path: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Denied, format!("path `{user_path}` is outside the workspace root"))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ok(root: &str, path: &str) -> String {
        confine(root, path).unwrap()
    }

    fn code(root: &str, path: &str) -> ErrorCode {
        confine(root, path).unwrap_err().code
    }

    #[test]
    fn examples() {
        assert_eq!(ok("/w", ""), "/w");
        assert_eq!(ok("/w", "."), "/w");
        assert_eq!(ok("/w", "a/b"), "/w/a/b");
        assert_eq!(ok("/w", "a//b/./c/"), "/w/a/b/c");
        assert_eq!(ok("/w", "a/../b"), "/w/b");
        assert_eq!(ok("/w/", "a"), "/w/a");
        assert_eq!(ok("//w//x/", "a"), "/w/x/a");
        assert_eq!(ok("/w", "/w"), "/w");
        assert_eq!(ok("/w", "/w/"), "/w");
        assert_eq!(ok("/w", "/w/a/../b"), "/w/b");
        assert_eq!(ok("/", "a"), "/a");
        assert_eq!(ok("/", "/a/../../b"), "/b");
        assert_eq!(ok("/", ""), "/");
        assert_eq!(ok("/w", "..."), "/w/...");
        assert_eq!(ok("/w", ".hidden"), "/w/.hidden");
    }

    #[test]
    fn escapes_are_denied() {
        assert_eq!(code("/w", ".."), ErrorCode::Denied);
        assert_eq!(code("/w", "../w/a"), ErrorCode::Denied);
        assert_eq!(code("/w", "a/../../x"), ErrorCode::Denied);
        assert_eq!(code("/w", "/etc/passwd"), ErrorCode::Denied);
        assert_eq!(code("/w", "/w/../x"), ErrorCode::Denied);
        assert_eq!(code("/w", "/wx"), ErrorCode::Denied);
        assert_eq!(code("/w", "/wx/a"), ErrorCode::Denied);
        assert_eq!(code("/w", "/"), ErrorCode::Denied);
    }

    #[test]
    fn malformed_input_is_invalid() {
        assert_eq!(code("w", "a"), ErrorCode::InvalidParams);
        assert_eq!(code("", "a"), ErrorCode::InvalidParams);
        assert_eq!(code("/w", "a\0b"), ErrorCode::InvalidParams);
    }

    #[test]
    fn relative_to_root() {
        assert_eq!(relative_to("/w/a/b", "/w"), Some("a/b"));
        assert_eq!(relative_to("/w", "/w"), Some(""));
        assert_eq!(relative_to("/wx", "/w"), None);
        assert_eq!(relative_to("/a", "/"), Some("a"));
    }

    fn segment() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            Just(".".to_owned()),
            Just("..".to_owned()),
            Just("...".to_owned()),
            Just("w".to_owned()),
            Just("wx".to_owned()),
            "[a-c.]{1,3}",
        ]
    }

    fn body() -> impl Strategy<Value = String> {
        prop::collection::vec(segment(), 0..8).prop_map(|segs| segs.join("/"))
    }

    fn absolute() -> impl Strategy<Value = String> {
        body().prop_map(|body| format!("/{body}"))
    }

    /// A relative path: never starts with `/`.
    fn relative() -> impl Strategy<Value = String> {
        body().prop_map(|body| if body.starts_with('/') { format!(".{body}") } else { body })
    }

    fn path() -> impl Strategy<Value = String> {
        prop_oneof![absolute(), relative()]
    }

    fn root() -> impl Strategy<Value = String> {
        prop::collection::vec(prop_oneof![Just("w"), Just("wx"), Just("a"), Just("b")], 0..3)
            .prop_map(|segs| format!("/{}", segs.join("/")))
    }

    fn is_normal(path: &str) -> bool {
        path == "/"
            || (!path.ends_with('/')
                && path.strip_prefix('/').is_some_and(|rest| rest.split('/').all(|s| !s.is_empty() && s != "." && s != "..")))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        /// Every accepted path is normalised and inside the (normalised) root.
        #[test]
        fn accepted_paths_are_normal_and_inside(root in root(), path in path()) {
            if let Ok(out) = confine(&root, &path) {
                let root = normalize(&root).unwrap();
                prop_assert!(is_normal(&out), "{out:?} not normal");
                prop_assert!(is_within(&out, &root), "{out:?} outside {root:?}");
            }
        }

        /// The only failure for an absolute root and NUL-free path is `denied`.
        #[test]
        fn only_denials(root in root(), path in path()) {
            if let Err(err) = confine(&root, &path) {
                prop_assert_eq!(err.code, ErrorCode::Denied);
            }
        }

        /// Confining an accepted result again is the identity (idempotence).
        #[test]
        fn idempotent(root in root(), path in path()) {
            if let Ok(out) = confine(&root, &path) {
                prop_assert_eq!(confine(&root, &out).unwrap(), out);
            }
        }

        /// An absolute path is accepted exactly when its normalisation lies within the root.
        #[test]
        fn absolute_iff_within(root in root(), path in absolute()) {
            let root_n = normalize(&root).unwrap();
            let norm = normalize(&path).unwrap();
            prop_assert_eq!(confine(&root, &path).is_ok(), is_within(&norm, &root_n));
            if let Ok(out) = confine(&root, &path) {
                prop_assert_eq!(out, norm);
            }
        }

        /// A relative path without `..` segments is always accepted and ends with its own
        /// normalised segments.
        #[test]
        fn relative_without_dotdot_is_accepted(root in root(), segs in prop::collection::vec("[a-c]{1,3}|\\.|", 0..6)) {
            let path = segs.join("/");
            let path = if path.starts_with('/') { format!(".{path}") } else { path };
            let out = confine(&root, &path).unwrap();
            let kept: Vec<&str> = segs.iter().map(String::as_str).filter(|s| !s.is_empty() && *s != ".").collect();
            let root_n = normalize(&root).unwrap();
            let expected = if kept.is_empty() { root_n } else if root_n == "/" { format!("/{}", kept.join("/")) } else { format!("{root_n}/{}", kept.join("/")) };
            prop_assert_eq!(out, expected);
        }

        /// A relative path that starts with `..` is always denied.
        #[test]
        fn leading_dotdot_is_denied(root in root(), path in relative()) {
            let escaped = format!("../{path}");
            prop_assert_eq!(confine(&root, &escaped).unwrap_err().code, ErrorCode::Denied);
        }

        /// Normalisation is idempotent and yields normal paths.
        #[test]
        fn normalize_is_idempotent(path in absolute()) {
            let once = normalize(&path).unwrap();
            prop_assert!(is_normal(&once));
            prop_assert_eq!(normalize(&once).unwrap(), once);
        }
    }
}
