//! Kernel API rules (docs/adr/0005): no precondition on public exec functions, no cheating.

use std::path::Path;

use aim_gate_validators::source::load_dir;

/// Constructs that make a proof an assumption; also rejected by `verus --no-cheating`, checked
/// here so the plain gate catches them without Verus installed.
const CHEATS: [&str; 5] = ["assume(", "admit(", "external_body", "assume_specification", "#[verifier::external]"];

/// Whether `trimmed` starts a public *executable* function (`pub fn` / `pub const fn`).
fn is_pub_exec_fn(trimmed: &str) -> bool {
    (trimmed.starts_with("pub fn ") || trimmed.starts_with("pub const fn "))
        && !trimmed.contains("spec fn")
        && !trimmed.contains("proof fn")
}

/// Checks every kernel source file.
pub fn check(root: &Path) -> Vec<String> {
    let dir = root.join("crates/aim-kernel/src");
    let files = match load_dir(&dir) {
        Ok(files) => files,
        Err(e) => return vec![e],
    };
    let mut findings = Vec::new();
    for file in &files {
        let lines: Vec<&str> = file.text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let at = format!("{}:{}", file.path.display(), i + 1);
            if !trimmed.starts_with("//") {
                for cheat in CHEATS {
                    if trimmed.contains(cheat) {
                        findings.push(format!("{at}: `{cheat}` is not allowed in aim-kernel (no cheating)"));
                    }
                }
            }
            let is_pub_ghost_fn = trimmed.starts_with("pub ") && (trimmed.contains("spec fn ") || trimmed.contains("proof fn "));
            if is_pub_ghost_fn {
                let documented = lines
                    .get(..i)
                    .unwrap_or_default()
                    .iter()
                    .rev()
                    .map(|l| l.trim_start())
                    .find(|l| !l.starts_with("#["))
                    .is_some_and(|l| l.starts_with("///"));
                if !documented {
                    findings.push(format!("{at}: public spec/proof fn needs a doc comment (specs are aim's live documentation)"));
                }
            }
            if !is_pub_exec_fn(trimmed) || trimmed.trim_end().ends_with('{') {
                continue;
            }
            // verusfmt puts the body's `{` alone on a line at the fn's indentation; any `requires`
            // between the signature and that line is a precondition on a public function.
            let indent = line.len() - trimmed.len();
            for next in lines.iter().skip(i + 1) {
                let next_trimmed = next.trim_start();
                if next_trimmed == "{" && next.len() - next_trimmed.len() == indent {
                    break;
                }
                if next_trimmed == "requires" || next_trimmed.starts_with("requires ") {
                    findings.push(format!(
                        "{at}: public exec fn has a `requires` clause; make it total (return Result/Option) and keep preconditions on private fns"
                    ));
                    break;
                }
            }
        }
    }
    findings
}
