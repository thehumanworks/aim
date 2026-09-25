//! Source-tree checks run by the gate's pinned binary after candidate exit.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::check_locked;

/// Gate-owned source protection and dependency allowlist. Paths end in `/` for a subtree.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedManifest {
    version: u32,
    protected: Vec<String>,
    sensitive: Vec<String>,
    #[serde(default)]
    allowed_sensitive: Vec<String>,
    #[serde(default)]
    allowed_packages: Vec<String>,
}

impl ProtectedManifest {
    /// Parse and validate a gate-owned manifest. Never call this on candidate bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed TOML, unsupported versions or unsafe path rules.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self = toml::from_str(text).map_err(|e| format!("invalid protected manifest: {e}"))?;
        if manifest.version != 1 {
            return Err(format!("unsupported protected manifest version {}", manifest.version));
        }
        for path in manifest.protected.iter().chain(&manifest.sensitive).chain(&manifest.allowed_sensitive) {
            valid_rule(path)?;
        }
        Ok(manifest)
    }

    /// Load the immutable manifest embedded in this pinned library build.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedded manifest is invalid.
    pub fn embedded() -> Result<Self, String> {
        Self::parse(include_str!("../protected.toml"))
    }
}

/// Inputs captured by the gate outside candidate authority.
#[derive(Clone, Debug)]
pub struct ValidationConfig {
    /// The pinned protection policy.
    pub manifest: ProtectedManifest,
    /// Full output of baseline `cargo test -- --list`, run in the sandbox.
    pub baseline_test_inventory: String,
    /// Full output of candidate `cargo test -- --list`, run in the sandbox.
    pub candidate_test_inventory: String,
}

/// Independent comparison of final source trees and compiled test lists.
#[derive(Clone, Debug, Serialize)]
pub struct ValidationReport {
    /// Every changed source path, including untracked generated source outside `target`.
    pub changed_paths: Vec<String>,
    /// Rejection reasons; promotion requires this to be empty.
    pub findings: Vec<String>,
}

impl ValidationReport {
    /// True only when all protected checks passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }
}

#[derive(Clone, Eq, PartialEq)]
enum Entry {
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn valid_rule(rule: &str) -> Result<(), String> {
    let path = Path::new(rule.trim_end_matches('/'));
    if path.as_os_str().is_empty() || path.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err(format!("invalid protected path rule: {rule}"));
    }
    Ok(())
}

fn matches_rule(path: &str, rule: &str) -> bool {
    if let Some(prefix) = rule.strip_suffix('/') { path == prefix || path.starts_with(&format!("{prefix}/")) } else { path == rule }
}

fn read_tree(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    let mut result = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
            let name = relative.to_string_lossy().replace('\\', "/");
            if name == ".git" || name.starts_with(".git/") || name == "target" || name.starts_with("target/") {
                continue;
            }
            let file_type = entry.file_type().map_err(|e| format!("{}: {e}", path.display()))?;
            if file_type.is_symlink() {
                result.insert(name, Entry::Symlink(fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?));
            } else if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                result.insert(name, Entry::File(fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?));
            } else {
                return Err(format!("unsupported source file type: {}", path.display()));
            }
        }
    }
    Ok(result)
}

fn lock_packages(tree: &BTreeMap<String, Entry>) -> Result<BTreeSet<String>, String> {
    let Some(Entry::File(bytes)) = tree.get("Cargo.lock") else {
        return Err("Cargo.lock missing or not a regular file".to_owned());
    };
    let text = std::str::from_utf8(bytes).map_err(|e| format!("Cargo.lock: {e}"))?;
    let value: toml::Value = toml::from_str(text).map_err(|e| format!("Cargo.lock: {e}"))?;
    let packages = value.get("package").and_then(toml::Value::as_array).ok_or("Cargo.lock has no package array")?;
    packages
        .iter()
        .map(|package| {
            let name = package.get("name").and_then(toml::Value::as_str).ok_or("Cargo.lock package has no name")?;
            let version = package.get("version").and_then(toml::Value::as_str).ok_or("Cargo.lock package has no version")?;
            let source = package.get("source").and_then(toml::Value::as_str).unwrap_or("local");
            let checksum = package.get("checksum").and_then(toml::Value::as_str).unwrap_or("none");
            Ok(format!("{name}@{version}#{source}~{checksum}"))
        })
        .collect()
}

fn target_from_header(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("Doc-tests ") {
        return Some(format!("doctests:{rest}"));
    }
    let rest = line.strip_prefix("Running ")?;
    let (kind, path) = rest.split_once(' ')?;
    let executable = path.rsplit_once('(')?.1.strip_suffix(')')?;
    let basename = Path::new(executable).file_name()?.to_str()?;
    let stable_name =
        basename.rsplit_once('-').filter(|(_, hash)| hash.bytes().all(|byte| byte.is_ascii_hexdigit())).map_or(basename, |(stem, _)| stem);
    Some(format!("{kind}:{stable_name}"))
}

fn test_inventory(text: &str) -> Result<BTreeMap<String, usize>, String> {
    let mut tests = BTreeMap::new();
    let mut target = None;
    for line in text.lines() {
        if let Some(header) = target_from_header(line) {
            target = Some(header);
        } else if let Some(name) = line.trim().strip_suffix(": test") {
            let Some(section) = &target else {
                return Err(format!("test inventory entry lacks a target header: {name}"));
            };
            *tests.entry(format!("{section}::{name}")).or_insert(0) += 1;
        }
    }
    Ok(tests)
}

fn test_attrs(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut result = BTreeMap::new();
    let mut attrs = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("#[") {
            attrs.insert(line.to_owned());
            continue;
        }
        if attrs.iter().any(|attr| attr == "#[test]" || attr.starts_with("#[tokio::test") || attr.ends_with("::test]"))
            && let Some((_, tail)) = line.split_once("fn ")
            && let Some(name) = tail.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next()
        {
            result.insert(name.to_owned(), attrs.clone());
        }
        if !line.starts_with("///") && !line.is_empty() {
            attrs.clear();
        }
    }
    result
}

fn test_module_cfgs(text: &str) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut attrs = BTreeSet::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with("#[") {
            attrs.insert(line.to_owned());
            continue;
        }
        if line.contains("mod tests") {
            result.extend(attrs.iter().filter(|attr| attr.starts_with("#[cfg(") || attr.starts_with("#[cfg_attr(")).cloned());
        }
        if !line.starts_with("///") && !line.is_empty() {
            attrs.clear();
        }
    }
    result
}

fn check_test_attrs(path: &str, base: Option<&Entry>, candidate: Option<&Entry>, findings: &mut Vec<String>) {
    let (Some(Entry::File(candidate)), base) = (candidate, base) else { return };
    let Ok(candidate) = std::str::from_utf8(candidate) else { return };
    let baseline = match base {
        Some(Entry::File(bytes)) => std::str::from_utf8(bytes).unwrap_or_default(),
        _ => "",
    };
    let old = test_attrs(baseline);
    for (name, attrs) in test_attrs(candidate) {
        let previous = old.get(&name);
        if attrs.iter().any(|attr| attr.starts_with("#[ignore"))
            && !previous.is_some_and(|attrs| attrs.iter().any(|attr| attr.starts_with("#[ignore")))
        {
            findings.push(format!("test {path}::{name} newly ignored"));
        }
        if attrs.iter().any(|attr| attr.starts_with("#[cfg(") || attr.starts_with("#[cfg_attr("))
            && !previous.is_some_and(|attrs| attrs.iter().any(|attr| attr.starts_with("#[cfg(") || attr.starts_with("#[cfg_attr(")))
        {
            findings.push(format!("test {path}::{name} newly cfg-gated"));
        }
    }
    for cfg in test_module_cfgs(candidate).difference(&test_module_cfgs(baseline)) {
        findings.push(format!("test module {path} newly cfg-gated: {cfg}"));
    }
}

/// Compare final baseline and candidate trees after candidate exit.
///
/// The gate must execute each test-list command under its sandbox and pass actual output here.
///
/// # Errors
///
/// Returns an error if source trees or lock files cannot be read or parsed.
pub fn validate(base: &Path, candidate: &Path, config: &ValidationConfig) -> Result<ValidationReport, String> {
    let baseline = read_tree(base)?;
    let proposed = read_tree(candidate)?;
    let paths = baseline.keys().chain(proposed.keys()).cloned().collect::<BTreeSet<_>>();
    let changed_paths = paths.into_iter().filter(|path| baseline.get(path) != proposed.get(path)).collect::<Vec<_>>();
    let mut findings = Vec::new();
    for path in &changed_paths {
        if config.manifest.protected.iter().any(|rule| matches_rule(path, rule)) {
            findings.push(format!("protected path changed: {path}"));
        }
        if config.manifest.sensitive.iter().any(|rule| matches_rule(path, rule))
            && !config.manifest.allowed_sensitive.iter().any(|rule| matches_rule(path, rule))
        {
            findings.push(format!("sensitive path changed without allowlist: {path}"));
        }
        if (path.ends_with("/build.rs") || path == "build.rs") && !baseline.contains_key(path) {
            findings.push(format!("new build script: {path}"));
        }
        if Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("rs")) {
            check_test_attrs(path, baseline.get(path), proposed.get(path), &mut findings);
        }
    }
    let old_packages = lock_packages(&baseline)?;
    let new_packages = lock_packages(&proposed)?;
    for package in new_packages.difference(&old_packages) {
        if !config.manifest.allowed_packages.contains(package) {
            findings.push(format!("new dependency or source without allowlist: {package}"));
        }
    }
    let old_tests = test_inventory(&config.baseline_test_inventory)?;
    let new_tests = test_inventory(&config.candidate_test_inventory)?;
    if old_tests.is_empty() || new_tests.is_empty() {
        findings.push("compiled test inventory missing or empty".to_owned());
    }
    for (test, old_count) in old_tests {
        if new_tests.get(&test).copied().unwrap_or_default() < old_count {
            findings.push(format!("compiled test deleted or cfg'd out: {test}"));
        }
    }
    findings.extend(check_locked(candidate));
    Ok(ValidationReport { changed_paths, findings })
}
