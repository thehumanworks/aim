//! `ui_show`, `ui_update`, `ui_close` and `ui_catalog` (ADR 0064): the agent's side of the UI
//! protocol. Descriptions are compact — the argument shape and the component names — and details
//! come from `ui_catalog` or from a refusal, which names the offending component's props.

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use aim_proto::ui::catalog::TERMINAL;
use aim_proto::ui::{Component, DataOp, Placement, ROOT_ID, TERMINAL_CATALOG, UiMessage};
use serde_json::{Map, Value, json};

use std::collections::HashSet;

use super::limits::Limits;
use super::session::{current, limits, submit};
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

fn too_big(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::LimitExceeded, message)
}

/// The raw arguments within the session's bounds before any lenient work (flattening, root
/// detection) is spent on them: their size, and the number of components they list.
fn check_raw(arguments: &Value, limits: &Limits) -> Result<(), ProtoError> {
    let size = serde_json::to_vec(arguments).map_or(usize::MAX, |bytes| bytes.len());
    if size > limits.message_bytes {
        return Err(too_big(format!("the arguments are {size} bytes; the limit is {}", limits.message_bytes)));
    }
    if let Some(Value::Array(list)) = arguments.get("components")
        && list.len() > limits.components
    {
        return Err(too_big(format!("`components` lists {} components; a surface holds at most {}", list.len(), limits.components)));
    }
    Ok(())
}

fn components_arg(arguments: &Value, required: bool, limits: &Limits) -> Result<Vec<Component>, ProtoError> {
    match arguments.get("components") {
        None | Some(Value::Null) if !required => Ok(Vec::new()),
        None | Some(Value::Null) => Err(invalid("`components` is required: [{id, component, ...props}], one with id \"root\"")),
        Some(Value::Array(list)) => {
            let mut flat = Vec::new();
            for (index, value) in list.iter().enumerate() {
                flatten(value.clone(), &format!("c{index}"), 1, limits, &mut flat).map_err(|e| match e {
                    Flatten::Limit(error) => error,
                    Flatten::Shape(e) => invalid(format!("components[{index}] is not {{id, component, ...props}}: {e}")),
                })?;
            }
            Ok(flat)
        }
        Some(_) => Err(invalid("`components` must be an array")),
    }
}

/// Why flattening stopped.
enum Flatten {
    /// A bound was reached (components or nesting).
    Limit(ProtoError),
    /// A component is not `{id, component, ...props}`.
    Shape(serde_json::Error),
}

/// Forgiving at the model boundary: a component nested inline as a `child` or in `children` is
/// moved into the flat list (with an id made from its parent's when it has none) and referenced
/// by id, as the protocol wants. Bounded by the session's component count and tree depth.
fn flatten(mut value: Value, fallback_id: &str, depth: usize, limits: &Limits, out: &mut Vec<Component>) -> Result<String, Flatten> {
    if depth > limits.depth {
        return Err(Flatten::Limit(too_big(format!("components are nested deeper than {} levels", limits.depth))));
    }
    if out.len() >= limits.components {
        return Err(Flatten::Limit(too_big(format!("more than {} components; a surface holds at most that many", limits.components))));
    }
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
            let nested = flatten(child.take(), &format!("{id}.0"), depth + 1, limits, out)?;
            *child = Value::String(nested);
        }
        if let Some(Value::Array(children)) = map.get_mut("children") {
            for (index, child) in children.iter_mut().enumerate() {
                if child.is_object() {
                    let nested = flatten(child.take(), &format!("{id}.{index}"), depth + 1, limits, out)?;
                    *child = Value::String(nested);
                }
            }
        }
    }
    let component: Component = serde_json::from_value(value).map_err(Flatten::Shape)?;
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

/// Forgiving at the model boundary: when no component is called `root`, a lone top-level
/// component (one no other contains) becomes the root — renaming it needs no reference rewrite —
/// and several top-level components are stacked under a new root `Column`, in order.
fn ensure_root(components: &mut Vec<Component>) {
    if components.iter().any(|c| c.id == ROOT_ID) {
        return;
    }
    let contained: HashSet<&str> = components.iter().flat_map(Component::child_ids).collect();
    let tops: Vec<String> = components.iter().filter(|c| !contained.contains(c.id.as_str())).map(|c| c.id.clone()).collect();
    match tops.as_slice() {
        [] => {}
        [single] => {
            if let Some(top) = components.iter_mut().find(|c| c.id == *single) {
                ROOT_ID.clone_into(&mut top.id);
            }
        }
        several => components.push(Component::new(ROOT_ID, "Column").with("children", json!(several))),
    }
}

/// A surface id for a `ui_show` that gave none: `ui1`, `ui2`, … (the first that is free).
fn fresh_surface_id() -> String {
    // A session holds at most a few dozen surfaces, so one of the first thousand ids is free.
    (1_u32..=1_000).map(|n| format!("ui{n}")).find(|id| current(id).is_none()).unwrap_or_else(|| "ui".to_owned())
}

fn summary(verb: &str, surface: &str, notes: &Notes) -> ToolResult {
    let text = if notes.missing.is_empty() {
        format!("{verb} `{surface}`")
    } else {
        format!("{verb} `{surface}` (not defined yet, shown as placeholders: {})", notes.missing.join(", "))
    };
    ToolResult::text(text)
}

fn show(arguments: &Value, limits: &Limits) -> Result<ToolResult, ProtoError> {
    check_raw(arguments, limits)?;
    // A new surface needs no id from the model: the result names the one it got.
    let surface_id = surface_arg(arguments).unwrap_or_else(|_| fresh_surface_id());
    let placement = match arguments.get("placement").and_then(Value::as_str) {
        Some(name) => Placement::parse(name)
            .ok_or_else(|| invalid(format!("unknown placement `{name}` (one of {}, tool(<call_id>))", Placement::NAMES.join(", "))))?,
        None => Placement::default(),
    };
    let mut components = components_arg(arguments, true, limits)?;
    ensure_root(&mut components);
    let data = match arguments.get("data") {
        None | Some(Value::Null) => None,
        Some(Value::Object(members)) => Some(Value::Object(members.clone())),
        Some(_) => return Err(invalid("`data` must be an object (the surface's initial data model)")),
    };
    // Showing an existing id replaces that surface, in one message: a refused replacement leaves
    // the old surface as it was.
    let replaced = current(&surface_id).is_some();
    let (_, notes) = submit(UiMessage::CreateSurface {
        surface_id: surface_id.clone(),
        replace: replaced,
        catalog_id: TERMINAL_CATALOG.to_owned(),
        placement: placement.clone(),
        components,
        data,
    })?;
    let verb = if replaced { "replaced" } else { "showing" };
    Ok(summary(&format!("{verb} ({placement})"), &surface_id, &notes))
}

fn update(arguments: &Value, limits: &Limits) -> Result<ToolResult, ProtoError> {
    check_raw(arguments, limits)?;
    let surface_id = surface_arg(arguments)?;
    let components = components_arg(arguments, false, limits)?;
    let ops = ops_arg(arguments)?;
    if components.is_empty() && ops.is_empty() {
        return Err(invalid("nothing to update: give `components` and/or `data`"));
    }
    // Components and data are one message: all of it applies, or none.
    let message = if components.is_empty() {
        UiMessage::UpdateDataModel { surface_id: surface_id.clone(), ops }
    } else {
        UiMessage::UpdateComponents { surface_id: surface_id.clone(), components, ops }
    };
    let (_, notes) = submit(message)?;
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
                SHOW => show(&arguments, &limits()),
                UPDATE => update(&arguments, &limits()),
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
        assert_eq!(sent.len(), 1, "one replacing message");
        let (closed, _) = run(&ui, CLOSE, json!({"surface": "t"})).await;
        assert!(closed.is_ok() && ui.accepted("t").is_none());
        let (again, sent) = run(&ui, CLOSE, json!({"surface": "t"})).await;
        assert_eq!(again.unwrap_err().code, ErrorCode::NotFound);
        assert!(sent.is_empty(), "a refused message is never published");
    }

    fn ui_messages(sent: &[SessionUpdate]) -> Vec<UiMessage> {
        sent.iter()
            .filter_map(|u| match u {
                SessionUpdate::Ui { message } => Some(message.message.clone()),
                _ => None,
            })
            .collect()
    }

    /// REV19 A1: a refused replacement leaves the shown surface as it was and publishes nothing.
    #[tokio::test]
    async fn rev19_a_refused_replacement_keeps_the_surface() {
        let ui = Arc::new(SessionUi::new("A"));
        let good = json!({"surface": "s", "components": [{"id": "root", "component": "Text", "text": "kept"}]});
        assert!(run(&ui, SHOW, good).await.0.is_ok());
        let bad = json!({"surface": "s", "components": [{"id": "root", "component": "Text"}]});
        let (refused, sent) = run(&ui, SHOW, bad).await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::InvalidParams);
        assert!(sent.is_empty(), "nothing was published: {sent:?}");
        assert_eq!(ui.accepted("s").and_then(|s| s.root().and_then(|c| c.prop("text").cloned())), Some(json!("kept")));
        // With one token left, a valid replacement is one message and succeeds.
        let limited = Arc::new(SessionUi::with_limits("A", Limits { burst: 2, per_second: 0, ..Limits::default() }));
        assert!(run(&limited, SHOW, json!({"surface": "s", "components": [{"id": "root", "component": "Divider"}]})).await.0.is_ok());
        let (replaced, sent) = run(&limited, SHOW, json!({"surface": "s", "components": [{"id": "root", "component": "Spinner"}]})).await;
        assert!(replaced.is_ok(), "{replaced:?}");
        assert!(matches!(ui_messages(&sent).as_slice(), [UiMessage::CreateSurface { replace: true, .. }]));
    }

    /// REV19 A2: components and data are one transaction: a bad data op publishes nothing and
    /// leaves the components as they were.
    #[tokio::test]
    async fn rev19_an_update_with_components_and_data_is_all_or_nothing() {
        let ui = Arc::new(SessionUi::new("A"));
        assert!(
            run(&ui, SHOW, json!({"surface": "s", "components": [{"id": "root", "component": "Text", "text": "old"}]})).await.0.is_ok()
        );
        let mixed = json!({"surface": "s", "components": [{"id": "root", "component": "Text", "text": "new"}], "data": [{"path": "bad", "value": 1}]});
        let (refused, sent) = run(&ui, UPDATE, mixed).await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::InvalidParams);
        assert!(sent.is_empty(), "nothing was published: {sent:?}");
        assert_eq!(ui.accepted("s").and_then(|s| s.root().and_then(|c| c.prop("text").cloned())), Some(json!("old")));
        let good =
            json!({"surface": "s", "components": [{"id": "root", "component": "Text", "text": {"path": "/t"}}], "data": {"t": "new"}});
        let (updated, sent) = run(&ui, UPDATE, good).await;
        assert!(updated.is_ok());
        assert!(matches!(ui_messages(&sent).as_slice(), [UiMessage::UpdateComponents { ops, .. }] if ops.len() == 1), "one message");
    }

    /// REV19 A4: raw arguments are bounded before any lenient work: their bytes (whatever they
    /// hold), how many components they list, and how deep inline components nest.
    #[tokio::test]
    async fn rev19_raw_arguments_are_bounded_before_flattening() {
        let ui = Arc::new(SessionUi::new("A"));
        let padded = json!({"surface": "s", "components": [{"id": "root", "component": "Divider"}], "note": "x".repeat(70_000)});
        let (refused, sent) = run(&ui, SHOW, padded).await;
        let refused = refused.unwrap_err();
        assert_eq!(refused.code, ErrorCode::LimitExceeded);
        assert!(refused.message.contains("the arguments are"), "{}", refused.message);
        assert!(sent.is_empty() && ui.accepted("s").is_none());
        let many: Vec<Value> =
            (0..300).map(|n| json!({"id": format!("c{n}"), "component": "Column", "children": [format!("m{n}")]})).collect();
        let refused = run(&ui, SHOW, json!({"surface": "s", "components": many})).await.0.unwrap_err();
        assert!(refused.message.contains("`components` lists 300 components"), "{}", refused.message);
        let mut nested = json!({"component": "Text", "text": "leaf"});
        for _ in 0..40 {
            nested = json!({"component": "Column", "children": [nested]});
        }
        let refused = run(&ui, SHOW, json!({"surface": "s", "components": [nested]})).await.0.unwrap_err();
        assert!(refused.message.contains("nested deeper than 16"), "{}", refused.message);
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
    async fn siblings_without_a_root_are_stacked_and_a_missing_id_is_made_up() {
        let ui = Arc::new(SessionUi::new("A"));
        let flat = json!({"components": [
            {"id": "t", "component": "Table", "columns": ["a"], "rows": [["1"]]},
            {"id": "p", "component": "Progress", "value": {"path": "/progress"}}
        ], "data": {"progress": 20}});
        let (shown, _) = run(&ui, SHOW, flat).await;
        assert!(text(&shown.unwrap()).contains("`ui1`"), "the result names the surface");
        let surface = ui.accepted("ui1").unwrap();
        assert_eq!(surface.root().and_then(|r| r.prop("children")), Some(&json!(["t", "p"])));
        let (second, _) = run(&ui, SHOW, json!({"components": [{"id": "x", "component": "Divider"}]})).await;
        assert!(text(&second.unwrap()).contains("`ui2`"));
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
