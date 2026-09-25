//! Agent tools for saving, listing, and running trusted Git-backed programs.

use std::collections::HashMap;
use std::sync::Arc;

use aim_coderun::protocol::Execute;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::CodeToolHost;
use super::supervisor::Supervisor;
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;
use crate::programs::{ProgramError, ProgramManifest, ProgramScope, ProgramStore};

/// Code mode plus the three saved-program tools for one session.
#[derive(Clone)]
pub struct ProgramToolHost {
    code: CodeToolHost,
    programs: Arc<ProgramStore>,
}

impl ProgramToolHost {
    /// Attach a Git-backed store to a session's code tool host.
    #[must_use]
    pub fn new(code: CodeToolHost, programs: Arc<ProgramStore>) -> Self {
        Self { code, programs }
    }

    async fn save(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let mut args: SaveArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid save_program arguments"))?;
        let offered = self.code.shared.inner.specs();
        if args.manifest.grants.tools.iter().any(|name| !offered.iter().any(|spec| &spec.name == name)) {
            return Err(ProtoError::new(ErrorCode::Denied, "program grants exceed current tools"));
        }
        args.manifest.provenance.session_id.clone_from(&self.code.shared.session_id);
        args.manifest.provenance.turn = self.code.shared.turn;
        args.manifest.tools = aim_coderun::runtime::referenced_tools(&args.source)?;
        let programs = Arc::clone(&self.programs);
        let saved = tokio::task::spawn_blocking(move || {
            args.manifest.id =
                programs.load(args.scope, &args.slug).map_or_else(|_| Uuid::now_v7().to_string(), |previous| previous.manifest.id);
            programs.save(args.scope, &args.slug, &args.manifest, &args.source)
        })
        .await
        .map_err(|_| unavailable())?
        .map_err(program_error)?;
        Ok(ToolResult::text(json!({"slug":saved.slug,"scope":saved.scope,"sha256":saved.sha256}).to_string()))
    }

    async fn list(&self) -> Result<ToolResult, ProtoError> {
        let programs = Arc::clone(&self.programs);
        let saved = tokio::task::spawn_blocking(move || programs.list()).await.map_err(|_| unavailable())?.map_err(program_error)?;
        let descriptions: Vec<Value> = saved
            .iter()
            .map(|program| {
                json!({"slug":program.slug,"scope":program.scope,"name":program.manifest.name,
                    "description":program.manifest.description,"version":program.manifest.version,
                    "trusted":program.trusted,"sha256":program.sha256})
            })
            .collect();
        Ok(ToolResult::text(Value::Array(descriptions).to_string()))
    }

    async fn run(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let args: RunArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid run_program arguments"))?;
        let programs = Arc::clone(&self.programs);
        let saved = tokio::task::spawn_blocking(move || programs.load(args.scope, &args.slug))
            .await
            .map_err(|_| unavailable())?
            .map_err(program_error)?;
        if !saved.trusted {
            return Err(ProtoError::new(ErrorCode::Denied, "program content hash is not trusted"));
        }
        validate_schema(&saved.manifest.params, &args.params, 0)?;
        let narrowed: Arc<dyn ToolHost> = Arc::new(saved.narrow_host(Arc::clone(&self.code.shared.inner)));
        let supervisor = Supervisor::new(self.code.shared.executable.clone(), Arc::clone(&narrowed));
        let request = Execute {
            session_id: self.code.shared.session_id.clone(),
            cell_id: Uuid::now_v7().to_string(),
            code: saved.source,
            timeout_ms: 120_000,
            memory_limit_bytes: 64 * 1024 * 1024,
            output_limit_bytes: 40_000,
            tools: narrowed.specs(),
            store: HashMap::new(),
            program_args: Some(args.params),
        };
        let (sender, _receiver) = mpsc::unbounded_channel();
        let result = supervisor.execute(request, sender).await?;
        let returned = serde_json::from_str(&result.output).unwrap_or_else(|_| Value::String(result.output.clone()));
        validate_schema(&saved.manifest.returns, &returned, 0)?;
        Ok(ToolResult::text(result.output))
    }
}

impl ToolHost for ProgramToolHost {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.code.specs();
        specs.push(spec("save_program", "Save a typed JS/TS program in the user or project Git repository. Source and the program manifest are committed locally; no remote sync occurs.", json!({"type":"object","properties":{"scope":{"enum":["user","project"]},"slug":{"type":"string"},"manifest":{"type":"object"},"source":{"type":"string"}},"required":["scope","slug","manifest","source"]})));
        specs.push(spec("run_program", "Run a trusted saved program with its recorded grants intersected with current session tools.", json!({"type":"object","properties":{"scope":{"enum":["user","project"]},"slug":{"type":"string"},"params":{}},"required":["scope","slug","params"]})));
        specs.push(spec(
            "list_programs",
            "List saved programs and their trust state in user and project repositories.",
            json!({"type":"object","properties":{}}),
        ));
        specs
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let host = self.clone();
        Box::pin(async move {
            match name.as_str() {
                "save_program" => host.save(arguments).await,
                "run_program" => host.run(arguments).await,
                "list_programs" => host.list().await,
                _ => host.code.call(name, arguments, key).await,
            }
        })
    }
}

fn spec(name: &str, description: &str, schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: schema,
        input: ToolInput::Json,
        annotations: ToolAnnotations { location: ToolLocation::LocalService, ..ToolAnnotations::default() },
    }
}

#[derive(Deserialize)]
struct SaveArgs {
    scope: ProgramScope,
    slug: String,
    manifest: ProgramManifest,
    source: String,
}

#[derive(Deserialize)]
struct RunArgs {
    scope: ProgramScope,
    slug: String,
    params: Value,
}

fn validate_schema(schema: &Value, value: &Value, depth: usize) -> Result<(), ProtoError> {
    if depth > 8 {
        return Err(invalid("program schema exceeds supported nesting"));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(invalid("program value is outside schema enum"));
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let matches = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => return Err(invalid("unsupported program schema type")),
        };
        if !matches {
            return Err(invalid("program value does not match schema type"));
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required {
                let name = field.as_str().ok_or_else(|| invalid("invalid required field"))?;
                if !object.contains_key(name) {
                    return Err(invalid("program argument is missing a required field"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property) in properties {
                if let Some(child) = object.get(name) {
                    validate_schema(property, child, depth + 1)?;
                }
            }
        }
    }
    if let Some(items) = schema.get("items")
        && let Some(values) = value.as_array()
    {
        for child in values {
            validate_schema(items, child, depth + 1)?;
        }
    }
    Ok(())
}

fn program_error(error: ProgramError) -> ProtoError {
    match error {
        ProgramError::Invalid(message) => ProtoError::new(ErrorCode::InvalidParams, message),
        ProgramError::Missing(_) => ProtoError::new(ErrorCode::NotFound, "program not found"),
        _ => unavailable(),
    }
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "program store is unavailable")
}

fn invalid(message: &str) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}
