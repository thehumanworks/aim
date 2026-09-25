//! Durable board ledger on the daemon's SQLite file.
//!
//! The session store owns a separate connection and thread. SQLite WAL admits both actors, and
//! `BEGIN IMMEDIATE` serializes board writers with any other writer. No external effect runs while
//! a transaction is open; each board operation commits its projection and outbox event together.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use aim_proto::board::BoardEvent;
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior};
use tokio::sync::{broadcast, oneshot};

use super::Error;

type Task = Box<dyn FnOnce(&mut Connection, &broadcast::Sender<BoardEvent>) + Send>;
const QUEUE_CAPACITY: usize = 256;
const BOARD_SCHEMA_VERSION: i64 = 1;
const INTEGRATION_SCHEMA_VERSION: i64 = 1;

/// One board actor with one SQLite connection, sharing the session store's WAL file.
#[derive(Clone)]
pub(super) struct Ledger {
    tx: mpsc::SyncSender<Task>,
}

impl core::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BoardLedger")
    }
}

fn backend(err: impl core::fmt::Display) -> Error {
    Error::Storage(err.to_string())
}

fn committed_events(conn: &Connection, after: i64) -> Result<Vec<BoardEvent>, Error> {
    let mut statement = conn
        .prepare("SELECT seq,event_id,run_id,job_id,job_version,kind,created_ms FROM board_outbox WHERE seq>?1 ORDER BY seq")
        .map_err(backend)?;
    let rows = statement
        .query_map([after], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(backend)?;
    rows.map(|row| {
        let (seq, id, run_id, job_id, version, kind, ts_ms) = row.map_err(backend)?;
        Ok(BoardEvent {
            id,
            run_id,
            job_id: job_id.unwrap_or_default(),
            seq: u64::try_from(seq).map_err(backend)?,
            version: u64::try_from(version.unwrap_or_default()).map_err(backend)?,
            kind,
            ts_ms: u64::try_from(ts_ms).map_err(backend)?,
        })
    })
    .collect()
}

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = FULL;
    PRAGMA foreign_keys = ON;
    CREATE TABLE IF NOT EXISTS board_schema (
        version INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS board_integration_schema (
        version INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS board_runs (
        id TEXT PRIMARY KEY,
        owner_session TEXT NOT NULL,
        workspace TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS board_workers (
        worker TEXT PRIMARY KEY,
        capacity INTEGER NOT NULL CHECK(capacity BETWEEN 1 AND 64)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS board_jobs (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL REFERENCES board_runs(id),
        title TEXT NOT NULL,
        contract_json TEXT NOT NULL,
        post_key TEXT NOT NULL DEFAULT '' UNIQUE,
        state TEXT NOT NULL,
        review_state TEXT NOT NULL DEFAULT 'pending',
        generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
        claim_id INTEGER NOT NULL DEFAULT 0 CHECK(claim_id >= 0),
        lease_until_ms INTEGER NOT NULL DEFAULT 0,
        retries INTEGER NOT NULL DEFAULT 0 CHECK(retries >= 0),
        max_retries INTEGER NOT NULL DEFAULT 0 CHECK(max_retries >= 0),
        assignee TEXT,
        version INTEGER NOT NULL DEFAULT 1 CHECK(version > 0),
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        CHECK(retries <= max_retries)
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS board_jobs_by_run ON board_jobs(run_id, created_ms, id);
    CREATE TABLE IF NOT EXISTS board_dependencies (
        job_id TEXT NOT NULL REFERENCES board_jobs(id),
        dependency_id TEXT NOT NULL REFERENCES board_jobs(id),
        PRIMARY KEY(job_id, dependency_id),
        CHECK(job_id <> dependency_id)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS board_attempts (
        id TEXT PRIMARY KEY,
        job_id TEXT NOT NULL REFERENCES board_jobs(id),
        generation INTEGER NOT NULL CHECK(generation >= 0),
        assignee TEXT NOT NULL,
        claim_id INTEGER NOT NULL CHECK(claim_id > 0),
        token_hash BLOB NOT NULL CHECK(length(token_hash) = 32),
        claim_key TEXT NOT NULL DEFAULT '',
        lease_until_ms INTEGER NOT NULL,
        heartbeat_seq INTEGER NOT NULL DEFAULT 0 CHECK(heartbeat_seq >= 0),
        state TEXT NOT NULL,
        launch_intent INTEGER NOT NULL DEFAULT 0 CHECK(launch_intent IN (0, 1)),
        cleanup_state TEXT NOT NULL DEFAULT 'unknown',
        started_ms INTEGER,
        ended_ms INTEGER,
        failure_reason TEXT,
        UNIQUE(job_id, generation),
        UNIQUE(job_id, claim_id)
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS board_attempts_by_job ON board_attempts(job_id, generation DESC);
    CREATE INDEX IF NOT EXISTS board_attempts_by_worker_hold ON board_attempts(assignee, job_id, generation)
        WHERE cleanup_state != 'confirmed';
    CREATE TABLE IF NOT EXISTS board_messages (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL REFERENCES board_runs(id),
        job_id TEXT NOT NULL REFERENCES board_jobs(id),
        attempt_id TEXT REFERENCES board_attempts(id),
        sender TEXT NOT NULL,
        recipient TEXT NOT NULL,
        kind TEXT NOT NULL,
        body TEXT NOT NULL CHECK(length(body) <= 65536),
        idempotency_key TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS board_messages_by_job ON board_messages(job_id, created_ms, id);
    CREATE UNIQUE INDEX IF NOT EXISTS board_messages_by_key ON board_messages(job_id, idempotency_key);
    CREATE TABLE IF NOT EXISTS board_artifacts (
        id TEXT PRIMARY KEY,
        job_id TEXT NOT NULL REFERENCES board_jobs(id),
        attempt_id TEXT NOT NULL REFERENCES board_attempts(id),
        uri TEXT NOT NULL,
        media_type TEXT NOT NULL,
        byte_size INTEGER NOT NULL CHECK(byte_size >= 0),
        sha256 BLOB NOT NULL CHECK(length(sha256) = 32),
        data BLOB NOT NULL,
        created_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS board_artifacts_by_job ON board_artifacts(job_id, attempt_id);
    CREATE TRIGGER IF NOT EXISTS board_artifacts_no_update BEFORE UPDATE ON board_artifacts
        BEGIN SELECT RAISE(ABORT, 'board artifacts are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS board_artifacts_no_delete BEFORE DELETE ON board_artifacts
        BEGIN SELECT RAISE(ABORT, 'board artifacts are immutable'); END;
    CREATE TABLE IF NOT EXISTS board_reviews (
        id TEXT PRIMARY KEY,
        job_id TEXT NOT NULL REFERENCES board_jobs(id),
        attempt_id TEXT NOT NULL REFERENCES board_attempts(id),
        reviewer TEXT NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('accepted', 'rejected')),
        evidence_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS board_reviews_by_job ON board_reviews(job_id, created_ms);
    CREATE TABLE IF NOT EXISTS board_outbox (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        event_id TEXT NOT NULL UNIQUE,
        run_id TEXT NOT NULL REFERENCES board_runs(id),
        job_id TEXT REFERENCES board_jobs(id),
        job_version INTEGER,
        kind TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        delivery_state TEXT NOT NULL DEFAULT 'pending',
        attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0)
    );
    CREATE INDEX IF NOT EXISTS board_outbox_by_run ON board_outbox(run_id, seq);
    CREATE TABLE IF NOT EXISTS board_integrations (
        job_id TEXT PRIMARY KEY REFERENCES board_jobs(id),
        attempt_id TEXT NOT NULL REFERENCES board_attempts(id),
        source_branch TEXT NOT NULL,
        target_branch TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('queued','integrating','integrated','conflict','failed')),
        target_head TEXT,
        source_commit TEXT,
        scratch_path TEXT,
        result_commit TEXT,
        rescue_ref TEXT,
        conflicts_json TEXT NOT NULL DEFAULT '[]',
        artifact_id TEXT,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS board_integrations_by_state ON board_integrations(state, created_ms);
    CREATE TABLE IF NOT EXISTS board_cleanup_receipts (
        attempt_id TEXT PRIMARY KEY REFERENCES board_attempts(id),
        job_id TEXT NOT NULL REFERENCES board_jobs(id),
        receipt_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
";

// Integration schema is versioned separately so a FIX15 binary still sees board_schema version 1
// and can read the expanded rows. The schema lock spans both additive migrations.
fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
    let version: Option<i64> =
        tx.query_row("SELECT version FROM board_schema LIMIT 1", [], |row| row.get(0)).optional().map_err(backend)?;
    match version {
        Some(BOARD_SCHEMA_VERSION) => {}
        None => {
            let mut columns = tx.prepare("PRAGMA table_info(board_attempts)").map_err(backend)?;
            let names =
                columns.query_map([], |row| row.get::<_, String>(1)).map_err(backend)?.collect::<Result<Vec<_>, _>>().map_err(backend)?;
            drop(columns);
            if !names.iter().any(|name| name == "claim_key") {
                tx.execute_batch("ALTER TABLE board_attempts ADD COLUMN claim_key TEXT NOT NULL DEFAULT ''").map_err(backend)?;
            }
            tx.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS board_attempts_by_claim_key ON board_attempts(claim_key) WHERE claim_key != ''",
            )
            .map_err(backend)?;
            tx.execute("INSERT INTO board_schema (version) VALUES (?1)", [BOARD_SCHEMA_VERSION]).map_err(backend)?;
        }
        Some(other) => return Err(Error::Storage(format!("unsupported board schema version {other}"))),
    }
    let integration_version: Option<i64> =
        tx.query_row("SELECT version FROM board_integration_schema LIMIT 1", [], |row| row.get(0)).optional().map_err(backend)?;
    match integration_version {
        Some(INTEGRATION_SCHEMA_VERSION) => {}
        None => {
            let mut columns = tx.prepare("PRAGMA table_info(board_integrations)").map_err(backend)?;
            let names =
                columns.query_map([], |row| row.get::<_, String>(1)).map_err(backend)?.collect::<Result<Vec<_>, _>>().map_err(backend)?;
            drop(columns);
            for (name, statement) in [
                ("scratch_path", "ALTER TABLE board_integrations ADD COLUMN scratch_path TEXT"),
                ("result_commit", "ALTER TABLE board_integrations ADD COLUMN result_commit TEXT"),
                ("rescue_ref", "ALTER TABLE board_integrations ADD COLUMN rescue_ref TEXT"),
            ] {
                if !names.iter().any(|column| column == name) {
                    tx.execute_batch(statement).map_err(backend)?;
                }
            }
            tx.execute("INSERT INTO board_integration_schema (version) VALUES (?1)", [INTEGRATION_SCHEMA_VERSION]).map_err(backend)?;
        }
        Some(other) => return Err(Error::Storage(format!("unsupported integration schema version {other}"))),
    }
    tx.commit().map_err(backend)
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
            Err(err) => return Err(backend(err)),
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(backend)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(backend)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => return Err(Error::Storage("database path is not a regular file".into())),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(backend(err)),
    }
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path).map_err(backend)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(backend)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let journal = Path::new(&name);
        match std::fs::symlink_metadata(journal) {
            Ok(meta) if meta.file_type().is_file() => {
                std::fs::set_permissions(journal, std::fs::Permissions::from_mode(0o600)).map_err(backend)?;
            }
            Ok(_) => return Err(Error::Storage("database journal is not a regular file".into())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(backend(err)),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_files(path: &Path) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(backend)?;
    }
    let _file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(backend)?;
    Ok(())
}

impl Ledger {
    #[cfg(test)]
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        let (events, _) = broadcast::channel(256);
        Self::open_with_events(path, events)
    }

    pub(super) fn open_with_events(path: &Path, events: broadcast::Sender<BoardEvent>) -> Result<Self, Error> {
        private_files(path)?;
        let _schema_lock = crate::store::schema_lock(path).map_err(backend)?;
        let mut conn = Connection::open(path).map_err(backend)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        conn.execute_batch(SCHEMA).map_err(backend)?;
        migrate(&mut conn)?;
        private_files(path)?;
        let (tx, rx) = mpsc::sync_channel::<Task>(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("aim-board-db".into())
            .spawn(move || {
                let mut conn = conn;
                while let Ok(task) = rx.recv() {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(&mut conn, &events))).is_err() {
                        tracing::error!("board database task panicked; transaction rolled back");
                    }
                }
            })
            .map_err(backend)?;
        Ok(Self { tx })
    }

    pub(super) async fn transact<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .try_send(Box::new(move |conn, events| {
                let result = (|| {
                    let before: i64 =
                        conn.query_row("SELECT COALESCE(MAX(seq),0) FROM board_outbox", [], |row| row.get(0)).map_err(backend)?;
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
                    let value = work(&tx)?;
                    tx.commit().map_err(backend)?;
                    match committed_events(conn, before) {
                        Ok(committed) => {
                            for event in committed {
                                drop(events.send(event));
                            }
                        }
                        Err(err) => tracing::warn!("board outbox hints unavailable after commit: {err}"),
                    }
                    Ok(value)
                })();
                drop(reply.send(result));
            }))
            .map_err(|err| match err {
                mpsc::TrySendError::Full(_) => Error::Storage("board database queue full".into()),
                mpsc::TrySendError::Disconnected(_) => Error::Storage("board database thread stopped".into()),
            })?;
        answer.await.map_err(|_| Error::Storage("board database thread dropped the request".into()))?
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use rusqlite::params;

    #[test]
    fn board_connection_uses_full_wal_sync() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        let mode: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, 2);
    }

    #[test]
    fn concurrent_board_opens_upgrade_claim_key_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".aim");
        std::fs::create_dir(&home).unwrap();
        let path = home.join("aim.db");
        let conn = Connection::open(&path).unwrap();
        let legacy = SCHEMA.replace("claim_key TEXT NOT NULL DEFAULT '',", "");
        conn.execute_batch(&legacy).unwrap();
        drop(conn);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Ledger::open(&path)
                })
            })
            .collect();
        barrier.wait();
        for worker in workers {
            assert!(worker.join().unwrap().is_ok(), "concurrent board startup failed to upgrade the shared database");
        }
        let conn = Connection::open(path).unwrap();
        let mut columns = conn.prepare("PRAGMA table_info(board_attempts)").unwrap();
        let names = columns.query_map([], |row| row.get::<_, String>(1)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(names.iter().filter(|name| *name == "claim_key").count(), 1);
    }

    #[tokio::test]
    async fn additive_schema_version_and_actor_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aim/aim.db");
        let ledger = Ledger::open(&path).unwrap();
        let version = ledger
            .transact(|tx| tx.query_row("SELECT version FROM board_schema", [], |row| row.get::<_, i64>(0)).map_err(backend))
            .await
            .unwrap();
        assert_eq!(version, BOARD_SCHEMA_VERSION);
        let failed: Result<(), Error> = ledger.transact(|_| panic!("injected actor task panic")).await;
        assert!(failed.is_err());
        let alive = ledger.transact(|tx| tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)).map_err(backend)).await.unwrap();
        assert_eq!(alive, 1);
    }

    #[test]
    fn integration_columns_upgrade_existing_board_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aim/aim.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE board_schema (version INTEGER NOT NULL); \
             INSERT INTO board_schema VALUES (1); \
             CREATE TABLE board_integrations ( \
                 job_id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL, \
                 source_branch TEXT NOT NULL, target_branch TEXT NOT NULL, \
                 state TEXT NOT NULL, target_head TEXT, source_commit TEXT, \
                 conflicts_json TEXT NOT NULL DEFAULT '[]', artifact_id TEXT, \
                 created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL \
             ) WITHOUT ROWID;",
        )
        .unwrap();
        drop(old);
        Ledger::open(&path).unwrap();
        let upgraded = Connection::open(path).unwrap();
        let version: i64 = upgraded.query_row("SELECT version FROM board_schema", [], |row| row.get(0)).unwrap();
        let integration_version: i64 = upgraded.query_row("SELECT version FROM board_integration_schema", [], |row| row.get(0)).unwrap();
        assert_eq!((version, integration_version), (1, 1));
        let mut statement = upgraded.prepare("PRAGMA table_info(board_integrations)").unwrap();
        let columns = statement.query_map([], |row| row.get::<_, String>(1)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        for column in ["scratch_path", "result_commit", "rescue_ref"] {
            assert_eq!(columns.iter().filter(|name| *name == column).count(), 1);
        }
    }

    #[tokio::test]
    async fn state_and_outbox_commit_or_rollback_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aim/aim.db");
        let ledger = Ledger::open(&path).unwrap();
        ledger
            .transact(|tx| {
                tx.execute(
                    "INSERT INTO board_runs (id, owner_session, workspace, created_ms) VALUES (?1, ?2, ?3, 1)",
                    params!["r", "owner", "/workspace"],
                )
                .map_err(backend)?;
                Ok(())
            })
            .await
            .unwrap();
        let failed: Result<(), Error> = ledger
            .transact(|tx| {
                tx.execute(
                    "INSERT INTO board_jobs (id, run_id, title, contract_json, state, created_ms, updated_ms) VALUES ('j', 'r', 'job', '{}', 'posted', 1, 1)",
                    [],
                )
                .map_err(backend)?;
                Err(Error::Invalid("injected failure before event".into()))
            })
            .await;
        assert!(failed.is_err());
        ledger
            .transact(|tx| {
                let n: i64 = tx.query_row("SELECT count(*) FROM board_jobs", [], |row| row.get(0)).map_err(backend)?;
                assert_eq!(n, 0);
                tx.execute(
                    "INSERT INTO board_jobs (id, run_id, title, contract_json, state, created_ms, updated_ms) VALUES ('j', 'r', 'job', '{}', 'posted', 1, 1)",
                    [],
                )
                .map_err(backend)?;
                tx.execute(
                    "INSERT INTO board_outbox (event_id, run_id, job_id, job_version, kind, payload_json, created_ms) VALUES ('e', 'r', 'j', 1, 'posted', '{}', 1)",
                    [],
                )
                .map_err(backend)?;
                Ok(())
            })
            .await
            .unwrap();
        drop(ledger);
        let reopened = Ledger::open(&path).unwrap();
        reopened
            .transact(|tx| {
                let jobs: i64 = tx.query_row("SELECT count(*) FROM board_jobs", [], |row| row.get(0)).map_err(backend)?;
                let events: i64 = tx.query_row("SELECT count(*) FROM board_outbox", [], |row| row.get(0)).map_err(backend)?;
                assert_eq!((jobs, events), (1, 1));
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn attempt_generation_is_unique_and_token_hash_is_fixed_length() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join(".aim/aim.db")).unwrap();
        ledger
            .transact(|tx| {
                tx.execute("INSERT INTO board_runs (id, owner_session, workspace, created_ms) VALUES ('r','s','/w',1)", []).map_err(backend)?;
                tx.execute("INSERT INTO board_jobs (id, run_id, title, contract_json, state, created_ms, updated_ms) VALUES ('j','r','j','{}','posted',1,1)", []).map_err(backend)?;
                let insert = "INSERT INTO board_attempts (id, job_id, generation, assignee, claim_id, token_hash, lease_until_ms, state) VALUES (?1,'j',1,'w',?2,?3,99,'claimed')";
                tx.execute(insert, params!["a", 1, [7_u8; 32].as_slice()]).map_err(backend)?;
                assert!(tx.execute(insert, params!["b", 2, [8_u8; 32].as_slice()]).is_err());
                assert!(tx.execute("INSERT INTO board_attempts (id, job_id, generation, assignee, claim_id, token_hash, lease_until_ms, state) VALUES ('c','j',2,'w',3,x'00',99,'claimed')", []).is_err());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".aim");
        let path = home.join("aim.db");
        let _ledger = Ledger::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
