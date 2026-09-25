//! Agent-authored surfaces in the browser (ADR 0064), rendered from the same fold the session host
//! and the TUI run.
//!
//! Rendering is two steps so the security property is testable without a browser: a surface
//! becomes a [`Node`] tree built only from a fixed set of tags and classes, with every piece of
//! model-supplied text a [`Node::Text`]; the Leptos layer ([`view`]) turns nodes into elements and
//! text nodes. Nothing is ever parsed as HTML (no `innerHTML`): Markdown goes through
//! `pulldown-cmark` into the same tree, raw HTML in it stays text, and links keep an `href` only for
//! `http(s)` and `mailto`. The page's CSP forbids inline styles, so a progress bar is a native
//! `<progress>` element.
//!
//! The web renders `Text`, `Markdown`, `Code`, `List`, `Table`, `KeyValue`, `Progress`, `Badge` and `Button`; any
//! other component shows its fallback, or its children stacked, or its name.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use aim_proto::ui::catalog::TEXT_STYLES;
use aim_proto::ui::model::Surface;
use aim_proto::ui::{Component, Fallback, Placement, ROOT_ID, UiAction};
use leptos::prelude::*;
use pulldown_cmark::{Event, Options, Parser, Tag as Md, TagEnd};
use serde_json::Value;

/// Deepest nesting followed (the host bounds trees tighter; this guards replays).
const MAX_DEPTH: usize = 32;

/// The elements a surface may produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    /// `<div>`.
    Div,
    /// `<span>`.
    Span,
    /// `<p>`.
    P,
    /// `<strong>`.
    Strong,
    /// `<em>`.
    Em,
    /// `<code>`.
    Code,
    /// `<pre>`.
    Pre,
    /// `<ul>`.
    Ul,
    /// `<ol>`.
    Ol,
    /// `<li>`.
    Li,
    /// `<table>`.
    Table,
    /// `<thead>`.
    Thead,
    /// `<tbody>`.
    Tbody,
    /// `<tr>`.
    Tr,
    /// `<th>`.
    Th,
    /// `<td>`.
    Td,
    /// `<dl>`.
    Dl,
    /// `<dt>`.
    Dt,
    /// `<dd>`.
    Dd,
    /// `<blockquote>`.
    Blockquote,
    /// `<hr>`.
    Hr,
    /// `<br>`.
    Br,
}

impl Tag {
    /// The element name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Div => "div",
            Self::Span => "span",
            Self::P => "p",
            Self::Strong => "strong",
            Self::Em => "em",
            Self::Code => "code",
            Self::Pre => "pre",
            Self::Ul => "ul",
            Self::Ol => "ol",
            Self::Li => "li",
            Self::Table => "table",
            Self::Thead => "thead",
            Self::Tbody => "tbody",
            Self::Tr => "tr",
            Self::Th => "th",
            Self::Td => "td",
            Self::Dl => "dl",
            Self::Dt => "dt",
            Self::Dd => "dd",
            Self::Blockquote => "blockquote",
            Self::Hr => "hr",
            Self::Br => "br",
        }
    }
}

/// A rendered surface, before it becomes DOM.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// An element from [`Tag`] with a class from this module.
    El {
        /// The element.
        tag: Tag,
        /// Its class (fixed strings only).
        class: &'static str,
        /// Its children.
        children: Vec<Node>,
    },
    /// Text, shown as text whatever it contains.
    Text(String),
    /// A link with a checked `http(s)`/`mailto` target.
    Link {
        /// The target.
        href: String,
        /// Its content.
        children: Vec<Node>,
    },
    /// A native progress bar.
    Progress {
        /// Done so far.
        value: f64,
        /// The total.
        max: f64,
    },
    /// A button that sends `action` when pressed.
    Button {
        /// Its text.
        label: String,
        /// What a press sends.
        action: UiAction,
    },
}

fn el(tag: Tag, class: &'static str, children: Vec<Node>) -> Node {
    Node::El { tag, class, children }
}

fn text(text: impl Into<String>) -> Node {
    Node::Text(text.into())
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

impl Node {
    /// The visible text.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::El { tag: Tag::Br, .. } => "\n".to_owned(),
            Self::El { children, .. } | Self::Link { children, .. } => children.iter().map(Self::text).collect::<Vec<_>>().join(" "),
            Self::Progress { .. } => String::new(),
            Self::Button { label, .. } => label.clone(),
        }
    }

    /// The HTML this tree becomes in the DOM (for tests and inspection): tags from [`Tag`], text
    /// escaped.
    #[must_use]
    pub fn to_html(&self) -> String {
        let mut out = String::new();
        self.write_html(&mut out);
        out
    }

    fn write_html(&self, out: &mut String) {
        match self {
            Self::Text(text) => escape(text, out),
            Self::El { tag: tag @ (Tag::Hr | Tag::Br), class, .. } => {
                let _infallible = write!(out, "<{} class=\"{class}\">", tag.name());
            }
            Self::El { tag, class, children } => {
                let _infallible = write!(out, "<{} class=\"{class}\">", tag.name());
                for child in children {
                    child.write_html(out);
                }
                let _infallible = write!(out, "</{}>", tag.name());
            }
            Self::Link { href, children } => {
                out.push_str("<a href=\"");
                escape(href, out);
                out.push_str("\" rel=\"noopener noreferrer\" target=\"_blank\">");
                for child in children {
                    child.write_html(out);
                }
                out.push_str("</a>");
            }
            Self::Progress { value, max } => {
                let _infallible = write!(out, "<progress value=\"{value}\" max=\"{max}\"></progress>");
            }
            Self::Button { label, .. } => {
                out.push_str("<button class=\"ui-button\">");
                escape(label, out);
                out.push_str("</button>");
            }
        }
    }
}

/// Where the web client shows a placement: in the transcript, pinned above the composer, or in
/// the top bar. `overlay`, `title` and `tool(…)` degrade to the transcript, like in the TUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// In the transcript.
    Transcript,
    /// Pinned above the composer (widgets, dialogs, the side panel, toasts).
    Pinned,
    /// In the top bar.
    Status,
}

/// The slot `placement` renders in.
#[must_use]
pub fn slot(placement: &Placement) -> Slot {
    match placement {
        Placement::StatusLeft | Placement::StatusRight => Slot::Status,
        Placement::WidgetAboveEditor | Placement::WidgetBelowEditor | Placement::PanelSide | Placement::Dialog | Placement::Toast => {
            Slot::Pinned
        }
        Placement::Transcript | Placement::Tool { .. } | Placement::Overlay | Placement::Title => Slot::Transcript,
    }
}

/// A value as display text.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// The class of a text style (only the catalog's names; anything else is plain).
fn style_class(name: &str) -> &'static str {
    match name {
        "muted" => "ui-muted",
        "accent" => "ui-accent",
        "bold" => "ui-bold",
        "italic" => "ui-italic",
        "code" => "ui-code-span",
        "heading" => "ui-heading",
        "success" => "ui-success",
        "warning" => "ui-warning",
        "error" => "ui-error",
        _ => "ui-plain",
    }
}

fn tone_class(name: &str) -> &'static str {
    match name {
        "success" => "ui-badge ui-success",
        "warning" => "ui-badge ui-warning",
        "error" => "ui-badge ui-error",
        "muted" => "ui-badge ui-muted",
        _ => "ui-badge ui-accent",
    }
}

/// Markdown as nodes: `CommonMark` plus tables and strikethrough; raw HTML stays text, images show
/// their alt text, links keep only safe targets.
#[must_use]
pub fn markdown(source: &str) -> Vec<Node> {
    // Open containers: (tag, class, children); a link target rides along with its container.
    let mut stack: Vec<(Tag, &'static str, Vec<Node>, Option<String>)> = vec![(Tag::Div, "ui-markdown", Vec::new(), None)];
    let push = |stack: &mut Vec<(Tag, &'static str, Vec<Node>, Option<String>)>, node: Node| {
        if let Some((_, _, children, _)) = stack.last_mut() {
            children.push(node);
        }
    };
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut in_head = false;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => {
                let (element, class, href) = match tag {
                    Md::Paragraph => (Tag::P, "", None),
                    Md::Heading { .. } => (Tag::Strong, "ui-heading", None),
                    Md::BlockQuote(_) => (Tag::Blockquote, "", None),
                    Md::CodeBlock(_) => (Tag::Pre, "ui-code", None),
                    Md::List(Some(_)) => (Tag::Ol, "", None),
                    Md::List(None) => (Tag::Ul, "", None),
                    Md::Item => (Tag::Li, "", None),
                    Md::Emphasis => (Tag::Em, "", None),
                    Md::Strong => (Tag::Strong, "", None),
                    Md::Strikethrough => (Tag::Span, "ui-strike", None),
                    Md::Link { dest_url, .. } => {
                        let url = dest_url.to_string();
                        let safe = url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:");
                        (Tag::Span, "ui-link", safe.then_some(url))
                    }
                    Md::Image { .. } => (Tag::Span, "ui-image-alt", None),
                    Md::Table(_) => (Tag::Table, "ui-table", None),
                    Md::TableHead => {
                        in_head = true;
                        (Tag::Tr, "", None)
                    }
                    Md::TableRow => (Tag::Tr, "", None),
                    Md::TableCell => (if in_head { Tag::Th } else { Tag::Td }, "", None),
                    _ => (Tag::Span, "", None),
                };
                stack.push((element, class, Vec::new(), href));
            }
            Event::End(end) => {
                if matches!(end, TagEnd::TableHead) {
                    in_head = false;
                }
                if stack.len() > 1
                    && let Some((tag, class, children, href)) = stack.pop()
                {
                    let node = match href {
                        Some(href) => Node::Link { href, children },
                        None => el(tag, class, children),
                    };
                    push(&mut stack, node);
                }
            }
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => push(&mut stack, text(t.to_string())),
            Event::Code(code) => push(&mut stack, el(Tag::Code, "", vec![text(code.to_string())])),
            Event::SoftBreak => push(&mut stack, text(" ")),
            Event::HardBreak => push(&mut stack, el(Tag::Br, "", Vec::new())),
            Event::Rule => push(&mut stack, el(Tag::Hr, "", Vec::new())),
            Event::TaskListMarker(done) => push(&mut stack, text(if done { "[x] " } else { "[ ] " })),
            other => push(&mut stack, text(format!("{other:?}"))),
        }
    }
    // Close anything left open (malformed input), innermost first.
    while stack.len() > 1 {
        if let Some((tag, class, children, _)) = stack.pop() {
            push(&mut stack, el(tag, class, children));
        }
    }
    stack.pop().map(|(_, _, children, _)| children).unwrap_or_default()
}

struct Ctx<'a> {
    surface: &'a Surface,
}

impl Ctx<'_> {
    fn text(&self, component: &Component, name: &str) -> Option<String> {
        self.surface.prop(component, name).map(|value| scalar(&value))
    }

    fn list(&self, component: &Component, name: &str) -> Vec<Value> {
        match self.surface.prop(component, name).as_deref() {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        }
    }

    fn node(&self, id: &str, depth: usize, path: &mut BTreeSet<String>) -> Node {
        let Some(component) = self.surface.component(id) else { return el(Tag::Span, "ui-missing", vec![text("…")]) };
        if depth > MAX_DEPTH || !path.insert(id.to_owned()) {
            return el(Tag::Span, "ui-missing", vec![text("[…]")]);
        }
        let node = self.component(component, depth, path);
        path.remove(id);
        node
    }

    fn children(&self, component: &Component, depth: usize, path: &mut BTreeSet<String>) -> Vec<Node> {
        component
            .child_ids()
            .into_iter()
            .filter(|id| component.fallback.as_ref() != Some(&Fallback::Child { child: (*id).to_owned() }))
            .map(|id| self.node(id, depth + 1, path))
            .collect()
    }

    fn component(&self, component: &Component, depth: usize, path: &mut BTreeSet<String>) -> Node {
        match component.component.as_str() {
            "Text" => {
                let class = style_class(&self.text(component, "style").unwrap_or_default());
                if let Some(Value::Array(spans)) = component.prop("spans") {
                    let spans = spans
                        .iter()
                        .map(|span| {
                            let style = span.get("style").and_then(Value::as_str).filter(|s| TEXT_STYLES.contains(s)).unwrap_or("plain");
                            el(Tag::Span, style_class(style), vec![text(span.get("text").map(scalar).unwrap_or_default())])
                        })
                        .collect();
                    return el(Tag::Div, class, spans);
                }
                el(Tag::Div, class, vec![text(self.text(component, "text").unwrap_or_default())])
            }
            "Markdown" => el(Tag::Div, "ui-markdown", markdown(&self.text(component, "text").unwrap_or_default())),
            "Code" => {
                let mut children = Vec::new();
                if let Some(language) = self.text(component, "language").filter(|l| !l.is_empty()) {
                    children.push(el(Tag::Div, "ui-code-lang", vec![text(language)]));
                }
                children.push(el(
                    Tag::Pre,
                    "ui-code",
                    vec![el(Tag::Code, "", vec![text(self.text(component, "text").unwrap_or_default())])],
                ));
                el(Tag::Div, "ui-code-block", children)
            }
            "List" => {
                let ordered = component.prop("ordered").and_then(Value::as_bool).unwrap_or(false);
                let items: Vec<Node> = if component.props.contains_key("items") {
                    self.list(component, "items").iter().map(|item| el(Tag::Li, "", vec![text(scalar(item))])).collect()
                } else {
                    self.children(component, depth, path).into_iter().map(|child| el(Tag::Li, "", vec![child])).collect()
                };
                el(if ordered { Tag::Ol } else { Tag::Ul }, "ui-list", items)
            }
            "Table" => {
                let columns: Vec<String> = self.list(component, "columns").iter().map(scalar).collect();
                let head =
                    el(Tag::Thead, "", vec![el(Tag::Tr, "", columns.iter().map(|c| el(Tag::Th, "", vec![text(c.clone())])).collect())]);
                let rows = self
                    .list(component, "rows")
                    .iter()
                    .map(|row| {
                        let cells: Vec<String> = match row {
                            Value::Array(cells) => cells.iter().map(scalar).collect(),
                            Value::Object(cells) => columns.iter().map(|c| cells.get(c).map(scalar).unwrap_or_default()).collect(),
                            other => vec![scalar(other)],
                        };
                        el(Tag::Tr, "", cells.into_iter().map(|c| el(Tag::Td, "", vec![text(c)])).collect())
                    })
                    .collect();
                el(Tag::Table, "ui-table", vec![head, el(Tag::Tbody, "", rows)])
            }
            "KeyValue" => {
                let mut pairs = Vec::new();
                for pair in self.list(component, "items") {
                    pairs.push(el(Tag::Dt, "", vec![text(pair.get("key").map(scalar).unwrap_or_default())]));
                    pairs.push(el(Tag::Dd, "", vec![text(pair.get("value").map(scalar).unwrap_or_default())]));
                }
                el(Tag::Dl, "ui-kv", pairs)
            }
            "Progress" => {
                let value = self.surface.prop(component, "value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let max = self.surface.prop(component, "max").and_then(|v| v.as_f64()).filter(|m| *m > 0.0).unwrap_or(100.0);
                let percent = (value / max).clamp(0.0, 1.0) * 100.0;
                let label = self.text(component, "label").map(|l| format!(" {l}")).unwrap_or_default();
                el(
                    Tag::Div,
                    "ui-progress",
                    vec![
                        Node::Progress { value: value.clamp(0.0, max), max },
                        el(Tag::Span, "", vec![text(format!("{percent:.0}%{label}"))]),
                    ],
                )
            }
            "Badge" => el(
                Tag::Span,
                tone_class(&self.text(component, "tone").unwrap_or_default()),
                vec![text(self.text(component, "text").unwrap_or_default())],
            ),
            "Button" => {
                let label = match component.prop("child").and_then(Value::as_str) {
                    Some(child) => self.node(child, depth + 1, path).text(),
                    None => self.text(component, "label").unwrap_or_default(),
                };
                match self.surface.action(&component.id) {
                    Some(action) => Node::Button { label, action },
                    None => el(Tag::Span, "ui-muted", vec![text(label)]),
                }
            }
            _ => self.fallback(component, depth, path),
        }
    }

    /// What a component this client does not render shows: its fallback, else its children
    /// stacked, else its name.
    fn fallback(&self, component: &Component, depth: usize, path: &mut BTreeSet<String>) -> Node {
        match &component.fallback {
            Some(Fallback::Text(fallback)) => el(Tag::Div, "ui-fallback", vec![text(fallback.clone())]),
            Some(Fallback::Child { child }) => self.node(child, depth + 1, path),
            None => {
                let children = self.children(component, depth, path);
                if children.is_empty() {
                    el(Tag::Span, "ui-missing", vec![text(format!("[{}]", component.component))])
                } else {
                    el(Tag::Div, "ui-stack", children)
                }
            }
        }
    }
}

/// A surface as nodes, from its `root` (else its first component).
#[must_use]
pub fn render(surface: &Surface) -> Node {
    let ctx = Ctx { surface };
    let start = if surface.root().is_some() { Some(ROOT_ID.to_owned()) } else { surface.components.first().map(|c| c.id.clone()) };
    let body = start.map(|id| ctx.node(&id, 1, &mut BTreeSet::new())).into_iter().collect();
    el(Tag::Div, "ui-surface", body)
}

/// An action as a transcript row shows it.
#[must_use]
pub fn action_line(action: &UiAction) -> String {
    let context = if action.context.is_empty() { String::new() } else { format!(" {}", Value::Object(action.context.clone())) };
    format!("⚡ {} · {}/{}{context}", action.name, action.surface_id, action.source_component_id)
}

/// Nodes as Leptos views: elements from [`Tag`], model text only ever as text nodes. `press`
/// receives a pressed button's action.
pub fn view<F>(node: Node, press: &F) -> AnyView
where
    F: Fn(UiAction) + Clone + Send + Sync + 'static,
{
    match node {
        Node::Text(content) => content.into_any(),
        Node::Progress { value, max } => view! { <progress value=value max=max></progress> }.into_any(),
        Node::Button { label, action } => {
            let press = press.clone();
            view! { <button type="button" class="ui-button" on:click=move |_| press(action.clone())>{label}</button> }.into_any()
        }
        Node::Link { href, children } => {
            let children = children.into_iter().map(|child| view(child, press)).collect_view();
            view! { <a href=href rel="noopener noreferrer" target="_blank">{children}</a> }.into_any()
        }
        Node::El { tag, class, children } => {
            let kids = children.into_iter().map(|child| view(child, press)).collect_view();
            match tag {
                Tag::Div => view! { <div class=class>{kids}</div> }.into_any(),
                Tag::Span => view! { <span class=class>{kids}</span> }.into_any(),
                Tag::P => view! { <p class=class>{kids}</p> }.into_any(),
                Tag::Strong => view! { <strong class=class>{kids}</strong> }.into_any(),
                Tag::Em => view! { <em class=class>{kids}</em> }.into_any(),
                Tag::Code => view! { <code class=class>{kids}</code> }.into_any(),
                Tag::Pre => view! { <pre class=class>{kids}</pre> }.into_any(),
                Tag::Ul => view! { <ul class=class>{kids}</ul> }.into_any(),
                Tag::Ol => view! { <ol class=class>{kids}</ol> }.into_any(),
                Tag::Li => view! { <li class=class>{kids}</li> }.into_any(),
                Tag::Table => view! { <table class=class>{kids}</table> }.into_any(),
                Tag::Thead => view! { <thead class=class>{kids}</thead> }.into_any(),
                Tag::Tbody => view! { <tbody class=class>{kids}</tbody> }.into_any(),
                Tag::Tr => view! { <tr class=class>{kids}</tr> }.into_any(),
                Tag::Th => view! { <th class=class>{kids}</th> }.into_any(),
                Tag::Td => view! { <td class=class>{kids}</td> }.into_any(),
                Tag::Dl => view! { <dl class=class>{kids}</dl> }.into_any(),
                Tag::Dt => view! { <dt class=class>{kids}</dt> }.into_any(),
                Tag::Dd => view! { <dd class=class>{kids}</dd> }.into_any(),
                Tag::Blockquote => view! { <blockquote class=class>{kids}</blockquote> }.into_any(),
                Tag::Hr => view! { <hr class=class /> }.into_any(),
                Tag::Br => view! { <br /> }.into_any(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aim_proto::ui::UiEnvelope;
    use aim_proto::ui::model::Surfaces;
    use serde_json::json;

    use super::*;

    fn surface(components: Value) -> Surface {
        let components: Vec<Component> = serde_json::from_value(components).unwrap();
        let mut s = Surface::new("s", aim_proto::ui::TERMINAL_CATALOG, Placement::Transcript, 0);
        s.upsert(&components);
        s
    }

    /// A component test: every web-rendered component becomes the elements and text expected.
    #[test]
    fn components_render_to_expected_elements() {
        let s = surface(json!([
            {"id": "root", "component": "Column", "children": ["t", "c", "l", "tbl", "kv", "p", "b", "go"]},
            {"id": "t", "component": "Text", "spans": [{"text": "Hi ", "style": "bold"}, {"text": "there"}]},
            {"id": "c", "component": "Code", "language": "sh", "text": "ls"},
            {"id": "l", "component": "List", "items": ["a", "b"], "ordered": true},
            {"id": "tbl", "component": "Table", "columns": ["K"], "rows": [["v"]]},
            {"id": "kv", "component": "KeyValue", "items": [{"key": "Owner", "value": "aim"}]},
            {"id": "p", "component": "Progress", "value": 30, "max": 60, "label": "build"},
            {"id": "b", "component": "Badge", "text": "ok", "tone": "success"},
            {"id": "go", "component": "Button", "label": "Go", "action": {"name": "go"}}
        ]));
        let html = render(&s).to_html();
        assert_eq!(
            html,
            "<div class=\"ui-surface\"><div class=\"ui-stack\">\
             <div class=\"ui-plain\"><span class=\"ui-bold\">Hi </span><span class=\"ui-plain\">there</span></div>\
             <div class=\"ui-code-block\"><div class=\"ui-code-lang\">sh</div><pre class=\"ui-code\"><code class=\"\">ls</code></pre></div>\
             <ol class=\"ui-list\"><li class=\"\">a</li><li class=\"\">b</li></ol>\
             <table class=\"ui-table\"><thead class=\"\"><tr class=\"\"><th class=\"\">K</th></tr></thead><tbody class=\"\"><tr class=\"\"><td class=\"\">v</td></tr></tbody></table>\
             <dl class=\"ui-kv\"><dt class=\"\">Owner</dt><dd class=\"\">aim</dd></dl>\
             <div class=\"ui-progress\"><progress value=\"30\" max=\"60\"></progress><span class=\"\">50% build</span></div>\
             <span class=\"ui-badge ui-success\">ok</span>\
             <button class=\"ui-button\">Go</button></div></div>"
        );
        let Node::El { children, .. } = render(&s) else { panic!("a surface is a div") };
        let buttons: Vec<UiAction> = children
            .iter()
            .flat_map(|n| match n {
                Node::El { children, .. } => children.clone(),
                other => vec![other.clone()],
            })
            .filter_map(|n| match n {
                Node::Button { action, .. } => Some(action),
                _ => None,
            })
            .collect();
        assert_eq!(buttons.first().map(|a| (a.name.as_str(), a.source_component_id.as_str())), Some(("go", "go")));
    }

    /// Model text containing markup renders inert: as escaped text, never as elements or
    /// attributes, in plain text, Markdown, tables and button labels alike.
    #[test]
    fn model_markup_renders_inert() {
        fn tags(node: &Node, out: &mut Vec<&'static str>) {
            if let Node::El { tag, children, .. } = node {
                out.push(tag.name());
                for child in children {
                    tags(child, out);
                }
            }
        }
        let attack = "<script>alert(1)</script><img src=x onerror=alert(2)>";
        let s = surface(json!([
            {"id": "root", "component": "Column", "children": ["t", "m", "tbl", "go", "u"]},
            {"id": "t", "component": "Text", "text": attack},
            {"id": "m", "component": "Markdown", "text": format!("{attack}\n\n[click](javascript:alert(3)) ![x](https://evil.example/t.png)")},
            {"id": "tbl", "component": "Table", "columns": [attack], "rows": [[attack]]},
            {"id": "go", "component": "Button", "label": attack, "action": {"name": "x"}},
            {"id": "u", "component": "Unknown", "fallback": attack}
        ]));
        let html = render(&s).to_html();
        assert!(!html.contains("<script") && !html.contains("<img") && !html.contains("javascript:"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{html}");
        assert!(!html.contains("evil.example"), "images show their alt text only: {html}");
        // Every text node holds the attack verbatim; none became an element.
        let mut names = Vec::new();
        tags(&render(&s), &mut names);
        assert!(names.iter().all(|n| !["script", "img", "iframe", "a"].contains(n)), "{names:?}");
        let safe = markdown("[docs](https://example.com/x)");
        assert!(
            matches!(safe.first(), Some(Node::El { children, .. }) if matches!(children.first(), Some(Node::Link { href, .. }) if href == "https://example.com/x"))
        );
    }

    #[test]
    fn unknown_components_fall_back_and_containers_stack() {
        let s = surface(json!([
            {"id": "root", "component": "Row", "children": ["d", "spark", "gone"]},
            {"id": "d", "component": "Diff", "text": "+x", "fallback": "diff: +x"},
            {"id": "spark", "component": "Sparkline", "fallback": {"child": "alt"}},
            {"id": "alt", "component": "Text", "text": "trend"}
        ]));
        assert_eq!(render(&s).text(), "diff: +x trend …");
    }

    /// The web half of ADR 0017's `builtin_surface_tui_web_parity`: every string the shared fixture
    /// lists as visible is rendered (the TUI checks the same list).
    #[test]
    fn builtin_surface_tui_web_parity() {
        let fixture: Value = serde_json::from_str(include_str!("../../aim-proto/tests/fixtures/ui_surface.json")).unwrap();
        let mut all = Surfaces::default();
        for message in fixture["messages"].as_array().unwrap() {
            let envelope: UiEnvelope = serde_json::from_value(message.clone()).unwrap();
            all.apply(&envelope.message, 0).unwrap();
        }
        let shown = render(all.get("release").unwrap()).text();
        for visible in fixture["visible"].as_array().unwrap() {
            let visible = visible.as_str().unwrap();
            assert!(shown.contains(visible), "`{visible}` is shown: {shown}");
        }
    }
}
