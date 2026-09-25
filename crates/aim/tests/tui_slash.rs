//! Configured slash commands in the real TUI, including their prompt-history boundary.

mod tui_support;

use serde_json::json;
use tui_support::{Start, Tui, text};

#[test]
fn runtime_commands_execute_each_time_and_route_real_tool_output() {
    let home = tempfile::tempdir().unwrap();
    let settings = json!({"commands": {
        "readprivate": {"description":"Read privately","output":"user","run":["cat","$1"]},
        "reviewfile": {"description":"Review file","output":"agent","tool":{"name":"Read","arguments":{"file_path":"$1"}},
            "text":"Review runtime result:\n{{output}}"},
        "fail": {"description":"Fail","output":"agent","run":["sh","-c","printf FAILED_OUTPUT; exit 7"]},
        "timeout": {"description":"Timeout","output":"agent","run":["sh","-c","sleep 2"],"timeout_ms":100},
        "literal": {"description":"Quote arguments","output":"user","run":["printf","%s","$ARGUMENTS"],"text":"SAFE_RESULT {{output}}"}
    }});
    std::fs::write(home.path().join("tui.json"), settings.to_string()).unwrap();
    let script = json!({"responses": [[text("MODEL ANSWER", 1, 0)]]});
    let tui = Tui::start(&script, &Start { env: &[("AIM_HOME", home.path().to_str().unwrap())], ..Start::default() });
    tui.wait_for("idle");
    for value in ["PRIVATE_VALUE_ONE", "PRIVATE_VALUE_TWO"] {
        std::fs::write(tui.workspace.join("data.txt"), value).unwrap();
        tui.type_text("/readprivate data.txt");
        tui.send("\r");
        tui.wait_for(value);
    }
    tui.type_text("/fail");
    tui.send("\r");
    tui.wait_for("exit code 7");
    tui.type_text("/timeout");
    tui.send("\r");
    tui.wait_for("timed out");
    tui.type_text("/literal literal'; touch injected #");
    tui.send("\r");
    tui.wait_for("SAFE_RESULT literal'; touch injected #");
    assert!(!tui.workspace.join("injected").exists(), "expanded argv must remain literal, not shell code");
    assert!(!home.path().join("history").exists(), "user-only and failed actions never become prompts");
    std::fs::write(tui.workspace.join("data.txt"), "AGENT_FRESH_VALUE").unwrap();
    tui.type_text("/reviewfile data.txt");
    tui.send("\r");
    tui.wait_for("MODEL ANSWER");
    let rows = tui.quit();
    assert!(rows.iter().any(|row| row.contains("AGENT_FRESH_VALUE")));
    let history = std::fs::read_to_string(home.path().join("history")).unwrap();
    assert!(history.contains("Review runtime result:") && history.contains("AGENT_FRESH_VALUE"));
    assert!(!history.contains("PRIVATE_VALUE") && !history.contains("FAILED_OUTPUT"));
}

#[test]
fn cancelling_a_runtime_command_releases_its_real_process() {
    let home = tempfile::tempdir().unwrap();
    let settings = json!({"commands": {
        "slow": {"description":"Slow command","output":"agent","run":["sh","-c","touch started; sleep 2; touch escaped"], "timeout_ms":10000}
    }});
    std::fs::write(home.path().join("tui.json"), settings.to_string()).unwrap();
    let tui = Tui::start(&json!({}), &Start { env: &[("AIM_HOME", home.path().to_str().unwrap())], ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("/slow");
    tui.send("\r");
    tui.wait("the command to start", std::time::Duration::from_secs(5), |_| tui.workspace.join("started").exists());
    tui.send("\x03");
    tui.wait_for("slash command cancelled");
    std::thread::sleep(std::time::Duration::from_millis(2300));
    assert!(!tui.workspace.join("escaped").exists(), "cancellation must kill the process, not merely hide its result");
    assert!(!home.path().join("history").exists());
    tui.quit();
}

#[tokio::test(flavor = "multi_thread")]
async fn status_fetches_each_time_without_any_agent_turn_or_history() {
    use aim_llm_codex::auth::{CredentialStore as _, Credentials, FileCredentialStore, Secret};
    use std::io::{Read as _, Write as _};
    let home = tempfile::tempdir().unwrap();
    FileCredentialStore::new(home.path().join(".aim/auth/codex.json"))
        .save(Credentials {
            access_token: Secret::new("test-only-token"),
            refresh_token: None,
            id_token: None,
            account_id: "test-only-account".into(),
            expires_at: u64::MAX,
        })
        .await
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/backend-api/codex", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for percent in [17, 29] {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            assert!(request.starts_with(b"GET /backend-api/wham/usage HTTP/1.1\r\n"));
            let body = json!({"email":"ACCOUNT_METADATA_NOT_FOR_DISPLAY","rate_limit":{
                "primary_window":{"used_percent":percent,"limit_window_seconds":18000,"reset_at":1_790_000_000}
            }})
            .to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let tui = Tui::start(
        &json!({}),
        &Start {
            env: &[("HOME", home.path().to_str().unwrap()), ("AIM_CODEX_BASE_URL", &base), ("NO_PROXY", "127.0.0.1")],
            ..Start::default()
        },
    );
    tui.wait_for("idle");
    for percent in [17, 29] {
        tui.type_text("/status");
        tui.send("\r");
        tui.wait_for(&format!("{percent}% used"));
    }
    assert!(!tui.dir.join("home/history").exists());
    let rows = tui.quit();
    assert!(!rows.join("\n").contains("ACCOUNT_METADATA_NOT_FOR_DISPLAY"));
    server.join().unwrap();
}

#[test]
fn configured_commands_route_output_in_a_terminal() {
    let home = tempfile::tempdir().unwrap();
    let settings = json!({
        "plain": true,
        "status": [],
        "commands": {
            "private": {"description": "Private checklist", "output": "user", "text": "LOCAL ONLY $ARGUMENTS"},
            "review": {"description": "Review changes", "output": "agent", "text": "Review $ARGUMENTS"}
        }
    });
    std::fs::write(home.path().join("tui.json"), settings.to_string()).unwrap();
    let script = json!({"responses": [[text("FIRST MODEL RESPONSE", 1, 0)]]});
    let tui = Tui::start(&script, &Start { env: &[("AIM_HOME", home.path().to_str().unwrap())], ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("/private my checklist");
    tui.send("\r");
    tui.wait_for("LOCAL ONLY my checklist");
    assert!(!home.path().join("history").exists(), "user-only commands do not create prompt history");
    assert!(!tui.rows().join("\n").contains("FIRST MODEL RESPONSE"), "the user-only command does not start a model turn");
    tui.type_text("/review this change");
    tui.send("\r");
    tui.wait_for("FIRST MODEL RESPONSE");
    let rows = tui.quit();
    assert!(rows.iter().any(|r| r.contains("Review this change")));
    let history = std::fs::read_to_string(home.path().join("history")).unwrap();
    assert!(history.contains("Review this change"));
    assert!(!history.contains("private") && !history.contains("LOCAL ONLY") && !history.contains("/status"));
}
