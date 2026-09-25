//! The surface state every party folds messages into: the session host (for replay and
//! validation), the TUI and the web client. It applies messages mechanically and reports what is
//! structurally impossible (a surface that does not exist, a pointer into a string); limits and
//! ownership are the host's policy (`aim::ui`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{Component, DataOp, Placement, ROOT_ID, UiAction, UiMessage};

/// Why a message or data operation could not apply.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ApplyError {
    /// `create_surface` for an id that exists.
    Exists(String),
    /// A message for a surface that does not exist.
    Missing(String),
    /// A malformed JSON pointer.
    Pointer(String),
    /// A pointer through something that is not an object or array, or past an array's end.
    Path(String),
}

impl core::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exists(id) => write!(f, "surface `{id}` already exists"),
            Self::Missing(id) => write!(f, "no surface `{id}`"),
            Self::Pointer(path) => write!(f, "`{path}` is not a JSON pointer (RFC 6901: \"\" or \"/a/0/b\")"),
            Self::Path(path) => write!(f, "`{path}` does not lead into an object or array"),
        }
    }
}

impl core::error::Error for ApplyError {}

/// One surface: where it shows, its components and its data.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Surface {
    /// Id, unique within its session.
    pub id: String,
    /// Catalog of its components.
    pub catalog_id: String,
    /// Where it shows.
    pub placement: Placement,
    /// How many transcript items preceded its creation: a `transcript` surface replays there.
    #[serde(default)]
    pub anchor: u64,
    /// Components in first-upsert order, ids unique.
    #[serde(default)]
    pub components: Vec<Component>,
    /// The data model (an object).
    #[serde(default = "empty_object")]
    pub data: Value,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

impl Surface {
    /// An empty surface.
    #[must_use]
    pub fn new(id: impl Into<String>, catalog_id: impl Into<String>, placement: Placement, anchor: u64) -> Self {
        Self { id: id.into(), catalog_id: catalog_id.into(), placement, anchor, components: Vec::new(), data: empty_object() }
    }

    /// The component with `id`.
    #[must_use]
    pub fn component(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    /// The component rendering starts from.
    #[must_use]
    pub fn root(&self) -> Option<&Component> {
        self.component(ROOT_ID)
    }

    /// Adds or replaces components by id (a replaced one keeps its position).
    pub fn upsert(&mut self, components: &[Component]) {
        for component in components {
            match self.components.iter_mut().find(|c| c.id == component.id) {
                Some(slot) => slot.clone_from(component),
                None => self.components.push(component.clone()),
            }
        }
    }

    /// Applies one data operation.
    ///
    /// # Errors
    /// [`ApplyError::Pointer`] or [`ApplyError::Path`]; the data is unchanged then.
    pub fn apply_data(&mut self, op: &DataOp) -> Result<(), ApplyError> {
        let tokens = parse_pointer(&op.path)?;
        let mut next = self.data.clone();
        if op.value.is_null() {
            remove(&mut next, &tokens, &op.path)?;
        } else {
            set(&mut next, &tokens, op.value.clone(), &op.path)?;
        }
        if !next.is_object() {
            next = empty_object();
        }
        self.data = next;
        Ok(())
    }

    /// `value` with a binding (`{"path": "/pointer"}`) replaced by the data it points at (`null`
    /// when nothing is there); any other value unchanged.
    #[must_use]
    pub fn resolve<'a>(&'a self, value: &'a Value) -> std::borrow::Cow<'a, Value> {
        match binding(value) {
            Some(path) => std::borrow::Cow::Owned(lookup(&self.data, path).cloned().unwrap_or(Value::Null)),
            None => std::borrow::Cow::Borrowed(value),
        }
    }

    /// A prop of `component` with its binding resolved.
    #[must_use]
    pub fn prop<'a>(&'a self, component: &'a Component, name: &str) -> Option<std::borrow::Cow<'a, Value>> {
        component.props.get(name).map(|value| self.resolve(value))
    }

    /// The action a press of button `id` sends: its `action.name`, and its `action.context` with
    /// bindings resolved against the data (so every client sends the same action).
    #[must_use]
    pub fn action(&self, id: &str) -> Option<UiAction> {
        let action = self.component(id)?.prop("action")?;
        let name = action.get("name")?.as_str()?.to_owned();
        let context = action
            .get("context")
            .and_then(Value::as_object)
            .map(|context| context.iter().map(|(k, v)| (k.clone(), self.resolve(v).into_owned())).collect())
            .unwrap_or_default();
        Some(UiAction { name, surface_id: self.id.clone(), source_component_id: id.to_owned(), context })
    }

    /// Serialized size in bytes (what limits are measured on).
    #[must_use]
    pub fn size(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }
}

/// The path of a binding: an object whose only key is a string `path`.
#[must_use]
pub fn binding(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) if map.len() == 1 => map.get("path").and_then(Value::as_str),
        _ => None,
    }
}

/// Splits an RFC 6901 pointer into unescaped tokens; `""` and `"/"` are the root (A2UI).
///
/// # Errors
/// [`ApplyError::Pointer`] for text that does not start with `/`, or a bad `~` escape.
pub fn parse_pointer(path: &str) -> Result<Vec<String>, ApplyError> {
    if path.is_empty() || path == "/" {
        return Ok(Vec::new());
    }
    let Some(rest) = path.strip_prefix('/') else { return Err(ApplyError::Pointer(path.to_owned())) };
    rest.split('/')
        .map(|token| {
            let mut out = String::with_capacity(token.len());
            let mut chars = token.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    match chars.next() {
                        Some('0') => out.push('~'),
                        Some('1') => out.push('/'),
                        _ => return Err(ApplyError::Pointer(path.to_owned())),
                    }
                } else {
                    out.push(c);
                }
            }
            Ok(out)
        })
        .collect()
}

/// The value `path` points at in `data`.
#[must_use]
pub fn lookup<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let tokens = parse_pointer(path).ok()?;
    let mut here = data;
    for token in &tokens {
        here = match here {
            Value::Object(map) => map.get(token)?,
            Value::Array(items) => items.get(index(token)?)?,
            _ => return None,
        };
    }
    Some(here)
}

/// An array index token: digits without a leading zero.
fn index(token: &str) -> Option<usize> {
    if token.is_empty() || (token.len() > 1 && token.starts_with('0')) || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

/// Sets `tokens` in `data` to `value`, creating missing object members on the way; in an array
/// the index may be one past the end (or `-`) to append.
fn set(data: &mut Value, tokens: &[String], value: Value, path: &str) -> Result<(), ApplyError> {
    let Some((last, parents)) = tokens.split_last() else {
        *data = value;
        return Ok(());
    };
    let mut here = data;
    for token in parents {
        here = match here {
            Value::Object(map) => map.entry(token.clone()).or_insert_with(empty_object),
            Value::Array(items) => {
                let at = index(token).ok_or_else(|| ApplyError::Path(path.to_owned()))?;
                items.get_mut(at).ok_or_else(|| ApplyError::Path(path.to_owned()))?
            }
            _ => return Err(ApplyError::Path(path.to_owned())),
        };
    }
    match here {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(items) => {
            let at = if last == "-" { items.len() } else { index(last).ok_or_else(|| ApplyError::Path(path.to_owned()))? };
            if let Some(slot) = items.get_mut(at) {
                *slot = value;
            } else if at == items.len() {
                items.push(value);
            } else {
                return Err(ApplyError::Path(path.to_owned()));
            }
            Ok(())
        }
        _ => Err(ApplyError::Path(path.to_owned())),
    }
}

/// Removes what `tokens` points at (nothing to remove is not an error); the root becomes `{}`.
fn remove(data: &mut Value, tokens: &[String], path: &str) -> Result<(), ApplyError> {
    let Some((last, parents)) = tokens.split_last() else {
        *data = empty_object();
        return Ok(());
    };
    let mut here = data;
    for token in parents {
        here = match here {
            Value::Object(map) => match map.get_mut(token) {
                Some(next) => next,
                None => return Ok(()),
            },
            Value::Array(items) => match index(token).and_then(|at| items.get_mut(at)) {
                Some(next) => next,
                None => return Ok(()),
            },
            _ => return Err(ApplyError::Path(path.to_owned())),
        };
    }
    match here {
        Value::Object(map) => {
            map.remove(last);
            Ok(())
        }
        Value::Array(items) => {
            if let Some(at) = index(last).filter(|at| *at < items.len()) {
                items.remove(at);
            }
            Ok(())
        }
        _ => Err(ApplyError::Path(path.to_owned())),
    }
}

/// What applying a message did.
#[derive(Clone, PartialEq, Debug)]
pub enum Change {
    /// A surface was created.
    Created(String),
    /// A surface's components or data changed.
    Updated(String),
    /// A surface was removed (its last state).
    Deleted(Box<Surface>),
}

/// A session's surfaces, in creation order.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Surfaces {
    /// The surfaces.
    pub list: Vec<Surface>,
}

impl Surfaces {
    /// The surfaces of a snapshot (e.g. `SessionAttachResult::surfaces`).
    #[must_use]
    pub fn from_snapshot(list: Vec<Surface>) -> Self {
        Self { list }
    }

    /// The surface with `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Surface> {
        self.list.iter().find(|s| s.id == id)
    }

    /// Applies `message`; a created surface records `anchor` (transcript items before it).
    /// Data operations apply all or nothing.
    ///
    /// # Errors
    /// [`ApplyError`]; nothing changed then.
    pub fn apply(&mut self, message: &UiMessage, anchor: u64) -> Result<Change, ApplyError> {
        match message {
            UiMessage::CreateSurface { surface_id, catalog_id, placement, components, data } => {
                if self.get(surface_id).is_some() {
                    return Err(ApplyError::Exists(surface_id.clone()));
                }
                let mut surface = Surface::new(surface_id.clone(), catalog_id.clone(), placement.clone(), anchor);
                surface.upsert(components);
                if let Some(data) = data {
                    surface.apply_data(&DataOp { path: String::new(), value: data.clone() })?;
                }
                self.list.push(surface);
                Ok(Change::Created(surface_id.clone()))
            }
            UiMessage::UpdateComponents { surface_id, components } => {
                let surface = self.get_mut(surface_id)?;
                surface.upsert(components);
                Ok(Change::Updated(surface_id.clone()))
            }
            UiMessage::UpdateDataModel { surface_id, ops } => {
                let surface = self.get_mut(surface_id)?;
                let mut next = surface.clone();
                for op in ops {
                    next.apply_data(op)?;
                }
                surface.data = next.data;
                Ok(Change::Updated(surface_id.clone()))
            }
            UiMessage::DeleteSurface { surface_id } => {
                let at = self.list.iter().position(|s| s.id == *surface_id).ok_or_else(|| ApplyError::Missing(surface_id.clone()))?;
                Ok(Change::Deleted(Box::new(self.list.remove(at))))
            }
        }
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Surface, ApplyError> {
        self.list.iter_mut().find(|s| s.id == id).ok_or_else(|| ApplyError::Missing(id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn surface() -> Surface {
        Surface::new("s", super::super::TERMINAL_CATALOG, Placement::Transcript, 0)
    }

    fn op(path: &str, value: Value) -> DataOp {
        DataOp { path: path.into(), value }
    }

    #[test]
    fn pointers_follow_rfc_6901_with_a2ui_root() {
        assert_eq!(parse_pointer("").ok(), Some(vec![]));
        assert_eq!(parse_pointer("/").ok(), Some(vec![]));
        assert_eq!(parse_pointer("/a~1b/~0c/0").ok(), Some(vec!["a/b".into(), "~c".into(), "0".into()]));
        assert!(parse_pointer("a").is_err());
        assert!(parse_pointer("/a~2").is_err());
        let data = json!({"rows": [[1, 2], [3, 4]], "a/b": {"~": true}});
        assert_eq!(lookup(&data, "/rows/1/0"), Some(&json!(3)));
        assert_eq!(lookup(&data, "/a~1b/~0"), Some(&json!(true)));
        assert_eq!(lookup(&data, "/rows/01"), None, "no leading zeros");
        assert_eq!(lookup(&data, "/rows/9"), None);
    }

    #[test]
    fn data_ops_set_append_delete_and_refuse_paths_through_scalars() {
        let mut s = surface();
        s.apply_data(&op("/progress/value", json!(30))).unwrap_or_default();
        assert_eq!(s.data, json!({"progress": {"value": 30}}), "missing parents are created");
        s.apply_data(&op("/log", json!(["a"]))).unwrap_or_default();
        s.apply_data(&op("/log/-", json!("b"))).unwrap_or_default();
        s.apply_data(&op("/log/2", json!("c"))).unwrap_or_default();
        assert_eq!(s.data["log"], json!(["a", "b", "c"]));
        assert_eq!(s.apply_data(&op("/log/9", json!("x"))), Err(ApplyError::Path("/log/9".into())));
        assert_eq!(s.apply_data(&op("/progress/value/x", json!(1))), Err(ApplyError::Path("/progress/value/x".into())));
        s.apply_data(&op("/log/0", Value::Null)).unwrap_or_default();
        assert_eq!(s.data["log"], json!(["b", "c"]));
        s.apply_data(&op("/nothing/here", Value::Null)).unwrap_or_default();
        s.apply_data(&op("", Value::Null)).unwrap_or_default();
        assert_eq!(s.data, json!({}), "deleting the root empties the model");
        s.apply_data(&op("/", json!([1]))).unwrap_or_default();
        assert_eq!(s.data, json!({}), "the root stays an object");
    }

    #[test]
    fn bindings_resolve_against_the_data_model() {
        let mut s = surface();
        s.apply_data(&op("/p", json!(60))).unwrap_or_default();
        assert_eq!(*s.resolve(&json!({"path": "/p"})), json!(60));
        assert_eq!(*s.resolve(&json!({"path": "/missing"})), Value::Null);
        assert_eq!(*s.resolve(&json!({"path": "/p", "x": 1})), json!({"path": "/p", "x": 1}), "not a binding");
        assert_eq!(*s.resolve(&json!("text")), json!("text"));
    }

    #[test]
    fn a_button_press_resolves_its_context() {
        let mut s = surface();
        s.upsert(&[Component::new("go", "Button").with("action", json!({"name": "deploy", "context": {"n": {"path": "/n"}, "k": "v"}}))]);
        s.apply_data(&op("/n", json!(3))).unwrap_or_default();
        let action = s.action("go").unwrap_or_else(|| UiAction {
            name: String::new(),
            surface_id: String::new(),
            source_component_id: String::new(),
            context: Map::new(),
        });
        assert_eq!((action.name.as_str(), action.surface_id.as_str(), action.source_component_id.as_str()), ("deploy", "s", "go"));
        assert_eq!(Value::Object(action.context), json!({"n": 3, "k": "v"}));
        assert!(s.action("missing").is_none());
    }

    #[test]
    fn messages_fold_into_surfaces() {
        let mut all = Surfaces::default();
        let create = UiMessage::CreateSurface {
            surface_id: "s".into(),
            catalog_id: super::super::TERMINAL_CATALOG.into(),
            placement: Placement::Dialog,
            components: vec![Component::new("root", "Text").with("text", json!("hi"))],
            data: Some(json!({"n": 1})),
        };
        assert_eq!(all.apply(&create, 4), Ok(Change::Created("s".into())));
        assert_eq!(all.apply(&create, 5), Err(ApplyError::Exists("s".into())));
        let update = UiMessage::UpdateComponents {
            surface_id: "s".into(),
            components: vec![Component::new("root", "Text").with("text", json!("bye")), Component::new("x", "Divider")],
        };
        assert_eq!(all.apply(&update, 9), Ok(Change::Updated("s".into())));
        let s = all.get("s").cloned().unwrap_or_else(surface);
        assert_eq!((s.anchor, s.components.len()), (4, 2));
        assert_eq!(s.root().and_then(|c| c.prop("text")), Some(&json!("bye")));
        let bad = UiMessage::UpdateDataModel { surface_id: "s".into(), ops: vec![op("/m", json!(2)), op("/n/x", json!(1))] };
        assert!(all.apply(&bad, 9).is_err());
        assert_eq!(all.get("s").map(|s| s.data.clone()), Some(json!({"n": 1})), "all or nothing");
        assert!(matches!(all.apply(&UiMessage::DeleteSurface { surface_id: "s".into() }, 9), Ok(Change::Deleted(_))));
        assert_eq!(all.apply(&UiMessage::DeleteSurface { surface_id: "s".into() }, 9), Err(ApplyError::Missing("s".into())));
    }
}
