//! Project and user resources (docs/architecture.md §6.8, ADR 0014): instructions, path-scoped
//! rules, skills, agent definitions, prompt templates and memory.
//!
//! **Discovery** ([`discover`]) builds a typed [`Catalog`] once per session start. It reads
//! everything and executes nothing; problems (a bad schema, a missing frontmatter, an oversize
//! file, a name collision, a field aim cannot honour) become [`Diagnostic`]s, never errors.
//!
//! - Project resources are read **through the session's harness** ([`files::HarnessFiles`]), so
//!   under `--ssh` the remote project applies and is labelled remote; nothing local is consulted.
//! - User resources come from `~/.aim` ([`files::LocalFiles`]), on this machine.
//! - Foreign formats (Claude Code, Codex, pi, oh-my-pi) are parsed read-only into the same
//!   descriptors, with their provenance.
//! - Everything is bounded by [`Bounds`]: files, bytes and time.
//!
//! **Precedence.** For one kind and name, the nearest wins: native project (`.agents/`), then
//! native user (`~/.aim`), then foreign project files in [`Source`] order. A loser is reported as a
//! collision unless it is byte-identical (e.g. `.claude/skills` symlinked to `.agents/skills`).
//!
//! **Use.** [`instructions::render`] turns the catalog into the stable instruction prefix;
//! [`mentions::expand_mentions`] injects explicitly mentioned skills into the user turn;
//! [`agents::AgentDef`] and [`tools::AllowedTools`] apply a named agent; [`Catalog::expand_prompt`]
//! expands prompt templates for UIs.

use std::path::PathBuf;
use std::time::Duration;

pub mod agents;
pub mod backend;
pub mod discover;
pub mod files;
pub mod instructions;
pub mod mentions;
pub mod prompts;
pub mod skills;
pub mod tools;
pub mod yaml;

pub use discover::discover;
pub use files::{Files, HarnessFiles, LocalFiles, MemoryFiles};

use agents::AgentDef;
use instructions::{InstructionFile, MemoryIndex, Rule};
use prompts::Prompt;
use skills::Skill;

/// What a resource is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Always-on instructions (`AGENTS.md`, `CLAUDE.md`, `.agents/instructions.md`).
    Instructions,
    /// Instructions for some paths (`.agents/rules/*.md`).
    Rule,
    /// An Agent Skill (`<dir>/SKILL.md`).
    Skill,
    /// An agent definition.
    Agent,
    /// A prompt template (slash command).
    Prompt,
    /// A memory index (`MEMORY.md`).
    Memory,
}

impl Kind {
    /// The kind's name, for messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Rule => "rule",
            Self::Skill => "skill",
            Self::Agent => "agent",
            Self::Prompt => "prompt",
            Self::Memory => "memory",
        }
    }
}

/// Whose resource it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The workspace's (read through the harness; remote under `--ssh`).
    Project,
    /// The user's (`~/.aim`, on this machine).
    User,
}

/// The format a resource was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// aim's own layout (`.agents/`, `~/.aim/`, `AGENTS.md`).
    Native,
    /// Claude Code (`.claude/`, `CLAUDE.md`).
    Claude,
    /// Codex (`.codex/agents/*.toml`).
    Codex,
    /// pi (`.pi/`).
    Pi,
    /// oh-my-pi (`.omp/`).
    OhMyPi,
}

impl Source {
    /// The source's name, for labels.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Native => "aim",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::OhMyPi => "oh-my-pi",
        }
    }
}

/// How far a resource is trusted. Discovery executes nothing whatever the trust; every resource
/// found so far is text for the model, which can advise but never grants a permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trust {
    /// The user's own files (`~/.aim`).
    User,
    /// Native files in the workspace: as trusted as the checkout the agent works on.
    Workspace,
    /// Another harness's files, parsed read-only. Their text may be loaded; anything of theirs
    /// that would execute (MCP servers, hooks) needs a per-source opt-in (ADR 0014) and is not
    /// activated by discovery.
    Imported,
}

/// A discovered resource: what it is and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// What it is.
    pub kind: Kind,
    /// The name it is addressed by (`$name`, `--agent name`, `/name`); the path for instructions.
    pub name: String,
    /// One line on what it is for.
    pub description: String,
    /// Where it is: workspace-relative for project files, absolute on this machine for user files.
    pub path: String,
    /// Whose it is.
    pub scope: Scope,
    /// Which format.
    pub source: Source,
    /// How far it is trusted.
    pub trust: Trust,
    /// Where the file lives: `local`, or the workspace's `ssh:<destination>`.
    pub location: String,
    /// `sha256:<hex>` of the content.
    pub hash: String,
    /// Which parser read it (`<format>/<version>`), so a format change is visible in the record.
    pub parser_version: &'static str,
}

impl Descriptor {
    /// Whether the file lives on another host.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.location != "local"
    }
}

/// Where a file being parsed came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    /// Whose file.
    pub scope: Scope,
    /// Which format.
    pub source: Source,
    /// `local`, or the workspace's `ssh:<destination>`.
    pub location: String,
    /// The file as displayed.
    pub path: String,
}

impl Origin {
    /// The trust that follows from scope and source.
    #[must_use]
    pub fn trust(&self) -> Trust {
        match (self.scope, self.source) {
            (_, Source::Claude | Source::Codex | Source::Pi | Source::OhMyPi) => Trust::Imported,
            (Scope::Project, Source::Native) => Trust::Workspace,
            (Scope::User, Source::Native) => Trust::User,
        }
    }

    /// A descriptor for a resource of this origin.
    #[must_use]
    pub fn descriptor(&self, kind: Kind, name: &str, description: &str, hash: &str, parser_version: &'static str) -> Descriptor {
        Descriptor {
            kind,
            name: name.to_owned(),
            description: description.to_owned(),
            path: self.path.clone(),
            scope: self.scope,
            source: self.source,
            trust: self.trust(),
            location: self.location.clone(),
            hash: hash.to_owned(),
            parser_version,
        }
    }

    /// A diagnostic about this file.
    #[must_use]
    pub fn diagnostic(&self, problem: Problem, message: impl Into<String>) -> Diagnostic {
        Diagnostic { problem, path: self.path.clone(), scope: self.scope, message: message.into() }
    }
}

/// What kind of problem a diagnostic reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Problem {
    /// The file is malformed or breaks its format's rules (skipped, or loaded as noted).
    Invalid,
    /// A field or feature aim does not honour (ignored, as noted).
    Unsupported,
    /// Another resource of the same kind and name wins.
    Collision,
    /// Content was cut to fit a budget.
    Truncated,
    /// A bound (files, bytes, time) stopped discovery early.
    Limit,
    /// An agent whose fields cannot be mapped exactly: listed, but refused if selected.
    NotImportable,
    /// The file or directory could not be read.
    Unreadable,
}

/// A problem found during discovery. Never fatal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// What kind of problem.
    pub problem: Problem,
    /// The file (or directory) concerned, as displayed.
    pub path: String,
    /// Whose file.
    pub scope: Scope,
    /// What happened and what aim did about it.
    pub message: String,
}

/// Bounds on discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// Most files requested per scope, fixed files included whether they exist or not (ADR 0038).
    pub max_files: usize,
    /// Most bytes read of one file (the rest is reported truncated).
    pub max_file_bytes: u64,
    /// Most bytes read per scope.
    pub max_total_bytes: u64,
    /// Most entries listed per directory.
    pub max_dir_entries: u32,
    /// Longest a scope's discovery may take; past it the scope contributes nothing.
    pub timeout: Duration,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_files: 256,
            max_file_bytes: 64 * 1024,
            max_total_bytes: 4 * 1024 * 1024,
            max_dir_entries: 256,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Where user resources come from and how discovery is bounded.
#[derive(Clone, Debug, Default)]
pub struct ResourceConfig {
    /// The user's resource root (`~/.aim`); `None` loads no user resources.
    pub user_home: Option<PathBuf>,
    /// Bounds for each scope.
    pub bounds: Bounds,
    /// The skill catalog's budget in bytes; `None` derives it from the model's context window.
    pub skill_budget: Option<usize>,
}

impl ResourceConfig {
    /// User resources from `home` (normally [`crate::cli::aim_home`]), default bounds.
    #[must_use]
    pub fn user(home: impl Into<PathBuf>) -> Self {
        Self { user_home: Some(home.into()), ..Self::default() }
    }
}

/// Everything discovered for a session, after precedence. Built once per session start.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// Always-on instruction files, outermost first.
    pub instructions: Vec<InstructionFile>,
    /// Rules, native first.
    pub rules: Vec<Rule>,
    /// Skills that won their name, in precedence order.
    pub skills: Vec<Skill>,
    /// Agent definitions that won their name.
    pub agents: Vec<AgentDef>,
    /// Prompt templates that won their name.
    pub prompts: Vec<Prompt>,
    /// Memory indexes: project, then user.
    pub memory: Vec<MemoryIndex>,
    /// Everything worth telling the user.
    pub diagnostics: Vec<Diagnostic>,
}

/// One scope's findings, before precedence.
#[derive(Clone, Debug, Default)]
pub struct Found {
    /// Always-on instruction files, outermost first.
    pub instructions: Vec<InstructionFile>,
    /// Rules.
    pub rules: Vec<Rule>,
    /// Skills, in the scope's source order.
    pub skills: Vec<Skill>,
    /// Agent definitions.
    pub agents: Vec<AgentDef>,
    /// Prompt templates.
    pub prompts: Vec<Prompt>,
    /// Memory indexes.
    pub memory: Vec<MemoryIndex>,
    /// Problems found.
    pub diagnostics: Vec<Diagnostic>,
}

/// Something with a descriptor.
pub trait Described {
    /// Its descriptor.
    fn descriptor(&self) -> &Descriptor;
}

/// Keeps the first resource of each name, reporting the rest (quietly when identical).
fn resolve<T: Described>(candidates: Vec<T>, diagnostics: &mut Vec<Diagnostic>) -> Vec<T> {
    let mut kept: Vec<T> = Vec::new();
    for candidate in candidates {
        let meta = candidate.descriptor();
        match kept.iter().find(|k| k.descriptor().name == meta.name) {
            Some(winner) if winner.descriptor().hash == meta.hash => {}
            Some(winner) => diagnostics.push(Diagnostic {
                problem: Problem::Collision,
                path: meta.path.clone(),
                scope: meta.scope,
                message: format!(
                    "{} `{}` ({}, {}) is shadowed by {} ({}, {})",
                    meta.kind.name(),
                    meta.name,
                    meta.source.name(),
                    scope_name(meta.scope),
                    winner.descriptor().path,
                    winner.descriptor().source.name(),
                    scope_name(winner.descriptor().scope),
                ),
            }),
            None => kept.push(candidate),
        }
    }
    kept
}

const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Project => "project",
        Scope::User => "user",
    }
}

/// Orders one kind's candidates: native project, native user, then foreign project by source.
fn ordered<T: Described>(project: Vec<T>, user: Vec<T>) -> Vec<T> {
    let (native, mut foreign): (Vec<T>, Vec<T>) = project.into_iter().partition(|r| r.descriptor().source == Source::Native);
    // Stable: files of one source keep their discovery order.
    foreign.sort_by_key(|r| r.descriptor().source);
    native.into_iter().chain(user).chain(foreign).collect()
}

impl Catalog {
    /// Applies precedence to a project's and the user's findings.
    #[must_use]
    pub fn assemble(project: Found, user: Found) -> Self {
        let mut diagnostics = project.diagnostics;
        diagnostics.extend(user.diagnostics);
        let skills = resolve(ordered(project.skills, user.skills), &mut diagnostics);
        let agents = resolve(ordered(project.agents, user.agents), &mut diagnostics);
        let prompts = resolve(ordered(project.prompts, user.prompts), &mut diagnostics);
        let rules = resolve(ordered(project.rules, user.rules), &mut diagnostics);
        let mut instructions = project.instructions;
        instructions.extend(user.instructions);
        let mut memory = project.memory;
        memory.extend(user.memory);
        Self { instructions, rules, skills, agents, prompts, memory, diagnostics }
    }

    /// The skill named `name`.
    #[must_use]
    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.meta.name == name)
    }

    /// The agent definition named `name`.
    #[must_use]
    pub fn agent(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.meta.name == name)
    }

    /// The prompt template named `name`.
    #[must_use]
    pub fn prompt(&self, name: &str) -> Option<&Prompt> {
        self.prompts.iter().find(|p| p.meta.name == name)
    }

    /// Expands prompt template `name` with `args` (the text typed after the command), in the
    /// template's own argument dialect. `None` when there is no such template.
    #[must_use]
    pub fn expand_prompt(&self, name: &str, args: &str) -> Option<String> {
        self.prompt(name).map(|p| p.expand(args))
    }

    /// Every loaded resource's descriptor.
    #[must_use]
    pub fn descriptors(&self) -> Vec<&Descriptor> {
        let mut out: Vec<&Descriptor> = self.instructions.iter().map(Described::descriptor).collect();
        out.extend(self.rules.iter().map(Described::descriptor));
        out.extend(self.skills.iter().map(Described::descriptor));
        out.extend(self.agents.iter().map(Described::descriptor));
        out.extend(self.prompts.iter().map(Described::descriptor));
        out.extend(self.memory.iter().map(Described::descriptor));
        out
    }
}

/// Whether `name` can be addressed as `$name`, `/name` or `--agent name`: 1–64 characters of
/// ASCII letters, digits, `-`, `_` and `.` (not starting with `.` or `-`), and `:` between
/// namespace parts.
#[must_use]
pub fn is_addressable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split(':').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// The first `max` bytes of `text` (whole characters), and whether anything was cut.
#[must_use]
pub fn clip(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    (text.get(..cut).unwrap_or_default(), true)
}

/// `text` flattened to one line of at most `max` characters, with `…` when cut.
#[must_use]
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressable_names() {
        for good in ["haiku", "code-review", "v2.1", "front_end", "frontend:component"] {
            assert!(is_addressable(good), "{good}");
        }
        for bad in ["", "-x", ".x", "a b", "a/b", "ns:", &"x".repeat(65), "é"] {
            assert!(!is_addressable(bad), "{bad}");
        }
    }

    #[test]
    fn clips_on_character_boundaries() {
        assert_eq!(clip("héllo", 2), ("h", true));
        assert_eq!(clip("hé", 3), ("hé", false));
        assert_eq!(one_line("a\n b   c", 3), "a …");
        assert_eq!(one_line("a b", 3), "a b");
    }
}
