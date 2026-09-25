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

use crate::source::{self, Item, load_dir};

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

/// Digest of `root_name` plus the transitive closure of kernel items it references.
fn closure_digest(root_name: &str, all: &BTreeMap<String, Item>) -> String {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root_name.to_owned()];
    while let Some(name) = stack.pop() {
        let Some(item) = all.get(&name) else { continue };
        if !seen.insert(name) {
            continue;
        }
        for token in source::tokens(&item.text) {
            if all.contains_key(token) && !seen.contains(token) {
                stack.push(token.to_owned());
            }
        }
    }
    let mut hasher = Sha256::new();
    for name in &seen {
        if let Some(item) = all.get(name) {
            hasher.update(format!("{:?} {name}\n", item.kind));
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

/// Computes every LOCKED decision in the kernel.
fn current(root: &Path) -> Result<Vec<Locked>, String> {
    let files = load_dir(&root.join("crates/aim-kernel/src"))?;
    let mut all = BTreeMap::new();
    let mut owners = BTreeMap::new();
    for file in &files {
        for (name, item) in source::items(file) {
            owners.insert(name.clone(), file.module.clone());
            all.insert(name, item);
        }
    }
    let mut locked = Vec::new();
    for (name, item) in &all {
        if item.kind != source::ItemKind::SpecFn {
            continue;
        }
        if let Some(adr) = locked_adr(&item.doc) {
            let module = owners.get(name).cloned().unwrap_or_default();
            locked.push(Locked { key: format!("{module}::{name}"), adr, digest: closure_digest(name, &all) });
        }
    }
    Ok(locked)
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
