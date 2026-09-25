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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

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
/// How long a file index stays fresh.
pub const INDEX_TTL: Duration = Duration::from_secs(10);

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

/// A completion request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The app's input generation when it was made.
    pub generation: u64,
    /// The token.
    pub context: Context,
    /// Values the app knows for command arguments (models and efforts seen, …).
    pub hints: Vec<String>,
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
                    let names: Vec<String> = COMMANDS.iter().map(|c| c.name.to_owned()).collect();
                    let mut ranked = rank(&request.context.query, names, false);
                    // Prefix matches first, in table order; then fuzzy matches by score.
                    ranked.sort_by_key(|(name, _)| !name.starts_with(&request.context.query));
                    ranked
                        .into_iter()
                        .filter_map(|(name, _)| COMMANDS.iter().find(|c| c.name == name))
                        .map(|c| Candidate {
                            label: if c.args.is_empty() { format!("/{}", c.name) } else { format!("/{} {}", c.name, c.args) },
                            insert: if c.args.is_empty() { format!("/{}", c.name) } else { format!("/{} ", c.name) },
                            detail: c.help.to_owned(),
                            kind: Kind::Command,
                        })
                        .collect()
                }
                Trigger::Argument { .. } => {
                    let mut hints = request.hints.clone();
                    hints.dedup();
                    rank(&request.context.query, hints, false)
                        .into_iter()
                        .take(MAX_CANDIDATES)
                        .map(|(value, _)| Candidate { label: value.clone(), insert: value, detail: String::new(), kind: Kind::Argument })
                        .collect()
                }
                Trigger::File | Trigger::Skill => Vec::new(),
            }
        })
    }
}

/// A workspace entry in the file index: its relative path (`/` after directories).
type Index = Arc<Vec<String>>;

/// Files and directories of a local workspace: an `ignore` walk (gitignore-aware, bounded,
/// cached for [`INDEX_TTL`]) ranked by `nucleo`.
pub struct LocalFiles {
    root: PathBuf,
    cache: Arc<Mutex<Option<(Instant, Index)>>>,
}

impl LocalFiles {
    /// Files under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self { root, cache: Arc::new(Mutex::new(None)) }
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

fn file_candidates(index: &[String], query: &str) -> Vec<Candidate> {
    let chosen: Vec<String> = if query.is_empty() {
        let mut shallow: Vec<&String> = index.iter().filter(|p| p.trim_end_matches('/').matches('/').count() == 0).collect();
        shallow.sort_by_key(|p| (!p.ends_with('/'), p.to_ascii_lowercase()));
        shallow.into_iter().take(MAX_CANDIDATES).cloned().collect()
    } else {
        rank(query, index.to_vec(), true).into_iter().take(MAX_CANDIDATES).map(|(p, _)| p).collect()
    };
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
        let cache = Arc::clone(&self.cache);
        let query = request.context.query.clone();
        Box::pin(async move {
            let fresh = cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .filter(|(at, _)| at.elapsed() < INDEX_TTL)
                .map(|(_, index)| Arc::clone(index));
            tokio::task::spawn_blocking(move || {
                let index = fresh.unwrap_or_else(|| {
                    let index: Index = Arc::new(walk(&root));
                    *cache.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), Arc::clone(&index)));
                    index
                });
                file_candidates(&index, &query)
            })
            .await
            .unwrap_or_default()
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

/// Skills under the given directories (`<dir>/<skill>/SKILL.md`, ADR 0014).
pub struct LocalSkills {
    dirs: Vec<PathBuf>,
}

impl LocalSkills {
    /// Skills in `dirs`.
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }
}

fn scan_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path().join("SKILL.md")) else { continue };
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
        let query = request.context.query.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let skills = scan_skills(&dirs);
                let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
                rank(&query, names, false)
                    .into_iter()
                    .take(MAX_CANDIDATES)
                    .filter_map(|(name, _)| skills.iter().find(|s| s.name == name))
                    .map(|s| Candidate {
                        label: format!("${}", s.name),
                        insert: format!("${} ", s.name),
                        detail: s.description.clone(),
                        kind: Kind::Skill,
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
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
        let labels: Vec<String> = file_candidates(&index, "").into_iter().map(|c| c.label).collect();
        assert_eq!(labels, ["a/", "src/", "b.txt"]);
        let ranked = file_candidates(&index, "main");
        assert_eq!(ranked.first().map(|c| c.insert.as_str()), Some("@src/main.rs "));
    }

    #[tokio::test]
    async fn commands_rank_prefixes_first_and_arguments_come_from_hints() {
        let request = |trigger, query: &str| Request {
            generation: 1,
            context: Context { trigger, query: query.into(), start: 0, end: 0 },
            hints: vec!["gpt-6-sol".into(), "gpt-6-mini".into()],
        };
        let got = CommandSource.complete(&request(Trigger::Command, "se")).await;
        assert_eq!(got.first().map(|c| c.insert.as_str()), Some("/sessions"));
        let got = CommandSource.complete(&request(Trigger::Argument { command: "model".into() }, "mini")).await;
        assert_eq!(got.first().map(|c| c.label.as_str()), Some("gpt-6-mini"));
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
}
