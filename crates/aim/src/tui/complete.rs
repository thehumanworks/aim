//! The completion broker (docs/adr/0015): one async path for `@` files and directories, `/`
//! commands (with argument completion) and `$` skills.
//!
//! Every keystroke bumps the app's input generation. The broker cancels the request of an older
//! generation when a newer one arrives, and the app drops any result whose generation is not the
//! current one (fencing), so a slow answer can never overwrite a newer popup.
//!
//! Sources sit behind [`Source`], so a harness-backed file source (for SSH workspaces) and, later,
//! cloud buckets plug in without touching the app. The local file source walks the workspace with
//! `ignore` (gitignore-aware, bounded) and ranks with `nucleo`'s matcher.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::FutureExt as _;
use futures_util::future::Shared;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::commands::COMMANDS;
use crate::host::BoxFuture;

/// Most candidates a source returns.
pub const MAX_CANDIDATES: usize = 50;
/// Most workspace entries the file index holds.
pub const MAX_INDEXED: usize = 50_000;
/// Deepest directory level the file index walks.
pub const MAX_DEPTH: usize = 16;
/// How long a file index (or skill list) stays fresh.
pub const INDEX_TTL: Duration = Duration::from_secs(10);
/// Most bytes of a `SKILL.md` read for its front matter.
pub const FRONT_MATTER_BYTES: u64 = 8 * 1024;
/// Most skills read from one directory.
pub const MAX_SKILLS: usize = 256;
/// Paths ranked between two yields: a cancelled request stops within one chunk.
pub const RANK_CHUNK: usize = 512;

/// What is being completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `@path`: files and directories.
    File,
    /// `/name` at the start of the draft.
    Command,
    /// The argument of `/name arg`.
    Argument {
        /// The command.
        command: String,
    },
    /// `$name`: skills.
    Skill,
}

/// The token under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// What kind of completion.
    pub trigger: Trigger,
    /// The text typed so far (without the sigil).
    pub query: String,
    /// Byte offset where the replaced token starts (at the sigil).
    pub start: usize,
    /// Byte offset where it ends (the cursor).
    pub end: usize,
}

/// Finds the completion context at `cursor` in `text`, if any.
pub fn context(text: &str, cursor: usize) -> Option<Context> {
    let head = text.get(..cursor)?;
    if let Some(rest) = head.strip_prefix('/')
        && !head.contains('\n')
    {
        return Some(match rest.split_once(' ') {
            None => Context { trigger: Trigger::Command, query: rest.to_owned(), start: 0, end: cursor },
            Some((command, arg)) => {
                if arg.contains(' ') {
                    return None;
                }
                Context {
                    trigger: Trigger::Argument { command: command.to_owned() },
                    query: arg.to_owned(),
                    start: cursor - arg.len(),
                    end: cursor,
                }
            }
        });
    }
    let start = match head.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
        Some((at, space)) => at + space.len_utf8(),
        None => 0,
    };
    let token = head.get(start..)?;
    let trigger = match token.chars().next()? {
        '@' => Trigger::File,
        '$' => Trigger::Skill,
        _ => return None,
    };
    Some(Context { trigger, query: token.get(1..)?.to_owned(), start, end: cursor })
}

/// What a candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A file.
    File,
    /// A directory.
    Dir,
    /// A slash command.
    Command,
    /// A command argument.
    Argument,
    /// A skill.
    Skill,
}

/// One completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What the popup shows.
    pub label: String,
    /// What replaces the token (sigil included).
    pub insert: String,
    /// A short description.
    pub detail: String,
    /// What it is.
    pub kind: Kind,
}

/// A value the app knows for a command argument, and what the popup says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    /// The value (what is inserted).
    pub value: String,
    /// A short description (empty for none).
    pub detail: String,
}

/// A completion request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The app's input generation when it was made.
    pub generation: u64,
    /// The token.
    pub context: Context,
    /// Configured command names for a command trigger; for arguments, the session's models and
    /// efforts (ADR 0074), providers, or values seen so far.
    pub hints: Vec<Hint>,
}

/// Produces candidates for one kind of token.
pub trait Source: Send + Sync {
    /// Candidates for `request`, best first. The future is dropped when a newer request arrives.
    fn complete(&self, request: &Request) -> BoxFuture<Vec<Candidate>>;
}

/// The broker's sources, by trigger.
#[derive(Clone)]
pub struct Sources {
    /// `@` files and directories.
    pub files: Arc<dyn Source>,
    /// `$` skills.
    pub skills: Arc<dyn Source>,
    /// `/` commands and their arguments.
    pub commands: Arc<dyn Source>,
}

impl Sources {
    /// Local sources for a workspace on this machine; `user_skills` is `~/.aim/skills`.
    pub fn local(root: &Path, user_skills: Option<PathBuf>) -> Self {
        let mut skill_dirs = vec![root.join(".agents").join("skills")];
        skill_dirs.extend(user_skills);
        Self {
            files: Arc::new(LocalFiles::new(root.to_path_buf())),
            skills: Arc::new(LocalSkills::new(skill_dirs)),
            commands: Arc::new(CommandSource),
        }
    }

    /// Sources for a workspace on another machine: never this machine's files or project skills
    /// (a harness-backed source replaces them later); user skills and commands still apply.
    pub fn remote(user_skills: Option<PathBuf>) -> Self {
        Self {
            files: Arc::new(NoFiles),
            skills: Arc::new(LocalSkills::new(user_skills.into_iter().collect())),
            commands: Arc::new(CommandSource),
        }
    }

    /// The default factory: local sources for local workspaces, [`Sources::remote`] otherwise.
    pub fn factory(user_skills: Option<PathBuf>) -> SourceFactory {
        Arc::new(
            move |root: &Path, local: bool| {
                if local { Self::local(root, user_skills.clone()) } else { Self::remote(user_skills.clone()) }
            },
        )
    }
}

/// Builds the sources for a workspace (its root, and whether it is on this machine). The TUI calls
/// it for the launch directory and again whenever it attaches a session in another workspace.
pub type SourceFactory = Arc<dyn Fn(&Path, bool) -> Sources + Send + Sync>;

/// No file completion (a remote workspace, until a harness-backed source exists).
pub struct NoFiles;

impl Source for NoFiles {
    fn complete(&self, _request: &Request) -> BoxFuture<Vec<Candidate>> {
        Box::pin(async { Vec::new() })
    }
}

/// A build in flight or done: when it finished, and what it made.
type Build<T> = Shared<BoxFuture<Option<(Instant, Arc<T>)>>>;

/// A value built by one blocking task and shared: a build in progress is always reused, and a
/// finished one stays fresh for a TTL counted from when it *finished*. Concurrent, cancelled or
/// late requests never start a second build (the work runs once, whoever still waits for it).
pub struct SingleFlight<T> {
    state: Mutex<Option<Build<T>>>,
    builds: AtomicUsize,
}

impl<T: Send + Sync + 'static> SingleFlight<T> {
    /// Nothing built yet.
    pub fn new() -> Self {
        Self { state: Mutex::new(None), builds: AtomicUsize::new(0) }
    }

    /// The value in progress or finished less than `ttl` ago, or a new build of it.
    pub fn get(&self, ttl: Duration, build: impl FnOnce() -> T + Send + 'static) -> BoxFuture<Option<Arc<T>>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let reusable = state.as_ref().filter(|shared| match shared.peek() {
            None => true,
            Some(Some((done, _))) => done.elapsed() < ttl,
            Some(None) => false,
        });
        let shared = if let Some(shared) = reusable {
            shared.clone()
        } else {
            self.builds.fetch_add(1, Ordering::Relaxed);
            let task = tokio::task::spawn_blocking(move || {
                let value = Arc::new(build());
                (Instant::now(), value)
            });
            let future: BoxFuture<Option<(Instant, Arc<T>)>> = Box::pin(async move { task.await.ok() });
            let shared = future.shared();
            *state = Some(shared.clone());
            shared
        };
        Box::pin(async move { shared.await.map(|(_, value)| value) })
    }

    /// How many builds have started.
    #[cfg(test)]
    pub fn builds(&self) -> usize {
        self.builds.load(Ordering::Relaxed)
    }
}

impl<T: Send + Sync + 'static> Default for SingleFlight<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A finished completion, tagged with its generation.
#[derive(Clone, Debug)]
pub struct Completed {
    /// The generation it answers.
    pub generation: u64,
    /// The candidates.
    pub candidates: Vec<Candidate>,
}

/// Runs completion requests, cancelling superseded ones.
pub struct Broker {
    sources: Sources,
    current: Option<CancellationToken>,
    results: UnboundedSender<Completed>,
    cancel_superseded: bool,
}

impl Broker {
    /// A broker that sends results to `results`.
    pub fn new(sources: Sources, results: UnboundedSender<Completed>) -> Self {
        Self { sources, current: None, results, cancel_superseded: true }
    }

    /// Keeps superseded requests running (tests use it to prove the app's fence on its own).
    pub fn keep_superseded(&mut self) {
        self.cancel_superseded = false;
    }

    /// Uses `sources` from now on (a session in another workspace was attached).
    pub fn set_sources(&mut self, sources: Sources) {
        self.cancel();
        self.sources = sources;
    }

    /// Starts `request`, cancelling the previous one.
    pub fn request(&mut self, request: &Request) {
        self.cancel();
        let source = match request.context.trigger {
            Trigger::File => Arc::clone(&self.sources.files),
            Trigger::Skill => Arc::clone(&self.sources.skills),
            Trigger::Command | Trigger::Argument { .. } => Arc::clone(&self.sources.commands),
        };
        let token = CancellationToken::new();
        let results = self.results.clone();
        let cancelled = token.clone();
        let generation = request.generation;
        let work = source.complete(request);
        tokio::spawn(async move {
            tokio::select! {
                () = cancelled.cancelled() => {}
                candidates = work => {
                    // The app is gone when this fails; nothing to do.
                    let _gone = results.send(Completed { generation, candidates });
                }
            }
        });
        if self.cancel_superseded {
            self.current = Some(token);
        }
    }

    /// Cancels the request in flight.
    pub fn cancel(&mut self) {
        if let Some(token) = self.current.take() {
            token.cancel();
        }
    }
}

fn rank(query: &str, items: Vec<String>, paths: bool) -> Vec<(String, u32)> {
    let mut config = Config::DEFAULT;
    if paths {
        config.set_match_paths();
    }
    let mut matcher = Matcher::new(config);
    Pattern::parse(query, CaseMatching::Smart, Normalization::Smart).match_list(items, &mut matcher)
}

/// `/` commands, and argument values from the app's hints.
pub struct CommandSource;

impl Source for CommandSource {
    fn complete(&self, request: &Request) -> BoxFuture<Vec<Candidate>> {
        let request = request.clone();
        Box::pin(async move {
            match &request.context.trigger {
                Trigger::Command => {
                    let names: Vec<String> =
                        COMMANDS.iter().map(|c| c.name.to_owned()).chain(request.hints.iter().map(|h| h.value.clone())).collect();
                    let mut ranked = rank(&request.context.query, names, false);
                    // Prefix matches first, in table order; then fuzzy matches by score.
                    ranked.sort_by_key(|(name, _)| !name.starts_with(&request.context.query));
                    ranked
                        .into_iter()
                        .take(MAX_CANDIDATES)
                        .filter_map(|(name, _)| {
                            if let Some(hint) = request.hints.iter().find(|h| h.value == name) {
                                return Some(Candidate {
                                    label: format!("/{name}"),
                                    insert: format!("/{name} "),
                                    detail: hint.detail.clone(),
                                    kind: Kind::Command,
                                });
                            }
                            COMMANDS.iter().find(|c| c.name == name).map(|c| Candidate {
                                label: if c.args.is_empty() { format!("/{}", c.name) } else { format!("/{} {}", c.name, c.args) },
                                insert: if c.args.is_empty() { format!("/{}", c.name) } else { format!("/{} ", c.name) },
                                detail: c.help.to_owned(),
                                kind: Kind::Command,
                            })
                        })
                        .collect()
                }
                Trigger::Argument { .. } => {
                    let mut hints: Vec<&Hint> = Vec::new();
                    for hint in &request.hints {
                        if !hints.iter().any(|h| h.value == hint.value) {
                            hints.push(hint);
                        }
                    }
                    let query = request.context.query.as_str();
                    let mut ranked = rank(query, hints.iter().map(|h| h.value.clone()).collect(), false);
                    // Prefix matches first; otherwise the matcher's order (the source's, for no query).
                    ranked.sort_by_key(|(value, _)| !value.starts_with(query));
                    ranked
                        .into_iter()
                        .take(MAX_CANDIDATES)
                        .filter_map(|(value, _)| hints.iter().find(|h| h.value == value))
                        .map(|hint| Candidate {
                            label: hint.value.clone(),
                            insert: hint.value.clone(),
                            detail: hint.detail.clone(),
                            kind: Kind::Argument,
                        })
                        .collect()
                }
                Trigger::File | Trigger::Skill => Vec::new(),
            }
        })
    }
}

/// A workspace entry in the file index: its relative path (`/` after directories).
type Index = Vec<String>;

/// Files and directories of a local workspace: an `ignore` walk (gitignore-aware, bounded, built
/// once per [`INDEX_TTL`] however many requests ask) ranked by `nucleo`.
pub struct LocalFiles {
    root: PathBuf,
    index: Arc<SingleFlight<Index>>,
    /// Paths ranked so far (all requests).
    scored: Arc<AtomicUsize>,
}

impl LocalFiles {
    /// Files under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self { root, index: Arc::new(SingleFlight::new()), scored: Arc::new(AtomicUsize::new(0)) }
    }

    /// How many index builds have started.
    #[cfg(test)]
    pub fn builds(&self) -> usize {
        self.index.builds()
    }

    /// How many paths have been ranked.
    #[cfg(test)]
    pub fn scored(&self) -> usize {
        self.scored.load(Ordering::Relaxed)
    }
}

/// Walks `root` (gitignore rules, `.git` skipped, bounded by count and depth).
fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(false).max_depth(Some(MAX_DEPTH)).filter_entry(|e| e.file_name() != ".git").build();
    for entry in walker.flatten() {
        if out.len() >= MAX_INDEXED {
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else { continue };
        let Some(text) = relative.to_str() else { continue };
        if text.is_empty() {
            continue;
        }
        let dir = entry.file_type().is_some_and(|t| t.is_dir());
        out.push(if dir { format!("{text}/") } else { text.to_owned() });
    }
    out
}

/// The top-level entries, directories first (an empty `@` query).
fn shallow(index: &[String]) -> Vec<String> {
    let mut shallow: Vec<&String> = index.iter().filter(|p| p.trim_end_matches('/').matches('/').count() == 0).collect();
    shallow.sort_by_key(|p| (!p.ends_with('/'), p.to_ascii_lowercase()));
    shallow.into_iter().take(MAX_CANDIDATES).cloned().collect()
}

/// The best paths for `query`, ranked over references in chunks that yield between them: the
/// work lives in the request's future, so a cancelled request stops ranking within one chunk and
/// no copy of the index is made.
async fn ranked(index: &[String], query: &str, scored: &AtomicUsize) -> Vec<String> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut hits: Vec<(u32, usize)> = Vec::new();
    for (n, chunk) in index.chunks(RANK_CHUNK).enumerate() {
        if n > 0 {
            tokio::task::yield_now().await;
        }
        let mut config = Config::DEFAULT;
        config.set_match_paths();
        let mut matcher = Matcher::new(config);
        let mut buf = Vec::new();
        for (offset, path) in chunk.iter().enumerate() {
            if let Some(score) = pattern.score(nucleo_matcher::Utf32Str::new(path, &mut buf), &mut matcher) {
                hits.push((score, n * RANK_CHUNK + offset));
            }
        }
        scored.fetch_add(chunk.len(), Ordering::Relaxed);
    }
    hits.sort_by_key(|(score, at)| (core::cmp::Reverse(*score), *at));
    hits.into_iter().take(MAX_CANDIDATES).filter_map(|(_, at)| index.get(at).cloned()).collect()
}

fn file_candidates(chosen: Vec<String>) -> Vec<Candidate> {
    chosen
        .into_iter()
        .map(|path| {
            let dir = path.ends_with('/');
            Candidate {
                insert: if dir { format!("@{path}") } else { format!("@{path} ") },
                label: path,
                detail: String::new(),
                kind: if dir { Kind::Dir } else { Kind::File },
            }
        })
        .collect()
}

impl Source for LocalFiles {
    fn complete(&self, request: &Request) -> BoxFuture<Vec<Candidate>> {
        let root = self.root.clone();
        let index = self.index.get(INDEX_TTL, move || walk(&root));
        let query = request.context.query.clone();
        let scored = Arc::clone(&self.scored);
        Box::pin(async move {
            let Some(index) = index.await else { return Vec::new() };
            let chosen = if query.is_empty() { shallow(&index) } else { ranked(&index, &query, &scored).await };
            file_candidates(chosen)
        })
    }
}

/// A skill found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Its name (front matter `name`, else the directory name).
    pub name: String,
    /// Its description (front matter `description`).
    pub description: String,
}

/// Reads `name` and `description` from a `SKILL.md` front matter block.
pub fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').to_owned();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    (name, description)
}

/// Skills under the given directories (`<dir>/<skill>/SKILL.md`, ADR 0014): at most
/// [`MAX_SKILLS`] per directory, [`FRONT_MATTER_BYTES`] of each file, scanned once per
/// [`INDEX_TTL`] however many requests ask.
pub struct LocalSkills {
    dirs: Vec<PathBuf>,
    list: Arc<SingleFlight<Vec<Skill>>>,
}

impl LocalSkills {
    /// Skills in `dirs`.
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs, list: Arc::new(SingleFlight::new()) }
    }

    /// How many scans have started.
    #[cfg(test)]
    pub fn builds(&self) -> usize {
        self.list.builds()
    }
}

/// The first `limit` bytes of a file as text (a skill's front matter is at its top).
fn read_prefix(path: &Path, limit: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn scan_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten().take(MAX_SKILLS) {
            let Some(text) = read_prefix(&entry.path().join("SKILL.md"), FRONT_MATTER_BYTES) else { continue };
            let (name, description) = front_matter(&text);
            let name = name.unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
            if !skills.iter().any(|s: &Skill| s.name == name) {
                skills.push(Skill { name, description: description.unwrap_or_default() });
            }
        }
    }
    skills
}

impl Source for LocalSkills {
    fn complete(&self, request: &Request) -> BoxFuture<Vec<Candidate>> {
        let dirs = self.dirs.clone();
        let list = self.list.get(INDEX_TTL, move || scan_skills(&dirs));
        let query = request.context.query.clone();
        Box::pin(async move {
            let Some(skills) = list.await else { return Vec::new() };
            let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
            rank(&query, names, false)
                .into_iter()
                .take(MAX_CANDIDATES)
                .filter_map(|(name, _)| skills.iter().find(|s| s.name == name))
                .map(|s| Candidate {
                    label: format!("${}", s.name),
                    insert: format!("${} ", s.name),
                    detail: super::text::truncate(&s.description, 200),
                    kind: Kind::Skill,
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_are_found_under_the_cursor() {
        let c = context("look at @src/ma", 15).unwrap();
        assert_eq!((c.trigger, c.query.as_str(), c.start), (Trigger::File, "src/ma", 8));
        assert_eq!(context("/mo", 3).map(|c| c.trigger), Some(Trigger::Command));
        let arg = context("/model gp", 9).unwrap();
        assert_eq!(arg.trigger, Trigger::Argument { command: "model".into() });
        assert_eq!((arg.query.as_str(), arg.start), ("gp", 7));
        assert_eq!(context("use $rev", 8).map(|c| c.trigger), Some(Trigger::Skill));
        assert_eq!(context("plain words", 11), None);
        assert_eq!(context("x /model", 8), None, "commands only at the start");
        assert_eq!(context("é @x", 5).map(|c| c.start), Some(3));
    }

    #[test]
    fn front_matter_is_read() {
        let text = "---\nname: review\ndescription: \"Review a diff\"\n---\nbody";
        assert_eq!(front_matter(text), (Some("review".into()), Some("Review a diff".into())));
        assert_eq!(front_matter("no front matter"), (None, None));
    }

    #[test]
    fn empty_file_queries_list_the_top_level_directories_first() {
        let index = vec!["b.txt".to_owned(), "src/".to_owned(), "src/main.rs".to_owned(), "a/".to_owned()];
        let labels: Vec<String> = file_candidates(shallow(&index)).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, ["a/", "src/", "b.txt"]);
        let ranked = file_candidates(futures_util::FutureExt::now_or_never(ranked(&index, "main", &AtomicUsize::new(0))).unwrap());
        assert_eq!(ranked.first().map(|c| c.insert.as_str()), Some("@src/main.rs "));
    }

    #[tokio::test]
    async fn commands_rank_prefixes_first_and_arguments_come_from_hints() {
        let request = |trigger, query: &str| Request {
            generation: 1,
            context: Context { trigger, query: query.into(), start: 0, end: 0 },
            hints: ["gpt-6-sol", "gpt-6-mini", "gpt-6-sol", "o-gpt"]
                .into_iter()
                .map(|value| Hint { value: value.into(), detail: format!("about {value}") })
                .collect(),
        };
        let got = CommandSource.complete(&request(Trigger::Command, "se")).await;
        assert_eq!(got.first().map(|c| c.insert.as_str()), Some("/sessions"));
        let got = CommandSource.complete(&request(Trigger::Argument { command: "model".into() }, "mini")).await;
        assert_eq!(got.first().map(|c| (c.label.as_str(), c.detail.as_str())), Some(("gpt-6-mini", "about gpt-6-mini")));
        let all = CommandSource.complete(&request(Trigger::Argument { command: "model".into() }, "")).await;
        let labels: Vec<&str> = all.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["gpt-6-sol", "gpt-6-mini", "o-gpt"], "no query keeps the source's order, each value once");
        let got = CommandSource.complete(&request(Trigger::Argument { command: "model".into() }, "gpt")).await;
        assert_eq!(got.last().map(|c| c.label.as_str()), Some("o-gpt"), "prefix matches come first");
    }

    #[tokio::test]
    async fn the_broker_cancels_superseded_requests() {
        struct Slow;
        impl Source for Slow {
            fn complete(&self, request: &Request) -> BoxFuture<Vec<Candidate>> {
                let slow = request.context.query == "a";
                let label = request.context.query.clone();
                Box::pin(async move {
                    if slow {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    vec![Candidate { label, insert: String::new(), detail: String::new(), kind: Kind::File }]
                })
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let slow: Arc<dyn Source> = Arc::new(Slow);
        let sources = Sources { files: Arc::clone(&slow), skills: Arc::clone(&slow), commands: slow };
        let mut broker = Broker::new(sources, tx);
        let file = |q: &str, generation| Request {
            generation,
            context: Context { trigger: Trigger::File, query: q.into(), start: 0, end: 0 },
            hints: Vec::new(),
        };
        broker.request(&file("a", 1));
        broker.request(&file("ab", 2));
        let first = rx.recv().await.unwrap();
        assert_eq!(first.generation, 2);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(rx.try_recv().is_err(), "the superseded request never answers");
    }

    /// REV10 #10: skill scans read a bounded prefix of each file and a bounded number of skills.
    #[test]
    fn rev10_skill_scans_are_bounded() {
        let dir = std::env::temp_dir().join(format!("aim-skills-{}", uuid::Uuid::new_v4().simple()));
        let big = dir.join("big");
        std::fs::create_dir_all(&big).unwrap();
        let mut text = String::from("---\nname: big\ndescription: Large body\n---\n");
        text.push_str(&"x".repeat(4 * 1024 * 1024));
        std::fs::write(big.join("SKILL.md"), &text).unwrap();
        let prefix = read_prefix(&big.join("SKILL.md"), FRONT_MATTER_BYTES).unwrap();
        assert!(prefix.len() <= usize::try_from(FRONT_MATTER_BYTES).unwrap());
        for n in 0..(MAX_SKILLS + 20) {
            let skill = dir.join(format!("s{n:04}"));
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), format!("---\nname: s{n}\n---\n")).unwrap();
        }
        let skills = scan_skills(std::slice::from_ref(&dir));
        assert!(skills.len() <= MAX_SKILLS, "{} skills", skills.len());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// REV10 #10: concurrent cold requests (and cancelled ones) share one index build.
    #[tokio::test(flavor = "multi_thread")]
    async fn rev10_cold_requests_share_one_build() {
        let dir = std::env::temp_dir().join(format!("aim-files-{}", uuid::Uuid::new_v4().simple()));
        for n in 0..200 {
            let sub = dir.join(format!("d{}", n % 10));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("f{n}.rs")), "").unwrap();
        }
        let files = LocalFiles::new(dir.clone());
        let request = |q: &str| Request {
            generation: 1,
            context: Context { trigger: Trigger::File, query: q.into(), start: 0, end: 0 },
            hints: Vec::new(),
        };
        // Start and drop some (cancelled by the broker), then run many at once.
        for q in ["a", "b", "c"] {
            drop(files.complete(&request(q)));
        }
        let all = futures_util::future::join_all((0..16).map(|n| files.complete(&request(&format!("f{n}"))))).await;
        assert!(all.iter().all(|c| !c.is_empty()));
        assert_eq!(files.builds(), 1, "one walk served every request");
        let skills = LocalSkills::new(vec![dir.clone()]);
        let skill_request = Request { context: Context { trigger: Trigger::Skill, ..request("x").context }, ..request("x") };
        futures_util::future::join_all((0..16).map(|_| skills.complete(&skill_request))).await;
        assert_eq!(skills.builds(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// REV12 (residual of REV10 #10): a build that outlives the TTL is still the one shared.
    #[tokio::test(flavor = "multi_thread")]
    async fn rev12_a_build_longer_than_the_ttl_is_not_duplicated() {
        let flight: SingleFlight<u32> = SingleFlight::new();
        let ttl = Duration::from_millis(50);
        let first = flight.get(ttl, || {
            std::thread::sleep(Duration::from_millis(250));
            1
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        let second = flight.get(ttl, || 2);
        assert_eq!(flight.builds(), 1, "the running build is reused past the TTL");
        assert_eq!(second.await.map(|v| *v), Some(1));
        assert_eq!(first.await.map(|v| *v), Some(1));
    }

    /// REV12 (residual of REV10 #10): a cancelled `@` request stops ranking.
    #[tokio::test(flavor = "multi_thread")]
    async fn rev12_a_cancelled_request_stops_ranking() {
        let dir = std::env::temp_dir().join(format!("aim-rank-{}", uuid::Uuid::new_v4().simple()));
        for d in 0..60 {
            let sub = dir.join(format!("d{d}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..100 {
                std::fs::write(sub.join(format!("f{f}.rs")), "").unwrap();
            }
        }
        let files = LocalFiles::new(dir.clone());
        let request = |q: &str| Request {
            generation: 1,
            context: Context { trigger: Trigger::File, query: q.into(), start: 0, end: 0 },
            hints: Vec::new(),
        };
        assert!(!files.complete(&request("f1")).await.is_empty());
        let total = files.scored();
        let mut cancelled = files.complete(&request("f2"));
        let _pending = futures_util::poll!(&mut cancelled);
        drop(cancelled);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let ranked = files.scored() - total;
        assert!(ranked < total, "the cancelled request stopped ranking ({ranked} of {total})");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
