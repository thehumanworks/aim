//! Service tools share the model's tool namespace with aimx, but never replace aimx names.

use std::collections::HashSet;
use std::sync::Arc;

use aim_llm::LlmError;
use aim_llm_codex::media::{Image, MediaClient, SearchAnswer};
use aim_proto::content::Base64Bytes;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolContent, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde_json::{Value, json};

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

const INLINE_IMAGE_LIMIT: usize = 5 * 1024 * 1024;

pub(crate) trait MediaService: Send + Sync {
    fn search_enabled(&self) -> bool;
    fn image_enabled(&self) -> bool;
    fn web_search(&self, query: String) -> BoxFuture<Result<SearchAnswer, LlmError>>;
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
}

impl Dispatcher {
    /// Composes the workspace with Codex media tools.
    #[must_use]
    pub fn new(workspace: Arc<dyn ToolHost>, media: Arc<MediaClient>) -> Self {
        Self::with_service(workspace, media)
    }

    fn with_service(workspace: Arc<dyn ToolHost>, media: Arc<dyn MediaService>) -> Self {
        let workspace_names = workspace.specs().into_iter().map(|spec| spec.name).collect();
        let search_enabled = media.search_enabled();
        let image_enabled = media.image_enabled();
        Self { workspace, media, workspace_names, search_enabled, image_enabled }
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
            "Search the public web with Codex. The query is sent to the service even in a private or ephemeral session. Cite the returned source URLs when answering.",
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

impl ToolHost for Dispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.workspace.specs();
        specs.extend(service_specs().into_iter().filter(|spec| {
            !self.workspace_names.contains(&spec.name)
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
            "web_search" if self.search_enabled => {
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
                            Ok(ToolResult::text(json!({"text":answer.text,"citations":citations,"queries":answer.queries}).to_string()))
                        }
                        Err(error) => Ok(ToolResult::error(error.to_string())),
                    }
                })
            }
            "generate_image" if self.image_enabled => {
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
                let path = match optional_string(&arguments, "path") {
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
                    let path = path.unwrap_or_else(|| format!("images/{}.{}", uuid::Uuid::new_v4(), image_extension(&image.media_type)));
                    workspace.write_blob(path.clone(), image.bytes.clone(), key).await?;
                    let mut content = vec![ToolContent::Text { text: format!("Generated image saved to {path}") }];
                    if image.bytes.len() <= INLINE_IMAGE_LIMIT {
                        content.push(ToolContent::Image { media_type: image.media_type, data: Base64Bytes(image.bytes) });
                    }
                    Ok(ToolResult { content, ..ToolResult::default() })
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
    use std::sync::{Arc, Mutex};

    use aim_llm::LlmError;
    use aim_llm_codex::media::{Citation, Image, SearchAnswer};
    use aim_proto::error::ProtoError;
    use aim_proto::ids::IdempotencyKey;
    use aim_proto::tool::{ToolResult, ToolSpec};
    use serde_json::{Value, json};

    use super::{BoxFuture, Dispatcher, MediaService, ToolHost, service_specs};

    #[derive(Default)]
    struct Workspace {
        shadow_search: bool,
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
            self.writes.lock().unwrap().push((path, bytes));
            Box::pin(async { Ok(()) })
        }
    }

    struct Service;

    impl MediaService for Service {
        fn search_enabled(&self) -> bool {
            true
        }

        fn image_enabled(&self) -> bool {
            true
        }

        fn web_search(&self, _query: String) -> BoxFuture<Result<SearchAnswer, LlmError>> {
            Box::pin(async {
                Ok(SearchAnswer {
                    text: "result".into(),
                    citations: vec![Citation { url: "https://example.test".into(), title: "Example".into(), start: 0, end: 6 }],
                    queries: vec!["query".into()],
                })
            })
        }

        fn generate_image(&self, _prompt: String, _size: Option<String>, _quality: Option<String>) -> BoxFuture<Result<Image, LlmError>> {
            Box::pin(async { Ok(Image { bytes: vec![1, 2, 3], media_type: "image/png".into(), usage: None }) })
        }
    }

    fn key() -> IdempotencyKey {
        IdempotencyKey::new("media-test")
    }

    #[tokio::test]
    async fn workspace_tool_shadows_service_tool() {
        let workspace = Arc::new(Workspace { shadow_search: true, ..Workspace::default() });
        let dispatcher = Dispatcher::with_service(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(Service));
        assert_eq!(dispatcher.specs().iter().filter(|spec| spec.name == "web_search").count(), 1);
        let result = dispatcher.call("web_search".into(), json!({"query":"query"}), key()).await.unwrap();
        assert_eq!(result, ToolResult::text("workspace result"));
        assert_eq!(*workspace.calls.lock().unwrap(), ["web_search"]);
    }

    #[tokio::test]
    async fn service_search_returns_citations() {
        let dispatcher = Dispatcher::with_service(Arc::new(Workspace::default()), Arc::new(Service));
        let result = dispatcher.call("web_search".into(), json!({"query":"query"}), key()).await.unwrap();
        let value = serde_json::to_value(result).unwrap();
        let text: Value = serde_json::from_str(value["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["citations"][0]["url"], "https://example.test");
        assert_eq!(text["text"], "result");
    }

    #[tokio::test]
    async fn generated_image_is_written_via_workspace() {
        let workspace = Arc::new(Workspace::default());
        let dispatcher = Dispatcher::with_service(Arc::clone(&workspace) as Arc<dyn ToolHost>, Arc::new(Service));
        let result = dispatcher.call("generate_image".into(), json!({"prompt":"sun","path":"art/sun.png"}), key()).await.unwrap();
        assert_eq!(*workspace.writes.lock().unwrap(), [("art/sun.png".into(), vec![1, 2, 3])]);
        assert_eq!(result.content.len(), 2);
    }
}
