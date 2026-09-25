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

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use aim_llm::ModelProvider;
use aim_llm_codex::CodexProvider;
use aim_llm_codex::media::{MediaClient, MediaConfig};
use aim_llm_openai::{OpenAiProvider, Profile};

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
        let provider = OpenAiProvider::new(profile).map_err(|e| format!("{id}: {e}"))?;
        Ok((Arc::new(provider), model.unwrap_or(GATEWAY_DEFAULT_MODEL).to_owned()))
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
///   persistent sessions only.
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
    crate::host::NativeServices { media: Some(media), decider }
}

/// Every backend this build can host: `acp:*` agents, else the native loop with [`build`]'s
/// providers and local workspaces served by the `aimx` binary at `aimx`.
#[must_use]
pub fn backends(aimx: PathBuf, max_requests: u32) -> crate::host::BackendFactory {
    let providers: crate::host::ProviderFactory = Arc::new(build);
    crate::acp::with_acp_at(crate::host::native_backends(providers, crate::host::aimx_workspaces(aimx.clone()), max_requests), aimx)
}
