//! LOCKED decision digests (docs/adr/0005).
//!
//! A decision is `LOCKED(ADR-NNNN)` in the doc comment of a kernel spec fn. Its digest covers the
//! spec fn *and everything it references transitively* inside the kernel (other spec fns, structs
//! and enums), so a decision cannot be changed indirectly by editing a helper or adding a variant.
//! `crates/aim-kernel/LOCKED.toml` records the digests and is in the protected set.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::source::{self, Item, SourceFile, load_dir};

const MANIFEST: &str = "crates/aim-kernel/LOCKED.toml";

/// One locked decision and its current digest.
struct Locked {
    key: String,
    adr: String,
    digest: String,
}

/// Parses `LOCKED(ADR-NNNN)` from a doc comment.
fn locked_adr(doc: &[String]) -> Option<String> {
    doc.iter().find_map(|line| {
        let start = line.find("LOCKED(ADR-")? + "LOCKED(ADR-".len();
        let rest = line.get(start..)?;
        let end = rest.find(')')?;
        rest.get(..end).map(str::to_owned)
    })
}

/// Every declaration in the kernel, resolved the way Rust resolves the names a spec uses: by
/// module, not by bare name (two modules may both declare `next`, `wf` or `Event`).
struct Kernel {
    /// `module::name` → every declaration of that name in that module (methods of different
    /// types can share a name).
    items: BTreeMap<String, Vec<Item>>,
    /// Bare name → the `module::name` keys that declare it.
    by_name: BTreeMap<String, Vec<String>>,
    /// Module → the names it imports with `use crate::…`, and the module each comes from.
    imports: BTreeMap<String, BTreeMap<String, String>>,
}

impl Kernel {
    fn new(files: &[SourceFile]) -> Self {
        let mut kernel = Self { items: BTreeMap::new(), by_name: BTreeMap::new(), imports: BTreeMap::new() };
        for file in files {
            for (name, item) in source::items(file) {
                let key = format!("{}::{name}", file.module);
                let declarations = kernel.items.entry(key.clone()).or_default();
                if declarations.is_empty() {
                    kernel.by_name.entry(name).or_default().push(key);
                }
                declarations.push(item);
            }
            kernel.imports.insert(file.module.clone(), source::imports(file));
        }
        kernel
    }

    /// The declarations an identifier inside `module` may refer to: an explicit `module::name`
    /// path exactly; otherwise the module's own declaration; otherwise the module it is imported
    /// from; otherwise (a glob import, or a name the scanner cannot place) every declaration of
    /// that name. An unplaceable reference therefore widens the digest and never narrows it.
    fn resolve(&self, module: &str, qualifier: Option<&str>, name: &str) -> Vec<String> {
        if let Some(qualifier) = qualifier.filter(|q| self.imports.contains_key(*q)) {
            let key = format!("{qualifier}::{name}");
            return if self.items.contains_key(&key) { vec![key] } else { Vec::new() };
        }
        let local = format!("{module}::{name}");
        if self.items.contains_key(&local) {
            return vec![local];
        }
        if let Some(from) = self.imports.get(module).and_then(|imports| imports.get(name)) {
            let key = format!("{from}::{name}");
            if self.items.contains_key(&key) {
                return vec![key];
            }
        }
        self.by_name.get(name).cloned().unwrap_or_default()
    }

    /// Digest of `root` (a `module::name` key) plus the transitive closure of kernel items it
    /// references.
    fn closure_digest(&self, root: &str) -> String {
        let mut seen = BTreeSet::new();
        let mut stack = vec![root.to_owned()];
        while let Some(key) = stack.pop() {
            let Some(declarations) = self.items.get(&key) else { continue };
            if !seen.insert(key.clone()) {
                continue;
            }
            let module = key.split_once("::").map_or("", |(module, _)| module);
            for item in declarations {
                for (qualifier, name) in source::paths(&item.text) {
                    for next in self.resolve(module, qualifier, name) {
                        if !seen.contains(&next) {
                            stack.push(next);
                        }
                    }
                }
            }
        }
        let mut hasher = Sha256::new();
        for key in &seen {
            for item in self.items.get(key).into_iter().flatten() {
                hasher.update(format!("{:?} {key}\n", item.kind));
                hasher.update(source::normalize(&item.text));
                hasher.update("\n");
            }
        }
        let mut hex = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Every LOCKED decision, with its digest.
    fn locked(&self) -> Result<Vec<Locked>, String> {
        let mut locked = Vec::new();
        for (key, declarations) in &self.items {
            let marked: Vec<String> =
                declarations.iter().filter(|item| item.kind == source::ItemKind::SpecFn).filter_map(|item| locked_adr(&item.doc)).collect();
            let Some(adr) = marked.first() else { continue };
            if declarations.len() > 1 {
                return Err(format!(
                    "{key} is LOCKED but declared {} times in its module; give the decision a unique name",
                    declarations.len()
                ));
            }
            locked.push(Locked { key: key.clone(), adr: adr.clone(), digest: self.closure_digest(key) });
        }
        Ok(locked)
    }
}

/// Computes every LOCKED decision in the kernel.
fn current(root: &Path) -> Result<Vec<Locked>, String> {
    Kernel::new(&load_dir(&root.join("crates/aim-kernel/src"))?).locked()
}

fn render(locked: &[Locked]) -> String {
    let mut out = String::from(
        "# LOCKED decisions of aim-kernel and the digest of each spec's transitive closure.\n\
         # PROTECTED (docs/adr/0005): agents and evolution candidates must not edit this file.\n\
         # A change means the decision changed: it needs the maintainer and a superseding ADR,\n\
         # then `mise run locked:update`.\n",
    );
    for l in locked {
        let _ = write!(out, "\n[[locked]]\ndecision = \"{}\"\nadr = \"{}\"\nsha256 = \"{}\"\n", l.key, l.adr, l.digest);
    }
    out
}

/// Parses the manifest into `decision -> (adr, sha256)`.
fn parse(manifest: &str) -> BTreeMap<String, (String, String)> {
    let mut map = BTreeMap::new();
    let (mut decision, mut adr) = (String::new(), String::new());
    for line in manifest.lines() {
        let Some((key, value)) = line.split_once(" = ") else { continue };
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim() {
            "decision" => decision = value,
            "adr" => adr = value,
            "sha256" => {
                map.insert(std::mem::take(&mut decision), (std::mem::take(&mut adr), value));
            }
            _ => {}
        }
    }
    map
}

/// Compares the kernel's LOCKED decisions with the manifest.
pub fn check(root: &Path) -> Vec<String> {
    let locked = match current(root) {
        Ok(locked) => locked,
        Err(e) => return vec![e],
    };
    let manifest = std::fs::read_to_string(root.join(MANIFEST)).unwrap_or_default();
    let recorded = parse(&manifest);
    let mut findings = Vec::new();
    for l in &locked {
        if !root
            .join("docs/adr")
            .read_dir()
            .is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with(&format!("{}-", l.adr)))))
        {
            findings.push(format!("{} is LOCKED by ADR-{}, which does not exist in docs/adr", l.key, l.adr));
        }
        match recorded.get(&l.key) {
            None => findings.push(format!("{} is LOCKED but not recorded in {MANIFEST} (maintainer: `mise run locked:update`)", l.key)),
            Some((adr, digest)) if *digest != l.digest || *adr != l.adr => findings.push(format!(
                "LOCKED decision {} changed (spec or something it references). Locked decisions change only with the maintainer and a superseding ADR.",
                l.key
            )),
            Some(_) => {}
        }
    }
    for key in recorded.keys() {
        if !locked.iter().any(|l| &l.key == key) {
            findings.push(format!("{key} is recorded in {MANIFEST} but is no longer LOCKED in the kernel"));
        }
    }
    findings
}

/// Rewrites the manifest from the current kernel.
pub fn update(root: &Path) -> Vec<String> {
    match current(root) {
        Ok(locked) => match std::fs::write(root.join(MANIFEST), render(&locked)) {
            Ok(()) => Vec::new(),
            Err(e) => vec![format!("{MANIFEST}: {e}")],
        },
        Err(e) => vec![e],
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn file(module: &str, text: &str) -> SourceFile {
        SourceFile { path: PathBuf::from(format!("{module}.rs")), module: module.to_owned(), text: text.to_owned() }
    }

    fn digest(files: &[SourceFile], key: &str) -> String {
        let locked = Kernel::new(files).locked().unwrap_or_default();
        locked.into_iter().find(|l| l.key == key).map(|l| l.digest).unwrap_or_default()
    }

    const JOB: &str = "pub enum Event {\n    Claim,\n}\n\n/// LOCKED(ADR-0048)\npub open spec fn accepted(e: Event) -> bool {\n    next(e)\n}\n\npub open spec fn next(e: Event) -> bool {\n    true\n}\n";
    const TURN: &str = "pub enum Event {\n    Start,\n}\n\npub open spec fn next(e: Event) -> bool {\n    false\n}\n";

    #[test]
    fn a_same_named_item_in_another_module_is_not_the_one_a_spec_uses() {
        let base = digest(&[file("job", JOB), file("turn", TURN)], "job::accepted");
        let other_changed = digest(&[file("job", JOB), file("turn", &TURN.replace("false", "true"))], "job::accepted");
        let own_changed = digest(&[file("job", &JOB.replace("    true", "    false")), file("turn", TURN)], "job::accepted");
        let own_type_changed = digest(&[file("job", &JOB.replace("Claim", "Claimed")), file("turn", TURN)], "job::accepted");
        assert!(!base.is_empty());
        assert_eq!(base, other_changed, "turn::next is not part of job::accepted");
        assert_ne!(base, own_changed, "job::next is part of job::accepted");
        assert_ne!(base, own_type_changed, "job::Event is part of job::accepted");
    }

    #[test]
    fn imports_and_paths_pick_the_named_module() {
        let board = "use crate::job::{Event};\n\n/// LOCKED(ADR-0048)\npub open spec fn may_claim(e: Event) -> bool {\n    crate::job::next(e)\n}\n";
        let files = |job: &str, turn: &str| vec![file("board", board), file("job", job), file("turn", turn)];
        let base = digest(&files(JOB, TURN), "board::may_claim");
        assert_eq!(base, digest(&files(JOB, &TURN.replace("Start", "Begin").replace("false", "true")), "board::may_claim"));
        assert_ne!(base, digest(&files(&JOB.replace("Claim", "Claimed"), TURN), "board::may_claim"));
        assert_ne!(base, digest(&files(&JOB.replace("    true", "    false"), TURN), "board::may_claim"));
    }

    #[test]
    fn an_unplaceable_name_widens_the_digest() {
        let glob = "use crate::job::*;\n\n/// LOCKED(ADR-0048)\npub open spec fn uses(e: Event) -> bool {\n    true\n}\n";
        let files = |turn: &str| vec![file("glob", glob), file("job", JOB), file("turn", turn)];
        assert_ne!(digest(&files(TURN), "glob::uses"), digest(&files(&TURN.replace("Start", "Begin")), "glob::uses"));
    }

    #[test]
    fn a_locked_name_declared_twice_in_its_module_is_refused() {
        let twice = "/// LOCKED(ADR-0048)\npub open spec fn wf() -> bool {\n    true\n}\n\npub open spec fn wf() -> bool {\n    false\n}\n";
        assert!(Kernel::new(&[file("dup", twice)]).locked().is_err());
    }
}
