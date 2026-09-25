//! Minimal, formatting-tolerant scanning of Verus source (which `syn` cannot parse).
//!
//! The kernel is always verusfmt-formatted (the gate runs `verusfmt --check` first), so a
//! line/brace scanner is reliable here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One Rust source file of the kernel.
pub struct SourceFile {
    pub path: PathBuf,
    pub module: String,
    pub text: String,
}

/// Loads every `.rs` file directly under `dir`, sorted by name.
pub fn load_dir(dir: &Path) -> Result<Vec<SourceFile>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let module = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            files.push(SourceFile { path, module, text });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Kind of a named kernel item that a decision can depend on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemKind {
    SpecFn,
    Type,
}

/// A named item (spec fn, struct or enum) with its full source text.
pub struct Item {
    pub kind: ItemKind,
    pub text: String,
    /// Doc-comment lines directly above the item.
    pub doc: Vec<String>,
}

/// Returns the identifier following `keyword` on `line` (e.g. `fn`, `enum`, `struct`).
pub fn ident_after(line: &str, keyword: &str) -> Option<String> {
    let mut words = line.split(|c: char| !(c.is_alphanumeric() || c == '_'));
    words.by_ref().find(|w| *w == keyword)?;
    words.find(|w| !w.is_empty()).map(str::to_owned)
}

/// Extracts the text from `lines[start]` to the end of the item: the matching closing brace of
/// the first `{`, or the first `;` when the item has no body.
fn item_text(lines: &[&str], start: usize) -> String {
    let mut depth: i64 = 0;
    let mut opened = false;
    let mut out = String::new();
    for line in lines.iter().skip(start) {
        out.push_str(line);
        out.push('\n');
        for c in line.chars() {
            match c {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if (opened && depth <= 0) || (!opened && line.trim_end().ends_with(';')) {
            break;
        }
    }
    out
}

/// Collects spec fns, structs and enums declared in `file`, keyed by name.
pub fn items(file: &SourceFile) -> BTreeMap<String, Item> {
    let lines: Vec<&str> = file.text.lines().collect();
    let mut found = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let (kind, name) = if trimmed.contains("spec fn ") && !trimmed.starts_with("//") {
            (ItemKind::SpecFn, ident_after(trimmed, "fn"))
        } else if trimmed.starts_with("pub enum ") || trimmed.starts_with("pub struct ") {
            let keyword = if trimmed.starts_with("pub enum ") { "enum" } else { "struct" };
            (ItemKind::Type, ident_after(trimmed, keyword))
        } else {
            continue;
        };
        let Some(name) = name else { continue };
        let doc = lines
            .get(..i)
            .unwrap_or_default()
            .iter()
            .rev()
            .map(|l| l.trim_start())
            .take_while(|l| l.starts_with("///") || l.starts_with("#["))
            .filter(|l| l.starts_with("///"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        found.insert(name, Item { kind, text: item_text(&lines, i), doc });
    }
    found
}

/// Whitespace-insensitive normal form used for digests (formatting never changes a decision).
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Identifier-like tokens appearing in `text`.
pub fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| !t.is_empty())
}
