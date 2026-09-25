//! Agent Skills: `<dir>/<name>/SKILL.md` (the open spec, <https://agentskills.io/specification>;
//! research/agents-conventions.md §1).
//!
//! The standard core is parsed exactly: `name` (1–64 lowercase letters, digits and single hyphens,
//! equal to the directory name) and `description` (1–1024 characters) are required; `license`,
//! `compatibility`, `metadata` and `allowed-tools` are accepted. A skill is addressed by its
//! **directory** name, so `$name` binds to what is on disk even when the frontmatter disagrees
//! (reported). Claude Code's `disable-model-invocation: true` keeps a skill out of the model's
//! catalog while `$name` still activates it; other extension fields are reported as unsupported.
//! Claude skills may omit `name` and `description` (Claude uses the directory name and the first
//! paragraph), and so may they here.

use super::files::FileText;
use super::yaml::{self, Fields, Split, Value};
use super::{Described, Descriptor, Diagnostic, Kind, Origin, Problem, Source, clip, is_addressable, one_line};

/// Parser version recorded in skill descriptors.
pub const PARSER: &str = "agentskills/1";
/// Longest description kept, in bytes (the spec's limit is 1024 characters).
pub const MAX_DESCRIPTION: usize = 1024;

/// Standard fields other than `name` and `description`.
const STANDARD: [&str; 4] = ["license", "compatibility", "metadata", "allowed-tools"];

/// A skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// What and where it is.
    pub meta: Descriptor,
    /// `SKILL.md` after its frontmatter: the instructions.
    pub body: String,
    /// The file was larger than what was read; the rest is at [`Descriptor::path`].
    pub truncated: bool,
    /// The file's full size in bytes.
    pub size: u64,
    /// Listed in the model's catalog (false: only an explicit `$name` activates it).
    pub model_invocable: bool,
}

impl Described for Skill {
    fn descriptor(&self) -> &Descriptor {
        &self.meta
    }
}

/// Whether `name` follows the Agent Skills naming rule.
#[must_use]
pub fn is_spec_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
}

/// The first paragraph of `body` as one line (Claude's description fallback).
fn first_paragraph(body: &str) -> String {
    let paragraph: Vec<&str> =
        body.lines().map(str::trim).skip_while(|l| l.is_empty() || l.starts_with('#')).take_while(|l| !l.is_empty()).collect();
    one_line(&paragraph.join(" "), 200)
}

/// Parses `SKILL.md` of directory `dir_name`. `None` (with a diagnostic) when it cannot be used.
pub fn parse(file: &FileText, dir_name: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<Skill> {
    let claude = origin.source == Source::Claude;
    if !is_addressable(dir_name) {
        diagnostics.push(origin.diagnostic(Problem::Invalid, format!("skill directory `{dir_name}` is not a usable name; skipped")));
        return None;
    }
    let (yaml, body) = match yaml::split(&file.text) {
        Split::Found { yaml, body } => (yaml, body),
        Split::None(_) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, "SKILL.md has no frontmatter (`name`, `description`); skipped"));
            return None;
        }
        Split::Unterminated => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, "SKILL.md's frontmatter is not closed; skipped"));
            return None;
        }
    };
    let mut fields = match yaml::parse(yaml) {
        Ok(entries) => Fields::new(entries),
        Err(message) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; skipped")));
            return None;
        }
    };
    let mut invalid = |message: String| diagnostics.push(origin.diagnostic(Problem::Invalid, message));
    let name = fields.string("name").unwrap_or_else(|message| {
        invalid(message);
        None
    });
    match name.as_deref() {
        None if !claude => invalid(format!("no `name`; addressed by its directory, `{dir_name}`")),
        Some(name) if name != dir_name => invalid(format!("`name: {name}` differs from its directory; addressed as `{dir_name}`")),
        _ => {}
    }
    if !is_spec_name(dir_name) {
        invalid(format!("`{dir_name}` is not an Agent Skills name (lowercase letters, digits, single hyphens)"));
    }
    let description = match fields.string("description") {
        Ok(Some(text)) if !text.trim().is_empty() => text.trim().to_owned(),
        Ok(_) if claude && !first_paragraph(body).is_empty() => first_paragraph(body),
        Ok(_) => {
            invalid("no `description`; skipped".to_owned());
            return None;
        }
        Err(message) => {
            invalid(format!("{message}; skipped"));
            return None;
        }
    };
    let (kept, cut) = clip(&description, MAX_DESCRIPTION);
    if cut {
        invalid(format!("`description` is over {MAX_DESCRIPTION} bytes; cut"));
    }
    let description = kept.to_owned();
    let hidden = matches!(fields.take("disable-model-invocation"), Some(Value::Str(v)) if v == "true");
    for key in STANDARD {
        let _accepted = fields.take(key);
    }
    report_unsupported(&fields, origin, diagnostics);
    if file.truncated {
        diagnostics.push(
            origin.diagnostic(Problem::Truncated, format!("SKILL.md is {} bytes; the first {} are loaded", file.size, file.text.len())),
        );
    }
    Some(Skill {
        meta: origin.descriptor(Kind::Skill, dir_name, &description, &file.hash, PARSER),
        body: body.trim().to_owned(),
        truncated: file.truncated,
        size: file.size,
        model_invocable: !hidden,
    })
}

/// Reports frontmatter fields left over after parsing (one diagnostic per file).
pub fn report_unsupported(fields: &Fields, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) {
    let rest = fields.rest();
    if rest.is_empty() {
        return;
    }
    let executable: Vec<&str> = rest.iter().map(String::as_str).filter(|k| matches!(*k, "hooks" | "mcpServers" | "mcp_servers")).collect();
    let mut message = format!("ignored fields: {}", rest.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", "));
    if !executable.is_empty() {
        message.push_str(" (hooks and MCP servers run code and need an explicit opt-in; none is started)");
    }
    diagnostics.push(origin.diagnostic(Problem::Unsupported, message));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{Scope, Source};

    fn origin(source: Source) -> Origin {
        Origin { scope: Scope::Project, source, location: "local".into(), path: ".agents/skills/x/SKILL.md".into() }
    }

    fn file(text: &str) -> FileText {
        FileText { text: text.into(), hash: "sha256:0".into(), size: text.len() as u64, truncated: false }
    }

    #[test]
    fn parses_the_standard_core() {
        let mut diagnostics = Vec::new();
        let skill = parse(
            &file("---\nname: haiku\ndescription: Reply in a haiku.\nlicense: MIT\nmetadata:\n  a: b\n---\n# Haiku\nWrite 5-7-5.\n"),
            "haiku",
            &origin(Source::Native),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!((skill.meta.name.as_str(), skill.meta.description.as_str()), ("haiku", "Reply in a haiku."));
        assert_eq!(skill.body, "# Haiku\nWrite 5-7-5.");
        assert!(skill.model_invocable);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn binds_by_directory_and_reports_disagreements() {
        let mut diagnostics = Vec::new();
        let text = "---\nname: Other\ndescription: d\nwhen_to_use: x\ndisable-model-invocation: true\n---\nbody";
        let skill = parse(&file(text), "My_Skill", &origin(Source::Native), &mut diagnostics).unwrap();
        assert_eq!(skill.meta.name, "My_Skill");
        assert!(!skill.model_invocable);
        let problems: Vec<Problem> = diagnostics.iter().map(|d| d.problem).collect();
        assert_eq!(problems, [Problem::Invalid, Problem::Invalid, Problem::Unsupported], "{diagnostics:?}");
    }

    #[test]
    fn claude_skills_may_omit_name_and_description() {
        let mut diagnostics = Vec::new();
        let skill = parse(
            &file("---\nallowed-tools: Read\n---\n# T\n\nDo the thing\nwell.\n\nMore."),
            "thing",
            &origin(Source::Claude),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(skill.meta.description, "Do the thing well.");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut diagnostics = Vec::new();
        assert!(parse(&file("---\nname: thing\n---\nbody"), "thing", &origin(Source::Native), &mut diagnostics).is_none());
        assert!(diagnostics[0].message.contains("no `description`"));
    }
}
