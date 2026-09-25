//! Discovery: finds and parses a scope's resources (docs/architecture.md §6.8, ADR 0014).
//!
//! A scope is scanned in two round trips: every resource directory is listed while the fixed
//! files (the `AGENTS.md` walk, `.agents/instructions.md`, `MEMORY.md`) are read; then the files
//! the listings name are read in batches (one more round trip for Claude's namespaced command
//! directories). Every scope is bounded by [`Bounds`] — files, bytes per file, bytes in total,
//! entries per directory, and time — and whatever a bound stops is reported. The file and byte
//! bounds are one admission budget, checked before each read ([`Admission`], ADR 0038).
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

use std::collections::HashMap;

use aim_kernel::discovery::{Budget, ReadCharge};
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

/// A scope's admission budget (ADR 0038, REV8-9). Every file requested counts against
/// [`Bounds::max_files`], fixed files first. A read is requested only when each file in it may
/// return [`Bounds::max_file_bytes`] within the bytes left; afterwards the bytes it returned are
/// charged (a `CLAUDE.md` that loses to `AGENTS.md` too), a failed read its whole cap, a missing
/// file nothing.
#[derive(Debug)]
struct Admission {
    budget: Budget,
    /// The per-file cap every read asks for ([`Bounds::max_file_bytes`]).
    cap: u64,
}

/// The bytes a read returned (REV14 F10). An untruncated read returned exactly its text. A
/// truncated one returned the per-file `cap` it asked for (at most the file's size), including an
/// incomplete trailing character that its text no longer holds; if the harness returned less (its
/// own `fs.read_many` budget), this charges more, never less, than was returned.
fn returned_bytes(file: &FileText, cap: u64) -> u64 {
    let text = u64::try_from(file.text.len()).unwrap_or(u64::MAX);
    if file.truncated { text.max(cap.min(file.size)) } else { text }
}

impl Admission {
    fn new(bounds: &Bounds) -> Self {
        Self {
            budget: Budget::new(u64::try_from(bounds.max_files).unwrap_or(u64::MAX), bounds.max_total_bytes, bounds.max_file_bytes),
            cap: bounds.max_file_bytes,
        }
    }

    /// How many of `wanted` files (at most `batch`) the next read may name; reserves their bytes.
    fn admit(&mut self, wanted: usize, batch: usize) -> usize {
        let selected = self.budget.reserve(u64::try_from(wanted).unwrap_or(u64::MAX), u64::try_from(batch).unwrap_or(u64::MAX));
        usize::try_from(selected).unwrap_or(usize::MAX)
    }

    /// Settles one admitted file's reservation once it was read.
    fn settle(&mut self, read: &Read) {
        let charge = match read {
            Read::Ok(file) => ReadCharge::Bytes(returned_bytes(file, self.cap)),
            Read::Missing => ReadCharge::Missing,
            Read::Failed(_) => ReadCharge::Failed,
        };
        let _settled = self.budget.settle(charge);
    }

    /// Why the next file is not read.
    fn refusal(&self, bounds: &Bounds) -> String {
        if self.budget.files_left() == 0 {
            format!("not read: more than {} resource files", bounds.max_files)
        } else {
            format!("not read: the {}-byte budget for resources is spent", bounds.max_total_bytes)
        }
    }

    /// Admits a prefix of `fixed` (they are in priority order), reporting the rest.
    fn admit_fixed(&mut self, mut fixed: Vec<String>, bounds: &Bounds, files: &dyn Files, scope: Scope, found: &mut Found) -> Vec<String> {
        let admitted = self.admit(fixed.len(), usize::MAX);
        let refused = fixed.split_off(admitted);
        if !refused.is_empty() {
            let why = self.refusal(bounds);
            found.diagnostics.extend(refused.iter().map(|path| Diagnostic {
                problem: Problem::Limit,
                path: files.display(path),
                scope,
                message: why.clone(),
            }));
        }
        fixed
    }
}

/// Reads the admitted `paths` (none: no request at all), settling their reservations; each path
/// with its outcome.
async fn read_admitted(files: &dyn Files, paths: Vec<String>, bounds: &Bounds) -> Vec<(String, Read)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let reads = files.read_many(paths.clone(), bounds.max_file_bytes).await;
    paths.into_iter().zip(reads).collect()
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
    fixed.push(INSTRUCTIONS.to_owned());
    fixed.push(PROJECT_MEMORY.to_owned());
    let mut admission = Admission::new(bounds);
    let fixed = admission.admit_fixed(fixed, bounds, files, Scope::Project, &mut found);
    let (planned, reads) =
        tokio::join!(plan(files, &PROJECT_DIRS, Scope::Project, bounds, &mut found.diagnostics), read_admitted(files, fixed, bounds));
    for (_, read) in &reads {
        admission.settle(read);
    }
    // A file not admitted was reported; here it is as good as missing.
    let mut reads: HashMap<String, Read> = reads.into_iter().collect();
    let mut take = |path: &str| reads.remove(path).unwrap_or(Read::Missing);
    for dir in &dirs {
        let (agents_path, claude_path) = (join(dir, "AGENTS.md"), join(dir, "CLAUDE.md"));
        let chosen = match (take(&agents_path), take(&claude_path)) {
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
            let origin = origin(files, Scope::Project, source, location, &path);
            found.instructions.push(instructions::instruction_file(&file, &origin, &mut found.diagnostics));
        }
    }
    match take(INSTRUCTIONS) {
        Read::Ok(file) => {
            let origin = origin(files, Scope::Project, Source::Native, location, INSTRUCTIONS);
            found.instructions.push(instructions::instruction_file(&file, &origin, &mut found.diagnostics));
        }
        other => unreadable(files, Scope::Project, INSTRUCTIONS, other, &mut found.diagnostics),
    }
    match take(PROJECT_MEMORY) {
        Read::Ok(file) => {
            found.memory.push(instructions::memory_index(&file, &origin(files, Scope::Project, Source::Native, location, PROJECT_MEMORY)));
        }
        other => unreadable(files, Scope::Project, PROJECT_MEMORY, other, &mut found.diagnostics),
    }
    read_planned(files, planned, Scope::Project, location, bounds, &mut admission, &mut found).await;
    found
}

/// The project's always-on aim instructions.
const INSTRUCTIONS: &str = ".agents/instructions.md";
/// The project's memory index.
const PROJECT_MEMORY: &str = ".agents/memory/MEMORY.md";
/// The user's memory index (under `~/.aim`).
const USER_MEMORY: &str = "memory/MEMORY.md";

/// Discovers the user's resources (`~/.aim`) through `files`.
pub async fn discover_user(files: &dyn Files, bounds: &Bounds) -> Found {
    let mut found = Found::default();
    let mut admission = Admission::new(bounds);
    let fixed = admission.admit_fixed(vec![USER_MEMORY.to_owned()], bounds, files, Scope::User, &mut found);
    let (planned, reads) =
        tokio::join!(plan(files, &USER_DIRS, Scope::User, bounds, &mut found.diagnostics), read_admitted(files, fixed, bounds));
    for (path, read) in reads {
        admission.settle(&read);
        match read {
            Read::Ok(file) => {
                found.memory.push(instructions::memory_index(&file, &origin(files, Scope::User, Source::Native, "local", &path)));
            }
            other => unreadable(files, Scope::User, &path, other, &mut found.diagnostics),
        }
    }
    read_planned(files, planned, Scope::User, "local", bounds, &mut admission, &mut found).await;
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

/// Reads the planned files in batches the scope's admission budget allows, and parses them.
async fn read_planned(
    files: &dyn Files,
    planned: Vec<Planned>,
    scope: Scope,
    location: &str,
    bounds: &Bounds,
    admission: &mut Admission,
    found: &mut Found,
) {
    let mut rest = planned.as_slice();
    while !rest.is_empty() {
        let admitted = admission.admit(rest.len(), READ_BATCH);
        let Some((batch, later)) = rest.split_at_checked(admitted).filter(|_| admitted > 0) else {
            let why = admission.refusal(bounds);
            found.diagnostics.extend(rest.iter().map(|skipped| Diagnostic {
                problem: Problem::Limit,
                path: files.display(&skipped.path),
                scope,
                message: why.clone(),
            }));
            return;
        };
        rest = later;
        let reads = files.read_many(batch.iter().map(|p| p.path.clone()).collect(), bounds.max_file_bytes).await;
        for (item, read) in batch.iter().zip(reads) {
            admission.settle(&read);
            let origin = origin(files, scope, item.source, location, &item.path);
            match read {
                Read::Ok(file) => parse(item, &file, &origin, found),
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

#[cfg(test)]
mod tests {
    use super::{FileText, returned_bytes};

    fn file(text: &str, size: u64, truncated: bool) -> FileText {
        FileText { text: text.to_owned(), hash: "sha256:0".to_owned(), size, truncated }
    }

    /// REV14 F10: a truncated read is charged the bytes returned, including the up to three bytes
    /// of a cut character its text dropped.
    #[test]
    fn a_read_is_charged_the_bytes_it_returned() {
        assert_eq!(returned_bytes(&file("abc", 3, false), 8), 3);
        // Eight bytes returned, the last two the start of a three-byte character.
        assert_eq!(returned_bytes(&file("abcdef", 100, true), 8), 8);
        assert_eq!(returned_bytes(&file("abcdefgh", 100, true), 8), 8);
        // Never less than the text itself.
        assert_eq!(returned_bytes(&file("abcdefghij", 100, true), 8), 10);
    }
}
