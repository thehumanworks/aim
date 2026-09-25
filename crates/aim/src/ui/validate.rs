//! Checking a message against the catalog and the session's limits before it is accepted
//! (ADR 0064). Pure: a message and the surfaces so far in, the surfaces after it or a reason out.

use std::collections::{BTreeSet, HashMap};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ui::catalog::{Catalog, ComponentDef, PropKind, TERMINAL, TEXT_STYLES};
use aim_proto::ui::model::{Surface, Surfaces, binding, parse_pointer};
use aim_proto::ui::{A2UI_VERSION, Component, Fallback, ROOT_ID, TERMINAL_CATALOG, UiEnvelope, UiMessage};
use serde_json::Value;

use super::limits::Limits;

fn invalid(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}

fn too_big(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::LimitExceeded, message)
}

/// Whether `id` is a valid surface or component id: 1–64 of `[A-Za-z0-9_.-]`.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The catalog a surface names, when this build has it.
#[must_use]
pub fn catalog(id: &str) -> Option<&'static Catalog> {
    (id == TERMINAL_CATALOG).then_some(&TERMINAL)
}

/// What an accepted message leaves behind besides the new state: things worth telling the agent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notes {
    /// Children referenced but not (yet) defined; clients show a placeholder.
    pub missing: Vec<String>,
}

/// Checks `envelope` against `surfaces` (the session's accepted state) and returns the state
/// after it.
///
/// # Errors
/// `invalid_params` for a malformed message, `not_found` for a surface that does not exist,
/// `limit_exceeded` past a bound.
pub fn check(envelope: &UiEnvelope, surfaces: &Surfaces, limits: &Limits) -> Result<(Surfaces, Notes), ProtoError> {
    if envelope.a2ui != A2UI_VERSION {
        return Err(invalid(format!("a2ui `{}` is not supported (this build speaks {A2UI_VERSION})", envelope.a2ui)));
    }
    let size = serde_json::to_vec(envelope).map_or(usize::MAX, |bytes| bytes.len());
    if size > limits.message_bytes {
        return Err(too_big(format!("the message is {size} bytes; the limit is {}", limits.message_bytes)));
    }
    let message = &envelope.message;
    let id = message.surface_id();
    if !valid_id(id) {
        return Err(invalid(format!("surface id `{id}` must be 1–64 of A-Z a-z 0-9 _ . -")));
    }
    match message {
        UiMessage::CreateSurface { replace, catalog_id, components, data, .. } => {
            let exists = surfaces.get(id).is_some();
            if exists && !replace {
                return Err(ProtoError::new(ErrorCode::Conflict, format!("surface `{id}` already exists")));
            }
            // A replacement does not add a surface.
            if !exists && surfaces.list.len() >= limits.surfaces {
                return Err(too_big(format!("a session shows at most {} surfaces; close one first", limits.surfaces)));
            }
            let Some(catalog) = catalog(catalog_id) else {
                return Err(invalid(format!("catalog `{catalog_id}` is not supported (supported: {TERMINAL_CATALOG})")));
            };
            check_components(components, catalog)?;
            if data.as_ref().is_some_and(|d| !d.is_object()) {
                return Err(invalid("`data` must be an object"));
            }
        }
        UiMessage::UpdateComponents { components, ops, .. } => {
            let surface = surfaces.get(id).ok_or_else(|| missing(id))?;
            let catalog = catalog(&surface.catalog_id).unwrap_or(&TERMINAL);
            check_components(components, catalog)?;
            for op in ops {
                parse_pointer(&op.path).map_err(|e| invalid(e.to_string()))?;
            }
        }
        UiMessage::UpdateDataModel { ops, .. } => {
            surfaces.get(id).ok_or_else(|| missing(id))?;
            for op in ops {
                parse_pointer(&op.path).map_err(|e| invalid(e.to_string()))?;
            }
        }
        UiMessage::DeleteSurface { .. } => {
            surfaces.get(id).ok_or_else(|| missing(id))?;
        }
    }
    let mut next = surfaces.clone();
    next.apply(message, 0).map_err(|e| invalid(e.to_string()))?;
    let notes = match next.get(id) {
        Some(surface) => check_surface(surface, limits)?,
        None => Notes::default(),
    };
    Ok((next, notes))
}

fn missing(id: &str) -> ProtoError {
    ProtoError::new(ErrorCode::NotFound, format!("no surface `{id}` (ui_show creates one)"))
}

fn check_components(components: &[Component], catalog: &Catalog) -> Result<(), ProtoError> {
    let mut seen = BTreeSet::new();
    for component in components {
        if !valid_id(&component.id) {
            return Err(invalid(format!("component id `{}` must be 1–64 of A-Z a-z 0-9 _ . -", component.id)));
        }
        if !seen.insert(component.id.as_str()) {
            return Err(invalid(format!("component id `{}` appears twice", component.id)));
        }
        if let Some(Fallback::Child { child }) = &component.fallback
            && !valid_id(child)
        {
            return Err(invalid(format!("component `{}`: fallback child `{child}` is not a component id", component.id)));
        }
        match catalog.component(&component.component) {
            Some(def) => check_props(component, def)?,
            None if component.fallback.is_some() => {}
            None => {
                return Err(invalid(format!(
                    "component `{}`: `{}` is not in {} and has no fallback. Components: {}",
                    component.id,
                    component.component,
                    catalog.id,
                    catalog.names()
                )));
            }
        }
    }
    Ok(())
}

fn check_props(component: &Component, def: &ComponentDef) -> Result<(), ProtoError> {
    let wrong = |why: String| invalid(format!("component `{}` ({}): {why}. {}", component.id, def.name, def.describe()));
    for (name, value) in &component.props {
        let Some(prop) = def.prop(name) else {
            return Err(wrong(format!("unknown prop `{name}`")));
        };
        if let Some(path) = binding(value) {
            if !prop.kind.bindable() {
                return Err(wrong(format!("`{name}` cannot be bound to data")));
            }
            parse_pointer(path).map_err(|e| wrong(e.to_string()))?;
            continue;
        }
        if !fits(prop.kind, value) {
            return Err(wrong(format!("`{name}` must be {}", prop.kind.describe())));
        }
    }
    for prop in def.props.iter().filter(|p| p.required) {
        if !component.props.contains_key(prop.name) {
            return Err(wrong(format!("`{}` is required", prop.name)));
        }
    }
    if !def.one_of.is_empty() && !def.one_of.iter().any(|name| component.props.contains_key(*name)) {
        return Err(wrong(format!("one of {} is required", def.one_of.join(", "))));
    }
    Ok(())
}

fn scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null)
}

/// Whether a literal `value` fits `kind`.
fn fits(kind: PropKind, value: &Value) -> bool {
    match kind {
        // Numbers and booleans read fine as text; a model often sends them for cells and labels.
        PropKind::Text => matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_)),
        PropKind::Number => value.is_number(),
        PropKind::Bool => value.is_boolean(),
        PropKind::Choice(values) => value.as_str().is_some_and(|v| values.contains(&v)),
        PropKind::Child => value.as_str().is_some_and(valid_id),
        PropKind::Children => value.as_array().is_some_and(|ids| ids.iter().all(|id| id.as_str().is_some_and(valid_id))),
        PropKind::TextList => value.as_array().is_some_and(|items| items.iter().all(scalar)),
        PropKind::Rows => value.as_array().is_some_and(|rows| {
            rows.iter().all(|row| match row {
                Value::Array(cells) => cells.iter().all(scalar),
                Value::Object(cells) => cells.values().all(scalar),
                _ => false,
            })
        }),
        PropKind::Pairs => value.as_array().is_some_and(|pairs| {
            pairs.iter().all(|pair| {
                pair.get("key").is_some_and(scalar)
                    && pair.get("value").is_some_and(scalar)
                    && pair.as_object().is_some_and(|p| p.len() == 2)
            })
        }),
        PropKind::Spans => value.as_array().is_some_and(|spans| {
            spans.iter().all(|span| {
                span.get("text").is_some_and(Value::is_string)
                    && span.get("style").is_none_or(|style| style.as_str().is_some_and(|s| TEXT_STYLES.contains(&s)))
                    && span.as_object().is_some_and(|s| s.keys().all(|k| k == "text" || k == "style"))
            })
        }),
        PropKind::Action => value.as_object().is_some_and(|action| {
            action.get("name").and_then(Value::as_str).is_some_and(|name| !name.is_empty())
                && action.get("context").is_none_or(Value::is_object)
                && action.keys().all(|k| k == "name" || k == "context")
        }),
    }
}

/// Checks a surface's size and its whole component graph — `root`'s tree and any component not
/// under it: every component is contained at most once, there are no cycles, and no tree is
/// deeper than the limit. Missing children (and a missing `root`) are allowed: clients show a
/// placeholder, and they are reported.
fn check_surface(surface: &Surface, limits: &Limits) -> Result<Notes, ProtoError> {
    if surface.components.len() > limits.components {
        return Err(too_big(format!("a surface holds at most {} components", limits.components)));
    }
    let size = surface.size();
    if size > limits.surface_bytes {
        return Err(too_big(format!("the surface would be {size} bytes; the limit is {}", limits.surface_bytes)));
    }
    let by_id: HashMap<&str, usize> = surface.components.iter().enumerate().map(|(index, c)| (c.id.as_str(), index)).collect();
    let mut notes = Notes::default();
    if !surface.components.is_empty() && !by_id.contains_key(ROOT_ID) {
        notes.missing.push(ROOT_ID.to_owned());
    }
    // Every component is contained at most once: the graph is then a forest plus, possibly,
    // cycles, which are exactly the components no walk from an uncontained one reaches.
    let mut contained = vec![false; surface.components.len()];
    for component in &surface.components {
        for child in component.child_ids() {
            match by_id.get(child).and_then(|index| contained.get_mut(*index)) {
                Some(seen) if *seen => {
                    return Err(invalid(format!(
                        "component `{child}` is contained twice (a child shared by two parents, or listed twice)"
                    )));
                }
                Some(seen) => *seen = true,
                None => notes.missing.push(child.to_owned()),
            }
        }
    }
    let mut reached = vec![false; surface.components.len()];
    let mut stack: Vec<(usize, usize)> = contained.iter().enumerate().filter(|(_, c)| !**c).map(|(index, _)| (index, 1)).collect();
    while let Some((index, depth)) = stack.pop() {
        if depth > limits.depth {
            return Err(too_big(format!("a component tree is deeper than {} levels", limits.depth)));
        }
        let (Some(component), Some(seen)) = (surface.components.get(index), reached.get_mut(index)) else { continue };
        *seen = true;
        stack.extend(component.child_ids().into_iter().filter_map(|child| by_id.get(child)).map(|child| (*child, depth.saturating_add(1))));
    }
    if let Some(index) = reached.iter().position(|r| !r) {
        let id = surface.components.get(index).map_or("?", |c| c.id.as_str());
        return Err(invalid(format!("component `{id}` is part of a cycle")));
    }
    notes.missing.sort();
    notes.missing.dedup();
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use aim_proto::ui::{DataOp, Placement};
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn create(components: Vec<Component>) -> UiEnvelope {
        UiEnvelope::new(UiMessage::CreateSurface {
            surface_id: "s".into(),
            replace: false,
            catalog_id: TERMINAL_CATALOG.into(),
            placement: Placement::Transcript,
            components,
            data: None,
        })
    }

    fn text(id: &str) -> Component {
        Component::new(id, "Text").with("text", json!("hi"))
    }

    fn column(id: &str, children: &[&str]) -> Component {
        Component::new(id, "Column").with("children", json!(children))
    }

    fn code(result: Result<(Surfaces, Notes), ProtoError>) -> Option<ErrorCode> {
        result.err().map(|e| e.code)
    }

    #[test]
    fn catalog_props_are_checked_strictly_and_errors_name_the_props() {
        let limits = Limits::default();
        let none = Surfaces::default();
        assert!(check(&create(vec![text("root")]), &none, &limits).is_ok());
        let wrong = check(&create(vec![Component::new("root", "Table").with("columns", json!(["a"]))]), &none, &limits).unwrap_err();
        assert!(wrong.message.contains("`rows` is required") && wrong.message.contains("rows: [[cells]]"), "{}", wrong.message);
        let unknown = check(&create(vec![text("root").with("colour", json!("red"))]), &none, &limits).unwrap_err();
        assert!(unknown.message.contains("unknown prop `colour`"), "{}", unknown.message);
        let typed = check(&create(vec![Component::new("root", "Progress").with("value", json!("half"))]), &none, &limits).unwrap_err();
        assert!(typed.message.contains("`value` must be number or {path}"), "{}", typed.message);
        let bound = create(vec![Component::new("root", "Progress").with("value", json!({"path": "/p"}))]);
        assert!(check(&bound, &none, &limits).is_ok());
        let not_bindable = create(vec![Component::new("root", "List").with("ordered", json!({"path": "/o"})).with("items", json!([]))]);
        assert_eq!(code(check(&not_bindable, &none, &limits)), Some(ErrorCode::InvalidParams));
        let button = Component::new("root", "Button").with("action", json!({"name": "go"}));
        assert!(check(&create(vec![button.clone()]), &none, &limits).unwrap_err().message.contains("one of label, child"));
        assert!(check(&create(vec![button.with("label", json!("Go"))]), &none, &limits).is_ok());
    }

    #[test]
    fn unknown_components_need_a_fallback() {
        let limits = Limits::default();
        let none = Surfaces::default();
        let bare = Component::new("root", "Sparkline").with("values", json!([1, 2]));
        assert_eq!(code(check(&create(vec![bare.clone()]), &none, &limits)), Some(ErrorCode::InvalidParams));
        let with = Component { fallback: Some(Fallback::Text("1 2".into())), ..bare };
        assert!(check(&create(vec![with]), &none, &limits).is_ok());
    }

    #[test]
    fn trees_are_bounded_acyclic_and_may_miss_children() {
        let limits = Limits { depth: 3, ..Limits::default() };
        let none = Surfaces::default();
        let (_, notes) = check(&create(vec![column("root", &["a", "later"]), text("a")]), &none, &limits).unwrap();
        assert_eq!(notes.missing, ["later"]);
        let cycle = create(vec![column("root", &["a"]), column("a", &["root"])]);
        assert_eq!(code(check(&cycle, &none, &limits)), Some(ErrorCode::InvalidParams));
        let shared = create(vec![column("root", &["a", "b"]), column("a", &["t"]), column("b", &["t"]), text("t")]);
        assert_eq!(code(check(&shared, &none, &limits)), Some(ErrorCode::InvalidParams));
        let deep = create(vec![column("root", &["a"]), column("a", &["b"]), column("b", &["c"]), text("c")]);
        assert_eq!(code(check(&deep, &none, &limits)), Some(ErrorCode::LimitExceeded));
        let duplicate = create(vec![text("root"), text("root")]);
        assert_eq!(code(check(&duplicate, &none, &limits)), Some(ErrorCode::InvalidParams));
    }

    /// REV19 A3: the whole graph is checked, not only what `root` reaches.
    #[test]
    fn rev19_cycles_and_depth_outside_root_are_refused() {
        let limits = Limits { depth: 3, ..Limits::default() };
        let none = Surfaces::default();
        let rootless_cycle = create(vec![column("a", &["b"]), column("b", &["a"])]);
        let refused = check(&rootless_cycle, &none, &limits).unwrap_err();
        assert!(refused.message.contains("cycle"), "{}", refused.message);
        let beside_root = create(vec![text("root"), column("a", &["b"]), column("b", &["a"])]);
        assert_eq!(code(check(&beside_root, &none, &limits)), Some(ErrorCode::InvalidParams));
        let self_loop = create(vec![text("root"), column("x", &["x"])]);
        assert_eq!(code(check(&self_loop, &none, &limits)), Some(ErrorCode::InvalidParams));
        let deep_orphan = create(vec![text("root"), column("a", &["b"]), column("b", &["c"]), column("c", &["d"]), text("d")]);
        assert_eq!(code(check(&deep_orphan, &none, &limits)), Some(ErrorCode::LimitExceeded));
        // A rootless forest is still accepted and reported: clients show a placeholder.
        let (_, notes) = check(&create(vec![column("a", &["t"]), text("t")]), &none, &limits).unwrap();
        assert_eq!(notes.missing, ["root"]);
    }

    /// REV19 A1: replacing is one message; validation never leaves the old surface deleted.
    #[test]
    fn rev19_a_refused_replacement_keeps_the_old_surface() {
        let limits = Limits { surfaces: 1, ..Limits::default() };
        let (one, _) = check(&create(vec![text("root")]), &Surfaces::default(), &limits).unwrap();
        let replace = |components| {
            UiEnvelope::new(UiMessage::CreateSurface {
                surface_id: "s".into(),
                replace: true,
                catalog_id: TERMINAL_CATALOG.into(),
                placement: Placement::Dialog,
                components,
                data: None,
            })
        };
        let bad = replace(vec![Component::new("root", "Text")]);
        assert_eq!(code(check(&bad, &one, &limits)), Some(ErrorCode::InvalidParams));
        let (next, _) = check(&replace(vec![text("root")]), &one, &limits).unwrap();
        assert_eq!(next.get("s").map(|s| s.placement.clone()), Some(Placement::Dialog), "at the session's surface limit, too");
    }

    #[test]
    fn surfaces_must_exist_and_ids_must_be_plain() {
        let limits = Limits::default();
        let none = Surfaces::default();
        let update = UiEnvelope::new(UiMessage::UpdateDataModel { surface_id: "s".into(), ops: vec![] });
        assert_eq!(code(check(&update, &none, &limits)), Some(ErrorCode::NotFound));
        let bad = UiEnvelope::new(UiMessage::DeleteSurface { surface_id: "no spaces".into() });
        assert_eq!(code(check(&bad, &none, &limits)), Some(ErrorCode::InvalidParams));
        let (one, _) = check(&create(vec![text("root")]), &none, &limits).unwrap();
        assert_eq!(code(check(&create(vec![text("root")]), &one, &limits)), Some(ErrorCode::Conflict));
        let pointer =
            UiEnvelope::new(UiMessage::UpdateDataModel { surface_id: "s".into(), ops: vec![DataOp { path: "x".into(), value: json!(1) }] });
        assert_eq!(code(check(&pointer, &one, &limits)), Some(ErrorCode::InvalidParams));
        let foreign = UiEnvelope { a2ui: "0.9".into(), ..create(vec![]) };
        assert_eq!(code(check(&foreign, &none, &limits)), Some(ErrorCode::InvalidParams));
    }

    /// A random message over a small alphabet of surfaces, components and pointers.
    fn message() -> impl Strategy<Value = UiMessage> {
        let surface = prop::sample::select(vec!["a", "b", "c"]).prop_map(str::to_owned);
        let ids = prop::sample::select(vec!["root", "x", "y", "z"]);
        let component = (ids.clone(), prop::collection::vec(ids, 0..3), any::<bool>())
            .prop_map(|(id, children, leaf)| if leaf { text(id) } else { column(id, &children) });
        let path = prop::sample::select(vec!["", "/", "/p", "/p/q", "/list", "/list/0", "/list/-", "bad"]).prop_map(str::to_owned);
        let value = prop_oneof![Just(Value::Null), any::<i32>().prop_map(|n| json!(n)), Just(json!([1, 2])), Just(json!({"q": 1}))];
        prop_oneof![
            (surface.clone(), prop::collection::vec(component.clone(), 0..5), any::<bool>()).prop_map(
                |(surface_id, components, replace)| {
                    UiMessage::CreateSurface {
                        surface_id,
                        replace,
                        catalog_id: TERMINAL_CATALOG.into(),
                        placement: Placement::Transcript,
                        components,
                        data: None,
                    }
                }
            ),
            (
                surface.clone(),
                prop::collection::vec(component, 1..4),
                prop::collection::vec((path.clone(), value.clone()).prop_map(|(path, value)| DataOp { path, value }), 0..3)
            )
                .prop_map(|(surface_id, components, ops)| UiMessage::UpdateComponents { surface_id, components, ops }),
            (surface.clone(), prop::collection::vec((path, value).prop_map(|(path, value)| DataOp { path, value }), 1..4))
                .prop_map(|(surface_id, ops)| UiMessage::UpdateDataModel { surface_id, ops }),
            surface.prop_map(|surface_id| UiMessage::DeleteSurface { surface_id }),
        ]
    }

    proptest! {
        /// Whatever the agent sends, the accepted state never exceeds a bound, every surface's tree
        /// is acyclic and within depth, a refused message changes nothing, and an accepted one is
        /// exactly the shared fold.
        #[test]
        fn accepted_state_stays_within_bounds(messages in prop::collection::vec(message(), 1..40)) {
            let limits = Limits { surfaces: 2, components: 3, depth: 3, ..Limits::default() };
            let mut state = Surfaces::default();
            for message in messages {
                let envelope = UiEnvelope::new(message.clone());
                if let Ok((next, _)) = check(&envelope, &state, &limits) {
                    let mut folded = state.clone();
                    prop_assert!(folded.apply(&message, 0).is_ok());
                    prop_assert_eq!(&folded, &next);
                    state = next;
                }
                prop_assert!(state.list.len() <= limits.surfaces);
                for surface in &state.list {
                    prop_assert!(surface.components.len() <= limits.components);
                    prop_assert!(check_surface(surface, &limits).is_ok());
                    prop_assert!(surface.data.is_object());
                }
            }
        }
    }
}
