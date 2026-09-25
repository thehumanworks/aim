//! The ACP → session-update bridge, offline, plus one live Claude Code session through the host.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use aim::acp::{Bridge, with_acp_at};
use aim::host::{BackendFactory, BackendRequest, HostConfig, SessionClient, SessionHost};
use aim::store::MemoryStore;
use aim_acp::{AcpEvent, Chunk, ContentPart, ToolCallState, ToolCallStatus, TurnEnd, Update};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::tool::ToolResult;
use futures_util::StreamExt as _;
use serde_json::{Value, json};

fn update(update: Update) -> AcpEvent {
    AcpEvent::Update { update, raw: Value::Null }
}

fn text(t: &str) -> AcpEvent {
    update(Update::AgentMessage(Chunk { message_id: None, content: ContentPart::Text { text: t.into() } }))
}

fn tool(id: &str, status: ToolCallStatus) -> AcpEvent {
    let mut call = ToolCallState::new(id);
    call.name = Some("Read".into());
    call.status = status;
    call.raw_input = Some(json!({"file_path": "a.txt"}));
    update(Update::ToolCall(call))
}

fn stopped(stop: StopReason) -> AcpEvent {
    AcpEvent::Stopped(TurnEnd {
        stop,
        acp_stop_reason: "end_turn".into(),
        usage: Some(Usage { input_tokens: 7, output_tokens: 3, ..Usage::default() }),
        raw: Value::Null,
    })
}

#[expect(clippy::expect_used, reason = "live fixture setup must stop if the aimx binary cannot be built")]
fn aimx_binary() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| {
            assert!(Command::new("cargo").args(["build", "-q", "-p", "aimx", "--bin", "aimx"]).status().expect("build aimx").success());
            std::env::current_exe().expect("test executable").parent().expect("deps dir").parent().expect("target dir").join("aimx")
        })
        .clone()
}

struct Sshd {
    dir: tempfile::TempDir,
    child: Child,
    config: PathBuf,
}

impl Sshd {
    #[expect(clippy::expect_used, reason = "live fixture setup must stop if the private sshd cannot start")]
    fn start() -> Self {
        let dir = tempfile::Builder::new().prefix("aim-acp-ssh").tempdir_in("/private/tmp").expect("sshd tempdir");
        let host = dir.path().join("host");
        let client = dir.path().join("client");
        for key in [&host, &client] {
            assert!(
                Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(key).status().expect("ssh-keygen").success()
            );
        }
        std::fs::copy(client.with_extension("pub"), dir.path().join("authorized_keys")).expect("authorized keys");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ssh port");
        let port = listener.local_addr().expect("listener address").port();
        drop(listener);
        let remote_home = dir.path().join("remote_home");
        std::fs::create_dir(&remote_home).expect("remote home");
        let sshd_config = dir.path().join("sshd_config");
        std::fs::write(&sshd_config, format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\nPidFile {}\nSetEnv HOME={} PATH=/usr/bin:/bin:/usr/sbin:/sbin\n",
            host.display(), dir.path().join("authorized_keys").display(), dir.path().join("sshd.pid").display(), remote_home.display()
        )).expect("sshd config");
        let host_key = std::fs::read_to_string(host.with_extension("pub")).expect("host public key");
        std::fs::write(dir.path().join("known_hosts"), format!("[127.0.0.1]:{port} {host_key}")).expect("known hosts");
        let config = dir.path().join("ssh_config");
        std::fs::write(&config, format!(
            "Host aim-acp-test\n  HostName 127.0.0.1\n  Port {port}\n  User {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  LogLevel ERROR\n",
            std::env::var("USER").expect("user"), client.display(), dir.path().join("known_hosts").display()
        )).expect("SSH client config");
        let check = Command::new("/usr/sbin/sshd").args(["-t", "-f"]).arg(&sshd_config).output().expect("sshd config check");
        assert!(check.status.success());
        let child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sshd");
        let mut ready = false;
        for _ in 0..30 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(ready, "private sshd did not start");
        Self { dir, child, config }
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn run(bridge: &mut Bridge, events: Vec<AcpEvent>) -> (Vec<SessionUpdate>, Option<TurnEnd>) {
    let mut all = Vec::new();
    let mut end = None;
    for event in events {
        let (updates, stop) = bridge.accept(event);
        all.extend(updates);
        end = end.or(stop);
    }
    (all, end)
}

#[test]
fn a_tool_call_starts_once_finishes_once_and_its_items_follow() {
    let mut bridge = Bridge::default();
    let call = Item::ToolCall { call_id: "t1".into(), name: "Read".into(), arguments: r#"{"file_path":"a.txt"}"#.into(), native: None };
    let result = Item::ToolResult { call_id: "t1".into(), result: ToolResult::text("hello") };
    let (updates, end) = run(
        &mut bridge,
        vec![
            tool("t1", ToolCallStatus::Pending),
            tool("t1", ToolCallStatus::InProgress),
            tool("t1", ToolCallStatus::Completed),
            AcpEvent::Item { item: call.clone() },
            AcpEvent::Item { item: result.clone() },
            text("It says hello."),
            stopped(StopReason::EndTurn),
        ],
    );
    let started = updates.iter().filter(|u| matches!(u, SessionUpdate::ToolStarted { .. })).count();
    let finished: Vec<&SessionUpdate> = updates.iter().filter(|u| matches!(u, SessionUpdate::ToolFinished { .. })).collect();
    assert_eq!(started, 1);
    assert_eq!(
        finished,
        [&SessionUpdate::ToolFinished { call_id: "t1".into(), name: "Read".into(), result: ToolResult::text("hello"), parent: None }]
    );
    let position = |wanted: &SessionUpdate| updates.iter().position(|u| u == wanted).unwrap();
    assert!(position(&SessionUpdate::ItemAdded { item: call }) < position(finished[0]));
    assert!(position(finished[0]) < position(&SessionUpdate::ItemAdded { item: result }));
    assert!(updates.contains(&SessionUpdate::TextDelta { delta: "It says hello.".into() }));
    assert!(updates.iter().any(|u| matches!(u, SessionUpdate::Usage { usage } if usage.input_tokens == 7)));
    assert_eq!(end.map(|e| e.stop), Some(StopReason::EndTurn));
    assert!(bridge.settle().is_empty(), "nothing left open");
}

#[test]
fn a_call_starts_when_its_input_is_known_not_when_it_is_announced() {
    let mut bridge = Bridge::default();
    let mut announced = ToolCallState::new("t1");
    announced.name = Some("mcp__aim__read".into());
    announced.raw_input = Some(json!({}));
    let (updates, _) = run(&mut bridge, vec![update(Update::ToolCall(announced.clone()))]);
    assert!(updates.is_empty(), "an empty input is not known yet");
    announced.status = ToolCallStatus::InProgress;
    announced.raw_input = Some(json!({"file_path": "a.txt"}));
    let (updates, _) = run(&mut bridge, vec![update(Update::ToolCall(announced))]);
    assert_eq!(
        updates,
        [SessionUpdate::ToolStarted {
            call_id: "t1".into(),
            name: "mcp__aim__read".into(),
            arguments: r#"{"file_path":"a.txt"}"#.into(),
            parent: None
        }]
    );
}

#[test]
fn calls_left_open_by_the_stop_are_settled_with_a_failed_result() {
    let mut bridge = Bridge::default();
    let (_, end) = run(&mut bridge, vec![tool("t1", ToolCallStatus::InProgress), stopped(StopReason::Cancelled)]);
    assert_eq!(end.map(|e| e.stop), Some(StopReason::Cancelled));
    let settled = bridge.settle();
    assert!(matches!(settled.first(), Some(SessionUpdate::ToolFinished { call_id, result, .. }) if call_id == "t1" && result.is_error));
    // The durable history keeps the interrupted call, paired with its failed result.
    assert!(
        matches!(settled.get(1), Some(SessionUpdate::ItemAdded { item: Item::ToolCall { call_id, name, .. } }) if call_id == "t1" && name == "Read")
    );
    assert!(
        matches!(settled.get(2), Some(SessionUpdate::ItemAdded { item: Item::ToolResult { call_id, result } }) if call_id == "t1" && result.is_error)
    );
    assert!(bridge.settle().is_empty(), "settled once");
}

#[test]
fn empty_chunks_and_other_updates_are_quiet() {
    let mut bridge = Bridge::default();
    let (updates, end) = run(
        &mut bridge,
        vec![text(""), update(Update::CurrentMode { mode_id: "default".into() }), update(Update::Other { kind: "future".into() })],
    );
    assert!(updates.is_empty());
    assert!(end.is_none());
}

#[tokio::test]
async fn acp_sessions_require_authority_and_persistence_gates() {
    let native: BackendFactory =
        Arc::new(|_request: BackendRequest| Box::pin(async { Err(ProtoError::new(ErrorCode::Internal, "native")) }));
    let factory = with_acp_at(native, PathBuf::from("/nonexistent/aimx"));
    let base = SessionSpec {
        workspace: "/tmp".into(),
        location: Location::Local,
        provider: "acp:claude".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Persistent,
    };
    let refuse = |spec: SessionSpec, transcript: Vec<Item>| {
        let factory = Arc::clone(&factory);
        async move { factory(BackendRequest { spec, session_id: "s".into(), transcript, recorded: None }).await.err().map(|e| e.code) }
    };
    let ssh = SessionSpec { location: Location::Ssh { destination: "host".into() }, ..base.clone() };
    assert_eq!(refuse(ssh, Vec::new()).await, Some(ErrorCode::Unavailable));
    assert_eq!(refuse(base.clone(), Vec::new()).await, Some(ErrorCode::Unavailable), "strict local session requires a relay witness");
    let native_ssh =
        SessionSpec { provider: "acp:claude-native".into(), location: Location::Ssh { destination: "host".into() }, ..base.clone() };
    assert_eq!(refuse(native_ssh, Vec::new()).await, Some(ErrorCode::Unavailable));
    // A named agent's ceiling cannot be enforced over ACP yet: refused before anything starts.
    let named = SessionSpec { agent: Some("reader".into()), ..base.clone() };
    let refused =
        factory(BackendRequest { spec: named, session_id: "s".into(), transcript: Vec::new(), recorded: None }).await.err().unwrap();
    assert_eq!(refused.code, ErrorCode::Unavailable);
    assert!(refused.message.contains("named agents"), "{}", refused.message);
    let ephemeral = SessionSpec { persistence: Persistence::Ephemeral, ..base.clone() };
    assert_eq!(refuse(ephemeral, Vec::new()).await, Some(ErrorCode::Unavailable));
    let resumed = vec![Item::User { parts: vec![Part::Text { text: "hi".into() }] }];
    assert_eq!(refuse(base.clone(), resumed).await, Some(ErrorCode::Unavailable));
    let native_spec = SessionSpec { provider: "codex".into(), ..base };
    assert_eq!(refuse(native_spec, Vec::new()).await, Some(ErrorCode::Internal), "non-ACP providers go to the native factory");
}

/// A real Claude Code session through the host: a tool call, text, usage, one terminal event.
/// Run with `mise exec -- cargo test -p aim --test acp_bridge -- --ignored live_`.
#[tokio::test]
#[ignore = "live: runs Claude Code through claude-agent-acp"]
async fn live_acp_claude_session_through_the_host() {
    let dir = std::env::temp_dir().join(format!("aim-live-acp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("probe.txt"), "before\n").unwrap();
    let native: BackendFactory =
        Arc::new(|_request: BackendRequest| Box::pin(async { Err(ProtoError::new(ErrorCode::Internal, "native")) }));
    let host = SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: with_acp_at(native, aimx_binary()),
        update_capacity: 1024,
    });
    let started = std::time::Instant::now();
    let summary = host
        .create(SessionSpec {
            workspace: dir.to_string_lossy().into_owned(),
            location: Location::Local,
            provider: "acp:claude".into(),
            model: None,
            effort: None,
            agent: None,
            persistence: Persistence::Persistent,
        })
        .await
        .unwrap();
    let created = started.elapsed();
    let id = summary.meta.id.clone();
    let (_, mut updates) = host.attach(id.clone()).await.unwrap();
    host.prompt(
        id.clone(),
        vec![Part::Text {
            text: "Use Read on probe.txt, then Edit to replace before with after. Read it again and reply with only the new word.".into(),
        }],
    )
    .await
    .unwrap();
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(180), updates.next()).await.unwrap().unwrap();
        let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        got.push(update);
        if idle {
            break;
        }
    }
    let turn = started.elapsed();
    let text: String =
        got.iter().filter_map(|u| if let SessionUpdate::TextDelta { delta } = u { Some(delta.as_str()) } else { None }).collect();
    let terminal = got.iter().filter(|u| matches!(u, SessionUpdate::TurnEnded { .. } | SessionUpdate::TurnFailed { .. })).count();
    let tools = got.iter().filter(|u| matches!(u, SessionUpdate::ToolStarted { .. })).count();
    let finished = got.iter().filter(|u| matches!(u, SessionUpdate::ToolFinished { .. })).count();
    eprintln!(
        "live acp: model {}, created in {created:?}, turn done at {turn:?}, {tools} tool call(s), reply {text:?}",
        summary.meta.model
    );
    assert!(text.to_lowercase().contains("after"));
    assert_eq!(std::fs::read_to_string(dir.join("probe.txt")).unwrap(), "after\n");
    assert_eq!(terminal, 1, "exactly one terminal event");
    assert!(tools >= 1 && tools == finished, "every started tool finished");
    assert!(
        got.iter()
            .filter_map(|u| match u {
                SessionUpdate::ToolStarted { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .all(|name| name.starts_with("mcp__aim__")),
        "strict authority must report only aim MCP tools"
    );
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { stop: StopReason::EndTurn })));
    host.close(id).await.unwrap();
    let _cleanup = std::fs::remove_dir_all(&dir);
}

/// `acp:claude` with `-m opus`: claude-agent-acp offers Opus only as `opus[1m]`, so the host
/// session starts on the resolved value, and an unknown model is refused with the offered values
/// (ADR 0075). Run with `mise exec -- cargo test -p aim --test acp_bridge -- --ignored live_acp_claude_session_with_model_alias`.
#[tokio::test]
#[ignore = "live: runs Claude Code through claude-agent-acp"]
async fn live_acp_claude_session_with_model_alias() {
    let dir = std::env::temp_dir().join(format!("aim-live-acp-model-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let native: BackendFactory =
        Arc::new(|_request: BackendRequest| Box::pin(async { Err(ProtoError::new(ErrorCode::Internal, "native")) }));
    let host = SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: with_acp_at(native, aimx_binary()),
        update_capacity: 1024,
    });
    let spec = |model: &str| SessionSpec {
        workspace: dir.to_string_lossy().into_owned(),
        location: Location::Local,
        provider: "acp:claude".into(),
        model: Some(model.into()),
        effort: None,
        agent: None,
        persistence: Persistence::Persistent,
    };
    let refused = host.create(spec("gpt-6")).await.unwrap_err();
    eprintln!("live acp model: gpt-6 refused: {}", refused.message);
    assert!(
        refused.message.contains("the requested model is not offered") && refused.message.contains("`opus[1m]`"),
        "{}",
        refused.message
    );
    assert!(!refused.message.contains("gpt-6"), "the request is never repeated: {}", refused.message);

    let started = std::time::Instant::now();
    let summary = host.create(spec("opus")).await.unwrap();
    let id = summary.meta.id.clone();
    let (_, mut updates) = host.attach(id.clone()).await.unwrap();
    host.prompt(id.clone(), vec![Part::Text { text: "Reply with the word ok.".into() }]).await.unwrap();
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(180), updates.next()).await.unwrap().unwrap();
        let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        got.push(update);
        if idle {
            break;
        }
    }
    let text: String =
        got.iter().filter_map(|u| if let SessionUpdate::TextDelta { delta } = u { Some(delta.as_str()) } else { None }).collect();
    eprintln!("live acp model: -m opus runs on {} ({:?}), reply {text:?}", summary.meta.model, started.elapsed());
    assert!(summary.meta.model.contains("opus"), "{}", summary.meta.model);
    assert!(text.to_lowercase().contains("ok"), "{text:?}");
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { stop: StopReason::EndTurn })));
    host.close(id).await.unwrap();
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: runs Claude Code through aim MCP and a private user-space sshd"]
async fn live_acp_claude_ssh_remote_changed_local_untouched() {
    let sshd = Sshd::start();
    let remote = sshd.dir.path().join("remote_workspace");
    let local = sshd.dir.path().join("local_workspace");
    std::fs::create_dir(&remote).unwrap();
    std::fs::create_dir(&local).unwrap();
    std::fs::write(remote.join("task.sh"), "#!/bin/sh\nprintf before\n").unwrap();
    std::fs::write(local.join("task.sh"), "local sentinel\n").unwrap();
    let aim_home = sshd.dir.path().join("aim_home");
    std::fs::create_dir(&aim_home).unwrap();
    std::fs::set_permissions(&aim_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(240), tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
        .arg("run")
        .args(["-p", "acp:claude", "--ssh", "aim-acp-test", "--json", "--aimx"])
        .arg(aimx_binary())
        .arg("-C").arg(&remote)
        .arg("In order, use Read on task.sh; Edit to replace 'before' with 'remote-ok' in task.sh; Glob to find '*.sh'; Grep for 'remote-ok' in task.sh; then Bash to run 'sh task.sh'. Reply with the command output.")
        .env("AIM_SSH_CONFIG", &sshd.config)
        .env("AIM_HOME", &aim_home)
        .kill_on_drop(true)
        .output()).await.unwrap().unwrap();
    eprintln!("live_acp_ssh_turn_ms={}", started.elapsed().as_millis());
    assert!(output.status.success(), "aim CLI exited with {}", output.status);
    assert_eq!(std::fs::read_to_string(remote.join("task.sh")).unwrap(), "#!/bin/sh\nprintf remote-ok\n");
    assert_eq!(std::fs::read_to_string(local.join("task.sh")).unwrap(), "local sentinel\n");
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let names: Vec<_> = events.iter().filter_map(|event| event.get("name").and_then(Value::as_str)).collect();
    assert!(!names.is_empty() && names.iter().all(|name| name.starts_with("mcp__aim__")), "only aim MCP tools may execute");
    assert!(String::from_utf8_lossy(&output.stdout).contains("remote-ok"));
}
