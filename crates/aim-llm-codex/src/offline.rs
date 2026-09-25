//! Offline end-to-end tests: the real provider (production client builder, auth manager,
//! request mapping, stream driver) against a local fake backend.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aim_llm::{LlmErrorKind, ModelProvider as _, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason};
use futures_util::StreamExt as _;
use serde_json::{Value, json};

use crate::auth::AuthManager;
use crate::fake::{self, FakeServer, MemoryStore, Reply, unix_now};
use crate::{CodexConfig, CodexProvider};

const TEXT_TURN: &[u8] = include_bytes!("../fixtures/text_turn.sse");
const TOOL_TURN: &[u8] = include_bytes!("../fixtures/tool_turn.sse");

fn provider_with(config: CodexConfig) -> CodexProvider {
    let client = fake::client(&config);
    let store = MemoryStore::new(Some(fake::credentials(unix_now() + 3_600, Some("rt"))));
    let auth = Arc::new(AuthManager::with_config(client.clone(), store, &config));
    CodexProvider::with_auth(config, client, auth)
}

fn provider(server: &FakeServer) -> CodexProvider {
    provider_with(server.config())
}

#[tokio::test]
async fn account_usage_is_fresh_bounded_and_authenticated_without_model_calls() {
    let server = FakeServer::start(|_, index| {
        Reply::json(
            200,
            &json!({
                "email": "never-display",
                "rate_limit": {"primary_window": {"used_percent": index, "limit_window_seconds": 18_000, "reset_at": 1_790_000_000}}
            }),
        )
    })
    .await;
    let provider = provider(&server);
    for expected in [0.0, 1.0] {
        let limits = provider.account_limits().await.unwrap();
        assert!((limits.windows[0].used_percent - expected).abs() < f64::EPSILON);
        assert!(limits.native.is_none());
    }
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path(), "/backend-api/wham/usage");
        assert_eq!(request.header("chatgpt-account-id"), Some("acct-fixture"));
        assert!(request.header("authorization").is_some());
        assert_eq!(request.header("cache-control"), Some("no-cache"));
        assert!(request.body.is_empty());
    }
}

#[tokio::test]
async fn account_usage_errors_never_echo_response_bodies_or_follow_redirects() {
    let server = FakeServer::start(|_, index| match index {
        0 => Reply::text(401, "private-account-data"),
        1 => Reply::text(302, "private-account-data").header("location", "/do-not-follow"),
        2 => Reply::text(200, "{}"),
        3 => Reply::text(200, &"x".repeat(256 * 1024 + 1)),
        _ => Reply::Silent,
    })
    .await;
    let provider = provider_with(CodexConfig { request_timeout: Duration::from_millis(100), ..server.config() });
    for kind in [LlmErrorKind::Auth, LlmErrorKind::Unavailable, LlmErrorKind::Protocol, LlmErrorKind::Protocol, LlmErrorKind::Transport] {
        let error = provider.account_limits().await.unwrap_err();
        assert_eq!(error.kind, kind);
        assert!(!error.message.contains("private-account-data"));
    }
    assert_eq!(server.requests().await.len(), 5);
}

fn request(session: Option<&str>, turn: Option<&str>) -> Request {
    Request {
        model: "gpt-6-luna".into(),
        instructions: "Reply briefly.".into(),
        items: vec![Item::User { parts: vec![Part::Text { text: "Reply OK".into() }] }],
        tools: vec![],
        effort: Some("low".into()),
        tier: None,
        cache_key: Some("ws".into()),
        session_id: session.map(str::to_owned),
        turn_id: turn.map(str::to_owned),
        parallel_tool_calls: false,
        max_output_tokens: None,
    }
}

async fn collect(provider: &CodexProvider, request: Request) -> Result<Vec<StreamEvent>, aim_llm::LlmError> {
    let mut stream = provider.stream(request).await?;
    let mut events = Vec::new();
    while let Some(next) = stream.next().await {
        events.push(next?);
    }
    Ok(events)
}

#[tokio::test]
async fn stream_sends_the_documented_request_and_parses_the_reply() {
    let server = FakeServer::start(|_, _| {
        Reply::sse(TEXT_TURN)
            .header("x-codex-primary-used-percent", "24")
            .header("x-codex-primary-window-minutes", "10080")
            .header("x-codex-secondary-used-percent", "3")
            .header("x-codex-secondary-window-minutes", "300")
            .header("x-codex-plan-type", "pro")
    })
    .await;
    let provider = provider(&server);
    let events = collect(&provider, request(Some("session-1"), Some("turn-1"))).await.unwrap();
    let recorded = &server.requests().await[0];
    assert_eq!((recorded.method.as_str(), recorded.path()), ("POST", "/backend-api/codex/responses"));
    let token = fake::jwt(0, "").len();
    assert!(recorded.header("authorization").is_some_and(|v| v.starts_with("Bearer e30.") && v.len() > token / 2));
    assert_eq!(recorded.header("chatgpt-account-id"), Some("acct-fixture"));
    assert_eq!(recorded.header("originator"), Some("aim"));
    assert_eq!(recorded.header("user-agent"), Some(concat!("aim/", env!("CARGO_PKG_VERSION"))));
    assert_eq!(recorded.header("accept"), Some("text/event-stream"));
    assert_eq!(recorded.header("session-id"), Some("session-1"));
    assert_eq!(recorded.header("x-codex-turn-state"), None);
    let body = recorded.json();
    assert_eq!(body["model"], "gpt-6-luna");
    assert_eq!(body["input"], json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply OK"}]}]));
    assert_eq!(body["reasoning"], json!({"effort":"low","summary":"auto"}));
    assert_eq!((body["store"].clone(), body["stream"].clone(), body["prompt_cache_key"].clone()), (json!(false), json!(true), json!("ws")));
    assert!(body.get("max_output_tokens").is_none());
    assert!(matches!(events[0], StreamEvent::Created { .. }));
    let StreamEvent::RateLimits { limits } = &events[1] else { panic!("RateLimits must follow Created: {events:?}") };
    let ids: Vec<&str> = limits.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["codex.primary", "codex.secondary"]);
    assert_eq!(limits.native.as_ref().unwrap()["x-codex-plan-type"], "pro");
    assert!(matches!(events.last(), Some(StreamEvent::Completed { stop: StopReason::EndTurn, .. })));
}

#[tokio::test]
async fn a_stream_may_run_longer_than_the_idle_timeout_while_data_flows() {
    // 1.5 s of keep-alives 150 ms apart under a 500 ms idle timeout, through the real client.
    let server = FakeServer::start(|_, _| {
        let mut steps = vec![(Duration::ZERO, b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n".to_vec())];
        steps.extend((0..10).map(|_| (Duration::from_millis(150), b": keep-alive\n\n".to_vec())));
        steps.push((Duration::from_millis(150), b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n".to_vec()));
        Reply::Steps { headers: vec![("content-type".into(), "text/event-stream".into())], steps, hang: false }
    })
    .await;
    let provider = provider_with(CodexConfig { idle_timeout: Duration::from_millis(500), ..server.config() });
    let started = Instant::now();
    let events = collect(&provider, request(None, None)).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1_600), "{:?}", started.elapsed());
    assert!(matches!(events.last(), Some(StreamEvent::Completed { .. })));
}

#[tokio::test]
async fn silence_is_an_idle_timeout_before_and_after_the_headers() {
    let server = FakeServer::start(|_, index| {
        if index == 0 {
            Reply::Silent
        } else {
            let created = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n".to_vec();
            Reply::Steps { headers: vec![], steps: vec![(Duration::ZERO, created)], hang: true }
        }
    })
    .await;
    let provider = provider_with(CodexConfig { idle_timeout: Duration::from_millis(300), ..server.config() });
    let started = Instant::now();
    let error = provider.stream(request(None, None)).await.err().unwrap();
    assert_eq!(error.kind, LlmErrorKind::Transport);
    assert!(error.message.contains("idle timeout"), "{}", error.message);
    assert!(started.elapsed() < Duration::from_secs(3));
    let mut stream = provider.stream(request(None, None)).await.unwrap();
    assert!(matches!(stream.next().await, Some(Ok(StreamEvent::Created { .. }))));
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(error.kind, LlmErrorKind::Transport);
    assert!(error.message.contains("idle timeout"), "{}", error.message);
}

#[tokio::test]
async fn turn_state_is_echoed_within_a_turn_and_never_across_turns() {
    let server = FakeServer::start(|_, index| {
        let fixture = if index == 0 { TOOL_TURN } else { TEXT_TURN };
        Reply::sse(fixture).header("x-codex-turn-state", &format!("state-{index}"))
    })
    .await;
    let provider = provider(&server);
    let turns = [
        (Some("s"), Some("t1")), // turn start: server sets state-0
        (Some("s"), Some("t1")), // tool follow-up: echoes state-0 (state-1 is ignored)
        (Some("s"), Some("t1")), // still the first value
        (Some("s"), Some("t2")), // next user turn: nothing, then state-3 is kept
        (Some("s"), Some("t2")), // echoes state-3
        (Some("s"), None),       // no turn id: never sent
        (Some("other"), Some("t2")),
    ];
    for (session, turn) in turns {
        collect(&provider, request(session, turn)).await.unwrap();
    }
    let sent: Vec<Option<String>> = server.requests().await.iter().map(|r| r.header("x-codex-turn-state").map(str::to_owned)).collect();
    let expected = [None, Some("state-0"), Some("state-0"), None, Some("state-3"), None, None];
    assert_eq!(sent, expected.map(|s| s.map(str::to_owned)));
}

#[tokio::test]
async fn http_errors_are_classified_with_bounded_bodies() {
    let huge = "x".repeat(1024 * 1024);
    let server = FakeServer::start(move |_, index| match index {
        0 => Reply::json(429, &json!({"error":{"type":"usage_limit_reached","plan_type":"pro","resets_at":unix_now() + 600}}))
            .header("x-codex-primary-used-percent", "100"),
        1 => Reply::text(429, "slow down").header("retry-after", "3"),
        2 => Reply::json(400, &json!({"detail":"Unsupported parameter: max_output_tokens"})),
        3 => Reply::json(400, &json!({"error":{"code":"context_length_exceeded","message":"too long"}})),
        4 => Reply::text(502, &huge),
        _ => Reply::json(401, &json!({"detail":"token expired"})),
    })
    .await;
    let provider = provider(&server);
    let mut errors = Vec::new();
    for _ in 0..6 {
        errors.push(provider.stream(request(None, None)).await.err().unwrap());
    }
    assert_eq!(errors[0].kind, LlmErrorKind::RateLimited);
    assert!(errors[0].retry_after_ms.is_some_and(|ms| (590_000..=600_000).contains(&ms)), "{:?}", errors[0].retry_after_ms);
    assert_eq!((errors[1].kind, errors[1].retry_after_ms, errors[1].status), (LlmErrorKind::RateLimited, Some(3_000), Some(429)));
    assert_eq!(errors[2].kind, LlmErrorKind::InvalidRequest);
    assert!(errors[2].message.ends_with("Unsupported parameter: max_output_tokens"), "{}", errors[2].message);
    assert_eq!(errors[3].kind, LlmErrorKind::ContextOverflow);
    assert_eq!(errors[4].kind, LlmErrorKind::Unavailable);
    assert!(errors[4].message.chars().count() < 400, "bounded message");
    assert_eq!(errors[5].kind, LlmErrorKind::Auth);
}

#[tokio::test]
async fn catalog_uses_the_configured_version_and_etag() {
    let catalog = json!({"models":[
        {"slug":"gpt-6-luna","display_name":"GPT-6 Luna","context_window":272_000,"visibility":"list",
         "supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"},{"effort":"max"}],"default_reasoning_level":"medium",
         "service_tiers":[{"id":"priority","name":"Fast"}],"supports_reasoning_summary_parameter":false},
        {"slug":"codex-auto-review","visibility":"hide"}]});
    let server = FakeServer::start(move |request, _| match (request.path(), request.header("if-none-match")) {
        ("/backend-api/codex/models", None) => Reply::json(200, &catalog).header("etag", "\"v1\""),
        ("/backend-api/codex/models", Some(_)) => Reply::Full { status: 304, headers: vec![], body: vec![] },
        _ => Reply::sse(TEXT_TURN),
    })
    .await;
    let provider = provider(&server);
    let first = provider.catalog().await.unwrap();
    let second = provider.catalog().await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first[0].efforts, ["low", "xhigh", "max"]);
    assert!(first[1].hidden);
    let requests = server.requests().await;
    assert_eq!(requests[0].target, "/backend-api/codex/models?client_version=9.9.9");
    assert_eq!(requests[1].header("if-none-match"), Some("\"v1\""));
    // Catalog data shapes the request: this model takes no `reasoning.summary`.
    collect(&provider, request(None, None)).await.unwrap();
    assert_eq!(server.requests().await[2].json()["reasoning"], json!({"effort":"low"}));
}

fn compaction_stream(items: &[Value]) -> Vec<u8> {
    let mut text = String::from("data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n");
    for item in items {
        text.push_str(&["data: ", &json!({"type":"response.output_item.done","item":item}).to_string(), "\n\n"].concat());
    }
    text.push_str("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n");
    text.into_bytes()
}

#[tokio::test]
async fn compaction_returns_the_single_encrypted_item() {
    let good = json!({"type":"compaction","id":"cmp_1","encrypted_content":"opaque"});
    let server = FakeServer::start(move |_, index| match index {
        0 => Reply::sse(&compaction_stream(std::slice::from_ref(&good))),
        1 => Reply::sse(&compaction_stream(&[good.clone(), good.clone()])),
        2 => Reply::sse(&compaction_stream(&[json!({"type":"compaction","encrypted_content":""})])),
        _ => Reply::sse(&compaction_stream(&[])),
    })
    .await;
    let provider = provider(&server);
    let item = provider.compact(request(None, None)).await.unwrap();
    assert!(matches!(item, Item::Compaction { ref native } if native.value["encrypted_content"] == "opaque"));
    let input = server.requests().await[0].json()["input"].clone();
    assert_eq!(input.as_array().unwrap().last().unwrap(), &json!({"type":"compaction_trigger"}));
    for expected in ["several compaction items", "no encrypted content", "no compaction item"] {
        let error = provider.compact(request(None, None)).await.unwrap_err();
        assert!(error.message.contains(expected), "{}", error.message);
    }
}
