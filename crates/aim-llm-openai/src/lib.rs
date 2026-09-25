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
//! - **Errors** keep the provider's detail (message, code, type, upstream error), scrubbed of
//!   the API key and truncated, whether they arrive as an HTTP status or inside the stream; 401
//!   bodies are dropped because servers echo key prefixes.

mod catalog;
mod decode;
mod errors;
mod profile;
mod request;
mod sse;

pub use profile::{MaxOutputTokensField, Profile, Quirks, ReasoningParam, Wire};

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
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

/// An OpenAI-compatible provider configured by a profile.
pub struct OpenAiProvider {
    profile: Profile,
    client: Client,
    headers: HeaderMap,
    session_header: Option<HeaderName>,
    /// Image-input support per model id, from the static catalog or the last fetched one.
    vision: Mutex<BTreeMap<String, bool>>,
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

async fn failure(response: Response, key: &str) -> LlmError {
    let status = response.status().as_u16();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let body = read_capped(response, MAX_ERROR_BODY_BYTES).await.map(|(body, _)| body).unwrap_or_default();
    errors::http_error(status, retry_after.as_ref(), &body, &[key])
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
    /// `Authorization` headers); `Transport` if the TLS client cannot be built.
    pub fn new(profile: Profile) -> Result<Self, LlmError> {
        if profile.wire == Wire::Responses {
            return Err(invalid("the Responses wire is not implemented for OpenAI-compatible profiles; set wire = \"chat\""));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &profile.headers {
            let header_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid(format!("profile header name {name:?} is invalid")))?;
            if header_name == header::AUTHORIZATION {
                return Err(invalid("profile headers must not set Authorization: the key is read from api_key_env"));
            }
            let mut header_value =
                HeaderValue::from_str(value).map_err(|_| invalid(format!("profile header {name} has an invalid value")))?;
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
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
        Ok(Self { profile, client, headers, session_header, vision: Mutex::new(vision) })
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

    /// The exact Chat Completions body [`ModelProvider::stream`] sends for `request`.
    ///
    /// # Errors
    /// `InvalidRequest` when the request asks for something the profile cannot express.
    pub fn request_body(&self, request: &Request) -> Result<Value, LlmError> {
        request::request_body(&self.profile, request, self.accepts_images(&request.model))
    }

    /// Tool-result images are sent as image parts unless the profile or the model's catalog
    /// entry says the model cannot take them (unknown models are assumed capable).
    fn accepts_images(&self, model: &str) -> bool {
        self.profile.quirks.tool_result_images && self.vision.lock().ok().and_then(|known| known.get(model).copied()).unwrap_or(true)
    }

    fn remember(&self, models: &[ModelInfo]) {
        if let Ok(mut known) = self.vision.lock() {
            known.extend(models.iter().map(|model| (model.id.clone(), model.images)));
        }
    }

    fn key(&self) -> Result<String, LlmError> {
        std::env::var(&self.profile.api_key_env)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| LlmError::new(LlmErrorKind::Auth, format!("environment variable {} is unset", self.profile.api_key_env)))
    }

    fn endpoint(&self, route: &str) -> String {
        format!("{}/{route}", self.profile.base_url.trim_end_matches('/'))
    }

    fn authorized(&self, builder: RequestBuilder, key: &str) -> RequestBuilder {
        builder.headers(self.headers.clone()).bearer_auth(key)
    }

    async fn fetch_catalog(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let key = self.key()?;
        let response = self.authorized(self.client.get(self.endpoint("models")), &key).send().await.map_err(|error| transport(&error))?;
        if !response.status().is_success() {
            return Err(failure(response, &key).await);
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
            let models = match &self.profile.models {
                Some(models) => models.clone(),
                None => self.fetch_catalog().await?,
            };
            self.remember(&models);
            Ok(models)
        })
    }

    fn stream(&self, request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>> {
        Box::pin(async move {
            let body = self.request_body(&request)?;
            let key = self.key()?;
            let mut builder = self.authorized(self.client.post(self.endpoint("chat/completions")), &key).json(&body);
            if let (Some(name), Some(session)) = (&self.session_header, &request.session_id) {
                let value = HeaderValue::from_str(session).map_err(|_| invalid("the session id is not a valid header value"))?;
                builder = builder.header(name.clone(), value);
            }
            let response = builder.send().await.map_err(|error| transport(&error))?;
            if !response.status().is_success() {
                return Err(failure(response, &key).await);
            }
            let mut decoder = ChatDecoder::new(&self.profile, request::freeform_names(&request.tools), vec![key]);
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
