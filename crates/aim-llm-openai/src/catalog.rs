//! `GET {base}/models` → [`ModelInfo`]; unknown capabilities are filled conservatively.

use aim_llm::{LlmError, LlmErrorKind, ModelInfo};
use serde_json::Value;

use crate::profile::{Profile, ReasoningParam};

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn model(entry: &Value, profile: &Profile) -> Option<ModelInfo> {
    let id = entry.get("id")?.as_str()?.to_owned();
    // AI Gateway lists video, image, embedding, … models too; only language models chat.
    if entry.get("type").and_then(Value::as_str).is_some_and(|kind| kind != "language") {
        return None;
    }
    let params = entry.get("supported_parameters").and_then(Value::as_array);
    let has = |needle: &str| params.is_some_and(|p| p.iter().any(|v| v.as_str() == Some(needle)));
    let reported_efforts = entry
        .get("reasoning_options")
        .and_then(Value::as_array)
        .and_then(|options| options.iter().find(|option| option.get("type").and_then(Value::as_str) == Some("effort")))
        .and_then(|option| option.get("values"))
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>());
    let efforts = match profile.quirks.reasoning_param {
        ReasoningParam::OpenRouter if has("reasoning") || has("reasoning_effort") => {
            strings(&["minimal", "low", "medium", "high", "xhigh"])
        }
        ReasoningParam::OpenAi => {
            reported_efforts.unwrap_or_else(|| if has("reasoning_effort") { strings(&["low", "medium", "high"]) } else { Vec::new() })
        }
        ReasoningParam::None | ReasoningParam::OpenRouter => Vec::new(),
    };
    let modalities =
        entry.pointer("/architecture/input_modalities").or_else(|| entry.pointer("/modalities/input")).and_then(Value::as_array);
    Some(ModelInfo {
        display_name: entry.get("name").and_then(Value::as_str).unwrap_or(&id).into(),
        id,
        context_window: entry.get("context_length").or_else(|| entry.get("context_window")).and_then(Value::as_u64),
        efforts,
        default_effort: None,
        tiers: Vec::new(),
        tools: has("tools"),
        images: modalities.is_some_and(|m| m.iter().any(|v| v.as_str() == Some("image"))),
        hidden: false,
        native: Some(entry.clone()),
    })
}

/// Parses an OpenAI-style model list.
pub(crate) fn parse_catalog(data: &Value, profile: &Profile) -> Result<Vec<ModelInfo>, LlmError> {
    let entries = data
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::new(LlmErrorKind::Protocol, "model catalog lacks a data array"))?;
    Ok(entries.iter().filter_map(|entry| model(entry, profile)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknowns_are_conservative_and_non_language_models_skipped() -> Result<(), LlmError> {
        let models = parse_catalog(
            &json!({"data": [{"id": "one"}, {"id": "video", "type": "video"}, {"id": "embed", "type": "embedding"}]}),
            &Profile::ai_gateway(),
        )?;
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["one"]);
        assert!(models.iter().all(|model| !model.tools && !model.images && model.context_window.is_none() && model.efforts.is_empty()));
        let models = parse_catalog(
            &json!({"data": [{
                "id": "two", "type": "language", "context_window": 128_000, "supported_parameters": ["tools", "reasoning"],
                "modalities": {"input": ["text", "image"]},
                "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high"]}]
            }]}),
            &Profile::ai_gateway(),
        )?;
        let model = models.first().ok_or_else(|| LlmError::new(LlmErrorKind::Protocol, "missing"))?;
        assert!(model.tools && model.images);
        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.efforts, ["none", "low", "medium", "high"]);
        assert_eq!(parse_catalog(&json!({"models": []}), &Profile::openrouter()).err().map(|e| e.kind), Some(LlmErrorKind::Protocol));
        Ok(())
    }

    #[test]
    fn openrouter_shape() -> Result<(), LlmError> {
        let models = parse_catalog(
            &json!({"data": [{
                "id": "anthropic/claude-haiku-4.5", "name": "Claude Haiku 4.5", "context_length": 200_000,
                "architecture": {"input_modalities": ["text", "image"]},
                "supported_parameters": ["tools", "reasoning", "max_tokens"]
            }]}),
            &Profile::openrouter(),
        )?;
        let model = models.first().ok_or_else(|| LlmError::new(LlmErrorKind::Protocol, "missing"))?;
        assert_eq!(model.display_name, "Claude Haiku 4.5");
        assert!(model.tools && model.images && model.efforts.contains(&"low".to_owned()));
        Ok(())
    }
}
