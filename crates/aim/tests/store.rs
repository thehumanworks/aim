//! Both stores obey the same contract: gap-free append-only logs, round-trips, listing.
#![expect(clippy::unwrap_used, clippy::indexing_slicing, reason = "the shared contract helper is test code")]

use aim::store::{MemoryStore, SessionStore, SqliteStore, StoreError};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::daemon::SessionState;
use aim_proto::event::{EVENT_SCHEMA, EventBody, ForkPoint, SessionEvent, SessionMeta};

fn meta(id: &str, created_ms: i64) -> SessionMeta {
    SessionMeta {
        id: id.into(),
        created_ms,
        workspace: "/w".into(),
        location: "local".into(),
        provider: "codex".into(),
        model: "gpt-6-sol".into(),
        title: None,
        parent: None,
    }
}

fn event(seq: u64, body: EventBody) -> SessionEvent {
    SessionEvent { schema: EVENT_SCHEMA, seq, turn: 1, ts_ms: 7, body }
}

fn timed_event(seq: u64, turn: u64, ts_ms: i64) -> SessionEvent {
    SessionEvent { schema: EVENT_SCHEMA, seq, turn, ts_ms, body: EventBody::TurnStarted }
}

async fn summary_contract(store: &dyn SessionStore) {
    store.create(meta("empty", 10)).await.unwrap();
    store.create(meta("parent", 20)).await.unwrap();
    store.append("parent".into(), vec![timed_event(1, 1, 100), timed_event(2, 2, 200), timed_event(3, 3, 300)]).await.unwrap();

    let mut child = meta("child", 250);
    child.parent = Some(ForkPoint { session: "parent".into(), seq: 2 });
    store.create(child).await.unwrap();
    let mut grandchild = meta("grandchild", 260);
    grandchild.parent = Some(ForkPoint { session: "child".into(), seq: 2 });
    store.create(grandchild).await.unwrap();
    // Activity follows the greatest visible seq, even if wall-clock timestamps regress.
    store.append("child".into(), vec![timed_event(3, 3, 150)]).await.unwrap();

    let summaries = store.summarize(10).await.unwrap();
    let projection: Vec<_> = summaries.iter().map(|s| (s.meta.id.as_str(), s.turns, s.last_activity_ms, s.state)).collect();
    assert_eq!(
        projection,
        [
            ("parent", 3, 300, SessionState::Closed),
            ("grandchild", 2, 200, SessionState::Closed),
            ("child", 3, 150, SessionState::Closed),
            ("empty", 0, 10, SessionState::Closed),
        ]
    );
    assert_eq!(store.summarize(2).await.unwrap().len(), 2);
    assert!(store.summarize(0).await.unwrap().is_empty());
}

async fn contract(store: &dyn SessionStore) {
    store.create(meta("a", 1)).await.unwrap();
    store.create(meta("b", 2)).await.unwrap();
    assert_eq!(store.create(meta("a", 3)).await.unwrap_err(), StoreError::Exists("a".into()));

    let user = Item::User { parts: vec![Part::Text { text: "hi".into() }] };
    store.append("a".into(), vec![event(1, EventBody::TurnStarted), event(2, EventBody::Item { item: user.clone() })]).await.unwrap();
    // Gaps, duplicates and unknown sessions are refused, and nothing partial is written.
    assert!(matches!(
        store.append("a".into(), vec![event(4, EventBody::TurnStarted)]).await.unwrap_err(),
        StoreError::Sequence { expected: 3, got: 4, .. }
    ));
    assert!(matches!(
        store.append("a".into(), vec![event(3, EventBody::TurnStarted), event(3, EventBody::TurnStarted)]).await.unwrap_err(),
        StoreError::Sequence { expected: 4, got: 3, .. }
    ));
    assert_eq!(store.append("zz".into(), vec![event(1, EventBody::TurnStarted)]).await.unwrap_err(), StoreError::NotFound("zz".into()));
    store.append("a".into(), vec![event(3, EventBody::TurnEnded { stop: StopReason::EndTurn })]).await.unwrap();

    let (m, events) = store.load("a".into()).await.unwrap();
    assert_eq!(m, meta("a", 1));
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(events[1].body, EventBody::Item { item: user });

    let listed: Vec<String> = store.list(10).await.unwrap().into_iter().map(|m| m.id).collect();
    assert_eq!(listed, ["b", "a"], "newest first");
}

async fn fork_contract(store: &dyn SessionStore) {
    let mut missing = meta("missing-child", 2);
    missing.parent = Some(ForkPoint { session: "absent".into(), seq: 0 });
    assert!(matches!(store.create(missing).await, Err(StoreError::NotFound(_))));

    store.create(meta("parent", 1)).await.unwrap();
    store
        .append("parent".into(), vec![event(1, EventBody::TurnStarted), event(2, EventBody::TurnStarted), event(3, EventBody::TurnStarted)])
        .await
        .unwrap();
    let mut invalid = meta("invalid-child", 2);
    invalid.parent = Some(ForkPoint { session: "parent".into(), seq: 99 });
    assert!(store.create(invalid).await.is_err());

    let mut child = meta("child", 2);
    child.parent = Some(ForkPoint { session: "parent".into(), seq: 2 });
    store.create(child).await.unwrap();
    assert!(matches!(
        store.append("child".into(), vec![event(1, EventBody::TurnStarted)]).await,
        Err(StoreError::Sequence { expected: 3, .. })
    ));
    store.append("child".into(), vec![event(3, EventBody::TurnEnded { stop: StopReason::EndTurn })]).await.unwrap();

    let mut grandchild = meta("grandchild", 3);
    grandchild.parent = Some(ForkPoint { session: "child".into(), seq: 3 });
    store.create(grandchild).await.unwrap();
    store.append("grandchild".into(), vec![event(4, EventBody::TurnStarted)]).await.unwrap();
    store.append("parent".into(), vec![event(4, EventBody::TurnStarted)]).await.unwrap();
    let (_, history) = store.load("grandchild".into()).await.unwrap();
    assert_eq!(history.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert!(matches!(history[2].body, EventBody::TurnEnded { .. }), "the fork keeps the child event, not the later parent event");
}

#[tokio::test]
async fn memory_store_obeys_the_contract() {
    contract(&MemoryStore::default()).await;
}

#[tokio::test]
async fn memory_store_materializes_nested_forks() {
    fork_contract(&MemoryStore::default()).await;
}

#[tokio::test]
async fn memory_store_summarizes_many_sessions_and_forks() {
    summary_contract(&MemoryStore::default()).await;
}

#[tokio::test]
async fn sqlite_store_summarizes_many_sessions_and_forks() {
    let dir = std::env::temp_dir().join(format!("aim-summary-test-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&dir);
    let store = SqliteStore::open(&dir.join("aim.db")).unwrap();
    summary_contract(&store).await;
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sqlite_store_obeys_the_contract_and_survives_reopening() {
    let dir = std::env::temp_dir().join(format!("aim-store-test-{}", std::process::id()));
    let path = dir.join("aim.db");
    let _stale = std::fs::remove_dir_all(&dir);
    {
        let store = SqliteStore::open(&path).unwrap();
        contract(&store).await;
    }
    let reopened = SqliteStore::open(&path).unwrap();
    let (_, events) = reopened.load("a".into()).await.unwrap();
    assert_eq!(events.len(), 3, "the log is durable");
    reopened.append("a".into(), vec![event(4, EventBody::TurnStarted)]).await.unwrap();
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sqlite_store_materializes_nested_forks() {
    let dir = std::env::temp_dir().join(format!("aim-fork-test-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&dir);
    let store = SqliteStore::open(&dir.join("aim.db")).unwrap();
    fork_contract(&store).await;
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sqlite_store_rejects_out_of_range_event_schema() {
    let dir = std::env::temp_dir().join(format!("aim-schema-test-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&dir);
    let path = dir.join("aim.db");
    let store = SqliteStore::open(&path).unwrap();
    store.create(meta("schema", 1)).await.unwrap();
    store.append("schema".into(), vec![event(1, EventBody::TurnStarted)]).await.unwrap();
    rusqlite::Connection::open(&path).unwrap().execute("UPDATE events SET schema = 70000 WHERE session_id = 'schema'", []).unwrap();
    assert!(matches!(store.load("schema".into()).await, Err(StoreError::Backend(_))));
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn sqlite_files_are_private_under_umask_022() {
    let executable = std::env::current_exe().unwrap();
    let output = std::process::Command::new("sh")
        .args(["-c", "umask 022; exec \"$1\" --exact sqlite_private_mode_child --ignored --nocapture", "sh"])
        .arg(executable)
        .env("AIM_PRIVATE_MODE_CHILD", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "run through sqlite_files_are_private_under_umask_022 with a permissive umask"]
async fn sqlite_private_mode_child() {
    use std::os::unix::fs::PermissionsExt as _;

    if std::env::var_os("AIM_PRIVATE_MODE_CHILD").is_none() {
        return;
    }
    let root = std::env::temp_dir().join(format!("aim-mode-test-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&root);
    for (name, existing) in [("new", false), ("loose", true)] {
        let dir = root.join(name).join(".aim");
        let path = dir.join("aim.db");
        if existing {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, []).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        store.create(meta(name, 1)).await.unwrap();
        store.append(name.into(), vec![event(1, EventBody::TurnStarted)]).await.unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        for extension in ["-wal", "-shm"] {
            let journal = dir.join(format!("aim.db{extension}"));
            assert!(journal.exists(), "the WAL-mode test must exercise {extension}");
            std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o666)).unwrap();
        }
        let _reopened = SqliteStore::open(&path).unwrap();
        for extension in ["-wal", "-shm"] {
            let journal = dir.join(format!("aim.db{extension}"));
            assert_eq!(std::fs::metadata(journal).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
    let _cleanup = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn sqlite_store_refuses_shared_parent_without_changing_its_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = std::env::temp_dir().join(format!("aim-shared-parent-test-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(SqliteStore::open(&root.join("aim.db")).is_err());
    assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o755);
    let _cleanup = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn sqlite_store_refuses_symlinked_database_and_journals() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = std::env::temp_dir().join(format!("aim-symlink-test-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&root);
    let dir = root.join(".aim");
    std::fs::create_dir_all(&dir).unwrap();
    let target = root.join("target");
    std::fs::write(&target, []).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let db = dir.join("aim.db");
    symlink(&target, &db).unwrap();
    assert!(SqliteStore::open(&db).is_err());
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
    std::fs::remove_file(&db).unwrap();
    symlink(&target, dir.join("aim.db-wal")).unwrap();
    assert!(SqliteStore::open(&db).is_err());
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
    let _cleanup = std::fs::remove_dir_all(root);
}
