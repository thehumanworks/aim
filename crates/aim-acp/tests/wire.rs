//! Offline wire tests: redacted live transcripts (`tests/fixtures/*.jsonl`, recorded from
//! `claude-agent-acp` 0.81.2 by `tests/live.rs`) replayed by a scripted fake agent against the real
//! client stack, plus `_meta` construction, auth-method parsing and process handling.
//!
//! The replayer walks a transcript in order: for an `out` frame it reads the client's next message
//! and checks its method (remapping request ids); for an `in` frame it sends the recorded message
//! (with response ids remapped to the client's actual ids).
#![expect(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing, reason = "test harness helpers fail loudly")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use aim_acp::{
    AcpAgentConfig, AcpClient, AcpError, AcpEvent, AuthMethodKind, ConfigKey, McpServerSpec, PermissionDecision, PermissionHandler,
    PermissionKind, PermissionOption, PermissionRequest, SessionOptions, ToolAuthority, ToolCallStatus, Update, parse_auth_methods,
    parse_config_options, yolo_choice,
};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::tool::ToolContent;
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn transcript(name: &str) -> Vec<(bool, Value)> {
    fixture(name)
        .lines()
        .map(|line| {
            let entry: Value = serde_json::from_str(line).unwrap();
            (entry["dir"] == "out", entry["msg"].clone())
        })
        .collect()
}

/// Connects a client to a fake agent replaying `frames`; returns the client and a handle yielding
/// every message the client sent.
async fn replay(
    frames: Vec<(bool, Value)>,
    permissions: Option<Box<dyn FnOnce(aim_acp::AcpClientBuilder) -> aim_acp::AcpClientBuilder>>,
) -> (AcpClient, tokio::task::JoinHandle<Vec<Value>>) {
    let (client_io, agent_io) = tokio::io::duplex(1 << 20);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (agent_read, mut agent_write) = tokio::io::split(agent_io);
    let agent = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_read).lines();
        let mut ids: HashMap<String, Value> = HashMap::new();
        let mut received = Vec::new();
        for (outgoing, recorded) in frames {
            if outgoing {
                let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                    .await
                    .unwrap_or_else(|_| panic!("client never sent {recorded}"))
                    .unwrap()
                    .unwrap_or_else(|| panic!("client closed before sending {recorded}"));
                let actual: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(actual.get("method"), recorded.get("method"), "client sent {actual}, transcript expects {recorded}");
                if let (Some(recorded_id), Some(actual_id), Some(_)) = (recorded.get("id"), actual.get("id"), recorded.get("method")) {
                    ids.insert(recorded_id.to_string(), actual_id.clone());
                }
                received.push(actual);
            } else {
                let mut message = recorded.clone();
                if message.get("method").is_none()
                    && let Some(id) = message.get("id").map(Value::to_string)
                {
                    message["id"] = ids.get(&id).cloned().unwrap_or_else(|| panic!("no request for recorded id {id}"));
                }
                agent_write.write_all(format!("{message}\n").as_bytes()).await.unwrap();
            }
        }
        received
    });
    let mut builder = AcpClient::builder(AcpAgentConfig::claude());
    if let Some(configure) = permissions {
        builder = configure(builder);
    }
    let client = builder.connect(client_read, client_write).await.unwrap();
    (client, agent)
}

fn sent<'a>(received: &'a [Value], method: &str) -> &'a Value {
    received.iter().find(|m| m["method"] == method).unwrap_or_else(|| panic!("client never sent {method}"))
}

fn recorded<'a>(frames: &'a [(bool, Value)], method: &str) -> &'a Value {
    frames.iter().map(|(_, m)| m).find(|m| m["method"] == method).unwrap()
}

fn assistant_text(events: &[AcpEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Item { item: Item::Assistant { parts, .. } } => {
                Some(parts.iter().filter_map(|p| if let Part::Text { text } = p { Some(text.clone()) } else { None }).collect())
            }
            _ => None,
        })
        .collect()
}

fn aim_tools_options() -> SessionOptions {
    let mut options = SessionOptions::new("/tmp/aim-acp-live-aimtools");
    options.persist = false;
    options.tool_authority = ToolAuthority::aim();
    options.mcp_servers = vec![McpServerSpec::Stdio {
        name: "aim".into(),
        command: PathBuf::from("/home/user/aim/target/debug/aim-acp-echo-mcp"),
        args: Vec::new(),
        env: [("AIM_ACP_ECHO_LOG".to_owned(), "/tmp/aim-acp-live-aimtools/echo-mcp.log".to_owned())].into(),
    }];
    options
}

#[tokio::test]
async fn replays_an_aim_tools_turn_into_typed_events_and_items() {
    let frames = transcript("aim_tools_turn.jsonl");
    let (client, agent) = replay(frames.clone(), None).await;
    assert_eq!(client.agent().version, "0.81.2");
    assert!(client.capabilities().claude_code && client.capabilities().sessions.close && client.capabilities().mcp.http);
    assert_eq!(client.auth_methods().first().map(|m| m.id.as_str()), Some("claude-login"));

    let mut session = client.new_session(aim_tools_options()).await.unwrap();
    assert_eq!(session.id(), "ae789e40-33e5-4fda-a64e-1843bea54685");
    let ids: Vec<_> = session.config_options().iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["mode", "model", "effort", "fast"]);

    let events = session.prompt_text("Call the mcp__aim__echo tool").await.unwrap().collect_all().await.unwrap();
    // Updates that arrived between session/new and the prompt come first.
    assert!(matches!(&events[0], AcpEvent::Update { update: Update::AvailableCommands { commands }, .. } if commands.len() == 2));
    let states: Vec<_> = events
        .iter()
        .filter_map(|e| if let AcpEvent::Update { update: Update::ToolCall(call), .. } = e { Some(call.clone()) } else { None })
        .collect();
    assert_eq!(states.len(), 4);
    assert!(states.iter().all(|c| c.id == "toolu_01KvcAdTcmiEkmwB8NN3bucD" && c.name.as_deref() == Some("mcp__aim__echo")));
    assert_eq!(states[0].status, ToolCallStatus::Pending);
    // The upsert keeps earlier fields: the final update carries only status/output/content.
    let last = states.last().unwrap();
    assert_eq!(last.status, ToolCallStatus::Completed);
    assert_eq!(last.raw_input, Some(json!({"text": "ping-1790296931576"})));
    assert_eq!(last.title, "mcp__aim__echo");

    let items: Vec<_> = events.iter().filter_map(|e| if let AcpEvent::Item { item } = e { Some(item.clone()) } else { None }).collect();
    assert_eq!(items.len(), 3, "{items:?}");
    match &items[0] {
        Item::ToolCall { call_id, name, arguments, native } => {
            assert_eq!(call_id, "toolu_01KvcAdTcmiEkmwB8NN3bucD");
            assert_eq!(name, "mcp__aim__echo");
            assert_eq!(serde_json::from_str::<Value>(arguments).unwrap(), json!({"text": "ping-1790296931576"}));
            assert_eq!(native.as_ref().unwrap().provider, "acp:claude");
        }
        other => panic!("{other:?}"),
    }
    match &items[1] {
        Item::ToolResult { result, .. } => {
            assert_eq!(result.content, vec![ToolContent::Text { text: "ping-1790296931576".into() }]);
            assert!(!result.is_error);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(assistant_text(&events), ["ping-1790296931576"]);
    let AcpEvent::Stopped(end) = events.last().unwrap() else { panic!("no stop") };
    assert_eq!(end.stop, StopReason::EndTurn);
    let usage = end.usage.as_ref().unwrap();
    // inputTokens 4 + cachedRead 3108 + cachedWrite 3205.
    assert_eq!((usage.input_tokens, usage.cached_input_tokens, usage.cache_write_tokens, usage.output_tokens), (6317, 3108, 3205, 70));

    // Turn 2: the title pushed after turn 1 ended leads; two chunks of one message form one item.
    let events = session.prompt_text("List your tools").await.unwrap().collect_all().await.unwrap();
    assert!(matches!(&events[0], AcpEvent::Update { update: Update::SessionInfo { title: Some(t), .. }, .. } if t == "MCP echo tool test"));
    assert_eq!(assistant_text(&events), ["mcp__aim__echo"]);
    session.close().await.unwrap();

    let received = agent.await.unwrap();
    // What aim sends is exactly what worked live.
    assert_eq!(sent(&received, "initialize")["params"], recorded(&frames, "initialize")["params"]);
    assert_eq!(sent(&received, "session/new")["params"]["_meta"], recorded(&frames, "session/new")["params"]["_meta"]);
    assert_eq!(sent(&received, "session/new")["params"]["mcpServers"], recorded(&frames, "session/new")["params"]["mcpServers"]);
}

#[tokio::test]
async fn replays_a_permission_request_answered_by_the_yolo_handler() {
    let frames = transcript("permission_turn.jsonl");
    let (client, agent) = replay(frames.clone(), None).await;
    let mut options = SessionOptions::new("/tmp/aim-acp-live-permission");
    options.persist = false;
    let mut session = client.new_session(options).await.unwrap();
    session.set_config(&ConfigKey::Mode, "default").await.unwrap();
    assert_eq!(session.config_options().iter().find(|o| o.id == "mode").unwrap().current().as_deref(), Some("default"));

    let events = session.prompt_text("Use the Write tool").await.unwrap().collect_all().await.unwrap();
    let (request, decision) = events
        .iter()
        .find_map(|e| if let AcpEvent::Permission { request, decision } = e { Some((request, decision)) } else { None })
        .unwrap();
    assert_eq!(request.tool_call.name.as_deref(), Some("Write"));
    assert_eq!(request.tool_call.kind, "edit");
    let kinds: Vec<_> = request.options.iter().map(|o| (o.id.as_str(), o.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("allow-once", PermissionKind::AllowOnce),
            ("allow-with-updates", PermissionKind::AllowAlways),
            ("reject", PermissionKind::RejectOnce)
        ]
    );
    assert_eq!(decision, &PermissionDecision::Selected { option_id: "allow-once".into() });
    let items: Vec<_> = events.iter().filter_map(|e| if let AcpEvent::Item { item } = e { Some(item.clone()) } else { None }).collect();
    match &items[1] {
        Item::ToolResult { result, .. } => {
            assert!(matches!(&result.content[..], [ToolContent::Text { text }] if text.starts_with("File created successfully")));
        }
        other => panic!("{other:?}"),
    }
    session.close().await.unwrap();

    let received = agent.await.unwrap();
    let answer = received.iter().find(|m| m.get("method").is_none() && m.get("result").is_some()).unwrap();
    assert_eq!(answer["result"], json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}));
    assert_eq!(answer["id"], 0);
}

struct Reject;

impl PermissionHandler for Reject {
    fn request_permission(&self, request: PermissionRequest) -> futures::future::BoxFuture<'static, PermissionDecision> {
        let reject = request.options.iter().find(|o| o.kind == PermissionKind::RejectOnce).map(|o| o.id.clone());
        Box::pin(async move { reject.map_or(PermissionDecision::Cancelled, |option_id| PermissionDecision::Selected { option_id }) })
    }
}

#[tokio::test]
async fn a_custom_permission_handler_answers_for_the_embedder() {
    let frames = transcript("permission_turn.jsonl");
    let (client, agent) = replay(frames, Some(Box::new(|builder| builder.permissions(Reject)))).await;
    let mut session = client.new_session(SessionOptions::new("/tmp/x")).await.unwrap();
    session.set_config(&ConfigKey::Mode, "default").await.unwrap();
    drop(session.prompt_text("Use the Write tool").await.unwrap().collect_all().await.unwrap());
    session.close().await.unwrap();
    let received = agent.await.unwrap();
    let answer = received.iter().find(|m| m.get("method").is_none() && m.get("result").is_some()).unwrap();
    assert_eq!(answer["result"], json!({"outcome": {"outcome": "selected", "optionId": "reject"}}));
}

fn initialize_frames() -> Vec<(bool, Value)> {
    let frames = transcript("aim_tools_turn.jsonl");
    frames.into_iter().take(2).collect()
}

#[tokio::test]
async fn auth_required_maps_to_needs_login_with_the_advertised_methods() {
    let mut frames = initialize_frames();
    frames.extend([
        (true, json!({"jsonrpc": "2.0", "id": "n", "method": "session/new"})),
        (false, json!({"jsonrpc": "2.0", "id": "n", "result": {"sessionId": "s1"}})),
        (true, json!({"jsonrpc": "2.0", "id": "p", "method": "session/prompt"})),
        (false, json!({"jsonrpc": "2.0", "id": "p", "error": {"code": -32000, "message": "Authentication required", "data": {"reason": "claude_subscription_not_supported"}}})),
    ]);
    let (client, _agent) = replay(frames, None).await;
    let mut session = client.new_session(SessionOptions::new("/tmp/x")).await.unwrap();
    let error = session.prompt_text("hi").await.unwrap().collect_all().await.unwrap_err();
    assert_eq!(
        error,
        AcpError::NeedsLogin {
            message: "Authentication required".into(),
            reason: Some("claude_subscription_not_supported".into()),
            methods: vec!["claude-login".into()],
        }
    );
    assert_eq!(error.code(), aim_proto::error::ErrorCode::Unauthenticated);
}

#[tokio::test]
async fn a_dropped_turn_is_cancelled_and_its_leftovers_do_not_leak_into_the_next() {
    let chunk = |text: &str| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}});
    let mut frames = initialize_frames();
    frames.extend([
        (true, json!({"jsonrpc": "2.0", "id": "n", "method": "session/new"})),
        (false, json!({"jsonrpc": "2.0", "id": "n", "result": {"sessionId": "s1"}})),
        (true, json!({"jsonrpc": "2.0", "id": "p1", "method": "session/prompt"})),
        (false, chunk("first")),
        (true, json!({"jsonrpc": "2.0", "method": "session/cancel"})),
        (false, chunk("late")),
        (false, json!({"jsonrpc": "2.0", "id": "p1", "result": {"stopReason": "cancelled"}})),
        (true, json!({"jsonrpc": "2.0", "id": "p2", "method": "session/prompt"})),
        (false, chunk("second")),
        (false, json!({"jsonrpc": "2.0", "id": "p2", "result": {"stopReason": "end_turn"}})),
    ]);
    let (client, agent) = replay(frames, None).await;
    let mut session = client.new_session(SessionOptions::new("/tmp/x")).await.unwrap();
    {
        let mut turn = session.prompt_text("one").await.unwrap();
        let first = turn.next().await.unwrap().unwrap();
        assert!(matches!(first, AcpEvent::Update { update: Update::AgentMessage(_), .. }));
    }
    let events = session.prompt_text("two").await.unwrap().collect_all().await.unwrap();
    assert_eq!(assistant_text(&events), ["second"]);
    assert!(matches!(events.last(), Some(AcpEvent::Stopped(end)) if end.stop == StopReason::EndTurn));
    drop(agent.await.unwrap());
}

#[tokio::test]
async fn unknown_updates_and_stop_reasons_pass_through_with_their_payload() {
    let mut frames = initialize_frames();
    let odd = json!({"sessionUpdate": "subagent_spawned", "subagentId": "a1"});
    frames.extend([
        (true, json!({"jsonrpc": "2.0", "id": "n", "method": "session/new"})),
        (false, json!({"jsonrpc": "2.0", "id": "n", "result": {"sessionId": "s1"}})),
        (true, json!({"jsonrpc": "2.0", "id": "p", "method": "session/prompt"})),
        (false, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": odd}})),
        (false, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call_update"}}})),
        (false, json!({"jsonrpc": "2.0", "id": "p", "result": {"stopReason": "max_turn_requests", "extra": true}})),
    ]);
    let (client, _agent) = replay(frames, None).await;
    let mut session = client.new_session(SessionOptions::new("/tmp/x")).await.unwrap();
    let events = session.prompt_text("hi").await.unwrap().collect_all().await.unwrap();
    assert!(matches!(&events[0], AcpEvent::Update { update: Update::Other { kind }, raw } if kind == "subagent_spawned" && *raw == odd));
    assert!(matches!(&events[1], AcpEvent::Update { update: Update::Other { kind }, .. } if kind == "tool_call_update"));
    let AcpEvent::Stopped(end) = &events[2] else { panic!("{events:?}") };
    assert_eq!(end.stop, StopReason::Other { reason: "max_turn_requests".into() });
    assert_eq!(end.raw["extra"], true);
}

#[tokio::test]
async fn an_agent_vanishing_mid_turn_ends_the_stream_with_an_error() {
    let mut frames = initialize_frames();
    frames.extend([
        (true, json!({"jsonrpc": "2.0", "id": "n", "method": "session/new"})),
        (false, json!({"jsonrpc": "2.0", "id": "n", "result": {"sessionId": "s1"}})),
        (true, json!({"jsonrpc": "2.0", "id": "p", "method": "session/prompt"})),
        (false, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "partial"}}}})),
        // …and the agent's stdout closes without answering the prompt.
    ]);
    let (client, _agent) = replay(frames, None).await;
    let mut session = client.new_session(SessionOptions::new("/tmp/x")).await.unwrap();
    let mut turn = session.prompt_text("hi").await.unwrap();
    assert!(matches!(turn.next().await, Some(Ok(AcpEvent::Update { update: Update::AgentMessage(_), .. }))));
    // The partial message is still recorded before the failure.
    assert!(matches!(turn.next().await, Some(Ok(AcpEvent::Item { item: Item::Assistant { .. } }))));
    assert!(matches!(turn.next().await, Some(Err(AcpError::AgentExited { .. }))));
    assert!(turn.next().await.is_none());
    drop(turn);
    assert!(client.is_closed());
}

#[test]
fn handles_can_move_across_tasks() {
    fn send<T: Send>() {}
    fn sync<T: Sync>() {}
    send::<AcpClient>();
    sync::<AcpClient>();
    send::<aim_acp::AcpSession>();
    send::<aim_acp::Turn<'static>>();
    send::<aim_acp::CancelHandle>();
    sync::<aim_acp::CancelHandle>();
}

#[test]
fn aim_tool_authority_meta_is_the_documented_recipe() {
    let mut options = SessionOptions::new("/w");
    options.persist = false;
    options.tool_authority =
        ToolAuthority::Aim { keep_builtins: vec!["WebSearch".into()], aliases: [("Bash".into(), "mcp__aim__bash".into())].into() };
    options.system_prompt_append = Some("Workspace is remote.".into());
    options.claude_options.insert("maxTurns".into(), json!(8));
    // A caller cannot weaken the authority settings through the passthrough.
    options.claude_options.insert("strictMcpConfig".into(), json!(false));
    assert_eq!(
        Value::Object(options.meta().unwrap()),
        json!({
            "claudeCode": {"options": {
                "maxTurns": 8,
                "tools": ["WebSearch"],
                "toolAliases": {"Bash": "mcp__aim__bash"},
                "strictMcpConfig": true,
                "settingSources": [],
                "allowedTools": ["mcp__aim"],
                "env": {"ENABLE_TOOL_SEARCH": "false"},
                "persistSession": false
            }},
            "systemPrompt": {"append": "Workspace is remote."}
        })
    );
    let ToolAuthority::Aim { aliases, keep_builtins } = ToolAuthority::aim() else { panic!() };
    assert!(keep_builtins.is_empty());
    assert_eq!(aliases.keys().map(String::as_str).collect::<Vec<_>>(), ["Bash", "Edit", "Glob", "Grep", "Read", "Write"]);
    assert!(aliases.values().all(|v| v.starts_with("mcp__aim__")));
}

#[test]
fn native_meta_says_only_what_is_needed() {
    assert_eq!(SessionOptions::new("/w").meta(), None);
    let mut private = SessionOptions::new("/w");
    private.persist = false;
    assert_eq!(Value::Object(private.meta().unwrap()), json!({"claudeCode": {"options": {"persistSession": false}}}));
    let params = private.new_session_params();
    assert_eq!(params["cwd"], "/w");
    assert_eq!(params["mcpServers"], json!([]));
}

#[test]
fn mcp_server_specs_use_the_acp_shapes_and_hide_secrets_from_debug() {
    let stdio = McpServerSpec::Stdio {
        name: "aim".into(),
        command: "/bin/relay".into(),
        args: vec!["--x".into()],
        env: [("TOKEN".into(), "s3cret".into())].into(),
    };
    assert_eq!(
        stdio.to_acp(),
        json!({"name": "aim", "command": "/bin/relay", "args": ["--x"], "env": [{"name": "TOKEN", "value": "s3cret"}]})
    );
    let http = McpServerSpec::Http {
        name: "aim".into(),
        url: "http://127.0.0.1:9/mcp".into(),
        headers: [("Authorization".into(), "Bearer t".into())].into(),
    };
    assert_eq!(http.to_acp()["type"], "http");
    for spec in [stdio, http] {
        let debug = format!("{spec:?}");
        assert!(!debug.contains("s3cret") && !debug.contains("Bearer t"), "{debug}");
    }
}

#[test]
fn terminal_login_commands_come_from_the_real_initialize_result() {
    let init: Value = serde_json::from_str(&fixture("initialize_local.json")).unwrap();
    let methods = parse_auth_methods(&init);
    assert_eq!(methods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["claude-ai-login", "console-login"]);
    assert!(methods.iter().all(aim_acp::AuthMethodInfo::is_terminal));
    let agent = AcpAgentConfig::claude().with_env("ANTHROPIC_BASE_URL", "http://gateway");
    let login = aim_acp::login_command(&methods[0], &agent).unwrap();
    assert!(login.program.ends_with("bin/node"));
    assert_eq!(login.args.last().map(String::as_str), Some("--claudeai"));
    assert!(login.args.iter().any(|a| a.ends_with("claude-agent-acp")));
    assert_eq!(login.env.get("ANTHROPIC_BASE_URL").map(String::as_str), Some("http://gateway"));
    assert_eq!(login.label, "Claude Login");

    // Without the legacy `_meta` block, ACP's rule applies: the agent's own command + `args`.
    let mut bare = methods[1].clone();
    bare.terminal_auth = None;
    let agent = AcpAgentConfig::new("claude", "/opt/claude-agent-acp").with_args(["--hide-claude-auth"]);
    let login = aim_acp::login_command(&bare, &agent).unwrap();
    assert_eq!(login.program, PathBuf::from("/opt/claude-agent-acp"));
    assert_eq!(login.args, ["--hide-claude-auth", "--cli", "auth", "login", "--console"]);
    assert!(matches!(&bare.kind, AuthMethodKind::Terminal { args, .. } if args.len() == 4));

    let gateway = parse_auth_methods(
        &json!({"authMethods": [{"id": "gateway", "name": "Gateway", "_meta": {"gateway": {"protocol": "anthropic"}}}, {"name": "no id"}]}),
    );
    assert_eq!(gateway.len(), 1);
    assert_eq!(aim_acp::login_command(&gateway[0], &agent).unwrap_err(), AcpError::NotTerminalAuth { id: "gateway".into() });
}

#[test]
fn config_options_parse_from_the_live_session_result() {
    let frames = transcript("permission_turn.jsonl");
    let result = frames.iter().find_map(|(_, m)| m.get("result").filter(|r| r.get("sessionId").is_some())).unwrap();
    let options = parse_config_options(result.get("configOptions"));
    let effort = options.iter().find(|o| o.category.as_deref() == Some("thought_level")).unwrap();
    assert_eq!(effort.id, "effort");
    assert_eq!(effort.allowed(), ["default", "low", "medium", "high", "xhigh", "max"]);
    assert!(options.iter().find(|o| o.id == "model").unwrap().allowed().iter().any(|m| m == "haiku"));
    assert_eq!(options.iter().find(|o| o.id == "fast").unwrap().category.as_deref(), Some("model_config"));

    let other = parse_config_options(Some(&json!([
        {"id": "fast", "name": "Fast", "type": "boolean", "currentValue": false},
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "a",
         "options": [{"group": "g1", "name": "Group", "options": [{"value": "a", "name": "A"}, {"value": "b", "name": "B"}]}]},
        {"id": "broken", "type": "select"}
    ])));
    assert_eq!(other.len(), 2, "malformed entries are skipped");
    assert_eq!(other[0].allowed(), ["true", "false"]);
    assert_eq!(other[1].allowed(), ["a", "b"]);
    assert!(matches!(&other[1].kind, aim_acp::ConfigKind::Select { values, .. } if values[1].group.as_deref() == Some("Group")));
}

#[tokio::test]
async fn config_values_are_validated_against_what_the_agent_advertised() {
    let mut frames = initialize_frames();
    let session_result = transcript("permission_turn.jsonl")
        .into_iter()
        .find_map(|(_, m)| m.get("result").filter(|r| r.get("sessionId").is_some()).cloned())
        .unwrap();
    frames.extend([
        (true, json!({"jsonrpc": "2.0", "id": "n", "method": "session/new"})),
        (false, json!({"jsonrpc": "2.0", "id": "n", "result": session_result})),
        // The agent claims success but reports another value: not applied.
        (true, json!({"jsonrpc": "2.0", "id": "c", "method": "session/set_config_option"})),
        (false, json!({"jsonrpc": "2.0", "id": "c", "result": {"configOptions": [{"id": "effort", "name": "Effort", "type": "select", "currentValue": "medium", "options": [{"value": "low", "name": "Low"}]}]}})),
    ]);
    let (client, agent) = replay(frames, None).await;
    let mut session = client.new_session(SessionOptions::new("/tmp/x")).await.unwrap();
    let error = session.set_config(&ConfigKey::Effort, "ultra").await.unwrap_err();
    assert!(matches!(&error, AcpError::ConfigValueRejected { id, allowed, .. } if id == "effort" && allowed.len() == 6), "{error}");
    let error = session.set_config(&ConfigKey::Id("nonexistent".into()), "x").await.unwrap_err();
    assert_eq!(error, AcpError::ConfigUnavailable { key: "nonexistent".into() });
    let error = session.set_config(&ConfigKey::Effort, "low").await.unwrap_err();
    assert_eq!(error, AcpError::ConfigNotApplied { id: "effort".into(), requested: "low".into(), current: Some("medium".into()) });
    let received = agent.await.unwrap();
    assert_eq!(
        sent(&received, "session/set_config_option")["params"],
        json!({"sessionId": session.id(), "configId": "effort", "value": "low"})
    );
}

#[test]
fn yolo_prefers_allow_once_and_otherwise_cancels() {
    let option = |id: &str, kind| PermissionOption { id: id.into(), name: id.into(), kind };
    let options = [
        option("always", PermissionKind::AllowAlways),
        option("once", PermissionKind::AllowOnce),
        option("no", PermissionKind::RejectOnce),
    ];
    assert_eq!(yolo_choice(&options), PermissionDecision::Selected { option_id: "once".into() });
    assert_eq!(yolo_choice(&options[..1]), PermissionDecision::Selected { option_id: "always".into() });
    assert_eq!(yolo_choice(&options[2..]), PermissionDecision::Cancelled);
    assert_eq!(PermissionDecision::Cancelled.to_acp(), json!({"outcome": {"outcome": "cancelled"}}));
}

#[tokio::test]
async fn a_missing_adapter_is_a_typed_error() {
    let config = AcpAgentConfig::new("claude", "/nonexistent/claude-agent-acp");
    let error = AcpClient::spawn(config).await.unwrap_err();
    assert!(matches!(error, AcpError::AgentNotFound { ref command, .. } if command == "/nonexistent/claude-agent-acp"), "{error}");
}

#[tokio::test]
async fn an_agent_that_dies_reports_its_exit_code_and_a_redacted_stderr_tail() {
    let config = AcpAgentConfig::new("fake", "/bin/sh").with_args(["-c", "echo 'boom: token=abc123' >&2; exit 3"]);
    let error = AcpClient::spawn(config).await.unwrap_err();
    let AcpError::AgentExited { code, stderr_tail } = &error else { panic!("{error}") };
    assert_eq!(*code, Some(3));
    assert!(stderr_tail.contains("boom") && stderr_tail.contains("token=***") && !stderr_tail.contains("abc123"), "{stderr_tail}");
}

#[tokio::test]
async fn dropping_the_client_kills_the_agents_whole_process_group() {
    let dir = std::env::temp_dir().join(format!("aim-acp-pgroup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("child.pid");
    // The "agent" starts a grandchild (like node → claude) and never answers `initialize`.
    let script = format!("sleep 60 & echo $! > '{}'; sleep 60", pid_file.display());
    let config = AcpAgentConfig::new("fake", "/bin/sh").with_args(["-c", script.as_str()]);
    let error = AcpClient::builder(config).request_timeout(Duration::from_millis(500)).spawn().await.unwrap_err();
    assert!(matches!(error, AcpError::Timeout { ref method, .. } if method == "initialize"), "{error}");
    let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_owned();
    let mut alive = true;
    for _ in 0..40 {
        alive = std::process::Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!alive, "grandchild {pid} survived the client");
}
