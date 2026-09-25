//! Exact-substring edit application (docs/architecture.md §4.1 `fs.edit`).
//!
//! Kernel candidate: pure and total. Edits apply in order, each to the result of the previous one;
//! the whole request fails (and nothing is written) when any edit is invalid. Matching is on bytes,
//! so files that are not valid UTF-8 can still be edited; occurrences are counted left to right
//! without overlap (as `str::matches` does).

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::ExactEdit;
use memchr::memmem;

/// Why an edit request cannot be applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditError {
    /// Edit `edit_index` has an empty `old` text (it would match everywhere).
    EmptyOld {
        /// Index of the offending edit.
        edit_index: usize,
    },
    /// Edit `edit_index` matched `occurrences` times where exactly one (or, with `replace_all`,
    /// at least one) was required.
    Occurrences {
        /// Index of the offending edit.
        edit_index: usize,
        /// How often its `old` text occurs in the content it applies to.
        occurrences: usize,
    },
}

impl From<EditError> for ProtoError {
    fn from(err: EditError) -> Self {
        match err {
            EditError::EmptyOld { edit_index } => {
                Self::new(ErrorCode::InvalidParams, format!("edit {edit_index}: `old` must not be empty"))
                    .with_detail(serde_json::json!({ "edit_index": edit_index }))
            }
            EditError::Occurrences { edit_index, occurrences } => {
                let message = if occurrences == 0 {
                    format!("edit {edit_index}: `old` text not found")
                } else {
                    format!("edit {edit_index}: `old` text occurs {occurrences} times; make it unique or set replace_all")
                };
                Self::new(ErrorCode::Conflict, message)
                    .with_detail(serde_json::json!({ "edit_index": edit_index, "occurrences": occurrences }))
            }
        }
    }
}

/// The result of applying edits: the new content and the replacements made per edit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Applied {
    /// The edited content.
    pub content: Vec<u8>,
    /// Replacements made by each edit, in order.
    pub replacements: Vec<u32>,
}

/// Start offsets of the non-overlapping occurrences of `needle` in `haystack`, left to right.
#[must_use]
pub fn occurrences(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    memmem::find_iter(haystack, needle).collect()
}

/// Replaces `needle` at each of `positions` (sorted, non-overlapping starts) with `with`.
fn replace_at(haystack: &[u8], needle_len: usize, positions: &[usize], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len().saturating_add(positions.len().saturating_mul(with.len())));
    let mut cursor = 0usize;
    for &start in positions {
        out.extend_from_slice(haystack.get(cursor..start).unwrap_or_default());
        out.extend_from_slice(with);
        cursor = start.saturating_add(needle_len);
    }
    out.extend_from_slice(haystack.get(cursor..).unwrap_or_default());
    out
}

/// Applies `edits` to `content` in order, all or nothing.
///
/// # Errors
/// [`EditError::EmptyOld`] for an empty `old` text; [`EditError::Occurrences`] when an edit's
/// `old` text does not occur exactly once (or, with `replace_all`, not at all).
pub fn apply_edits(content: &[u8], edits: &[ExactEdit]) -> Result<Applied, EditError> {
    let mut current = content.to_vec();
    let mut replacements = Vec::with_capacity(edits.len());
    for (edit_index, edit) in edits.iter().enumerate() {
        let old = edit.old.as_bytes();
        if old.is_empty() {
            return Err(EditError::EmptyOld { edit_index });
        }
        let positions = occurrences(&current, old);
        let count = positions.len();
        if count == 0 || (!edit.replace_all && count != 1) {
            return Err(EditError::Occurrences { edit_index, occurrences: count });
        }
        current = replace_at(&current, old.len(), &positions, edit.new.as_bytes());
        replacements.push(u32::try_from(count).unwrap_or(u32::MAX));
    }
    Ok(Applied { content: current, replacements })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn edit(old: &str, new: &str, replace_all: bool) -> ExactEdit {
        ExactEdit { old: old.to_owned(), new: new.to_owned(), replace_all }
    }

    #[test]
    fn single_replacement() {
        let out = apply_edits(b"hello world", &[edit("world", "there", false)]).unwrap();
        assert_eq!(out.content, b"hello there");
        assert_eq!(out.replacements, vec![1]);
    }

    #[test]
    fn edits_apply_in_order() {
        let out = apply_edits(b"a b c", &[edit("a", "x", false), edit("x b", "y", false)]).unwrap();
        assert_eq!(out.content, b"y c");
    }

    #[test]
    fn ambiguous_and_missing_fail() {
        assert_eq!(apply_edits(b"aa", &[edit("a", "b", false)]), Err(EditError::Occurrences { edit_index: 0, occurrences: 2 }));
        assert_eq!(
            apply_edits(b"abc", &[edit("a", "z", false), edit("q", "r", false)]),
            Err(EditError::Occurrences { edit_index: 1, occurrences: 0 })
        );
        assert_eq!(apply_edits(b"abc", &[edit("", "r", false)]), Err(EditError::EmptyOld { edit_index: 0 }));
    }

    #[test]
    fn replace_all_counts_non_overlapping() {
        let out = apply_edits(b"aaaa", &[edit("aa", "b", true)]).unwrap();
        assert_eq!(out.content, b"bb");
        assert_eq!(out.replacements, vec![2]);
        assert_eq!(apply_edits(b"xyz", &[edit("a", "b", true)]), Err(EditError::Occurrences { edit_index: 0, occurrences: 0 }));
    }

    #[test]
    fn non_utf8_content_is_editable() {
        let out = apply_edits(&[0xff, b'a', 0xfe], &[edit("a", "bc", false)]).unwrap();
        assert_eq!(out.content, vec![0xff, b'b', b'c', 0xfe]);
    }

    proptest! {
        /// A unique `old` is replaced exactly: prefix + new + suffix.
        #[test]
        fn unique_replacement_is_exact(prefix in "[a-c]{0,8}", suffix in "[a-c]{0,8}", new in "[a-z]{0,5}") {
            let old = "XYZ";
            let content = format!("{prefix}{old}{suffix}");
            let out = apply_edits(content.as_bytes(), &[edit(old, &new, false)]).unwrap();
            prop_assert_eq!(out.content, format!("{prefix}{new}{suffix}").into_bytes());
        }

        /// Matches `str::replace` for replace-all, and agrees with `str::matches` on the count.
        #[test]
        fn replace_all_matches_std(content in "[ab]{0,24}", old in "[ab]{1,3}", new in "[abc]{0,3}") {
            let count = content.matches(old.as_str()).count();
            match apply_edits(content.as_bytes(), &[edit(&old, &new, true)]) {
                Ok(out) => {
                    prop_assert_eq!(out.content, content.replace(old.as_str(), &new).into_bytes());
                    prop_assert_eq!(out.replacements, vec![u32::try_from(count).unwrap()]);
                }
                Err(err) => prop_assert_eq!(err, EditError::Occurrences { edit_index: 0, occurrences: 0 }),
            }
        }

        /// A failing request reports the first failing edit and the success path never fails.
        #[test]
        fn single_edit_succeeds_iff_exactly_once(content in "[ab]{0,16}", old in "[ab]{1,2}") {
            let count = content.matches(old.as_str()).count();
            let result = apply_edits(content.as_bytes(), &[edit(&old, "", false)]);
            prop_assert_eq!(result.is_ok(), count == 1);
        }
    }
}
