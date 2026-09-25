//! `ui_show`, `ui_update`, `ui_close` and `ui_catalog` (ADR 0064): the agent's side of the UI
//! protocol. Descriptions are compact — the argument shape and the component names — and details
//! come from `ui_catalog` or from a refusal, which names the offending component's props.

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use aim_proto::ui::catalog::TERMINAL;
use aim_proto::ui::{Component, DataOp, Placement, ROOT_ID, TERMINAL_CATALOG, UiMessage};
use serde_json::{Map, Value, json};

use super::session::{current, submit};
use super::validate::Notes;
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

/// Tool names.
pub const SHOW: &str = "ui_show";
/// See [`SHOW`].
pub const UPDATE: &str = "ui_update";
/// See [`SHOW`].
pub const CLOSE: &str = "ui_close";
/// See [`SHOW`].
pub const CATALOG: &str = "ui_catalog";

fn spec(name: &str, description: String) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description,
        // The host validates arguments itself (and says exactly what is wrong), so the schema
        // stays minimal: every byte here is paid on every request.
        input_schema: json!({"type": "object"}),
        input: ToolInput::Json,
        annotations: ToolAnnotations { location: ToolLocation::LocalService, ..ToolAnnotations::default() },
    }
}

/// The four tools as offered to a model.
#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            SHOW,
            format!(
                "Show UI {{surface, components:[{{id,component,...props}}], data?, placement?:transcript|widget|dialog|toast}}; \
                 {{\"path\":\"/p\"}} binds data. Components: {}. Props: ui_catalog.",
                TERMINAL.names()
            ),
        ),
        spec(UPDATE, "Update UI {surface, components?, data?:[{path,value}]}.".to_owned()),
        spec(CLOSE, "Close UI {surface}.".to_owned()),
        spec(CATALOG, "UI component props {component?}.".to_owned()),
    ]
}

fn invalid(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}

fn surface_arg(arguments: &Value) -> Result<String, ProtoError> {
    ["surface", "surface_id", "id"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| invalid("`surface` (the surface id) is required"))
}

fn components_arg(arguments: &Value, required: bool) -> Result<Vec<Component>, ProtoError> {
    match arguments.get("components") {
        None | Some(Value::Null) if !required => Ok(Vec::new()),
        None | Some(Value::Null) => Err(invalid("`components` is required: [{id, component, ...props}], one with id \"root\"")),
        Some(Value::Array(list)) => {
            let mut flat = Vec::new();
            for (index, value) in list.iter().enumerate() {
                flatten(value.clone(), &format!("c{index}"), &mut flat)
                    .map_err(|e| invalid(format!("components[{index}] is not {{id, component, ...props}}: {e}")))?;
            }
            Ok(flat)
        }
        Some(_) => Err(invalid("`components` must be an array")),
    }
}

/// Forgiving at the model boundary: a component nested inline as a `child` or in `children` is
/// moved into the flat list (with an id made from its parent's when it has none) and referenced
/// by id, as the protocol wants.
fn flatten(mut value: Value, fallback_id: &str, out: &mut Vec<Component>) -> Result<String, serde_json::Error> {
    if let Value::Object(map) = &mut value {
        let id = if let Some(id) = map.get("id").and_then(Value::as_str) {
            id.to_owned()
        } else {
            map.insert("id".into(), Value::String(fallback_id.to_owned()));
            fallback_id.to_owned()
        };
        if let Some(child) = map.get_mut("child")
            && child.is_object()
        {
            let nested = flatten(child.take(), &format!("{id}.0"), out)?;
            *child = Value::String(nested);
        }
        if let Some(Value::Array(children)) = map.get_mut("children") {
            for (index, child) in children.iter_mut().enumerate() {
                if child.is_object() {
                    let nested = flatten(child.take(), &format!("{id}.{index}"), out)?;
                    *child = Value::String(nested);
                }
            }
        }
    }
    let component: Component = serde_json::from_value(value)?;
    let id = component.id.clone();
    out.push(component);
    Ok(id)
}

/// `data` as ops: an array of `{path, value}`, or an object whose members are set one by one.
fn ops_arg(arguments: &Value) -> Result<Vec<DataOp>, ProtoError> {
    match arguments.get("data") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => list
            .iter()
            .enumerate()
            .map(|(index, op)| {
                serde_json::from_value::<DataOp>(op.clone()).map_err(|e| invalid(format!("data[{index}] is not {{path, value}}: {e}")))
            })
            .collect(),
        Some(Value::Object(members)) => Ok(members
            .iter()
            .map(|(key, value)| DataOp { path: format!("/{}", key.replace('~', "~0").replace('/', "~1")), value: value.clone() })
            .collect()),
        Some(_) => Err(invalid("`data` must be [{path, value}] or an object")),
    }
}

/// Forgiving at the model boundary: when no component is called `root`, the first component no
/// other one contains becomes the root (renaming it needs no reference rewrite).
fn ensure_root(components: &mut [Component]) {
    if components.iter().any(|c| c.id == ROOT_ID) {
        return;
    }
    let contained: Vec<String> = components.iter().flat_map(|c| c.child_ids().into_iter().map(str::to_owned)).collect();
    if let Some(top) = components.iter_mut().find(|c| !contained.contains(&c.id)) {
        ROOT_ID.clone_into(&mut top.id);
    }
}

fn summary(verb: &str, surface: &str, notes: &Notes) -> ToolResult {
    let text = if notes.missing.is_empty() {
        format!("{verb} `{surface}`")
    } else {
        format!("{verb} `{surface}` (not defined yet, shown as placeholders: {})", notes.missing.join(", "))
    };
    ToolResult::text(text)
}

fn show(arguments: &Value) -> Result<ToolResult, ProtoError> {
    let surface_id = surface_arg(arguments)?;
    let placement = match arguments.get("placement").and_then(Value::as_str) {
        Some(name) => Placement::parse(name)
            .ok_or_else(|| invalid(format!("unknown placement `{name}` (one of {}, tool(<call_id>))", Placement::NAMES.join(", "))))?,
        None => Placement::default(),
    };
    let mut components = components_arg(arguments, true)?;
    ensure_root(&mut components);
    let data = match arguments.get("data") {
        None | Some(Value::Null) => None,
        Some(Value::Object(members)) => Some(Value::Object(members.clone())),
        Some(_) => return Err(invalid("`data` must be an object (the surface's initial data model)")),
    };
    // Showing an existing id replaces that surface.
    let replaced = current(&surface_id).is_some();
    if replaced {
        submit(UiMessage::DeleteSurface { surface_id: surface_id.clone() })?;
    }
    let (_, notes) = submit(UiMessage::CreateSurface {
        surface_id: surface_id.clone(),
        catalog_id: TERMINAL_CATALOG.to_owned(),
        placement: placement.clone(),
        components,
        data,
    })?;
    let verb = if replaced { "replaced" } else { "showing" };
    Ok(summary(&format!("{verb} ({placement})"), &surface_id, &notes))
}

fn update(arguments: &Value) -> Result<ToolResult, ProtoError> {
    let surface_id = surface_arg(arguments)?;
    let components = components_arg(arguments, false)?;
    let ops = ops_arg(arguments)?;
    if components.is_empty() && ops.is_empty() {
        return Err(invalid("nothing to update: give `components` and/or `data`"));
    }
    let mut notes = Notes::default();
    if !components.is_empty() {
        notes = submit(UiMessage::UpdateComponents { surface_id: surface_id.clone(), components })?.1;
    }
    if !ops.is_empty() {
        notes = submit(UiMessage::UpdateDataModel { surface_id: surface_id.clone(), ops })?.1;
    }
    Ok(summary("updated", &surface_id, &notes))
}

fn close(arguments: &Value) -> Result<ToolResult, ProtoError> {
    let surface_id = surface_arg(arguments)?;
    submit(UiMessage::DeleteSurface { surface_id: surface_id.clone() })?;
    Ok(ToolResult::text(format!("closed `{surface_id}`")))
}

/// `ui_catalog`: one component in detail, or all of them.
fn catalog(arguments: &Value) -> ToolResult {
    let common = "Every component: {id, component, fallback?: text | {child: id}}; a prop {\"path\": \"/pointer\"} binds to the \
                  surface's data (ui_update data ops change it). A component outside the catalog needs a fallback.";
    if let Some(name) = arguments.get("component").and_then(Value::as_str) {
        return match TERMINAL.component(name) {
            Some(def) => ToolResult::text(format!("{}\n{common}", def.describe())),
            None => ToolResult::error(format!("no component `{name}` in {TERMINAL_CATALOG}: {}", TERMINAL.names())),
        };
    }
    let mut text = format!("{TERMINAL_CATALOG}. {common}\nPlacements: {}, tool(<call_id>).", Placement::NAMES.join(", "));
    for def in TERMINAL.components {
        text.push('\n');
        text.push_str(&def.describe());
    }
    ToolResult::text(text)
}

/// The UI tools (stateless: each call reaches its session's surfaces through the turn's outlet).
#[derive(Clone, Copy, Debug, Default)]
pub struct UiTools;

impl ToolHost for UiTools {
    fn specs(&self) -> Vec<ToolSpec> {
        specs()
    }

    fn call(&self, name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async move {
            let arguments = if arguments.is_object() { arguments } else { Value::Object(Map::new()) };
            match name.as_str() {
                SHOW => show(&arguments),
                UPDATE => update(&arguments),
                CLOSE => close(&arguments),
                CATALOG => Ok(catalog(&arguments)),
                other => Err(ProtoError::new(ErrorCode::NotFound, format!("no tool named `{other}`"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aim_proto::daemon::SessionUpdate;

    use super::*;
    use crate::ui::session::{SessionUi, scope};

    fn key() -> IdempotencyKey {
        IdempotencyKey::new("k".to_owned())
    }

    async fn run(ui: &Arc<SessionUi>, name: &str, arguments: Value) -> (Result<ToolResult, ProtoError>, Vec<SessionUpdate>) {
        let (events, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let result = scope(Arc::clone(ui), events, UiTools.call(name.to_owned(), arguments, key())).await;
        let mut sent = Vec::new();
        while let Ok(update) = updates.try_recv() {
            sent.push(update);
        }
        (result, sent)
    }

    fn text(result: &ToolResult) -> String {
        serde_json::to_string(&result.content).unwrap()
    }

    #[tokio::test]
    async fn show_update_close_emit_validated_messages() {
        let ui = Arc::new(SessionUi::new("A"));
        let table = json!({"surface": "t", "components": [{"id": "tbl", "component": "Table", "columns": ["a"], "rows": {"path": "/rows"}}], "data": {"rows": [["1"]]}});
        let (shown, sent) = run(&ui, SHOW, table).await;
        assert!(text(&shown.unwrap()).contains("showing (transcript) `t`"));
        assert_eq!(sent.len(), 1);
        assert_eq!(
            ui.accepted("t").and_then(|s| s.root().map(|c| c.component.clone())),
            Some("Table".into()),
            "the lone component became root"
        );
        let (updated, sent) = run(&ui, UPDATE, json!({"surface": "t", "data": [{"path": "/rows/-", "value": ["2"]}]})).await;
        assert!(updated.is_ok() && sent.len() == 1);
        let (merged, _) = run(&ui, UPDATE, json!({"surface": "t", "data": {"extra": 1}})).await;
        assert!(merged.is_ok());
        assert_eq!(ui.accepted("t").map(|s| s.data), Some(json!({"rows": [["1"], ["2"]], "extra": 1})));
        let (replaced, sent) =
            run(&ui, SHOW, json!({"surface": "t", "placement": "dialog", "components": [{"id": "root", "component": "Spinner"}]})).await;
        assert!(text(&replaced.unwrap()).contains("replaced (dialog)"));
        assert_eq!(sent.len(), 2, "a delete, then the new surface");
        let (closed, _) = run(&ui, CLOSE, json!({"surface": "t"})).await;
        assert!(closed.is_ok() && ui.accepted("t").is_none());
        let (again, sent) = run(&ui, CLOSE, json!({"surface": "t"})).await;
        assert_eq!(again.unwrap_err().code, ErrorCode::NotFound);
        assert!(sent.is_empty(), "a refused message is never published");
    }

    #[tokio::test]
    async fn nested_children_are_flattened_and_a_lone_top_becomes_root() {
        let ui = Arc::new(SessionUi::new("A"));
        let nested = json!({"surface": "n", "components": [{"component": "Column", "children": [
            {"component": "Text", "text": "a"},
            {"id": "b", "component": "Box", "child": {"component": "Badge", "text": "ok"}}
        ]}]});
        let (shown, _) = run(&ui, SHOW, nested).await;
        assert!(shown.is_ok(), "{shown:?}");
        let surface = ui.accepted("n").unwrap();
        let ids: Vec<&str> = surface.components.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c0.0", "b.0", "b", "root"]);
        assert_eq!(surface.root().unwrap().prop("children"), Some(&json!(["c0.0", "b"])));
    }

    #[tokio::test]
    async fn refusals_explain_the_component() {
        let ui = Arc::new(SessionUi::new("A"));
        let (bad, _) =
            run(&ui, SHOW, json!({"surface": "p", "components": [{"id": "root", "component": "Progress", "value": "half"}]})).await;
        let message = bad.unwrap_err().message;
        assert!(message.contains("Progress: a progress bar") && message.contains("max?: number"), "{message}");
        let (placement, _) = run(&ui, SHOW, json!({"surface": "p", "placement": "sidebar", "components": []})).await;
        assert!(placement.unwrap_err().message.contains("unknown placement `sidebar`"));
        let (catalog, _) = run(&ui, CATALOG, json!({"component": "Button"})).await;
        assert!(text(&catalog.unwrap()).contains("<ui_action>"));
        let (all, _) = run(&ui, CATALOG, json!({})).await;
        assert!(text(&all.unwrap()).contains("KeyValue"));
    }

    /// The bytes the tools add to a first request, in each provider's wire form (the budget is
    /// 800 bytes, ADR 0064).
    #[test]
    fn the_tools_add_under_800_bytes_to_a_request() {
        let openrouter = aim_llm_openai::OpenAiProvider::new(aim_llm_openai::Profile::openrouter()).unwrap();
        let request = |tools: Vec<ToolSpec>| aim_llm::Request {
            model: "anthropic/claude-sonnet-5".into(),
            instructions: String::new(),
            items: Vec::new(),
            tools,
            effort: None,
            tier: None,
            cache_key: None,
            session_id: None,
            turn_id: None,
            parallel_tool_calls: true,
            max_output_tokens: None,
        };
        let base = vec![spec("read_file", "Read a file.".into())];
        let size = |tools: Vec<ToolSpec>| serde_json::to_vec(&openrouter.request_body(&request(tools)).unwrap()).unwrap().len();
        let mut with = base.clone();
        with.extend(specs());
        let added = size(with) - size(base);
        let codex: usize = specs()
            .iter()
            .map(|s| {
                let tool = json!({"type": "function", "name": s.name, "description": s.description, "parameters": s.input_schema, "strict": false});
                serde_json::to_vec(&tool).unwrap().len() + 1
            })
            .sum();
        println!("ui tools add {added} bytes (openrouter chat completions), {codex} bytes (codex responses)");
        assert!(added < 800, "{added} bytes");
        assert!(codex < 800, "{codex} bytes");
    }
}
