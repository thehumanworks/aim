//! Prompt templates (slash commands): `.agents/prompts/<name>.md`, `~/.aim/prompts/<name>.md`,
//! and, read-only, Claude Code's `.claude/commands/[<ns>/]<name>.md` (addressed `ns:name`) and
//! pi's `.pi/prompts/<name>.md` (research/agents-conventions.md §6).
//!
//! Frontmatter is optional (`description`, `argument-hint`; Claude also `arguments`); without a
//! description, the first non-empty line describes the template. Each source keeps its own
//! argument grammar ([`Dialect`]); arguments are split shell-style (`"two words"` is one).
//! Nothing is executed: Claude's `` !`command` `` insertions stay literal text (reported).

use super::files::FileText;
use super::skills::report_unsupported;
use super::yaml::{self, Fields, Split};
use super::{Described, Descriptor, Diagnostic, Kind, Origin, Problem, Source, is_addressable, one_line};

/// Parser version of aim prompt templates.
pub const PARSER_AIM: &str = "aim.prompt/1";
/// Parser version of Claude Code commands.
pub const PARSER_CLAUDE: &str = "claude.command/1";
/// Parser version of pi prompt templates.
pub const PARSER_PI: &str = "pi.prompt/1";

/// An argument grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// aim (and Codex's custom prompts): `$ARGUMENTS` is the whole text, `$1`…`$9` the
    /// arguments from one, `$$` a dollar sign.
    Aim,
    /// Claude Code: `$ARGUMENTS`, `$ARGUMENTS[N]` and `$N` counting from **zero**, `$name` for a
    /// name in `arguments`; an index with no argument stays literal; when arguments were given but
    /// nothing used them, `ARGUMENTS: <text>` is appended.
    Claude,
    /// pi: `$1`…, `$@` / `$ARGUMENTS`, `${N:-default}`, `${@:-default}`, `${@:N}`, `${@:N:L}`.
    Pi,
}

/// A prompt template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// What and where it is.
    pub meta: Descriptor,
    /// What arguments it expects, for completion.
    pub argument_hint: Option<String>,
    /// The template text.
    pub template: String,
    /// Its argument grammar.
    pub dialect: Dialect,
    /// Claude's named arguments, by position.
    pub named: Vec<String>,
}

impl Described for Prompt {
    fn descriptor(&self) -> &Descriptor {
        &self.meta
    }
}

impl Prompt {
    /// The template expanded with `args` (the text after the command).
    #[must_use]
    pub fn expand(&self, args: &str) -> String {
        expand(&self.template, self.dialect, &self.named, args)
    }
}

/// Parses template `name` (its file stem, `ns:stem` for a namespaced Claude command).
pub fn parse(file: &FileText, name: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<Prompt> {
    if !is_addressable(name) {
        diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`{name}` is not a usable command name; skipped")));
        return None;
    }
    let (dialect, parser) = match origin.source {
        Source::Claude => (Dialect::Claude, PARSER_CLAUDE),
        Source::Pi => (Dialect::Pi, PARSER_PI),
        Source::Native | Source::Codex | Source::OhMyPi => (Dialect::Aim, PARSER_AIM),
    };
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
    let mut take = |key: &str, diagnostics: &mut Vec<Diagnostic>| {
        fields.string(key).unwrap_or_else(|message| {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; ignored")));
            None
        })
    };
    let description = take("description", diagnostics)
        .map_or_else(|| one_line(body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default(), 120), |d| one_line(&d, 200));
    let argument_hint = take("argument-hint", diagnostics);
    let named = if dialect == Dialect::Claude {
        fields.list("arguments").unwrap_or_else(|message| {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; ignored")));
            None
        })
    } else {
        None
    }
    .unwrap_or_default();
    report_unsupported(&fields, origin, diagnostics);
    if dialect == Dialect::Claude && body.contains("!`") {
        diagnostics.push(origin.diagnostic(Problem::Unsupported, "shell insertions (!`…`) are not run; they stay as text"));
    }
    if file.truncated {
        diagnostics.push(origin.diagnostic(Problem::Truncated, format!("{} bytes; the first {} are loaded", file.size, file.text.len())));
    }
    Some(Prompt {
        meta: origin.descriptor(Kind::Prompt, name, &description, &file.hash, parser),
        argument_hint,
        template: body.trim().to_owned(),
        dialect,
        named,
    })
}

/// Splits arguments shell-style: whitespace separates, quotes group, `\` escapes (not inside
/// single quotes).
#[must_use]
pub fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (_, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Expands `template` in `dialect` with `args`.
#[must_use]
pub fn expand(template: &str, dialect: Dialect, named: &[String], args: &str) -> String {
    let raw = args.trim();
    let list = split_args(raw);
    let mut out = String::with_capacity(template.len().saturating_add(raw.len()));
    let mut used = false;
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at.saturating_add(1)..).unwrap_or_default();
        let (replacement, consumed) = placeholder(after, dialect, named, raw, &list);
        match replacement {
            Some(text) => {
                out.push_str(&text);
                used = true;
            }
            None => out.push_str(rest.get(at..at.saturating_add(1).saturating_add(consumed)).unwrap_or("$")),
        }
        rest = after.get(consumed..).unwrap_or_default();
    }
    out.push_str(rest);
    if dialect == Dialect::Claude && !used && !raw.is_empty() {
        out.push_str("\n\nARGUMENTS: ");
        out.push_str(raw);
    }
    out
}

/// The leading run of ASCII digits of `text` and its value.
fn digits(text: &str) -> Option<(usize, usize)> {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    let value = text.get(..len)?.parse().ok()?;
    Some((value, len))
}

/// What `$` followed by `after` expands to, and how many bytes of `after` it used. `None`
/// leaves the text as written.
fn placeholder(after: &str, dialect: Dialect, named: &[String], raw: &str, args: &[String]) -> (Option<String>, usize) {
    let arg = |index: usize| args.get(index).cloned();
    if dialect == Dialect::Aim && after.starts_with('$') {
        return (Some("$".to_owned()), 1);
    }
    if let Some(index_part) = after.strip_prefix("ARGUMENTS[")
        && dialect == Dialect::Claude
        && let Some((index, len)) = digits(index_part)
        && index_part.get(len..).is_some_and(|r| r.starts_with(']'))
    {
        let consumed = "ARGUMENTS[".len().saturating_add(len).saturating_add(1);
        return (arg(index), consumed);
    }
    if after.starts_with("ARGUMENTS") {
        return (Some(raw.to_owned()), "ARGUMENTS".len());
    }
    if dialect == Dialect::Pi {
        if after.starts_with('@') {
            return (Some(args.join(" ")), 1);
        }
        if let Some(inner) = after.strip_prefix('{')
            && let Some(close) = inner.find('}')
        {
            let consumed = close.saturating_add(2);
            return (pi_brace(inner.get(..close).unwrap_or_default(), args), consumed);
        }
    }
    if let Some((index, len)) = digits(after) {
        // aim and pi count from one (`$0` is not an argument); Claude from zero.
        return match dialect {
            Dialect::Claude => (arg(index), len),
            Dialect::Aim | Dialect::Pi if index >= 1 => (Some(arg(index.saturating_sub(1)).unwrap_or_default()), len),
            Dialect::Aim | Dialect::Pi => (None, len),
        };
    }
    if dialect == Dialect::Claude {
        let word_len = after.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
        let word = after.get(..word_len).unwrap_or_default();
        if let Some(position) = named.iter().position(|n| n == word) {
            return (arg(position), word_len);
        }
    }
    (None, 0)
}

/// pi's `${…}` forms.
fn pi_brace(inner: &str, args: &[String]) -> Option<String> {
    if let Some((what, default)) = inner.split_once(":-") {
        let value = if what == "@" { Some(args.join(" ")).filter(|v| !v.is_empty()) } else { index1(what, args) };
        return Some(value.unwrap_or_else(|| default.to_owned()));
    }
    if let Some(slice) = inner.strip_prefix("@:") {
        let (start, len) = slice.split_once(':').map_or((slice, None), |(s, l)| (s, Some(l)));
        let start: usize = start.parse().ok()?;
        let from = start.saturating_sub(1).min(args.len());
        let rest = args.get(from..).unwrap_or_default();
        let taken = match len {
            Some(len) => rest.iter().take(len.parse().ok()?).cloned().collect::<Vec<_>>(),
            None => rest.to_vec(),
        };
        return Some(taken.join(" "));
    }
    index1(inner, args).or_else(|| inner.parse::<usize>().ok().map(|_| String::new()))
}

fn index1(what: &str, args: &[String]) -> Option<String> {
    let index: usize = what.parse().ok()?;
    args.get(index.checked_sub(1)?).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(split_args(r#"one "two words" 'it''s' a\ b """#), ["one", "two words", "its", "a b", ""]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn aim_counts_from_one() {
        assert_eq!(
            expand("Fix $1 in $2 ($ARGUMENTS) for $$5; $0 $9", Dialect::Aim, &[], "bug main.rs"),
            "Fix bug in main.rs (bug main.rs) for $5; $0 "
        );
        assert_eq!(expand("no placeholders", Dialect::Aim, &[], "x"), "no placeholders");
    }

    #[test]
    fn claude_counts_from_zero_and_appends_unused_arguments() {
        let named = vec!["target".to_owned()];
        assert_eq!(
            expand("Migrate $ARGUMENTS[0] from $1 to $2; $target; $5 stays", Dialect::Claude, &named, "SearchBar JS TS"),
            "Migrate SearchBar from JS to TS; SearchBar; $5 stays"
        );
        assert_eq!(expand("Review.", Dialect::Claude, &[], "the diff"), "Review.\n\nARGUMENTS: the diff");
        assert_eq!(expand("Cost: $PATH and ${CLAUDE_SESSION_ID}", Dialect::Claude, &[], ""), "Cost: $PATH and ${CLAUDE_SESSION_ID}");
    }

    #[test]
    fn pi_has_defaults_and_slices() {
        let t = "Focus on ${1:-correctness}; all: $@; from 2: ${@:2}; one from 2: ${@:2:1}; or ${@:-none}; $2";
        assert_eq!(expand(t, Dialect::Pi, &[], "a b c"), "Focus on a; all: a b c; from 2: b c; one from 2: b; or a b c; b");
        assert_eq!(expand(t, Dialect::Pi, &[], ""), "Focus on correctness; all: ; from 2: ; one from 2: ; or none; ");
    }
}
