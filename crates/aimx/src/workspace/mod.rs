//! The `Workspace` trait: the only way aimx's tools reach files and processes
//! (docs/architecture.md §9, docs/adr/0004, 0009).
//!
//! Backends: `local` (this host), `ssh` (a resident remote aimx over an SSH channel, or the
//! agentless fallback), later containers and cloud buckets. Tools are written once against these
//! traits, so every tool call can be shadowed onto another host.
//!
//! Contract for every backend:
//! - Paths passed in are **already lexically confined** by the caller (`authz`): absolute,
//!   normalized, inside the workspace root. The backend must still refuse to follow a symlink out
//!   of the root (the lexical check cannot see symlinks) and report `denied` if it would.
//! - Mutations carry an [`IdempotencyKey`]; a backend that forwards to another harness (remote
//!   aimx) passes it on so retries are deduplicated where the effect happens. The local backend
//!   may ignore it (the server's dedup table sits above it).
//! - Writes are atomic (temp file + rename) and honour [`Precondition`]s.
//! - Errors use `aim_proto::error::ErrorCode`s: `not_found`, `precondition_failed`, `conflict`
//!   (e.g. an exact edit that does not match exactly once), `denied`, `unavailable`, `timeout`.
//! - Capabilities are reported honestly in [`Caps`]; an absent capability returns `unavailable`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    ByteRange, Caps, EditOutcome, ExactEdit, ExecReadResult, FsListResult, FsReadResult, GlobResult, GrepResult, Meta, Precondition,
    PtySize, Signal, WriteOutcome,
};
use aim_proto::ids::{IdempotencyKey, ProcId};

use crate::authz::Grant;

pub mod local;

/// A boxed, sendable future borrowing the backend.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result of a backend operation.
pub type Outcome<T> = Result<T, ProtoError>;

/// A workspace: a root directory on some host, with files, processes and search.
pub trait Workspace: Send + Sync {
    /// Bind this backend to one request's effective grant. A backend without descriptor-bound
    /// scope enforcement returns `None`, so callers can refuse scoped access.
    fn scoped(&self, _grant: Grant) -> Option<Arc<dyn Workspace>> {
        None
    }

    /// What this backend can do.
    fn caps(&self) -> &Caps;

    /// Canonical root on the target host.
    fn root(&self) -> &str;

    /// Filesystem operations.
    fn fs(&self) -> &dyn Fs;

    /// Process execution, when the backend supports it (`caps().exec`).
    fn exec(&self) -> Option<&dyn Exec>;

    /// Content and name search (native, or emulated over [`Fs`]/[`Exec`]).
    fn search(&self) -> &dyn Search;
}

/// Filesystem operations on confined paths.
pub trait Fs: Send + Sync {
    /// Metadata of one entry (symlinks are not followed).
    fn stat<'a>(&'a self, path: &'a str, hash: bool) -> BoxFuture<'a, Outcome<Meta>>;

    /// Reads a file or a byte range of it, returning at most `max_bytes`. When `hash` is true,
    /// computes the whole-file hash; otherwise the backend must stop reading at the byte limit.
    fn read<'a>(&'a self, path: &'a str, range: Option<ByteRange>, max_bytes: u64, hash: bool) -> BoxFuture<'a, Outcome<FsReadResult>>;

    /// Replaces a file atomically under a precondition.
    fn write<'a>(&'a self, req: WriteRequest<'a>) -> BoxFuture<'a, Outcome<WriteOutcome>>;

    /// Whether this backend can cancel a claimed file only while its marker hash still matches.
    fn supports_reservations(&self) -> bool {
        false
    }

    /// Removes a reservation marker only if the file still has `hash`.
    fn cancel_if_hash<'a>(&'a self, _path: &'a str, _hash: &'a aim_proto::harness::ContentHash) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "this backend cannot cancel reservations")) })
    }

    /// Applies exact-substring edits atomically: all or nothing, each edit applied to the result of
    /// the previous one; an edit whose `old` text does not occur exactly once (unless
    /// `replace_all`) fails the whole request with `conflict`.
    fn edit<'a>(&'a self, req: EditRequest<'a>) -> BoxFuture<'a, Outcome<EditOutcome>>;

    /// Lists a directory, sorted by name, paginated.
    fn list<'a>(&'a self, req: ListRequest<'a>) -> BoxFuture<'a, Outcome<FsListResult>>;

    /// Creates a directory and its parents.
    fn mkdir<'a>(&'a self, path: &'a str, key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>>;

    /// Removes a file, or a directory (recursively when asked).
    fn remove<'a>(&'a self, path: &'a str, recursive: bool, key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>>;

    /// Renames an entry.
    fn rename<'a>(&'a self, from: &'a str, to: &'a str, overwrite: bool, key: &'a IdempotencyKey) -> BoxFuture<'a, Outcome<()>>;

    /// Copies a file, or a directory tree when `recursive` (`fs.copy`). The destination appears
    /// atomically; symlinks inside a copied tree are copied as links, never followed. Backends
    /// that cannot copy answer `unavailable` (the default).
    fn copy<'a>(&'a self, _req: CopyRequest<'a>) -> BoxFuture<'a, Outcome<()>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "this backend cannot copy")) })
    }
}

/// A copy request.
#[derive(Clone, Debug)]
pub struct CopyRequest<'a> {
    /// Confined source path.
    pub from: &'a str,
    /// Confined destination path.
    pub to: &'a str,
    /// Replace an existing destination file.
    pub overwrite: bool,
    /// Copy a directory and everything in it.
    pub recursive: bool,
    /// Retry safety.
    pub key: &'a IdempotencyKey,
}

/// A file write.
#[derive(Clone, Debug)]
pub struct WriteRequest<'a> {
    /// Confined target path.
    pub path: &'a str,
    /// New content.
    pub content: &'a Content,
    /// Required current state.
    pub precondition: &'a Precondition,
    /// Create missing parent directories.
    pub create_dirs: bool,
    /// Retry safety.
    pub key: &'a IdempotencyKey,
}

/// An exact-edit request.
#[derive(Clone, Debug)]
pub struct EditRequest<'a> {
    /// Confined target path.
    pub path: &'a str,
    /// Edits in order.
    pub edits: &'a [ExactEdit],
    /// Required current state.
    pub precondition: &'a Precondition,
    /// Retry safety.
    pub key: &'a IdempotencyKey,
}

/// A directory listing request.
#[derive(Clone, Debug)]
pub struct ListRequest<'a> {
    /// Confined directory path.
    pub path: &'a str,
    /// Maximum entries.
    pub limit: u32,
    /// Continue from a previous page.
    pub page_token: Option<&'a str>,
    /// Include dot-entries.
    pub include_hidden: bool,
}

/// Process execution.
pub trait Exec: Send + Sync {
    /// Starts a process (in its own process group); returns its id.
    fn spawn<'a>(&'a self, spec: SpawnSpec<'a>) -> BoxFuture<'a, Outcome<ProcId>>;

    /// Output after `after_seq` (up to `max_bytes`), waiting up to `wait` for more or for exit.
    fn read<'a>(&'a self, proc: &'a ProcId, after_seq: u64, max_bytes: u64, wait: Duration) -> BoxFuture<'a, Outcome<ExecReadResult>>;

    /// Writes to stdin (or the pty); closes it after when `eof`.
    fn write_stdin<'a>(&'a self, proc: &'a ProcId, data: &'a [u8], eof: bool) -> BoxFuture<'a, Outcome<()>>;

    /// Resizes the pty.
    fn resize<'a>(&'a self, proc: &'a ProcId, size: PtySize) -> BoxFuture<'a, Outcome<()>>;

    /// Signals the process group.
    fn signal<'a>(&'a self, proc: &'a ProcId, signal: Signal) -> BoxFuture<'a, Outcome<()>>;

    /// Forgets a process (killing it if still running) and frees its output.
    fn release<'a>(&'a self, proc: &'a ProcId) -> BoxFuture<'a, Outcome<()>>;
}

/// What to spawn.
#[derive(Clone, Debug)]
pub struct SpawnSpec<'a> {
    /// Program and arguments, or a shell script.
    pub command: &'a aim_proto::harness::Command,
    /// Confined working directory.
    pub cwd: &'a str,
    /// Extra environment.
    pub env: &'a std::collections::BTreeMap<String, String>,
    /// Pseudo-terminal size, when requested.
    pub pty: Option<PtySize>,
    /// Keep stdin open.
    pub stdin: bool,
    /// Kill after this long.
    pub timeout: Option<Duration>,
    /// Retry safety.
    pub key: &'a IdempotencyKey,
}

/// Content and name search (ripgrep semantics; `.gitignore` respected).
pub trait Search: Send + Sync {
    /// Searches file contents.
    fn grep<'a>(&'a self, query: GrepQuery<'a>) -> BoxFuture<'a, Outcome<GrepResult>>;

    /// Finds files by glob.
    fn glob<'a>(&'a self, query: GlobQuery<'a>) -> BoxFuture<'a, Outcome<GlobResult>>;
}

/// A content search.
#[derive(Clone, Debug)]
pub struct GrepQuery<'a> {
    /// Pattern (regex unless `fixed_strings`).
    pub pattern: &'a str,
    /// Confined directory or file to search.
    pub path: &'a str,
    /// Only files matching these globs.
    pub globs: &'a [String],
    /// Case sensitivity.
    pub case: aim_proto::harness::CaseMode,
    /// Treat the pattern as a literal.
    pub fixed_strings: bool,
    /// Context lines around each match.
    pub context: u32,
    /// Maximum matches.
    pub max_matches: u32,
}

/// A name search.
#[derive(Clone, Debug)]
pub struct GlobQuery<'a> {
    /// Glob patterns.
    pub patterns: &'a [String],
    /// Confined directory to search.
    pub path: &'a str,
    /// Maximum paths.
    pub max_results: u32,
}
