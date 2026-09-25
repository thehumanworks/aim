//! Content and name search with the ripgrep libraries.
//!
//! Walks honour `.gitignore`, `.ignore` and git's global excludes even outside a git repository
//! (`require_git(false)`), skip hidden entries (as `rg` does), never follow symlinks (so a walk
//! cannot leave the root), and visit paths in sorted order so results are deterministic.
//!
//! The walk lists directories by path, so a directory swapped for a symlink mid-walk could make it
//! *list* a directory outside the root. Nothing it lists is trusted: every file is opened, and
//! every glob hit checked, through [`Beneath`], by descriptors from the start directory the
//! resolution held open (REV4-A finding 3). An entry that no longer resolves inside is skipped.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path};
use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{CaseMode, GlobResult, GrepMatch, GrepResult};
use globset::{GlobBuilder, GlobSetBuilder};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use rustix::fs::{Mode, OFlags};

use super::walk::{Follow, Loc, open_dir, open_entry, stat_entry};
use super::{Authority, Base, blocking};
use crate::authz::Access;
use crate::workspace::{BoxFuture, GlobQuery, GrepQuery, Outcome, Search};

/// Longest line (in bytes) returned in a match or context line; longer lines are cut.
const MAX_LINE_BYTES: usize = 4096;
/// Text (paths and lines) collected per search before it is reported as truncated, so a result
/// always fits in one message (JSON escaping can grow text up to six-fold; 16 MiB messages).
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;

/// Opens entries below a directory held open, by descriptors (never by path).
struct Beneath<'a> {
    /// The start: a directory, or (a single-file search) the directory holding the file and its name.
    start: Start<'a>,
    /// The directories opened for the last lookup, from the start down (the walk is sorted, so
    /// consecutive lookups share most of it and each directory is opened about once).
    stack: Vec<(OsString, OwnedFd)>,
}

enum Start<'a> {
    Dir(&'a OwnedFd),
    File(&'a OwnedFd, &'a OsStr),
}

impl<'a> Beneath<'a> {
    fn new(loc: &'a Loc) -> Option<Self> {
        let start = match (loc.target_dir(), loc.dir(), &loc.name) {
            (Some(dir), _, _) => Start::Dir(dir),
            (None, Ok(dir), Some(name)) if loc.missing.is_empty() => Start::File(dir, name),
            _ => return None,
        };
        Some(Self { start, stack: Vec::new() })
    }

    /// The directory `rel` (relative to the start), opened component by component without
    /// following symlinks.
    fn dir(&mut self, rel: &Path) -> io::Result<&OwnedFd> {
        let Start::Dir(start) = self.start else {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "the start is a file"));
        };
        let mut names = Vec::new();
        for component in rel.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a plain relative path"));
            };
            names.push(name);
        }
        let common = self.stack.iter().zip(&names).take_while(|((held, _), wanted)| held.as_os_str() == **wanted).count();
        self.stack.truncate(common);
        for name in names.into_iter().skip(common) {
            let opened = open_dir(self.stack.last().map_or(start, |(_, fd)| fd), name)?;
            self.stack.push((name.to_owned(), opened));
        }
        Ok(self.stack.last().map_or(start, |(_, fd)| fd))
    }

    /// The directory holding `rel` and its name.
    fn parent<'p>(&mut self, rel: &'p Path) -> io::Result<(&OwnedFd, &'p OsStr)> {
        let name = rel.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
        let dir = self.dir(rel.parent().unwrap_or_else(|| Path::new("")))?;
        Ok((dir, name))
    }

    /// Opens the regular file `rel` (relative to the start; empty for a single-file start).
    fn open(&mut self, rel: &Path) -> io::Result<File> {
        let file = match self.start {
            Start::File(dir, name) if rel.as_os_str().is_empty() => open_entry(dir, name, OFlags::RDONLY, Mode::empty())?,
            _ => {
                let (dir, name) = self.parent(rel)?;
                open_entry(dir, name, OFlags::RDONLY, Mode::empty())?
            }
        };
        if file.metadata()?.is_file() { Ok(file) } else { Err(io::Error::other("not a regular file")) }
    }

    /// Whether entry `rel` exists (without following it).
    fn exists(&mut self, rel: &Path) -> bool {
        self.parent(rel).is_ok_and(|(dir, name)| stat_entry(dir, name).is_ok())
    }
}

/// Search over the local filesystem.
#[derive(Debug)]
pub(super) struct LocalSearch {
    base: Arc<Base>,
}

impl LocalSearch {
    pub(super) fn new(base: Arc<Base>) -> Self {
        Self { base }
    }
}

fn walker(base: &Path, hidden: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(base);
    builder.standard_filters(true).require_git(false).hidden(!hidden).follow_links(false).sort_by_file_path(Ord::cmp);
    builder
}

/// A line without its terminator, cut to [`MAX_LINE_BYTES`] on a character boundary.
fn line_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_LINE_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_LINE_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = text.get(..cut).unwrap_or_default().to_owned();
    out.push('…');
    out
}

struct Collector<'a> {
    path: &'a str,
    matches: &'a mut Vec<GrepMatch>,
    /// Matches found for this file (indexes into `matches` start here).
    first: usize,
    pending_before: Vec<String>,
    limit: usize,
    bytes: &'a mut usize,
    truncated: &'a mut bool,
}

impl Collector<'_> {
    /// Accounts for `text`; false once the byte budget is spent.
    fn charge(&mut self, text: &str) -> bool {
        *self.bytes = self.bytes.saturating_add(text.len());
        if *self.bytes > MAX_RESULT_BYTES {
            *self.truncated = true;
            return false;
        }
        true
    }
}

impl Sink for Collector<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.matches.len() >= self.limit {
            *self.truncated = true;
            return Ok(false);
        }
        let text = line_text(mat.bytes());
        let path = self.path;
        if !self.charge(path) || !self.charge(&text) {
            return Ok(false);
        }
        self.matches.push(GrepMatch {
            path: self.path.to_owned(),
            line: mat.line_number().unwrap_or(0),
            text,
            before: std::mem::take(&mut self.pending_before),
            after: Vec::new(),
        });
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, context: &SinkContext<'_>) -> Result<bool, io::Error> {
        let text = line_text(context.bytes());
        if !self.charge(&text) {
            return Ok(false);
        }
        match context.kind() {
            SinkContextKind::Before => self.pending_before.push(text),
            SinkContextKind::After | SinkContextKind::Other => {
                if self.matches.len() > self.first
                    && let Some(last) = self.matches.last_mut()
                {
                    last.after.push(text);
                }
            }
        }
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        self.pending_before.clear();
        Ok(true)
    }
}

/// An owned [`GrepQuery`], for the blocking pool.
struct GrepJob {
    pattern: String,
    path: String,
    globs: Vec<String>,
    case: CaseMode,
    fixed: bool,
    context: u32,
    max: u32,
}

fn grep(base: &Base, job: &GrepJob) -> Outcome<GrepResult> {
    let GrepJob { pattern, path, globs, case, fixed, context, max } = job;
    let (case, fixed, context, max) = (*case, *fixed, *context, *max);
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Read))?;
    let mut beneath = Beneath::new(&loc).ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("`{path}` does not exist")))?;
    let start = base.real(&loc);
    if matches!(beneath.start, Start::File(..)) && beneath.open(Path::new("")).is_err() {
        return Err(ProtoError::new(ErrorCode::NotFound, format!("`{path}` does not exist")));
    }
    let regex = RegexMatcherBuilder::new()
        .case_smart(case == CaseMode::Smart)
        .case_insensitive(case == CaseMode::Insensitive)
        .fixed_strings(fixed)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("invalid pattern: {err}")))?;
    let context = usize::try_from(context).unwrap_or(usize::MAX);
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut walk = walker(&start, false);
    if !globs.is_empty() {
        let mut overrides = OverrideBuilder::new(&start);
        for glob in globs {
            overrides.add(glob).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("invalid glob `{glob}`: {err}")))?;
        }
        walk.overrides(overrides.build().map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("invalid globs: {err}")))?);
    }
    let limit = usize::try_from(max).unwrap_or(usize::MAX);
    let mut matches = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    for entry in walk.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = base.relative(entry.path());
        let first = matches.len();
        let mut sink = Collector {
            path: &rel,
            matches: &mut matches,
            first,
            pending_before: Vec::new(),
            limit,
            bytes: &mut bytes,
            truncated: &mut truncated,
        };
        // Opened through the held start directory, never by the path the walk listed.
        let opened = entry.path().strip_prefix(&start).map_err(io::Error::other).and_then(|within| beneath.open(within));
        match opened {
            Ok(file) => {
                if let Err(err) = searcher.search_file(&regex, &file, &mut sink) {
                    tracing::debug!(%err, path = %rel, "skipping an unreadable file");
                }
            }
            Err(err) => tracing::debug!(%err, path = %rel, "skipping a file that no longer resolves inside the root"),
        }
        if truncated {
            break;
        }
    }
    Ok(GrepResult { matches, truncated })
}

fn glob(base: &Base, patterns: &[String], path: &str, max: u32) -> Outcome<GlobResult> {
    let loc = base.resolve(path, Follow::Final, Authority::Path(Access::Read))?;
    if loc.target_dir().is_none() {
        return Err(ProtoError::new(ErrorCode::NotFound, format!("`{path}` is not a directory")));
    }
    let mut beneath = Beneath::new(&loc).ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("`{path}` does not exist")))?;
    let start = base.real(&loc);
    let mut set = GlobSetBuilder::new();
    let mut hidden = false;
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("invalid glob `{pattern}`: {err}")))?;
        set.add(glob);
        // A pattern that names dot-entries explicitly (`.github/**`, `**/.env`) should find them.
        hidden |= pattern.split('/').any(|segment| segment.starts_with('.') && segment != "." && segment != "..");
    }
    let set = set.build().map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("invalid globs: {err}")))?;
    let limit = usize::try_from(max).unwrap_or(usize::MAX);
    let mut paths = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    for entry in walker(&start, hidden).build() {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(within) = entry.path().strip_prefix(&start) else { continue };
        // Reported only if it still resolves inside, through the held start directory.
        if set.is_match(within) && beneath.exists(within) {
            let path = base.relative(entry.path());
            bytes = bytes.saturating_add(path.len());
            if paths.len() >= limit || bytes > MAX_RESULT_BYTES {
                truncated = true;
                break;
            }
            paths.push(path);
        }
    }
    paths.sort();
    Ok(GlobResult { paths, truncated })
}

impl Search for LocalSearch {
    fn grep<'a>(&'a self, query: GrepQuery<'a>) -> BoxFuture<'a, Outcome<GrepResult>> {
        let base = Arc::clone(&self.base);
        let job = GrepJob {
            pattern: query.pattern.to_owned(),
            path: query.path.to_owned(),
            globs: query.globs.to_vec(),
            case: query.case,
            fixed: query.fixed_strings,
            context: query.context,
            max: query.max_matches,
        };
        Box::pin(blocking(move || grep(&base, &job)))
    }

    fn glob<'a>(&'a self, query: GlobQuery<'a>) -> BoxFuture<'a, Outcome<GlobResult>> {
        let base = Arc::clone(&self.base);
        let (patterns, path, max) = (query.patterns.to_vec(), query.path.to_owned(), query.max_results);
        Box::pin(blocking(move || glob(&base, &patterns, &path, max)))
    }
}
