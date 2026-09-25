//! Project and user resources (ADR 0014, docs/architecture.md §6.8): discovery of every native and
//! foreign format, precedence and diagnostics, budgets, the SSH shape (project files only through
//! the harness), skill mentions, agent allowlists, and live smoke tests.
//!
//! Fixtures are built here rather than checked in, so their `.claude/` and `.agents/` trees never
//! become resources of agents working on this repository.
#![expect(clippy::unwrap_used, reason = "test fakes lock uncontended mutexes")]
#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim::agent::tools::{BoxFuture, ToolHost};
use aim::host::{Connected, HostConfig, NativeServices, SessionClient, SessionHost, UpdateStream, WorkspaceFactory, native_backends_with};
use aim::media::MediaService;
use aim::resources::files::{FileText, FilesFuture, Read};
use aim::resources::{
    self, Bounds, Catalog, Files, HarnessFiles, LocalFiles, MemoryFiles, Problem, ResourceConfig, Scope, Source, Trust, instructions,
};
use aim::store::{MemoryStore, SessionStore as _};
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_llm_codex::media::{Image, SearchAnswer};
use aim_proto::content::Content;
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::SessionAgent;
use aim_proto::harness::{ContentHash, DirEntry, FsList, FsListResult, FsRead, FsReadMany, FsReadManyResult, FsReadResult, ReadManyEntry};
use aim_proto::ids::{IdempotencyKey, WorkspaceId};
use aim_proto::tool::{ToolAnnotations, ToolResult, ToolSpec};
use aim_rpc::{NoHandler, Peer, PeerConfig, Router};
use futures_util::StreamExt as _;
use serde_json::{Value, json};

// ------------------------------------------------------------------------------------------------
// fixtures
// ------------------------------------------------------------------------------------------------

const HAIKU: &str = "---\nname: haiku\ndescription: Reply in a haiku (5-7-5 syllables).\n---\nAnswer only with one haiku: three lines of 5, 7 and 5 syllables. No other text.\n";

/// A project with every format aim reads: native, Claude Code, Codex, pi, oh-my-pi; with
/// collisions, a missing frontmatter, a bad schema, a skill directory without `SKILL.md` and an
/// executable-looking import.
fn project() -> Vec<(&'static str, String)> {
    let mut memory = String::from("# Project memory\n- [Build](build.md): use `mise run check`\n");
    for i in 0..100 {
        writeln!(memory, "- note {i}").unwrap();
    }
    vec![
        ("AGENTS.md", "Root: be terse.".into()),
        ("CLAUDE.md", "Claude-only root file.".into()),
        ("sub/CLAUDE.md", "Sub: prefer small diffs.".into()),
        (".agents/instructions.md", "aim: run the gate before committing.".into()),
        (".agents/rules/rust.md", "---\ndescription: Rust style.\npaths: [\"src/**/*.rs\", \"crates/**/*.rs\"]\n---\nUse `expect` with reasons.\n".into()),
        (".agents/rules/always.md", "Always cite sources.\n".into()),
        (".agents/skills/haiku/SKILL.md", HAIKU.into()),
        (".agents/skills/review/SKILL.md", "---\nname: review\ndescription: Review a change.\n---\nNative review steps.\n".into()),
        (".agents/skills/nofm/SKILL.md", "# A skill without frontmatter\n".into()),
        (".agents/skills/assets-only/README.md", "not a skill".into()),
        (".agents/skills/big/SKILL.md", format!("---\nname: big\ndescription: A large skill.\n---\n{}\n", "x".repeat(10_000))),
        (".agents/agents/reader.md", "---\nschema: aim.agent/v1\nname: reader\ndescription: Reads, never writes.\nprovider: scripted\nmodel: agent-model\neffort: low\ntools: [Read]\n---\nYou only read files.\n".into()),
        (".agents/agents/badschema.md", "---\nschema: aim.agent/v2\nname: badschema\ndescription: d\n---\n".into()),
        (".agents/agents/noschema.md", "Just instructions, no frontmatter.\n".into()),
        (".agents/prompts/fix.md", "---\ndescription: Fix an issue.\nargument-hint: <issue> <file>\n---\nFix issue $1 in $2. ($ARGUMENTS)\n".into()),
        (".agents/memory/MEMORY.md", memory),
        (".claude/skills/review/SKILL.md", "---\nname: review\ndescription: Claude's review.\n---\nClaude review steps.\n".into()),
        (".claude/skills/pdf/SKILL.md", "---\nallowed-tools: Read\n---\n# PDF\n\nFill PDF forms.\n".into()),
        (".claude/agents/web.md", "---\nname: web\ndescription: Browses.\ntools: Read, WebFetch\nmcpServers: [docs]\n---\nBrowse.\n".into()),
        (".claude/agents/grep-only.md", "---\nname: searcher\ndescription: Searches.\ntools: Grep, Glob\nmodel: haiku\n---\nSearch only.\n".into()),
        (".claude/commands/commit.md", "---\ndescription: Commit staged work.\n---\nCommit with message: $ARGUMENTS\n".into()),
        (".claude/commands/frontend/component.md", "Create component $0 in $1.\n".into()),
        (".claude/rules/tests.md", "---\npaths: tests/**\n---\nTests use fixtures.\n".into()),
        (".codex/agents/explorer.toml", "name = \"explorer\"\ndescription = \"Explores.\"\ndeveloper_instructions = \"Look around.\"\nmodel = \"gpt-6-sol\"\n".into()),
        (".pi/skills/pi-skill/SKILL.md", "---\nname: pi-skill\ndescription: A pi skill.\n---\npi body\n".into()),
        (".pi/prompts/review-pi.md", "Review focusing on ${1:-correctness}.\n".into()),
        (".omp/skills/omp-skill/SKILL.md", "---\nname: omp-skill\ndescription: An oh-my-pi skill.\n---\nomp body\n".into()),
        (".mcp.json", "{\"mcpServers\":{\"docs\":{\"command\":\"/bin/false\"}}}".into()),
    ]
}

/// The user's `~/.aim`.
fn user_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("skills/deploy/SKILL.md", "---\nname: deploy\ndescription: Deploy the app.\n---\nUser deploy steps.\n"),
        ("skills/haiku/SKILL.md", "---\nname: haiku\ndescription: The user's haiku.\n---\nUser haiku.\n"),
        ("agents/reader.md", "---\nschema: aim.agent/v1\nname: reader\ndescription: User reader.\n---\nUser reader.\n"),
        ("prompts/standup.md", "Write my standup about $ARGUMENTS.\n"),
        ("memory/MEMORY.md", "- The user prefers terse answers.\n"),
    ] {
        write(home.path(), path, text);
    }
    home
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn small_bounds() -> Bounds {
    Bounds { max_file_bytes: 4096, ..Bounds::default() }
}

fn names<'a>(items: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
    items.into_iter().collect()
}

fn problems(catalog: &Catalog, problem: Problem) -> Vec<&str> {
    catalog.diagnostics.iter().filter(|d| d.problem == problem).map(|d| d.path.as_str()).collect()
}

// ------------------------------------------------------------------------------------------------
// a fake harness peer: the SSH shape
// ------------------------------------------------------------------------------------------------

/// Serves `fs.list`, `fs.read_many` and `fs.read` from memory and logs every request.
struct FakeHarness {
    files: MemoryFiles,
    log: Mutex<Vec<String>>,
}

fn read_result(file: FileText) -> FsReadResult {
    FsReadResult {
        content: Content::Utf8 { text: file.text },
        size: file.size,
        hash: Some(ContentHash(file.hash)),
        truncated: file.truncated,
    }
}

/// A client peer connected to a fake harness serving `files`.
fn fake_harness(files: MemoryFiles) -> (Peer, Arc<FakeHarness>, Peer) {
    let state = Arc::new(FakeHarness { files, log: Mutex::new(Vec::new()) });
    let router = Router::new(Arc::clone(&state))
        .method::<FsList, _, _>(|state, _, params| async move {
            state.log.lock().unwrap().push(format!("fs.list {}", params.path));
            match state.files.list(&params.path, params.limit.unwrap_or(1000)).await {
                Ok(Some(entries)) => Ok(FsListResult { entries, next_page: None }),
                Ok(None) => Err(ProtoError::new(ErrorCode::NotFound, format!("{}: no such directory", params.path))),
                Err(message) => Err(ProtoError::new(ErrorCode::Internal, message)),
            }
        })
        .method::<FsReadMany, _, _>(|state, _, params| async move {
            state.log.lock().unwrap().push(format!("fs.read_many {}", params.paths.join(",")));
            let reads = state.files.read_many(params.paths.clone(), params.max_bytes_per_file.unwrap_or(u64::MAX)).await;
            let entries = params
                .paths
                .into_iter()
                .zip(reads)
                .map(|(path, read)| match read {
                    Read::Ok(file) => ReadManyEntry::Ok { path, read: read_result(file) },
                    Read::Missing => ReadManyEntry::Error { path, code: "not_found".into(), message: "no such file".into() },
                    Read::Failed(message) => ReadManyEntry::Error { path, code: "internal".into(), message },
                })
                .collect();
            Ok(FsReadManyResult { entries })
        })
        .method::<FsRead, _, _>(|state, _, params| async move {
            state.log.lock().unwrap().push(format!("fs.read {}", params.path));
            match state.files.read_many(vec![params.path.clone()], u64::MAX).await.pop() {
                Some(Read::Ok(file)) => Ok(read_result(file)),
                _ => Err(ProtoError::new(ErrorCode::NotFound, params.path)),
            }
        });
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let server = Peer::spawn(br, bw, router, PeerConfig::default());
    let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
    (client, state, server)
}

// ------------------------------------------------------------------------------------------------
// discovery
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn foreign_import_provenance() {
    let home = user_home();
    let config = ResourceConfig { user_home: Some(home.path().to_path_buf()), bounds: small_bounds(), skill_budget: None };
    let files = MemoryFiles::new(project());
    let catalog = resources::discover(&config, Some(&files), "local", "sub").await;

    // Skills: native project, user, then foreign; each winner once.
    assert_eq!(
        names(catalog.skills.iter().map(|s| s.meta.name.as_str())),
        names(["haiku", "review", "big", "deploy", "pdf", "pi-skill", "omp-skill"])
    );
    let haiku = catalog.skill("haiku").unwrap();
    assert_eq!((haiku.meta.scope, haiku.meta.source, haiku.meta.trust), (Scope::Project, Source::Native, Trust::Workspace));
    assert_eq!(catalog.skill("review").unwrap().meta.path, ".agents/skills/review/SKILL.md");
    let pdf = catalog.skill("pdf").unwrap();
    assert_eq!((pdf.meta.source, pdf.meta.trust, pdf.meta.parser_version), (Source::Claude, Trust::Imported, "agentskills/1"));
    assert_eq!(pdf.meta.description, "Fill PDF forms.");
    let deploy = catalog.skill("deploy").unwrap();
    assert_eq!((deploy.meta.scope, deploy.meta.trust, deploy.meta.location.as_str()), (Scope::User, Trust::User, "local"));
    assert!(Path::new(&deploy.meta.path).starts_with(home.path()));
    assert_eq!(catalog.skill("pi-skill").unwrap().meta.source, Source::Pi);
    assert_eq!(catalog.skill("omp-skill").unwrap().meta.source, Source::OhMyPi);
    assert!(catalog.skill("big").unwrap().truncated);

    // Agents: native, Claude (one importable, one not), Codex.
    assert_eq!(names(catalog.agents.iter().map(|a| a.meta.name.as_str())), names(["reader", "web", "searcher", "explorer"]));
    assert_eq!(catalog.agent("reader").unwrap().meta.scope, Scope::Project, "the project's reader wins over the user's");
    assert!(catalog.agent("web").unwrap().importable.as_ref().is_err_and(|why| why.contains("WebFetch")));
    let searcher = catalog.agent("searcher").unwrap();
    assert!(searcher.importable.is_ok() && searcher.tools.permits("Grep") && !searcher.tools.permits("Write"));
    assert_eq!(searcher.meta.parser_version, "claude.agent/1");
    let explorer = catalog.agent("explorer").unwrap();
    assert_eq!((explorer.meta.source, explorer.provider.as_deref()), (Source::Codex, Some("codex")));

    // Prompts in each dialect.
    assert_eq!(catalog.expand_prompt("fix", "42 main.rs").as_deref(), Some("Fix issue 42 in main.rs. (42 main.rs)"));
    assert_eq!(catalog.expand_prompt("commit", "\"fix: typo\"").as_deref(), Some("Commit with message: \"fix: typo\""));
    assert_eq!(catalog.expand_prompt("frontend:component", "Button src/ui").as_deref(), Some("Create component Button in src/ui."));
    assert_eq!(catalog.expand_prompt("review-pi", "").as_deref(), Some("Review focusing on correctness."));
    assert_eq!(catalog.expand_prompt("standup", "the parser").as_deref(), Some("Write my standup about the parser."));
    assert_eq!(catalog.prompt("fix").unwrap().argument_hint.as_deref(), Some("<issue> <file>"));
    assert!(catalog.expand_prompt("nope", "").is_none());

    // Rules, instructions (root → cwd, CLAUDE.md where there is no AGENTS.md), memory.
    assert_eq!(names(catalog.rules.iter().map(|r| r.meta.name.as_str())), names(["rust", "always", "tests"]));
    let paths: Vec<(&str, Source)> = catalog.instructions.iter().map(|i| (i.meta.path.as_str(), i.meta.source)).collect();
    assert_eq!(paths, [("AGENTS.md", Source::Native), ("sub/CLAUDE.md", Source::Claude), (".agents/instructions.md", Source::Native)]);
    assert_eq!(catalog.memory.len(), 2);
    assert!(catalog.memory[0].cut, "100 lines are cut to the index size");
    assert!(catalog.memory[0].text.lines().count() <= instructions::MEMORY_LINES);

    // Every descriptor carries a content hash; diagnostics name every problem.
    assert!(catalog.descriptors().iter().all(|d| d.hash.starts_with("sha256:") && d.hash.len() == 71));
    let collisions = problems(&catalog, Problem::Collision);
    for shadowed in [".claude/skills/review/SKILL.md", "CLAUDE.md", "skills/haiku/SKILL.md", "agents/reader.md"] {
        assert!(collisions.iter().any(|p| p.ends_with(shadowed)), "{shadowed} in {collisions:?}");
    }
    let invalid = problems(&catalog, Problem::Invalid);
    for bad in
        [".agents/skills/nofm/SKILL.md", ".agents/skills/assets-only/SKILL.md", ".agents/agents/badschema.md", ".agents/agents/noschema.md"]
    {
        assert!(invalid.contains(&bad), "{bad} in {invalid:?}");
    }
    assert!(problems(&catalog, Problem::Truncated).contains(&".agents/skills/big/SKILL.md"));
    assert_eq!(problems(&catalog, Problem::NotImportable), [".claude/agents/web.md"]);
    let unsupported: Vec<&str> =
        catalog.diagnostics.iter().filter(|d| d.problem == Problem::Unsupported).map(|d| d.message.as_str()).collect();
    assert!(unsupported.iter().any(|m| m.contains("`model: haiku` names a Claude model")), "{unsupported:?}");
}

#[tokio::test]
async fn imported_mcp_requires_opt_in() {
    let (peer, harness, _server) = fake_harness(MemoryFiles::new(project()));
    let files = HarnessFiles::new(peer, WorkspaceId::new("w"));
    let catalog = resources::discover(&ResourceConfig::default(), Some(&files), "local", "").await;
    // Discovery never reads MCP or hook configuration, let alone starts it.
    let log = harness.log.lock().unwrap().join("\n");
    assert!(!log.contains(".mcp.json") && !log.contains("hooks"), "{log}");
    // A Claude agent's MCP servers are reported as needing an opt-in, not started.
    let web: Vec<&str> = catalog.diagnostics.iter().filter(|d| d.path == ".claude/agents/web.md").map(|d| d.message.as_str()).collect();
    assert!(web.iter().any(|m| m.contains("`mcpServers`") && m.contains("opt-in")), "{web:?}");
}

#[tokio::test]
async fn remote_project_resources() {
    // The remote project, served by a fake harness peer.
    let (peer, harness, _server) = fake_harness(MemoryFiles::new(project()));
    let files = HarnessFiles::new(peer, WorkspaceId::new("w"));
    let catalog = resources::discover(&ResourceConfig::default(), Some(&files), "ssh:example", "").await;
    assert!(!catalog.skills.is_empty());
    assert!(catalog.descriptors().iter().all(|d| d.location == "ssh:example" && d.is_remote()));
    let log = harness.log.lock().unwrap().clone();
    assert!(log.iter().all(|l| l.starts_with("fs.list ") || l.starts_with("fs.read_many ")), "{log:?}");
    // Two round trips (listings with the fixed reads, then the listed files) plus one for Claude's
    // namespaced commands.
    assert_eq!(log.iter().filter(|l| l.starts_with("fs.read_many")).count(), 2, "{log:?}");
    let prefix = aim::context::instructions(&catalog, None, 4096).text;
    assert!(prefix.contains("## AGENTS.md (remote, ssh:example)"), "{prefix}");
    assert!(prefix.contains(".agents/skills/haiku/SKILL.md; remote, ssh:example"), "{prefix}");
}

#[tokio::test]
async fn remote_sessions_never_read_the_local_project() {
    // A local directory at the session's path holds decoys; the session must use the remote's.
    let local = tempfile::tempdir().unwrap();
    write(local.path(), "AGENTS.md", "LOCAL DECOY");
    write(local.path(), ".agents/skills/decoy/SKILL.md", "---\nname: decoy\ndescription: decoy\n---\ndecoy\n");
    let (peer, _harness, server) =
        fake_harness(MemoryFiles::new([("AGENTS.md", "Remote rules."), (".agents/skills/haiku/SKILL.md", HAIKU)]));
    let project: Arc<dyn Files> = Arc::new(HarnessFiles::new(peer, WorkspaceId::new("w")));
    let f = fixture_with(vec![text("ok")], move |_spec| Connected {
        tools: Arc::new(FakeTools::default()),
        root: "/remote/w".into(),
        location: "ssh:example".into(),
        project: Some(Arc::clone(&project)),
        shutdown: Box::new(|| Box::pin(async {})),
    });
    let mut spec = spec(None);
    spec.workspace = local.path().to_string_lossy().into_owned();
    spec.location = Location::Ssh { destination: "example".into() };
    turns(&f, spec, &["$haiku hello"]).await;
    let seen = f.provider.seen.lock().unwrap().clone();
    assert!(seen[0].instructions.contains("Remote rules.") && seen[0].instructions.contains("haiku"));
    assert!(!seen[0].instructions.contains("DECOY") && !seen[0].instructions.contains("decoy"));
    server.close();
}

#[tokio::test]
async fn real_aimx_serves_project_resources() {
    let Some(aimx) = aimx() else {
        eprintln!("skipped: aimx is not built next to the test binary (run `cargo build -p aimx` or the workspace gate)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in project() {
        write(dir.path(), path, &text);
    }
    // Ignored by git is not absent; a foreign skill tree symlinked to the native one is not a
    // collision; a skill linked from outside the workspace is refused by aimx's confinement.
    write(dir.path(), ".gitignore", ".claude/\n.agents/\n");
    std::fs::remove_dir_all(dir.path().join(".omp")).unwrap();
    std::fs::create_dir(dir.path().join(".omp")).unwrap();
    std::os::unix::fs::symlink("../.agents/skills", dir.path().join(".omp/skills")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "SKILL.md", "---\nname: escape\ndescription: outside the root\n---\nsecret\n");
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".agents/skills/escape")).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let harness = aim::harness::HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root.to_string_lossy()).await.unwrap();
    let files = HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone());
    let started = std::time::Instant::now();
    let found = resources::discover::discover_project(&files, "local", "sub", &small_bounds()).await;
    let elapsed = started.elapsed();
    let catalog = Catalog::assemble(found, resources::Found::default());
    assert_eq!(names(catalog.skills.iter().map(|s| s.meta.name.as_str())), names(["haiku", "review", "big", "pdf", "pi-skill"]));
    assert_eq!(names(catalog.agents.iter().map(|a| a.meta.name.as_str())), names(["reader", "web", "searcher", "explorer"]));
    assert_eq!(catalog.instructions.len(), 3);
    let collisions = problems(&catalog, Problem::Collision);
    assert!(!collisions.iter().any(|p| p.starts_with(".omp/")), "{collisions:?}");
    let escaped: Vec<_> = catalog.diagnostics.iter().filter(|d| d.path == ".agents/skills/escape/SKILL.md").collect();
    assert!(matches!(escaped.as_slice(), [d] if d.problem == Problem::Unreadable), "{escaped:?}");
    eprintln!("escape: {:?}", escaped[0].message);
    assert!(catalog.skill("big").unwrap().truncated);
    // aimx's hash is of the whole file.
    assert!(catalog.skill("haiku").unwrap().meta.hash.starts_with("sha256:"));
    eprintln!("real_aimx_discovery_ms={}", elapsed.as_millis());
    harness.shutdown().await;
}

/// `aimx` next to this test's `aim` binary (built by `cargo test --workspace`).
fn aimx() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_BIN_EXE_aim")).with_file_name("aimx");
    path.exists().then_some(path)
}

// ------------------------------------------------------------------------------------------------
// budgets and bounds
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn instruction_budget_is_shared_and_cuts_are_marked() {
    let files = MemoryFiles::new([
        ("AGENTS.md", "a".repeat(instructions::MAX_PROJECT_INSTRUCTIONS + 100)),
        (".agents/instructions.md", "LATER-CONTENT".to_owned()),
    ]);
    let config = ResourceConfig { bounds: Bounds { max_file_bytes: 64 * 1024, ..Bounds::default() }, ..ResourceConfig::default() };
    let catalog = resources::discover(&config, Some(&files), "local", "").await;
    let prefix = aim::context::instructions(&catalog, None, 4096);
    assert!(prefix.text.contains(&format!(
        "[… truncated: {} of {} bytes shown; read AGENTS.md for the rest]",
        instructions::MAX_PROJECT_INSTRUCTIONS,
        instructions::MAX_PROJECT_INSTRUCTIONS + 100
    )));
    assert!(prefix.text.contains("[… not included: the 32768-byte instruction budget is spent; read .agents/instructions.md if needed]"));
    assert!(!prefix.text.contains("LATER-CONTENT"));
    assert_eq!(prefix.diagnostics.len(), 2);
}

#[tokio::test]
async fn skill_catalog_fits_its_budget() {
    let mut project: Vec<(String, String)> = Vec::new();
    for i in 0..120 {
        let description = format!("Skill number {i} {}", "does a very particular thing with great care ".repeat(6));
        project.push((
            format!(".agents/skills/skill-{i:03}/SKILL.md"),
            format!("---\nname: skill-{i:03}\ndescription: {description}\n---\nbody\n"),
        ));
    }
    let files = MemoryFiles::new(project);
    let catalog = resources::discover(&ResourceConfig::default(), Some(&files), "local", "").await;
    assert_eq!(catalog.skills.len(), 120);
    for budget in [instructions::DEFAULT_SKILL_BUDGET, instructions::skill_budget(Some(200_000))] {
        let prefix = aim::context::instructions(&catalog, None, budget);
        let listing: usize =
            prefix.text.lines().filter(|l| l.starts_with("- skill-") || l.starts_with("- …and")).map(|l| l.len() + 1).sum();
        assert!(listing <= budget, "{listing} > {budget}");
        assert!(prefix.diagnostics.iter().any(|d| d.message.contains("skill catalog fitted")));
        eprintln!("skill_catalog budget={budget} listing={listing} prefix={}", prefix.text.len());
    }
}

#[tokio::test]
async fn discovery_is_bounded_in_files_and_time() {
    let mut many: Vec<(String, String)> = Vec::new();
    for i in 0..20 {
        many.push((format!(".agents/prompts/p{i:02}.md"), format!("Prompt {i}")));
    }
    let files = MemoryFiles::new(many);
    // Nine files: the four fixed ones (AGENTS.md, CLAUDE.md, instructions, memory) count too, even
    // when missing (ADR 0038), leaving five for prompts.
    let config = ResourceConfig { bounds: Bounds { max_files: 9, max_dir_entries: 10, ..Bounds::default() }, ..ResourceConfig::default() };
    let catalog = resources::discover(&config, Some(&files), "local", "").await;
    assert_eq!(catalog.prompts.len(), 5);
    assert_eq!(problems(&catalog, Problem::Limit).len(), 1 + 5, "one listing cut at 10, five files over the count");

    let config = ResourceConfig { bounds: Bounds { timeout: Duration::from_millis(50), ..Bounds::default() }, ..ResourceConfig::default() };
    let catalog = resources::discover(&config, Some(&Stalled), "local", "").await;
    assert!(catalog.descriptors().is_empty());
    assert!(catalog.diagnostics.iter().any(|d| d.problem == Problem::Limit && d.message.contains("took longer")));
}

/// `MemoryFiles` that meters reads: per request, how many files, their cap, and the bytes returned.
struct Metered {
    inner: MemoryFiles,
    requests: Mutex<Vec<(usize, u64, u64)>>,
}

impl Files for Metered {
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        self.inner.list(dir, limit)
    }

    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>> {
        Box::pin(async move {
            let count = paths.len();
            let reads = self.inner.read_many(paths, max_bytes).await;
            let returned = reads.iter().map(|r| if let Read::Ok(file) = r { file.text.len() as u64 } else { 0 }).sum();
            self.requests.lock().unwrap().push((count, max_bytes, returned));
            reads
        })
    }

    fn display(&self, path: &str) -> String {
        self.inner.display(path)
    }
}

#[tokio::test]
async fn fixed_and_listed_files_share_one_admission_budget() {
    // REV8-9: fixed files (the AGENTS.md walk, memory) counted neither files nor bytes, a losing
    // CLAUDE.md was not charged, and a 32-file batch started whenever any budget was left.
    let body = |tag: &str| format!("{tag}{}", "x".repeat(999 - tag.len()));
    let mut project = vec![
        ("AGENTS.md".to_owned(), body("agents")),
        ("CLAUDE.md".to_owned(), body("claude")),
        (".agents/memory/MEMORY.md".to_owned(), body("memory")),
    ];
    project.extend((0..40).map(|i| (format!(".agents/prompts/p{i:02}.md"), body(&format!("prompt {i} ")))));
    let files = Metered { inner: MemoryFiles::new(project), requests: Mutex::default() };
    let bounds = Bounds { max_file_bytes: 1000, max_total_bytes: 10_500, ..Bounds::default() };
    let found = resources::discover::discover_project(&files, "local", "", &bounds).await;
    let requests = files.requests.lock().unwrap().clone();
    let mut returned = 0_u64;
    for (count, cap, got) in &requests {
        assert!(*count as u64 * cap <= bounds.max_total_bytes - returned, "a read that could overrun the budget: {requests:?}");
        returned += got;
    }
    assert!(returned <= bounds.max_total_bytes, "{returned} bytes read");
    // Fixed: 3,000 bytes (the losing CLAUDE.md included); then seven 1,000-byte prompts fit.
    assert_eq!(found.prompts.len(), 7);
    assert_eq!(found.diagnostics.iter().filter(|d| d.problem == Problem::Limit && d.message.contains("budget")).count(), 33);

    // No budget at all: nothing is requested, fixed files included.
    let files = Metered { inner: MemoryFiles::new([("AGENTS.md", "hi"), (".agents/prompts/p.md", "p")]), requests: Mutex::default() };
    let none = Bounds { max_files: 0, max_total_bytes: 0, ..Bounds::default() };
    let found = resources::discover::discover_project(&files, "local", "", &none).await;
    assert_eq!(files.requests.lock().unwrap().iter().map(|r| r.0).sum::<usize>(), 0);
    assert!(found.instructions.is_empty() && found.prompts.is_empty());
}

#[test]
fn a_fifo_memory_index_never_blocks_discovery_or_exit() {
    // REV8-8: a FIFO at ~/.aim/memory/MEMORY.md blocked a worker thread past the discovery deadline
    // and held the runtime's shutdown.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("memory")).unwrap();
    let fifo = home.path().join("memory/MEMORY.md");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let config = ResourceConfig::user(home.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let catalog = runtime.block_on(resources::discover(&config, None, "local", ""));
        drop(runtime);
        let _sent = tx.send(catalog.diagnostics);
    });
    let outcome = rx.recv_timeout(Duration::from_secs(5));
    if outcome.is_err() {
        drop(std::fs::OpenOptions::new().write(true).open(&fifo));
    }
    let diagnostics = outcome.expect("discovery and runtime shutdown finish");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(diagnostics.iter().any(|d| d.problem == Problem::Unreadable && d.message.contains("not a regular file")), "{diagnostics:?}");
}

#[tokio::test]
async fn real_aimx_keeps_a_project_instruction_split_at_the_byte_limit() {
    let Some(aimx) = aimx() else {
        eprintln!("skipped: aimx is not built next to the test binary (run `cargo build -p aimx` or the workspace gate)");
        return;
    };
    // REV8-10: the 65,536-byte cut splits `é`; aimx sends the bytes as base64. The valid prefix is
    // kept and AGENTS.md still takes precedence over CLAUDE.md.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "AGENTS.md", &format!("{}é and more", "a".repeat(65_535)));
    write(dir.path(), "CLAUDE.md", "Claude fallback.");
    let root = dir.path().canonicalize().unwrap();
    let harness = aim::harness::HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root.to_string_lossy()).await.unwrap();
    let files = HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone());
    let found = resources::discover::discover_project(&files, "local", "", &Bounds::default()).await;
    harness.shutdown().await;
    let [agents] = found.instructions.as_slice() else { panic!("{:?}", found.diagnostics) };
    assert!(agents.meta.path == "AGENTS.md" && agents.truncated, "{:?}", agents.meta);
    assert_eq!(agents.text.len(), 65_535);
    assert!(!found.diagnostics.iter().any(|d| d.problem == Problem::Unreadable), "{:?}", found.diagnostics);
}

/// A project whose reads never finish.
struct Stalled;

impl Files for Stalled {
    fn list<'a>(&'a self, _dir: &'a str, _limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        Box::pin(std::future::pending())
    }
    fn read_many(&self, _paths: Vec<String>, _max_bytes: u64) -> FilesFuture<'_, Vec<Read>> {
        Box::pin(std::future::pending())
    }
    fn display(&self, path: &str) -> String {
        path.to_owned()
    }
}

#[tokio::test]
async fn user_resources_are_read_locally() {
    let home = user_home();
    let found = resources::discover::discover_user(&LocalFiles::new(home.path()), &Bounds::default()).await;
    assert_eq!(names(found.skills.iter().map(|s| s.meta.name.as_str())), names(["deploy", "haiku"]));
    assert_eq!(found.memory.len(), 1);
    assert!(found.diagnostics.is_empty(), "{:?}", found.diagnostics);
}

// ------------------------------------------------------------------------------------------------
// sessions: mentions and agents
// ------------------------------------------------------------------------------------------------

struct Scripted {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
    seen: Mutex<Vec<Request>>,
    models: Vec<ModelInfo>,
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        let models = self.models.clone();
        Box::pin(async move { Ok(models) })
    }

    fn stream(&self, request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        self.seen.lock().unwrap().push(request);
        let next = self.responses.lock().unwrap().pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| LlmError::new(LlmErrorKind::InvalidRequest, "script exhausted"))?;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events));
            Ok(stream)
        })
    }
}

fn completed(stop: StopReason) -> Result<StreamEvent, LlmError> {
    Ok(StreamEvent::Completed { response_id: None, usage: Usage { input_tokens: 10, output_tokens: 2, ..Usage::default() }, stop })
}

fn text(t: &str) -> Vec<Result<StreamEvent, LlmError>> {
    vec![
        Ok(StreamEvent::TextDelta { item_id: "m".into(), delta: t.into() }),
        Ok(StreamEvent::ItemDone { item: Item::Assistant { id: None, parts: vec![Part::Text { text: t.into() }], native: None } }),
        completed(StopReason::EndTurn),
    ]
}

fn call(call_id: &str, name: &str) -> Result<StreamEvent, LlmError> {
    let arguments = json!({"file_path": "x"}).to_string();
    Ok(StreamEvent::ItemDone { item: Item::ToolCall { call_id: call_id.into(), name: name.into(), arguments, native: None } })
}

/// `Read` and `Write`; records calls.
#[derive(Default)]
struct FakeTools {
    calls: Mutex<Vec<String>>,
}

impl ToolHost for FakeTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["Read", "Write"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: name.into(),
                input_schema: json!({"type": "object"}),
                input: aim_proto::tool::ToolInput::default(),
                annotations: ToolAnnotations::default(),
            })
            .collect()
    }

    fn call(&self, name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.calls.lock().unwrap().push(name.clone());
        Box::pin(async move { Ok(ToolResult::text(format!("{name} ok"))) })
    }
}

struct Fixture {
    host: SessionHost,
    provider: Arc<Scripted>,
}

fn fixture_with(
    script: Vec<Vec<Result<StreamEvent, LlmError>>>,
    connect: impl Fn(&SessionSpec) -> Connected + Send + Sync + 'static,
) -> Fixture {
    fixture_on(Arc::new(MemoryStore::default()), Vec::new(), script, NativeServices::default(), connect)
}

fn fixture_on(
    store: Arc<MemoryStore>,
    models: Vec<ModelInfo>,
    script: Vec<Vec<Result<StreamEvent, LlmError>>>,
    services: NativeServices,
    connect: impl Fn(&SessionSpec) -> Connected + Send + Sync + 'static,
) -> Fixture {
    let provider = Arc::new(Scripted { responses: Mutex::new(script.into()), seen: Mutex::default(), models });
    let connect = Arc::new(connect);
    let workspaces: WorkspaceFactory = Arc::new(move |spec: &SessionSpec| {
        let connected = connect(spec);
        Box::pin(async move { Ok(connected) })
    });
    let for_factory = Arc::clone(&provider);
    let host = SessionHost::new(HostConfig {
        store,
        backends: native_backends_with(
            Arc::new(move |_name, _model| Ok((Arc::clone(&for_factory) as Arc<dyn ModelProvider>, "m1".to_owned()))),
            workspaces,
            8,
            ResourceConfig::default(),
            services,
        ),
        update_capacity: 256,
    });
    Fixture { host, provider }
}

fn local_fixture(script: Vec<Vec<Result<StreamEvent, LlmError>>>, tools: Arc<FakeTools>) -> Fixture {
    let files: Arc<dyn Files> = Arc::new(MemoryFiles::new(project()));
    fixture_with(script, move |spec| Connected {
        tools: Arc::clone(&tools) as Arc<dyn ToolHost>,
        root: spec.workspace.clone(),
        location: "local".into(),
        project: Some(Arc::clone(&files)),
        shutdown: Box::new(|| Box::pin(async {})),
    })
}

fn spec(agent: Option<&str>) -> SessionSpec {
    SessionSpec {
        workspace: "/w".into(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: agent.map(str::to_owned),
        persistence: Persistence::Ephemeral,
    }
}

/// Runs `prompts` as consecutive turns of a new session; returns every update.
async fn turns(f: &Fixture, spec: SessionSpec, prompts: &[&str]) -> Vec<SessionUpdate> {
    let id = f.host.create(spec).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    let mut all = Vec::new();
    for prompt in prompts {
        f.host.prompt(id.clone(), vec![Part::Text { text: (*prompt).to_owned() }]).await.unwrap();
        all.extend(until_idle(&mut updates).await);
    }
    all
}

async fn until_idle(updates: &mut UpdateStream) -> Vec<SessionUpdate> {
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.next()).await.unwrap().unwrap();
        let done = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        got.push(update);
        if done {
            return got;
        }
    }
}

fn texts(parts: &[Part]) -> Vec<&str> {
    parts
        .iter()
        .map(|p| match p {
            Part::Text { text } => text.as_str(),
            Part::Image { .. } => "<image>",
        })
        .collect()
}

#[tokio::test]
async fn skill_mention_user_turn() {
    let f = local_fixture(vec![text("one"), text("two"), text("three")], Arc::default());
    turns(&f, spec(None), &["$haiku about rain, and $nope", "no mention", "again: $haiku."]).await;
    let seen = f.provider.seen.lock().unwrap().clone();
    // The cached prefix never changes, and carries the catalog, not the body.
    assert!(seen.iter().all(|r| r.instructions == seen[0].instructions));
    assert!(seen[0].instructions.contains("- haiku: Reply in a haiku (5-7-5 syllables). (.agents/skills/haiku/SKILL.md)"));
    assert!(!seen[0].instructions.contains("three lines of 5, 7 and 5"));
    // Turn 1: environment, then the skill, then the user's text unchanged; `$nope` changes nothing.
    let Some(Item::User { parts }) = seen[0].items.last() else { panic!("user item last") };
    let parts = texts(parts);
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(parts[0].starts_with("<environment>"));
    assert!(parts[1].starts_with("<skill name=\"haiku\" path=\".agents/skills/haiku/SKILL.md\">\nAnswer only with one haiku"));
    assert!(parts[1].ends_with("</skill>"));
    assert_eq!(parts[2], "$haiku about rain, and $nope");
    // Turn 2 has no mention; turn 3 injects the skill again.
    let Some(Item::User { parts }) = seen[1].items.last() else { panic!() };
    assert_eq!(texts(parts), ["no mention"]);
    let Some(Item::User { parts }) = seen[2].items.last() else { panic!() };
    assert_eq!(texts(parts).len(), 2);
    assert!(texts(parts)[0].starts_with("<skill name=\"haiku\""));
}

#[tokio::test]
async fn agent_allowlist_refuses_calls() {
    let tools = Arc::new(FakeTools::default());
    let script = vec![vec![call("c1", "Write"), call("c2", "Read"), completed(StopReason::ToolUse)], text("done")];
    let f = local_fixture(script, Arc::clone(&tools));
    let updates = turns(&f, spec(Some("reader")), &["write then read"]).await;
    let seen = f.provider.seen.lock().unwrap().clone();
    // Only the allowed tools are offered, with the agent's defaults and instructions.
    assert_eq!(seen[0].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Read"]);
    assert_eq!((seen[0].model.as_str(), seen[0].effort.as_deref()), ("agent-model", Some("low")));
    assert!(seen[0].instructions.ends_with("# Agent: reader\n\nYou only read files.\n"), "{}", seen[0].instructions);
    assert_eq!(seen[0].cache_key.as_deref(), Some("aim:/w:agent:reader"));
    // A call outside the list never reaches the tools and comes back as a failed result.
    assert_eq!(*tools.calls.lock().unwrap(), ["Read"]);
    let finished: Vec<(&str, bool, String)> = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::ToolFinished { name, result, .. } => Some((name.as_str(), result.is_error, format!("{:?}", result.content))),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 2);
    assert!(
        finished.iter().any(|(n, e, t)| *n == "Write" && *e && t.contains("denied") && t.contains("may not use `Write`")),
        "{finished:?}"
    );
    assert!(finished.iter().any(|(n, e, _)| *n == "Read" && !*e));
}

#[tokio::test]
async fn unknown_and_unimportable_agents_are_refused() {
    let f = local_fixture(vec![], Arc::default());
    let unknown = f.host.create(spec(Some("ghost"))).await.err().unwrap();
    assert_eq!(unknown.code, ErrorCode::NotFound);
    assert!(unknown.message.contains("reader"), "{}", unknown.message);
    let refused = f.host.create(spec(Some("web"))).await.err().unwrap();
    assert_eq!(refused.code, ErrorCode::InvalidParams);
    assert!(refused.message.contains("WebFetch"), "{}", refused.message);
    // An importable Claude agent narrows the tools like a native one.
    let tools = Arc::new(FakeTools::default());
    let f = local_fixture(vec![text("ok")], tools);
    turns(&f, spec(Some("searcher")), &["hi"]).await;
    assert!(f.provider.seen.lock().unwrap()[0].tools.is_empty(), "Grep and Glob are not offered by this workspace");
}

/// Media that is always available; counts its calls.
#[derive(Default)]
struct FakeMedia {
    calls: Mutex<Vec<String>>,
}

impl MediaService for FakeMedia {
    fn search_enabled(&self) -> bool {
        true
    }

    fn image_enabled(&self) -> bool {
        true
    }

    fn web_search(&self, query: String) -> BoxFuture<Result<SearchAnswer, LlmError>> {
        self.calls.lock().unwrap().push(format!("web_search {query}"));
        Box::pin(async { Ok(SearchAnswer { text: "found".into(), citations: Vec::new(), queries: Vec::new() }) })
    }

    fn generate_image(&self, prompt: String, _size: Option<String>, _quality: Option<String>) -> BoxFuture<Result<Image, LlmError>> {
        self.calls.lock().unwrap().push(format!("generate_image {prompt}"));
        Box::pin(async { Err(LlmError::new(LlmErrorKind::Unavailable, "no images in tests")) })
    }
}

fn with_media(media: &Arc<FakeMedia>) -> NativeServices {
    let media = Arc::clone(media);
    NativeServices {
        media: Some(Arc::new(move || {
            let media = Arc::clone(&media) as Arc<dyn MediaService>;
            Box::pin(async move { Some(media) })
        })),
        decider: None,
        tools: Vec::new(),
        code: None,
    }
}

/// A host over `store` whose sessions see `files` as their project, `tools` and `media`.
fn project_host(
    store: &Arc<MemoryStore>,
    files: Vec<(&'static str, String)>,
    script: Vec<Vec<Result<StreamEvent, LlmError>>>,
    tools: &Arc<FakeTools>,
    media: &Arc<FakeMedia>,
) -> Fixture {
    let files: Arc<dyn Files> = Arc::new(MemoryFiles::new(files));
    let tools = Arc::clone(tools);
    fixture_on(Arc::clone(store), Vec::new(), script, with_media(media), move |spec| Connected {
        tools: Arc::clone(&tools) as Arc<dyn ToolHost>,
        root: spec.workspace.clone(),
        location: "local".into(),
        project: Some(Arc::clone(&files)),
        shutdown: Box::new(|| Box::pin(async {})),
    })
}

fn persistent(agent: &str) -> SessionSpec {
    SessionSpec { persistence: Persistence::Persistent, ..spec(Some(agent)) }
}

/// A project whose `reader` agent allows `tools`, or has no `reader` (`None`).
fn reader_project(tools: Option<&str>) -> Vec<(&'static str, String)> {
    let mut files: Vec<(&'static str, String)> = vec![("AGENTS.md", "Root: be terse.".into())];
    if let Some(tools) = tools {
        files.push((
            ".agents/agents/reader.md",
            format!(
                "---\nschema: aim.agent/v1\nname: reader\ndescription: Reads, never writes.\ntools: [{tools}]\n---\nYou only read files.\n"
            ),
        ));
    }
    files
}

#[tokio::test]
async fn a_resumed_named_agent_keeps_its_tool_ceiling() {
    let store = Arc::new(MemoryStore::default());
    let (tools, media) = (Arc::new(FakeTools::default()), Arc::new(FakeMedia::default()));
    let first = project_host(&store, reader_project(Some("Read")), vec![text("hi")], &tools, &media);
    let id = first.host.create(persistent("reader")).await.unwrap().meta.id;
    let (_, mut updates) = first.host.attach(id.clone()).await.unwrap();
    first.host.prompt(id.clone(), vec![Part::Text { text: "hello".into() }]).await.unwrap();
    until_idle(&mut updates).await;
    first.host.shutdown().await.unwrap();
    let (meta, _) = store.load(id.clone()).await.unwrap();
    assert_eq!(meta.agent, Some(SessionAgent { name: "reader".into(), allow: Some(vec!["Read".into()]), deny: Vec::new() }));

    // A restarted daemon resumes the session: the model asks for Write and a media tool (REV8-2).
    let script = vec![vec![call("c1", "Write"), call("c2", "generate_image"), completed(StopReason::ToolUse)], text("done")];
    let second = project_host(&store, reader_project(Some("Read")), script, &tools, &media);
    let (_, mut updates) = second.host.attach(id.clone()).await.unwrap();
    second.host.prompt(id.clone(), vec![Part::Text { text: "write it".into() }]).await.unwrap();
    let got = until_idle(&mut updates).await;
    let seen = second.provider.seen.lock().unwrap().clone();
    assert_eq!(seen[0].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Read"], "Write and media are not offered");
    assert_eq!(seen[0].cache_key.as_deref(), Some("aim:/w:agent:reader"));
    assert!(seen[0].instructions.ends_with("# Agent: reader\n\nYou only read files.\n"), "{}", seen[0].instructions);
    assert!(tools.calls.lock().unwrap().is_empty(), "Write never reached the workspace");
    assert!(media.calls.lock().unwrap().is_empty(), "no media call left the machine");
    let denied =
        got.iter().filter(|u| matches!(u, SessionUpdate::ToolFinished { result, .. } if result.is_error && format!("{:?}", result.content).contains("denied"))).count();
    assert_eq!(denied, 2, "{got:?}");
    second.host.shutdown().await.unwrap();

    // Widening the definition later cannot widen the session it was recorded for.
    let third = project_host(&store, reader_project(Some("Read, Write, generate_image")), vec![text("ok")], &tools, &media);
    let (_, mut updates) = third.host.attach(id.clone()).await.unwrap();
    third.host.prompt(id, vec![Part::Text { text: "again".into() }]).await.unwrap();
    until_idle(&mut updates).await;
    let offered: Vec<String> = third.provider.seen.lock().unwrap()[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(offered, ["Read"]);
}

#[tokio::test]
async fn a_resumed_agent_session_is_refused_when_its_definition_is_gone() {
    let store = Arc::new(MemoryStore::default());
    let (tools, media) = (Arc::new(FakeTools::default()), Arc::new(FakeMedia::default()));
    let first = project_host(&store, reader_project(Some("Read")), vec![text("hi")], &tools, &media);
    let id = first.host.create(persistent("reader")).await.unwrap().meta.id;
    first.host.shutdown().await.unwrap();
    // Fail closed: without its definition the session is not resumed with every tool.
    let mut unimportable = reader_project(None);
    unimportable
        .push((".claude/agents/reader.md", "---\nname: reader\ndescription: Browses.\ntools: Read, WebFetch\n---\nBrowse.\n".into()));
    for (project, code) in [(reader_project(None), ErrorCode::NotFound), (unimportable, ErrorCode::InvalidParams)] {
        let later = project_host(&store, project, vec![text("unrestricted")], &tools, &media);
        let refused = later.host.attach(id.clone()).await.err().unwrap();
        assert_eq!(refused.code, code, "{refused:?}");
        assert!(refused.message.contains("cannot resume without it"), "{}", refused.message);
        assert!(later.provider.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn the_skill_budget_follows_the_agent_model() {
    let model = |id: &str, window: u64| ModelInfo {
        id: id.into(),
        display_name: id.into(),
        context_window: Some(window),
        efforts: Vec::new(),
        default_effort: None,
        tiers: Vec::new(),
        tools: true,
        images: false,
        hidden: false,
        native: None,
    };
    let mut project: Vec<(String, String)> = (0..120)
        .map(|i| {
            let description = format!("Skill number {i} {}", "does a very particular thing with great care ".repeat(6));
            (format!(".agents/skills/skill-{i:03}/SKILL.md"), format!("---\nname: skill-{i:03}\ndescription: {description}\n---\nbody\n"))
        })
        .collect();
    project.push((
        ".agents/agents/small.md".into(),
        "---\nschema: aim.agent/v1\nname: small\ndescription: Uses the small model.\nprovider: scripted\nmodel: small-model\n---\nBe brief.\n".into(),
    ));
    let files: Arc<dyn Files> = Arc::new(MemoryFiles::new(project));
    // The provider's default model has a large window; the agent's model a small one (REV8-15).
    let models = vec![model("m1", 1_000_000), model("small-model", 25_600)];
    let f = fixture_on(Arc::new(MemoryStore::default()), models, vec![text("ok")], NativeServices::default(), move |spec| Connected {
        tools: Arc::new(FakeTools::default()) as Arc<dyn ToolHost>,
        root: spec.workspace.clone(),
        location: "local".into(),
        project: Some(Arc::clone(&files)),
        shutdown: Box::new(|| Box::pin(async {})),
    });
    turns(&f, spec(Some("small")), &["hi"]).await;
    let seen = f.provider.seen.lock().unwrap().clone();
    assert_eq!(seen[0].model, "small-model");
    let budget = instructions::skill_budget(Some(25_600));
    let listing: usize =
        seen[0].instructions.lines().filter(|l| l.starts_with("- skill-") || l.starts_with("- …and")).map(|l| l.len() + 1).sum();
    assert!(listing <= budget, "the catalog ({listing} bytes) must fit the agent model's budget ({budget})");
}

// ------------------------------------------------------------------------------------------------
// live smoke (ADR 0022): `mise exec -- cargo test -p aim --test resources -- --ignored live_ --nocapture`
// ------------------------------------------------------------------------------------------------

/// A temporary repository with the haiku skill and a Read-only agent.
fn live_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".agents/skills/haiku/SKILL.md", HAIKU);
    write(
        dir.path(),
        ".agents/agents/reader.md",
        "---\nschema: aim.agent/v1\nname: reader\ndescription: Reads files and reports; never changes anything.\ntools: [Read]\n---\nYou can only read files. If asked to change anything, say you cannot.\n",
    );
    write(dir.path(), "notes.txt", "Pelican is the first word of this file.\n");
    dir
}

#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and a built aimx"]
async fn live_skill_mention_via_aim_run_openrouter() {
    let aimx = aimx().expect("aimx next to the aim binary");
    let repo = live_repo();
    let home = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aim"))
        .args(["run", "-p", "openrouter", "--ephemeral", "--json", "--aimx"])
        .arg(&aimx)
        .arg("-C")
        .arg(repo.path())
        .arg("$haiku Describe the sea.")
        .env("AIM_HOME", home.path())
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(output.status.success(), "aim run failed: {}", String::from_utf8_lossy(&output.stderr));
    let updates: Vec<SessionUpdate> =
        String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    // The recorded user turn carries the skill; the answer follows it.
    let user_parts: Vec<String> = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::ItemAdded { item: Item::User { parts } } => {
                Some(texts(parts).into_iter().map(str::to_owned).collect::<Vec<_>>())
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert!(user_parts.iter().any(|p| p.starts_with("<skill name=\"haiku\" path=\".agents/skills/haiku/SKILL.md\">")), "{user_parts:?}");
    let answer: String = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    let usage: Vec<&Usage> = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::Usage { usage } => Some(usage),
            _ => None,
        })
        .collect();
    eprintln!("live_aim_run answer:\n{answer}\nusage: {usage:?}\nlive_aim_run_ms={}", elapsed.as_millis());
    let lines: Vec<&str> = answer.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "a haiku has three lines: {lines:?}");
    // The same repository's prefix, as the session built it.
    let prefix = live_prefix(&aimx, repo.path(), None).await;
    eprintln!("live_instruction_prefix_bytes={}", prefix.len());
}

#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and a built aimx"]
async fn live_read_only_agent_on_openrouter() {
    let aimx = aimx().expect("aimx next to the aim binary");
    let repo = live_repo();
    let home = tempfile::tempdir().unwrap();
    let host = SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: native_backends_with(
            Arc::new(aim::providers::build),
            aim::host::aimx_workspaces(aimx.clone()),
            8,
            ResourceConfig::user(home.path()),
            NativeServices::default(),
        ),
        update_capacity: 4096,
    });
    let mut session = spec(Some("reader"));
    session.workspace = repo.path().canonicalize().unwrap().to_string_lossy().into_owned();
    session.provider = "openrouter".into();
    let id = host.create(session).await.unwrap().meta.id;
    let (_, mut updates) = host.attach(id.clone()).await.unwrap();
    let started = std::time::Instant::now();
    let ask = "Read notes.txt and tell me its first word. Then create a file named out.txt containing the word.";
    host.prompt(id.clone(), vec![Part::Text { text: ask.into() }]).await.unwrap();
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(240), updates.next()).await.unwrap().unwrap();
        let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        got.push(update);
        if idle && got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { .. } | SessionUpdate::TurnFailed { .. })) {
            break;
        }
    }
    let elapsed = started.elapsed();
    let tools: Vec<&str> = got
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::ToolStarted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let answer: String = got
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    let refused = got.iter().filter(|u| matches!(u, SessionUpdate::ToolFinished { result, .. } if result.is_error)).count();
    eprintln!("live_agent tools={tools:?} refused={refused} ms={}\nanswer: {answer}", elapsed.as_millis());
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { .. })), "{got:?}");
    assert!(tools.contains(&"Read"), "the agent read the file");
    assert!(answer.to_lowercase().contains("pelican"), "{answer}");
    assert!(!repo.path().join("out.txt").exists(), "a Read-only agent created a file");
    let prefix = live_prefix(&aimx, repo.path(), Some("reader")).await;
    eprintln!("live_agent_instruction_prefix_bytes={}", prefix.len());
    host.close(id).await.unwrap();
    host.shutdown().await.unwrap();
}

/// The instruction prefix a session in `repo` gets (4 KiB skill budget), read through a real aimx.
async fn live_prefix(aimx: &Path, repo: &Path, agent: Option<&str>) -> String {
    let root = repo.canonicalize().unwrap();
    let harness = aim::harness::HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root.to_string_lossy()).await.unwrap();
    let files = HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone());
    let catalog = resources::discover(&ResourceConfig::default(), Some(&files), "local", "").await;
    harness.shutdown().await;
    let agent = agent.and_then(|name| catalog.agent(name));
    aim::context::instructions(&catalog, agent, instructions::DEFAULT_SKILL_BUDGET).text
}
