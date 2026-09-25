//! Bounded TypeScript declarations for the tools actually admitted to a session.

use aim_proto::tool::{ToolInput, ToolSpec};
use serde_json::Value;

const PER_TOOL_BYTES: usize = 16 * 1024;
const TOTAL_BYTES: usize = 32 * 1024;

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn schema_type(schema: &Value, depth: usize) -> String {
    if depth >= 8 {
        return "unknown".to_owned();
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let choices: Vec<String> = values.iter().take(32).filter_map(|value| serde_json::to_string(value).ok()).collect();
        if !choices.is_empty() {
            return choices.join(" | ");
        }
    }
    if let Some(alternatives) = schema.get("anyOf").or_else(|| schema.get("oneOf")).and_then(Value::as_array) {
        return alternatives.iter().take(16).map(|part| schema_type(part, depth + 1)).collect::<Vec<_>>().join(" | ");
    }
    let ty = schema.get("type").and_then(Value::as_str);
    match ty {
        Some("string") => "string".to_owned(),
        Some("integer" | "number") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => {
            format!("Array<{}>", schema.get("items").map_or_else(|| "unknown".to_owned(), |item| schema_type(item, depth + 1)))
        }
        Some("object") | None if schema.get("properties").is_some() => {
            let required = schema.get("required").and_then(Value::as_array);
            let mut fields = Vec::new();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, value) in properties.iter().take(128) {
                    let is_required = required.is_some_and(|items| items.iter().any(|item| item.as_str() == Some(name)));
                    fields.push(format!("{}{}: {}", quoted(name), if is_required { "" } else { "?" }, schema_type(value, depth + 1)));
                }
            }
            if schema.get("additionalProperties").and_then(Value::as_bool) == Some(true) {
                fields.push("[key: string]: unknown".to_owned());
            }
            format!("{{ {} }}", fields.join("; "))
        }
        Some("object") => "Record<string, unknown>".to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Generate bounded TypeScript declarations from the admitted tool schemas.
#[must_use]
pub fn declarations(specs: &[ToolSpec]) -> String {
    let mut result = String::from(
        "type CallToolResult = { content: Array<{ type: string; text?: string }>; is_error: boolean; truncated: boolean };\ndeclare const tools: {\n",
    );
    for spec in specs {
        let input = match spec.input {
            ToolInput::Json => schema_type(&spec.input_schema, 0),
            ToolInput::Freeform { .. } => "string".to_owned(),
        };
        let line = format!("  {}: (args: {}) => Promise<CallToolResult>;\n", quoted(&spec.name), input);
        if line.len() > PER_TOOL_BYTES || result.len().saturating_add(line.len()).saturating_add(3) > TOTAL_BYTES {
            continue;
        }
        result.push_str(&line);
    }
    result.push_str("};\n");
    result
}

/// A compact list of available tools for a code-mode prompt.
#[must_use]
pub fn index(specs: &[ToolSpec]) -> String {
    let mut result = String::from(
        "Tools are available as await tools.NAME(args). Use ALL_TOOLS, describe(name), or search(query) inside a cell for details.\n",
    );
    for spec in specs {
        let summary = spec.description.lines().next().unwrap_or("");
        let line = format!("{}: {}\n", spec.name, summary);
        if result.len().saturating_add(line.len()) > 8 * 1024 {
            break;
        }
        result.push_str(&line);
    }
    result
}

#[cfg(test)]
mod tests {
    use aim_proto::tool::{ToolAnnotations, ToolInput, ToolSpec};
    use serde_json::json;

    use super::{declarations, index};

    #[test]
    fn required_nested_schema_and_budget() {
        let spec = ToolSpec {
            name: "read.file".into(),
            description: "Read one file.\nMore guidance".into(),
            input_schema: json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"limit":{"type":"integer"}}}),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        };
        let ts = declarations(std::slice::from_ref(&spec));
        assert!(ts.contains("\"path\": string"));
        assert!(ts.contains("\"limit\"?: number"));
        assert!(ts.len() < 32 * 1024);
        assert_eq!(
            index(&[spec]),
            "Tools are available as await tools.NAME(args). Use ALL_TOOLS, describe(name), or search(query) inside a cell for details.\nread.file: Read one file.\n"
        );
    }
}
