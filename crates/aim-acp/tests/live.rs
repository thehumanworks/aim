//! Live smoke tests against the installed `claude-agent-acp` with the maintainer's Claude login
//! (docs/adr/0022). Run with `mise exec -- cargo test -p aim-acp -- --ignored live_` (mise puts
//! `node` and the pinned adapter on `PATH`).
//!
//! Output is limited to capability data, ids, timings and model text; account details are never
//! printed. Set `AIM_ACP_CAPTURE_DIR` to record the JSON-RPC wire of each test (raw: redact before
//! committing anything from it).
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::print_stdout,
    reason = "live smoke tests report to the terminal and fail loudly"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aim_acp::{AcpAgentConfig, AcpClient, AcpEvent, AcpSession, ConfigKey, SessionOptions, ToolCallStatus, Update, WireDirection, WireTap};
use aim_proto::conversation::{Item, Part, StopReason};

fn config() -> AcpAgentConfig {
    let mut config = AcpAgentConfig::claude();
    if let Ok(command) = std::env::var("AIM_ACP_COMMAND") {
        config.command = PathBuf::from(command);
    }
    config
}

/// A wire tap writing `{"dir": "out"|"in", "msg": …}` lines to `$AIM_ACP_CAPTURE_DIR/<name>.jsonl`.
fn capture(name: &str) -> Option<WireTap> {
    let dir = std::env::var_os("AIM_ACP_CAPTURE_DIR")?;
    std::fs::create_dir_all(&dir).unwrap();
    let file = Arc::new(Mutex::new(std::fs::File::create(Path::new(&dir).join(format!("{name}.jsonl"))).unwrap()));
    Some(Arc::new(move |direction: WireDirection, line: &str| {
        let dir = if direction == WireDirection::Outgoing { "out" } else { "in" };
        let msg: serde_json::Value = serde_json::from_str(line).unwrap_or(serde_json::Value::String(line.to_owned()));
        let entry = serde_json::json!({"dir": dir, "msg": msg});
        writeln!(file.lock().unwrap(), "{entry}").unwrap();
    }))
}

async fn connect(name: &str) -> AcpClient {
    let mut builder = AcpClient::builder(config());
    if let Some(tap) = capture(name) {
        builder = builder.wire_tap(tap);
    }
    let start = Instant::now();
    let client = builder.spawn().await.unwrap_or_else(|e| panic!("spawn + initialize failed: {e}"));
    println!("[{name}] spawn + initialize: {} ms", start.elapsed().as_millis());
    client
}

/// A fresh, empty, absolute working directory unique to this run.
fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("aim-acp-live-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// Runs one prompt to completion; returns its events and the concatenated assistant text.
async fn run_turn(session: &mut AcpSession, text: &str) -> (Vec<AcpEvent>, String, Duration) {
    let start = Instant::now();
    let turn = session.prompt_text(text).await.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(180), turn.collect_all()).await.expect("turn timed out").unwrap();
    let elapsed = start.elapsed();
    let reply = events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Item { item: Item::Assistant { parts, .. } } => Some(parts),
            _ => None,
        })
        .flatten()
        .filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None })
        .collect::<String>();
    (events, reply, elapsed)
}

fn stop_of(events: &[AcpEvent]) -> &aim_acp::TurnEnd {
    match events.last() {
        Some(AcpEvent::Stopped(end)) => end,
        other => panic!("turn did not end with Stopped: {other:?}"),
    }
}

#[tokio::test]
#[ignore = "live: needs claude-agent-acp and a Claude login"]
async fn live_probe() {
    let client = connect("live_probe").await;
    let agent = client.agent();
    println!("[live_probe] agent: {} {} (protocol 1)", agent.name, agent.version);
    println!("[live_probe] capabilities: {}", serde_json::to_string(&client.capabilities().raw).unwrap());
    let methods: Vec<_> = client.auth_methods().iter().map(|m| (m.id.clone(), m.is_terminal(), m.terminal_auth.is_some())).collect();
    println!("[live_probe] auth methods (id, terminal, explicit command): {methods:?}");
    for method in client.auth_methods() {
        let login = client.login_command(&method.id).unwrap();
        println!("[live_probe] login `{}`: program={} args={:?}", login.method_id, login.program.display(), login.args);
    }
    let cwd = scratch("probe");
    let report = client.probe(&cwd).await.unwrap();
    // Give the background `claude auth status` push a moment (the adapter's probe has a 5 s timeout).
    for _ in 0..50 {
        if client.auth_status().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("[live_probe] auth status (no personal data): {:?}", client.auth_status());
    for option in &report.config_options {
        println!(
            "[live_probe] option {} (category {:?}) current={:?} allowed={:?}",
            option.id,
            option.category,
            option.current(),
            option.allowed()
        );
    }
    println!(
        "[live_probe] session/new {} ms; set_config_option ok={} in {:?} ms; effort option={}",
        report.session_new_ms, report.set_config_option, report.set_config_ms, report.effort_option
    );
    println!("[live_probe] missing (native): {:?}; missing (aim tools): {:?}", report.missing_native, report.missing_aim_tools);
    assert!(report.missing_native.is_empty(), "{:?}", report.missing_native);
    assert!(report.missing_aim_tools.is_empty(), "{:?}", report.missing_aim_tools);
    assert!(client.capabilities().claude_code);
    client.shutdown(Duration::from_secs(3)).await;
}

/// Where Claude Code keeps session transcripts (`$CLAUDE_CONFIG_DIR/projects`, default
/// `~/.claude/projects`).
fn projects_dir() -> PathBuf {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude"), PathBuf::from);
    base.join("projects")
}

/// Files under `dir` (two levels: `<project-slug>/<file>`) whose name contains `needle`.
fn files_named(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(projects) = std::fs::read_dir(dir) else { return found };
    for project in projects.flatten() {
        let path = project.path();
        if path.file_name().is_some_and(|n| n.to_string_lossy().contains(needle)) {
            found.push(path.clone());
        }
        if let Ok(files) = std::fs::read_dir(&path) {
            found.extend(files.flatten().map(|f| f.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(needle))));
        }
    }
    found
}

/// Polls for a transcript of `session_id` for up to `wait`.
async fn find_transcript(session_id: &str, wait: Duration) -> Vec<PathBuf> {
    let start = Instant::now();
    loop {
        let found = files_named(&projects_dir(), session_id);
        if !found.is_empty() || start.elapsed() >= wait {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn usage_updates(events: &[AcpEvent]) -> Vec<aim_acp::ContextUsage> {
    events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Update { update: Update::Usage(usage), .. } => Some(usage.clone()),
            _ => None,
        })
        .collect()
}

fn event_kinds(events: &[AcpEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            AcpEvent::Update { update, .. } => {
                format!("update:{}", serde_json::to_value(update).unwrap()["update"].as_str().unwrap_or("?"))
            }
            AcpEvent::Item { item } => format!("item:{}", serde_json::to_value(item).unwrap()["type"].as_str().unwrap_or("?")),
            AcpEvent::Permission { .. } => "permission".into(),
            AcpEvent::Stopped(_) => "stopped".into(),
        })
        .collect()
}

#[tokio::test]
#[ignore = "live: needs claude-agent-acp and a Claude login"]
async fn live_prompt_native() {
    let client = connect("live_prompt_native").await;
    let projects = projects_dir();
    println!("[live_prompt_native] transcript store: {} (searched as <slug>/<sessionId>*)", projects.display());

    // Positive control: a persisted session does leave a transcript, so "no file" below means
    // something.
    let persisted_cwd = scratch("persisted");
    let mut persisted = client.new_session(SessionOptions::new(&persisted_cwd)).await.unwrap();
    let (events, reply, elapsed) = run_turn(&mut persisted, "Reply with exactly: OK").await;
    println!("[live_prompt_native] persisted turn: {} ms, reply {reply:?}, stop {:?}", elapsed.as_millis(), stop_of(&events).stop);
    let found = find_transcript(persisted.id(), Duration::from_secs(5)).await;
    println!("[live_prompt_native] persisted session {} transcript: {found:?}", persisted.id());
    assert!(!found.is_empty(), "control failed: a persisted session left no transcript under {}", projects.display());
    let persisted_id = persisted.id().to_owned();
    persisted.delete().await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let residue = files_named(&projects, &persisted_id);
    println!(
        "[live_prompt_native] after session/delete (+3 s): {residue:?} entry types {:?}",
        residue.iter().map(|p| entry_types(p)).collect::<Vec<_>>()
    );

    // The private session: `persistSession: false`.
    let private_cwd = scratch("private");
    let mut options = SessionOptions::new(&private_cwd);
    options.persist = false;
    let mut session = client.new_session(options).await.unwrap();
    let id = session.id().to_owned();
    let (events, reply, elapsed) = run_turn(&mut session, "Reply with exactly: OK").await;
    let end = stop_of(&events);
    println!(
        "[live_prompt_native] private turn 1: {} ms, reply {reply:?}, stop {:?}, usage {:?}",
        elapsed.as_millis(),
        end.stop,
        end.usage
    );
    println!("[live_prompt_native] usage updates 1: {:?}", usage_updates(&events));
    println!("[live_prompt_native] event kinds: {:?}", event_kinds(&events));
    assert_eq!(end.stop, StopReason::EndTurn);
    assert!(reply.contains("OK"), "reply {reply:?}");
    let (events2, reply2, elapsed2) = run_turn(&mut session, "Reply with exactly: OK2").await;
    println!(
        "[live_prompt_native] private turn 2: {} ms, reply {reply2:?}, usage updates {:?}",
        elapsed2.as_millis(),
        usage_updates(&events2)
    );
    // Check after the session is closed and the adapter (and its `claude` child) has exited, so
    // anything written at teardown is caught too.
    session.close().await.unwrap();
    client.shutdown(Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let found = files_named(&projects, &id);
    let slug_dirs = files_named(&projects, private_cwd.file_name().unwrap().to_str().unwrap());
    let slug_files: Vec<_> = slug_dirs.iter().flat_map(|d| files_under(d)).collect();
    println!(
        "[live_prompt_native] private session {id} after close + exit: files named after it {found:?}; project dirs for its cwd {slug_dirs:?} holding files {slug_files:?}"
    );
    assert!(found.is_empty(), "persistSession:false left a transcript: {found:?}");
    assert!(slug_files.is_empty(), "files in the private session's project dir: {slug_files:?}");

    // Leave no test residue in the user's Claude store.
    for dir in files_named(&projects, "aim-acp-live-") {
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}

/// Every file (not directory) below `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.flatten().map(|e| e.path()).flat_map(|p| if p.is_dir() { files_under(&p) } else { vec![p] }).collect()
}

/// The `type` of each JSONL entry of a transcript file (no content).
fn entry_types(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| v.get("type").and_then(|t| t.as_str()).unwrap_or("?").to_owned())
        .collect()
}

#[tokio::test]
#[ignore = "live: needs claude-agent-acp and a Claude login"]
async fn live_set_config() {
    let client = connect("live_set_config").await;
    let mut options = SessionOptions::new(scratch("config"));
    options.persist = false;
    let mut session = client.new_session(options).await.unwrap();
    let show = |session: &AcpSession| {
        session.config_options().iter().map(|o| format!("{}={}", o.id, o.current().unwrap_or_default())).collect::<Vec<_>>().join(" ")
    };
    println!("[live_set_config] initial: {}", show(&session));

    let start = Instant::now();
    session.set_config(&ConfigKey::Effort, "low").await.unwrap();
    println!("[live_set_config] effort=low confirmed in {} ms: {}", start.elapsed().as_millis(), show(&session));

    let model = session.config_options().iter().find(|o| o.id == "model").unwrap();
    let target =
        ["haiku", "sonnet"].into_iter().find(|m| model.allowed().iter().any(|a| a == m) && model.current().as_deref() != Some(m)).unwrap();
    let start = Instant::now();
    session.set_config(&ConfigKey::Model, target).await.unwrap();
    println!("[live_set_config] model={target} confirmed in {} ms: {}", start.elapsed().as_millis(), show(&session));

    let rejected = session.set_config(&ConfigKey::Model, "no-such-model").await.unwrap_err();
    println!("[live_set_config] unadvertised model rejected locally: {rejected}");
    assert!(matches!(rejected, aim_acp::AcpError::ConfigValueRejected { .. }));

    let (events, reply, elapsed) = run_turn(&mut session, "Reply with exactly: OK").await;
    let models: Vec<_> = usage_updates(&events).into_iter().filter_map(|u| u.model).collect();
    println!("[live_set_config] turn on {target}: {} ms, reply {reply:?}, usage model {models:?}", elapsed.as_millis());
    assert!(models.iter().any(|m| m.contains(target)), "turn did not run on {target}: {models:?}");
    session.close().await.unwrap();
    client.shutdown(Duration::from_secs(3)).await;
}

#[tokio::test]
#[ignore = "live: needs claude-agent-acp and a Claude login"]
async fn live_aim_tools_mode_start() {
    let client = connect("live_aim_tools_mode_start").await;
    let cwd = scratch("aimtools");
    let log = cwd.join("echo-mcp.log");
    let nonce = format!("ping-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis());
    let mut options = SessionOptions::new(&cwd);
    options.persist = false;
    options.tool_authority = aim_acp::ToolAuthority::aim();
    options.mcp_servers = vec![aim_acp::McpServerSpec::Stdio {
        name: aim_acp::AIM_MCP_SERVER.into(),
        command: PathBuf::from(env!("CARGO_BIN_EXE_aim-acp-echo-mcp")),
        args: Vec::new(),
        env: [("AIM_ACP_ECHO_LOG".to_owned(), log.display().to_string())].into(),
    }];
    let start = Instant::now();
    let mut session = client.new_session(options).await.unwrap();
    println!("[live_aim_tools_mode_start] session/new (aim tools) {} ms", start.elapsed().as_millis());

    let prompt = format!("Call the mcp__aim__echo tool with text \"{nonce}\", then reply with exactly the text it returned.");
    let (events, reply, elapsed) = run_turn(&mut session, &prompt).await;
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Update { update: Update::ToolCall(call), .. } if call.status.is_final() => {
                Some((call.display_name().to_owned(), call.status))
            }
            _ => None,
        })
        .collect();
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    println!("[live_aim_tools_mode_start] turn {} ms; reply {reply:?}; finished tool calls {calls:?}", elapsed.as_millis());
    println!("[live_aim_tools_mode_start] event kinds: {:?}", event_kinds(&events));
    println!("[live_aim_tools_mode_start] echo server log: {:?}", log_text.lines().collect::<Vec<_>>());
    println!("[live_aim_tools_mode_start] stdio MCP spawned (issue #883 does NOT reproduce): {}", log_text.contains("start"));
    assert!(log_text.contains("start"), "the stdio MCP server was never spawned (adapter issue #883)");
    assert!(log_text.contains(&format!("call {nonce}")), "echo was not called");
    assert!(calls.iter().any(|(name, status)| name.contains("echo") && *status == ToolCallStatus::Completed), "{calls:?}");
    assert!(reply.contains(&nonce), "reply {reply:?}");

    let (_, tools, _) = run_turn(&mut session, "List the exact names of every tool you can call, comma-separated, nothing else.").await;
    println!("[live_aim_tools_mode_start] tools the model reports: {tools}");
    session.close().await.unwrap();
    client.shutdown(Duration::from_secs(3)).await;
}

#[tokio::test]
#[ignore = "live: needs claude-agent-acp and a Claude login"]
async fn live_permission_yolo() {
    let client = connect("live_permission_yolo").await;
    let mut options = SessionOptions::new(scratch("permission"));
    options.persist = false;
    let mut session = client.new_session(options).await.unwrap();
    // "Manual" mode asks before every Bash call.
    session.set_config(&ConfigKey::Mode, "default").await.unwrap();
    let nonce = format!("perm-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis());
    let (events, reply, elapsed) =
        run_turn(&mut session, &format!("Run the shell command `echo {nonce}` with the Bash tool, then reply with exactly its output."))
            .await;
    let permissions: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Permission { request, decision } => Some((
                request.tool_call.display_name().to_owned(),
                request.options.iter().map(|o| (o.id.clone(), o.kind)).collect::<Vec<_>>(),
                decision.clone(),
            )),
            _ => None,
        })
        .collect();
    println!("[live_permission_yolo] turn {} ms; reply {reply:?}", elapsed.as_millis());
    println!("[live_permission_yolo] permission requests (tool, options, decision): {permissions:?}");
    println!("[live_permission_yolo] event kinds: {:?}", event_kinds(&events));
    assert!(!permissions.is_empty(), "no permission was requested");
    assert!(reply.contains(&nonce), "reply {reply:?}");
    session.close().await.unwrap();
    client.shutdown(Duration::from_secs(3)).await;
}
