//! Explicit skill activation (ADR 0014; tny ADR 0056): `$name` in a user prompt, or a prompt
//! starting with `/skill name` (the TUI's command), puts that skill's instructions **into the user
//! turn**, so the cached instruction prefix never changes.
//!
//! **Matcher** (tny ADR 0056): `$name` counts when `$` starts the text or follows whitespace, and
//! the name is followed by the end, whitespace, or punctuation other than `/`, `-` and `_`; so
//! `$deploy.` activates `deploy` while `$deploy-prod`, `a$deploy` and `$deploy/x` do not. Since
//! names may contain dots, a `.` ends a name only before whitespace or the end of the text. Names
//! are exact and case-sensitive. A name that is not a skill (`$HOME`) is left alone.
//!
//! **Placement.** Each mentioned skill is injected once per prompt, in order of first mention, as
//! a `<skill name="…" path="…">` block placed just before the first part that mentions a skill:
//! the request the model acts on stays the last thing it reads, and aim's environment block (the
//! first part of a session's first prompt) stays first. The user's text is not changed.
//!
//! **Compaction.** A skill is injected on every explicit mention, with no "already loaded"
//! bookkeeping, so a mention after a compaction summarized an earlier activation away still
//! delivers the full instructions. Within a turn the prompt is pinned, so the injected body
//! survives compaction verbatim.

use std::fmt::Write as _;

use aim_proto::conversation::Part;

use super::skills::Skill;
use super::{Catalog, Scope};

/// Characters that may continue a name (so they end no mention).
const fn continues_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_')
}

/// The skill names `text` mentions, in order of first mention: known skills, then the unknown
/// `$names` (as written, without `$`).
#[must_use]
pub fn mentions(text: &str, catalog: &Catalog) -> (Vec<String>, Vec<String>) {
    let mut known: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let remember = |name: &str, list: &mut Vec<String>| {
        if !list.iter().any(|n| n == name) {
            list.push(name.to_owned());
        }
    };
    if let Some(rest) = text.trim_start().strip_prefix("/skill")
        && rest.starts_with(char::is_whitespace)
    {
        let name = rest.split_whitespace().next().unwrap_or_default();
        if catalog.skill(name).is_some() {
            remember(name, &mut known);
        } else if !name.is_empty() {
            remember(name, &mut unknown);
        }
    }
    let mut previous: Option<char> = None;
    for (at, c) in text.char_indices() {
        if c == '$' && previous.is_none_or(char::is_whitespace) {
            let after = text.get(at.saturating_add(1)..).unwrap_or_default();
            // The longest known name that ends where a mention may end.
            let hit = catalog
                .skills
                .iter()
                .map(|s| s.meta.name.as_str())
                .filter(|name| {
                    after
                        .strip_prefix(name)
                        .is_some_and(|tail| (!tail.starts_with(continues_name) && !tail.starts_with('.')) || ends_sentence(tail))
                })
                .max_by_key(|name| name.len());
            if let Some(name) = hit {
                remember(name, &mut known);
            } else {
                let word: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')).collect();
                let word = word.trim_end_matches('.');
                if word.starts_with(|c: char| c.is_ascii_alphabetic()) {
                    remember(word, &mut unknown);
                }
            }
        }
        previous = Some(c);
    }
    (known, unknown)
}

/// Whether `tail` (after a name ending in or followed by `.`) ends the mention: a `.` followed by
/// the end or whitespace is punctuation, while `.x` continues a dotted name.
fn ends_sentence(tail: &str) -> bool {
    tail.strip_prefix('.').is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// The block that carries a skill's instructions into a user turn.
#[must_use]
pub fn skill_block(skill: &Skill) -> String {
    let mut block = format!("<skill name=\"{}\" path=\"{}\"", skill.meta.name, skill.meta.path);
    if skill.meta.scope == Scope::User {
        block.push_str(" scope=\"user\"");
    }
    block.push_str(">\n");
    block.push_str(&skill.body);
    if skill.truncated {
        let _infallible = write!(
            block,
            "\n[… truncated: the first {} of {} bytes of SKILL.md are shown; the rest is in {}]",
            skill.body.len(),
            skill.size,
            skill.meta.path
        );
    }
    block.push_str("\n</skill>");
    block
}

/// `parts` with the skills they mention injected (see the module docs). Unknown mentions change
/// nothing.
#[must_use]
pub fn expand_mentions(parts: Vec<Part>, catalog: &Catalog) -> Vec<Part> {
    let mut first_mention: Option<usize> = None;
    let mut names: Vec<String> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Part::Text { text } = part {
            let (known, _) = mentions(text, catalog);
            if !known.is_empty() && first_mention.is_none() {
                first_mention = Some(index);
            }
            for name in known {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    let Some(at) = first_mention else {
        return parts;
    };
    let blocks = names.iter().filter_map(|name| catalog.skill(name)).map(|skill| Part::Text { text: skill_block(skill) });
    let mut out = Vec::with_capacity(parts.len().saturating_add(names.len()));
    let mut parts = parts.into_iter();
    out.extend(parts.by_ref().take(at));
    out.extend(blocks);
    out.extend(parts);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{Descriptor, Kind, Source, Trust};

    fn skill(name: &str) -> Skill {
        Skill {
            meta: Descriptor {
                kind: Kind::Skill,
                name: name.into(),
                description: "d".into(),
                path: format!(".agents/skills/{name}/SKILL.md"),
                scope: Scope::Project,
                source: Source::Native,
                trust: Trust::Workspace,
                location: "local".into(),
                hash: "sha256:0".into(),
                parser_version: "agentskills/1",
            },
            body: format!("{name} body"),
            truncated: false,
            size: 9,
            model_invocable: true,
        }
    }

    fn catalog() -> Catalog {
        Catalog { skills: vec![skill("deploy"), skill("deploy-prod"), skill("v2.1")], ..Catalog::default() }
    }

    #[test]
    fn matches_whole_tokens_only() {
        let c = catalog();
        let known = |text: &str| mentions(text, &c).0;
        assert_eq!(known("$deploy now"), ["deploy"]);
        assert_eq!(known("run $deploy."), ["deploy"]);
        assert_eq!(known("run $deploy, then"), ["deploy"]);
        assert_eq!(known("$deploy-prod"), ["deploy-prod"]);
        assert_eq!(known("$v2.1 please"), ["v2.1"]);
        for none in ["a$deploy", "$deploy/x", "$deploy_x", "$Deploy", "`$deploy`", "$deployx"] {
            assert!(known(none).is_empty(), "{none}");
        }
        assert_eq!(known("/skill deploy the app"), ["deploy"]);
        assert_eq!(known("$deploy and $deploy again, $deploy-prod"), ["deploy", "deploy-prod"]);
    }

    #[test]
    fn unknown_mentions_are_reported_and_change_nothing() {
        let c = catalog();
        assert_eq!(mentions("echo $HOME and $nope. $5", &c), (vec![], vec!["HOME".to_owned(), "nope".to_owned()]));
        let parts = vec![Part::Text { text: "use $nope".into() }];
        assert_eq!(expand_mentions(parts.clone(), &c), parts);
    }

    #[test]
    fn injects_before_the_first_mentioning_part() {
        let c = catalog();
        let parts = vec![
            Part::Text { text: "<environment/>".into() },
            Part::Text { text: "do $deploy-prod then $deploy".into() },
            Part::Text { text: "and $deploy".into() },
        ];
        let out = expand_mentions(parts, &c);
        let texts: Vec<&str> = out
            .iter()
            .map(|p| match p {
                Part::Text { text } => text.as_str(),
                Part::Image { .. } => "",
            })
            .collect();
        assert_eq!(texts.len(), 5);
        assert_eq!(texts[0], "<environment/>");
        assert!(texts[1].starts_with("<skill name=\"deploy-prod\" path=\".agents/skills/deploy-prod/SKILL.md\">\ndeploy-prod body"));
        assert!(texts[2].starts_with("<skill name=\"deploy\""));
        assert_eq!(texts[3], "do $deploy-prod then $deploy");
    }
}
