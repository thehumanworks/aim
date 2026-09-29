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

/// A live image reservation that is cancelled if the generation stops early. A `generate_image`
/// future dropped mid-flight (an interrupted turn replaces the running tool call) cancels it on a
/// spawned task, so neither its marker nor its slot outlives the call (`REV13a` M4, ADR 0067).
struct ReservationGuard {
    workspace: Arc<dyn ToolHost>,
    reservation: Option<String>,
    cancel_key: IdempotencyKey,
}

impl ReservationGuard {
    fn new(workspace: Arc<dyn ToolHost>, reservation: String, cancel_key: IdempotencyKey) -> Self {
        Self { workspace, reservation: Some(reservation), cancel_key }
    }

    /// The reservation, still armed.
    fn reservation(&self) -> String {
        self.reservation.clone().unwrap_or_default()
    }

    /// The reservation was finalized: nothing to cancel.
    fn disarm(&mut self) {
        self.reservation = None;
    }

    /// Cancels now, logging a failure as `what`. The guard stays armed until the harness answers,
    /// so a future dropped while this cancel is pending still cancels from `Drop` (REV19 B5); the
    /// retry reuses the idempotency key, so aimx runs the cancel at most once.
    async fn cancel(mut self, what: &str) {
        let Some(reservation) = self.reservation.clone() else { return };
        let answered = self.workspace.cancel_blob(reservation, self.cancel_key.clone()).await;
        self.reservation = None;
        if let Err(cleanup) = answered {
            tracing::warn!(%cleanup, "could not cancel image reservation after {what}");
        }
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        let Some(reservation) = self.reservation.take() else { return };
        let cancel = self.workspace.cancel_blob(reservation, self.cancel_key.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(cleanup) = cancel.await {
                    tracing::warn!(%cleanup, "could not cancel the reservation of a dropped image generation");
                }
            });
        } else {
            tracing::warn!("a dropped image generation left its reservation: no runtime to cancel it");
        }
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
                    // Choose the destination before calling the paid provider. Its media type is
                    // unknown yet, so an automatic path uses a format-neutral suffix.
                    let requested = path.unwrap_or_else(|| format!("images/{}.img", uuid::Uuid::new_v4()));
                    let reserve_key = IdempotencyKey::new(format!("{}/reserve", key.as_str()));
                    let reservation = match workspace.reserve_blob(requested.clone(), reserve_key).await {
                        Ok(reservation) => reservation,
                        Err(error) => return Ok(ToolResult::error(format!("Image destination cannot be reserved: {error}"))),
                    };
                    // From here on, every way out (including this future being dropped) ends the
                    // reservation: finalized, or cancelled.
                    let cancel_key = IdempotencyKey::new(format!("{}/cancel", key.as_str()));
                    let mut guard = ReservationGuard::new(Arc::clone(&workspace), reservation, cancel_key);
                    let image = match media.generate_image(prompt, size, quality).await {
                        Ok(image) => image,
                        Err(error) => {
                            guard.cancel("provider failure").await;
                            return Ok(ToolResult::error(error.to_string()));
                        }
                    };
                    if image.bytes.len() > MAX_IMAGE_BYTES {
                        guard.cancel("an oversized image").await;
                        return Ok(ToolResult::error("Image was generated but is too large for the harness frame; it was not saved"));
                    }
                    let media_type = image.media_type;
                    let finalize_key = IdempotencyKey::new(format!("{}/finalize", key.as_str()));
                    if let Err(error) = workspace.finalize_blob(guard.reservation(), image.bytes, finalize_key).await {
                        guard.cancel("finalize failure").await;
                        return Ok(ToolResult::error(format!("Image was generated but could not be saved: {error}")));
                    }
                    guard.disarm();
                    Ok(ToolResult::text(format!("Generated {media_type} image saved to {requested}. Read the path to inspect it.")))
                })
            }
            _ => self.workspace.call(name, arguments, key),
        }
    }

    fn reserve_blob(&self, path: String, key: IdempotencyKey) -> BoxFuture<Result<String, ProtoError>> {
        self.workspace.reserve_blob(path, key)
    }

    fn finalize_blob(&self, reservation: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.workspace.finalize_blob(reservation, bytes, key)
    }

    fn cancel_blob(&self, reservation: String, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.workspace.cancel_blob(reservation, key)
    }

    fn write_blob(&self, path: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.workspace.write_blob(path, bytes, key)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use aim_llm::{LlmError, LlmErrorKind};
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
        fail_finalize_with: Option<ErrorCode>,
        reserved: Mutex<Option<String>>,
        cancels: AtomicUsize,
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

        fn reserve_blob(&self, path: String, _key: IdempotencyKey) -> BoxFuture<Result<String, ProtoError>> {
            if let Some(code) = self.fail_with {
                return Box::pin(async move { Err(ProtoError::new(code, "reserve denied")) });
            }
            if self.fail_first_write && self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(async { Err(ProtoError::new(ErrorCode::PreconditionFailed, "file exists")) });
            }
            *self.reserved.lock().unwrap() = Some(path);
            Box::pin(async { Ok("reservation".into()) })
        }

        fn finalize_blob(&self, _reservation: String, bytes: Vec<u8>, _key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
            if let Some(code) = self.fail_finalize_with {
                return Box::pin(async move { Err(ProtoError::new(code, "write denied")) });
            }
            if let Some(path) = self.reserved.lock().unwrap().take() {
                self.writes.lock().unwrap().push((path, bytes));
            }
            Box::pin(async { Ok(()) })
        }

        fn cancel_blob(&self, _reservation: String, _key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
            *self.reserved.lock().unwrap() = None;
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct Service {
        image_bytes: usize,
        image_calls: Arc<AtomicUsize>,
        cited: bool,
        fail_image: bool,
    }

    impl Default for Service {
        fn default() -> Self {
            Self { image_bytes: 3, image_calls: Arc::new(AtomicUsize::new(0)), cited: true, fail_image: false }
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
            if self.fail_image {
                return Box::pin(async { Err(LlmError::new(LlmErrorKind::Unavailable, "provider unavailable")) });
            }
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
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(workspace.writes.lock().unwrap().is_empty());
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn read_only_scope_refuses_before_provider_call() {
        let workspace = Arc::new(Workspace { fail_with: Some(ErrorCode::Denied), ..Workspace::default() });
        let service = Service::default();
        let calls = Arc::clone(&service.image_calls);
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key()).await.unwrap();
        assert!(result.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provider_failure_cancels_reservation() {
        let workspace = Arc::new(Workspace::default());
        let service = Service { fail_image: true, ..Service::default() };
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key()).await.unwrap();
        assert!(result.is_error);
        assert!(workspace.reserved.lock().unwrap().is_none());
        assert!(workspace.writes.lock().unwrap().is_empty());
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
        let workspace = Arc::new(Workspace { fail_finalize_with: Some(ErrorCode::Denied), ..Workspace::default() });
        let service = Service::default();
        let calls = Arc::clone(&service.image_calls);
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun"}), key()).await.unwrap();
        assert!(result.is_error);
        assert!(serde_json::to_string(&result).unwrap().contains("generated but could not be saved"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(workspace.writes.lock().unwrap().is_empty());
    }

    /// A provider that never answers, to drop a generation mid-flight.
    struct Hanging;

    impl MediaService for Hanging {
        fn search_enabled(&self) -> bool {
            false
        }

        fn image_enabled(&self) -> bool {
            true
        }

        fn web_search(&self, _query: String) -> BoxFuture<Result<SearchAnswer, LlmError>> {
            Box::pin(std::future::pending())
        }

        fn generate_image(&self, _prompt: String, _size: Option<String>, _quality: Option<String>) -> BoxFuture<Result<Image, LlmError>> {
            Box::pin(std::future::pending())
        }
    }

    /// `REV13a` M4: dropping the tool call during generation cancels its reservation.
    #[tokio::test]
    async fn dropped_generation_cancels_its_reservation() {
        let workspace = Arc::new(Workspace::default());
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(Hanging), true);
        let call = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key());
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), call).await.is_err(), "the provider never answers");
        for _ in 0..100 {
            if workspace.reserved.lock().unwrap().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*workspace.reserved.lock().unwrap(), None, "the dropped call's reservation was cancelled");
        assert_eq!(workspace.cancels.load(Ordering::SeqCst), 1);
        assert!(workspace.writes.lock().unwrap().is_empty());
    }

    /// A workspace whose first `cancel_blob` never answers, counting calls.
    #[derive(Default)]
    struct SlowCancel {
        cancels: AtomicUsize,
    }

    impl ToolHost for SlowCancel {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }

        fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
            Box::pin(async { Ok(ToolResult::text("workspace result")) })
        }

        fn reserve_blob(&self, _path: String, _key: IdempotencyKey) -> BoxFuture<Result<String, ProtoError>> {
            Box::pin(async { Ok("reservation".into()) })
        }

        fn cancel_blob(&self, _reservation: String, _key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
            if self.cancels.fetch_add(1, Ordering::SeqCst) == 0 { Box::pin(std::future::pending()) } else { Box::pin(async { Ok(()) }) }
        }
    }

    /// REV19 B5: a generation dropped while its explicit cancel is still pending (after a provider
    /// failure) still cancels from `Drop`, rather than losing the cancellation.
    #[tokio::test]
    async fn dropping_during_a_pending_cancel_still_cancels() {
        let workspace = Arc::new(SlowCancel::default());
        let service = Service { fail_image: true, ..Service::default() };
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(service), true);
        let call = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key());
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), call).await.is_err(), "the explicit cancel is blocked");
        assert_eq!(workspace.cancels.load(Ordering::SeqCst), 2, "the dropped call's guard sent the cancel again");
    }

    /// A finalized image is never cancelled afterwards.
    #[tokio::test]
    async fn a_finalized_generation_is_not_cancelled() {
        let workspace = Arc::new(Workspace::default());
        let dispatcher = Dispatcher::with_policy(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(Service::default()), true);
        dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key()).await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(workspace.cancels.load(Ordering::SeqCst), 0);
        assert_eq!(workspace.writes.lock().unwrap().len(), 1);
    }

    /// Live (ADR 0022, `REV13a` M4/M5): one real Codex image generated through aim's dispatcher
    /// into a real `aimx serve --stdio` workspace, then one dropped mid-generation. No reservation
    /// marker or journal entry is left. Two media calls.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live: needs ChatGPT credentials and image quota; build aimx first"]
    async fn live_generate_image_through_aimx_and_cancel_mid_generation() {
        use std::time::{Duration, Instant};
        let aimx = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("aimx");
        assert!(aimx.exists(), "build aimx (same profile) first: {}", aimx.display());
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        let root = std::fs::canonicalize(dir.path().join("ws")).unwrap();
        let mut child = tokio::process::Command::new(&aimx)
            .args(["serve", "--stdio", "--root"])
            .arg(&root)
            .env("HOME", &home)
            .env("AIMX_LOG", "off")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let harness = Arc::new(crate::harness::HarnessClient::connect(stdout, stdin, root.to_str().unwrap()).await.unwrap());
        let media = Arc::new(aim_llm_codex::media::MediaClient::new().unwrap());
        let dispatcher = Dispatcher::with_policy(Arc::clone(&harness) as Arc<dyn ToolHost>, media, true);
        let key = || IdempotencyKey::new(uuid::Uuid::now_v7().to_string());
        let arguments = |path: &str| json!({"prompt": "A single blue circle on a white background", "path": path, "size": "1024x1024", "quality": "low"});

        let started = Instant::now();
        let result = dispatcher.call("generate_image".into(), arguments("art/live.png"), key()).await.unwrap();
        let generated_ms = started.elapsed().as_millis();
        assert!(!result.is_error, "{result:?}");
        let bytes = std::fs::read(root.join("art/live.png")).unwrap();
        let image = bytes.starts_with(b"\x89PNG") || bytes.starts_with(b"\xff\xd8\xff") || bytes.get(8..12) == Some(b"WEBP");
        assert!(image, "the finalized file is an image, not a marker");

        let started = Instant::now();
        let mut call = dispatcher.call("generate_image".into(), arguments("art/cancelled.png"), key());
        tokio::select! {
            finished = &mut call => panic!("the generation finished before it could be dropped: {finished:?}"),
            () = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
        let marker = std::fs::read_to_string(root.join("art/cancelled.png")).unwrap_or_default();
        assert!(marker.starts_with("aim-reservation:"), "reserved before the provider answered");
        drop(call);
        let deadline = Instant::now() + Duration::from_secs(10);
        while root.join("art/cancelled.png").exists() {
            assert!(Instant::now() < deadline, "the dropped generation's marker must be cancelled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let cancelled_ms = started.elapsed().as_millis();

        drop(dispatcher);
        let harness = Arc::try_unwrap(harness).unwrap_or_else(|_| panic!("the harness is still shared"));
        harness.shutdown().await;
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
        assert!(status.success());
        let journal = std::fs::read_dir(home.join(".aim/aimx/reservations"))
            .map_or(0, |dir| dir.flatten().filter(|entry| entry.file_name() != ".lock").count());
        assert_eq!(journal, 0, "no reservation is left in the journal");
        let left: Vec<_> = std::fs::read_dir(root.join("art")).unwrap().flatten().map(|entry| entry.file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("live.png")], "only the finalized image is left");
        eprintln!(
            "live_generate_image generated_ms={generated_ms} bytes={} cancelled_after_drop_ms={cancelled_ms} journal_entries={journal}",
            bytes.len()
        );
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
