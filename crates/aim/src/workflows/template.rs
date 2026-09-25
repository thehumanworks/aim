//! Bounded interpolation for workflow arguments and prompts.
//!
//! Only `${params.name}` and `${steps.id.result}` are recognized. There are no expressions,
//! environment lookups, file reads, or secret-store lookups. Error messages never contain values.

use std::collections::BTreeMap;
use std::io::{self, Write};

use serde_json::{Map, Value};

const MAX_TEMPLATE_BYTES: usize = 16 * 1024;
const MAX_RENDERED_BYTES: usize = 64 * 1024;
const MAX_REFERENCES: usize = 64;
const MAX_VALUE_DEPTH: usize = 16;
const MAX_VALUE_NODES: usize = 1024;

/// A validated reference in a template.
#[derive(Clone, PartialEq, Eq)]
pub enum TemplateReference {
    /// An invocation parameter property.
    Param(String),
    /// The entire result of a previous step.
    StepResult(String),
}

fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn parse_reference(value: &str) -> Result<TemplateReference, String> {
    if let Some(name) = value.strip_prefix("params.")
        && identifier(name)
    {
        return Ok(TemplateReference::Param(name.into()));
    }
    if let Some(name) = value.strip_prefix("steps.").and_then(|rest| rest.strip_suffix(".result"))
        && identifier(name)
    {
        return Ok(TemplateReference::StepResult(name.into()));
    }
    Err("unsupported workflow template reference".into())
}

fn spans(input: &str) -> Result<Vec<(usize, usize, TemplateReference)>, String> {
    if input.len() > MAX_TEMPLATE_BYTES {
        return Err("template exceeds 16 KiB".into());
    }
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = input.get(cursor..).and_then(|rest| rest.find("${")) {
        if found.len() == MAX_REFERENCES {
            return Err("template exceeds 64 references".into());
        }
        let start = cursor + relative;
        let body_start = start + 2;
        let Some(end_relative) = input.get(body_start..).and_then(|rest| rest.find('}')) else {
            return Err("unterminated workflow template reference".into());
        };
        let end = body_start + end_relative;
        let body = input.get(body_start..end).ok_or("invalid workflow template reference")?;
        found.push((start, end + 1, parse_reference(body)?));
        cursor = end + 1;
    }
    Ok(found)
}

/// Returns validated references in a text template.
///
/// # Errors
/// Returns a bounds or syntax error, with no template values in the message.
pub fn references(input: &str) -> Result<Vec<TemplateReference>, String> {
    Ok(spans(input)?.into_iter().map(|(_, _, reference)| reference).collect())
}

/// Checks a text template without interpolating it.
///
/// # Errors
/// Returns a bounds or syntax error.
pub fn check_text(input: &str) -> Result<(), String> {
    spans(input).map(|_| ())
}

fn check_value_inner(input: &Value, depth: usize, nodes: &mut usize) -> Result<(), String> {
    *nodes += 1;
    if depth > MAX_VALUE_DEPTH || *nodes > MAX_VALUE_NODES {
        return Err("template JSON exceeds nesting or node limit".into());
    }
    match input {
        Value::String(value) => check_text(value),
        Value::Array(values) => {
            for value in values {
                check_value_inner(value, depth + 1, nodes)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            for value in values.values() {
                check_value_inner(value, depth + 1, nodes)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Checks a JSON argument template's depth, node count, and references.
///
/// # Errors
/// Returns a bounds or syntax error.
pub fn check_value(input: &Value) -> Result<(), String> {
    check_value_inner(input, 0, &mut 0)?;
    check_rendered_size(input)
}

fn collect_references(input: &Value, output: &mut Vec<TemplateReference>) -> Result<(), String> {
    match input {
        Value::String(value) => output.extend(references(value)?),
        Value::Array(values) => {
            for value in values {
                collect_references(value, output)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_references(value, output)?;
            }
        }
        _ => {}
    }
    if output.len() > MAX_REFERENCES {
        return Err("argument template exceeds 64 references".into());
    }
    Ok(())
}

/// Returns validated references across a JSON argument template.
///
/// # Errors
/// Returns a bounds or syntax error.
pub fn references_value(input: &Value) -> Result<Vec<TemplateReference>, String> {
    check_value(input)?;
    let mut output = Vec::new();
    collect_references(input, &mut output)?;
    Ok(output)
}

fn resolved<'a>(reference: &TemplateReference, params: &'a Value, steps: &'a BTreeMap<String, Value>) -> Result<&'a Value, String> {
    match reference {
        TemplateReference::Param(name) => params.get(name).ok_or_else(|| format!("missing parameter `{name}`")),
        TemplateReference::StepResult(name) => steps.get(name).ok_or_else(|| format!("missing result for step `{name}`")),
    }
}

fn scalar(value: &Value) -> Result<std::borrow::Cow<'_, str>, String> {
    match value {
        Value::String(value) => Ok(std::borrow::Cow::Borrowed(value)),
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(std::borrow::Cow::Owned(value.to_string())),
        Value::Array(_) | Value::Object(_) => Err("structured template value needs a whole-field reference".into()),
    }
}

struct LimitedWriter {
    bytes: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.checked_add(buffer.len()).ok_or_else(|| io::Error::other("too large"))?;
        if self.bytes > MAX_RENDERED_BYTES {
            return Err(io::Error::other("too large"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_rendered_size(value: &Value) -> Result<(), String> {
    serde_json::to_writer(LimitedWriter { bytes: 0 }, value).map_err(|_| "rendered arguments exceed 64 KiB".into())
}

/// Renders a bounded text template. References to arrays or objects must occupy an entire JSON
/// value and be rendered with [`render_value`].
///
/// # Errors
/// Returns a bounds, reference, or type error without including substituted values.
pub fn render_text(input: &str, params: &Value, steps: &BTreeMap<String, Value>) -> Result<String, String> {
    let found = spans(input)?;
    let mut output = String::new();
    let mut cursor = 0;
    for (start, end, reference) in found {
        output.push_str(input.get(cursor..start).ok_or("invalid template span")?);
        let replacement = scalar(resolved(&reference, params, steps)?)?;
        if output.len().saturating_add(replacement.len()) > MAX_RENDERED_BYTES {
            return Err("rendered template exceeds 64 KiB".into());
        }
        output.push_str(&replacement);
        cursor = end;
    }
    output.push_str(input.get(cursor..).ok_or("invalid template span")?);
    if output.len() > MAX_RENDERED_BYTES {
        return Err("rendered template exceeds 64 KiB".into());
    }
    Ok(output)
}

fn render_inner(input: &Value, params: &Value, steps: &BTreeMap<String, Value>, depth: usize, nodes: &mut usize) -> Result<Value, String> {
    *nodes += 1;
    if depth > MAX_VALUE_DEPTH || *nodes > MAX_VALUE_NODES {
        return Err("template JSON exceeds nesting or node limit".into());
    }
    match input {
        Value::String(value) => {
            let found = spans(value)?;
            if let Some((start, end, reference)) = found.first()
                && *start == 0
                && *end == value.len()
                && found.len() == 1
            {
                let resolved = resolved(reference, params, steps)?;
                check_rendered_size(resolved)?;
                return Ok(resolved.clone());
            }
            render_text(value, params, steps).map(Value::String)
        }
        Value::Array(values) => {
            let mut result = Vec::with_capacity(values.len());
            for value in values {
                result.push(render_inner(value, params, steps, depth + 1, nodes)?);
            }
            Ok(Value::Array(result))
        }
        Value::Object(values) => {
            let mut result = Map::new();
            for (name, value) in values {
                result.insert(name.clone(), render_inner(value, params, steps, depth + 1, nodes)?);
            }
            Ok(Value::Object(result))
        }
        other => Ok(other.clone()),
    }
}

/// Renders JSON arguments while preserving the type of a whole-field reference.
///
/// # Errors
/// Returns a bounds, reference, or type error without including substituted values.
pub fn render_value(input: &Value, params: &Value, steps: &BTreeMap<String, Value>) -> Result<Value, String> {
    check_value(input)?;
    let result = render_inner(input, params, steps, 0, &mut 0)?;
    check_rendered_size(&result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_text_and_typed_json() {
        let params = json!({"path":"demo.txt","count":3});
        let steps = BTreeMap::from([("build".into(), json!({"ok":true}))]);
        assert_eq!(render_text("write ${params.path}", &params, &steps).unwrap(), "write demo.txt");
        let args = json!({"path":"${params.path}", "count":"${params.count}", "result":"${steps.build.result}"});
        assert_eq!(render_value(&args, &params, &steps).unwrap(), json!({"path":"demo.txt","count":3,"result":{"ok":true}}));
    }

    #[test]
    fn rejects_unsupported_or_unbounded_templates() {
        let params = json!({"secret":"private"});
        let steps = BTreeMap::new();
        assert!(render_text("${env.SECRET}", &params, &steps).is_err());
        assert!(render_text("${params.secret", &params, &steps).is_err());
        assert!(!render_text("${params.missing}", &params, &steps).unwrap_err().contains("private"));
        assert!(render_text(&"a".repeat(MAX_TEMPLATE_BYTES + 1), &params, &steps).is_err());
        assert!(render_text(&"${params.secret}".repeat(MAX_REFERENCES + 1), &params, &steps).is_err());
        assert!(render_text("${params.secret}", &json!({"secret":"x".repeat(MAX_RENDERED_BYTES + 1)}), &steps).is_err());
    }
}
