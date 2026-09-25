//! `aim-harness/1`: the execution layer's protocol (docs/architecture.md §4.1, docs/adr/0006).
//!
//! The agent layer, MCP adapters and remote `aimx` peers all speak this. Paths are strings,
//! relative to the workspace root or absolute on the target host; aimx confines them to the
//! workspace roots its principal is granted. Every mutating request carries an
//! [`IdempotencyKey`]: a retry after a transport drop returns the recorded outcome, and an expired
//! record yields `unknown_outcome` rather than a second execution.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Content;
use crate::ids::{IdempotencyKey, ProcId, ResumeToken, WorkspaceId};
use crate::tool::{ToolResult, ToolSpec};
use crate::{method, notification};

// ------------------------------------------------------------------------------------------------
// initialize
// ------------------------------------------------------------------------------------------------

/// An inclusive range of protocol generations.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GenerationRange {
    /// Oldest generation spoken.
    pub min: u32,
    /// Newest generation spoken.
    pub max: u32,
}

/// Who is on the other end of the connection.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PeerInfo {
    /// Program name, e.g. `aim` or `aimx`.
    pub name: String,
    /// Program version.
    pub version: String,
}

/// How a network client proves its identity (unix-socket peers are identified by peer
/// credentials and send nothing). `Debug` never prints the secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthProof {
    /// A scoped bearer token issued by the harness owner. Only ever sent over TLS or a local
    /// channel.
    Bearer {
        /// The token.
        token: String,
    },
}

impl core::fmt::Debug for AuthProof {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bearer { .. } => f.write_str("Bearer(***)"),
        }
    }
}

/// `initialize` parameters: the first request on every connection.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InitializeParams {
    /// Generations the client speaks.
    pub generations: GenerationRange,
    /// The client.
    pub client: PeerInfo,
    /// Credentials for network transports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthProof>,
    /// Resume an earlier session's processes and streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeToken>,
}

/// Limits the harness enforces; clients must stay within them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    /// Largest JSON-RPC message accepted, in bytes.
    pub max_message_bytes: u64,
    /// Largest single `fs.read` payload returned, in bytes.
    pub max_read_bytes: u64,
    /// Output kept per process for `exec.read` and resume, in bytes.
    pub output_ring_bytes: u64,
    /// How long idempotency records are kept, in seconds.
    pub dedup_window_secs: u32,
    /// How long a disconnected session stays resumable, in seconds.
    pub resume_ttl_secs: u32,
}

/// The authenticated principal and what it may do.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PrincipalInfo {
    /// Stable principal id (e.g. `local:<uid>` or a token's subject).
    pub id: String,
    /// Workspace roots the principal may open.
    pub roots: Vec<String>,
    /// The principal may only read.
    pub read_only: bool,
}

/// `initialize` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    /// The negotiated generation.
    pub generation: u32,
    /// The harness.
    pub server: PeerInfo,
    /// Who the harness thinks the client is.
    pub principal: PrincipalInfo,
    /// Enforced limits.
    pub limits: Limits,
    /// Token to resume this session after a transport drop.
    pub resume_token: ResumeToken,
    /// Whether `resume` in the request was honoured.
    pub resumed: bool,
}

method!(
    /// `initialize` — negotiate the generation, authenticate, optionally resume.
    Initialize = "initialize" (InitializeParams) -> InitializeResult
);

// ------------------------------------------------------------------------------------------------
// workspace
// ------------------------------------------------------------------------------------------------

/// Which backend serves a workspace.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendSpec {
    /// The harness's own host.
    #[default]
    Local,
    /// A remote host over SSH (resident remote aimx, or agentless fallback).
    Ssh {
        /// `ssh` destination (`host`, `user@host`, or an `ssh_config` alias).
        destination: String,
        /// Whether a remote aimx may be installed.
        #[serde(default)]
        bootstrap: BootstrapPolicy,
    },
}

/// Whether the harness may install a remote aimx binary (docs/adr/0009).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapPolicy {
    /// Install a verified binary when needed; fall back to agentless.
    #[default]
    Auto,
    /// Never install; agentless only.
    Never,
}

/// What a workspace backend can do.
#[expect(clippy::struct_excessive_bools, reason = "independent capability flags are the natural wire shape")]
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Caps {
    /// Processes can be spawned.
    pub exec: bool,
    /// Processes can get a pseudo-terminal.
    pub pty: bool,
    /// File changes can be watched.
    pub watch: bool,
    /// Search runs next to the data (no per-file round trips).
    pub native_search: bool,
    /// Renames are atomic.
    pub atomic_rename: bool,
    /// Processes and streams survive transport drops.
    pub resumable: bool,
    /// Concurrency cap of the backend, if any (agentless SSH: sshd `MaxSessions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u16>,
    /// Target OS, e.g. `linux`, `macos`.
    pub os: String,
    /// Target architecture, e.g. `aarch64`.
    pub arch: String,
    /// Default shell, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

/// `workspace.open` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceOpenParams {
    /// Root directory on the target host.
    pub root: String,
    /// Backend to use.
    #[serde(default)]
    pub backend: BackendSpec,
}

/// An opened workspace.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceInfo {
    /// Id to use in later requests.
    pub id: WorkspaceId,
    /// Canonical root on the target host.
    pub root: String,
    /// What the backend can do.
    pub caps: Caps,
}

method!(
    /// `workspace.open` — open (or reuse) a workspace rooted at a directory.
    WorkspaceOpen = "workspace.open" (WorkspaceOpenParams) -> WorkspaceInfo
);

// ------------------------------------------------------------------------------------------------
// fs
// ------------------------------------------------------------------------------------------------

/// A content hash, `sha256:<hex>`.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ContentHash(pub String);

/// Condition a write requires of the current file.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Precondition {
    /// No condition.
    #[default]
    Any,
    /// The file must not exist.
    IfAbsent,
    /// The file must currently have this hash (it has not changed since it was read).
    IfHash {
        /// Expected current hash.
        hash: ContentHash,
    },
}

/// A byte range `[start, start + len)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// Number of bytes.
    pub len: u64,
}

/// Kind of a filesystem entry.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (not followed).
    Symlink,
    /// Anything else (socket, device, fifo).
    Other,
}

/// Metadata of a filesystem entry.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Meta {
    /// Kind of entry.
    pub kind: EntryKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    /// Content hash, for files, when computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<ContentHash>,
}

/// `fs.stat` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsStatParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Path to inspect.
    pub path: String,
    /// Also compute the content hash of a file.
    #[serde(default)]
    pub hash: bool,
}

method!(
    /// `fs.stat` — metadata of one entry.
    FsStat = "fs.stat" (FsStatParams) -> Meta
);

/// `fs.read` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsReadParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// File to read.
    pub path: String,
    /// Only this byte range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ByteRange>,
}

/// `fs.read` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsReadResult {
    /// The bytes read.
    pub content: Content,
    /// Total file size.
    pub size: u64,
    /// Hash of the whole file (use it as an `IfHash` precondition when writing back).
    pub hash: ContentHash,
    /// Fewer bytes than requested were returned because of `max_read_bytes`.
    pub truncated: bool,
}

method!(
    /// `fs.read` — read a file (or a range of it).
    FsRead = "fs.read" (FsReadParams) -> FsReadResult
);

/// `fs.write` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsWriteParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// File to write (replaced atomically).
    pub path: String,
    /// New content.
    pub content: Content,
    /// Required current state.
    #[serde(default)]
    pub precondition: Precondition,
    /// Create missing parent directories.
    #[serde(default)]
    pub create_dirs: bool,
    /// Retry safety.
    pub idempotency_key: IdempotencyKey,
}

/// Outcome of a successful write or edit.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WriteOutcome {
    /// Hash of the file after the change.
    pub hash: ContentHash,
    /// Size of the file after the change.
    pub size: u64,
    /// The file did not exist before.
    pub created: bool,
}

method!(
    /// `fs.write` — replace a file atomically, under a precondition.
    FsWrite = "fs.write" (FsWriteParams) -> WriteOutcome
);

/// One exact-substring replacement.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExactEdit {
    /// Text to find; must occur exactly once unless `replace_all`.
    pub old: String,
    /// Replacement text.
    pub new: String,
    /// Replace every occurrence instead of requiring exactly one.
    #[serde(default)]
    pub replace_all: bool,
}

/// `fs.edit` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsEditParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// File to edit.
    pub path: String,
    /// Edits, applied in order to the result of the previous one; all or nothing.
    pub edits: Vec<ExactEdit>,
    /// Required current state.
    #[serde(default)]
    pub precondition: Precondition,
    /// Retry safety.
    pub idempotency_key: IdempotencyKey,
}

/// `fs.edit` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EditOutcome {
    /// The resulting file.
    pub write: WriteOutcome,
    /// Replacements made, per edit.
    pub replacements: Vec<u32>,
}

method!(
    /// `fs.edit` — apply exact-substring edits atomically (all or nothing).
    FsEdit = "fs.edit" (FsEditParams) -> EditOutcome
);

/// `fs.list` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsListParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Directory to list.
    pub path: String,
    /// Maximum entries to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Continue from a previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
    /// Include entries whose name starts with `.`.
    #[serde(default)]
    pub include_hidden: bool,
}

/// One directory entry.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DirEntry {
    /// Entry name (not a path).
    pub name: String,
    /// Kind of entry.
    pub kind: EntryKind,
    /// Size in bytes (files).
    pub size: u64,
}

/// `fs.list` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsListResult {
    /// Entries, sorted by name.
    pub entries: Vec<DirEntry>,
    /// Token for the next page, when more entries exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
}

method!(
    /// `fs.list` — list a directory, paginated.
    FsList = "fs.list" (FsListParams) -> FsListResult
);

/// `fs.mkdir` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsMkdirParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Directory to create (with parents).
    pub path: String,
    /// Retry safety.
    pub idempotency_key: IdempotencyKey,
}

method!(
    /// `fs.mkdir` — create a directory and its parents.
    FsMkdir = "fs.mkdir" (FsMkdirParams) -> ()
);

/// `fs.remove` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsRemoveParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Entry to remove.
    pub path: String,
    /// Remove a directory and everything in it.
    #[serde(default)]
    pub recursive: bool,
    /// Retry safety.
    pub idempotency_key: IdempotencyKey,
}

method!(
    /// `fs.remove` — remove a file or directory.
    FsRemove = "fs.remove" (FsRemoveParams) -> ()
);

/// `fs.rename` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsRenameParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Source path.
    pub from: String,
    /// Destination path.
    pub to: String,
    /// Replace an existing destination.
    #[serde(default)]
    pub overwrite: bool,
    /// Retry safety.
    pub idempotency_key: IdempotencyKey,
}

method!(
    /// `fs.rename` — move or rename an entry.
    FsRename = "fs.rename" (FsRenameParams) -> ()
);

/// `fs.read_many` parameters: several files in one round trip (search-heavy work over SSH).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsReadManyParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Files to read.
    pub paths: Vec<String>,
    /// Most bytes returned per file (clamped to the harness's `max_read_bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_file: Option<u64>,
}

/// The outcome for one file of `fs.read_many`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReadManyEntry {
    /// The file was read.
    Ok {
        /// Path as requested.
        path: String,
        /// Result.
        read: FsReadResult,
    },
    /// The file could not be read.
    Error {
        /// Path as requested.
        path: String,
        /// Error code name (`not_found`, `denied`, …).
        code: String,
        /// Human-readable reason.
        message: String,
    },
}

/// `fs.read_many` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsReadManyResult {
    /// One entry per requested path, in order.
    pub entries: Vec<ReadManyEntry>,
}

method!(
    /// `fs.read_many` — read several files in one round trip.
    FsReadMany = "fs.read_many" (FsReadManyParams) -> FsReadManyResult
);

/// `fs.copy` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FsCopyParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Source path.
    pub from: String,
    /// Destination path.
    pub to: String,
    /// Replace an existing destination.
    #[serde(default)]
    pub overwrite: bool,
    /// Copy a directory recursively.
    #[serde(default)]
    pub recursive: bool,
    /// Retry safety.
    pub idempotency_key: IdempotencyKey,
}

method!(
    /// `fs.copy` — copy a file or directory.
    FsCopy = "fs.copy" (FsCopyParams) -> ()
);

/// `watch.start` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchStartParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Directory or file to watch (recursively for directories; `.gitignore` respected).
    pub path: String,
}

/// `watch.start` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchStartResult {
    /// The watch; events arrive as `watch.event` notifications.
    pub watch: String,
}

method!(
    /// `watch.start` — watch for file changes (requires `Caps.watch`).
    WatchStart = "watch.start" (WatchStartParams) -> WatchStartResult
);

/// `watch.stop` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchStopParams {
    /// The watch to stop.
    pub watch: String,
}

method!(
    /// `watch.stop` — stop a watch.
    WatchStop = "watch.stop" (WatchStopParams) -> ()
);

/// What happened to a watched path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WatchChange {
    /// Created.
    Created,
    /// Modified.
    Modified,
    /// Removed.
    Removed,
    /// Events were dropped; rescan.
    Overflow,
}

/// `watch.event` notification parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchEventParams {
    /// The watch.
    pub watch: String,
    /// Sequence number within the watch.
    pub seq: u64,
    /// What happened.
    pub change: WatchChange,
    /// Path, relative to the workspace root.
    pub path: String,
}

notification!(
    /// `watch.event` — a watched path changed.
    WatchEvent = "watch.event" (WatchEventParams)
);

// ------------------------------------------------------------------------------------------------
// exec
// ------------------------------------------------------------------------------------------------

/// What to run.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    /// A program and its arguments, run without a shell.
    Argv {
        /// Program followed by its arguments.
        argv: Vec<String>,
    },
    /// A script run by the target's shell (`sh -c` on POSIX).
    Shell {
        /// The script.
        script: String,
    },
}

/// Terminal size of a pseudo-terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PtySize {
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

/// `exec.spawn` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecSpawnParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// What to run.
    pub command: Command,
    /// Working directory (defaults to the workspace root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Run under a pseudo-terminal of this size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<PtySize>,
    /// Keep stdin open for `exec.write_stdin` (otherwise stdin is empty).
    #[serde(default)]
    pub stdin: bool,
    /// Kill the process after this many milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Retry safety: a retried spawn returns the same process.
    pub idempotency_key: IdempotencyKey,
}

/// `exec.spawn` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecSpawnResult {
    /// The process.
    pub proc: ProcId,
}

method!(
    /// `exec.spawn` — start a process.
    ExecSpawn = "exec.spawn" (ExecSpawnParams) -> ExecSpawnResult
);

/// Which output stream a chunk came from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// The pseudo-terminal (stdout and stderr merged).
    Pty,
}

/// A piece of process output. `seq` is strictly increasing per process across both streams.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OutputChunk {
    /// Sequence number within the process.
    pub seq: u64,
    /// Stream it came from.
    pub stream: OutputStream,
    /// The bytes.
    pub data: Content,
}

/// How a process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitStatus {
    /// Exited normally with a status code.
    Exited {
        /// Exit code.
        code: i32,
    },
    /// Terminated by a signal.
    Signaled {
        /// Signal number.
        signal: i32,
    },
    /// Killed because its timeout elapsed.
    TimedOut,
}

/// `exec.read` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecReadParams {
    /// Process.
    pub proc: ProcId,
    /// Return chunks with `seq` greater than this (0 = from the start of the retained output).
    #[serde(default)]
    pub after_seq: u64,
    /// Maximum bytes to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Wait up to this long for new output or exit before answering.
    #[serde(default)]
    pub wait_ms: u64,
}

/// `exec.read` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecReadResult {
    /// Chunks after `after_seq`, in order.
    pub chunks: Vec<OutputChunk>,
    /// Output before this seq was dropped from the ring buffer (the reader fell too far behind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_before: Option<u64>,
    /// Set once the process has ended and all output has been returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitStatus>,
}

method!(
    /// `exec.read` — pull output after a sequence number (works across reconnects).
    ExecRead = "exec.read" (ExecReadParams) -> ExecReadResult
);

/// `exec.output` notification parameters (push; `exec.read` is the reconciliation read).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecOutputParams {
    /// Process.
    pub proc: ProcId,
    /// The chunk.
    pub chunk: OutputChunk,
}

notification!(
    /// `exec.output` — output pushed as it happens.
    ExecOutput = "exec.output" (ExecOutputParams)
);

/// `exec.exited` notification parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecExitedParams {
    /// Process.
    pub proc: ProcId,
    /// How it ended.
    pub status: ExitStatus,
    /// Sequence number of its last output chunk (0 if none).
    pub last_seq: u64,
}

notification!(
    /// `exec.exited` — the process ended.
    ExecExited = "exec.exited" (ExecExitedParams)
);

/// `exec.write_stdin` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecWriteStdinParams {
    /// Process.
    pub proc: ProcId,
    /// Bytes to write.
    pub data: Content,
    /// Close stdin afterwards.
    #[serde(default)]
    pub eof: bool,
    /// Retry safety: a retried write does not send its bytes twice.
    pub idempotency_key: IdempotencyKey,
}

method!(
    /// `exec.write_stdin` — write to a process's stdin (or its pty).
    ExecWriteStdin = "exec.write_stdin" (ExecWriteStdinParams) -> ()
);

/// `exec.resize` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecResizeParams {
    /// Process.
    pub proc: ProcId,
    /// New size.
    pub size: PtySize,
}

method!(
    /// `exec.resize` — resize a process's pseudo-terminal.
    ExecResize = "exec.resize" (ExecResizeParams) -> ()
);

/// Signals a client may send.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Interrupt (SIGINT).
    Interrupt,
    /// Terminate (SIGTERM).
    Terminate,
    /// Kill (SIGKILL).
    Kill,
}

/// `exec.signal` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecSignalParams {
    /// Process.
    pub proc: ProcId,
    /// Signal to send (to the process group).
    pub signal: Signal,
}

method!(
    /// `exec.signal` — signal a process group.
    ExecSignal = "exec.signal" (ExecSignalParams) -> ()
);

/// `exec.wait` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecWaitParams {
    /// Process.
    pub proc: ProcId,
    /// Give up after this long (the process keeps running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `exec.wait` result.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecWaitResult {
    /// How it ended, or `None` if the timeout elapsed first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitStatus>,
}

method!(
    /// `exec.wait` — wait for a process to end.
    ExecWait = "exec.wait" (ExecWaitParams) -> ExecWaitResult
);

/// `exec.release` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecReleaseParams {
    /// Process (killed if still running) whose retained output is freed.
    pub proc: ProcId,
}

method!(
    /// `exec.release` — forget a process and free its output.
    ExecRelease = "exec.release" (ExecReleaseParams) -> ()
);

// ------------------------------------------------------------------------------------------------
// search
// ------------------------------------------------------------------------------------------------

/// Case sensitivity of a search.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaseMode {
    /// Insensitive unless the pattern has an uppercase letter.
    #[default]
    Smart,
    /// Case-sensitive.
    Sensitive,
    /// Case-insensitive.
    Insensitive,
}

/// `search.grep` parameters (ripgrep semantics; `.gitignore` respected).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Regular expression (or literal with `fixed_strings`).
    pub pattern: String,
    /// Search under this path (defaults to the root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only files matching these globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    /// Case sensitivity.
    #[serde(default)]
    pub case: CaseMode,
    /// Treat the pattern as a literal string.
    #[serde(default)]
    pub fixed_strings: bool,
    /// Lines of context before and after each match.
    #[serde(default)]
    pub context: u32,
    /// Maximum matches to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<u32>,
}

/// One matching line.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepMatch {
    /// File, relative to the workspace root.
    pub path: String,
    /// 1-based line number.
    pub line: u64,
    /// The matching line (without its newline).
    pub text: String,
    /// Context lines before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// Context lines after.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// `search.grep` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepResult {
    /// Matches in path, then line order.
    pub matches: Vec<GrepMatch>,
    /// More matches exist than were returned.
    pub truncated: bool,
}

method!(
    /// `search.grep` — search file contents.
    Grep = "search.grep" (GrepParams) -> GrepResult
);

/// `search.glob` parameters (`.gitignore` respected).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GlobParams {
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Glob patterns, e.g. `**/*.rs`.
    pub patterns: Vec<String>,
    /// Search under this path (defaults to the root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum paths to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
}

/// `search.glob` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GlobResult {
    /// Matching paths, relative to the workspace root, sorted.
    pub paths: Vec<String>,
    /// More paths exist than were returned.
    pub truncated: bool,
}

method!(
    /// `search.glob` — find files by name pattern.
    Glob = "search.glob" (GlobParams) -> GlobResult
);

// ------------------------------------------------------------------------------------------------
// tools
// ------------------------------------------------------------------------------------------------

/// `tools.list` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolsListParams {}

/// `tools.list` result.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolsListResult {
    /// The harness's tools.
    pub tools: Vec<ToolSpec>,
}

method!(
    /// `tools.list` — the harness's high-level tools (what MCP projects).
    ToolsList = "tools.list" (ToolsListParams) -> ToolsListResult
);

/// `tools.call` parameters.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolsCallParams {
    /// Workspace the call acts on.
    pub workspace: WorkspaceId,
    /// Tool name.
    pub name: String,
    /// Arguments, matching the tool's `input_schema`.
    pub arguments: Value,
    /// Required for mutating tools (see their annotations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

method!(
    /// `tools.call` — run a high-level tool.
    ToolsCall = "tools.call" (ToolsCallParams) -> ToolResult
);
