//! Agent definitions (docs/architecture.md §6.8, research/agents-conventions.md §3).
//!
//! **Native** — `.agents/agents/<name>.md` and `~/.aim/agents/<name>.md`: YAML frontmatter with
//! `schema: aim.agent/v1`, `name`, `description`, and optional `provider`, `model`, `effort` and
//! `tools` (an allowlist of tool names); the body is the agent's instructions. The file name is
//! the agent's address (`SessionSpec::agent`).
//!
//! **Imported, read-only** — Claude Code's `.claude/agents/*.md` and Codex's
//! `.codex/agents/*.toml`. An import is only applied when it maps **exactly**: an allowlist that
//! names a tool aim does not offer under the same name and shape, a restriction aim cannot enforce
//! (Claude `permissionMode: plan`, hooks; Codex `sandbox_mode = "read-only"`) makes the agent
//! *not importable* — listed with the reason, refused if selected. Fields that would only add
//! (MCP servers) or that name another harness's models are reported and ignored.

use std::collections::BTreeSet;

use aim_proto::event::SessionAgent;

use super::files::FileText;
use super::skills::report_unsupported;
use super::yaml::{self, Fields, Split, Value};
use super::{Described, Descriptor, Diagnostic, Kind, Origin, Problem, is_addressable};

/// The schema a native agent declares.
pub const SCHEMA: &str = "aim.agent/v1";
/// Parser version of native agents.
pub const PARSER_AIM: &str = "aim.agent/v1";
/// Parser version of imported Claude Code agents.
pub const PARSER_CLAUDE: &str = "claude.agent/1";
/// Parser version of imported Codex agents.
pub const PARSER_CODEX: &str = "codex.agent/1";

/// Claude Code tools that aim's harness offers under the same name and argument shape (aimx
/// mirrors Claude Code's tools, docs/architecture.md §6.1). A Claude allowlist naming only these
/// maps exactly.
pub const CLAUDE_TOOLS: [&str; 9] = ["Read", "Write", "Edit", "LS", "Glob", "Grep", "Bash", "BashOutput", "KillShell"];

/// Which tools an agent may use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Only these (`None`: every tool the session offers).
    pub allow: Option<BTreeSet<String>>,
    /// Never these.
    pub deny: BTreeSet<String>,
}

impl ToolPolicy {
    /// Whether tool `name` is permitted.
    #[must_use]
    pub fn permits(&self, name: &str) -> bool {
        !self.deny.contains(name) && self.allow.as_ref().is_none_or(|allow| allow.contains(name))
    }

    /// Whether every tool is permitted.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty()
    }

    /// What both policies permit: allowlists intersect and denylists unite, so the result never
    /// permits a tool either refuses (a resumed session's ceiling, ADR 0038).
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        let allow = match (&self.allow, &other.allow) {
            (None, None) => None,
            (Some(only), None) | (None, Some(only)) => Some(only.clone()),
            (Some(a), Some(b)) => Some(a.intersection(b).cloned().collect()),
        };
        Self { allow, deny: self.deny.union(&other.deny).cloned().collect() }
    }

    /// This policy as recorded for agent `name` in the session's metadata (ADR 0038).
    #[must_use]
    pub fn record(&self, name: &str) -> SessionAgent {
        SessionAgent {
            name: name.to_owned(),
            allow: self.allow.as_ref().map(|allow| allow.iter().cloned().collect()),
            deny: self.deny.iter().cloned().collect(),
        }
    }

    /// The policy a session recorded.
    #[must_use]
    pub fn recorded(record: &SessionAgent) -> Self {
        Self { allow: record.allow.as_ref().map(|allow| allow.iter().cloned().collect()), deny: record.deny.iter().cloned().collect() }
    }
}

/// An agent definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDef {
    /// What and where it is.
    pub meta: Descriptor,
    /// The provider its `model` belongs to.
    pub provider: Option<String>,
    /// Default model.
    pub model: Option<String>,
    /// Default reasoning effort.
    pub effort: Option<String>,
    /// Which tools it may use.
    pub tools: ToolPolicy,
    /// Its instructions (appended to the session's).
    pub instructions: String,
    /// `Err(reason)` when it cannot be applied exactly (an import aim cannot honour).
    pub importable: Result<(), String>,
}

impl Described for AgentDef {
    fn descriptor(&self) -> &Descriptor {
        &self.meta
    }
}

/// A session's model and effort once an agent's defaults are applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Defaults {
    /// The model to use when the session named none.
    pub model: Option<String>,
    /// The effort to use when the session named none.
    pub effort: Option<String>,
    /// Why the agent's defaults were not applied, when they were not.
    pub note: Option<String>,
}

impl AgentDef {
    /// The agent's defaults for a session on `provider`: explicit session values win; the
    /// agent's model and effort apply only on its own provider (or when it names none), since a
    /// model id belongs to one provider's catalog.
    #[must_use]
    pub fn defaults(&self, provider: &str, model: Option<&str>, effort: Option<&str>) -> Defaults {
        if let Some(own) = self.provider.as_deref()
            && own != provider
        {
            let note = (self.model.is_some() || self.effort.is_some()).then(|| {
                format!(
                    "agent `{}` is for provider `{own}`; the session uses `{provider}`, so its model and effort are not applied",
                    self.meta.name
                )
            });
            return Defaults { model: model.map(str::to_owned), effort: effort.map(str::to_owned), note };
        }
        Defaults {
            model: model.map(str::to_owned).or_else(|| self.model.clone()),
            effort: effort.map(str::to_owned).or_else(|| self.effort.clone()),
            note: None,
        }
    }
}

fn frontmatter<'a>(file: &'a FileText, origin: &Origin, diagnostics: &mut Vec<Diagnostic>, what: &str) -> Option<(Fields, &'a str)> {
    let (yaml, body) = match yaml::split(&file.text) {
        Split::Found { yaml, body } => (yaml, body),
        Split::None(_) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("no frontmatter; {what}; skipped")));
            return None;
        }
        Split::Unterminated => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, "the frontmatter is not closed; skipped"));
            return None;
        }
    };
    match yaml::parse(yaml) {
        Ok(entries) => Some((Fields::new(entries), body)),
        Err(message) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; skipped")));
            None
        }
    }
}

fn string(fields: &mut Fields, key: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<String> {
    match fields.string(key) {
        Ok(value) => value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()),
        Err(message) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; ignored")));
            None
        }
    }
}

fn truncated(file: &FileText, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) {
    if file.truncated {
        diagnostics.push(origin.diagnostic(Problem::Truncated, format!("{} bytes; the first {} are loaded", file.size, file.text.len())));
    }
}

/// Parses a native `aim.agent/v1` definition from file `<stem>.md`.
pub fn parse_native(file: &FileText, stem: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<AgentDef> {
    if !is_addressable(stem) {
        diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`{stem}` is not a usable agent name; skipped")));
        return None;
    }
    let (mut fields, body) = frontmatter(file, origin, diagnostics, "an aim agent starts with `schema: aim.agent/v1` frontmatter")?;
    match string(&mut fields, "schema", origin, diagnostics).as_deref() {
        Some(SCHEMA) => {}
        Some(other) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`schema: {other}` is not `{SCHEMA}`; skipped")));
            return None;
        }
        None => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("no `schema: {SCHEMA}`; skipped")));
            return None;
        }
    }
    match string(&mut fields, "name", origin, diagnostics) {
        None => diagnostics.push(origin.diagnostic(Problem::Invalid, format!("no `name`; addressed by its file name, `{stem}`"))),
        Some(name) if name != stem => {
            diagnostics
                .push(origin.diagnostic(Problem::Invalid, format!("`name: {name}` differs from its file name; addressed as `{stem}`")));
        }
        Some(_) => {}
    }
    let description = string(&mut fields, "description", origin, diagnostics).unwrap_or_else(|| {
        diagnostics.push(origin.diagnostic(Problem::Invalid, "no `description`"));
        String::new()
    });
    let provider = string(&mut fields, "provider", origin, diagnostics);
    let model = string(&mut fields, "model", origin, diagnostics);
    let effort = string(&mut fields, "effort", origin, diagnostics);
    let allow = match fields.list("tools") {
        Ok(tools) => tools.map(|t| t.into_iter().collect::<BTreeSet<_>>()),
        Err(message) => {
            // An allowlist that cannot be read must not become "every tool".
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{message}; skipped")));
            return None;
        }
    };
    report_unsupported(&fields, origin, diagnostics);
    truncated(file, origin, diagnostics);
    Some(AgentDef {
        meta: origin.descriptor(Kind::Agent, stem, &description, &file.hash, PARSER_AIM),
        provider,
        model,
        effort,
        tools: ToolPolicy { allow, deny: BTreeSet::new() },
        instructions: body.trim().to_owned(),
        importable: Ok(()),
    })
}

/// Parses a Claude Code agent (`.claude/agents/<stem>.md`), addressed by its `name`.
pub fn parse_claude(file: &FileText, stem: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<AgentDef> {
    let (mut fields, body) = frontmatter(file, origin, diagnostics, "a Claude agent needs `name` and `description`")?;
    let name = match string(&mut fields, "name", origin, diagnostics) {
        Some(name) if is_addressable(&name) => name,
        other => {
            if !is_addressable(stem) {
                diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`{stem}` is not a usable agent name; skipped")));
                return None;
            }
            let why = other.map_or_else(|| "no `name`".to_owned(), |n| format!("`name: {n}` is not a usable name"));
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("{why}; addressed by its file name, `{stem}`")));
            stem.to_owned()
        }
    };
    let description = string(&mut fields, "description", origin, diagnostics).unwrap_or_else(|| {
        diagnostics.push(origin.diagnostic(Problem::Invalid, "no `description`"));
        String::new()
    });
    let mut refusals: Vec<String> = Vec::new();
    let allow = match fields.list("tools") {
        Ok(None) => None,
        Ok(Some(tools)) => {
            let unmapped: Vec<&String> = tools.iter().filter(|t| !CLAUDE_TOOLS.contains(&t.as_str())).collect();
            if !unmapped.is_empty() {
                let names = unmapped.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ");
                refusals.push(format!("its tools {names} have no exact aim equivalent"));
            }
            Some(tools.into_iter().collect::<BTreeSet<_>>())
        }
        Err(message) => {
            refusals.push(message);
            Some(BTreeSet::new())
        }
    };
    // Denying a tool aim does not offer is already true, so any deny list maps exactly.
    let deny = match fields.list("disallowedTools") {
        Ok(tools) => tools.unwrap_or_default().into_iter().collect(),
        Err(message) => {
            refusals.push(message);
            BTreeSet::new()
        }
    };
    if let Some(mode) = string(&mut fields, "permissionMode", origin, diagnostics) {
        if mode == "plan" {
            refusals.push("aim cannot hold it to Claude's read-only `permissionMode: plan`".to_owned());
        } else {
            diagnostics.push(origin.diagnostic(
                Problem::Unsupported,
                format!("`permissionMode: {mode}` ignored: aim runs tools without asking, within its ceilings (ADR 0021)"),
            ));
        }
    }
    if fields.take("hooks").is_some() {
        refusals.push("its hooks may block tool calls, and aim does not run imported hooks".to_owned());
    }
    if let Some(model) = string(&mut fields, "model", origin, diagnostics) {
        diagnostics
            .push(origin.diagnostic(Problem::Unsupported, format!("`model: {model}` names a Claude model; the session's model is used")));
    }
    report_unsupported(&fields, origin, diagnostics);
    truncated(file, origin, diagnostics);
    let importable = if refusals.is_empty() {
        Ok(())
    } else {
        let reason = refusals.join("; ");
        diagnostics.push(origin.diagnostic(Problem::NotImportable, format!("agent `{name}` is not importable: {reason}")));
        Err(reason)
    };
    Some(AgentDef {
        meta: origin.descriptor(Kind::Agent, &name, &description, &file.hash, PARSER_CLAUDE),
        provider: None,
        model: None,
        effort: None,
        tools: ToolPolicy { allow, deny },
        instructions: body.trim().to_owned(),
        importable,
    })
}

/// Parses a Codex agent role (`.codex/agents/<stem>.toml`), addressed by its `name`.
pub fn parse_codex(file: &FileText, stem: &str, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) -> Option<AgentDef> {
    let mut table = match file.text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(err) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("invalid TOML: {}; skipped", err.message())));
            return None;
        }
    };
    let mut text = |key: &str, diagnostics: &mut Vec<Diagnostic>| match table.remove(key) {
        None => None,
        Some(toml::Value::String(value)) => Some(value.trim().to_owned()).filter(|v| !v.is_empty()),
        Some(other) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`{key}` must be a string, not {}; ignored", other.type_str())));
            None
        }
    };
    let name = match text("name", diagnostics) {
        Some(name) if is_addressable(&name) => name,
        _ if is_addressable(stem) => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("no usable `name`; addressed by its file name, `{stem}`")));
            stem.to_owned()
        }
        _ => {
            diagnostics.push(origin.diagnostic(Problem::Invalid, format!("`{stem}` is not a usable agent name; skipped")));
            return None;
        }
    };
    let description = text("description", diagnostics).unwrap_or_default();
    let Some(instructions) = text("developer_instructions", diagnostics) else {
        diagnostics.push(origin.diagnostic(Problem::Invalid, "no `developer_instructions`; skipped"));
        return None;
    };
    let model = text("model", diagnostics);
    let effort = text("model_reasoning_effort", diagnostics);
    let mut importable = Ok(());
    match text("sandbox_mode", diagnostics).as_deref() {
        Some("read-only") => {
            let reason = "aim cannot hold it to Codex's `sandbox_mode = \"read-only\"`".to_owned();
            diagnostics.push(origin.diagnostic(Problem::NotImportable, format!("agent `{name}` is not importable: {reason}")));
            importable = Err(reason);
        }
        Some(mode) => {
            diagnostics.push(origin.diagnostic(
                Problem::Unsupported,
                format!("`sandbox_mode = \"{mode}\"` ignored: aimx confines the session to its workspace"),
            ));
        }
        None => {}
    }
    let rest: Vec<(String, Value)> = table.keys().map(|k| (k.clone(), Value::Null)).collect();
    report_unsupported(&Fields::new(rest), origin, diagnostics);
    truncated(file, origin, diagnostics);
    Some(AgentDef {
        meta: origin.descriptor(Kind::Agent, &name, &description, &file.hash, PARSER_CODEX),
        // A Codex role's model is a Codex model id.
        provider: model.as_ref().map(|_| "codex".to_owned()),
        model,
        effort,
        tools: ToolPolicy::default(),
        instructions,
        importable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{Scope, Source};

    fn origin(source: Source) -> Origin {
        Origin { scope: Scope::Project, source, location: "local".into(), path: "x".into() }
    }

    #[test]
    fn intersected_policies_permit_exactly_what_both_permit() {
        let set = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<BTreeSet<String>>();
        let policies = [
            ToolPolicy::default(),
            ToolPolicy { allow: Some(set(&["Read"])), deny: BTreeSet::new() },
            ToolPolicy { allow: Some(set(&["Read", "Write"])), deny: set(&["Bash"]) },
            ToolPolicy { allow: None, deny: set(&["Write"]) },
            ToolPolicy { allow: Some(BTreeSet::new()), deny: BTreeSet::new() },
        ];
        for a in &policies {
            for b in &policies {
                let both = a.intersect(b);
                for tool in ["Read", "Write", "Bash", "web_search"] {
                    assert_eq!(both.permits(tool), a.permits(tool) && b.permits(tool), "{a:?} ∩ {b:?} on {tool}");
                }
                assert_eq!(ToolPolicy::recorded(&both.record("x")), both, "the record round-trips");
            }
        }
    }

    fn file(text: &str) -> FileText {
        FileText { text: text.into(), hash: "sha256:0".into(), size: text.len() as u64, truncated: false }
    }

    #[test]
    fn native_agents_need_the_schema() {
        let mut diagnostics = Vec::new();
        let text = "---\nschema: aim.agent/v1\nname: reader\ndescription: Reads.\nmodel: m\ntools: [Read]\n---\nOnly read.\n";
        let agent = parse_native(&file(text), "reader", &origin(Source::Native), &mut diagnostics).unwrap();
        assert_eq!(agent.tools.allow, Some(BTreeSet::from(["Read".to_owned()])));
        assert_eq!((agent.model.as_deref(), agent.instructions.as_str()), (Some("m"), "Only read."));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for bad in ["---\nname: reader\n---\n", "---\nschema: aim.agent/v2\n---\n", "no frontmatter"] {
            let mut diagnostics = Vec::new();
            assert!(parse_native(&file(bad), "reader", &origin(Source::Native), &mut diagnostics).is_none(), "{bad}");
            assert_eq!(diagnostics[0].problem, Problem::Invalid);
        }
    }

    #[test]
    fn claude_agents_map_only_exact_tools() {
        let mut diagnostics = Vec::new();
        let ok = parse_claude(
            &file("---\nname: rev\ndescription: d\ntools: Read, Grep\n---\nbody"),
            "rev",
            &origin(Source::Claude),
            &mut diagnostics,
        )
        .unwrap();
        assert!(ok.importable.is_ok());
        assert!(ok.tools.permits("Grep") && !ok.tools.permits("Bash"));
        let mut diagnostics = Vec::new();
        let text = "---\nname: web\ndescription: d\ntools: Read, WebFetch\nmodel: sonnet\ncolor: blue\n---\nbody";
        let refused = parse_claude(&file(text), "web", &origin(Source::Claude), &mut diagnostics).unwrap();
        assert!(refused.importable.as_ref().is_err_and(|why| why.contains("`WebFetch`")));
        let problems: Vec<Problem> = diagnostics.iter().map(|d| d.problem).collect();
        assert_eq!(problems, [Problem::Unsupported, Problem::Unsupported, Problem::NotImportable], "{diagnostics:?}");
        let mut diagnostics = Vec::new();
        let plan = parse_claude(
            &file("---\nname: p\ndescription: d\npermissionMode: plan\n---\n"),
            "p",
            &origin(Source::Claude),
            &mut diagnostics,
        )
        .unwrap();
        assert!(plan.importable.is_err());
    }

    #[test]
    fn codex_roles_import_from_toml() {
        let mut diagnostics = Vec::new();
        let text = "name = \"explorer\"\ndescription = \"Explores.\"\ndeveloper_instructions = \"Look around.\"\nmodel = \"gpt-6-sol\"\nmodel_reasoning_effort = \"low\"\n";
        let agent = parse_codex(&file(text), "explorer", &origin(Source::Codex), &mut diagnostics).unwrap();
        assert_eq!((agent.provider.as_deref(), agent.effort.as_deref()), (Some("codex"), Some("low")));
        assert!(agent.importable.is_ok() && diagnostics.is_empty(), "{diagnostics:?}");
        let mut diagnostics = Vec::new();
        let sandboxed = format!("{text}sandbox_mode = \"read-only\"\n");
        assert!(parse_codex(&file(&sandboxed), "explorer", &origin(Source::Codex), &mut diagnostics).unwrap().importable.is_err());
        let mut diagnostics = Vec::new();
        assert!(parse_codex(&file("name = "), "x", &origin(Source::Codex), &mut diagnostics).is_none());
    }

    #[test]
    fn defaults_apply_on_the_agents_own_provider() {
        let mut diagnostics = Vec::new();
        let text = "---\nschema: aim.agent/v1\nname: a\ndescription: d\nprovider: codex\nmodel: gpt-6-sol\neffort: high\n---\n";
        let agent = parse_native(&file(text), "a", &origin(Source::Native), &mut diagnostics).unwrap();
        let on_codex = agent.defaults("codex", None, None);
        assert_eq!((on_codex.model.as_deref(), on_codex.effort.as_deref()), (Some("gpt-6-sol"), Some("high")));
        let explicit = agent.defaults("codex", Some("other"), Some("low"));
        assert_eq!((explicit.model.as_deref(), explicit.effort.as_deref()), (Some("other"), Some("low")));
        let elsewhere = agent.defaults("openrouter", None, None);
        assert_eq!((elsewhere.model, elsewhere.effort), (None, None));
        assert!(elsewhere.note.is_some());
    }
}
