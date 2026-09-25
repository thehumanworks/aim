//! OS-access boundary of the execution layer (docs/adr/0004, docs/adr/0009).
//!
//! Under `crates/aimx/src`, only the backends (`workspace/local`, `ssh`) and the server plumbing
//! (`server`, `main.rs`) may touch the filesystem or spawn processes. Everything else — tools,
//! authorization, dispatch — must go through the `Workspace` trait, so every effect can be
//! shadowed over SSH. Clippy's `disallowed-methods` is crate-wide and cannot express this.

use std::path::Path;

/// Paths (relative to `crates/aimx/src`) allowed to use OS APIs directly.
const ALLOWED: [&str; 4] = ["workspace/local", "ssh", "server", "main.rs"];

/// Qualified paths that mean direct OS access.
const FORBIDDEN: [&str; 4] = ["std::fs", "std::process", "tokio::fs", "tokio::process"];

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `line` uses a forbidden OS API, including grouped imports like `use std::{fs, …}`.
fn violates(line: &str) -> Option<&'static str> {
    let code = line.split("//").next().unwrap_or_default();
    if let Some(found) = FORBIDDEN.into_iter().find(|p| code.contains(p)) {
        return Some(found);
    }
    for root in ["std::{", "tokio::{"] {
        if let Some(start) = code.find(root) {
            let group = code.get(start + root.len()..).unwrap_or_default();
            let group = group.split('}').next().unwrap_or_default();
            if group
                .split(',')
                .map(str::trim)
                .any(|item| item == "fs" || item == "process" || item.starts_with("fs::") || item.starts_with("process::"))
            {
                return Some(if root.starts_with("std") { "std::{fs|process}" } else { "tokio::{fs|process}" });
            }
        }
    }
    None
}

/// Checks the execution layer's sources.
pub fn check(root: &Path) -> Vec<String> {
    let base = root.join("crates/aimx/src");
    let mut files = Vec::new();
    collect(&base, &mut files);
    files.sort();
    let mut findings = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&base).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        if ALLOWED.iter().any(|allowed| rel == *allowed || rel.starts_with(&format!("{allowed}/")) || rel == format!("{allowed}.rs")) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for (i, line) in text.lines().enumerate() {
            if let Some(api) = violates(line) {
                findings.push(format!(
                    "crates/aimx/src/{rel}:{}: `{api}` outside the backends/server — go through the Workspace trait (ADR 0004)",
                    i + 1
                ));
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::violates;

    #[test]
    fn detects_direct_and_grouped_os_access() {
        assert!(violates("use std::fs;").is_some());
        assert!(violates("let x = tokio::process::Command::new(\"ls\");").is_some());
        assert!(violates("use std::{collections::HashMap, fs};").is_some());
        assert!(violates("use tokio::{io, process::Command};").is_some());
        assert!(violates("use std::collections::HashMap;").is_none());
        assert!(violates("// std::fs is mentioned in a comment").is_none());
        assert!(violates("use std::{fmt, sync::Arc};").is_none());
    }
}
