//! ADR hygiene (docs/adr/README.md): unique numbers, well-formed names and headers.

use std::collections::BTreeMap;
use std::path::Path;

/// Checks `docs/adr`.
pub fn check(root: &Path) -> Vec<String> {
    let dir = root.join("docs/adr");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => return vec![format!("{}: {e}", dir.display())],
    };
    let mut by_number: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut findings = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_markdown = Path::new(&name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if name == "README.md" || name == "TEMPLATE.md" || !is_markdown {
            continue;
        }
        let number = name.get(..4).unwrap_or_default();
        let well_formed = number.len() == 4
            && number.chars().all(|c| c.is_ascii_digit())
            && name.as_bytes().get(4) == Some(&b'-')
            && name
                .trim_end_matches(".md")
                .get(5..)
                .is_some_and(|slug| !slug.is_empty() && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        if !well_formed {
            findings.push(format!("docs/adr/{name}: expected NNNN-kebab-slug.md"));
            continue;
        }
        by_number.entry(number.to_owned()).or_default().push(name.clone());
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let header: Vec<&str> = text.lines().take(12).collect();
        if !header.first().is_some_and(|l| l.starts_with(&format!("# ADR {number}: "))) {
            findings.push(format!("docs/adr/{name}: first line must be `# ADR {number}: <title>`"));
        }
        for field in ["- Status: ", "- Date: ", "- Scope: "] {
            if !header.iter().any(|l| l.starts_with(field)) {
                findings.push(format!("docs/adr/{name}: missing `{}` header line", field.trim_end()));
            }
        }
    }
    for (number, names) in &by_number {
        if names.len() > 1 {
            findings.push(format!("ADR number {number} is used more than once: {}", names.join(", ")));
        }
    }
    findings
}
