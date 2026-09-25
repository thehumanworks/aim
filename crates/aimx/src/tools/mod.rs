//! The model-facing tools (docs/architecture.md §6.3, docs/adr/0012).
//!
//! Shapes follow Claude Code's well-known tools (`Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`,
//! `BashOutput`, `KillShell`, `LS`), so any model is at home and Claude's aim-tools mode can alias
//! them one to one. Every tool is written against the [`Workspace`] traits only — never OS APIs
//! (`cargo xtask check` enforces it) — so the same call runs locally or on a remote host.
//!
//! Errors split in two: policy and transport failures (`denied`, `unavailable`, …) are protocol
//! errors; failures the model should see and handle (missing file, ambiguous edit, non-zero exit)
//! are results with `is_error` set.

mod files;
mod search;
mod shell;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::{IdempotencyKey, ProcId, WorkspaceId};
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::authz::Grant;
use crate::workspace::{Outcome, Workspace};

/// Everything a tool call may use.
#[derive(Clone)]
pub struct ToolCtx {
    /// The workspace the call acts on.
    pub workspace_id: WorkspaceId,
    /// Its backend.
    pub workspace: Arc<dyn Workspace>,
    /// The caller's authority over it (every path goes through it).
    pub grant: Grant,
    /// The session's processes (background commands, large-output handles).
    pub procs: Arc<ProcTable>,
    /// The call's idempotency key (required for mutating tools).
    pub key: Option<IdempotencyKey>,
    /// Largest file read, in bytes.
    pub max_read_bytes: u64,
    /// Cancels detached tool work when the caller cancels its request.
    pub cancelled: CancellationToken,
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx").field("workspace_id", &self.workspace_id).field("key", &self.key).finish_non_exhaustive()
    }
}

impl ToolCtx {
    /// A key for one workspace mutation made by this call, derived from the call's key.
    fn derived_key(&self, step: &str) -> IdempotencyKey {
        match &self.key {
            Some(key) => IdempotencyKey::new(format!("{key}/{step}")),
            None => IdempotencyKey::new(format!("tool-{}/{step}", crate::id::random_hex())),
        }
    }
}

/// Processes a session owns, with the workspace each runs in and how far `BashOutput` has read.
///
/// It also admits them: every process holds a [`ProcSlot`] from the session's cap and the server's
/// global cap from before it is spawned until it is released (or the session ends), whether or not
/// it has exited, since an unreleased process keeps its output ring (and its process group).
#[derive(Debug)]
pub struct ProcTable {
    procs: Mutex<HashMap<ProcId, ProcEntry>>,
    session: Arc<Semaphore>,
    global: Arc<Semaphore>,
}

impl Default for ProcTable {
    /// A table without a cap (for tests and embedders that admit processes elsewhere).
    fn default() -> Self {
        Self::new(Semaphore::MAX_PERMITS, Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)))
    }
}

#[derive(Debug)]
struct ProcEntry {
    workspace: WorkspaceId,
    cursor: u64,
    _slot: ProcSlot,
}

/// A reservation for one live process: one of the session's slots and one of the server's.
#[derive(Debug)]
pub struct ProcSlot {
    _session: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

impl ProcTable {
    /// A table admitting at most `per_session` live processes, and only while `global` (shared
    /// by every session of the server) has room.
    #[must_use]
    pub fn new(per_session: usize, global: Arc<Semaphore>) -> Self {
        Self { procs: Mutex::new(HashMap::new()), session: Arc::new(Semaphore::new(per_session.min(Semaphore::MAX_PERMITS))), global }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ProcId, ProcEntry>> {
        self.procs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserves room for one more process, before spawning it.
    ///
    /// # Errors
    /// `limit_exceeded` when the session or the server holds as many live processes as it may
    /// (release one first).
    pub fn reserve(&self) -> Outcome<ProcSlot> {
        let full = |whose: &str| {
            ProtoError::new(ErrorCode::LimitExceeded, format!("{whose} holds as many live processes as it may; release one first"))
        };
        let session = Arc::clone(&self.session).try_acquire_owned().map_err(|_| full("this session"))?;
        let global = Arc::clone(&self.global).try_acquire_owned().map_err(|_| full("the server"))?;
        Ok(ProcSlot { _session: session, _global: global })
    }

    /// Records a process spawned in `workspace` with the slot reserved for it.
    pub fn insert(&self, proc: ProcId, workspace: WorkspaceId, slot: ProcSlot) {
        self.lock().insert(proc, ProcEntry { workspace, cursor: 0, _slot: slot });
    }

    /// The workspace a process runs in, when the session owns it.
    #[must_use]
    pub fn workspace(&self, proc: &ProcId) -> Option<WorkspaceId> {
        self.lock().get(proc).map(|entry| entry.workspace.clone())
    }

    /// Forgets a process (freeing its slot); whether it was known.
    pub fn remove(&self, proc: &ProcId) -> bool {
        self.lock().remove(proc).is_some()
    }

    /// Every process with its workspace.
    #[must_use]
    pub fn all(&self) -> Vec<(ProcId, WorkspaceId)> {
        self.lock().iter().map(|(proc, entry)| (proc.clone(), entry.workspace.clone())).collect()
    }

    fn cursor(&self, proc: &ProcId) -> Option<u64> {
        self.lock().get(proc).map(|entry| entry.cursor)
    }

    fn set_cursor(&self, proc: &ProcId, cursor: u64) {
        if let Some(entry) = self.lock().get_mut(proc) {
            entry.cursor = cursor;
        }
    }
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    annotations: ToolAnnotations,
}

/// Only reads the workspace.
const READS: ToolAnnotations =
    ToolAnnotations { read_only: true, destructive: false, idempotent: true, open_world: false, location: ToolLocation::Workspace };
/// Only reads, but advances a cursor (repeating returns different output).
const OBSERVES: ToolAnnotations = ToolAnnotations { idempotent: false, ..READS };
/// Overwrites; repeating it has no further effect.
const REPLACES: ToolAnnotations =
    ToolAnnotations { read_only: false, destructive: true, idempotent: true, open_world: false, location: ToolLocation::Workspace };
/// Changes content; repeating it fails or changes more.
const CHANGES: ToolAnnotations = ToolAnnotations { idempotent: false, ..REPLACES };
/// Runs arbitrary commands (which may reach the network).
const RUNS: ToolAnnotations = ToolAnnotations { idempotent: false, open_world: true, ..REPLACES };

const TOOLS: [ToolDef; 9] = [
    ToolDef {
        name: "Read",
        description: "Read a file. Returns numbered lines (`cat -n` style) starting at line `offset` (1-based, default 1), at most `limit` lines (default 2000); lines longer than 2000 characters are cut. Images are returned as images; other binary files are refused.",
        schema: files::read_schema,
        annotations: READS,
    },
    ToolDef {
        name: "Write",
        description: "Create or overwrite a file with `content`. The write is atomic and creates missing parent directories.",
        schema: files::write_schema,
        annotations: REPLACES,
    },
    ToolDef {
        name: "Edit",
        description: "Replace `old_string` with `new_string` in a file. `old_string` must match exactly once (include enough surrounding context), unless `replace_all` is set.",
        schema: files::edit_schema,
        annotations: CHANGES,
    },
    ToolDef {
        name: "LS",
        description: "List a directory, sorted by name. Directories end with `/`, symlinks with `@`.",
        schema: files::ls_schema,
        annotations: READS,
    },
    ToolDef {
        name: "Glob",
        description: "Find files by glob pattern (e.g. `**/*.rs`, `src/*.{ts,tsx}`), respecting .gitignore. Returns sorted paths relative to the workspace root.",
        schema: search::glob_schema,
        annotations: READS,
    },
    ToolDef {
        name: "Grep",
        description: "Search file contents with a regular expression (ripgrep syntax), respecting .gitignore. `output_mode`: `files_with_matches` (default), `content` (matching lines, with `-n` line numbers and `-C` context lines) or `count`. `glob` filters files (e.g. `*.rs`); `-i` ignores case; `head_limit` keeps the first N entries.",
        schema: search::grep_schema,
        annotations: READS,
    },
    ToolDef {
        name: "Bash",
        description: "Run a bash command in the workspace root and return its combined stdout/stderr and exit code. `timeout` in ms (default 120000, max 600000). Long output keeps its head and tail. Jobs it starts with `&` end with it. With `run_in_background` it returns an id at once; read it with BashOutput, stop it with KillShell.",
        schema: shell::bash_schema,
        annotations: RUNS,
    },
    ToolDef {
        name: "BashOutput",
        description: "Return the new output and the status of a background command started with Bash.",
        schema: shell::id_schema,
        annotations: OBSERVES,
    },
    ToolDef {
        name: "KillShell",
        description: "Kill a background command started with Bash.",
        schema: shell::id_schema,
        annotations: REPLACES,
    },
];

/// Every tool, as advertised by `tools.list`.
#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    TOOLS.iter().map(spec_of).collect()
}

fn spec_of(def: &ToolDef) -> ToolSpec {
    ToolSpec {
        name: def.name.to_owned(),
        description: def.description.to_owned(),
        input_schema: (def.schema)(),
        input: ToolInput::Json,
        annotations: def.annotations,
    }
}

/// The annotations of a tool, when it exists.
#[must_use]
pub fn annotations_of(name: &str) -> Option<ToolAnnotations> {
    TOOLS.iter().find(|def| def.name == name).map(|def| def.annotations)
}

/// Whether a tool's result can name a process of the calling session (`Bash`: a background id or
/// a large-output handle), so a recorded result is only valid for that session.
#[must_use]
pub fn spawns_processes(name: &str) -> bool {
    name == "Bash"
}

/// Runs a tool.
///
/// # Errors
/// `not_found` for an unknown tool; policy (`denied`) and backend (`unavailable`, `internal`)
/// failures. Failures the model should handle are `Ok` results with `is_error` set.
pub async fn call(ctx: &ToolCtx, name: &str, arguments: Value) -> Outcome<ToolResult> {
    match name {
        "Read" => files::read(ctx, arguments).await,
        "Write" => files::write(ctx, arguments).await,
        "Edit" => files::edit(ctx, arguments).await,
        "LS" => files::ls(ctx, arguments).await,
        "Glob" => search::glob(ctx, arguments).await,
        "Grep" => search::grep(ctx, arguments).await,
        "Bash" => shell::bash(ctx, arguments).await,
        "BashOutput" => shell::bash_output(ctx, arguments).await,
        "KillShell" => shell::kill_shell(ctx, arguments).await,
        _ => Err(ProtoError::new(ErrorCode::NotFound, format!("unknown tool `{name}`"))),
    }
}

/// Parses tool arguments; malformed arguments become a result the model sees.
fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolResult> {
    let arguments = if arguments.is_null() { json!({}) } else { arguments };
    serde_json::from_value(arguments).map_err(|err| ToolResult::error(format!("invalid arguments: {err}")))
}

/// Turns a backend error into a model-visible result when the model can act on it.
fn model_error(err: ProtoError) -> Outcome<ToolResult> {
    match err.code {
        ErrorCode::NotFound
        | ErrorCode::Conflict
        | ErrorCode::PreconditionFailed
        | ErrorCode::InvalidParams
        | ErrorCode::Timeout
        | ErrorCode::LimitExceeded => Ok(ToolResult::error(err.message)),
        _ => Err(err),
    }
}

/// Cuts `text` to at most `max` characters, marking the cut.
fn clip_chars(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    match text.char_indices().nth(max) {
        None => std::borrow::Cow::Borrowed(text),
        Some((cut, _)) => std::borrow::Cow::Owned(format!("{}… [line cut]", text.get(..cut).unwrap_or_default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_well_formed() {
        let specs = specs();
        assert_eq!(specs.len(), 9);
        for spec in &specs {
            assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
            assert!(spec.description.len() < 400, "{} description is long", spec.name);
            assert_eq!(spec.annotations.location, ToolLocation::Workspace);
        }
        assert!(annotations_of("Read").unwrap().read_only);
        assert!(!annotations_of("Edit").unwrap().idempotent);
        assert!(annotations_of("Nope").is_none());
    }

    #[test]
    fn clipping() {
        assert_eq!(clip_chars("abc", 3), "abc");
        assert_eq!(clip_chars("abcd", 3), "abc… [line cut]");
        assert_eq!(clip_chars("ééé", 2), "éé… [line cut]");
    }
}
