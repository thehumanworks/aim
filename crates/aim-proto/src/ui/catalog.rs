//! `aim/terminal@1`: the components a surface may use, their props and what each prop holds
//! (docs/research/extensibility.md §C.2). It is data: the session host validates against it, the
//! `ui_catalog` tool describes it, and the JSON Schema of each component is generated from it.

use serde_json::{Map, Value, json};

use super::TERMINAL_CATALOG;

/// What a prop holds. Kinds marked *bindable* also accept `{"path": "/pointer"}`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PropKind {
    /// A string (bindable).
    Text,
    /// A number (bindable).
    Number,
    /// `true` or `false`.
    Bool,
    /// One of the listed strings.
    Choice(&'static [&'static str]),
    /// The id of another component.
    Child,
    /// Ids of other components.
    Children,
    /// Strings (bindable).
    TextList,
    /// Table rows: arrays of cells, or objects keyed by column (bindable).
    Rows,
    /// `[{"key": …, "value": …}]` (bindable).
    Pairs,
    /// Styled text: `[{"text": …, "style": <text style>}]`.
    Spans,
    /// `{"name": …, "context": {…}}`: what a press sends back.
    Action,
}

impl PropKind {
    /// Whether `{"path": …}` is accepted.
    #[must_use]
    pub const fn bindable(self) -> bool {
        matches!(self, Self::Text | Self::Number | Self::TextList | Self::Rows | Self::Pairs)
    }

    /// A short name for descriptions and errors.
    #[must_use]
    pub fn describe(self) -> String {
        let base = match self {
            Self::Text => "string".to_owned(),
            Self::Number => "number".to_owned(),
            Self::Bool => "boolean".to_owned(),
            Self::Choice(values) => values.join("|"),
            Self::Child => "component id".to_owned(),
            Self::Children => "[component ids]".to_owned(),
            Self::TextList => "[strings]".to_owned(),
            Self::Rows => "[[cells]] or [{column: cell}]".to_owned(),
            Self::Pairs => "[{key, value}]".to_owned(),
            Self::Spans => "[{text, style?}]".to_owned(),
            Self::Action => "{name, context?}".to_owned(),
        };
        if self.bindable() { format!("{base} or {{path}}") } else { base }
    }
}

/// One prop of a component.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PropDef {
    /// Its name.
    pub name: &'static str,
    /// What it holds.
    pub kind: PropKind,
    /// Whether it must be present.
    pub required: bool,
    /// One line for the model.
    pub doc: &'static str,
}

/// One component of a catalog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ComponentDef {
    /// Its name, e.g. `Table`.
    pub name: &'static str,
    /// One line for the model.
    pub summary: &'static str,
    /// Its props (`id`, `component` and `fallback` are common to all).
    pub props: &'static [PropDef],
    /// At least one of these props must be present (e.g. a Button's `label` or `child`).
    pub one_of: &'static [&'static str],
}

impl ComponentDef {
    /// The prop called `name`.
    #[must_use]
    pub fn prop(&self, name: &str) -> Option<&PropDef> {
        self.props.iter().find(|p| p.name == name)
    }

    /// The component described for a model: summary, then one line per prop.
    #[must_use]
    pub fn describe(&self) -> String {
        use core::fmt::Write as _;
        let mut out = format!("{}: {}", self.name, self.summary);
        for prop in self.props {
            let required = if prop.required { "" } else { "?" };
            let _infallible = write!(out, "\n  {}{required}: {} — {}", prop.name, prop.kind.describe(), prop.doc);
        }
        if !self.one_of.is_empty() {
            let _infallible = write!(out, "\n  (one of {} is required)", self.one_of.join(", "));
        }
        out
    }

    /// The JSON Schema of a component message of this type (common props included).
    #[must_use]
    pub fn json_schema(&self) -> Value {
        let mut properties = Map::new();
        properties.insert("id".into(), json!({"type": "string"}));
        properties.insert("component".into(), json!({"const": self.name}));
        properties.insert(
            "fallback".into(),
            json!({"anyOf": [{"type": "string"}, {"type": "object", "properties": {"child": {"type": "string"}}, "required": ["child"]}]}),
        );
        let mut required = vec![json!("id"), json!("component")];
        for prop in self.props {
            properties.insert(prop.name.into(), prop_schema(prop.kind));
            if prop.required {
                required.push(json!(prop.name));
            }
        }
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }
}

fn prop_schema(kind: PropKind) -> Value {
    let plain = match kind {
        PropKind::Text | PropKind::Child => json!({"type": "string"}),
        PropKind::Number => json!({"type": "number"}),
        PropKind::Bool => json!({"type": "boolean"}),
        PropKind::Choice(values) => json!({"enum": values}),
        PropKind::Children | PropKind::TextList => json!({"type": "array", "items": {"type": "string"}}),
        PropKind::Rows => json!({"type": "array", "items": {"type": ["array", "object"]}}),
        PropKind::Pairs => json!({"type": "array", "items": {"type": "object", "required": ["key", "value"]}}),
        PropKind::Spans => {
            json!({"type": "array", "items": {"type": "object", "properties": {"text": {"type": "string"}, "style": {"enum": TEXT_STYLES}}, "required": ["text"]}})
        }
        PropKind::Action => {
            json!({"type": "object", "properties": {"name": {"type": "string"}, "context": {"type": "object"}}, "required": ["name"]})
        }
    };
    if kind.bindable() {
        json!({"anyOf": [plain, {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false}]})
    } else {
        plain
    }
}

/// A catalog: an id and its components.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Catalog {
    /// Its id, e.g. `aim/terminal@1`.
    pub id: &'static str,
    /// Its components.
    pub components: &'static [ComponentDef],
}

impl Catalog {
    /// The component called `name`.
    #[must_use]
    pub fn component(&self, name: &str) -> Option<&ComponentDef> {
        self.components.iter().find(|c| c.name == name)
    }

    /// Component names, space-separated.
    #[must_use]
    pub fn names(&self) -> String {
        self.components.iter().map(|c| c.name).collect::<Vec<_>>().join(" ")
    }
}

/// Styles a `Text` (or a span) can take; they map onto each client's theme roles.
pub const TEXT_STYLES: &[&str] = &["plain", "muted", "accent", "bold", "italic", "code", "heading", "success", "warning", "error"];

/// Tones of a `Badge`.
pub const TONES: &[&str] = &["info", "success", "warning", "error", "muted"];

const fn prop(name: &'static str, kind: PropKind, required: bool, doc: &'static str) -> PropDef {
    PropDef { name, kind, required, doc }
}

const TEXT_PROP: PropDef = prop("text", PropKind::Text, true, "the text");

/// `aim/terminal@1`.
pub static TERMINAL: Catalog = Catalog {
    id: TERMINAL_CATALOG,
    components: &[
        ComponentDef {
            name: "Text",
            summary: "a line or paragraph of styled text",
            props: &[
                prop("text", PropKind::Text, false, "the text"),
                prop("spans", PropKind::Spans, false, "styled pieces, instead of text"),
                prop("style", PropKind::Choice(TEXT_STYLES), false, "style of the whole text"),
            ],
            one_of: &["text", "spans"],
        },
        ComponentDef { name: "Markdown", summary: "CommonMark text", props: &[TEXT_PROP], one_of: &[] },
        ComponentDef {
            name: "Code",
            summary: "a code block",
            props: &[TEXT_PROP, prop("language", PropKind::Text, false, "language label")],
            one_of: &[],
        },
        ComponentDef { name: "Diff", summary: "a unified diff, coloured by line", props: &[TEXT_PROP], one_of: &[] },
        ComponentDef {
            name: "Row",
            summary: "children side by side",
            props: &[prop("children", PropKind::Children, true, "component ids, left to right")],
            one_of: &[],
        },
        ComponentDef {
            name: "Column",
            summary: "children stacked",
            props: &[prop("children", PropKind::Children, true, "component ids, top to bottom")],
            one_of: &[],
        },
        ComponentDef {
            name: "Box",
            summary: "a bordered box around its children",
            props: &[
                prop("title", PropKind::Text, false, "shown in the border"),
                prop("child", PropKind::Child, false, "the content"),
                prop("children", PropKind::Children, false, "the content, stacked"),
            ],
            one_of: &["child", "children"],
        },
        ComponentDef {
            name: "Divider",
            summary: "a horizontal rule",
            props: &[prop("label", PropKind::Text, false, "text on the rule")],
            one_of: &[],
        },
        ComponentDef {
            name: "List",
            summary: "a bulleted or numbered list",
            props: &[
                prop("items", PropKind::TextList, false, "the entries"),
                prop("children", PropKind::Children, false, "component ids as entries"),
                prop("ordered", PropKind::Bool, false, "number the entries"),
            ],
            one_of: &["items", "children"],
        },
        ComponentDef {
            name: "Table",
            summary: "rows under a header",
            props: &[prop("columns", PropKind::TextList, true, "header cells"), prop("rows", PropKind::Rows, true, "the rows")],
            one_of: &[],
        },
        ComponentDef {
            name: "KeyValue",
            summary: "aligned key/value pairs",
            props: &[prop("items", PropKind::Pairs, true, "the pairs")],
            one_of: &[],
        },
        ComponentDef {
            name: "Progress",
            summary: "a progress bar",
            props: &[
                prop("value", PropKind::Number, true, "done so far"),
                prop("max", PropKind::Number, false, "the total (default 100)"),
                prop("label", PropKind::Text, false, "shown after the bar"),
            ],
            one_of: &[],
        },
        ComponentDef {
            name: "Spinner",
            summary: "work in progress",
            props: &[prop("label", PropKind::Text, false, "what is running")],
            one_of: &[],
        },
        ComponentDef {
            name: "Badge",
            summary: "a short status label",
            props: &[TEXT_PROP, prop("tone", PropKind::Choice(TONES), false, "colour")],
            one_of: &[],
        },
        ComponentDef {
            name: "Log",
            summary: "the tail of a stream of lines",
            props: &[
                prop("lines", PropKind::TextList, true, "the lines, oldest first"),
                prop("max_lines", PropKind::Number, false, "lines shown (default 8)"),
            ],
            one_of: &[],
        },
        ComponentDef {
            name: "Button",
            summary: "pressing it sends the action to you as a <ui_action> user message",
            props: &[
                prop("label", PropKind::Text, false, "its text"),
                prop("child", PropKind::Child, false, "a component shown instead of label"),
                prop("action", PropKind::Action, true, "what a press sends"),
            ],
            one_of: &["label", "child"],
        },
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_catalog_has_unique_names_and_valid_requirements() {
        let names: std::collections::BTreeSet<&str> = TERMINAL.components.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), TERMINAL.components.len());
        for component in TERMINAL.components {
            for prop in component.one_of {
                assert!(component.prop(prop).is_some(), "{}: one_of names a prop it has", component.name);
            }
            let schema = component.json_schema();
            assert_eq!(schema["properties"]["component"]["const"], component.name);
        }
        assert!(TERMINAL.component("Table").is_some_and(|t| t.describe().contains("columns: [strings] or {path}")));
    }
}
