//! Provider-level tests: profile config, construction checks, and the HTTP/SSE glue against a
//! local server serving canned responses.

use super::*;
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::tool::{ToolContent, ToolResult};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// An environment variable `cargo test` always sets; stands in for an API key.
const KEY_ENV: &str = "CARGO_PKG_NAME";
const KEY: &str = "aim-llm-openai";

#[test]
fn profile_toml_with_quirks() -> Result<(), Box<dyn std::error::Error>> {
    let profile: Profile = toml::from_str(
        r#"
id = "custom"
base_url = "https://example.invalid/v1"
api_key_env = "CUSTOM_API_KEY"
wire = "chat"
headers = { "x-title" = "aim" }
[quirks]
min_output_tokens = 16
max_output_tokens_field = "max_completion_tokens"
supports_parallel_tool_calls = true
supports_stream_usage = true
reasoning_param = "open_ai"
cost_pointer = "/cost"
replay_reasoning_details = true
tool_result_images = true
session_header = "x-session-id"
cache_key_field = "prompt_cache_key"
idle_timeout_secs = 30
[quirks.extra_body]
cache_control = { type = "ephemeral" }
"#,
    )?;
    assert_eq!(profile.quirks.effective_max_output_tokens(Some(1)), Some(16));
    assert_eq!(
        profile.quirks.extra_body.as_ref().and_then(|body| body.get("cache_control")),
        Some(&serde_json::json!({"type": "ephemeral"}))
    );
    let provider = OpenAiProvider::new(profile)?;
    assert_eq!(provider.effective_max_output_tokens(Some(100)), Some(100));
    Ok(())
}

#[test]
fn unknown_keys_are_rejected() {
    let typo = toml::from_str::<Profile>(
        "id = \"x\"\nbase_url = \"https://e.invalid\"\napi_key_env = \"K\"\nwire = \"chat\"\n[quirks]\nmin_output_token = 16\n",
    );
    assert!(typo.is_err());
    let top = toml::from_str::<Profile>("id = \"x\"\nbase_url = \"https://e.invalid\"\napi_key_env = \"K\"\nwire = \"chat\"\nwires = 1\n");
    assert!(top.is_err());
}

#[test]
fn construction_rejects_unservable_profiles() {
    let mut responses = Profile::openrouter();
    responses.wire = Wire::Responses;
    assert_eq!(OpenAiProvider::new(responses).err().map(|e| e.kind), Some(LlmErrorKind::InvalidRequest));
    let mut auth = Profile::openrouter();
    auth.headers.insert("Authorization".into(), "Bearer literal".into());
    assert_eq!(OpenAiProvider::new(auth).err().map(|e| e.kind), Some(LlmErrorKind::InvalidRequest));
    let mut bad = Profile::openrouter();
    bad.headers.insert("bad header".into(), "x".into());
    assert_eq!(OpenAiProvider::new(bad).err().map(|e| e.kind), Some(LlmErrorKind::InvalidRequest));
    let mut secret = Profile::openrouter();
    secret.headers.insert("helicone-auth".into(), "Bearer very-secret".into());
    assert!(!format!("{secret:?}").contains("very-secret"));
}

/// Reads one HTTP request (headers plus `content-length` body) from `socket`.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    while let Ok(read) = socket.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        request.extend_from_slice(buffer.get(..read).unwrap_or_default());
        let text = String::from_utf8_lossy(&request).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text
                .lines()
                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
    request
}

/// Serves one canned HTTP response and returns the raw request it received.
async fn serve(response: Vec<u8>, stall: Option<Duration>) -> Result<(String, tokio::task::JoinHandle<Vec<u8>>), std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else { return Vec::new() };
        let request = read_request(&mut socket).await;
        let _written = socket.write_all(&response).await;
        let _flushed = socket.flush().await;
        if let Some(stall) = stall {
            tokio::time::sleep(stall).await;
        }
        let _closed = socket.shutdown().await;
        request
    });
    Ok((base, handle))
}

/// Serves `responses` in order, one per connection (each says `connection: close`), and returns
/// the raw requests received. Stops early if no further connection arrives within a second.
async fn serve_sequence(responses: Vec<Vec<u8>>) -> Result<(String, tokio::task::JoinHandle<Vec<String>>), std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let Ok(Ok((mut socket, _))) = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await else { break };
            requests.push(String::from_utf8_lossy(&read_request(&mut socket).await).into_owned());
            let _written = socket.write_all(&response).await;
            let _flushed = socket.flush().await;
            let _closed = socket.shutdown().await;
        }
        requests
    });
    Ok((base, handle))
}

fn sse_response(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}").into_bytes()
}

fn json_response(status: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
        .into_bytes()
}

/// A finished text answer with the usage chunk a `supports_stream_usage` profile requires.
const ANSWER: &str = "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
                      data: {\"id\":\"r\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n\
                      data: [DONE]\n\n";

fn local(base: String) -> Profile {
    let mut profile = Profile::openrouter();
    profile.base_url = base;
    profile.api_key_env = KEY_ENV.into();
    profile
}

fn request() -> Request {
    Request {
        model: "openai/gpt-4.1-mini".into(),
        instructions: "System".into(),
        items: vec![Item::User { parts: vec![Part::Text { text: "Hi".into() }] }],
        tools: Vec::new(),
        effort: None,
        tier: None,
        cache_key: None,
        session_id: Some("session-1".into()),
        turn_id: Some("turn-1".into()),
        parallel_tool_calls: false,
        max_output_tokens: None,
    }
}

async fn collect(provider: &OpenAiProvider, request: Request) -> Result<Vec<StreamEvent>, LlmError> {
    let mut stream = provider.stream(request).await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

#[tokio::test]
async fn streams_a_fixture_and_sends_affinity_headers() -> Result<(), Box<dyn std::error::Error>> {
    let (base, server) = serve(sse_response(include_str!("../fixtures/openrouter_tools.sse")), None).await?;
    let provider = OpenAiProvider::new(local(base))?;
    let events = collect(&provider, request()).await?;
    let calls = events.iter().filter(|event| matches!(event, StreamEvent::ItemDone { item: Item::ToolCall { .. } })).count();
    assert_eq!(calls, 2);
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { stop: StopReason::ToolUse, usage, .. }) if usage.cost_micro_usd == Some(103))
    );
    let sent = String::from_utf8(server.await?)?;
    let lower = sent.to_ascii_lowercase();
    assert!(lower.contains(&format!("authorization: bearer {KEY}")));
    assert!(lower.contains("x-session-id: session-1"));
    assert!(sent.contains("\"cache_control\":{\"type\":\"ephemeral\"}"));
    Ok(())
}

#[tokio::test]
async fn http_errors_keep_scrubbed_detail_and_retry_after() -> Result<(), Box<dyn std::error::Error>> {
    let body = format!(r#"{{"error":{{"message":"slow down, {KEY}","code":429}}}}"#);
    let response = format!("HTTP/1.1 429 Too Many Requests\r\nretry-after: 3\r\ncontent-length: {}\r\n\r\n{body}", body.len());
    let (base, _server) = serve(response.into_bytes(), None).await?;
    let error = collect(&OpenAiProvider::new(local(base))?, request()).await.err().ok_or("expected an error")?;
    assert_eq!(error.kind, LlmErrorKind::RateLimited);
    assert_eq!(error.retry_after_ms, Some(3000));
    assert!(error.message.contains("slow down") && !error.message.contains(KEY));
    Ok(())
}

#[tokio::test]
async fn a_stream_cut_after_finish_is_a_protocol_error() -> Result<(), Box<dyn std::error::Error>> {
    let cut = "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
    let (base, _server) = serve(sse_response(cut), None).await?;
    let error = collect(&OpenAiProvider::new(local(base))?, request()).await.err().ok_or("expected an error")?;
    assert_eq!(error.kind, LlmErrorKind::Protocol);

    // Without a trailing blank line the final `[DONE]` is still honoured (usage not requested).
    let (base, _server) = serve(sse_response(&format!("{cut}data: [DONE]")), None).await?;
    let mut quiet = local(base);
    quiet.quirks.supports_stream_usage = false;
    let events = collect(&OpenAiProvider::new(quiet)?, request()).await?;
    assert!(matches!(events.last(), Some(StreamEvent::Completed { stop: StopReason::EndTurn, .. })));
    Ok(())
}

#[tokio::test]
async fn mid_stream_error_chunks_are_classified() -> Result<(), Box<dyn std::error::Error>> {
    let body = "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}],\"error\":null}\n\n\
                data: {\"id\":\"r\",\"error\":{\"code\":429,\"message\":\"Rate limit exceeded upstream\"},\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}]}\n\n";
    let (base, _server) = serve(sse_response(body), None).await?;
    let error = collect(&OpenAiProvider::new(local(base))?, request()).await.err().ok_or("expected an error")?;
    assert_eq!(error.kind, LlmErrorKind::RateLimited);
    assert!(error.message.contains("Rate limit exceeded upstream"));
    Ok(())
}

#[tokio::test]
async fn an_idle_stream_times_out_as_transport() -> Result<(), Box<dyn std::error::Error>> {
    let partial = sse_response("data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n");
    let (base, _server) = serve(partial, Some(Duration::from_secs(5))).await?;
    let mut profile = local(base);
    profile.quirks.idle_timeout_secs = Some(1);
    let started = std::time::Instant::now();
    let error = collect(&OpenAiProvider::new(profile)?, request()).await.err().ok_or("expected an error")?;
    assert_eq!(error.kind, LlmErrorKind::Transport);
    assert!(started.elapsed() < Duration::from_secs(4));
    Ok(())
}

fn model_info(id: &str, images: bool) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        display_name: id.into(),
        context_window: None,
        efforts: Vec::new(),
        default_effort: None,
        tiers: Vec::new(),
        tools: true,
        images,
        hidden: false,
        native: None,
    }
}

/// Known vision models get image parts, known text-only models and models the provider has not
/// seen (REV4-B Minor 5) get the text placeholder.
#[test]
fn catalog_images_gate_tool_result_images() -> Result<(), Box<dyn std::error::Error>> {
    let mut profile = Profile::openrouter();
    profile.models = Some(vec![model_info("text/only", false), model_info("vision/model", true)]);
    let provider = OpenAiProvider::new(profile)?;
    for (model, sees_images) in [("vision/model", true), ("text/only", false), ("never/seen", false)] {
        let body = provider.request_body(&with_tool_image(request(), model))?.to_string();
        assert_eq!(body.contains("image_url"), sees_images, "{model}");
        assert_eq!(body.contains("cannot view"), !sees_images, "{model}");
    }
    Ok(())
}

/// REV4-B Major 1: an error reported inside the stream keeps its detail but never the key.
#[tokio::test]
async fn in_stream_errors_are_scrubbed_of_the_key() -> Result<(), Box<dyn std::error::Error>> {
    let body = format!("data: {{\"error\":{{\"code\":400,\"message\":\"rejected {KEY}\"}}}}\n\n");
    let (base, _server) = serve(sse_response(&body), None).await?;
    let error = collect(&OpenAiProvider::new(local(base))?, request()).await.err().ok_or("expected an error")?;
    assert_eq!(error.kind, LlmErrorKind::InvalidRequest);
    assert!(error.message.contains("rejected"), "detail kept: {}", error.message);
    assert!(!error.message.contains(KEY), "key scrubbed: {}", error.message);
    Ok(())
}

/// REV4-B Major 2: a profile that requests streamed usage never reports a completed turn with
/// usage it did not receive — neither after `[DONE]` without usage nor after `usage: {}` and EOF.
#[tokio::test]
async fn promised_usage_that_never_arrives_is_a_protocol_error() -> Result<(), Box<dyn std::error::Error>> {
    let finished = "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
    for body in [format!("{finished}data: [DONE]\n\n"), format!("{finished}data: {{\"id\":\"r\",\"choices\":[],\"usage\":{{}}}}\n\n")] {
        let (base, _server) = serve(sse_response(&body), None).await?;
        let outcome = collect(&OpenAiProvider::new(local(base))?, request()).await;
        assert_eq!(outcome.err().map(|error| error.kind), Some(LlmErrorKind::Protocol), "{body}");
    }
    let (base, _server) = serve(sse_response(ANSWER), None).await?;
    let events = collect(&OpenAiProvider::new(local(base))?, request()).await?;
    assert!(matches!(events.last(), Some(StreamEvent::Completed { usage, .. }) if usage.input_tokens == 5 && usage.output_tokens == 1));
    Ok(())
}

fn with_tool_image(mut request: Request, model: &str) -> Request {
    model.clone_into(&mut request.model);
    request.items.push(Item::ToolCall { call_id: "a".into(), name: "shot".into(), arguments: "{}".into(), native: None });
    request.items.push(Item::ToolResult {
        call_id: "a".into(),
        result: ToolResult {
            content: vec![ToolContent::Image { media_type: "image/png".into(), data: aim_proto::content::Base64Bytes(vec![1]) }],
            ..ToolResult::default()
        },
    });
    request
}

const CATALOG: &str = r#"{"data":[
    {"id":"text/only","architecture":{"input_modalities":["text"]},"supported_parameters":["tools"]},
    {"id":"vision/model","architecture":{"input_modalities":["text","image"]},"supported_parameters":["tools"]}
]}"#;

/// REV4-B Minor 5: a model the provider has not seen yet learns its image support from the
/// catalog before a tool-result image is mapped; a text-only model gets the placeholder.
#[tokio::test]
async fn unknown_models_learn_image_support_from_the_catalog_first() -> Result<(), Box<dyn std::error::Error>> {
    for (model, sees_images) in [("text/only", false), ("vision/model", true)] {
        let (base, server) = serve_sequence(vec![json_response("200 OK", CATALOG), sse_response(ANSWER)]).await?;
        let provider = OpenAiProvider::new(local(base))?;
        let events = collect(&provider, with_tool_image(request(), model)).await?;
        assert!(matches!(events.last(), Some(StreamEvent::Completed { .. })));
        let requests = server.await?;
        assert_eq!(requests.len(), 2, "{model}: catalog, then chat");
        assert!(requests.first().is_some_and(|sent| sent.starts_with("GET /v1/models")), "{model}: catalog first");
        let chat = requests.get(1).ok_or("no chat request")?;
        assert!(chat.starts_with("POST /v1/chat/completions"));
        assert_eq!(chat.contains("image_url"), sees_images, "{model}");
        assert_eq!(chat.contains("cannot view"), !sees_images, "{model}");
    }
    Ok(())
}

/// A catalog that cannot be fetched does not fail the turn: the model is treated as unable to
/// see images, and the catalog is not fetched again for the next image.
#[tokio::test]
async fn an_unavailable_catalog_falls_back_to_the_placeholder() -> Result<(), Box<dyn std::error::Error>> {
    let (base, server) = serve_sequence(vec![
        json_response("404 Not Found", r#"{"error":{"message":"no such route"}}"#),
        sse_response(ANSWER),
        sse_response(ANSWER),
    ])
    .await?;
    let provider = OpenAiProvider::new(local(base))?;
    collect(&provider, with_tool_image(request(), "vision/model")).await?;
    collect(&provider, with_tool_image(request(), "vision/model")).await?;
    let requests = server.await?;
    assert_eq!(requests.iter().filter(|sent| sent.starts_with("GET")).count(), 1, "one catalog attempt");
    assert!(
        requests.iter().filter(|sent| sent.starts_with("POST")).all(|chat| !chat.contains("image_url") && chat.contains("cannot view"))
    );
    Ok(())
}

/// A static `models` list is authoritative: an unlisted model is never looked up and gets no images.
#[tokio::test]
async fn a_static_catalog_is_never_fetched() -> Result<(), Box<dyn std::error::Error>> {
    let (base, server) = serve(sse_response(ANSWER), None).await?;
    let mut profile = local(base);
    profile.models = Some(Vec::new());
    collect(&OpenAiProvider::new(profile)?, with_tool_image(request(), "vision/model")).await?;
    let sent = String::from_utf8(server.await?)?;
    assert!(sent.starts_with("POST /v1/chat/completions") && !sent.contains("image_url"));
    Ok(())
}
