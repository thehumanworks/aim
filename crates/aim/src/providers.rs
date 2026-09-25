//! Providers by id (docs/architecture.md §6.5): what `--provider` and `SessionSpec::provider`
//! name.
//!
//! - `openrouter`, `ai-gateway` — OpenAI-compatible gateways (ADR 0011); keys from
//!   `OPENROUTER_API_KEY` / `AI_GATEWAY_API_KEY`.
//! - Endpoints can be overridden with `AIM_CODEX_BASE_URL`, `AIM_OPENROUTER_BASE_URL` and
//!   `AIM_AI_GATEWAY_BASE_URL` (https, or http to loopback only).
//! - `codex` — the `ChatGPT` subscription (ADR 0010). Credentials: aim's own login
//!   (`~/.aim/auth/codex.json`), else the Codex CLI's file, read-only and never refreshed. One
//!   provider (one auth manager) serves the whole process, so a refresh never races itself.
//! - `acp:claude` uses witnessed strict aim tools; `acp:claude-native` explicitly uses Claude's
//!   local built-ins. Both are agent backends, not model providers: sessions run them through
//!   [`crate::acp::with_acp`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio_util::sync::CancellationToken;

use aim_llm::ModelProvider;
use aim_llm_codex::CodexProvider;
use aim_llm_codex::media::{MediaClient, MediaConfig};
use aim_llm_openai::{OpenAiProvider, Profile};
use sha2::Digest as _;

/// A provider endpoint override from the environment (endpoints are configuration data: a
/// recording proxy for benchmarks, an enterprise gateway). Only `https://` URLs, or `http://` to a
/// loopback address, are accepted; anything else is ignored with a warning, so a typo cannot send
/// credentials in the clear.
fn base_url_override(var: &str) -> Option<String> {
    let value = std::env::var(var).ok().filter(|v| !v.trim().is_empty())?;
    let url = value.trim().trim_end_matches('/').to_owned();
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|prefix| url.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')));
    if url.starts_with("https://") || loopback {
        Some(url)
    } else {
        tracing::warn!(variable = var, "ignoring a base URL that is neither https nor loopback http");
        None
    }
}

fn with_base_url(mut profile: Profile, var: &str) -> Profile {
    if let Some(base) = base_url_override(var) {
        profile.base_url = base;
    }
    profile
}

/// Default model for `codex`.
pub const CODEX_DEFAULT_MODEL: &str = "gpt-6-sol";

static CODEX: Mutex<Option<Arc<CodexProvider>>> = Mutex::new(None);
type GatewayMap = HashMap<(String, String), Arc<OpenAiProvider>>;
static GATEWAYS: Mutex<Option<GatewayMap>> = Mutex::new(None);

/// The process's codex provider (created on first use).
///
/// # Errors
/// If the provider cannot be prepared (HTTP client, credential path).
pub fn codex() -> Result<Arc<CodexProvider>, String> {
    let mut slot = CODEX.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(provider) = slot.as_ref() {
        return Ok(Arc::clone(provider));
    }
    let mut config = aim_llm_codex::CodexConfig::default();
    if let Some(base) = base_url_override("AIM_CODEX_BASE_URL") {
        config.base_url = base;
    }
    let provider = Arc::new(CodexProvider::with_config(config).map_err(|e| format!("codex: {e}"))?);
    *slot = Some(Arc::clone(&provider));
    Ok(provider)
}

/// Default model for the OpenAI-compatible gateways (both catalogs list it).
pub const GATEWAY_DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5";

/// Provider ids this build knows.
pub const KNOWN: &[&str] = &["openrouter", "ai-gateway", "codex", "acp:claude", "acp:claude-native"];

/// Builds provider `id` and resolves the model (`model`, else the provider's default).
///
/// # Errors
/// A message for the user: unknown provider, or one this build cannot construct.
pub fn build(id: &str, model: Option<&str>) -> Result<(Arc<dyn ModelProvider>, String), String> {
    let gateway = |profile: Profile| -> Result<(Arc<dyn ModelProvider>, String), String> {
        let key = (profile.id.clone(), profile.base_url.clone());
        let mut slot = GATEWAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let gateways = slot.get_or_insert_with(HashMap::new);
        let provider = if let Some(provider) = gateways.get(&key) {
            Arc::clone(provider)
        } else {
            let provider = Arc::new(OpenAiProvider::new(profile).map_err(|e| format!("{id}: {e}"))?);
            gateways.insert(key, Arc::clone(&provider));
            provider
        };
        Ok((provider as Arc<dyn ModelProvider>, model.unwrap_or(GATEWAY_DEFAULT_MODEL).to_owned()))
    };
    match id {
        "openrouter" => gateway(with_base_url(Profile::openrouter(), "AIM_OPENROUTER_BASE_URL")),
        "ai-gateway" => gateway(with_base_url(Profile::ai_gateway(), "AIM_AI_GATEWAY_BASE_URL")),
        "codex" => Ok((codex()? as Arc<dyn ModelProvider>, model.unwrap_or(CODEX_DEFAULT_MODEL).to_owned())),
        "acp:claude" | "acp:claude-native" => Err(format!("`{id}` is an agent (Claude Code), not a model provider: run it as a session")),
        other => Err(format!("unknown provider `{other}` (known: {})", KNOWN.join(", "))),
    }
}

/// The services native sessions get on this machine (ADR 0038):
/// - Codex media tools (web search, image generation), when codex credentials exist at a
///   session's start;
/// - Jev effort advice (ADR 0013), when `TYPESAFE_API_KEY` is set; the host attaches it to
///   persistent sessions only;
/// - the UI tools (`ui_show`, `ui_update`, `ui_close`, `ui_catalog`; ADR 0064).
#[must_use]
pub fn services() -> crate::host::NativeServices {
    let media: crate::host::MediaFactory = Arc::new(|| {
        Box::pin(async {
            let media = MediaClient::with_provider(codex().ok()?, MediaConfig::default());
            // A missing credential omits the tools rather than making every call fail.
            media.has_credentials().await.then(|| Arc::new(media) as Arc<dyn crate::media::MediaService>)
        })
    });
    let decider = std::env::var_os("TYPESAFE_API_KEY")
        .is_some_and(|value| !value.is_empty())
        .then(|| Arc::new(crate::jev::JevDecider) as Arc<dyn crate::jev::Decider>);
    crate::host::NativeServices {
        media: Some(media),
        decider,
        tools: vec![search_tools(), board_tools(), mcp_tools(), crate::ui::tools_factory()],
        code: code_mode(),
    }
}

/// Code mode as `AIM_CODE_MODE` asks (ADR 0076; unset means the provisional default, an invalid
/// value means off), when the
/// `aim-coderun` worker is available: `$AIM_CODERUN`, else next to this executable. It runs
/// sandboxed on macOS and refuses to run on Linux until its bubblewrap profile exists (ADR 0018),
/// so it is offered on macOS only. `None` when the mode is off or cannot run.
pub(crate) fn code_mode() -> Option<crate::host::CodeConfig> {
    code_mode_as(crate::coderun::mode::requested_from_env())
}

/// [`code_mode`] with an explicit request.
pub(crate) fn code_mode_as(requested: crate::coderun::mode::CodeModeRequest) -> Option<crate::host::CodeConfig> {
    let worker = std::env::var_os("AIM_CODERUN")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok().map(|exe| exe.with_file_name("aim-coderun")))
        .filter(|path| path.exists());
    let exposure = crate::coderun::mode::decide(requested, worker.is_some(), cfg!(target_os = "macos"), true);
    let worker = worker.filter(|_| exposure.code)?;
    Some(crate::host::CodeConfig { worker, user_programs: crate::cli::aim_home().join("programs"), mode: exposure.mode })
}

type SearchParts = (Arc<crate::search::SearchEngine>, Arc<crate::store::SqliteStore>);

/// How long a search call waits for the index to open before answering "initializing".
const SEARCH_READY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long after a failed open the next session may retry it.
const SEARCH_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
/// Includes the pinned model's bounded download on a cold cache.
const SEARCH_OPEN_LEASE: std::time::Duration = std::time::Duration::from_mins(11);

type SearchOpener = Arc<dyn Fn(PathBuf, CancellationToken) -> Result<SearchParts, String> + Send + Sync>;

/// The process's search index: opened in the background (a first open may download the
/// embedding model), never on a session's start path; a failed open is retried later.
struct SearchIndex {
    ready: tokio::sync::watch::Sender<Option<SearchParts>>,
    state: Mutex<SearchOpen>,
    generation: AtomicU64,
    database: PathBuf,
    opener: SearchOpener,
    opening_lease: std::time::Duration,
    retry_after: std::time::Duration,
}

#[derive(Default)]
enum SearchOpen {
    #[default]
    NotStarted,
    Opening {
        generation: u64,
        started: std::time::Instant,
        cancel: CancellationToken,
    },
    Open,
    Failed(std::time::Instant),
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self {
            ready: tokio::sync::watch::Sender::new(None),
            state: Mutex::new(SearchOpen::NotStarted),
            generation: AtomicU64::new(0),
            database: crate::cli::aim_home().join("aim.db"),
            opener: Arc::new(|database, cancel| {
                let engine = crate::search::SearchEngine::open_with_cancel(&database, &cancel)?;
                let store = crate::store::SqliteStore::open(&database).map_err(|e| e.to_string())?;
                Ok((Arc::new(engine), Arc::new(store)))
            }),
            opening_lease: SEARCH_OPEN_LEASE,
            retry_after: SEARCH_RETRY_AFTER,
        }
    }
}

impl SearchIndex {
    /// Starts opening the index unless it is open, opening, or failed too recently.
    fn ensure_opening(self: &Arc<Self>) {
        let (generation, cancel) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            match &*state {
                SearchOpen::Opening { started, .. } if started.elapsed() < self.opening_lease => return,
                SearchOpen::Opening { cancel, .. } => cancel.cancel(),
                SearchOpen::Open => return,
                SearchOpen::Failed(at) if at.elapsed() < self.retry_after => return,
                SearchOpen::NotStarted | SearchOpen::Failed(_) => {}
            }
            let generation = self.generation.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let cancel = CancellationToken::new();
            *state = SearchOpen::Opening { generation, started: std::time::Instant::now(), cancel: cancel.clone() };
            (generation, cancel)
        };
        let index = Arc::clone(self);
        tokio::spawn(async move {
            let database = index.database.clone();
            let opener = Arc::clone(&index.opener);
            let result = tokio::task::spawn_blocking(move || opener(database, cancel)).await.ok().and_then(Result::ok);
            let mut state = index.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !matches!(&*state, SearchOpen::Opening { generation: current, .. } if *current == generation) {
                return;
            }
            if let Some(parts) = result {
                *state = SearchOpen::Open;
                index.ready.send_replace(Some(parts));
            } else {
                tracing::debug!("conversation search is unavailable; retrying later");
                *state = SearchOpen::Failed(std::time::Instant::now());
            }
        });
    }
}

/// Search tools that are offered at once and wait (bounded) for the index on their first call.
struct LazySearch {
    index: Arc<SearchIndex>,
    persistence: aim_proto::daemon::Persistence,
}

impl crate::agent::ToolHost for LazySearch {
    fn specs(&self) -> Vec<aim_proto::tool::ToolSpec> {
        crate::search::tools::specs()
    }

    fn call(
        &self,
        name: String,
        arguments: serde_json::Value,
        key: aim_proto::ids::IdempotencyKey,
    ) -> crate::agent::tools::BoxFuture<Result<aim_proto::tool::ToolResult, aim_proto::error::ProtoError>> {
        self.index.ensure_opening();
        let mut ready = self.index.ready.subscribe();
        let persistence = self.persistence;
        Box::pin(async move {
            let parts = tokio::time::timeout(SEARCH_READY_WAIT, ready.wait_for(Option::is_some))
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|parts| parts.clone());
            let Some((engine, store)) = parts else {
                return Err(aim_proto::error::ProtoError::new(
                    aim_proto::error::ErrorCode::Unavailable,
                    "conversation search is still initializing; try again shortly",
                ));
            };
            let reranker = std::env::var_os("TYPESAFE_API_KEY")
                .is_some_and(|value| !value.is_empty())
                .then(|| Arc::new(crate::search::rerank::JevReranker) as Arc<dyn crate::search::tools::Reranker>);
            crate::search::tools::SearchToolHost::new(engine, store, persistence, reranker).call(name, arguments, key).await
        })
    }
}

/// Conversation search tools (`search_sessions`, `read_session`, ADR 0035) over this user's session
/// database. The index opens in the background, off every session's start path.
fn search_tools() -> crate::host::ToolsFactory {
    let index: Arc<SearchIndex> = Arc::new(SearchIndex::default());
    Arc::new(move |spec: &aim_proto::daemon::SessionSpec| {
        index.ensure_opening();
        let host: Arc<dyn crate::agent::ToolHost> = Arc::new(LazySearch { index: Arc::clone(&index), persistence: spec.persistence });
        Box::pin(async move { Some(host) })
    })
}

/// Ordinary native sessions share a durable run by workspace location and root. Private and
/// ephemeral sessions receive no board handle and never open its SQLite ledger.
fn board_tools() -> crate::host::ToolsFactory {
    board_tools_at(crate::cli::aim_home())
}

fn board_tools_at(aim_home: PathBuf) -> crate::host::ToolsFactory {
    Arc::new(move |spec: &aim_proto::daemon::SessionSpec| {
        let spec = spec.clone();
        let aim_home = aim_home.clone();
        Box::pin(async move {
            if spec.persistence != aim_proto::daemon::Persistence::Persistent {
                return None;
            }
            let mut digest = sha2::Sha256::new();
            digest.update(serde_json::to_vec(&spec.location).ok()?);
            digest.update([0]);
            digest.update(spec.workspace.as_bytes());
            let run_id = format!("native-{:x}", digest.finalize());
            let path = aim_home.join("aim.db");
            let board = tokio::task::spawn_blocking(move || crate::board::Board::open(&path)).await.ok()?.ok()?;
            let host: Arc<dyn crate::agent::ToolHost> =
                Arc::new(crate::board::tools::BoardTools::reviewer(board, run_id.clone(), format!("session:{run_id}")));
            Some(host)
        })
    })
}

/// Discover trusted MCP config through the selected workspace harness, then start its servers
/// in the background. The returned host advertises a private last-known catalog immediately.
fn mcp_tools() -> crate::host::ToolsFactory {
    Arc::new(|spec: &aim_proto::daemon::SessionSpec| {
        let spec = spec.clone();
        Box::pin(async move { crate::mcp::session::connect_for_session(&spec).await })
    })
}

/// Every backend this build can host: `acp:*` agents, else the native loop with [`build`]'s
/// providers and local workspaces served by the `aimx` binary at `aimx`.
#[must_use]
pub fn backends(aimx: PathBuf, max_requests: u32) -> crate::host::BackendFactory {
    let providers: crate::host::ProviderFactory = Arc::new(build);
    crate::acp::with_acp_at(crate::host::native_backends(providers, crate::host::aimx_workspaces(aimx.clone()), max_requests), aimx)
}

#[cfg(test)]
mod tests {
    use aim_proto::daemon::{Location, Persistence, SessionSpec};
    use aim_proto::ids::IdempotencyKey;

    use super::{board_tools_at, build};
    use std::sync::Arc;

    fn spec(persistence: Persistence) -> SessionSpec {
        SessionSpec {
            workspace: "/workspace".to_owned(),
            location: Location::Local,
            provider: "scripted".to_owned(),
            model: None,
            effort: None,
            agent: None,
            persistence,
        }
    }

    #[tokio::test]
    async fn persistent_board_tools_are_real_and_private_sessions_do_not_open_a_ledger() {
        let home = tempfile::tempdir().unwrap();
        let aim_home = home.path().join("aim");
        let factory = board_tools_at(aim_home.clone());
        assert!(factory(&spec(Persistence::Ephemeral)).await.is_none());
        assert!(!aim_home.exists(), "private session must not create shared board state");
        let board = factory(&spec(Persistence::Persistent)).await.expect("persistent board tools");
        assert!(board.specs().iter().any(|tool| tool.name == "board_list"));
        let listed =
            board.call("board_list".to_owned(), serde_json::json!({}), IdempotencyKey::new("list")).await.expect("real board list");
        assert!(!listed.is_error);
        assert!(aim_home.join("aim.db").exists());
    }

    #[test]
    fn gateway_sessions_share_one_provider_and_catalog_cache() {
        let (first, _) = build("openrouter", Some("test/model")).expect("provider");
        let (second, _) = build("openrouter", Some("other/model")).expect("provider");
        assert!(Arc::ptr_eq(&first, &second));
    }
}

#[cfg(test)]
mod search_index_tests {
    use super::{SearchIndex, SearchOpen};
    use std::fs::{self, OpenOptions};
    use std::io::Read as _;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::Duration;

    struct LockChild(Child);

    impl Drop for LockChild {
        fn drop(&mut self) {
            drop(self.0.kill());
            drop(self.0.wait());
        }
    }

    #[test]
    fn child_holds_model_lock() {
        let Ok(path) = std::env::var("AIM_LOCK_TEST_PATH") else { return };
        let Ok(ready) = std::env::var("AIM_LOCK_TEST_READY") else { return };
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(path).expect("lock file");
        file.lock().expect("child lock");
        fs::write(ready, b"ready").expect("ready marker");
        drop(std::io::stdin().read(&mut [0_u8; 1]));
    }

    #[tokio::test]
    async fn held_model_lock_retries_and_recovers_without_restart() {
        let dir = tempfile::tempdir().expect("temp directory");
        #[cfg(unix)]
        fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("private state directory");
        let models = dir.path().join("models");
        fs::create_dir_all(&models).expect("models directory");
        let lock_path = models.join("potion-retrieval-32M.lock");
        let ready_path = dir.path().join("child-ready");
        let child = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "providers::search_index_tests::child_holds_model_lock", "--nocapture"])
            .env("AIM_LOCK_TEST_PATH", &lock_path)
            .env("AIM_LOCK_TEST_READY", &ready_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("lock holder process");
        let mut child = LockChild(child);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child acquired the lock");

        let cancel = tokio_util::sync::CancellationToken::new();
        let waiting = tokio::task::spawn_blocking({
            let home = dir.path().to_path_buf();
            let cancel = cancel.clone();
            move || crate::search::embedding::acquire_lock(&home, &cancel, Duration::from_secs(2))
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        let cancelled =
            tokio::time::timeout(Duration::from_millis(500), waiting).await.expect("lock wait was cancellable").expect("lock task joined");
        assert!(matches!(cancelled, Err(crate::search::embedding::ModelOpenError::Interrupted(_))));

        let database = dir.path().join("aim.db");
        let _store = crate::store::SqliteStore::open(&database).expect("initialize session tables");
        let opener = Arc::new(|database: PathBuf, cancel| {
            let home = database.parent().ok_or_else(|| "database has no parent".to_owned())?;
            let _lock = crate::search::embedding::acquire_lock(home, &cancel, Duration::from_millis(80)).map_err(|e| e.to_string())?;
            let engine = crate::search::SearchEngine::open_without_model(&database)?;
            let store = crate::store::SqliteStore::open(&database).map_err(|e| e.to_string())?;
            Ok((Arc::new(engine), Arc::new(store)))
        });
        let index = Arc::new(SearchIndex {
            ready: tokio::sync::watch::Sender::new(None),
            state: std::sync::Mutex::new(SearchOpen::NotStarted),
            generation: std::sync::atomic::AtomicU64::new(0),
            database,
            opener,
            opening_lease: Duration::from_secs(1),
            retry_after: Duration::from_millis(20),
        });
        index.ensure_opening();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(*index.state.lock().expect("state"), SearchOpen::Failed(_)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("held lock causes bounded failed opening");
        drop(child.0.stdin.take());
        child.0.wait().expect("lock holder exited");
        tokio::time::sleep(Duration::from_millis(25)).await;
        index.ensure_opening();
        let mut ready = index.ready.subscribe();
        tokio::time::timeout(Duration::from_secs(5), ready.wait_for(Option::is_some))
            .await
            .expect("search recovered after lock release")
            .expect("ready sender open");
        assert!(matches!(*index.state.lock().expect("state"), SearchOpen::Open));
    }

    #[tokio::test]
    async fn expired_opening_cannot_publish_a_stale_failure() {
        let dir = tempfile::tempdir().expect("temp directory");
        #[cfg(unix)]
        fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("private state directory");
        let _store = crate::store::SqliteStore::open(&dir.path().join("aim.db")).expect("initialize session tables");
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&attempts);
        let opener = Arc::new(move |database: PathBuf, _cancel| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(150));
                return Err("first opening expired".to_owned());
            }
            let engine = crate::search::SearchEngine::open_without_model(&database)?;
            let store = crate::store::SqliteStore::open(&database).map_err(|e| e.to_string())?;
            Ok((Arc::new(engine), Arc::new(store)))
        });
        let index = Arc::new(SearchIndex {
            ready: tokio::sync::watch::Sender::new(None),
            state: std::sync::Mutex::new(SearchOpen::NotStarted),
            generation: std::sync::atomic::AtomicU64::new(0),
            database: dir.path().join("aim.db"),
            opener,
            opening_lease: Duration::from_millis(30),
            retry_after: Duration::from_millis(20),
        });
        index.ensure_opening();
        tokio::time::timeout(Duration::from_secs(5), async {
            while attempts.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("first opening started");
        tokio::time::sleep(Duration::from_millis(40)).await;
        index.ensure_opening();
        let mut ready = index.ready.subscribe();
        tokio::time::timeout(Duration::from_secs(5), ready.wait_for(Option::is_some))
            .await
            .expect("second opening finished")
            .expect("ready sender open");
        tokio::time::sleep(Duration::from_millis(170)).await;
        assert!(matches!(*index.state.lock().expect("state"), SearchOpen::Open));
        assert!(index.ready.borrow().is_some());
    }
}
