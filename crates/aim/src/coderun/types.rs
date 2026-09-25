//! Bounded TypeScript declarations for the tools actually admitted to a session.

use aim_proto::tool::{ToolInput, ToolSpec};
use serde_json::Value;

const PER_TOOL_BYTES: usize = 16 * 1024;
const TOTAL_BYTES: usize = 32 * 1024;
/// Most bytes one tool's signature may take in the model-visible API ([`api`]).
const API_TOOL_BYTES: usize = 1024;
/// Most bytes of tool names listed after the typed signatures ([`api`]); as W26's index.
const API_NAMES_BYTES: usize = 512;

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// A property or member name: bare when it is a plain identifier, else quoted.
fn key(value: &str) -> String {
    let mut chars = value.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain { value.to_owned() } else { quoted(value) }
}

fn schema_type(schema: &Value, depth: usize) -> String {
    schema_type_with(schema, depth, quoted)
}

fn schema_type_with(schema: &Value, depth: usize, name: fn(&str) -> String) -> String {
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
        return alternatives.iter().take(16).map(|part| schema_type_with(part, depth + 1, name)).collect::<Vec<_>>().join(" | ");
    }
    let ty = schema.get("type").and_then(Value::as_str);
    match ty {
        Some("string") => "string".to_owned(),
        Some("integer" | "number") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => {
            format!("Array<{}>", schema.get("items").map_or_else(|| "unknown".to_owned(), |item| schema_type_with(item, depth + 1, name)))
        }
        Some("object") | None if schema.get("properties").is_some() => {
            let required = schema.get("required").and_then(Value::as_array);
            let mut fields = Vec::new();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (property, value) in properties.iter().take(128) {
                    let is_required = required.is_some_and(|items| items.iter().any(|item| item.as_str() == Some(property)));
                    let optional = if is_required { "" } else { "?" };
                    fields.push(format!("{}{optional}: {}", name(property), schema_type_with(value, depth + 1, name)));
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
        "type CallToolResult = { text: string; content: Array<{ type: string; text?: string }>; is_error: boolean; truncated: boolean };\ndeclare const tools: {\n",
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

/// The model-visible API of the tools callable in a cell (ADR 0076): a TypeScript signature for
/// each tool `typed` selects, in order, while `budget` bytes last, and every other tool by name
/// (at most 512 bytes of names, as W26's index). A result's `.text` joins its text parts.
#[must_use]
pub fn api(specs: &[ToolSpec], typed: &dyn Fn(&str) -> bool, budget: usize) -> String {
    let mut signatures = String::new();
    let mut named = Vec::new();
    for spec in specs {
        if typed(&spec.name) {
            let input = match spec.input {
                ToolInput::Json => schema_type_with(&spec.input_schema, 0, key),
                ToolInput::Freeform { .. } => "string".to_owned(),
            };
            let line = format!("  {}(args: {input}): R;\n", key(&spec.name));
            if line.len() <= API_TOOL_BYTES && signatures.len().saturating_add(line.len()) <= budget {
                signatures.push_str(&line);
                continue;
            }
        }
        named.push(spec.name.as_str());
    }
    let mut result = String::new();
    if !signatures.is_empty() {
        result.push_str("type R = Promise<{ text: string; is_error: boolean; content: unknown[] }>;\ndeclare const tools: {\n");
        result.push_str(&signatures);
        result.push_str("};\n");
    }
    if !named.is_empty() {
        let mut listed = String::new();
        for name in named {
            let separator = if listed.is_empty() { "" } else { ", " };
            if listed.len().saturating_add(separator.len()).saturating_add(name.len()) > API_NAMES_BYTES {
                break;
            }
            listed.push_str(separator);
            listed.push_str(name);
        }
        result.push_str(if signatures.is_empty() { "Cell tools: " } else { "Also in cells: " });
        result.push_str(&listed);
        result.push_str(".\n");
    }
    result.push_str("describe(name) returns a tool's schema; search(query) finds tools.\n");
    result
}

/// A compact list of available tools for a code-mode prompt.
#[must_use]
pub fn index(specs: &[ToolSpec]) -> String {
    let mut result = String::from("Cell tools: ");
    for spec in specs {
        let separator = if result.ends_with(": ") { "" } else { ", " };
        if result.len().saturating_add(separator.len()).saturating_add(spec.name.len()) > 512 {
            break;
        }
        result.push_str(separator);
        result.push_str(&spec.name);
    }
    result.push_str(". Use ALL_TOOLS, describe(name), or search(query) for schemas.\n");
    result
}

#[cfg(test)]
mod tests {
    use aim_proto::tool::{ToolAnnotations, ToolInput, ToolSpec};
    use serde_json::json;

    use super::{api, declarations, index};

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
        assert_eq!(index(&[spec]), "Cell tools: read.file. Use ALL_TOOLS, describe(name), or search(query) for schemas.\n");
    }

    fn tool(name: &str, properties: usize) -> ToolSpec {
        let properties: serde_json::Map<String, serde_json::Value> =
            (0..properties).map(|i| (format!("field_{i}"), json!({"type":"string"}))).collect();
        ToolSpec {
            name: name.into(),
            description: "A tool.".into(),
            input_schema: json!({"type":"object","required":["field_0"],"properties":properties}),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        }
    }

    /// ADR 0076: the selected tools are typed while the budget lasts (bare identifiers, required
    /// fields plain, optional ones marked), and every other tool is still named.
    #[test]
    fn the_cell_api_types_selected_tools_within_its_budget() {
        let specs = [tool("Glob", 2), tool("Read", 1), tool("huge", 200), tool("Grep", 1)];
        let text = api(&specs, &|name| name != "Read", 4096);
        assert!(text.contains("  Glob(args: { field_0: string; field_1?: string }): R;\n"), "{text}");
        assert!(text.contains("  Grep(args: { field_0: string }): R;\n"), "{text}");
        assert!(!text.contains("huge(args"), "a signature over the per-tool limit is only named: {text}");
        assert!(text.contains("Also in cells: Read, huge."), "{text}");
        let tight = api(&specs, &|_| true, 60);
        assert!(
            tight.contains("Glob(args") && !tight.contains("Grep(args") && tight.contains("Also in cells: Read, huge, Grep."),
            "{tight}"
        );
        let untyped = api(&specs, &|_| false, 4096);
        assert!(untyped.starts_with("Cell tools: Glob, Read, huge, Grep.\n"), "{untyped}");
    }
}
