//! The stable instruction prefix (docs/architecture.md §6.4, §6.8).
//!
//! What goes in, in order, after aim's system prompt:
//!
//! 1. **Project instructions** — `AGENTS.md` in every directory from the workspace root down to
//!    the session's directory (`CLAUDE.md` where a directory has no `AGENTS.md`; Codex walks the
//!    same way, research/agents-conventions.md §4), then `.agents/instructions.md`, then rules
//!    without `paths:` (Claude loads those unconditionally). All share one byte cap,
//!    [`MAX_PROJECT_INSTRUCTIONS`], filled outermost first; a cut is marked.
//! 2. **Rules index** — each path-scoped rule's name, `paths:` globs and description. Bodies are
//!    read **on demand** by the model (its `Read`), not injected: rules govern the files being
//!    changed, which a prompt rarely names, so matching the prompt would miss most uses and
//!    matching belongs at the tool boundary (a dispatcher hook, later; research §4). The prefix
//!    and the user's turn stay unchanged.
//! 3. **Skill catalog** — name, description and path of each skill, within
//!    [`skill_budget`] (about 2% of the context window; 4 KiB when unknown). Descriptions shrink
//!    before skills are dropped (Codex's order, research §1).
//! 4. **Memory index** — the first [`MEMORY_LINES`] lines (at most [`MEMORY_BYTES`]) of the
//!    project's `.agents/memory/MEMORY.md` and the user's `~/.aim/memory/MEMORY.md`, under a note
//!    that memory advises and never grants.
//! 5. **The agent's instructions**, when the session runs a named agent.
//!
//! Everything here is fixed at session start, so the prefix (and the provider's prompt cache)
//! never changes mid-session; explicit skill activations go into the user turn instead.

use std::fmt::Write as _;

use super::agents::AgentDef;
use super::files::FileText;
use super::skills::report_unsupported;
use super::yaml::{self, Fields, Split};
use super::{Catalog, Described, Descriptor, Diagnostic, Kind, Origin, Problem, Scope, Source, clip, is_addressable, one_line};

/// Most bytes of project instructions (`AGENTS.md` walk, `.agents/instructions.md`, global
/// rules) in the prefix. Codex uses the same 32 KiB cap.
pub const MAX_PROJECT_INSTRUCTIONS: usize = 32 * 1024;
/// The skill catalog's budget when the context window is unknown.
pub const DEFAULT_SKILL_BUDGET: usize = 4 * 1024;
/// Most lines of a memory index in the prefix.
pub const MEMORY_LINES: usize = 80;
/// Most bytes of a memory index in the prefix.
pub const MEMORY_BYTES: usize = 6 * 1024;
/// Shortest description the catalog shrinks to before it drops skills.
pub const MIN_DESCRIPTION: usize = 48;

/// Parser version of instruction files.
pub const PARSER_INSTRUCTIONS: &str = "agents-md/1";
/// Parser version of rules.
pub const PARSER_RULE: &str = "aim.rule/1";
/// Parser version of memory indexes.
pub const PARSER_MEMORY: &str = "aim.memory/1";

const RULES_NOTE: &str = include_str!("../../prompts/resources/rules.md");
const SKILLS_NOTE: &str = include_str!("../../prompts/resources/skills.md");
const MEMORY_NOTE: &str = include_str!("../../prompts/resources/memory.md");

/// An always-on instruction file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    /// What and where it is.
    pub meta: Descriptor,
    /// Its text.
    pub text: String,
    /// The file was larger than what was read.
    pub truncated: bool,
}

impl Described for InstructionFile {
    fn descriptor(&self) -> &Descriptor {
        &self.meta
    }
}

/// A rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// What and where it is.
    pub meta: Descriptor,
    /// The globs it applies to (empty: everywhere).
    pub paths: Vec<String>,
    /// Its instructions.
    pub body: String,
}

impl Described for Rule {
    fn descriptor(&self) -> &Descriptor {
        &self.meta
    }
}

/// A memory index, cut to what the prefix shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryIndex {
    /// What and where it is.
    pub meta: Descriptor,
    /// The lines shown.
    pub text: String,
    /// More lines exist than are shown.
    pub cut: bool,
}

impl Described for MemoryIndex {
    fn descriptor(&self) -> &Descriptor {
        &self.meta
    }
}

/// The directories from the workspace root to `cwd` (relative to the root), root first: `""`,
/// `"a"`, `"a/b"`. `None` when `cwd` leaves the root.
#[must_use]
pub fn walk(cwd: &str) -> Option<Vec<String>> {
    let mut dirs = vec![String::new()];
    let mut current = String::new();
    for part in cwd.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            part => {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(part);
                dirs.push(current.clone());
            }
        }
    }
    Some(dirs)
}

/// `dir/name`, or `name` at the root.
#[must_use]
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

/// An instruction file as read.
#[must_use]
pub fn instruction_file(file: &FileText, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> InstructionFile {
    if file.truncated {
        diagnostics.push(origin.diagnostic(Problem::Truncated, format!("{} bytes; the first {} are read", file.size, file.text.len())));
    }
    InstructionFile {
        meta: origin.descriptor(Kind::Instructions, &origin.path, "instructions", &file.hash, PARSER_INSTRUCTIONS),
        text: file.text.trim().to_owned(),
        truncated: file.truncated,
    }
}

/// Parses rule `<stem>.md`: optional frontmatter with `description` and `paths` (a list of globs
/// or one string).
pub fn parse_rule(file: &FileText, stem: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<Rule> {
    if !is_addressable(stem) {
        diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`{stem}` is not a usable rule name; skipped")));
        return None;
    }
    let (mut fields, body) = match yaml::split(&file.text) {
        Split::None(body) => (Fields::default(), body),
        Split::Found { yaml, body } => match yaml::parse(yaml) {
            Ok(entries) => (Fields::new(entries), body),
            Err(message) => {
                diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; skipped")));
                return None;
            }
        },
        Split::Unterminated => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, "the frontmatter is not closed; skipped"));
            return None;
        }
    };
    let paths = match fields.list("paths") {
        Ok(paths) => paths.unwrap_or_default(),
        Err(message) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; skipped")));
            return None;
        }
    };
    let description = match fields.string("description") {
        Ok(Some(d)) => one_line(&d, 200),
        Ok(None) | Err(_) => one_line(body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default(), 120),
    };
    report_unsupported(&fields, origin, diagnostics);
    if file.truncated {
        diagnostics.push(origin.diagnostic(Problem::Truncated, format!("{} bytes; the first {} are read", file.size, file.text.len())));
    }
    Some(Rule { meta: origin.descriptor(Kind::Rule, stem, &description, &file.hash, PARSER_RULE), paths, body: body.trim().to_owned() })
}

/// A memory index cut to [`MEMORY_LINES`] lines and [`MEMORY_BYTES`] bytes.
#[must_use]
pub fn memory_index(file: &FileText, origin: &Origin) -> MemoryIndex {
    let mut text = String::new();
    let mut lines = file.text.lines();
    for line in lines.by_ref().take(MEMORY_LINES) {
        text.push_str(line);
        text.push('\n');
    }
    let more_lines = lines.next().is_some();
    let (kept, cut_bytes) = clip(&text, MEMORY_BYTES);
    MemoryIndex {
        meta: origin.descriptor(Kind::Memory, "MEMORY.md", "memory index", &file.hash, PARSER_MEMORY),
        text: kept.trim_end().to_owned(),
        cut: more_lines || cut_bytes || file.truncated,
    }
}

/// The skill catalog's budget in bytes: about 2% of a `window`-token context at ~4 bytes per
/// token, between 2 and 32 KiB; [`DEFAULT_SKILL_BUDGET`] when the window is unknown.
#[must_use]
pub fn skill_budget(window: Option<u64>) -> usize {
    window.map_or(DEFAULT_SKILL_BUDGET, |tokens| {
        let bytes = tokens.saturating_mul(4).saturating_mul(2) / 100;
        usize::try_from(bytes).unwrap_or(usize::MAX).clamp(2 * 1024, 32 * 1024)
    })
}

/// One skill as the catalog lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillLine {
    /// Its name.
    pub name: String,
    /// Its description.
    pub description: String,
    /// Where it is, or how it is activated.
    pub note: String,
}

/// A catalog fitted to its budget.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fitted {
    /// The listing, one line per skill.
    pub text: String,
    /// Skills listed.
    pub listed: usize,
    /// Descriptions were shortened.
    pub shortened: bool,
    /// Skills left out.
    pub omitted: usize,
}

fn render_lines(lines: &[SkillLine], max_description: usize) -> (String, bool) {
    let mut out = String::new();
    let mut shortened = false;
    for line in lines {
        let (description, cut) = clip(&line.description, max_description);
        shortened |= cut;
        let ellipsis = if cut { "…" } else { "" };
        let _infallible = writeln!(out, "- {}: {description}{ellipsis} ({})", line.name, line.note);
    }
    (out, shortened)
}

fn omitted_note(count: usize) -> String {
    format!("- …and {count} more skill(s) not listed here (catalog budget).\n")
}

/// Fits `lines` (in precedence order) into `budget` bytes: whole if they fit; else with every
/// description cut to the longest common length that fits (not below [`MIN_DESCRIPTION`]);
/// else with the last skills left out and counted. Pure.
#[must_use]
pub fn fit_skills(lines: &[SkillLine], budget: usize) -> Fitted {
    let longest = lines.iter().map(|l| l.description.len()).max().unwrap_or(0);
    let (full, _) = render_lines(lines, longest);
    if full.len() <= budget {
        return Fitted { text: full, listed: lines.len(), shortened: false, omitted: 0 };
    }
    // The longest description cap that fits, by bisection over [MIN_DESCRIPTION, longest).
    let (mut low, mut high) = (MIN_DESCRIPTION, longest);
    let mut best: Option<usize> = None;
    while low < high {
        let mid = low.saturating_add(high.saturating_sub(low) / 2);
        if render_lines(lines, mid).0.len() <= budget {
            best = Some(mid);
            low = mid.saturating_add(1);
        } else {
            high = mid;
        }
    }
    if let Some(cap) = best {
        let (text, shortened) = render_lines(lines, cap);
        return Fitted { text, listed: lines.len(), shortened, omitted: 0 };
    }
    let mut kept = lines.len();
    while kept > 0 {
        kept = kept.saturating_sub(1);
        let (text, shortened) = render_lines(lines.get(..kept).unwrap_or_default(), MIN_DESCRIPTION);
        let note = omitted_note(lines.len().saturating_sub(kept));
        if text.len().saturating_add(note.len()) <= budget || kept == 0 {
            return Fitted { text: text + &note, listed: kept, shortened, omitted: lines.len().saturating_sub(kept) };
        }
    }
    Fitted { text: omitted_note(lines.len()), listed: 0, shortened: false, omitted: lines.len() }
}

/// The prefix built from a catalog, and what the budgets cut.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefix {
    /// The text (without aim's system prompt).
    pub text: String,
    /// Cuts made to fit the budgets.
    pub diagnostics: Vec<Diagnostic>,
}

/// Where a resource comes from, when that is not simply the local workspace's native files.
fn labels(meta: &Descriptor) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    if meta.source != Source::Native {
        labels.push(meta.source.name().to_owned());
    }
    if meta.is_remote() {
        labels.push(format!("remote, {}", meta.location));
    }
    labels
}

fn provenance(meta: &Descriptor) -> String {
    let labels = labels(meta);
    if labels.is_empty() { String::new() } else { format!(" ({})", labels.join("; ")) }
}

/// `first; label; label`.
fn with_labels(first: String, meta: &Descriptor) -> String {
    std::iter::once(first).chain(labels(meta)).collect::<Vec<_>>().join("; ")
}

fn budget_note(problem: Problem, path: &str, scope: Scope, message: String) -> Diagnostic {
    Diagnostic { problem, path: path.to_owned(), scope, message }
}

/// Appends always-on files under the shared cap, outermost first.
fn render_project(catalog: &Catalog, out: &mut String, diagnostics: &mut Vec<Diagnostic>) {
    let global_rules: Vec<&Rule> = catalog.rules.iter().filter(|r| r.paths.is_empty()).collect();
    if catalog.instructions.is_empty() && global_rules.is_empty() {
        return;
    }
    out.push_str("\n# Project instructions\n\nFrom the workspace, outermost first: a later file is more specific.\n");
    let mut left = MAX_PROJECT_INSTRUCTIONS;
    let sections = catalog
        .instructions
        .iter()
        .map(|file| (format!("{}{}", file.meta.path, provenance(&file.meta)), file.text.as_str(), &file.meta))
        .chain(global_rules.iter().map(|rule| {
            (format!("Rule `{}` ({}){}", rule.meta.name, rule.meta.path, provenance(&rule.meta)), rule.body.as_str(), &rule.meta)
        }));
    for (heading, text, meta) in sections {
        if left == 0 {
            let _infallible = write!(
                out,
                "\n## {heading}\n\n[… not included: the {MAX_PROJECT_INSTRUCTIONS}-byte instruction budget is spent; read {} if needed]\n",
                meta.path
            );
            diagnostics.push(budget_note(
                Problem::Truncated,
                &meta.path,
                meta.scope,
                "left out: the project instruction budget is spent".to_owned(),
            ));
            continue;
        }
        let (kept, cut) = clip(text, left);
        left = left.saturating_sub(kept.len());
        let _infallible = write!(out, "\n## {heading}\n\n{kept}\n");
        if cut {
            let _also = writeln!(out, "\n[… truncated: {} of {} bytes shown; read {} for the rest]", kept.len(), text.len(), meta.path);
            diagnostics.push(budget_note(
                Problem::Truncated,
                &meta.path,
                meta.scope,
                format!("cut to {} bytes by the instruction budget", kept.len()),
            ));
            left = 0;
        }
    }
}

fn render_rules(catalog: &Catalog, out: &mut String) {
    let scoped: Vec<&Rule> = catalog.rules.iter().filter(|r| !r.paths.is_empty()).collect();
    if scoped.is_empty() {
        return;
    }
    let _infallible = write!(out, "\n# Rules for specific paths\n\n{}\n", RULES_NOTE.trim());
    for rule in scoped {
        let globs = rule.paths.iter().map(|g| format!("`{g}`")).collect::<Vec<_>>().join(", ");
        let _also = writeln!(
            out,
            "- `{}` — {globs}: {} (read {}){}",
            rule.meta.name,
            rule.meta.description,
            rule.meta.path,
            provenance(&rule.meta)
        );
    }
}

fn render_skills(catalog: &Catalog, budget: usize, out: &mut String, diagnostics: &mut Vec<Diagnostic>) {
    let lines: Vec<SkillLine> = catalog
        .skills
        .iter()
        .filter(|s| s.model_invocable)
        .map(|s| SkillLine {
            name: s.meta.name.clone(),
            description: s.meta.description.clone(),
            note: match s.meta.scope {
                Scope::Project => with_labels(s.meta.path.clone(), &s.meta),
                Scope::User => format!("user skill, outside the workspace: it arrives when the user writes ${}", s.meta.name),
            },
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let fitted = fit_skills(&lines, budget);
    let _infallible = write!(out, "\n# Skills\n\n{}\n\n{}", SKILLS_NOTE.trim(), fitted.text);
    if fitted.shortened || fitted.omitted > 0 {
        diagnostics.push(budget_note(
            Problem::Truncated,
            "skills",
            Scope::Project,
            format!(
                "skill catalog fitted to {budget} bytes: descriptions shortened: {}, skills left out: {}",
                fitted.shortened, fitted.omitted
            ),
        ));
    }
}

fn render_memory(catalog: &Catalog, out: &mut String) {
    if catalog.memory.is_empty() {
        return;
    }
    let _infallible = write!(out, "\n# Memory\n\n{}\n", MEMORY_NOTE.trim());
    for index in &catalog.memory {
        let whose = match index.meta.scope {
            Scope::Project => with_labels("project".to_owned(), &index.meta),
            Scope::User => "user, on the user's machine: its topic files are not reachable through the workspace tools".to_owned(),
        };
        let _also = write!(out, "\n## {} ({whose})\n\n{}\n", index.meta.path, index.text);
        if index.cut {
            let _more = writeln!(out, "[… more in {}]", index.meta.path);
        }
    }
}

/// Renders the prefix that follows aim's system prompt: project instructions, rules index, skill
/// catalog within `skill_budget` bytes, memory, and `agent`'s instructions.
#[must_use]
pub fn render(catalog: &Catalog, agent: Option<&AgentDef>, skill_budget: usize) -> Prefix {
    let mut out = String::new();
    let mut diagnostics = Vec::new();
    render_project(catalog, &mut out, &mut diagnostics);
    render_rules(catalog, &mut out);
    render_skills(catalog, skill_budget, &mut out, &mut diagnostics);
    render_memory(catalog, &mut out);
    if let Some(agent) = agent {
        let _infallible = write!(out, "\n# Agent: {}{}\n\n{}\n", agent.meta.name, provenance(&agent.meta), agent.instructions);
    }
    Prefix { text: out, diagnostics }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, description_len: usize) -> SkillLine {
        SkillLine { name: name.into(), description: "d".repeat(description_len), note: "p".into() }
    }

    #[test]
    fn walks_from_the_root_down() {
        assert_eq!(walk(""), Some(vec![String::new()]));
        assert_eq!(walk("./a//b/"), Some(vec![String::new(), "a".into(), "a/b".into()]));
        assert_eq!(walk("a/../b"), None);
        assert_eq!(join("a", "AGENTS.md"), "a/AGENTS.md");
    }

    #[test]
    fn budget_follows_the_window() {
        assert_eq!(skill_budget(None), 4096);
        assert_eq!(skill_budget(Some(200_000)), 16_000);
        assert_eq!(skill_budget(Some(1_000)), 2048);
        assert_eq!(skill_budget(Some(u64::MAX)), 32 * 1024);
    }

    #[test]
    fn catalog_shrinks_descriptions_before_dropping_skills() {
        let lines: Vec<SkillLine> = (0..10).map(|i| line(&format!("s{i}"), 200)).collect();
        let whole = fit_skills(&lines, 10_000);
        assert_eq!((whole.listed, whole.shortened, whole.omitted), (10, false, 0));
        let shrunk = fit_skills(&lines, 1_000);
        assert!(shrunk.text.len() <= 1_000, "{}", shrunk.text.len());
        assert_eq!((shrunk.listed, shrunk.shortened, shrunk.omitted), (10, true, 0));
        let dropped = fit_skills(&lines, 400);
        assert!(dropped.text.len() <= 400, "{}", dropped.text.len());
        assert!(dropped.omitted > 0 && dropped.listed + dropped.omitted == 10);
        assert!(dropped.text.ends_with(&omitted_note(dropped.omitted)));
        assert_eq!(fit_skills(&[], 10).text, "");
    }

    #[test]
    fn every_budget_is_respected_or_everything_is_dropped() {
        let lines: Vec<SkillLine> = (0..30).map(|i| line(&format!("skill-{i}"), 10 + i * 37 % 300)).collect();
        for budget in (0..12_000).step_by(97) {
            let fitted = fit_skills(&lines, budget);
            assert!(fitted.text.len() <= budget || fitted.listed == 0, "budget {budget}: {}", fitted.text.len());
            assert_eq!(fitted.listed + fitted.omitted, 30);
        }
    }
}
