//! The A2UI v1.0 adapter: the only code that knows A2UI's wire names (ADR 0064).
//!
//! aim's own messages ([`UiEnvelope`]) use aim's naming and carry aim's additions; this module
//! exports them as A2UI v1.0 messages and imports A2UI messages, so an A2UI-speaking agent renders
//! in aim and aim's surfaces can feed any A2UI renderer. aim's additions travel as the extension
//! `dev_aim` under A2UI's `metadata.extensions` (surface placement, component fallback), which
//! conformant renderers ignore. When A2UI renames something again, only this module changes.
//!
//! Mappings:
//! - `create_surface` ↔ `createSurface{surfaceId, catalogId, components, dataModel,
//!   metadata.extensions.dev_aim.placement}`;
//! - `create_surface{replace: true}` → `deleteSurface` then `createSurface` (A2UI has no replace;
//!   an import is never a replacement);
//! - `update_components` ↔ `updateComponents{surfaceId, components}`, its `ops` following as
//!   `updateDataModel`s;
//! - `update_data_model{ops}` → one `updateDataModel{surfaceId, path, value}` per op (and back,
//!   one op per message);
//! - `delete_surface` ↔ `deleteSurface{surfaceId}`;
//! - a component's `fallback` ↔ `metadata.extensions.dev_aim.fallback`; a `Button`'s
//!   `action{name, context}` ↔ `action.event{name, context}`;
//! - [`UiAction`] ↔ `action{name, surfaceId, sourceComponentId, timestamp, context}`.

use serde_json::{Map, Value, json};

use super::{A2UI_VERSION, Component, DataOp, Fallback, Placement, UiAction, UiEnvelope, UiMessage};

/// A2UI's version string for the version aim pins.
pub const WIRE_VERSION: &str = "v1.0";

/// The extension key aim's additions travel under.
pub const EXTENSION: &str = "dev_aim";

/// Why an A2UI message could not be imported.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImportError(pub String);

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "A2UI import: {}", self.0)
    }
}

impl core::error::Error for ImportError {}

fn fail<T>(message: impl Into<String>) -> Result<T, ImportError> {
    Err(ImportError(message.into()))
}

fn extension(value: &Value) -> Value {
    json!({"extensions": {EXTENSION: value}})
}

fn export_component(component: &Component) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(component.id));
    out.insert("component".into(), json!(component.component));
    for (key, value) in &component.props {
        let value = match (component.component.as_str(), key.as_str(), value) {
            ("Button", "action", Value::Object(action)) => json!({"event": action}),
            _ => value.clone(),
        };
        out.insert(key.clone(), value);
    }
    if let Some(fallback) = &component.fallback {
        out.insert("metadata".into(), extension(&json!({"fallback": fallback})));
    }
    Value::Object(out)
}

fn import_component(value: &Value) -> Result<Component, ImportError> {
    let Value::Object(map) = value else { return fail("a component is not an object") };
    let Some(id) = map.get("id").and_then(Value::as_str) else { return fail("a component has no string `id`") };
    let Some(name) = map.get("component").and_then(Value::as_str) else { return fail(format!("component `{id}` has no `component` name")) };
    let mut component = Component::new(id, name);
    for (key, value) in map {
        match key.as_str() {
            "id" | "component" => {}
            "metadata" => {
                if let Some(fallback) = value.pointer(&format!("/extensions/{EXTENSION}/fallback")) {
                    component.fallback = serde_json::from_value::<Fallback>(fallback.clone()).ok();
                }
            }
            "action" if name == "Button" => {
                let action = value.get("event").cloned().unwrap_or_else(|| value.clone());
                component.props.insert(key.clone(), action);
            }
            _ => {
                component.props.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(component)
}

/// `envelope` as A2UI v1.0 messages (one per data op for `update_data_model`).
#[must_use]
pub fn export(envelope: &UiEnvelope) -> Vec<Value> {
    let wrap = |key: &str, body: Value| json!({"version": WIRE_VERSION, key: body});
    match &envelope.message {
        UiMessage::CreateSurface { surface_id, replace, catalog_id, placement, components, data } => {
            let mut out = Vec::new();
            if *replace {
                out.push(wrap("deleteSurface", json!({"surfaceId": surface_id})));
            }
            let mut body = Map::new();
            body.insert("surfaceId".into(), json!(surface_id));
            body.insert("catalogId".into(), json!(catalog_id));
            if !components.is_empty() {
                body.insert("components".into(), Value::Array(components.iter().map(export_component).collect()));
            }
            if let Some(data) = data {
                body.insert("dataModel".into(), data.clone());
            }
            body.insert("metadata".into(), extension(&json!({"placement": placement.name()})));
            out.push(wrap("createSurface", Value::Object(body)));
            out
        }
        UiMessage::UpdateComponents { surface_id, components, ops } => {
            let mut out = vec![wrap(
                "updateComponents",
                json!({"surfaceId": surface_id, "components": components.iter().map(export_component).collect::<Vec<_>>()}),
            )];
            out.extend(export_ops(surface_id, ops));
            out
        }
        UiMessage::UpdateDataModel { surface_id, ops } => export_ops(surface_id, ops),
        UiMessage::DeleteSurface { surface_id } => vec![wrap("deleteSurface", json!({"surfaceId": surface_id}))],
    }
}

fn export_ops(surface_id: &str, ops: &[DataOp]) -> Vec<Value> {
    ops.iter()
        .map(|op| {
            let path = if op.path.is_empty() { "/" } else { op.path.as_str() };
            json!({"version": WIRE_VERSION, "updateDataModel": {"surfaceId": surface_id, "path": path, "value": op.value}})
        })
        .collect()
}

fn surface_id(body: &Value) -> Result<String, ImportError> {
    body.get("surfaceId").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| ImportError("`surfaceId` is missing".into()))
}

fn components(body: &Value) -> Result<Vec<Component>, ImportError> {
    match body.get("components") {
        None => Ok(Vec::new()),
        Some(Value::Array(list)) => list.iter().map(import_component).collect(),
        Some(_) => fail("`components` is not an array"),
    }
}

/// One A2UI v1.0 agent-to-renderer message as aim's envelope.
///
/// # Errors
/// [`ImportError`] for another version, an unknown or unsupported message (`callRendererFunction`,
/// `agentFunctionResponse`), or a malformed body.
pub fn import(message: &Value) -> Result<UiEnvelope, ImportError> {
    match message.get("version").and_then(Value::as_str) {
        Some(WIRE_VERSION) => {}
        Some(other) => return fail(format!("version `{other}` is not supported (aim speaks A2UI {WIRE_VERSION})")),
        None => return fail("`version` is missing"),
    }
    let message = if let Some(body) = message.get("createSurface") {
        let placement = match body.pointer(&format!("/metadata/extensions/{EXTENSION}/placement")).and_then(Value::as_str) {
            Some(name) => Placement::parse(name).ok_or_else(|| ImportError(format!("unknown placement `{name}`")))?,
            None => Placement::default(),
        };
        UiMessage::CreateSurface {
            surface_id: surface_id(body)?,
            replace: false,
            catalog_id: body.get("catalogId").and_then(Value::as_str).unwrap_or(super::TERMINAL_CATALOG).to_owned(),
            placement,
            components: components(body)?,
            data: body.get("dataModel").cloned(),
        }
    } else if let Some(body) = message.get("updateComponents") {
        UiMessage::UpdateComponents { surface_id: surface_id(body)?, components: components(body)?, ops: Vec::new() }
    } else if let Some(body) = message.get("updateDataModel") {
        let path = body.get("path").and_then(Value::as_str).unwrap_or_default().to_owned();
        let value = body.get("value").cloned().unwrap_or(Value::Null);
        UiMessage::UpdateDataModel { surface_id: surface_id(body)?, ops: vec![DataOp { path, value }] }
    } else if let Some(body) = message.get("deleteSurface") {
        UiMessage::DeleteSurface { surface_id: surface_id(body)? }
    } else if message.get("callRendererFunction").is_some() || message.get("agentFunctionResponse").is_some() {
        return fail("function calls are not supported yet");
    } else {
        return fail("not an agent-to-renderer message");
    };
    Ok(UiEnvelope { a2ui: A2UI_VERSION.to_owned(), message })
}

/// `action` as an A2UI v1.0 renderer-to-agent message; `timestamp` is ISO 8601.
#[must_use]
pub fn export_action(action: &UiAction, timestamp: &str) -> Value {
    json!({
        "version": WIRE_VERSION,
        "action": {
            "name": action.name,
            "surfaceId": action.surface_id,
            "sourceComponentId": action.source_component_id,
            "timestamp": timestamp,
            "context": action.context,
        }
    })
}

/// An A2UI v1.0 `action` message as aim's [`UiAction`].
///
/// # Errors
/// [`ImportError`] when it is not a v1.0 action with a name, surface and source component.
pub fn import_action(message: &Value) -> Result<UiAction, ImportError> {
    if message.get("version").and_then(Value::as_str) != Some(WIRE_VERSION) {
        return fail(format!("not an A2UI {WIRE_VERSION} message"));
    }
    let Some(action) = message.get("action") else { return fail("not an action") };
    let field =
        |name: &str| action.get(name).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| ImportError(format!("`{name}` is missing")));
    Ok(UiAction {
        name: field("name")?,
        surface_id: field("surfaceId")?,
        source_component_id: field("sourceComponentId")?,
        context: action.get("context").and_then(Value::as_object).cloned().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2ui_messages_round_trip_through_the_adapter() {
        let button = Component::new("go", "Button")
            .with("label", json!("Deploy"))
            .with("action", json!({"name": "deploy", "context": {"env": "prod"}}));
        let mut text = Component::new("root", "Column").with("children", json!(["go"]));
        text.fallback = Some(Fallback::Text("deploy?".into()));
        let create = UiEnvelope::new(UiMessage::CreateSurface {
            surface_id: "s".into(),
            replace: false,
            catalog_id: super::super::TERMINAL_CATALOG.into(),
            placement: Placement::Dialog,
            components: vec![text, button],
            data: Some(json!({"n": 1})),
        });
        let wire = export(&create);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["createSurface"]["metadata"]["extensions"]["dev_aim"]["placement"], "dialog");
        assert_eq!(wire[0]["createSurface"]["components"][1]["action"]["event"]["name"], "deploy");
        assert_eq!(wire[0]["createSurface"]["components"][0]["metadata"]["extensions"]["dev_aim"]["fallback"], "deploy?");
        assert_eq!(import(&wire[0]).unwrap(), create);

        let data = UiEnvelope::new(UiMessage::UpdateDataModel {
            surface_id: "s".into(),
            ops: vec![DataOp { path: "/n".into(), value: json!(2) }],
        });
        assert_eq!(import(&export(&data)[0]).unwrap(), data);
        let replace = UiEnvelope::new(UiMessage::CreateSurface {
            surface_id: "s".into(),
            replace: true,
            catalog_id: super::super::TERMINAL_CATALOG.into(),
            placement: Placement::Transcript,
            components: Vec::new(),
            data: None,
        });
        let wire = export(&replace);
        assert_eq!((wire.len(), wire[0].get("deleteSurface").is_some(), wire[1].get("createSurface").is_some()), (2, true, true));
        let mixed = UiEnvelope::new(UiMessage::UpdateComponents {
            surface_id: "s".into(),
            components: vec![Component::new("x", "Divider")],
            ops: vec![DataOp { path: String::new(), value: json!({"n": 3}) }],
        });
        let wire = export(&mixed);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[1]["updateDataModel"]["path"], "/");
        let delete = UiEnvelope::new(UiMessage::DeleteSurface { surface_id: "s".into() });
        assert_eq!(export(&delete), vec![json!({"version": "v1.0", "deleteSurface": {"surfaceId": "s"}})]);
        assert_eq!(import(&export(&delete)[0]).unwrap(), delete);
    }

    #[test]
    fn foreign_versions_and_function_calls_are_refused() {
        assert!(import(&json!({"version": "v0.9", "deleteSurface": {"surfaceId": "s"}})).is_err());
        assert!(import(&json!({"version": "v1.0", "callRendererFunction": {}})).is_err());
        assert!(import(&json!({"deleteSurface": {"surfaceId": "s"}})).is_err());
    }

    #[test]
    fn actions_map_to_and_from_a2ui() {
        let action = UiAction { name: "ok".into(), surface_id: "s".into(), source_component_id: "b".into(), context: Map::new() };
        let wire = export_action(&action, "2026-09-25T12:00:00Z");
        assert_eq!(wire["action"]["sourceComponentId"], "b");
        assert_eq!(import_action(&wire).unwrap(), action);
    }
}
