//! Both stores obey the same contract: gap-free append-only logs, round-trips, listing.
#![expect(clippy::unwrap_used, clippy::indexing_slicing, reason = "the shared contract helper is test code")]

use aim::store::{MemoryStore, SessionStore, SqliteStore, StoreError};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent, SessionMeta};

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

#[tokio::test]
async fn memory_store_obeys_the_contract() {
    contract(&MemoryStore::default()).await;
}

#[tokio::test]
async fn sqlite_store_obeys_the_contract_and_survives_reopening() {
    let dir = std::env::temp_dir().join(format!("aim-store-test-{}", std::process::id()));
    let path = dir.join("aim.db");
    let _stale = std::fs::remove_file(&path);
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
