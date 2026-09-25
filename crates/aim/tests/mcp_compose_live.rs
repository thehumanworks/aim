//! Live native-session composition against the pinned MCP server and OpenRouter.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aim::daemon::client::DaemonClient;
use aim::host::SessionClient as _;
use aim_proto::conversation::Part;
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use futures_util::StreamExt as _;

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY, pinned server-everything, and built aim/aimx binaries"]
#[expect(clippy::too_many_lines, reason = "one live daemon session and its bounded tool-call assertions")]
async fn live_openrouter_composed_mcp_echo_and_board_list() {
    assert!(std::env::var_os("OPENROUTER_API_KEY").is_some(), "OPENROUTER_API_KEY is required");
    let home = tempfile::tempdir().expect("isolated user home");
    let aim_home = home.path().join(".aim");
    std::fs::create_dir(&aim_home).expect("aim home");
    std::fs::set_permissions(&aim_home, std::fs::Permissions::from_mode(0o700)).expect("private aim home");
    let config = aim_home.join("mcp.json");
    std::fs::write(&config, r#"{"mcpServers":{"everything":{"command":"mcp-server-everything","args":["stdio"]}}}"#).expect("MCP config");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("private MCP config");
    let workspace = home.path().join("project");
    std::fs::create_dir(&workspace).expect("workspace");
    let aim = PathBuf::from(env!("CARGO_BIN_EXE_aim"));
    let aimx = aim.with_file_name("aimx");
    assert!(aimx.exists(), "build aimx before the live test");
    let socket = aim_home.join("run/daemon.sock");
    let mut daemon = tokio::process::Command::new(&aim)
        .arg("daemon")
        .arg("--socket")
        .arg(&socket)
        .arg("--idle-exit")
        .arg("120")
        .env("HOME", home.path())
        .env("AIM_HOME", &aim_home)
        .env("AIM_AIMX", &aimx)
        .env("AIM_CODERUN", home.path().join("missing-worker"))
        .kill_on_drop(true)
        .spawn()
        .expect("start aim daemon");
    let client = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(client) = DaemonClient::connect(&socket).await {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("daemon startup deadline");
    let created = client
        .create(SessionSpec {
            workspace: workspace.to_string_lossy().into_owned(),
            location: Location::Local,
            provider: "openrouter".to_owned(),
            model: None,
            effort: None,
            agent: None,
            persistence: Persistence::Persistent,
            code_mode: None,
        })
        .await
        .expect("create native session");
    let cache_dir = aim_home.join("cache/mcp");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if std::fs::read_dir(&cache_dir).is_ok_and(|mut files| files.next().is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("trusted MCP catalog cache appeared");
    let (_, mut stream) = client.attach(created.meta.id.clone()).await.expect("attach session");
    let start = Instant::now();
    client
        .prompt(
            created.meta.id.clone(),
            vec![Part::Text {
                text: "Call mcp__everything__echo directly with message W29-ECHO. Also call board_list with {}. Then answer briefly. Do not use other tools."
                    .to_owned(),
            }],
        )
        .await
        .expect("start OpenRouter turn");
    let updates = tokio::time::timeout(Duration::from_secs(120), async {
        let mut updates = Vec::new();
        while let Some(update) = stream.next().await {
            let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
            updates.push(update);
            if idle {
                break;
            }
        }
        updates
    })
    .await
    .expect("OpenRouter turn deadline");
    let started = updates
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ToolStarted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let finished = updates
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ToolFinished { name, result, .. } => Some((name.as_str(), result.is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(started.contains(&"mcp__everything__echo"), "MCP echo was not called; names: {started:?}");
    assert!(started.contains(&"board_list"), "board_list was not called; names: {started:?}");
    assert!(finished.contains(&("mcp__everything__echo", false)), "MCP echo failed; results: {finished:?}");
    assert!(finished.contains(&("board_list", false)), "board_list failed; results: {finished:?}");
    assert!(start.elapsed() < Duration::from_secs(120));
    client.close(created.meta.id).await.expect("close session");
    daemon.kill().await.expect("stop daemon");
    let _status = daemon.wait().await.expect("reap daemon");
}
