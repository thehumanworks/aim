//! Live conversation-search smoke tests against private persistent and ephemeral aim homes.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aim::agent::ToolHost as _;
use aim::search::SearchEngine;
use aim::search::rerank::JevReranker;
use aim::search::tools::{Reranker, SearchToolHost};
use aim::store::SqliteStore;
use aim_proto::daemon::Persistence;
use aim_proto::ids::IdempotencyKey;

#[expect(clippy::expect_used, reason = "live test fixture must stop if aimx cannot be built")]
fn aimx_binary() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| {
            assert!(Command::new("cargo").args(["build", "-q", "-p", "aimx", "--bin", "aimx"]).status().expect("build aimx").success());
            std::env::current_exe()
                .expect("test executable")
                .parent()
                .expect("deps dir")
                .parent()
                .expect("profile dir")
                .parent()
                .expect("target dir")
                .join("debug/aimx")
        })
        .clone()
}

#[expect(clippy::expect_used, reason = "live fixture setup must stop when a real aim turn fails")]
async fn run_aim(args: &[&str], home: &std::path::Path, workspace: &std::path::Path) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
            .arg("run")
            .arg("--aimx")
            .arg(aimx_binary())
            .arg("-C")
            .arg(workspace)
            .args(args)
            .env("AIM_HOME", home)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("aim run deadline")
    .expect("aim run")
}

fn failure_categories(output: &std::process::Output) -> Vec<&'static str> {
    let body = format!("{} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)).to_ascii_lowercase();
    [
        "401",
        "403",
        "429",
        "unauthorized",
        "invalid",
        "missing",
        "openrouter",
        "aimx",
        "permission",
        "database",
        "model",
        "catalog",
        "timeout",
        "failed",
        "error",
        "tool",
        "provider",
        "rate limit",
        "network",
        "exceeded",
        "protocol",
        "transport",
        "401",
        "402",
        "404",
        "500",
        "502",
        "503",
        "connection",
        "certificate",
        "quota",
    ]
    .into_iter()
    .filter(|category| body.contains(category))
    .collect()
}

#[expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stderr,
    reason = "live benchmark fixture assertions and latency output"
)]
fn benchmark_100k(mut connection: rusqlite::Connection, engine: &SearchEngine, session: &str, workspace: &std::path::Path) {
    // Warm full-pipeline latency at 100,000 synthetic chunks, with FTS5 and a resident 512-D
    // exact vector scan. Fixture construction and model startup are outside the timed region.
    let vectors = (0..512)
        .map(|position| (0..512).flat_map(|index| (if index == position { 1.0_f32 } else { 0.0_f32 }).to_le_bytes()).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let transaction = connection.transaction().expect("benchmark transaction");
    {
        let mut insert = transaction.prepare_cached("INSERT INTO search_chunks(session_id,seq,turn,ts_ms,kind,part,workspace,text,embedding) VALUES(?1,?2,1,1,'user',0,?3,?4,?5)")
            .expect("benchmark insert");
        for index in 0..100_000_i64 {
            let embedding = vectors.get(usize::try_from(index % 512).expect("vector index")).expect("vector");
            insert
                .execute(rusqlite::params![
                    session,
                    1_000_000_i64 + index,
                    workspace.to_string_lossy(),
                    format!("synthetic memory chunk {index}"),
                    embedding
                ])
                .expect("insert benchmark chunk");
        }
    }
    transaction.execute("UPDATE search_meta SET generation=generation+1", []).expect("refresh vector generation");
    transaction.commit().expect("commit benchmark fixture");
    engine.search("orbital cobalt", 8, None).expect("warm search");
    let mut micros = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        let hits = engine.search("orbital cobalt", 8, None).expect("measured search");
        assert!(!hits.is_empty());
        micros.push(started.elapsed().as_micros());
    }
    micros.sort_unstable();
    eprintln!("live_search_100k_p50_us={} p95_us={}", micros[15], micros[28]);
}

#[tokio::test]
#[ignore = "uses real OpenRouter turns, pinned model download and optional Jev"]
async fn live_search_real_sessions_reindex_and_optional_jev() {
    assert!(std::env::var_os("OPENROUTER_API_KEY").is_some(), "OpenRouter key required for live search");
    let dir = tempfile::tempdir().expect("test directory");
    let home = dir.path().join("aim_home");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&home).expect("private home");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("private permissions");
    std::fs::create_dir(&workspace).expect("workspace");
    for marker in ["orbital cobalt lantern", "violet migration compass"] {
        let prompt = format!("Reply with exactly these words and call no tools: {marker}");
        let output =
            run_aim(&["-p", "openrouter", "--model", "openai/gpt-4.1-mini", "--max-requests", "8", &prompt], &home, &workspace).await;
        assert!(output.status.success(), "live aim run failed with {}: {:?}", output.status, failure_categories(&output));
    }
    let private = run_aim(
        &["-p", "openrouter", "--model", "openai/gpt-4.1-mini", "--ephemeral", "Reply exactly: private amber sentinel. Call no tools."],
        &home,
        &workspace,
    )
    .await;
    assert!(private.status.success(), "private aim run failed with {}: {:?}", private.status, failure_categories(&private));

    let started = Instant::now();
    let rebuilt = tokio::time::timeout(
        Duration::from_secs(600),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
            .args(["search-sessions", "--reindex"])
            .env("AIM_HOME", &home)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("reindex deadline")
    .expect("reindex process");
    eprintln!("live_search_reindex_ms={}", started.elapsed().as_millis());
    assert!(rebuilt.status.success(), "reindex exited with {}", rebuilt.status);
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
        .args(["search-sessions", "--json", "orbital cobalt"])
        .env("AIM_HOME", &home)
        .kill_on_drop(true)
        .output()
        .await
        .expect("search process");
    assert!(result.status.success(), "search exited with {}", result.status);
    let hits: Vec<serde_json::Value> =
        String::from_utf8_lossy(&result.stdout).lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    assert!(hits.iter().any(|hit| hit["snippet"].as_str().is_some_and(|snippet| snippet.contains("orbital cobalt"))));
    let session = hits.first().and_then(|hit| hit["session"].as_str()).expect("session hit");

    let store = Arc::new(SqliteStore::open(&home.join("aim.db")).expect("store"));
    let database = home.join("aim.db");
    let engine = Arc::new(tokio::task::spawn_blocking(move || SearchEngine::open(&database)).await.expect("open task").expect("index"));
    let reranker: Option<Arc<dyn Reranker>> = std::env::var_os("TYPESAFE_API_KEY").map(|_| Arc::new(JevReranker) as Arc<dyn Reranker>);
    let host = SearchToolHost::new(Arc::clone(&engine), store, Persistence::Persistent, reranker);
    let search_started = Instant::now();
    let searched = host
        .call("search_sessions".into(), serde_json::json!({"query":"orbital cobalt","limit":8}), IdempotencyKey::new("search-live"))
        .await
        .expect("search tool");
    eprintln!("live_search_tool_ms={}", search_started.elapsed().as_millis());
    assert!(!searched.is_error);
    let read = host
        .call("read_session".into(), serde_json::json!({"session":session,"from_seq":1,"limit":20}), IdempotencyKey::new("read-live"))
        .await
        .expect("read tool");
    assert!(!read.is_error);
    let read_text = serde_json::to_string(&read).expect("read result JSON");
    assert!(read_text.contains("orbital cobalt"));

    let connection = rusqlite::Connection::open(home.join("aim.db")).expect("index connection");
    let private_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM search_chunks WHERE text LIKE '%private amber sentinel%'", [], |row| row.get(0))
        .expect("private exclusion query");
    assert_eq!(private_count, 0);

    benchmark_100k(connection, &engine, session, &workspace);
}
