//! `aim-llm-codex`: the `ChatGPT` Codex subscription backend as an [`aim_llm::ModelProvider`]
//! (docs/adr/0010, docs/research/codex-backend.md, docs/research/live-probes.md).
//!
//! Endpoints, the catalog client version and every timeout are data ([`CodexConfig`]), so tests
//! drive the provider against a local fake server and deployments can point it elsewhere.
//! Credentials come from [`auth`]: aim's own store first, then the Codex CLI's file, read-only.

pub mod auth;
mod errors;
mod limits;
mod stream;
mod turn_state;
mod wire;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod live;
#[cfg(test)]
mod offline;

use std::sync::Arc;
use std::time::Duration;

use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, ServiceTier, StreamEvent};
use aim_proto::conversation::Item;
use futures_util::StreamExt as _;
use reqwest::header::{ACCEPT, ETAG, HeaderMap, IF_NONE_MATCH, USER_AGENT};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::{AuthManager, Credentials, FileCredentialStore};
use crate::stream::{DriveOptions, drive, idle_timeout, unix_now};
use crate::turn_state::TurnStates;
use crate::wire::{BodyOptions, request_body};

/// The `originator` header value identifying aim to the backend.
const ORIGINATOR: &str = "aim";
/// Sessions whose turn-state token is remembered at once.
const TURN_STATE_SESSIONS: usize = 128;

/// Where the provider connects and how patient it is. Every field is data with a default.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodexConfig {
    /// Responses and models base URL.
    pub base_url: String,
    /// OAuth issuer.
    pub issuer: String,
    /// `client_version` sent to the catalog; older versions do not list the newest models
    /// (research §8).
    pub client_version: String,
    /// TCP and TLS connect timeout.
    pub connect_timeout: Duration,
    /// Longest silence tolerated while waiting for response headers or the next stream chunk.
    /// There is no total deadline on a streamed response.
    pub idle_timeout: Duration,
    /// Total deadline of short calls: catalog, OAuth token requests, device polls, error bodies.
    pub request_timeout: Duration,
    /// Largest SSE event accepted, in bytes.
    pub max_event_bytes: usize,
    /// Loopback ports tried in order for the browser-login callback.
    pub callback_ports: Vec<u16>,
    /// How long a browser or device login waits for the user.
    pub login_timeout: Duration,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            issuer: "https://auth.openai.com".into(),
            client_version: "0.158.0".into(),
            connect_timeout: Duration::from_secs(15),
            // codex's default stream idle timeout (refs:model-provider-info/src/lib.rs:63).
            idle_timeout: Duration::from_secs(300),
            request_timeout: Duration::from_secs(60),
            max_event_bytes: 16 * 1024 * 1024,
            // refs:login/src/server.rs:76-79,193-201.
            callback_ports: vec![1455, 1457],
            login_timeout: Duration::from_mins(15),
        }
    }
}

impl CodexConfig {
    /// The HTTP client aim uses for this backend: a connect timeout, and deliberately **no**
    /// total timeout (it would cut every streamed turn longer than it). Idle and per-call
    /// deadlines are applied per request.
    ///
    /// # Errors
    /// `Transport` if the TLS stack cannot be initialised.
    pub fn http_client(&self) -> Result<reqwest::Client, LlmError> {
        self.http_client_builder().build().map_err(|_| LlmError::new(LlmErrorKind::Transport, "cannot create the Codex HTTP client"))
    }

    pub(crate) fn http_client_builder(&self) -> reqwest::ClientBuilder {
        reqwest::Client::builder().connect_timeout(self.connect_timeout)
    }

    fn drive_options(&self) -> DriveOptions {
        DriveOptions { idle_timeout: self.idle_timeout, max_event_bytes: self.max_event_bytes }
    }
}

fn error(kind: LlmErrorKind, message: &str) -> LlmError {
    LlmError::new(kind, message)
}

/// A send failure, without the underlying error text (it may carry the URL).
fn send_error(error: &reqwest::Error, what: &str) -> LlmError {
    let why = if error.is_connect() {
        "cannot connect"
    } else if error.is_timeout() {
        "timed out"
    } else {
        "transport failed"
    };
    LlmError::new(LlmErrorKind::Transport, format!("{what}: {why}"))
}

fn header_string(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get(key).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

fn user_agent() -> String {
    format!("aim/{}", env!("CARGO_PKG_VERSION"))
}

fn catalog_entry(value: &Value) -> Option<ModelInfo> {
    let id = value.get("slug").and_then(Value::as_str)?.to_owned();
    let efforts = value
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("effort").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let tiers = value
        .get("service_tiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.as_str().or_else(|| entry.get("id").and_then(Value::as_str))?;
            Some(ServiceTier {
                id: id.to_owned(),
                name: entry.get("name").and_then(Value::as_str).unwrap_or(id).to_owned(),
                description: entry.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(),
            })
        })
        .collect();
    Some(ModelInfo {
        display_name: value.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_owned(),
        context_window: value.get("context_window").and_then(Value::as_u64),
        efforts,
        default_effort: value.get("default_reasoning_level").and_then(Value::as_str).map(str::to_owned),
        tiers,
        tools: value.get("tool_mode").and_then(Value::as_str) != Some("none"),
        images: value.get("input_modalities").and_then(Value::as_array).is_none_or(|m| m.iter().any(|v| v.as_str() == Some("image"))),
        // codex `ModelVisibility` is `list | hide | none` (refs:protocol/src/openai_models.rs:285-295).
        hidden: matches!(value.get("visibility").and_then(Value::as_str), Some("hide" | "none")),
        native: Some(value.clone()),
        id,
    })
}

/// The last catalog, per account (entitlements differ between accounts).
struct CatalogCache {
    account_id: String,
    etag: Option<String>,
    models: Vec<ModelInfo>,
}

/// The Codex provider. Share one instance (or one [`AuthManager`]) per process.
pub struct CodexProvider {
    client: reqwest::Client,
    config: CodexConfig,
    auth: Arc<AuthManager>,
    catalog_cache: Mutex<Option<CatalogCache>>,
    turn_states: Mutex<TurnStates>,
}

impl CodexProvider {
    /// The provider with default endpoints and aim's own credential store
    /// (`~/.aim/auth/codex.json`, falling back to the Codex CLI's file, read-only).
    ///
    /// # Errors
    /// If the HTTP client or the credential path cannot be prepared.
    pub fn new() -> Result<Self, LlmError> {
        Self::with_config(CodexConfig::default())
    }

    /// The provider with custom endpoints and aim's own credential store.
    ///
    /// # Errors
    /// If the HTTP client or the credential path cannot be prepared.
    pub fn with_config(config: CodexConfig) -> Result<Self, LlmError> {
        let client = config.http_client()?;
        let store = Arc::new(FileCredentialStore::new(FileCredentialStore::default_path()?));
        let auth = Arc::new(AuthManager::with_config(client.clone(), store, &config));
        Ok(Self::with_auth(config, client, auth))
    }

    /// The provider with a caller-provided client and auth manager.
    #[must_use]
    pub fn with_auth(config: CodexConfig, client: reqwest::Client, auth: Arc<AuthManager>) -> Self {
        Self { client, config, auth, catalog_cache: Mutex::new(None), turn_states: Mutex::new(TurnStates::new(TURN_STATE_SESSIONS)) }
    }

    /// The auth manager (for login flows and credential status).
    #[must_use]
    pub fn auth(&self) -> &Arc<AuthManager> {
        &self.auth
    }

    /// Catalog data for `model` when the catalog has been fetched: whether it accepts
    /// `reasoning.summary` (codex sends it only then, refs:core/src/client.rs:883-885).
    async fn body_options(&self, model: &str) -> BodyOptions {
        let cache = self.catalog_cache.lock().await;
        let flag = cache
            .as_ref()
            .and_then(|cache| cache.models.iter().find(|m| m.id == model))
            .and_then(|m| m.native.as_ref())
            .and_then(|native| native.get("supports_reasoning_summary_parameter"))
            .and_then(Value::as_bool);
        BodyOptions { reasoning_summary: flag.unwrap_or(true) }
    }

    fn authorized(builder: reqwest::RequestBuilder, credentials: &Credentials) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(credentials.access_token.expose())
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("originator", ORIGINATOR)
            .header(USER_AGENT, user_agent())
    }

    async fn failed(&self, response: reqwest::Response, context: &str) -> LlmError {
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // The token was rejected: re-read the sources on the next call.
            self.auth.invalidate().await;
        }
        errors::from_response(response, self.config.request_timeout, context, unix_now()).await
    }

    async fn start(&self, request: Request, trigger_compaction: bool) -> Result<EventStream, LlmError> {
        let credentials = self.auth.credentials().await?;
        let mut body = request_body(&request, &self.body_options(&request.model).await);
        if trigger_compaction {
            let input = body
                .get_mut("input")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| error(LlmErrorKind::Protocol, "invalid compaction input"))?;
            input.push(json!({"type": "compaction_trigger"}));
        }
        let mut builder = Self::authorized(self.client.post(format!("{}/responses", self.config.base_url)), &credentials)
            .header(ACCEPT, "text/event-stream")
            .json(&body);
        // Turn state is scoped to (session, turn); without both it is neither sent nor kept.
        let turn = request.session_id.as_deref().zip(request.turn_id.as_deref());
        if let Some(session) = &request.session_id {
            builder = builder.header("session-id", session);
        }
        if let Some((session, turn)) = turn
            && let Some(state) = self.turn_states.lock().await.get(session, turn)
        {
            builder = builder.header("x-codex-turn-state", state);
        }
        let response = tokio::time::timeout(self.config.idle_timeout, builder.send())
            .await
            .map_err(|_| idle_timeout(self.config.idle_timeout))?
            .map_err(|e| send_error(&e, "Codex request"))?;
        if !response.status().is_success() {
            return Err(self.failed(response, "Codex request").await);
        }
        let limits = limits::from_headers(response.headers(), unix_now());
        if let (Some((session, turn)), Some(state)) = (turn, header_string(response.headers(), "x-codex-turn-state")) {
            self.turn_states.lock().await.record(session, turn, state);
        }
        Ok(drive(response.bytes_stream(), limits, self.config.drive_options()))
    }

    /// V2 remote compaction: sends the history with a trailing `compaction_trigger` item and
    /// returns the single encrypted `compaction` item (research §7).
    ///
    /// # Errors
    /// If the request or stream fails, or the response has no, several, or an empty compaction item.
    pub async fn compact(&self, request: Request) -> Result<Item, LlmError> {
        let mut stream = self.start(request, true).await?;
        let mut compacted = None;
        let mut completed = false;
        while let Some(next) = stream.next().await {
            match next? {
                StreamEvent::ItemDone { item: item @ Item::Compaction { .. } } => {
                    if compacted.is_some() {
                        return Err(error(LlmErrorKind::Protocol, "Codex returned several compaction items"));
                    }
                    if let Item::Compaction { native } = &item
                        && native.value.get("encrypted_content").and_then(Value::as_str).is_none_or(str::is_empty)
                    {
                        return Err(error(LlmErrorKind::Protocol, "Codex compaction item has no encrypted content"));
                    }
                    compacted = Some(item);
                }
                StreamEvent::Completed { .. } => completed = true,
                _ => {}
            }
        }
        if !completed {
            return Err(error(LlmErrorKind::Protocol, "Codex compaction did not complete"));
        }
        compacted.ok_or_else(|| error(LlmErrorKind::Protocol, "Codex compaction returned no compaction item"))
    }

    async fn fetch_catalog(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let credentials = self.auth.credentials().await?;
        let cached_etag = {
            let cache = self.catalog_cache.lock().await;
            cache.as_ref().filter(|c| c.account_id == credentials.account_id).and_then(|c| c.etag.clone())
        };
        let mut url = url::Url::parse(&format!("{}/models", self.config.base_url))
            .map_err(|_| error(LlmErrorKind::InvalidRequest, "invalid Codex base URL"))?;
        url.query_pairs_mut().append_pair("client_version", &self.config.client_version);
        let mut request = Self::authorized(self.client.get(url), &credentials).timeout(self.config.request_timeout);
        if let Some(etag) = &cached_etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(|e| send_error(&e, "Codex catalog"))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            let cache = self.catalog_cache.lock().await;
            return cache
                .as_ref()
                .filter(|c| c.account_id == credentials.account_id)
                .map(|c| c.models.clone())
                .ok_or_else(|| error(LlmErrorKind::Protocol, "Codex catalog returned 304 without a cached copy"));
        }
        if !response.status().is_success() {
            return Err(self.failed(response, "Codex catalog").await);
        }
        let etag = header_string(response.headers(), ETAG.as_str());
        let bytes = response.bytes().await.map_err(|_| error(LlmErrorKind::Transport, "Codex catalog transfer failed"))?;
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| error(LlmErrorKind::Protocol, "Codex catalog is not JSON"))?;
        let entries =
            body.get("models").and_then(Value::as_array).ok_or_else(|| error(LlmErrorKind::Protocol, "Codex catalog has no models"))?;
        let models: Vec<ModelInfo> = entries.iter().filter_map(catalog_entry).collect();
        *self.catalog_cache.lock().await = Some(CatalogCache { account_id: credentials.account_id, etag, models: models.clone() });
        Ok(models)
    }
}

impl ModelProvider for CodexProvider {
    fn id(&self) -> &'static str {
        wire::PROVIDER
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(self.fetch_catalog())
    }

    fn stream(&self, request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>> {
        Box::pin(self.start(request, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_entries() {
        let entry = json!({"slug":"gpt-6-luna","display_name":"Luna","context_window":272_000,
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"}],"default_reasoning_level":"low",
            "visibility":"list","service_tiers":[{"id":"priority","name":"Fast"}],"input_modalities":["text"]});
        let model = catalog_entry(&entry).unwrap();
        assert_eq!(model.efforts, ["low", "xhigh"]);
        assert_eq!((model.context_window, model.default_effort.as_deref()), (Some(272_000), Some("low")));
        assert_eq!(model.tiers[0].name, "Fast");
        assert!(!model.hidden && !model.images && model.tools);
        for visibility in ["hide", "none"] {
            let hidden = json!({"slug":"x","visibility":visibility});
            assert!(catalog_entry(&hidden).unwrap().hidden, "{visibility}");
        }
        assert!(catalog_entry(&json!({"display_name":"no slug"})).is_none());
    }

    #[test]
    fn default_config_matches_the_reference_client() {
        let config = CodexConfig::default();
        assert_eq!(config.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(config.issuer, "https://auth.openai.com");
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.callback_ports, [1455, 1457]);
    }
}
