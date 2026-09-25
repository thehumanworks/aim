//! Durable workflow projections in the daemon's `aim.db`.
//!
//! The board remains the authority for board jobs. This store records workflow scheduling and
//! external-call intent. An `in_flight` step survives process death and must be reconciled with
//! its persisted attempt key before a new attempt can start.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS workflow_runs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    manifest_text TEXT NOT NULL,
    manifest_version TEXT NOT NULL,
    params_json TEXT NOT NULL,
    result_json TEXT,
    state TEXT NOT NULL CHECK(state IN ('running','succeeded','failed','cancelled')),
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
    board_run_id TEXT,
    workspace_root TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS workflow_runs_by_time ON workflow_runs(created_ms DESC, id);
CREATE TABLE IF NOT EXISTS workflow_steps (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    name TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','in_flight','succeeded','failed','cancelled')),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt >= 0),
    max_retries INTEGER NOT NULL CHECK(max_retries >= 0),
    attempt_key TEXT,
    result_json TEXT,
    error TEXT,
    board_job_id TEXT,
    agent_session_id TEXT,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    UNIQUE(run_id, name),
    UNIQUE(attempt_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS workflow_steps_by_run ON workflow_steps(run_id, name);
CREATE INDEX IF NOT EXISTS workflow_steps_in_flight ON workflow_steps(run_id, state);
";

/// A workflow persistence failure.
#[derive(Debug)]
pub enum Error {
    /// The addressed run or step does not exist.
    NotFound,
    /// Input cannot be stored as a valid workflow record.
    Invalid(String),
    /// The requested state change is not currently allowed.
    Conflict(String),
    /// The database or filesystem failed.
    Storage(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound => f.write_str("workflow record not found"),
            Self::Invalid(message) => write!(f, "invalid workflow record: {message}"),
            Self::Conflict(message) => write!(f, "workflow conflict: {message}"),
            Self::Storage(message) => write!(f, "workflow storage: {message}"),
        }
    }
}

impl core::error::Error for Error {}

fn storage(err: impl core::fmt::Display) -> Error {
    Error::Storage(err.to_string())
}

/// The durable state of one workflow run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    /// The scheduler may still make progress.
    Running,
    /// All steps completed successfully.
    Succeeded,
    /// The workflow ended with a failed step.
    Failed,
    /// Cancellation completed.
    Cancelled,
}

impl RunState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::Storage(format!("unknown workflow run state {value}"))),
        }
    }
}

/// The durable state of a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    /// No external call is known to have started for the next attempt.
    Pending,
    /// An external call may still be running; reconcile after restart.
    InFlight,
    /// A result is durable and must not be run again.
    Succeeded,
    /// The most recent attempt failed.
    Failed,
    /// Cancellation was confirmed or the pending step was skipped.
    Cancelled,
}

impl StepState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InFlight => "in_flight",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "in_flight" => Ok(Self::InFlight),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::Storage(format!("unknown workflow step state {value}"))),
        }
    }
}

/// One step declared in a manifest snapshot.
#[derive(Clone, Debug)]
pub struct NewStep {
    /// Stable manifest-local name.
    pub name: String,
    /// Maximum retries after the first attempt.
    pub max_retries: u32,
    /// Board job ID, when this is a board-backed step.
    pub board_job_id: Option<String>,
}

/// The data captured when a run starts. The manifest is never re-read on resume.
#[derive(Clone, Debug)]
pub struct NewRun {
    /// Workflow name.
    pub name: String,
    /// Hash of the trusted manifest bytes.
    pub manifest_hash: String,
    /// Exact manifest snapshot used for this run.
    pub manifest_text: String,
    /// Manifest format version.
    pub manifest_version: String,
    /// Bound workflow input parameters.
    pub params: Value,
    /// Declared steps in this run.
    pub steps: Vec<NewStep>,
    /// Board namespace, when one has been created for this run.
    pub board_run_id: Option<String>,
    /// Workspace root captured for restart, without credentials.
    pub workspace_root: String,
    /// Creation timestamp from the caller's clock.
    pub now_ms: i64,
}

/// Stored workflow run snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowRun {
    /// Stable run ID.
    pub id: String,
    /// Workflow name.
    pub name: String,
    /// Hash of the manifest snapshot.
    pub manifest_hash: String,
    /// Exact manifest text.
    pub manifest_text: String,
    /// Manifest format version.
    pub manifest_version: String,
    /// Bound input parameters.
    pub params: Value,
    /// Final result, if any.
    pub result: Option<Value>,
    /// Run lifecycle state.
    pub state: RunState,
    /// Cancellation request persisted before external cancellation effects.
    pub cancel_requested: bool,
    /// Board run namespace, if used.
    pub board_run_id: Option<String>,
    /// Workspace root captured for restart, without credentials.
    pub workspace_root: String,
    /// Creation timestamp.
    pub created_ms: i64,
    /// Last mutation timestamp.
    pub updated_ms: i64,
}

/// Stored workflow step snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowStep {
    /// Stable ID derived from the run ID and manifest-local name.
    pub id: String,
    /// Owning run.
    pub run_id: String,
    /// Manifest-local name.
    pub name: String,
    /// Step lifecycle state.
    pub state: StepState,
    /// Number of attempts whose intent has been committed.
    pub attempt: u32,
    /// Maximum retries after the first attempt.
    pub max_retries: u32,
    /// Stable idempotency key for the current or most recent attempt.
    pub attempt_key: Option<String>,
    /// Result retained after success.
    pub result: Option<Value>,
    /// Error retained after failure.
    pub error: Option<String>,
    /// Board job ID, if used.
    pub board_job_id: Option<String>,
    /// Agent session associated with this step, if any.
    pub agent_session_id: Option<String>,
    /// Creation timestamp.
    pub created_ms: i64,
    /// Last mutation timestamp.
    pub updated_ms: i64,
}

/// Workflow records sharing the daemon's SQLite file.
#[derive(Debug)]
pub struct WorkflowStore {
    conn: Mutex<Connection>,
}

#[derive(Clone, Copy)]
struct StepFinish<'a> {
    run_id: &'a str,
    name: &'a str,
    key: Option<&'a str>,
    state: StepState,
    result: Option<&'a Value>,
    error: Option<&'a str>,
    now_ms: i64,
}

fn hex_digest(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

fn step_id(run_id: &str, name: &str) -> String {
    hex_digest(&[b"aim.workflow.step.v1", run_id.as_bytes(), name.as_bytes()])
}

fn attempt_key(step_id: &str, attempt: u32) -> String {
    hex_digest(&[b"aim.workflow.attempt.v1", step_id.as_bytes(), &attempt.to_be_bytes()])
}

#[cfg(unix)]
fn private_files(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if !meta.file_type().is_dir() => return Err(Error::Storage("state directory is not a regular directory".into())),
            Ok(meta) if meta.permissions().mode() & 0o077 != 0 && dir.file_name() != Some(std::ffi::OsStr::new(".aim")) => {
                return Err(Error::Storage("existing state directory is shared; use a private directory".into()));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(storage(err)),
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(storage)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(storage)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => return Err(Error::Storage("database path is not a regular file".into())),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(storage(err)),
    }
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path).map_err(storage)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(storage)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let journal = Path::new(&name);
        match std::fs::symlink_metadata(journal) {
            Ok(meta) if meta.file_type().is_file() => {
                std::fs::set_permissions(journal, std::fs::Permissions::from_mode(0o600)).map_err(storage)?;
            }
            Ok(_) => return Err(Error::Storage("database journal is not a regular file".into())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(storage(err)),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_files(path: &Path) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(storage)?;
    }
    let _file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(storage)?;
    Ok(())
}

type RunRow = (String, String, String, String, String, String, Option<String>, String, i64, Option<String>, String, i64, i64);
type StepRow =
    (String, String, String, String, i64, i64, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, i64, i64);

fn read_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn decode_run(row: RunRow) -> Result<WorkflowRun, Error> {
    let (
        id,
        name,
        manifest_hash,
        manifest_text,
        manifest_version,
        params,
        result,
        state,
        cancel_requested,
        board_run_id,
        workspace_root,
        created_ms,
        updated_ms,
    ) = row;
    Ok(WorkflowRun {
        id,
        name,
        manifest_hash,
        manifest_text,
        manifest_version,
        params: serde_json::from_str(&params).map_err(storage)?,
        result: result.map(|value| serde_json::from_str(&value).map_err(storage)).transpose()?,
        state: RunState::parse(&state)?,
        cancel_requested: cancel_requested != 0,
        board_run_id,
        workspace_root,
        created_ms,
        updated_ms,
    })
}

fn read_step(row: &rusqlite::Row<'_>) -> rusqlite::Result<StepRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn decode_step(row: StepRow) -> Result<WorkflowStep, Error> {
    let (id, run_id, name, state, attempt, max_retries, attempt_key, result, error, board_job_id, agent_session_id, created_ms, updated_ms) =
        row;
    Ok(WorkflowStep {
        id,
        run_id,
        name,
        state: StepState::parse(&state)?,
        attempt: u32::try_from(attempt).map_err(storage)?,
        max_retries: u32::try_from(max_retries).map_err(storage)?,
        attempt_key,
        result: result.map(|value| serde_json::from_str(&value).map_err(storage)).transpose()?,
        error,
        board_job_id,
        agent_session_id,
        created_ms,
        updated_ms,
    })
}

const RUN_COLUMNS: &str = "id,name,manifest_hash,manifest_text,manifest_version,params_json,result_json,state,cancel_requested,board_run_id,workspace_root,created_ms,updated_ms";
const STEP_COLUMNS: &str =
    "id,run_id,name,state,attempt,max_retries,attempt_key,result_json,error,board_job_id,agent_session_id,created_ms,updated_ms";

fn get_run(conn: &Connection, id: &str) -> Result<Option<WorkflowRun>, Error> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM workflow_runs WHERE id=?1");
    conn.query_row(&sql, [id], read_run).optional().map_err(storage)?.map(decode_run).transpose()
}

fn get_step(conn: &Connection, run_id: &str, name: &str) -> Result<Option<WorkflowStep>, Error> {
    let sql = format!("SELECT {STEP_COLUMNS} FROM workflow_steps WHERE run_id=?1 AND name=?2");
    conn.query_row(&sql, params![run_id, name], read_step).optional().map_err(storage)?.map(decode_step).transpose()
}

impl WorkflowStore {
    /// Opens the shared database, serializing schema creation with session and board stores.
    ///
    /// # Errors
    /// Returns an error when the state path cannot be secured or SQLite setup fails.
    pub fn open(path: &Path) -> Result<Self, Error> {
        private_files(path)?;
        let _schema_lock = crate::store::schema_lock(path).map_err(storage)?;
        let conn = Connection::open(path).map_err(storage)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(storage)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").map_err(storage)?;
        conn.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
        conn.execute_batch(SCHEMA).map_err(storage)?;
        conn.execute_batch("COMMIT").map_err(storage)?;
        private_files(path)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with_conn<T>(&self, work: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        let conn = self.conn.lock().map_err(storage)?;
        work(&conn)
    }

    fn transact<T>(&self, work: impl FnOnce(&Transaction<'_>) -> Result<T, Error>) -> Result<T, Error> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        let value = work(&tx)?;
        tx.commit().map_err(storage)?;
        Ok(value)
    }

    /// Creates a run and all its step records in one transaction.
    ///
    /// # Errors
    /// Returns an error for duplicate or empty step names, invalid metadata, or storage failure.
    pub fn start_run(&self, input: &NewRun) -> Result<WorkflowRun, Error> {
        if input.name.is_empty() || input.manifest_hash.is_empty() || input.manifest_version.is_empty() || input.workspace_root.is_empty() {
            return Err(Error::Invalid("name, manifest hash, version, and workspace root are required".into()));
        }
        if input.steps.is_empty() {
            return Err(Error::Invalid("at least one step is required".into()));
        }
        let id = Uuid::now_v7().to_string();
        let params = serde_json::to_string(&input.params).map_err(storage)?;
        self.transact(|tx| {
            tx.execute(
                "INSERT INTO workflow_runs(id,name,manifest_hash,manifest_text,manifest_version,params_json,state,board_run_id,workspace_root,created_ms,updated_ms) VALUES (?1,?2,?3,?4,?5,?6,'running',?7,?8,?9,?9)",
                params![id, input.name, input.manifest_hash, input.manifest_text, input.manifest_version, params, input.board_run_id, input.workspace_root, input.now_ms],
            ).map_err(storage)?;
            for step in &input.steps {
                if step.name.is_empty() {
                    return Err(Error::Invalid("step name is empty".into()));
                }
                let step_id = step_id(&id, &step.name);
                match tx.execute(
                    "INSERT INTO workflow_steps(id,run_id,name,state,max_retries,board_job_id,created_ms,updated_ms) VALUES (?1,?2,?3,'pending',?4,?5,?6,?6)",
                    params![step_id, id, step.name, step.max_retries, step.board_job_id, input.now_ms],
                ) {
                    Ok(_) => {}
                    Err(rusqlite::Error::SqliteFailure(err, _)) if err.code == rusqlite::ErrorCode::ConstraintViolation => {
                        return Err(Error::Invalid(format!("duplicate step name {}", step.name)));
                    }
                    Err(err) => return Err(storage(err)),
                }
            }
            get_run(tx, &id)?.ok_or(Error::NotFound)
        })
    }

    /// Reads a run by ID.
    ///
    /// # Errors
    /// Returns an error for a database failure or malformed stored record.
    pub fn get_run(&self, id: &str) -> Result<Option<WorkflowRun>, Error> {
        self.with_conn(|conn| get_run(conn, id))
    }

    /// Lists unfinished runs for daemon restart recovery.
    ///
    /// # Errors
    /// Returns an error for a database failure or malformed stored record.
    pub fn list_active_runs(&self) -> Result<Vec<WorkflowRun>, Error> {
        self.with_conn(|conn| {
            let sql = format!("SELECT {RUN_COLUMNS} FROM workflow_runs WHERE state='running' ORDER BY created_ms,id");
            let mut statement = conn.prepare(&sql).map_err(storage)?;
            let rows = statement.query_map([], read_run).map_err(storage)?;
            rows.map(|row| decode_run(row.map_err(storage)?)).collect()
        })
    }

    /// Reads a step by run ID and manifest-local name.
    ///
    /// # Errors
    /// Returns an error for a database failure or malformed stored record.
    pub fn get_step(&self, run_id: &str, name: &str) -> Result<Option<WorkflowStep>, Error> {
        self.with_conn(|conn| get_step(conn, run_id, name))
    }

    /// Lists steps in stable manifest-local name order.
    ///
    /// # Errors
    /// Returns an error for a database failure or malformed stored record.
    pub fn list_steps(&self, run_id: &str) -> Result<Vec<WorkflowStep>, Error> {
        self.list_steps_where(run_id, None)
    }

    /// Lists steps whose external effects may still exist after a crash.
    ///
    /// # Errors
    /// Returns an error for a database failure or malformed stored record.
    pub fn list_in_flight(&self, run_id: &str) -> Result<Vec<WorkflowStep>, Error> {
        self.list_steps_where(run_id, Some(StepState::InFlight))
    }

    /// Computes the stable key for the next pending attempt before the board claim.
    /// Repeated reads return the same key until an attempt starts or the step changes state.
    ///
    /// # Errors
    /// Returns a conflict if the run or step cannot begin a new attempt.
    pub fn planned_attempt_key(&self, run_id: &str, name: &str) -> Result<String, Error> {
        self.with_conn(|conn| {
            let run = get_run(conn, run_id)?.ok_or(Error::NotFound)?;
            let step = get_step(conn, run_id, name)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || run.cancel_requested || step.state != StepState::Pending {
                return Err(Error::Conflict("run is not active or step is not pending".into()));
            }
            let attempt = step.attempt.checked_add(1).ok_or_else(|| Error::Conflict("attempt counter exhausted".into()))?;
            if attempt > step.max_retries.saturating_add(1) {
                return Err(Error::Conflict("retry limit reached".into()));
            }
            Ok(attempt_key(&step.id, attempt))
        })
    }

    fn list_steps_where(&self, run_id: &str, state: Option<StepState>) -> Result<Vec<WorkflowStep>, Error> {
        self.with_conn(|conn| {
            if get_run(conn, run_id)?.is_none() {
                return Err(Error::NotFound);
            }
            let sql = format!("SELECT {STEP_COLUMNS} FROM workflow_steps WHERE run_id=?1 AND (?2 IS NULL OR state=?2) ORDER BY name");
            let mut statement = conn.prepare(&sql).map_err(storage)?;
            let rows = statement.query_map(params![run_id, state.map(StepState::as_str)], read_step).map_err(storage)?;
            rows.map(|row| decode_step(row.map_err(storage)?)).collect()
        })
    }

    /// Commits intent for one attempt before any external effect starts.
    ///
    /// An in-flight replay is rejected. The caller must first inspect and reconcile its stored
    /// attempt key. A succeeded or cancelled step can never start again.
    ///
    /// # Errors
    /// Returns a conflict unless the run is active and the step is pending.
    pub fn start_step(&self, run_id: &str, name: &str, now_ms: i64) -> Result<WorkflowStep, Error> {
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            let step = get_step(tx, run_id, name)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || run.cancel_requested || step.state != StepState::Pending {
                return Err(Error::Conflict("run is not active or step is not pending".into()));
            }
            if run.board_run_id.is_none() || step.board_job_id.is_none() {
                return Err(Error::Conflict("board run and job must be recorded before starting a step".into()));
            }
            let attempt = step.attempt.checked_add(1).ok_or_else(|| Error::Conflict("attempt counter exhausted".into()))?;
            if attempt > step.max_retries.saturating_add(1) {
                return Err(Error::Conflict("retry limit reached".into()));
            }
            let key = attempt_key(&step.id, attempt);
            tx.execute(
                "UPDATE workflow_steps SET state='in_flight',attempt=?1,attempt_key=?2,error=NULL,updated_ms=?3 WHERE id=?4",
                params![attempt, key, now_ms, step.id],
            )
            .map_err(storage)?;
            tx.execute("UPDATE workflow_runs SET updated_ms=?1 WHERE id=?2", params![now_ms, run_id]).map_err(storage)?;
            get_step(tx, run_id, name)?.ok_or(Error::NotFound)
        })
    }

    /// Persists a successful result, fenced to the in-flight attempt key.
    ///
    /// # Errors
    /// Returns a conflict for stale keys or any finished step.
    pub fn succeed_step(&self, run_id: &str, name: &str, key: &str, result: &Value, now_ms: i64) -> Result<WorkflowStep, Error> {
        self.finish_step(&StepFinish {
            run_id,
            name,
            key: Some(key),
            state: StepState::Succeeded,
            result: Some(result),
            error: None,
            now_ms,
        })
    }

    /// Persists an attempt failure, fenced to the in-flight attempt key.
    ///
    /// # Errors
    /// Returns a conflict for stale keys or any finished step.
    pub fn fail_step(&self, run_id: &str, name: &str, key: &str, error: &str, now_ms: i64) -> Result<WorkflowStep, Error> {
        self.finish_step(&StepFinish { run_id, name, key: Some(key), state: StepState::Failed, result: None, error: Some(error), now_ms })
    }

    /// Cancels a pending step, or an in-flight step after its external effect is confirmed stopped.
    /// In-flight cancellation requires the persisted attempt key; it does not itself stop effects.
    ///
    /// # Errors
    /// Returns a conflict for stale keys or a completed step.
    pub fn cancel_step(&self, run_id: &str, name: &str, key: Option<&str>, now_ms: i64) -> Result<WorkflowStep, Error> {
        self.finish_step(&StepFinish { run_id, name, key, state: StepState::Cancelled, result: None, error: None, now_ms })
    }

    fn finish_step(&self, finish: &StepFinish<'_>) -> Result<WorkflowStep, Error> {
        let StepFinish { run_id, name, key, state, result, error, now_ms } = *finish;
        let result_json = result.map(serde_json::to_string).transpose().map_err(storage)?;
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            let step = get_step(tx, run_id, name)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running {
                return Err(Error::Conflict("run is finished".into()));
            }
            let permitted = match (step.state, state) {
                (StepState::Pending, StepState::Cancelled) => key.is_none(),
                (StepState::InFlight, _) => key.is_some_and(|given| step.attempt_key.as_deref() == Some(given)),
                _ => false,
            };
            if !permitted {
                return Err(Error::Conflict("step state or attempt key does not match".into()));
            }
            tx.execute(
                "UPDATE workflow_steps SET state=?1,result_json=?2,error=?3,updated_ms=?4 WHERE id=?5",
                params![state.as_str(), result_json, error, now_ms, step.id],
            )
            .map_err(storage)?;
            tx.execute("UPDATE workflow_runs SET updated_ms=?1 WHERE id=?2", params![now_ms, run_id]).map_err(storage)?;
            get_step(tx, run_id, name)?.ok_or(Error::NotFound)
        })
    }

    /// Moves a failed step back to pending, if it has retry capacity.
    ///
    /// # Errors
    /// Returns a conflict when the run is cancelled or no retry is available.
    pub fn retry_step(&self, run_id: &str, name: &str, now_ms: i64) -> Result<WorkflowStep, Error> {
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            let step = get_step(tx, run_id, name)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || run.cancel_requested || step.state != StepState::Failed || step.attempt > step.max_retries
            {
                return Err(Error::Conflict("step is not retryable".into()));
            }
            tx.execute(
                "UPDATE workflow_steps SET state='pending',attempt_key=NULL,agent_session_id=NULL,error=NULL,updated_ms=?1 WHERE id=?2",
                params![now_ms, step.id],
            )
            .map_err(storage)?;
            get_step(tx, run_id, name)?.ok_or(Error::NotFound)
        })
    }

    /// Persists cancellation intent. In-flight steps remain in flight until reconciled.
    ///
    /// # Errors
    /// Returns an error if the run is absent or already finished.
    pub fn request_cancel(&self, run_id: &str, now_ms: i64) -> Result<WorkflowRun, Error> {
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running {
                return Err(Error::Conflict("run is finished".into()));
            }
            tx.execute("UPDATE workflow_runs SET cancel_requested=1,updated_ms=?1 WHERE id=?2", params![now_ms, run_id])
                .map_err(storage)?;
            tx.execute(
                "UPDATE workflow_steps SET state='cancelled',updated_ms=?1 WHERE run_id=?2 AND state='pending'",
                params![now_ms, run_id],
            )
            .map_err(storage)?;
            get_run(tx, run_id)?.ok_or(Error::NotFound)
        })
    }

    /// Stores the board namespace after the board has created it.
    ///
    /// # Errors
    /// Returns a conflict if a different namespace was already stored.
    pub fn set_board_run_id(&self, run_id: &str, board_run_id: &str, now_ms: i64) -> Result<(), Error> {
        if board_run_id.is_empty() {
            return Err(Error::Invalid("board run ID is empty".into()));
        }
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || run.board_run_id.as_deref().is_some_and(|id| id != board_run_id) {
                return Err(Error::Conflict("board run ID cannot change".into()));
            }
            tx.execute("UPDATE workflow_runs SET board_run_id=?1,updated_ms=?2 WHERE id=?3", params![board_run_id, now_ms, run_id])
                .map_err(storage)?;
            Ok(())
        })
    }

    /// Stores the board job ID after it has been posted.
    ///
    /// # Errors
    /// Returns a conflict if a different job ID was already stored.
    pub fn set_board_job_id(&self, run_id: &str, name: &str, board_job_id: &str, now_ms: i64) -> Result<(), Error> {
        if board_job_id.is_empty() {
            return Err(Error::Invalid("board job ID is empty".into()));
        }
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            let step = get_step(tx, run_id, name)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || step.board_job_id.as_deref().is_some_and(|id| id != board_job_id) {
                return Err(Error::Conflict("board job ID cannot change".into()));
            }
            tx.execute("UPDATE workflow_steps SET board_job_id=?1,updated_ms=?2 WHERE id=?3", params![board_job_id, now_ms, step.id])
                .map_err(storage)?;
            Ok(())
        })
    }

    /// Associates an agent session with a step for restart reconciliation.
    ///
    /// # Errors
    /// Returns a conflict if a different session was already recorded.
    pub fn set_agent_session_id(&self, run_id: &str, name: &str, agent_session_id: &str, now_ms: i64) -> Result<(), Error> {
        if agent_session_id.is_empty() {
            return Err(Error::Invalid("agent session ID is empty".into()));
        }
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            let step = get_step(tx, run_id, name)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running
                || step.state != StepState::InFlight
                || step.agent_session_id.as_deref().is_some_and(|id| id != agent_session_id)
            {
                return Err(Error::Conflict("agent session ID cannot change".into()));
            }
            tx.execute(
                "UPDATE workflow_steps SET agent_session_id=?1,updated_ms=?2 WHERE id=?3",
                params![agent_session_id, now_ms, step.id],
            )
            .map_err(storage)?;
            Ok(())
        })
    }

    /// Marks a run successful only after every step succeeded.
    ///
    /// # Errors
    /// Returns a conflict if any step is unfinished, failed, or cancelled.
    pub fn finish_run(&self, run_id: &str, result: &Value, now_ms: i64) -> Result<WorkflowRun, Error> {
        let json = serde_json::to_string(result).map_err(storage)?;
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || run.cancel_requested {
                return Err(Error::Conflict("run is not active".into()));
            }
            let remaining: i64 = tx
                .query_row("SELECT COUNT(*) FROM workflow_steps WHERE run_id=?1 AND state!='succeeded'", [run_id], |row| row.get(0))
                .map_err(storage)?;
            if remaining != 0 {
                return Err(Error::Conflict("not all steps succeeded".into()));
            }
            tx.execute(
                "UPDATE workflow_runs SET state='succeeded',result_json=?1,updated_ms=?2 WHERE id=?3",
                params![json, now_ms, run_id],
            )
            .map_err(storage)?;
            get_run(tx, run_id)?.ok_or(Error::NotFound)
        })
    }

    /// Marks a run failed after a step exhausts its retry budget.
    ///
    /// # Errors
    /// Returns a conflict while external effects remain in flight.
    pub fn fail_run(&self, run_id: &str, now_ms: i64) -> Result<WorkflowRun, Error> {
        self.end_run(run_id, RunState::Failed, now_ms)
    }

    /// Marks a cancellation complete after all in-flight effects have been reconciled.
    ///
    /// # Errors
    /// Returns a conflict until cancellation was requested and no effects remain in flight.
    pub fn complete_cancel(&self, run_id: &str, now_ms: i64) -> Result<WorkflowRun, Error> {
        self.end_run(run_id, RunState::Cancelled, now_ms)
    }

    fn end_run(&self, run_id: &str, state: RunState, now_ms: i64) -> Result<WorkflowRun, Error> {
        self.transact(|tx| {
            let run = get_run(tx, run_id)?.ok_or(Error::NotFound)?;
            if run.state != RunState::Running || (state == RunState::Cancelled && !run.cancel_requested) {
                return Err(Error::Conflict("run cannot finish in this state".into()));
            }
            let in_flight: i64 = tx
                .query_row("SELECT COUNT(*) FROM workflow_steps WHERE run_id=?1 AND state='in_flight'", [run_id], |row| row.get(0))
                .map_err(storage)?;
            if in_flight != 0 {
                return Err(Error::Conflict("in-flight effects require reconciliation".into()));
            }
            if state == RunState::Failed {
                let failed: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM workflow_steps WHERE run_id=?1 AND state='failed' AND attempt > max_retries",
                        [run_id],
                        |row| row.get(0),
                    )
                    .map_err(storage)?;
                if failed == 0 {
                    return Err(Error::Conflict("no step exhausted its retries".into()));
                }
            }
            tx.execute("UPDATE workflow_runs SET state=?1,updated_ms=?2 WHERE id=?3", params![state.as_str(), now_ms, run_id])
                .map_err(storage)?;
            get_run(tx, run_id)?.ok_or(Error::NotFound)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_run() -> NewRun {
        NewRun {
            name: "deploy".into(),
            manifest_hash: "abc".into(),
            manifest_text: "version = '1'".into(),
            manifest_version: "1".into(),
            params: json!({"target":"staging"}),
            steps: vec![NewStep { name: "build".into(), max_retries: 1, board_job_id: Some("board-job".into()) }],
            board_run_id: Some("board-run".into()),
            workspace_root: "/tmp/project".into(),
            now_ms: 1,
        }
    }

    #[test]
    fn crash_reopen_keeps_in_flight_key_and_fences_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aim").join("aim.db");
        let store = WorkflowStore::open(&path).unwrap();
        let run = store.start_run(&sample_run()).unwrap();
        let planned = store.planned_attempt_key(&run.id, "build").unwrap();
        let step = store.start_step(&run.id, "build", 2).unwrap();
        let key = step.attempt_key.clone().unwrap();
        assert_eq!(key, planned);
        assert!(matches!(store.start_step(&run.id, "build", 3), Err(Error::Conflict(_))));
        drop(store);

        let reopened = WorkflowStore::open(&path).unwrap();
        assert_eq!(reopened.list_active_runs().unwrap()[0].id, run.id);
        assert_eq!(reopened.list_in_flight(&run.id).unwrap()[0].attempt_key.as_deref(), Some(key.as_str()));
        assert!(matches!(reopened.succeed_step(&run.id, "build", "stale", &json!(true), 4), Err(Error::Conflict(_))));
        reopened.succeed_step(&run.id, "build", &key, &json!({"ok":true}), 5).unwrap();
        assert!(matches!(reopened.start_step(&run.id, "build", 6), Err(Error::Conflict(_))));
        assert_eq!(reopened.finish_run(&run.id, &json!({"done":true}), 7).unwrap().result, Some(json!({"done":true})));
    }

    #[test]
    fn retries_change_key_and_stale_completion_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open(&dir.path().join(".aim/aim.db")).unwrap();
        let run = store.start_run(&sample_run()).unwrap();
        let first = store.start_step(&run.id, "build", 2).unwrap();
        let first_key = first.attempt_key.unwrap();
        store.fail_step(&run.id, "build", &first_key, "transient", 3).unwrap();
        store.retry_step(&run.id, "build", 4).unwrap();
        let second = store.start_step(&run.id, "build", 5).unwrap();
        assert_ne!(second.attempt_key.as_deref(), Some(first_key.as_str()));
        assert!(matches!(store.succeed_step(&run.id, "build", &first_key, &json!(1), 6), Err(Error::Conflict(_))));
        let second_key = second.attempt_key.unwrap();
        store.fail_step(&run.id, "build", &second_key, "final", 7).unwrap();
        assert!(matches!(store.retry_step(&run.id, "build", 8), Err(Error::Conflict(_))));
        store.fail_run(&run.id, 9).unwrap();
    }

    #[test]
    fn cancellation_preserves_uncertain_effect_until_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open(&dir.path().join(".aim/aim.db")).unwrap();
        let run = store.start_run(&sample_run()).unwrap();
        let step = store.start_step(&run.id, "build", 2).unwrap();
        store.request_cancel(&run.id, 3).unwrap();
        assert!(matches!(store.complete_cancel(&run.id, 4), Err(Error::Conflict(_))));
        store.cancel_step(&run.id, "build", step.attempt_key.as_deref(), 5).unwrap();
        store.complete_cancel(&run.id, 6).unwrap();
    }

    #[test]
    fn migration_is_additive_and_uses_full_wal_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aim/aim.db");
        let store = WorkflowStore::open(&path).unwrap();
        let conn = store.conn.lock().unwrap();
        assert_eq!(conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0)).unwrap(), "wal");
        assert_eq!(conn.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
        assert_eq!(conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    }
}
