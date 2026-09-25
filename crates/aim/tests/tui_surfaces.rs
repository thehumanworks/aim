//! Agent-authored UI surfaces in a real pseudo-terminal (ADR 0064): the `aim` binary in
//! `--script` mode, where the scripted model calls the real `ui_*` tools and the TUI renders what
//! the session publishes.

mod tui_support;

use std::time::Duration;

use serde_json::{Value, json};
use tui_support::{Start, Tui, text};

fn call(name: &str, arguments: &Value) -> Value {
    json!({"kind": "call", "name": name, "arguments": arguments})
}

fn show(surface: &str, placement: &str, components: &Value) -> Value {
    call("ui_show", &json!({"surface": surface, "placement": placement, "components": components, "data": {"done": 10}}))
}

/// Every component of `aim/terminal@1`, the placements the TUI shows, degradation to the
/// transcript, fallbacks, and an update of a pinned widget.
#[test]
fn surfaces_render_every_component_and_placement() {
    let everything = json!([
        {"id": "root", "component": "Column", "children": ["title", "md", "code", "diff", "row", "div", "box", "list", "table", "kv", "log", "spark"]},
        {"id": "title", "component": "Text", "spans": [{"text": "Release ", "style": "heading"}, {"text": "1.4", "style": "accent"}]},
        {"id": "md", "component": "Markdown", "text": "Ship **today**"},
        {"id": "code", "component": "Code", "language": "sh", "text": "cargo test"},
        {"id": "diff", "component": "Diff", "text": "-old line\n+new line"},
        {"id": "row", "component": "Row", "children": ["spin", "badge", "btn"]},
        {"id": "spin", "component": "Spinner", "label": "checking"},
        {"id": "badge", "component": "Badge", "text": "ready", "tone": "success"},
        {"id": "btn", "component": "Button", "label": "Open", "action": {"name": "open"}},
        {"id": "div", "component": "Divider", "label": "details"},
        {"id": "box", "component": "Box", "title": "Notes", "child": "note"},
        {"id": "note", "component": "Text", "text": "boxed note"},
        {"id": "list", "component": "List", "items": ["alpha", "beta"], "ordered": true},
        {"id": "table", "component": "Table", "columns": ["Crate", "Tests"], "rows": [["aim-proto", 42], ["aim", 310]]},
        {"id": "kv", "component": "KeyValue", "items": [{"key": "Owner", "value": "aim"}]},
        {"id": "log", "component": "Log", "lines": ["one", "two", "three"], "max_lines": 2},
        {"id": "spark", "component": "Sparkline", "values": [1, 2], "fallback": "trend: up"}
    ]);
    let progress = json!([{"id": "root", "component": "Progress", "value": {"path": "/done"}, "label": "build"}]);
    let script = json!({"responses": [
        [
            show("all", "transcript", &everything),
            show("w", "widget.above_editor", &progress),
            show("st", "status.right", &json!([{"id": "root", "component": "Badge", "text": "CI green", "tone": "success"}])),
            show("ov", "overlay", &json!([{"id": "root", "component": "Text", "text": "overlay degraded"}])),
            show("side", "panel.side", &json!([{"id": "root", "component": "Text", "text": "side panel text"}]))
        ],
        [call("ui_update", &json!({"surface": "w", "data": [{"path": "/done", "value": 60}]}))],
        [text("All shown.", 1, 0)]
    ]});
    let tui = Tui::start(&script, &Start { rows: 60, cols: 100, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("show everything");
    tui.send("\r");
    tui.wait("the turn to end", Duration::from_secs(20), |s| s.contains("All shown.") && s.contains("idle") && !s.contains("running"));
    let screen = tui.rows();
    assert!(screen.iter().any(|r| r.contains(" 60% build")), "the widget shows the updated value:\n{}", screen.join("\n"));
    let status = screen.iter().rfind(|r| r.starts_with("idle ·")).cloned().unwrap_or_default();
    assert!(status.ends_with("[CI green]"), "status.right: {status}");
    assert!(screen.iter().any(|r| r == "side panel text"), "panel.side is a widget inline");
    let rows = tui.quit();
    let all = rows.join("\n");
    for needle in [
        "Release 1.4",
        "Ship today",
        "cargo test",
        "-old line",
        "+new line",
        "checking",
        "[ready]",
        "[ Open ]",
        "── details ──",
        "┌─ Notes ─",
        "│ boxed note",
        "1. alpha",
        "2. beta",
        "Crate      Tests",
        "aim-proto  42",
        "Owner  aim",
        "trend: up",
        "overlay degraded",
    ] {
        assert!(all.contains(needle), "`{needle}` is in scrollback:\n{all}");
    }
    assert!(!all.contains("one\n"), "the log shows its last two lines only");
    assert!(all.contains("two") && all.contains("three"));
}

/// A dialog takes the focus; enter presses its button; the action reaches the model as user input
/// on the next turn, and the transcript shows it as an action.
#[test]
fn a_button_press_reaches_the_agent() {
    let dialog = json!([
        {"id": "root", "component": "Column", "children": ["q", "go"]},
        {"id": "q", "component": "Text", "text": "Deploy to prod?"},
        {"id": "go", "component": "Button", "label": "Deploy", "action": {"name": "deploy", "context": {"env": "prod"}}}
    ]);
    let script = json!({"responses": [
        [show("confirm", "dialog", &dialog)],
        [text("Waiting for you.", 1, 0)],
        [{"kind": "echo_input"}]
    ]});
    let tui = Tui::start(&script, &Start { rows: 30, cols: 100, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("ask me");
    tui.send("\r");
    tui.wait("the dialog", Duration::from_secs(20), |s| s.contains("Waiting for you.") && s.contains("[ Deploy ]") && s.contains("idle"));
    tui.send("\r");
    tui.wait("the agent's answer", Duration::from_secs(20), |s| s.contains("heard:") && s.contains("idle") && !s.contains("running"));
    let rows = tui.quit();
    let all = rows.join("\n");
    assert!(all.contains("⚡ deploy · confirm/go {\"env\":\"prod\"}"), "the action row:\n{all}");
    let heard = rows.iter().find(|r| r.contains("heard:")).cloned().unwrap_or_default();
    assert!(heard.contains("\"surface\":\"confirm\"") || all.contains("\"surface\":\"confirm\""), "{all}");
    assert!(all.contains("\"name\":\"deploy\"") && all.contains("\"component\":\"go\""), "the agent saw the action:\n{all}");
}

/// A surface shown in one session replays the same after switching away and attaching again.
#[test]
fn a_reattached_session_replays_its_surfaces() {
    let table = json!([{"id": "root", "component": "Table", "columns": ["File", "Lines"], "rows": [["main.rs", 12]]}]);
    let script = json!({"responses": [[show("files", "transcript", &table)], [text("Listed.", 1, 0)]]});
    let tui = Tui::start(&script, &Start { rows: 40, cols: 100, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("list files");
    tui.send("\r");
    tui.wait("the turn", Duration::from_secs(20), |s| s.contains("Listed.") && s.contains("idle") && !s.contains("running"));
    let first: Vec<String> = {
        let rows = tui.history();
        let from = rows.iter().position(|r| r.starts_with("› list files")).unwrap_or(0);
        let to = rows.iter().rposition(|r| r.contains("Listed.")).unwrap_or(0);
        rows.get(from..=to).unwrap_or_default().to_vec()
    };
    assert!(first.iter().any(|r| r.starts_with("main.rs")), "{first:?}");
    tui.send("/new\r");
    tui.wait("a new session", Duration::from_secs(10), |s| s.contains("idle") && !s.contains("starting"));
    std::thread::sleep(Duration::from_millis(200));
    tui.send("/sessions\r");
    tui.wait("the picker", Duration::from_secs(10), |s| s.contains("type to filter") && s.contains("1 turns"));
    // Newest first: the new session, then the first one.
    tui.send("\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    tui.send("\r");
    tui.wait("the replay", Duration::from_secs(10), |s| !s.contains("type to filter"));
    std::thread::sleep(Duration::from_millis(300));
    let rows = tui.quit();
    let starts: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.starts_with("› list files")).map(|(i, _)| i).collect();
    assert_eq!(starts.len(), 2, "shown live, then replayed:\n{}", rows.join("\n"));
    let replay: Vec<String> = rows.iter().skip(starts[1]).take(first.len()).cloned().collect();
    assert_eq!(replay, first, "the replay renders the same rows");
}
