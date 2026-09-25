use std::sync::Arc;
use std::time::Instant;

use aim_llm::LlmErrorKind;
use serde_json::json;

use super::*;
use crate::CodexProvider;
use crate::auth::AuthManager;
use crate::fake::{self, FakeServer, MemoryStore, Reply, unix_now};

fn client(server: &FakeServer) -> MediaClient {
    client_with(
        server,
        MediaConfig {
            search_model: Some("gpt-6-luna".into()),
            image_model: Some("gpt-image-2.5-sunburst".into()),
            ..MediaConfig::default()
        },
    )
}

fn client_with(server: &FakeServer, media: MediaConfig) -> MediaClient {
    let config = server.config();
    let http = fake::client(&config);
    let store = MemoryStore::new(Some(fake::credentials(unix_now() + 3_600, Some("rt"))));
    let auth = Arc::new(AuthManager::with_config(http.clone(), store, &config));
    let provider = Arc::new(CodexProvider::with_auth(config, http, auth));
    MediaClient::with_provider(provider, media)
}

fn catalog_media() -> MediaConfig {
    MediaConfig { search_model: None, search_from_catalog: true, image_model: Some(DEFAULT_IMAGE_MODEL.into()), ..MediaConfig::default() }
}

#[tokio::test]
async fn preflight_chooses_visible_tools_capable_catalog_default() {
    let catalog = json!({"models":[
        {"slug":"hidden-default","is_default":true,"visibility":"hide"},
        {"slug":"no-tools","is_default":true,"visibility":"list","tool_mode":"none"},
        {"slug":"code-only","is_default":true,"visibility":"list","tool_mode":"code_mode_only"},
        {"slug":"first-visible","visibility":"list"},
        {"slug":"chosen-default","is_default":true,"visibility":"list"}
    ]});
    let server = FakeServer::start(move |request, _| match request.path() {
        "/backend-api/codex/models" => Reply::json(200, &catalog),
        "/backend-api/codex/responses" => {
            let mut events = include_bytes!("../../fixtures/media_search.sse").to_vec();
            events.push(b'\n');
            Reply::sse(&events)
        }
        _ => Reply::text(404, "missing"),
    })
    .await;
    let media = client_with(&server, catalog_media());
    assert!(media.search_enabled());
    assert!(media.image_enabled());
    assert!(media.has_credentials().await);
    assert!(media.search_enabled());
    assert!(server.requests().await.is_empty(), "the catalog is deferred until web search is called");
    media.web_search("capital of France").await.unwrap();
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path(), "/backend-api/codex/models");
    assert_eq!(requests[1].json()["model"], "chosen-default");
}

#[tokio::test]
async fn direct_search_falls_back_to_first_usable_catalog_model() {
    let catalog = json!({"models":[
        {"slug":"hidden","visibility":"hide"},
        {"slug":"no-tools","visibility":"list","tool_mode":"none"},
        {"slug":"first-usable","visibility":"list"},
        {"slug":"second-usable","visibility":"list"}
    ]});
    let server = FakeServer::start(move |request, _| match request.path() {
        "/backend-api/codex/models" => Reply::json(200, &catalog),
        "/backend-api/codex/responses" => {
            let mut events = include_bytes!("../../fixtures/media_search.sse").to_vec();
            events.push(b'\n');
            Reply::sse(&events)
        }
        _ => Reply::text(404, "missing"),
    })
    .await;
    let media = client_with(&server, catalog_media());
    media.web_search("capital of France").await.unwrap();
    assert!(media.search_enabled());
    let requests = server.requests().await;
    assert_eq!(requests[1].json()["model"], "first-usable");
}

#[tokio::test]
async fn failed_catalog_disables_only_search() {
    let server = FakeServer::start(|_, _| Reply::json(500, &json!({"error":{"message":"unavailable"}}))).await;
    let media = client_with(&server, catalog_media());
    assert!(media.has_credentials().await);
    assert!(media.search_enabled(), "catalog discovery is attempted when search is called");
    assert!(media.image_enabled());
    assert_eq!(media.web_search("test").await.unwrap_err().kind, LlmErrorKind::Unavailable);
}

#[test]
fn blank_configured_model_disables_capability() {
    let config =
        MediaConfig { search_model: nonblank("  "), search_from_catalog: false, image_model: nonblank(" "), ..MediaConfig::default() };
    assert!(config.search_model.is_none() && config.image_model.is_none());
    assert!(!config.search_from_catalog);
}

#[test]
fn redacted_fixtures_parse() {
    let image = parse_image(include_bytes!("../../fixtures/media_image.json")).unwrap();
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.bytes.len(), 8);
    assert_eq!(image.usage.unwrap()["output_tokens"], 2);
    assert_eq!(parse_transcript(include_bytes!("../../fixtures/media_transcript.json")).unwrap(), "Test dictation.");
}

#[tokio::test]
async fn web_search_uses_isolated_hosted_turn() {
    let server = FakeServer::start(|_, _| {
        let mut events = include_bytes!("../../fixtures/media_search.sse").to_vec();
        events.push(b'\n');
        Reply::sse(&events)
    })
    .await;
    let answer = client(&server).web_search("capital of France").await.unwrap();
    assert_eq!(answer.text, "Paris");
    assert_eq!(answer.queries, ["capital of France"]);
    assert_eq!(answer.citations.len(), 1);
    let requests = server.requests().await;
    let request = requests.first().unwrap();
    assert_eq!(request.path(), "/backend-api/codex/responses");
    assert_eq!(request.header("session-id"), None);
    assert_eq!(request.header("originator"), Some("aim"));
    assert_eq!(request.json()["tools"], json!([{"type":"web_search","external_web_access":true}]));
    assert_eq!(request.json()["tool_choice"], "required");
}

#[tokio::test]
async fn web_search_accepts_uncited_answer_and_unknown_item() {
    let server = FakeServer::start(|_, _| {
        let mut events = include_bytes!("../../fixtures/media_search_uncited.sse").to_vec();
        events.push(b'\n');
        Reply::sse(&events)
    })
    .await;
    let answer = client(&server).web_search("no results").await.unwrap();
    assert_eq!(answer.text, "No results found.");
    assert!(answer.citations.is_empty());
    assert_eq!(answer.queries, ["no results"]);
}

#[test]
fn search_citation_spans_are_extracted() {
    let items = [
        Item::Hosted {
            native: aim_proto::conversation::NativeItem {
                provider: "codex".into(),
                value: json!({"type":"web_search_call","status":"completed","action":{"queries":["test"]}}),
            },
        },
        Item::Assistant {
            id: None,
            parts: vec![Part::Text { text: "Café source".into() }],
            native: Some(aim_proto::conversation::NativeItem {
                provider: "codex".into(),
                value: json!({"content":[{"text":"Café ","annotations":[]},{"text":"source","annotations":[{"type":"url_citation","url":"https://example.test","title":"Example","start_index":0,"end_index":6}]}]}),
            }),
        },
    ];
    let answer = parse_search_items(&items).unwrap();
    assert_eq!(answer.citations, [Citation { url: "https://example.test".into(), title: "Example".into(), start: 5, end: 11 }]);
    let uncited = [items[0].clone(), Item::Assistant { id: None, parts: vec![Part::Text { text: "Uncited answer".into() }], native: None }];
    let answer = parse_search_items(&uncited).unwrap();
    assert_eq!(answer.text, "Uncited answer");
    assert!(answer.citations.is_empty());
    let mut with_unknown = uncited.to_vec();
    with_unknown.push(Item::Hosted {
        native: aim_proto::conversation::NativeItem {
            provider: "codex".into(),
            value: json!({"type":"future_hosted_call","status":"completed"}),
        },
    });
    assert_eq!(parse_search_items(&with_unknown).unwrap().text, "Uncited answer");
}

#[test]
fn image_limit_fits_one_harness_frame_after_base64() {
    assert!(MAX_IMAGE_BYTES.div_ceil(3) * 4 < 16 * 1024 * 1024);
    let encoded = STANDARD.encode(vec![0_u8; MAX_IMAGE_BYTES + 1]);
    let payload = json!({"data":[{"b64_json":encoded}],"output_format":"png"}).to_string();
    let error = parse_image(payload.as_bytes()).unwrap_err();
    assert_eq!(error.kind, LlmErrorKind::Protocol);
    assert!(error.message.contains("too large to store"));
}

#[tokio::test]
async fn image_generation_sends_media_request() {
    let server = FakeServer::start(|_, _| Reply::Full {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: include_bytes!("../../fixtures/media_image.json").to_vec(),
    })
    .await;
    let image = client(&server).generate_image("draw a dot", Some("1024x1024"), Some("low")).await.unwrap();
    assert_eq!(image.bytes.len(), 8);
    let requests = server.requests().await;
    let request = requests.first().unwrap();
    assert_eq!(request.path(), "/backend-api/codex/images/generations");
    assert_eq!(request.json()["model"], "gpt-image-2.5-sunburst");
    assert_eq!(request.json()["quality"], "low");
}

#[tokio::test]
async fn transcription_uses_multipart_and_rejects_oversize() {
    let server = FakeServer::start(|_, _| Reply::Full {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: include_bytes!("../../fixtures/media_transcript.json").to_vec(),
    })
    .await;
    let client = client(&server);
    let err = client.transcribe(&vec![0_u8; MAX_AUDIO_BYTES + 1]).await.unwrap_err();
    assert_eq!(err.kind, LlmErrorKind::InvalidRequest);
    let text = client.transcribe(b"RIFF fake WAV").await.unwrap();
    assert_eq!(text, "Test dictation.");
    let requests = server.requests().await;
    let request = requests.first().unwrap();
    assert_eq!(request.path(), "/backend-api/transcribe");
    assert!(request.header("content-type").is_some_and(|v| v.starts_with("multipart/form-data; boundary=")));
    assert!(request.body.windows(b"name=\"file\"; filename=\"audio.wav\"".len()).any(|w| w == b"name=\"file\"; filename=\"audio.wav\""));
}

#[tokio::test]
async fn service_http_errors_are_sanitized() {
    let server = FakeServer::start(|_, _| Reply::json(403, &json!({"error":{"message":"Bearer SECRET-DO-NOT-PRINT"}}))).await;
    let error = client(&server).generate_image("dot", None, None).await.unwrap_err();
    assert_eq!(error.kind, LlmErrorKind::Auth);
    assert!(!error.message.contains("SECRET-DO-NOT-PRINT"));
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_web_search() {
    let client = MediaClient::new().unwrap();
    let start = Instant::now();
    let answer = client.web_search("What is the capital of France? Cite a source.").await.unwrap();
    assert!(!answer.text.trim().is_empty() && !answer.queries.is_empty());
    eprintln!("live_codex_web_search elapsed_ms={} citations={}", start.elapsed().as_millis(), answer.citations.len());
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_images() {
    let client = MediaClient::new().unwrap();
    let start = Instant::now();
    let image = client.generate_image("A single blue circle on white background", Some("1024x1024"), Some("low")).await.unwrap();
    assert!(!image.bytes.is_empty());
    eprintln!("live_codex_images elapsed_ms={} bytes={} type={}", start.elapsed().as_millis(), image.bytes.len(), image.media_type);
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_transcribe() {
    let wav = std::fs::read(std::env::var("AIM_LIVE_WAV").expect("AIM_LIVE_WAV must name a 24-kHz mono PCM16 speech WAV")).unwrap();
    let client = MediaClient::new().unwrap();
    let start = Instant::now();
    let text = client.transcribe(&wav).await.unwrap();
    assert!(!text.trim().is_empty());
    eprintln!("live_codex_transcribe elapsed_ms={} chars={}", start.elapsed().as_millis(), text.chars().count());
}
