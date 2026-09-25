//! `tools.list` and every tool through `tools.call`.

use std::time::Duration;

use aim_proto::error::ErrorCode;
use aim_proto::harness::{ExecRead, ExecReadParams, ToolsCall, ToolsCallParams, ToolsList, ToolsListParams};
use aim_proto::ids::{ProcId, WorkspaceId};
use aim_proto::tool::{ToolContent, ToolResult};
use serde_json::{Value, json};

use crate::common::{Client, env, key, session};

async fn call(client: &Client, ws: &WorkspaceId, name: &str, arguments: Value) -> ToolResult {
    let params = ToolsCallParams { workspace: ws.clone(), name: name.into(), arguments, idempotency_key: Some(key()) };
    client.peer.call::<ToolsCall>(params).await.unwrap()
}

fn text(result: &ToolResult) -> &str {
    match result.content.first() {
        Some(ToolContent::Text { text }) => text,
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_are_listed_with_annotations() {
    let env = env().await;
    let (client, _, _) = session(&env).await;
    let listed = client.peer.call::<ToolsList>(ToolsListParams {}).await.unwrap();
    let names: Vec<&str> = listed.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Read", "Write", "Edit", "LS", "Glob", "Grep", "Bash", "BashOutput", "KillShell"]);
    for tool in &listed.tools {
        assert_eq!(tool.input_schema["type"], "object");
        let read_only = ["Read", "LS", "Glob", "Grep", "BashOutput"].contains(&tool.name.as_str());
        assert_eq!(tool.annotations.read_only, read_only, "{}", tool.name);
    }
    let total: usize = listed.tools.iter().map(|t| t.description.len() + t.input_schema.to_string().len()).sum();
    assert!(total < 6000, "tool definitions cost {total} bytes of prompt");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_write_edit_ls() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;

    let wrote = call(&client, &ws, "Write", json!({"file_path": "src/a.txt", "content": "one\ntwo\nthree\n"})).await;
    assert!(!wrote.is_error);
    assert_eq!(text(&wrote), "Created src/a.txt (14 bytes)");

    let read = call(&client, &ws, "Read", json!({"file_path": "src/a.txt"})).await;
    assert_eq!(text(&read), "     1\tone\n     2\ttwo\n     3\tthree");
    let window = call(&client, &ws, "Read", json!({"file_path": "src/a.txt", "offset": 2, "limit": 1})).await;
    assert_eq!(text(&window), "     2\ttwo\n… showing lines 2-2 of 3; pass offset/limit to read more");
    let absolute = env.path("src/a.txt").to_str().unwrap().to_owned();
    assert!(!call(&client, &ws, "Read", json!({"file_path": absolute})).await.is_error);

    let missing = call(&client, &ws, "Read", json!({"file_path": "nope.txt"})).await;
    assert!(missing.is_error);
    let dir = call(&client, &ws, "Read", json!({"file_path": "src"})).await;
    assert!(dir.is_error);
    assert!(text(&dir).contains("is a directory"));
    std::fs::write(env.path("blob"), [0u8, 1, 2, 3]).unwrap();
    let binary = call(&client, &ws, "Read", json!({"file_path": "blob"})).await;
    assert!(binary.is_error && text(&binary).contains("binary"));
    std::fs::write(env.path("pic.png"), b"\x89PNG\r\n\x1a\n0000").unwrap();
    let image = call(&client, &ws, "Read", json!({"file_path": "pic.png"})).await;
    assert!(matches!(&image.content[0], ToolContent::Image { media_type, .. } if media_type == "image/png"));
    let long = "x".repeat(3000);
    std::fs::write(env.path("long"), &long).unwrap();
    let clipped = call(&client, &ws, "Read", json!({"file_path": "long"})).await;
    assert!(text(&clipped).ends_with("… [line cut]"));
    assert!(text(&clipped).len() < 2100);

    let edited = call(&client, &ws, "Edit", json!({"file_path": "src/a.txt", "old_string": "two", "new_string": "2"})).await;
    assert_eq!(text(&edited), "Replaced 1 occurrence in src/a.txt");
    let absent = call(&client, &ws, "Edit", json!({"file_path": "src/a.txt", "old_string": "two", "new_string": "2"})).await;
    assert!(absent.is_error);
    assert_eq!(text(&absent), "old_string not found in src/a.txt");
    std::fs::write(env.path("dup"), "x x x").unwrap();
    let ambiguous = call(&client, &ws, "Edit", json!({"file_path": "dup", "old_string": "x", "new_string": "y"})).await;
    assert!(ambiguous.is_error && text(&ambiguous).contains("occurs 3 times"));
    let all = call(&client, &ws, "Edit", json!({"file_path": "dup", "old_string": "x", "new_string": "y", "replace_all": true})).await;
    assert_eq!(text(&all), "Replaced 3 occurrences in dup");
    assert_eq!(std::fs::read_to_string(env.path("dup")).unwrap(), "y y y");
    let same = call(&client, &ws, "Edit", json!({"file_path": "dup", "old_string": "y", "new_string": "y"})).await;
    assert!(same.is_error);
    let bad = call(&client, &ws, "Edit", json!({"file_path": "dup"})).await;
    assert!(bad.is_error && text(&bad).starts_with("invalid arguments"));
    assert_eq!(std::fs::read_to_string(env.path("src/a.txt")).unwrap(), "one\n2\nthree\n");

    std::os::unix::fs::symlink(env.path("dup"), env.path("link")).unwrap();
    let ls = call(&client, &ws, "LS", json!({})).await;
    assert_eq!(text(&ls), "blob\ndup\nlink@\nlong\npic.png\nsrc/");
    let empty_dir = env.path("empty");
    std::fs::create_dir(&empty_dir).unwrap();
    assert_eq!(text(&call(&client, &ws, "LS", json!({"path": "empty"})).await), "(empty directory)");

    // Escapes are policy errors, not model-visible results.
    let params =
        ToolsCallParams { workspace: ws.clone(), name: "Read".into(), arguments: json!({"file_path": "../x"}), idempotency_key: None };
    assert_eq!(client.peer.call::<ToolsCall>(params).await.unwrap_err().code, ErrorCode::Denied);
    let params = ToolsCallParams { workspace: ws, name: "Nope".into(), arguments: json!({}), idempotency_key: None };
    assert_eq!(client.peer.call::<ToolsCall>(params).await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn glob_and_grep() {
    let env = env().await;
    std::fs::create_dir_all(env.path("src/deep")).unwrap();
    std::fs::write(env.path(".gitignore"), "ignored/\n").unwrap();
    std::fs::create_dir(env.path("ignored")).unwrap();
    std::fs::write(env.path("ignored/x.rs"), "fn needle() {}\n").unwrap();
    std::fs::write(env.path("src/a.rs"), "fn needle() {}\nfn other() {}\n").unwrap();
    std::fs::write(env.path("src/deep/b.rs"), "a\nb\nNeedle here\nc\n").unwrap();
    std::fs::write(env.path("src/c.txt"), "needle in text\n").unwrap();
    let (client, _, ws) = session(&env).await;

    assert_eq!(text(&call(&client, &ws, "Glob", json!({"pattern": "**/*.rs"})).await), "src/a.rs\nsrc/deep/b.rs");
    assert_eq!(text(&call(&client, &ws, "Glob", json!({"pattern": "*.rs", "path": "src"})).await), "src/a.rs");
    assert_eq!(text(&call(&client, &ws, "Glob", json!({"pattern": "*.zig"})).await), "No files found");

    assert_eq!(text(&call(&client, &ws, "Grep", json!({"pattern": "needle"})).await), "src/a.rs\nsrc/c.txt");
    assert_eq!(text(&call(&client, &ws, "Grep", json!({"pattern": "needle", "-i": true})).await), "src/a.rs\nsrc/c.txt\nsrc/deep/b.rs");
    assert_eq!(
        text(&call(&client, &ws, "Grep", json!({"pattern": "needle", "-i": true, "glob": "*.rs", "output_mode": "count"})).await),
        "src/a.rs:1\nsrc/deep/b.rs:1"
    );
    let content = call(&client, &ws, "Grep", json!({"pattern": "Needle", "output_mode": "content", "-C": 1})).await;
    assert_eq!(text(&content), "src/deep/b.rs-2-b\nsrc/deep/b.rs:3:Needle here\nsrc/deep/b.rs-4-c");
    let plain = call(&client, &ws, "Grep", json!({"pattern": "fn", "output_mode": "content", "-n": false})).await;
    assert_eq!(text(&plain), "src/a.rs:fn needle() {}\nsrc/a.rs:fn other() {}");
    let head = call(&client, &ws, "Grep", json!({"pattern": "fn", "output_mode": "content", "head_limit": 1})).await;
    assert_eq!(text(&head), "src/a.rs:1:fn needle() {}\n… 1 more entries (head_limit)");
    assert_eq!(text(&call(&client, &ws, "Grep", json!({"pattern": "zzz"})).await), "No matches found");
    let bad = call(&client, &ws, "Grep", json!({"pattern": "("})).await;
    assert!(bad.is_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_foreground_background_and_handles() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;

    let ok = call(&client, &ws, "Bash", json!({"command": "echo hi; echo err >&2; [[ 1 == 1 ]] && echo bashism"})).await;
    assert!(!ok.is_error);
    // stdout and stderr are separate pipes: both arrive, in no guaranteed relative order.
    let combined = text(&ok);
    assert!(combined.contains("hi\n") && combined.contains("err\n") && combined.contains("bashism\n"), "{combined}");
    let failed = call(&client, &ws, "Bash", json!({"command": "echo nope; exit 4"})).await;
    assert!(failed.is_error);
    assert_eq!(text(&failed), "nope\n[exit code 4]");
    let quiet = call(&client, &ws, "Bash", json!({"command": "true"})).await;
    assert_eq!(text(&quiet), "(no output)");
    let slow = call(&client, &ws, "Bash", json!({"command": "sleep 10", "timeout": 200})).await;
    assert!(slow.is_error);
    assert!(text(&slow).contains("timed out after 200 ms"));
    // Commands run in the workspace root.
    let pwd = call(&client, &ws, "Bash", json!({"command": "pwd"})).await;
    assert!(text(&pwd).trim_end().ends_with("/ws"));

    // Large output keeps head and tail and returns a handle to the rest.
    let big = call(&client, &ws, "Bash", json!({"command": "seq 1 20000"})).await;
    assert!(big.truncated);
    let body = text(&big);
    assert!(body.starts_with("1\n2\n3\n"));
    assert!(body.contains("bytes omitted"));
    assert!(body.contains("20000\n"));
    let handle = big.handle.clone().expect("a handle to the full output");
    let full = client
        .peer
        .call::<ExecRead>(ExecReadParams { proc: ProcId::new(handle.as_str()), after_seq: 0, max_bytes: Some(8 * 1024 * 1024), wait_ms: 0 })
        .await
        .unwrap();
    let all: String = full.chunks.into_iter().map(|c| String::from_utf8(c.data.into_bytes()).unwrap()).collect();
    assert_eq!(all.lines().count(), 20000);

    // Background commands: BashOutput returns new output since the last call; KillShell stops it.
    let started =
        call(&client, &ws, "Bash", json!({"command": "echo first; sleep 0.3; echo second; sleep 30", "run_in_background": true})).await;
    let id = text(&started).split_whitespace().find(|w| w.starts_with('p')).unwrap().trim_end_matches('.').to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let first = call(&client, &ws, "BashOutput", json!({"id": id})).await;
    assert_eq!(text(&first), "first\n[running]");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = call(&client, &ws, "BashOutput", json!({"bash_id": id})).await;
    assert_eq!(text(&second), "second\n[running]");
    let killed = call(&client, &ws, "KillShell", json!({"shell_id": id})).await;
    assert_eq!(text(&killed), format!("Killed {id}"));
    let gone = call(&client, &ws, "BashOutput", json!({"id": id})).await;
    assert!(gone.is_error);

    let done = call(&client, &ws, "Bash", json!({"command": "echo bye", "run_in_background": true})).await;
    let id = text(&done).split_whitespace().find(|w| w.starts_with('p')).unwrap().trim_end_matches('.').to_owned();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(text(&call(&client, &ws, "BashOutput", json!({"id": id})).await), "bye\n[finished] [exit code 0]");
}
