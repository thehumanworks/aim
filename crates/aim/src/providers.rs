//! Providers by id (docs/architecture.md §6.5): what `--provider` and `SessionSpec::provider`
//! name.
//!
//! - `openrouter`, `ai-gateway` — OpenAI-compatible gateways (ADR 0011); keys from
//!   `OPENROUTER_API_KEY` / `AI_GATEWAY_API_KEY`.
//! - `codex` — the `ChatGPT` subscription (ADR 0010). Credentials: aim's own login
//!   (`~/.aim/auth/codex.json`), else the Codex CLI's file, read-only and never refreshed. One
//!   provider (one auth manager) serves the whole process, so a refresh never races itself.
//! - `acp:claude` (Claude Code over ACP, ADR 0012) is wired as its integration lands on main.

use std::sync::{Arc, Mutex, PoisonError};

use aim_llm::ModelProvider;
use aim_llm_codex::CodexProvider;
use aim_llm_openai::{OpenAiProvider, Profile};

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
    let provider = Arc::new(CodexProvider::new().map_err(|e| format!("codex: {e}"))?);
    *slot = Some(Arc::clone(&provider));
    Ok(provider)
}

/// Default model for the OpenAI-compatible gateways (both catalogs list it).
pub const GATEWAY_DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5";

/// Provider ids this build knows.
pub const KNOWN: &[&str] = &["openrouter", "ai-gateway", "codex", "acp:claude"];

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
        "openrouter" => gateway(Profile::openrouter()),
        "ai-gateway" => gateway(Profile::ai_gateway()),
        "codex" => Ok((codex()? as Arc<dyn ModelProvider>, model.unwrap_or(CODEX_DEFAULT_MODEL).to_owned())),
        "acp:claude" => Err(format!("provider `{id}` is not wired into this build yet")),
        other => Err(format!("unknown provider `{other}` (known: {})", KNOWN.join(", "))),
    }
}
