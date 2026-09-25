//! Parsed, bounded declarative workflow definitions (ADR 0071).
//!
//! A caller reads `workflow.toml` through its workspace and establishes hash-pinned trust before
//! using this parser. This module does not grant authority or read project files.

use std::collections::{BTreeMap, BTreeSet};

use aim_proto::harness::CallScope;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::template;

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_STEPS: usize = 64;

/// A parsed `workflow.toml` document. JSON Schema fields are JSON-encoded TOML strings.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowManifest {
    /// Directory-safe workflow name.
    pub name: String,
    /// Workflow format version. This slice accepts version `1`.
    pub version: String,
    /// Trigger; only manual invocation is currently admitted.
    pub trigger: WorkflowTrigger,
    /// JSON Schema for invocation parameters.
    #[serde(deserialize_with = "json_value")]
    pub params: Value,
    /// JSON Schema for the final result.
    #[serde(deserialize_with = "json_value")]
    pub results: Value,
    /// Authority ceiling, subsequently intersected with the caller's ceiling.
    pub ceiling: CallScope,
    /// Per-run resource budget.
    pub budget: WorkflowBudget,
    /// DAG nodes in declaration order.
    pub steps: Vec<WorkflowStep>,
}

/// Supported workflow trigger.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTrigger {
    /// Started explicitly by a caller.
    Manual,
}

/// Hard ceilings for a workflow run.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBudget {
    /// Maximum distinct steps in a run; must cover the declared DAG.
    pub max_steps: u32,
    /// Maximum aggregate model tokens; enforced by the runner.
    pub max_tokens: u64,
    /// Maximum wall-clock duration, in seconds; enforced by the runner.
    pub timeout_seconds: u64,
}

/// One durable workflow step.
#[derive(Clone, PartialEq, Deserialize)]
pub struct WorkflowStep {
    /// Stable step identifier.
    pub id: String,
    /// Step operation and its arguments.
    #[serde(flatten)]
    pub kind: StepKind,
    /// Steps that must succeed before this one is ready.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Additional attempts after the initial attempt.
    #[serde(default)]
    pub retries: u32,
    /// Optional step deadline in seconds.
    pub timeout_seconds: Option<u64>,
}

/// Operations supported by a declarative workflow step.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    /// One dispatcher tool call with JSON arguments.
    Tool {
        /// Dispatcher tool name.
        tool: String,
        /// JSON arguments, encoded as a JSON string in TOML.
        #[serde(deserialize_with = "json_value")]
        arguments: Value,
    },
    /// One turn of a named agent.
    Agent {
        /// Agent definition name.
        agent: String,
        /// Optional configured provider; absent uses the named agent's provider or caller default.
        provider: Option<String>,
        /// Optional configured model; absent uses the named agent's model or provider default.
        model: Option<String>,
        /// Bounded prompt template.
        prompt: String,
    },
    /// A board job posted and awaited.
    Job {
        /// Board job title template.
        title: String,
        /// Board job description template.
        description: String,
    },
}

fn json_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    let raw = String::deserialize(deserializer)?;
    serde_json::from_str(&raw).map_err(|_| serde::de::Error::custom("expected valid JSON string"))
}

fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn schema_type(schema: &Value) -> Result<&str, String> {
    let object = schema.as_object().ok_or("schema must be a JSON object")?;
    let kind = object.get("type").and_then(Value::as_str).ok_or("schema needs a string `type`")?;
    if !matches!(kind, "object" | "array" | "string" | "integer" | "number" | "boolean" | "null") {
        return Err("schema has an unsupported `type`".into());
    }
    Ok(kind)
}

fn validate_schema(schema: &Value, depth: usize) -> Result<(), String> {
    if depth > 12 {
        return Err("schema nesting exceeds 12 levels".into());
    }
    let kind = schema_type(schema)?;
    let Some(object) = schema.as_object() else { return Err("schema must be a JSON object".into()) };
    if kind != "object"
        && (object.contains_key("properties") || object.contains_key("required") || object.contains_key("additionalProperties"))
    {
        return Err("object schema keywords require object type".into());
    }
    if kind != "array" && object.contains_key("items") {
        return Err("schema `items` requires array type".into());
    }
    if kind == "object" {
        let properties = object.get("properties");
        if let Some(properties) = properties {
            let properties = properties.as_object().ok_or("schema `properties` must be an object")?;
            if properties.len() > 128 {
                return Err("schema has too many properties".into());
            }
            for (name, child) in properties {
                if !identifier(name) {
                    return Err("schema property name is invalid".into());
                }
                validate_schema(child, depth + 1)?;
            }
        }
        if let Some(required) = object.get("required") {
            let required = required.as_array().ok_or("schema `required` must be an array")?;
            let properties = properties.and_then(Value::as_object);
            for name in required {
                let name = name.as_str().ok_or("schema `required` entries must be strings")?;
                if !properties.is_some_and(|properties| properties.contains_key(name)) {
                    return Err("schema `required` names must occur in `properties`".into());
                }
            }
        }
    }
    if kind == "array" {
        let items = object.get("items").ok_or("array schema needs `items`")?;
        validate_schema(items, depth + 1)?;
    }
    if let Some(additional) = object.get("additionalProperties")
        && !additional.is_boolean()
    {
        return Err("schema `additionalProperties` must be boolean".into());
    }
    if let Some(values) = object.get("enum") {
        let values = values.as_array().ok_or("schema `enum` must be an array")?;
        if values.is_empty() || values.len() > 128 {
            return Err("schema `enum` must have 1..=128 values".into());
        }
        for value in values {
            if !value_matches_type(value, kind) {
                return Err("schema `enum` value has wrong type".into());
            }
        }
    }
    for key in object.keys() {
        if !matches!(key.as_str(), "type" | "properties" | "required" | "items" | "additionalProperties" | "enum" | "description") {
            return Err(format!("unsupported schema keyword `{key}`"));
        }
    }
    Ok(())
}

fn value_matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn validate_value(schema: &Value, value: &Value, depth: usize) -> Result<(), String> {
    if depth > 12 {
        return Err("value nesting exceeds 12 levels".into());
    }
    let kind = schema_type(schema)?;
    if !value_matches_type(value, kind) {
        return Err(format!("expected {kind}"));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err("value is outside schema `enum`".into());
    }
    if kind == "object" {
        let Some(actual) = value.as_object() else { return Err("expected object".into()) };
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !actual.contains_key(name) {
                    return Err(format!("missing required property `{name}`"));
                }
            }
        }
        for (name, child) in actual {
            if let Some(child_schema) = properties.and_then(|properties| properties.get(name)) {
                validate_value(child_schema, child, depth + 1).map_err(|error| format!("property `{name}`: {error}"))?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("unexpected property `{name}`"));
            }
        }
    }
    if let (Some(items), Some(actual)) = (schema.get("items"), value.as_array()) {
        if actual.len() > 1024 {
            return Err("array exceeds 1024 items".into());
        }
        for (index, child) in actual.iter().enumerate() {
            validate_value(items, child, depth + 1).map_err(|error| format!("item {index}: {error}"))?;
        }
    }
    Ok(())
}

impl WorkflowManifest {
    /// Parses and validates a bounded `workflow.toml` string.
    ///
    /// # Errors
    /// Returns a redacted parse or validation error. The caller must establish project trust.
    pub fn parse(source: &str) -> Result<Self, String> {
        if source.len() > MAX_MANIFEST_BYTES {
            return Err("workflow.toml exceeds 256 KiB".into());
        }
        let raw: toml::Value = toml::from_str(source).map_err(|_| "invalid workflow.toml".to_owned())?;
        if let Some(steps) = raw.get("steps").and_then(toml::Value::as_array) {
            for step in steps {
                let Some(table) = step.as_table() else { return Err("workflow step must be a table".into()) };
                let fields: &[&str] = match table.get("kind").and_then(toml::Value::as_str) {
                    Some("tool") => &["tool", "arguments"],
                    Some("agent") => &["agent", "provider", "model", "prompt"],
                    Some("job") => &["title", "description"],
                    _ => return Err("workflow step kind must be tool, agent, or job".into()),
                };
                for field in table.keys() {
                    if !matches!(field.as_str(), "id" | "kind" | "depends_on" | "retries" | "timeout_seconds")
                        && !fields.contains(&field.as_str())
                    {
                        return Err(format!("workflow step has unknown field `{field}`"));
                    }
                }
            }
        }
        let manifest: Self = toml::from_str(source).map_err(|_| "invalid workflow.toml".to_owned())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates a constructed or deserialized manifest.
    ///
    /// # Errors
    /// Returns a field-specific error without printing parameter or result values.
    pub fn validate(&self) -> Result<(), String> {
        if !identifier(&self.name) {
            return Err("workflow name must start with a lowercase letter and contain only lowercase letters, digits, `_`, or `-` (at most 64 bytes)".into());
        }
        if self.version != "1" {
            return Err("workflow version must be `1`".into());
        }
        validate_schema(&self.params, 0).map_err(|error| format!("params: {error}"))?;
        validate_schema(&self.results, 0).map_err(|error| format!("results: {error}"))?;
        if schema_type(&self.params)? != "object" || schema_type(&self.results)? != "object" {
            return Err("params and results schemas must have object type".into());
        }
        if self.budget.max_steps == 0
            || self.budget.max_steps > 64
            || self.budget.max_tokens == 0
            || self.budget.max_tokens > 10_000_000
            || self.budget.timeout_seconds == 0
            || self.budget.timeout_seconds > 604_800
        {
            return Err("budget limits exceed supported bounds".into());
        }
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS || self.steps.len() > self.budget.max_steps as usize {
            return Err("workflow must have 1..=64 steps within budget.max_steps".into());
        }
        if self.ceiling.ops.iter().any(|op| !matches!(op.as_str(), "read" | "write" | "exec")) {
            return Err("ceiling.ops contains an unknown operation".into());
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            if !identifier(&step.id) {
                return Err("step id is invalid".into());
            }
            if !ids.insert(step.id.as_str()) {
                return Err(format!("duplicate step id `{}`", step.id));
            }
            if step.retries > 10 {
                return Err(format!("step `{}` retries exceeds 10", step.id));
            }
            if step.timeout_seconds.is_some_and(|timeout| timeout == 0 || timeout > self.budget.timeout_seconds) {
                return Err(format!("step `{}` timeout is outside the run budget", step.id));
            }
            match &step.kind {
                StepKind::Tool { tool, arguments } => {
                    if tool.is_empty() || tool.len() > 128 || !arguments.is_object() {
                        return Err(format!("step `{}` needs a tool name and JSON object arguments", step.id));
                    }
                    template::check_value(arguments).map_err(|error| format!("step `{}` arguments: {error}", step.id))?;
                }
                StepKind::Agent { agent, provider, model, prompt } => {
                    if !identifier(agent)
                        || prompt.is_empty()
                        || provider.as_ref().is_some_and(String::is_empty)
                        || model.as_ref().is_some_and(String::is_empty)
                    {
                        return Err(format!("step `{}` needs a valid agent name and prompt", step.id));
                    }
                    template::check_text(prompt).map_err(|error| format!("step `{}` prompt: {error}", step.id))?;
                }
                StepKind::Job { title, description } => {
                    if title.is_empty() || description.is_empty() {
                        return Err(format!("step `{}` needs a title and description", step.id));
                    }
                    template::check_text(title).map_err(|error| format!("step `{}` title: {error}", step.id))?;
                    template::check_text(description).map_err(|error| format!("step `{}` description: {error}", step.id))?;
                }
            }
        }
        self.validate_references()?;
        Ok(())
    }

    fn validate_references(&self) -> Result<(), String> {
        let order = self.topological_order()?;
        let properties = self.params.get("properties").and_then(Value::as_object);
        let mut ancestors: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for index in order {
            let Some(step) = self.steps.get(index) else {
                return Err("invalid workflow step index".into());
            };
            let mut inherited = BTreeSet::new();
            for dependency in &step.depends_on {
                inherited.insert(dependency.as_str());
                if let Some(previous) = ancestors.get(dependency.as_str()) {
                    inherited.extend(previous.iter().copied());
                }
            }
            let references = match &step.kind {
                StepKind::Tool { arguments, .. } => template::references_value(arguments)?,
                StepKind::Agent { prompt, .. } => template::references(prompt)?,
                StepKind::Job { title, description } => {
                    let mut refs = template::references(title)?;
                    refs.extend(template::references(description)?);
                    refs
                }
            };
            for reference in references {
                match reference {
                    template::TemplateReference::Param(name) => {
                        if !properties.is_some_and(|properties| properties.contains_key(&name)) {
                            return Err(format!("step `{}` references undeclared parameter `{name}`", step.id));
                        }
                    }
                    template::TemplateReference::StepResult(name) => {
                        if !inherited.contains(name.as_str()) {
                            return Err(format!("step `{}` references result `{name}` without a dependency", step.id));
                        }
                    }
                }
            }
            ancestors.insert(&step.id, inherited);
        }
        Ok(())
    }

    /// Returns step indices in deterministic dependency order, resolving declaration-order ties.
    ///
    /// # Errors
    /// Returns a clear error for unknown, repeated, self, or cyclic dependencies.
    pub fn topological_order(&self) -> Result<Vec<usize>, String> {
        let mut by_id = BTreeMap::new();
        for (index, step) in self.steps.iter().enumerate() {
            if by_id.insert(step.id.as_str(), index).is_some() {
                return Err(format!("duplicate step id `{}`", step.id));
            }
        }
        let mut incoming = vec![0usize; self.steps.len()];
        let mut outgoing = vec![Vec::<usize>::new(); self.steps.len()];
        for (index, step) in self.steps.iter().enumerate() {
            let mut seen = BTreeSet::new();
            for dependency in &step.depends_on {
                let Some(&parent) = by_id.get(dependency.as_str()) else {
                    return Err(format!("step `{}` depends on unknown step `{dependency}`", step.id));
                };
                if parent == index || !seen.insert(parent) {
                    return Err(format!("step `{}` has a self or repeated dependency", step.id));
                }
                let Some(count) = incoming.get_mut(index) else { return Err("invalid workflow step index".into()) };
                *count += 1;
                if let Some(children) = outgoing.get_mut(parent) {
                    children.push(index);
                }
            }
        }
        let mut order = Vec::with_capacity(self.steps.len());
        let mut ready: BTreeSet<_> = incoming.iter().enumerate().filter_map(|(index, count)| (*count == 0).then_some(index)).collect();
        while let Some(index) = ready.pop_first() {
            order.push(index);
            if let Some(children) = outgoing.get(index) {
                for &child in children {
                    if let Some(count) = incoming.get_mut(child) {
                        *count -= 1;
                        if *count == 0 {
                            ready.insert(child);
                        }
                    }
                }
            }
        }
        if order.len() != self.steps.len() {
            return Err("workflow step dependencies contain a cycle".into());
        }
        Ok(order)
    }

    /// Checks invocation parameters against the supported JSON Schema subset.
    ///
    /// # Errors
    /// Returns a field-specific type or required-property error without values.
    pub fn validate_params(&self, params: &Value) -> Result<(), String> {
        validate_value(&self.params, params, 0)
    }

    /// Checks a completed workflow result against the supported JSON Schema subset.
    ///
    /// # Errors
    /// Returns a field-specific type or required-property error without values.
    pub fn validate_result(&self, result: &Value) -> Result<(), String> {
        validate_value(&self.results, result, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VALID: &str = r#"
name = "sample"
version = "1"
trigger = "manual"
params = '{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}'
results = '{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}'
[ceiling]
roots = ["."]
ops = ["read", "write"]
[budget]
max_steps = 3
max_tokens = 1000
timeout_seconds = 120
[[steps]]
id = "verify"
kind = "tool"
tool = "fs.read"
arguments = '{"path":"${params.path}"}'
depends_on = ["build"]
retries = 1
timeout_seconds = 30
[[steps]]
id = "build"
kind = "agent"
agent = "builder"
prompt = "Build ${params.path}"
retries = 0
"#;

    #[test]
    fn parse_orders_forward_dependencies_and_validates_types() {
        let manifest = WorkflowManifest::parse(VALID).unwrap();
        assert_eq!(manifest.topological_order().unwrap(), vec![1, 0]);
        assert!(manifest.validate_params(&json!({"path":"x"})).is_ok());
        assert!(manifest.validate_params(&json!({"path":2})).unwrap_err().contains("expected string"));
        assert!(manifest.validate_result(&json!({"ok":true})).is_ok());
    }

    #[test]
    fn rejects_bad_dependencies_and_schema() {
        let unknown = VALID.replace("[\"build\"]", "[\"missing\"]");
        assert!(WorkflowManifest::parse(&unknown).err().unwrap().contains("unknown step"));
        let cycle = VALID.replace("prompt = \"Build ${params.path}\"", "prompt = \"Build ${params.path}\"\ndepends_on = [\"verify\"]");
        assert!(WorkflowManifest::parse(&cycle).err().unwrap().contains("cycle"));
        let bad_schema = VALID.replace("{\"type\":\"boolean\"}", "{\"type\":\"unknown\"}");
        assert!(WorkflowManifest::parse(&bad_schema).is_err());
    }

    #[test]
    fn rejects_oversized_and_invalid_steps() {
        assert!(WorkflowManifest::parse(&"x".repeat(MAX_MANIFEST_BYTES + 1)).is_err());
        let repeated = VALID.replace("id = \"build\"", "id = \"verify\"");
        assert!(WorkflowManifest::parse(&repeated).err().unwrap().contains("duplicate"));
        let unknown_field = VALID.replace("tool = \"fs.read\"", "tool = \"fs.read\"\nunknown = 1");
        assert!(WorkflowManifest::parse(&unknown_field).err().unwrap().contains("unknown field"));
        let unbound = VALID
            .replace("arguments = '{\"path\":\"${params.path}\"}'", "arguments = '{\"path\":\"${steps.build.result}\"}'")
            .replace("[\"build\"]", "[]");
        assert!(WorkflowManifest::parse(&unbound).err().unwrap().contains("without a dependency"));
        let unknown_param = VALID.replace("${params.path}", "${params.token}");
        assert!(WorkflowManifest::parse(&unknown_param).err().unwrap().contains("undeclared parameter"));
    }
}
