//! Service tools share the model's tool namespace with aimx, but never replace aimx names.

use std::collections::HashSet;
use std::sync::Arc;

use aim_llm::LlmError;
use aim_llm_codex::media::{Image, MAX_IMAGE_BYTES, MediaClient, SearchAnswer};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde_json::{Value, json};

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

/// Credential-local media service capabilities that a session may expose to its model.
pub trait MediaService: Send + Sync {
    /// Whether hosted web search is configured.
    fn search_enabled(&self) -> bool;
    /// Whether image generation is configured.
    fn image_enabled(&self) -> bool;
    /// Runs an independent hosted web search.
    fn web_search(&self, query: String) -> BoxFuture<Result<SearchAnswer, LlmError>>;
    /// Generates an image for a tool call.
    fn generate_image(&self, prompt: String, size: Option<String>, quality: Option<String>) -> BoxFuture<Result<Image, LlmError>>;
}

impl MediaService for MediaClient {
    fn search_enabled(&self) -> bool {
        self.search_enabled()
    }

    fn image_enabled(&self) -> bool {
        self.image_enabled()
    }

    fn web_search(&self, query: String) -> BoxFuture<Result<SearchAnswer, LlmError>> {
        let client = self.clone();
        Box::pin(async move { client.web_search(&query).await })
    }

    fn generate_image(&self, prompt: String, size: Option<String>, quality: Option<String>) -> BoxFuture<Result<Image, LlmError>> {
        let client = self.clone();
        Box::pin(async move { client.generate_image(&prompt, size.as_deref(), quality.as_deref()).await })
    }
}

/// Combines a session's workspace tools with local media services. Workspace names win.
pub struct Dispatcher {
    workspace: Arc<dyn ToolHost>,
    media: Arc<dyn MediaService>,
    workspace_names: HashSet<String>,
    search_enabled: bool,
    image_enabled: bool,
    allow_services: bool,
}

impl Dispatcher {
    /// Composes the workspace with Codex media tools.
    #[must_use]
    pub fn new(workspace: Arc<dyn ToolHost>, media: Arc<MediaClient>) -> Self {
        Self::with_policy(workspace, media, true)
    }

    /// Composes test or production services under an explicit session access decision. A private
    /// session must pass `false` until its user explicitly opts in to sending media prompts.
    #[must_use]
    pub fn with_policy(workspace: Arc<dyn ToolHost>, media: Arc<dyn MediaService>, allow_services: bool) -> Self {
        let workspace_names = workspace.specs().into_iter().map(|spec| spec.name).collect();
        let search_enabled = media.search_enabled();
        let image_enabled = media.image_enabled();
        Self { workspace, media, workspace_names, search_enabled, image_enabled, allow_services }
    }
}

fn spec(name: &str, description: &str, input_schema: Value, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        input: ToolInput::Json,
        annotations: ToolAnnotations {
            read_only,
            destructive: !read_only,
            idempotent: false,
            open_world: true,
            location: ToolLocation::LocalService,
        },
    }
}

fn service_specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "web_search",
            "Search the public web with Codex. The query is sent to the service. Cite returned source URLs when present; an uncited answer must be identified as such.",
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}),
            true,
        ),
        spec(
            "generate_image",
            "Generate an image with Codex, write it into this session's workspace, and return its path. The prompt is sent to the service.",
            json!({"type":"object","properties":{"prompt":{"type":"string"},"path":{"type":"string"},"size":{"type":"string"},"quality":{"type":"string"}},"required":["prompt"],"additionalProperties":false}),
            false,
        ),
    ]
}

fn required_string<'a>(arguments: &'a Value, field: &str) -> Result<&'a str, ProtoError> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, format!("`{field}` must be a nonempty string")))
}

fn optional_string(arguments: &Value, field: &str) -> Result<Option<String>, ProtoError> {
    match arguments.get(field) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        None => Ok(None),
        _ => Err(ProtoError::new(ErrorCode::InvalidParams, format!("`{field}` must be a nonempty string"))),
    }
}

fn image_extension(media_type: &str) -> &str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

fn image_path(arguments: &Value) -> Result<Option<String>, ProtoError> {
    let path = optional_string(arguments, "path")?;
    if path.as_deref().is_some_and(|path| path.contains('\0') || path.ends_with('/') || path.split('/').any(|part| part == "..")) {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "image path is invalid"));
    }
    Ok(path)
}

impl ToolHost for Dispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.workspace.specs();
        specs.extend(service_specs().into_iter().filter(|spec| {
            self.allow_services
                && !self.workspace_names.contains(&spec.name)
                && match spec.name.as_str() {
                    "web_search" => self.search_enabled,
                    "generate_image" => self.image_enabled,
                    _ => false,
                }
        }));
        specs
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        if self.workspace_names.contains(&name) {
            return self.workspace.call(name, arguments, key);
        }
        match name.as_str() {
            "web_search" if self.allow_services && self.search_enabled => {
                let query = match required_string(&arguments, "query") {
                    Ok(query) => query.to_owned(),
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                let media = Arc::clone(&self.media);
                Box::pin(async move {
                    match media.web_search(query).await {
                        Ok(answer) => {
                            let citations = answer
                                .citations
                                .iter()
                                .map(|citation| {
                                    json!({
                                        "url": citation.url,
                                        "title": citation.title,
                                        "start": citation.start,
                                        "end": citation.end,
                                    })
                                })
                                .collect::<Vec<_>>();
                            let uncited = citations.is_empty();
                            Ok(ToolResult::text(
                                json!({"text":answer.text,"citations":citations,"queries":answer.queries,"uncited":uncited}).to_string(),
                            ))
                        }
                        Err(error) => Ok(ToolResult::error(error.to_string())),
                    }
                })
            }
            "generate_image" if self.allow_services && self.image_enabled => {
                let prompt = match required_string(&arguments, "prompt") {
                    Ok(prompt) => prompt.to_owned(),
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                let size = match optional_string(&arguments, "size") {
                    Ok(size) => size,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                let quality = match optional_string(&arguments, "quality") {
                    Ok(quality) => quality,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                let path = match image_path(&arguments) {
                    Ok(path) => path,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                let media = Arc::clone(&self.media);
                let workspace = Arc::clone(&self.workspace);
                Box::pin(async move {
                    let image = match media.generate_image(prompt, size, quality).await {
                        Ok(image) => image,
                        Err(error) => return Ok(ToolResult::error(error.to_string())),
                    };
                    if image.bytes.len() > MAX_IMAGE_BYTES {
                        return Ok(ToolResult::error("Image was generated but is too large for the harness frame; it was not saved"));
                    }
                    let default_path = || format!("images/{}.{}", uuid::Uuid::new_v4(), image_extension(&image.media_type));
                    let requested = path.unwrap_or_else(default_path);
                    let actual = match workspace.write_blob(requested.clone(), image.bytes.clone(), key.clone()).await {
                        Ok(()) => requested,
                        Err(error) if error.code == ErrorCode::PreconditionFailed => {
                            // The generated image is already paid for. Preserve it at a fresh path
                            // rather than overwriting the model's chosen existing file.
                            let fallback = default_path();
                            let fallback_key = IdempotencyKey::new(format!("{}/fallback", key.as_str()));
                            if let Err(error) = workspace.write_blob(fallback.clone(), image.bytes, fallback_key).await {
                                return Ok(ToolResult::error(format!("Image was generated but could not be saved: {error}")));
                            }
                            fallback
                        }
                        Err(error) => return Ok(ToolResult::error(format!("Image was generated but could not be saved: {error}"))),
                    };
                    Ok(ToolResult::text(format!("Generated {} image saved to {actual}. Read the path to inspect it.", image.media_type)))
                })
            }
            _ => self.workspace.call(name, arguments, key),
        }
    }

    fn write_blob(&self, path: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.workspace.write_blob(path, bytes, key)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use aim_llm::LlmError;
    use aim_llm_codex::media::{Citation, Image, SearchAnswer};
    use aim_proto::error::{ErrorCode, ProtoError};
    use aim_proto::ids::IdempotencyKey;
    use aim_proto::tool::{ToolResult, ToolSpec};
    use serde_json::{Value, json};

    use super::{BoxFuture, Dispatcher, MAX_IMAGE_BYTES, MediaService, ToolHost, service_specs};

    #[derive(Default)]
    struct Workspace {
        shadow_search: bool,
        fail_first_write: bool,
        fail_with: Option<ErrorCode>,
        attempts: AtomicUsize,
        calls: Mutex<Vec<String>>,
        writes: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl ToolHost for Workspace {
        fn specs(&self) -> Vec<ToolSpec> {
            if self.shadow_search { service_specs().into_iter().filter(|spec| spec.name == "web_search").collect() } else { Vec::new() }
        }

        fn call(&self, name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
            self.calls.lock().unwrap().push(name);
            Box::pin(async { Ok(ToolResult::text("workspace result")) })
        }

        fn write_blob(&self, path: String, bytes: Vec<u8>, _key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
            if let Some(code) = self.fail_with {
                return Box::pin(async move { Err(ProtoError::new(code, "write denied")) });
            }
            if self.fail_first_write && self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(async { Err(ProtoError::new(ErrorCode::PreconditionFailed, "file exists")) });
            }
            self.writes.lock().unwrap().push((path, bytes));
            Box::pin(async { Ok(()) })
        }
    }

    struct Service {
        image_bytes: usize,
        image_calls: Arc<AtomicUsize>,
        cited: bool,
    }

    impl Default for Service {
        fn default() -> Self {
            Self { image_bytes: 3, image_calls: Arc::new(AtomicUsize::new(0)), cited: true }
        }
    }

    impl MediaService for Service {
        fn search_enabled(&self) -> bool {
            true
        }

        fn image_enabled(&self) -> bool {
            true
        }

        fn web_search(&self, _query: String) -> BoxFuture<Result<SearchAnswer, LlmError>> {
            let citations = if self.cited {
                vec![Citation { url: "https://example.test".into(), title: "Example".into(), start: 0, end: 6 }]
            } else {
                Vec::new()
            };
            Box::pin(async move { Ok(SearchAnswer { text: "result".into(), citations, queries: vec!["query".into()] }) })
        }

        fn generate_image(&self, _prompt: String, _size: Option<String>, _quality: Option<String>) -> BoxFuture<Result<Image, LlmError>> {
            self.image_calls.fetch_add(1, Ordering::SeqCst);
            let bytes = vec![1; self.image_bytes];
            Box::pin(async move { Ok(Image { bytes, media_type: "image/png".into(), usage: None }) })
        }
    }

    fn key() -> IdempotencyKey {
        IdempotencyKey::new("media-test")
    }

    #[tokio::test]
    async fn workspace_tool_shadows_service_tool() {
        let workspace = Arc::new(Workspace { shadow_search: true, ..Workspace::default() });
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(Service::default()), true);
        assert_eq!(dispatcher.specs().iter().filter(|spec| spec.name == "web_search").count(), 1);
        let result = dispatcher.call("web_search".into(), json!({"query":"query"}), key()).await.unwrap();
        assert_eq!(result, ToolResult::text("workspace result"));
        assert_eq!(*workspace.calls.lock().unwrap(), ["web_search"]);
    }

    #[tokio::test]
    async fn service_search_returns_citations() {
        let dispatcher = Dispatcher::with_policy(Arc::new(Workspace::default()), Arc::new(Service::default()), true);
        let result = dispatcher.call("web_search".into(), json!({"query":"query"}), key()).await.unwrap();
        let value = serde_json::to_value(result).unwrap();
        let text: Value = serde_json::from_str(value["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["citations"][0]["url"], "https://example.test");
        assert_eq!(text["text"], "result");
        assert_eq!(text["uncited"], false);
    }

    #[tokio::test]
    async fn service_search_marks_answers_without_citations() {
        let service = Service { cited: false, ..Service::default() };
        let dispatcher = Dispatcher::with_policy(Arc::new(Workspace::default()), Arc::new(service), true);
        let result = dispatcher.call("web_search".into(), json!({"query":"query"}), key()).await.unwrap();
        let value = serde_json::to_value(result).unwrap();
        let text: Value = serde_json::from_str(value["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["uncited"], true);
    }

    #[tokio::test]
    async fn generated_image_is_written_via_workspace() {
        let workspace = Arc::new(Workspace::default());
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(Service::default()), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key()).await.unwrap();
        assert_eq!(*workspace.writes.lock().unwrap(), [("art/sun.png".into(), vec![1, 1, 1])]);
        assert_eq!(result.content.len(), 1);
        assert!(serde_json::to_string(&result).unwrap().contains("art/sun.png"));
    }

    #[tokio::test]
    async fn existing_path_keeps_file_and_saves_generated_image_once() {
        let workspace = Arc::new(Workspace { fail_first_write: true, ..Workspace::default() });
        let service = Service::default();
        let calls = Arc::clone(&service.image_calls);
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(workspace.writes.lock().unwrap().len(), 1);
        assert!(!result.is_error);
        assert!(!serde_json::to_string(&result).unwrap().contains("art/sun.png"));
    }

    #[tokio::test]
    async fn oversized_image_is_not_sent_to_harness() {
        let workspace = Arc::new(Workspace::default());
        let service = Service { image_bytes: MAX_IMAGE_BYTES + 1, ..Service::default() };
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun"}), key()).await.unwrap();
        assert!(result.is_error);
        assert!(workspace.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_path_is_rejected_before_generation() {
        let service = Service::default();
        let calls = Arc::clone(&service.image_calls);
        let dispatcher = Dispatcher::with_policy(Arc::new(Workspace::default()), Arc::new(service), true);
        let error = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"../escape.png"}), key()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn write_failure_tells_model_generation_was_spent() {
        let workspace = Arc::new(Workspace { fail_with: Some(ErrorCode::Denied), ..Workspace::default() });
        let service = Service::default();
        let calls = Arc::clone(&service.image_calls);
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun"}), key()).await.unwrap();
        assert!(result.is_error);
        assert!(serde_json::to_string(&result).unwrap().contains("generated but could not be saved"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(workspace.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabled_session_exposes_no_media_tools_or_calls() {
        let workspace = Arc::new(Workspace::default());
        let service = Service::default();
        let calls = Arc::clone(&service.image_calls);
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), false);
        assert!(dispatcher.specs().is_empty());
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun"}), key()).await.unwrap();
        assert_eq!(result, ToolResult::text("workspace result"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
