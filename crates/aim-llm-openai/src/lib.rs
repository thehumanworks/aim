//! OpenAI-compatible model provider configured by named, deserializable endpoint profiles
//! (docs/adr/0011): `OpenRouter`, Vercel AI Gateway, or any compatible endpoint.
//!
//! Chat Completions is implemented. A profile selecting the Responses wire is rejected when the
//! provider is constructed (`InvalidRequest`: a configuration error, never retried).
//!
//! - **Affinity and caching**: `Request.session_id` is sent in the profile's `session_header`
//!   (`OpenRouter` `x-session-id`, AI Gateway `x-session-affinity`); prompt-cache settings come
//!   from `quirks.extra_body` and `quirks.cache_key_field`. `Request.turn_id` and `Request.tier`
//!   are ignored: neither gateway has per-turn affinity state or service tiers on this wire.
//! - **Timeouts**: 10 s to connect, then `quirks.idle_timeout_secs` (default 300 s) without any
//!   response byte — headers, data or keepalive comments — fails the call as `Transport`. There is
//!   no whole-request timeout, so long streams are never cut.
//! - **Tool-result images** are sent as image parts only to a model whose catalog entry says it
//!   accepts images. A request that carries one for a model the provider has not seen yet first
//!   fetches the catalog (once per provider; a static `models` list is authoritative and never
//!   fetched). A model the catalog cannot vouch for gets a text placeholder instead, so a
//!   text-only model is never sent an image it would reject.
//! - **Credentials** are read from the environment for every request: the key from
//!   `api_key_env` and each `{ env = "NAME" }` header value. Profiles hold variable names only.
//! - **Errors** keep the provider's detail (message, code, type, upstream error), scrubbed of
//!   the API key and environment-referenced header values and truncated, whether they arrive as
//!   an HTTP status or inside the stream; 401 bodies are dropped because servers echo key prefixes.

mod catalog;
mod decode;
mod errors;
mod profile;
mod request;
mod sse;

pub use profile::{EnvRef, HeaderSource, MaxOutputTokensField, Profile, Quirks, ReasoningParam, Wire};

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::Item;
use aim_proto::tool::ToolContent;
use async_stream::stream;
use futures_util::StreamExt as _;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, RequestBuilder, Response};
use serde_json::Value;

use crate::decode::ChatDecoder;
use crate::sse::Sse;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_FAILURE_TTL: Duration = Duration::from_secs(1);

struct CatalogCache {
    at: Instant,
    result: Result<Vec<ModelInfo>, LlmError>,
}

/// An OpenAI-compatible provider configured by a profile.
pub struct OpenAiProvider {
    profile: Profile,
    client: Client,
    /// Literal profile headers, validated at construction.
    headers: HeaderMap,
    /// Profile headers whose values are read from these environment variables per request.
    env_headers: Vec<(HeaderName, String)>,
    session_header: Option<HeaderName>,
    /// Image-input support per model id, from the static catalog or the last fetched one.
    vision: Mutex<BTreeMap<String, bool>>,
    /// Whether `stream` already fetched the catalog to learn an unseen model's image support.
    catalog_looked_up: AtomicBool,
    catalog_cache: Mutex<Option<CatalogCache>>,
    catalog_fetch: tokio::sync::Mutex<()>,
}

fn invalid(message: impl Into<String>) -> LlmError {
    LlmError::new(LlmErrorKind::InvalidRequest, message)
}

fn protocol(message: &'static str) -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, message)
}

/// A transport failure; the `reqwest::Error` itself is dropped (it may carry the URL).
fn transport(error: &reqwest::Error) -> LlmError {
    let message = if error.is_timeout() {
        "provider connection timed out"
    } else if error.is_connect() {
        "could not connect to the provider"
    } else {
        "provider transport failure"
    };
    LlmError::new(LlmErrorKind::Transport, message)
}

/// What one request authenticates with. Every value in `secrets` (the key and the
/// environment-referenced header values) is scrubbed from the errors the request reports.
struct Credentials {
    key: String,
    headers: HeaderMap,
    secrets: Vec<String>,
}

/// Reads at most `cap` bytes of a body; the flag says whether it was cut.
async fn read_capped(response: Response, cap: usize) -> Result<(Vec<u8>, bool), LlmError> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| transport(&error))?;
        let room = cap.saturating_sub(body.len());
        body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        if chunk.len() > room {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

async fn failure(response: Response, secrets: &[String]) -> LlmError {
    let status = response.status().as_u16();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let body = read_capped(response, MAX_ERROR_BODY_BYTES).await.map(|(body, _)| body).unwrap_or_default();
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    errors::http_error(status, retry_after.as_ref(), &body, &secrets)
}

/// A non-empty environment variable; `Auth` when it is unset, since it holds a credential.
fn credential(variable: &str, purpose: &str) -> Result<String, LlmError> {
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| LlmError::new(LlmErrorKind::Auth, format!("environment variable {variable} ({purpose}) is unset")))
}

/// Whether any tool result in `items` carries an image.
fn has_tool_image(items: &[Item]) -> bool {
    items.iter().any(|item| {
        matches!(item, Item::ToolResult { result, .. } if result.content.iter().any(|part| matches!(part, ToolContent::Image { .. })))
    })
}

/// Decodes SSE payloads until `[DONE]`; anything after the sentinel is ignored.
fn decode_frames(decoder: &mut ChatDecoder, frames: Vec<String>, done: &mut bool) -> Result<Vec<StreamEvent>, LlmError> {
    let mut events = Vec::new();
    for frame in frames {
        if *done {
            break;
        }
        if frame.trim() == "[DONE]" {
            *done = true;
            continue;
        }
        let value = serde_json::from_str::<Value>(&frame).map_err(|_| protocol("invalid SSE JSON"))?;
        events.extend(decoder.chunk(&value)?);
    }
    Ok(events)
}

impl OpenAiProvider {
    /// Validates the profile and builds a reusable HTTP client.
    ///
    /// # Errors
    /// `InvalidRequest` for a profile this provider cannot serve (Responses wire, invalid or
    /// `Authorization` headers, an empty header variable name); `Transport` if the TLS client
    /// cannot be built. Environment-referenced header values are read per request, not here.
    pub fn new(profile: Profile) -> Result<Self, LlmError> {
        if profile.wire == Wire::Responses {
            return Err(invalid("the Responses wire is not implemented for OpenAI-compatible profiles; set wire = \"chat\""));
        }
        let mut headers = HeaderMap::new();
        let mut env_headers = Vec::new();
        for (name, source) in &profile.headers {
            let header_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid(format!("profile header name {name:?} is invalid")))?;
            if header_name == header::AUTHORIZATION {
                return Err(invalid("profile headers must not set Authorization: the key is read from api_key_env"));
            }
            match source {
                HeaderSource::Literal(value) => {
                    let mut header_value =
                        HeaderValue::from_str(value).map_err(|_| invalid(format!("profile header {name} has an invalid value")))?;
                    header_value.set_sensitive(true);
                    headers.insert(header_name, header_value);
                }
                HeaderSource::Env(EnvRef { env }) if env.is_empty() => {
                    return Err(invalid(format!("profile header {name} references an empty environment variable name")));
                }
                HeaderSource::Env(EnvRef { env }) => env_headers.push((header_name, env.clone())),
            }
        }
        let session_header = profile
            .quirks
            .session_header
            .as_deref()
            .map(|name| HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid(format!("session_header {name:?} is invalid"))))
            .transpose()?;
        let idle = Duration::from_secs(profile.quirks.idle_timeout_secs.unwrap_or(profile::DEFAULT_IDLE_TIMEOUT_SECS));
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(idle)
            .build()
            .map_err(|_| LlmError::new(LlmErrorKind::Transport, "could not build the HTTP client"))?;
        let vision = profile.models.iter().flatten().map(|model| (model.id.clone(), model.images)).collect();
        Ok(Self {
            profile,
            client,
            headers,
            env_headers,
            session_header,
            vision: Mutex::new(vision),
            catalog_looked_up: AtomicBool::new(false),
            catalog_cache: Mutex::new(None),
            catalog_fetch: tokio::sync::Mutex::new(()),
        })
    }

    /// The profile this provider serves.
    #[must_use]
    pub const fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The effective output cap after applying the profile minimum.
    #[must_use]
    pub fn effective_max_output_tokens(&self, requested: Option<u32>) -> Option<u32> {
        self.profile.quirks.effective_max_output_tokens(requested)
    }

    /// The exact Chat Completions body [`ModelProvider::stream`] sends for `request`, given the
    /// model capabilities the provider knows now. (`stream` first fetches the catalog when the
    /// request carries a tool-result image for a model it has not seen; see the crate docs.)
    ///
    /// # Errors
    /// `InvalidRequest` when the request asks for something the profile cannot express.
    pub fn request_body(&self, request: &Request) -> Result<Value, LlmError> {
        request::request_body(&self.profile, request, self.accepts_images(&request.model))
    }

    /// Tool-result images are sent as image parts only when the profile allows them and the
    /// model's catalog entry says it accepts images; a model not in the catalog gets the placeholder.
    fn accepts_images(&self, model: &str) -> bool {
        self.profile.quirks.tool_result_images && self.vision.lock().ok().and_then(|known| known.get(model).copied()).unwrap_or(false)
    }

    /// Before mapping a tool-result image for a model it has not seen, fetches the catalog once
    /// (unless the profile's static `models` list is authoritative). A failed fetch is not an
    /// error: the model is then treated as unable to see images.
    async fn learn_image_support(&self, request: &Request) {
        let unseen = || self.vision.lock().is_ok_and(|known| !known.contains_key(&request.model));
        if self.profile.quirks.tool_result_images
            && self.profile.models.is_none()
            && has_tool_image(&request.items)
            && unseen()
            && !self.catalog_looked_up.swap(true, Ordering::Relaxed)
        {
            let _unavailable = self.catalog().await;
        }
    }

    fn remember(&self, models: &[ModelInfo]) {
        if let Ok(mut known) = self.vision.lock() {
            known.extend(models.iter().map(|model| (model.id.clone(), model.images)));
        }
    }

    fn cached_catalog(&self) -> Option<Result<Vec<ModelInfo>, LlmError>> {
        let cache = self.catalog_cache.lock().ok()?;
        let cache = cache.as_ref()?;
        let ttl = if cache.result.is_ok() { CATALOG_TTL } else { CATALOG_FAILURE_TTL };
        (cache.at.elapsed() < ttl).then(|| cache.result.clone())
    }

    /// Reads the key and the environment-referenced header values for one request.
    fn credentials(&self) -> Result<Credentials, LlmError> {
        let key = credential(&self.profile.api_key_env, "the API key")?;
        let mut headers = self.headers.clone();
        let mut secrets = vec![key.clone()];
        for (name, variable) in &self.env_headers {
            let value = credential(variable, &format!("header {name}"))?;
            let mut header_value = HeaderValue::from_str(&value)
                .map_err(|_| invalid(format!("environment variable {variable} (header {name}) is not a valid header value")))?;
            header_value.set_sensitive(true);
            headers.insert(name.clone(), header_value);
            secrets.push(value);
        }
        Ok(Credentials { key, headers, secrets })
    }

    fn endpoint(&self, route: &str) -> String {
        format!("{}/{route}", self.profile.base_url.trim_end_matches('/'))
    }

    fn authorized(builder: RequestBuilder, credentials: &Credentials) -> RequestBuilder {
        builder.headers(credentials.headers.clone()).bearer_auth(&credentials.key)
    }

    async fn fetch_catalog(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let credentials = self.credentials()?;
        let response =
            Self::authorized(self.client.get(self.endpoint("models")), &credentials).send().await.map_err(|error| transport(&error))?;
        if !response.status().is_success() {
            return Err(failure(response, &credentials.secrets).await);
        }
        let (body, cut) = read_capped(response, MAX_CATALOG_BYTES).await?;
        if cut {
            return Err(protocol("model catalog exceeds 16 MiB"));
        }
        let data = serde_json::from_slice::<Value>(&body).map_err(|_| protocol("model catalog is not JSON"))?;
        catalog::parse_catalog(&data, &self.profile)
    }
}

impl ModelProvider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.profile.id
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async move {
            if let Some(cached) = self.cached_catalog() {
                return cached;
            }
            // A session's background lookup and another session's startup share one in-flight GET.
            let _fetch = self.catalog_fetch.lock().await;
            if let Some(cached) = self.cached_catalog() {
                return cached;
            }
            let result = match &self.profile.models {
                Some(models) => Ok(models.clone()),
                None => self.fetch_catalog().await,
            };
            if let Ok(models) = &result {
                self.remember(models);
            }
            if let Ok(mut cache) = self.catalog_cache.lock() {
                *cache = Some(CatalogCache { at: Instant::now(), result: result.clone() });
            }
            result
        })
    }

    fn stream(&self, request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>> {
        Box::pin(async move {
            self.learn_image_support(&request).await;
            let body = self.request_body(&request)?;
            let credentials = self.credentials()?;
            let mut builder =
                Self::authorized(self.client.post(self.endpoint("chat/completions")), &credentials).json(&request::OrderedChat(&body));
            if let (Some(name), Some(session)) = (&self.session_header, &request.session_id) {
                let value = HeaderValue::from_str(session).map_err(|_| invalid("the session id is not a valid header value"))?;
                builder = builder.header(name.clone(), value);
            }
            let response = builder.send().await.map_err(|error| transport(&error))?;
            if !response.status().is_success() {
                return Err(failure(response, &credentials.secrets).await);
            }
            let mut decoder = ChatDecoder::new(&self.profile, request::freeform_names(&request.tools), credentials.secrets);
            let mut bytes = response.bytes_stream();
            let events = stream! {
                let mut sse = Sse::default();
                let mut done = false;
                loop {
                    let (frames, eof) = match bytes.next().await {
                        Some(Ok(chunk)) => (sse.push(&chunk), false),
                        Some(Err(error)) => {
                            yield Err(transport(&error));
                            return;
                        }
                        None => (sse.finish(), true),
                    };
                    match frames.and_then(|frames| decode_frames(&mut decoder, frames, &mut done)) {
                        Ok(events) => {
                            for event in events {
                                yield Ok(event);
                            }
                        }
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    }
                    if done || eof {
                        break;
                    }
                }
                match decoder.finish(done) {
                    Ok(events) => {
                        for event in events {
                            yield Ok(event);
                        }
                    }
                    Err(error) => yield Err(error),
                }
            };
            Ok(Box::pin(events) as EventStream)
        })
    }
}

#[cfg(test)]
mod tests;
