//! Alias-proof matching of protected paths (REV4-A findings 1 and 2).
//!
//! Kernel candidate: pure, no I/O. Comparing path *strings* misses aliases: on a case-insensitive
//! or normalization-insensitive volume (macOS's APFS, HFS+), `.aim/GATE` and `.aim/gate`, or an
//! NFC and an NFD spelling of `café`, name the same file. So a path is described as a [`Chain`]:
//! each component from `/` down, with the identity (device, inode) of every component that
//! exists, and how names compare in the directory where the missing rest would be created.
//!
//! [`within`] anchors on the deepest *existing* component of the outer path: the inner path is
//! within it when one of the inner path's existing components has that identity and the inner
//! path continues with the outer path's missing names. Existing components match by identity (so
//! every spelling, symlinked or hard-linked name of them matches); missing ones by name, compared
//! in Unicode NFD always (a volume that tells normalizations apart is only over-protected) and
//! ignoring case when the anchoring directory's volume does.

use std::ffi::{OsStr, OsString};

use unicode_normalization::UnicodeNormalization as _;

/// A file's identity: its device and inode (widened, so every platform's types fit).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileId {
    /// Device.
    pub dev: i128,
    /// Inode.
    pub ino: i128,
}

/// One component of a path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Step {
    /// Its name (empty for `/`).
    pub name: OsString,
    /// Its identity, when it exists.
    pub id: Option<FileId>,
}

/// A path as the filesystem sees it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Chain {
    /// Every component from `/` down; the existing ones come first.
    pub steps: Vec<Step>,
    /// Names in the directory of the deepest existing component compare ignoring case.
    pub fold_case: bool,
}

/// The key a name is compared by: Unicode NFD, and, when `fold_case`, case-folded (lower, then
/// upper, then lower again, so `ß`/`SS`/`ss` and similar spellings meet — over-matching is safe
/// here).
#[must_use]
pub fn name_key(name: &OsStr, fold_case: bool) -> String {
    let nfd: String = name.to_string_lossy().nfd().collect();
    if fold_case { nfd.to_lowercase().to_uppercase().to_lowercase().nfd().collect() } else { nfd }
}

/// Whether two components are the same entry, given that the components before them are.
fn same(inner: &Step, outer: &Step, fold_case: bool) -> bool {
    match (inner.id, outer.id) {
        (Some(a), Some(b)) => a == b,
        _ => name_key(&inner.name, fold_case) == name_key(&outer.name, fold_case),
    }
}

/// Whether `inner` names `outer` or something inside it.
#[must_use]
pub fn within(inner: &Chain, outer: &Chain) -> bool {
    let Some(anchor) = outer.steps.iter().rposition(|step| step.id.is_some()) else {
        // Nothing of `outer` exists (not even `/`): compare every name from the top.
        return inner.steps.len() >= outer.steps.len()
            && inner.steps.iter().zip(&outer.steps).all(|(a, b)| name_key(&a.name, true) == name_key(&b.name, true));
    };
    let anchor_id = outer.steps.get(anchor).and_then(|step| step.id);
    let rest = outer.steps.get(anchor + 1..).unwrap_or_default();
    inner.steps.iter().enumerate().filter(|(_, step)| step.id.is_some() && step.id == anchor_id).any(|(at, _)| {
        let tail = inner.steps.get(at + 1..).unwrap_or_default();
        tail.len() >= rest.len() && tail.iter().zip(rest).all(|(a, b)| same(a, b, outer.fold_case))
    })
}

/// Whether modifying `target` modifies `protected`: it is it, or lies inside it (`tree`: or
/// contains it, for removing and moving).
#[must_use]
pub fn guards(target: &Chain, protected: &Chain, tree: bool) -> bool {
    within(target, protected) || (tree && within(protected, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::unnecessary_wraps, reason = "reads as the `id` field it fills")]
    fn id(ino: i128) -> Option<FileId> {
        Some(FileId { dev: 1, ino })
    }

    fn chain(steps: &[(&str, Option<FileId>)], fold_case: bool) -> Chain {
        Chain { steps: steps.iter().map(|(name, id)| Step { name: OsString::from(name), id: *id }).collect(), fold_case }
    }

    #[test]
    fn existing_paths_match_by_identity_whatever_the_spelling() {
        let gate = chain(&[("", id(1)), ("home", id(2)), (".aim", id(3)), ("gate", id(4))], true);
        // `.AIM/GATE/x`: the walk opened the same directories under another spelling.
        let alias = chain(&[("", id(1)), ("home", id(2)), (".AIM", id(3)), ("GATE", id(4)), ("x", None)], true);
        assert!(within(&alias, &gate));
        assert!(guards(&alias, &gate, false));
        // A hard link to the protected file is the protected file.
        let link = chain(&[("", id(1)), ("elsewhere", id(9)), ("h", id(4))], true);
        assert!(within(&link, &gate));
        // A sibling is not.
        let sibling = chain(&[("", id(1)), ("home", id(2)), (".aim", id(3)), ("other", id(5))], true);
        assert!(!within(&sibling, &gate));
        // An ancestor contains it: only tree operations are refused.
        let ancestor = chain(&[("", id(1)), ("home", id(2)), (".AIM", id(3))], true);
        assert!(!guards(&ancestor, &gate, false));
        assert!(guards(&ancestor, &gate, true));
    }

    #[test]
    fn missing_paths_match_by_folded_name_under_their_anchor() {
        // `~/.aim/ledger` does not exist yet; `~/.aim` does.
        let ledger = chain(&[("", id(1)), ("home", id(2)), (".aim", id(3)), ("ledger", None)], true);
        let alias = chain(&[("", id(1)), ("home", id(2)), (".aim", id(3)), ("LEDGER", None), ("entry", None)], true);
        assert!(within(&alias, &ledger));
        // On a case-sensitive volume `LEDGER` is another name.
        let strict = Chain { fold_case: false, ..ledger.clone() };
        assert!(!within(&alias, &strict));
        // Another directory with the same name is not the ledger.
        let elsewhere = chain(&[("", id(1)), ("home", id(2)), ("tmp", id(7)), ("ledger", None)], true);
        assert!(!within(&elsewhere, &ledger));
        // Moving a directory to where the missing parent would be creates the protected path.
        let parent = chain(&[("", id(1)), ("home", id(2)), (".AIM", None)], true);
        let deep = chain(&[("", id(1)), ("home", id(2)), (".aim", None), ("gate", None)], true);
        assert!(guards(&parent, &deep, true));
        assert!(!guards(&parent, &deep, false));
    }

    #[test]
    fn names_compare_in_nfd_and_fold_case_when_asked() {
        let nfc = OsStr::new("caf\u{e9}");
        let nfd = OsStr::new("cafe\u{301}");
        assert_eq!(name_key(nfc, false), name_key(nfd, false));
        assert_eq!(name_key(OsStr::new("CAF\u{c9}"), true), name_key(nfd, true));
        assert_ne!(name_key(OsStr::new("Gate"), false), name_key(OsStr::new("gate"), false));
        assert_eq!(name_key(OsStr::new("STRASSE"), true), name_key(OsStr::new("stra\u{df}e"), true));
    }
}
