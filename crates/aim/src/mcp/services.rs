//! The local services exposed by `aim mcp` to an explicitly launched external agent.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aim_llm_codex::media::MediaClient;
use aim_proto::board::{JobRef, JobSpec, ListParams, PollParams, PostParams};
use aim_proto::content::Base64Bytes;
use aim_proto::daemon::Persistence;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolContent, ToolInput, ToolLocation, ToolResult, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;
use crate::board::Board;
use crate::resources::files::{Files as _, LocalFiles, Read};
use crate::search::SearchEngine;
use crate::search::tools::SearchToolHost;
use crate::store::SqliteStore;

const MAX_SERVICE_RESULT: usize = 256 * 1024;

/// The persistent user's search, board, media, and Markdown memory services.
pub struct AimServices {
    search: Arc<SearchToolHost>,
    board: Arc<Board>,
    media: Option<Arc<MediaClient>>,
    memory: LocalFiles,
    memory_home: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
struct BoardPostArgs {
    #[serde(default)]
    run_id: Option<String>,
    spec: JobSpec,
}

#[derive(Deserialize)]
struct MemoryReadArgs {
    path: String,
}

fn invalid(message: &'static str) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}

fn unavailable(message: &'static str) -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, message)
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type":"object"}))
}

fn spec(name: &str, description: &str, input_schema: Value, read_only: bool, open_world: bool) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        input: ToolInput::Json,
        annotations: ToolAnnotations {
            read_only,
            destructive: !read_only,
            idempotent: false,
            open_world,
            location: ToolLocation::LocalService,
        },
    }
}

fn board_specs() -> Vec<ToolSpec> {
    vec![
        spec("board_list", "List durable board jobs.", schema::<ListParams>(), true, false),
        spec("board_show", "Read one durable board job and its artifact summaries.", schema::<JobRef>(), true, false),
        spec("board_poll", "Poll ordered board event hints for a run.", schema::<PollParams>(), true, false),
        spec("board_post", "Post an immutable job contract; aim supplies the retry key.", schema::<BoardPostArgs>(), false, false),
    ]
}

fn memory_specs() -> Vec<ToolSpec> {
    vec![
        spec("memory_list", "List Markdown memory files in this user's aim home.", json!({"type":"object","properties":{}}), true, false),
        spec(
            "memory_read",
            "Read one Markdown memory file, at most 64 KiB.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            true,
            false,
        ),
    ]
}

fn media_specs(media: Option<&MediaClient>) -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    if media.is_some_and(MediaClient::search_enabled) {
        specs.push(spec(
            "web_search",
            "Search the public web through Codex; the query leaves this machine.",
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
            true,
            true,
        ));
    }
    if media.is_some_and(MediaClient::image_enabled) {
        specs.push(spec(
            "generate_image",
            "Generate an image through Codex; the prompt leaves this machine.",
            json!({"type":"object","properties":{"prompt":{"type":"string"},"size":{"type":"string"},"quality":{"type":"string"}},"required":["prompt"]}),
            false,
            true,
        ));
    }
    specs
}

fn bounded_json(value: &impl serde::Serialize) -> Result<ToolResult, ProtoError> {
    let bytes = serde_json::to_vec(value).map_err(|_| unavailable("service result cannot be serialized"))?;
    if bytes.len() > MAX_SERVICE_RESULT {
        return Err(ProtoError::new(ErrorCode::LimitExceeded, "service result exceeds 256 KiB"));
    }
    let text = String::from_utf8(bytes).map_err(|_| unavailable("service result is not UTF-8"))?;
    Ok(ToolResult::text(text))
}

fn safe_memory_path(raw: &str) -> Result<String, ProtoError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.extension().is_none_or(|extension| extension != "md") {
        return Err(invalid("memory path must name a Markdown file"));
    }
    if !path.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err(invalid("memory path must stay inside memory/"));
    }
    Ok(format!("memory/{raw}"))
}

async fn read_memory(memory: &LocalFiles, memory_home: PathBuf, arguments: Value) -> Result<ToolResult, ProtoError> {
    let args: MemoryReadArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid memory_read arguments"))?;
    let path = safe_memory_path(&args.path)?;
    let (root, target) =
        tokio::task::spawn_blocking(move || (memory_home.join("memory").canonicalize(), memory_home.join(&path).canonicalize()))
            .await
            .map_err(|_| unavailable("memory path check failed"))?;
    let root = root.map_err(|_| ProtoError::new(ErrorCode::NotFound, "memory directory not found"))?;
    let target = target.map_err(|_| ProtoError::new(ErrorCode::NotFound, "memory file not found"))?;
    if !target.starts_with(root) {
        return Err(invalid("memory path must stay inside memory/"));
    }
    let read = memory
        .read_many(vec![target.to_string_lossy().into_owned()], 64 * 1024)
        .await
        .into_iter()
        .next()
        .ok_or_else(|| unavailable("memory read failed"))?;
    match read {
        Read::Ok(file) => {
            Ok(ToolResult { content: vec![ToolContent::Text { text: file.text }], truncated: file.truncated, ..ToolResult::default() })
        }
        Read::Missing => Err(ProtoError::new(ErrorCode::NotFound, "memory file not found")),
        Read::Failed(_) => Err(unavailable("memory file cannot be read")),
    }
}

impl AimServices {
    /// Open the user's persistent services. Embedding-model loading happens off the Tokio worker.
    ///
    /// # Errors
    /// The persistent store, board, or search projection cannot be opened.
    pub async fn open(aim_home: &Path) -> Result<Self, String> {
        let database = aim_home.join("aim.db");
        let store = Arc::new(SqliteStore::open(&database).map_err(|_| "cannot open persistent session store")?);
        let search_path = database.clone();
        let engine = tokio::task::spawn_blocking(move || SearchEngine::open(&search_path))
            .await
            .map_err(|_| "search setup task failed")?
            .map_err(|_| "cannot open conversation search")?;
        let search = Arc::new(SearchToolHost::new(Arc::new(engine), store, Persistence::Persistent, None));
        let board = Arc::new(Board::open(&database).map_err(|_| "cannot open board service")?);
        let media = MediaClient::new().ok().map(Arc::new);
        Ok(Self { search, board, media, memory: LocalFiles::new(aim_home), memory_home: aim_home.to_path_buf() })
    }

    /// A testable constructor over already-opened services.
    #[must_use]
    pub fn with_services(search: Arc<SearchToolHost>, board: Arc<Board>, media: Option<Arc<MediaClient>>, memory_home: PathBuf) -> Self {
        Self { search, board, media, memory: LocalFiles::new(&memory_home), memory_home }
    }
}

/// Add code mode's `run_code` and saved-program tools to aim's MCP service catalog as
/// `AIM_CODE_MODE` asks, when its worker is present (ADR 0076): `off` serves the services alone,
/// `on` serves them beside the code tools (none is hidden: each service is a primary action), and
/// `only` serves the code tools alone. The wrapped host remains the authority for every nested call.
#[must_use]
pub fn with_programs(host: Arc<dyn ToolHost>, aim_home: &Path) -> Arc<dyn ToolHost> {
    with_code_mode(host, aim_home, crate::providers::code_mode())
}

/// [`with_programs`] with an explicit code-mode configuration (`None`: off).
#[must_use]
pub fn with_code_mode(host: Arc<dyn ToolHost>, aim_home: &Path, code: Option<crate::host::CodeConfig>) -> Arc<dyn ToolHost> {
    let Some(code) = code else { return host };
    let exposure = crate::coderun::mode::decide(crate::coderun::mode::CodeModeRequest::Set(code.mode), true, true, true);
    if !exposure.code {
        return host;
    }
    let runtime = crate::coderun::CodeToolHost::new(Arc::clone(&host), code.worker, "aim-mcp", crate::coderun::CodeMode::RunCode)
        .with_direct(exposure.direct, &[]);
    let store = Arc::new(crate::programs::ProgramStore::new(aim_home.join("programs")));
    let mut programs = crate::coderun::ProgramToolHost::new(runtime, store);
    // Project programs are read from the working directory's `.agents/programs`. They are written
    // only through a workspace's `Write` tool (ADR 0066), which this service does not offer, so
    // saving one here is refused with a clear message.
    if let Ok(root) = std::env::current_dir() {
        let files: Arc<dyn crate::resources::files::Files> = Arc::new(LocalFiles::new(root));
        programs = programs.with_project(crate::programs::project::ProjectPrograms::new(files, Arc::clone(&host)));
    }
    let programs: Arc<dyn ToolHost> = Arc::new(programs);
    let direct: Arc<dyn ToolHost> = Arc::new(crate::coderun::DirectCodeTools::new(host, exposure.direct, &[]));
    Arc::new(crate::agent::tools::Compose::new(direct, vec![programs]))
}

impl ToolHost for AimServices {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.search.specs();
        specs.extend(board_specs());
        specs.extend(memory_specs());
        specs.extend(media_specs(self.media.as_deref()));
        specs
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        if matches!(name.as_str(), "search_sessions" | "read_session") {
            return self.search.call(name, arguments, key);
        }
        let board = Arc::clone(&self.board);
        let media = self.media.clone();
        let memory = self.memory.clone();
        let memory_home = self.memory_home.clone();
        Box::pin(async move {
            match name.as_str() {
                "board_list" => {
                    let params: ListParams = serde_json::from_value(arguments).map_err(|_| invalid("invalid board list arguments"))?;
                    bounded_json(&board.list(params).await.map_err(|_| unavailable("board list failed"))?)
                }
                "board_show" => {
                    let params: JobRef = serde_json::from_value(arguments).map_err(|_| invalid("invalid board show arguments"))?;
                    bounded_json(&board.show(params.job_id).await.map_err(|_| unavailable("board show failed"))?)
                }
                "board_poll" => {
                    let params: PollParams = serde_json::from_value(arguments).map_err(|_| invalid("invalid board poll arguments"))?;
                    bounded_json(&board.poll(params).await.map_err(|_| unavailable("board poll failed"))?)
                }
                "board_post" => {
                    let args: BoardPostArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid board post arguments"))?;
                    bounded_json(
                        &board
                            .post(PostParams { run_id: args.run_id, spec: args.spec, idempotency_key: key.to_string() })
                            .await
                            .map_err(|_| unavailable("board post failed"))?,
                    )
                }
                "memory_list" => {
                    let entries = memory.list("memory", 100).await.map_err(|_| unavailable("cannot list memory"))?.unwrap_or_default();
                    let files = entries
                        .into_iter()
                        .filter(|entry| {
                            Path::new(&entry.name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                                && entry.kind == aim_proto::harness::EntryKind::File
                        })
                        .map(|entry| entry.name)
                        .collect::<Vec<_>>();
                    bounded_json(&json!({"files":files}))
                }
                "memory_read" => read_memory(&memory, memory_home, arguments).await,
                "web_search" => {
                    let client =
                        media.as_ref().filter(|client| client.search_enabled()).ok_or_else(|| unavailable("web search unavailable"))?;
                    let query = arguments
                        .get("query")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| invalid("query is required"))?;
                    let answer = client.web_search(query).await.map_err(|_| unavailable("web search failed"))?;
                    bounded_json(
                        &json!({"text":answer.text,"citations":answer.citations.iter().map(|citation| json!({"url":citation.url,"title":citation.title,"start":citation.start,"end":citation.end})).collect::<Vec<_>>(),"queries":answer.queries}),
                    )
                }
                "generate_image" => {
                    let client = media
                        .as_ref()
                        .filter(|client| client.image_enabled())
                        .ok_or_else(|| unavailable("image generation unavailable"))?;
                    let prompt = arguments
                        .get("prompt")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| invalid("prompt is required"))?;
                    let size = arguments.get("size").and_then(Value::as_str);
                    let quality = arguments.get("quality").and_then(Value::as_str);
                    let image = client.generate_image(prompt, size, quality).await.map_err(|_| unavailable("image generation failed"))?;
                    if image.bytes.len() > 10 * 1024 * 1024 {
                        return Err(ProtoError::new(ErrorCode::LimitExceeded, "image exceeds MCP output limit"));
                    }
                    Ok(ToolResult {
                        content: vec![ToolContent::Image { media_type: image.media_type, data: Base64Bytes(image.bytes) }],
                        ..ToolResult::default()
                    })
                }
                _ => Err(ProtoError::new(ErrorCode::MethodNotFound, "unknown aim MCP service tool")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::resources::files::LocalFiles;

    use super::{AimServices, board_specs, memory_specs, read_memory, safe_memory_path};

    #[test]
    fn service_schemas_and_memory_paths_are_bounded() {
        assert_eq!(board_specs().len(), 4);
        assert_eq!(memory_specs().len(), 2);
        assert_eq!(safe_memory_path("notes.md").as_deref(), Ok("memory/notes.md"));
        assert!(safe_memory_path("../secrets.md").is_err());
        assert!(safe_memory_path("/tmp/x.md").is_err());
        let _service_type = std::any::type_name::<AimServices>();
    }

    #[tokio::test]
    async fn memory_read_rejects_symlink_escape() {
        let home = tempfile::tempdir().expect("memory home");
        std::fs::create_dir(home.path().join("memory")).expect("memory directory");
        std::fs::write(home.path().join("outside.md"), "private").expect("outside file");
        std::os::unix::fs::symlink(home.path().join("outside.md"), home.path().join("memory/link.md")).expect("link fixture");
        let memory = LocalFiles::new(home.path());
        assert!(read_memory(&memory, home.path().to_path_buf(), serde_json::json!({"path":"link.md"})).await.is_err());
    }
}
