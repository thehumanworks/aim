//! Declarative UI surfaces (docs/architecture.md §8.2, docs/adr/0017 and 0064).
//!
//! Agents, plugins and aim itself describe UI as data in the shape of A2UI v1.0: a *surface* holds
//! a flat, id-keyed list of components (one with the id `root`), a JSON data model that components
//! bind to with `{"path": "/json/pointer"}`, and a placement that says where a client shows it.
//! Messages create a surface, upsert components, apply JSON-Pointer data operations and delete it;
//! a pressed button flows back as a [`UiAction`].
//!
//! - [`UiEnvelope`] pins the A2UI version aim speaks (`a2ui: "1.0"`) around one [`UiMessage`].
//! - [`model`] is the fold every party runs: the session host, the TUI and the web client apply the
//!   same messages to the same [`model::Surface`] state, so they stay at parity by construction.
//! - [`catalog`] describes `aim/terminal@1`, the components and props a surface may use.
//! - [`a2ui`] is the only place that knows A2UI's wire names, so a rename upstream touches one
//!   module.
//!
//! This module has no policy: limits, rates and ownership are the session host's (`aim::ui`).

use std::borrow::Cow;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

pub mod a2ui;
pub mod catalog;
pub mod model;

/// The A2UI version aim's envelope pins (A2UI v1.0 is a release candidate; see ADR 0064).
pub const A2UI_VERSION: &str = "1.0";

/// The id of aim's terminal-first catalog.
pub const TERMINAL_CATALOG: &str = "aim/terminal@1";

/// The component every surface renders from.
pub const ROOT_ID: &str = "root";

fn terminal_catalog() -> String {
    TERMINAL_CATALOG.to_owned()
}

/// Where a client shows a surface. A client that cannot show a placement degrades it (the TUI
/// shows anything it lacks in the transcript).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub enum Placement {
    /// The left end of the status line.
    StatusLeft,
    /// The right end of the status line.
    StatusRight,
    /// Pinned above the editor.
    WidgetAboveEditor,
    /// Pinned below the editor.
    WidgetBelowEditor,
    /// A side panel (the TUI's fullscreen layout; inline it becomes a widget above the editor).
    PanelSide,
    /// Floating over the content.
    Overlay,
    /// A modal box; its buttons take the keyboard.
    Dialog,
    /// In the transcript, where it was created (the default).
    #[default]
    Transcript,
    /// Under a tool call's row in the transcript.
    Tool {
        /// The provider call id of that tool call.
        call_id: String,
    },
    /// A short-lived notice.
    Toast,
    /// The window or tab title.
    Title,
}

impl Placement {
    /// Every placement name except `tool(<call_id>)`, as written in messages.
    pub const NAMES: [&'static str; 10] = [
        "status.left",
        "status.right",
        "widget.above_editor",
        "widget.below_editor",
        "panel.side",
        "overlay",
        "dialog",
        "transcript",
        "toast",
        "title",
    ];

    /// Parses `status.left`, `widget.above_editor`, `tool(<call_id>)`, …
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let placement = match text.trim() {
            "status.left" => Self::StatusLeft,
            "status.right" => Self::StatusRight,
            "widget.above_editor" | "widget" => Self::WidgetAboveEditor,
            "widget.below_editor" => Self::WidgetBelowEditor,
            "panel.side" => Self::PanelSide,
            "overlay" => Self::Overlay,
            "dialog" => Self::Dialog,
            "transcript" => Self::Transcript,
            "toast" => Self::Toast,
            "title" => Self::Title,
            other => {
                let call_id = other.strip_prefix("tool(")?.strip_suffix(')')?.trim();
                if call_id.is_empty() {
                    return None;
                }
                Self::Tool { call_id: call_id.to_owned() }
            }
        };
        Some(placement)
    }

    /// The placement as written in messages.
    #[must_use]
    pub fn name(&self) -> Cow<'static, str> {
        match self {
            Self::StatusLeft => "status.left".into(),
            Self::StatusRight => "status.right".into(),
            Self::WidgetAboveEditor => "widget.above_editor".into(),
            Self::WidgetBelowEditor => "widget.below_editor".into(),
            Self::PanelSide => "panel.side".into(),
            Self::Overlay => "overlay".into(),
            Self::Dialog => "dialog".into(),
            Self::Transcript => "transcript".into(),
            Self::Tool { call_id } => format!("tool({call_id})").into(),
            Self::Toast => "toast".into(),
            Self::Title => "title".into(),
        }
    }
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl Serialize for Placement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.name())
    }
}

impl<'de> Deserialize<'de> for Placement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("unknown placement `{text}` (expected {} or tool(<call_id>))", Self::NAMES.join(", ")))
        })
    }
}

impl JsonSchema for Placement {
    fn schema_name() -> Cow<'static, str> {
        "Placement".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "status.left | status.right | widget.above_editor | widget.below_editor | panel.side | overlay | dialog | transcript | tool(<call_id>) | toast | title",
            "pattern": "^(status\\.(left|right)|widget\\.(above|below)_editor|panel\\.side|overlay|dialog|transcript|toast|title|tool\\([^()]+\\))$"
        })
    }
}

/// What a client renders instead of a component it does not support.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Fallback {
    /// Plain text.
    Text(String),
    /// Another component of the same surface.
    Child {
        /// Its id.
        child: String,
    },
}

/// One component: an id, a catalog component name, an optional fallback and the component's own
/// props (checked against the catalog by the session host).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Component {
    /// Unique within its surface; `root` is where rendering starts.
    pub id: String,
    /// The catalog component, e.g. `Text` or `Table`.
    pub component: String,
    /// Shown by clients that do not support `component`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,
    /// The component's props (`text`, `children`, `rows`, …). A prop whose value is
    /// `{"path": "/pointer"}` is bound to the surface's data model.
    #[serde(flatten)]
    pub props: Map<String, Value>,
}

impl Component {
    /// A component with no props.
    #[must_use]
    pub fn new(id: impl Into<String>, component: impl Into<String>) -> Self {
        Self { id: id.into(), component: component.into(), fallback: None, props: Map::new() }
    }

    /// The same component with `prop` set to `value`.
    #[must_use]
    pub fn with(mut self, prop: &str, value: Value) -> Self {
        self.props.insert(prop.to_owned(), value);
        self
    }

    /// A prop, when present.
    #[must_use]
    pub fn prop(&self, name: &str) -> Option<&Value> {
        self.props.get(name)
    }

    /// Ids of the components this one contains: `child`, then `children`, then a `Child`
    /// fallback.
    #[must_use]
    pub fn child_ids(&self) -> Vec<&str> {
        let mut ids = Vec::new();
        if let Some(Value::String(child)) = self.props.get("child") {
            ids.push(child.as_str());
        }
        if let Some(Value::Array(children)) = self.props.get("children") {
            ids.extend(children.iter().filter_map(Value::as_str));
        }
        if let Some(Fallback::Child { child }) = &self.fallback {
            ids.push(child.as_str());
        }
        ids
    }
}

/// One JSON-Pointer data operation: set `value` at `path`, or delete what is there when `value`
/// is `null` (A2UI's `updateDataModel`). An empty path or `/` is the whole data model.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DataOp {
    /// RFC 6901 pointer; `""` or `"/"` is the root.
    #[serde(default)]
    pub path: String,
    /// The new value; `null` deletes.
    pub value: Value,
}

/// A message about one surface.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiMessage {
    /// Creates a surface, optionally with its components and data in the same message.
    CreateSurface {
        /// The surface's id (unique within its session).
        surface_id: String,
        /// The catalog its components come from.
        #[serde(default = "terminal_catalog")]
        catalog_id: String,
        /// Where it shows.
        #[serde(default)]
        placement: Placement,
        /// Initial components.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        components: Vec<Component>,
        /// Initial data model (an object).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// Adds or replaces components by id.
    UpdateComponents {
        /// The surface.
        surface_id: String,
        /// Components to upsert.
        components: Vec<Component>,
    },
    /// Applies data operations in order.
    UpdateDataModel {
        /// The surface.
        surface_id: String,
        /// Operations.
        ops: Vec<DataOp>,
    },
    /// Removes a surface.
    DeleteSurface {
        /// The surface.
        surface_id: String,
    },
}

impl UiMessage {
    /// The surface the message is about.
    #[must_use]
    pub fn surface_id(&self) -> &str {
        match self {
            Self::CreateSurface { surface_id, .. }
            | Self::UpdateComponents { surface_id, .. }
            | Self::UpdateDataModel { surface_id, .. }
            | Self::DeleteSurface { surface_id } => surface_id,
        }
    }
}

/// A [`UiMessage`] with the A2UI version it was written against (always [`A2UI_VERSION`] when
/// written by this build). Stored in session logs and sent to clients.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct UiEnvelope {
    /// The pinned A2UI version (`"1.0"`).
    pub a2ui: String,
    /// The message.
    #[serde(flatten)]
    pub message: UiMessage,
}

impl UiEnvelope {
    /// `message` under this build's A2UI version.
    #[must_use]
    pub fn new(message: UiMessage) -> Self {
        Self { a2ui: A2UI_VERSION.to_owned(), message }
    }
}

/// A user's action on a surface (a pressed button), flowing back to the surface's owner.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct UiAction {
    /// The action's name, from the component's `action.name`.
    pub name: String,
    /// The surface it happened on.
    pub surface_id: String,
    /// The component that raised it.
    pub source_component_id: String,
    /// The component's `action.context`, with bindings resolved.
    #[serde(default)]
    pub context: Map<String, Value>,
}

/// The tag around an action delivered to an agent as user input.
const ACTION_OPEN: &str = "<ui_action>";
const ACTION_CLOSE: &str = "</ui_action>";

/// The JSON inside an action's input text: the short names an agent reads.
#[derive(Serialize, Deserialize)]
struct ActionText {
    surface: String,
    component: String,
    name: String,
    #[serde(default)]
    context: Map<String, Value>,
}

impl UiAction {
    /// The action as the user-side input item an agent receives on its next turn:
    /// `<ui_action>{"surface":…,"component":…,"name":…,"context":{…}}</ui_action>`.
    #[must_use]
    pub fn to_input_text(&self) -> String {
        let text = ActionText {
            surface: self.surface_id.clone(),
            component: self.source_component_id.clone(),
            name: self.name.clone(),
            context: self.context.clone(),
        };
        let json = serde_json::to_string(&text).unwrap_or_else(|_| String::from("{}"));
        format!("{ACTION_OPEN}{json}{ACTION_CLOSE}")
    }

    /// Reads [`UiAction::to_input_text`]'s form back (surrounding whitespace allowed).
    #[must_use]
    pub fn from_input_text(text: &str) -> Option<Self> {
        let json = text.trim().strip_prefix(ACTION_OPEN)?.strip_suffix(ACTION_CLOSE)?;
        let parsed: ActionText = serde_json::from_str(json).ok()?;
        Some(Self { name: parsed.name, surface_id: parsed.surface, source_component_id: parsed.component, context: parsed.context })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn placements_parse_and_print_the_same_names() {
        for name in Placement::NAMES {
            let placement = Placement::parse(name).unwrap_or_default();
            assert_eq!(placement.name(), name);
        }
        assert_eq!(Placement::parse("tool(call_7)"), Some(Placement::Tool { call_id: "call_7".into() }));
        assert_eq!(Placement::parse("tool()"), None);
        assert_eq!(Placement::parse("sidebar"), None);
        assert_eq!(serde_json::to_value(Placement::Tool { call_id: "c".into() }).ok(), Some(json!("tool(c)")));
    }

    #[test]
    fn actions_survive_the_input_text_form() {
        let mut context = Map::new();
        context.insert("env".into(), json!("prod"));
        let action = UiAction { name: "deploy".into(), surface_id: "release".into(), source_component_id: "go".into(), context };
        let text = action.to_input_text();
        assert!(text.starts_with("<ui_action>{\"surface\":\"release\""), "{text}");
        assert_eq!(UiAction::from_input_text(&format!("  {text}\n")), Some(action));
        assert_eq!(UiAction::from_input_text("<ui_action>not json</ui_action>"), None);
        assert_eq!(UiAction::from_input_text("hello"), None);
    }
}
