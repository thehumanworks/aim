//! Discovery: finds and parses a scope's resources (docs/architecture.md §6.8, ADR 0014).
//!
//! A scope is scanned in two round trips: every resource directory is listed while the fixed
//! files (the `AGENTS.md` walk, `.agents/instructions.md`, `MEMORY.md`) are read; then the files
//! the listings name are read in batches (one more round trip for Claude's namespaced command
//! directories). Every scope is bounded by [`Bounds`] — files, bytes per file, bytes in total,
//! entries per directory, and time — and whatever a bound stops is reported.
//!
//! | Directory (project) | Kind | Source |
//! | --- | --- | --- |
//! | `.agents/skills/<n>/SKILL.md`, `.agents/agents/*.md`, `.agents/prompts/*.md`, `.agents/rules/*.md` | all | aim |
//! | `.claude/skills/<n>/SKILL.md`, `.claude/agents/*.md`, `.claude/commands/[<ns>/]*.md`, `.claude/rules/*.md` | all | Claude Code |
//! | `.codex/agents/*.toml` | agent | Codex |
//! | `.pi/skills/<n>/SKILL.md`, `.pi/prompts/*.md` | skill, prompt | pi |
//! | `.omp/skills/<n>/SKILL.md` | skill | oh-my-pi |
//!
//! User resources are `~/.aim/{skills/<n>/SKILL.md, agents/*.md, prompts/*.md, memory/MEMORY.md}`.

use aim_proto::harness::EntryKind;

use super::files::{FileText, Files, READ_BATCH, Read};
use super::instructions::{self, join, walk};
use super::{Bounds, Catalog, Diagnostic, Found, Kind, Origin, Problem, ResourceConfig, Scope, Source, agents, prompts, skills};

/// How a directory holds its resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// `<dir>/<name>/SKILL.md`.
    SkillDirs,
    /// `<dir>/<name>.md`.
    Markdown,
    /// `<dir>/<name>.toml`.
    Toml,
    /// `<dir>/<name>.md` and `<dir>/<ns>/<name>.md` (addressed `ns:name`).
    NamespacedMarkdown,
}

/// A resource directory.
#[derive(Clone, Copy, Debug)]
struct DirSpec {
    dir: &'static str,
    kind: Kind,
    source: Source,
    layout: Layout,
}

const fn spec(dir: &'static str, kind: Kind, source: Source, layout: Layout) -> DirSpec {
    DirSpec { dir, kind, source, layout }
}

/// Project resource directories, in precedence order within each kind.
const PROJECT_DIRS: [DirSpec; 12] = [
    spec(".agents/skills", Kind::Skill, Source::Native, Layout::SkillDirs),
    spec(".agents/agents", Kind::Agent, Source::Native, Layout::Markdown),
    spec(".agents/prompts", Kind::Prompt, Source::Native, Layout::Markdown),
    spec(".agents/rules", Kind::Rule, Source::Native, Layout::Markdown),
    spec(".claude/skills", Kind::Skill, Source::Claude, Layout::SkillDirs),
    spec(".claude/agents", Kind::Agent, Source::Claude, Layout::Markdown),
    spec(".claude/commands", Kind::Prompt, Source::Claude, Layout::NamespacedMarkdown),
    spec(".claude/rules", Kind::Rule, Source::Claude, Layout::Markdown),
    spec(".codex/agents", Kind::Agent, Source::Codex, Layout::Toml),
    spec(".pi/skills", Kind::Skill, Source::Pi, Layout::SkillDirs),
    spec(".pi/prompts", Kind::Prompt, Source::Pi, Layout::Markdown),
    spec(".omp/skills", Kind::Skill, Source::OhMyPi, Layout::SkillDirs),
];

/// User resource directories under `~/.aim`.
const USER_DIRS: [DirSpec; 3] = [
    spec("skills", Kind::Skill, Source::Native, Layout::SkillDirs),
    spec("agents", Kind::Agent, Source::Native, Layout::Markdown),
    spec("prompts", Kind::Prompt, Source::Native, Layout::Markdown),
];

/// A file to read and what it is.
#[derive(Clone, Debug)]
struct Planned {
    path: String,
    name: String,
    kind: Kind,
    source: Source,
}

/// Discovers a session's resources: the project's through `project` (the session's harness, so
/// under `--ssh` the remote project), the user's from [`ResourceConfig::user_home`], concurrently
/// and each within [`Bounds::timeout`]. `cwd` is the session's directory relative to the
/// workspace root (`""` for the root). `location` labels project resources (`local`,
/// `ssh:<destination>`).
pub async fn discover(config: &ResourceConfig, project: Option<&dyn Files>, location: &str, cwd: &str) -> Catalog {
    let bounds = config.bounds;
    let project = async {
        match project {
            Some(files) => bounded(discover_project(files, location, cwd, &bounds), &bounds, Scope::Project).await,
            None => Found::default(),
        }
    };
    let user = async {
        match &config.user_home {
            Some(home) => bounded(discover_user(&super::LocalFiles::new(home), &bounds), &bounds, Scope::User).await,
            None => Found::default(),
        }
    };
    let (project, user) = tokio::join!(project, user);
    Catalog::assemble(project, user)
}

async fn bounded(scan: impl Future<Output = Found>, bounds: &Bounds, scope: Scope) -> Found {
    tokio::time::timeout(bounds.timeout, scan).await.unwrap_or_else(|_| Found {
        diagnostics: vec![Diagnostic {
            problem: Problem::Limit,
            path: String::new(),
            scope,
            message: format!("discovery took longer than {} s; these resources are not loaded", bounds.timeout.as_secs()),
        }],
        ..Found::default()
    })
}

/// Discovers the project's resources through `files`. `cwd` is relative to the root.
pub async fn discover_project(files: &dyn Files, location: &str, cwd: &str, bounds: &Bounds) -> Found {
    let mut found = Found::default();
    let dirs = walk(cwd).unwrap_or_else(|| {
        found.diagnostics.push(Diagnostic {
            problem: Problem::Invalid,
            path: cwd.to_owned(),
            scope: Scope::Project,
            message: "the session directory is outside the workspace; only the root's instructions apply".to_owned(),
        });
        vec![String::new()]
    });
    let mut fixed: Vec<String> = dirs.iter().flat_map(|d| [join(d, "AGENTS.md"), join(d, "CLAUDE.md")]).collect();
    fixed.push(".agents/instructions.md".to_owned());
    fixed.push(".agents/memory/MEMORY.md".to_owned());
    let (planned, reads) = tokio::join!(
        plan(files, &PROJECT_DIRS, Scope::Project, bounds, &mut found.diagnostics),
        files.read_many(fixed.clone(), bounds.max_file_bytes)
    );
    let mut spent: u64 = 0;
    let mut reads = fixed.into_iter().zip(reads);
    for _dir in &dirs {
        let (Some((agents_path, agents)), Some((claude_path, claude))) = (reads.next(), reads.next()) else { break };
        let chosen = match (agents, claude) {
            (Read::Ok(agents), Read::Ok(claude)) => {
                if agents.hash != claude.hash {
                    let origin = origin(files, Scope::Project, Source::Claude, location, &claude_path);
                    found.diagnostics.push(
                        origin.diagnostic(Problem::Collision, format!("not loaded: {agents_path} takes precedence in its directory")),
                    );
                }
                Some((agents_path, Source::Native, agents))
            }
            (Read::Ok(agents), other) => {
                unreadable(files, Scope::Project, &claude_path, other, &mut found.diagnostics);
                Some((agents_path, Source::Native, agents))
            }
            (other, Read::Ok(claude)) => {
                unreadable(files, Scope::Project, &agents_path, other, &mut found.diagnostics);
                Some((claude_path, Source::Claude, claude))
            }
            (a, c) => {
                unreadable(files, Scope::Project, &agents_path, a, &mut found.diagnostics);
                unreadable(files, Scope::Project, &claude_path, c, &mut found.diagnostics);
                None
            }
        };
        if let Some((path, source, file)) = chosen {
            spent = spent.saturating_add(file.text.len() as u64);
            let origin = origin(files, Scope::Project, source, location, &path);
            found.instructions.push(instructions::instruction_file(&file, &origin, &mut found.diagnostics));
        }
    }
    if let Some((path, read)) = reads.next() {
        match read {
            Read::Ok(file) => {
                spent = spent.saturating_add(file.text.len() as u64);
                let origin = origin(files, Scope::Project, Source::Native, location, &path);
                found.instructions.push(instructions::instruction_file(&file, &origin, &mut found.diagnostics));
            }
            other => unreadable(files, Scope::Project, &path, other, &mut found.diagnostics),
        }
    }
    if let Some((path, read)) = reads.next() {
        match read {
            Read::Ok(file) => {
                found.memory.push(instructions::memory_index(&file, &origin(files, Scope::Project, Source::Native, location, &path)));
            }
            other => unreadable(files, Scope::Project, &path, other, &mut found.diagnostics),
        }
    }
    read_planned(files, planned, Scope::Project, location, bounds, spent, &mut found).await;
    found
}

/// Discovers the user's resources (`~/.aim`) through `files`.
pub async fn discover_user(files: &dyn Files, bounds: &Bounds) -> Found {
    let mut found = Found::default();
    let memory_path = "memory/MEMORY.md".to_owned();
    let (planned, reads) = tokio::join!(
        plan(files, &USER_DIRS, Scope::User, bounds, &mut found.diagnostics),
        files.read_many(vec![memory_path.clone()], bounds.max_file_bytes)
    );
    let mut spent: u64 = 0;
    match reads.into_iter().next() {
        Some(Read::Ok(file)) => {
            spent = file.text.len() as u64;
            found.memory.push(instructions::memory_index(&file, &origin(files, Scope::User, Source::Native, "local", &memory_path)));
        }
        Some(other) => unreadable(files, Scope::User, &memory_path, other, &mut found.diagnostics),
        None => {}
    }
    read_planned(files, planned, Scope::User, "local", bounds, spent, &mut found).await;
    found
}

fn origin(files: &dyn Files, scope: Scope, source: Source, location: &str, path: &str) -> Origin {
    Origin { scope, source, location: location.to_owned(), path: files.display(path) }
}

/// Reports a failed read (absence is not a problem).
fn unreadable(files: &dyn Files, scope: Scope, path: &str, read: Read, diagnostics: &mut Vec<Diagnostic>) {
    if let Read::Failed(message) = read {
        diagnostics.push(Diagnostic { problem: Problem::Unreadable, path: files.display(path), scope, message });
    }
}

/// Lists `specs`' directories (concurrently) and plans the files to read.
async fn plan(files: &dyn Files, specs: &[DirSpec], scope: Scope, bounds: &Bounds, diagnostics: &mut Vec<Diagnostic>) -> Vec<Planned> {
    let listings =
        futures_util::future::join_all(specs.iter().map(|spec| async move { (spec, files.list(spec.dir, bounds.max_dir_entries).await) }))
            .await;
    let mut planned = Vec::new();
    let mut namespaces: Vec<(DirSpec, String)> = Vec::new();
    for (spec, listing) in listings {
        let entries = match listing {
            Ok(Some(entries)) => entries,
            Ok(None) => continue,
            Err(message) => {
                diagnostics.push(Diagnostic { problem: Problem::Unreadable, path: files.display(spec.dir), scope, message });
                continue;
            }
        };
        if entries.len() >= usize::try_from(bounds.max_dir_entries).unwrap_or(usize::MAX) {
            diagnostics.push(Diagnostic {
                problem: Problem::Limit,
                path: files.display(spec.dir),
                scope,
                message: format!("only the first {} entries are read", bounds.max_dir_entries),
            });
        }
        for entry in entries {
            if entry.name.starts_with('.') {
                continue;
            }
            let dir_like = matches!(entry.kind, EntryKind::Dir | EntryKind::Symlink);
            let file_like = matches!(entry.kind, EntryKind::File | EntryKind::Symlink);
            let stem = |ext: &str| entry.name.strip_suffix(ext).filter(|s| !s.is_empty()).map(str::to_owned);
            match spec.layout {
                Layout::SkillDirs if dir_like => planned.push(Planned {
                    path: format!("{}/{}/SKILL.md", spec.dir, entry.name),
                    name: entry.name.clone(),
                    kind: spec.kind,
                    source: spec.source,
                }),
                Layout::Markdown | Layout::NamespacedMarkdown if file_like && stem(".md").is_some() => planned.push(Planned {
                    path: join(spec.dir, &entry.name),
                    name: stem(".md").unwrap_or_default(),
                    kind: spec.kind,
                    source: spec.source,
                }),
                Layout::NamespacedMarkdown if entry.kind == EntryKind::Dir => namespaces.push((*spec, entry.name.clone())),
                Layout::Toml if file_like && stem(".toml").is_some() => planned.push(Planned {
                    path: join(spec.dir, &entry.name),
                    name: stem(".toml").unwrap_or_default(),
                    kind: spec.kind,
                    source: spec.source,
                }),
                _ => {}
            }
        }
    }
    let nested = futures_util::future::join_all(namespaces.iter().map(|(spec, ns)| async move {
        let dir = join(spec.dir, ns);
        let listing = files.list(&dir, bounds.max_dir_entries).await;
        (spec, ns, dir, listing)
    }))
    .await;
    for (spec, ns, dir, listing) in nested {
        match listing {
            Ok(Some(entries)) => {
                for entry in entries.into_iter().filter(|e| matches!(e.kind, EntryKind::File | EntryKind::Symlink)) {
                    if let Some(stem) = entry.name.strip_suffix(".md").filter(|s| !s.is_empty() && !entry.name.starts_with('.')) {
                        planned.push(Planned {
                            path: join(&dir, &entry.name),
                            name: format!("{ns}:{stem}"),
                            kind: spec.kind,
                            source: spec.source,
                        });
                    }
                }
            }
            Ok(None) => {}
            Err(message) => diagnostics.push(Diagnostic { problem: Problem::Unreadable, path: files.display(&dir), scope, message }),
        }
    }
    planned
}

/// Reads the planned files in batches within the scope's file and byte budgets, and parses them.
async fn read_planned(
    files: &dyn Files,
    mut planned: Vec<Planned>,
    scope: Scope,
    location: &str,
    bounds: &Bounds,
    mut spent: u64,
    found: &mut Found,
) {
    if planned.len() > bounds.max_files {
        for skipped in planned.drain(bounds.max_files..) {
            found.diagnostics.push(Diagnostic {
                problem: Problem::Limit,
                path: files.display(&skipped.path),
                scope,
                message: format!("not read: more than {} resource files", bounds.max_files),
            });
        }
    }
    for batch in planned.chunks(READ_BATCH) {
        if spent >= bounds.max_total_bytes {
            for skipped in batch {
                found.diagnostics.push(Diagnostic {
                    problem: Problem::Limit,
                    path: files.display(&skipped.path),
                    scope,
                    message: format!("not read: the {}-byte budget for resources is spent", bounds.max_total_bytes),
                });
            }
            continue;
        }
        let reads = files.read_many(batch.iter().map(|p| p.path.clone()).collect(), bounds.max_file_bytes).await;
        for (item, read) in batch.iter().zip(reads) {
            let origin = origin(files, scope, item.source, location, &item.path);
            match read {
                Read::Ok(file) => {
                    spent = spent.saturating_add(file.text.len() as u64);
                    parse(item, &file, &origin, found);
                }
                Read::Missing if item.kind == Kind::Skill => {
                    found.diagnostics.push(origin.diagnostic(Problem::Invalid, "a skill directory without SKILL.md; skipped"));
                }
                Read::Missing => {}
                Read::Failed(message) => found.diagnostics.push(origin.diagnostic(Problem::Unreadable, message)),
            }
        }
    }
}

fn parse(item: &Planned, file: &FileText, origin: &Origin, found: &mut Found) {
    let diagnostics = &mut found.diagnostics;
    match (item.kind, item.source) {
        (Kind::Skill, _) => found.skills.extend(skills::parse(file, &item.name, origin, diagnostics)),
        (Kind::Agent, Source::Claude) => found.agents.extend(agents::parse_claude(file, &item.name, origin, diagnostics)),
        (Kind::Agent, Source::Codex) => found.agents.extend(agents::parse_codex(file, &item.name, origin, diagnostics)),
        (Kind::Agent, _) => found.agents.extend(agents::parse_native(file, &item.name, origin, diagnostics)),
        (Kind::Prompt, _) => found.prompts.extend(prompts::parse(file, &item.name, origin, diagnostics)),
        (Kind::Rule, _) => found.rules.extend(instructions::parse_rule(file, &item.name, origin, diagnostics)),
        (Kind::Instructions | Kind::Memory, _) => {}
    }
}
