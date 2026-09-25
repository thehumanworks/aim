//! Configured slash commands in the real TUI, including their prompt-history boundary.

mod tui_support;

use serde_json::json;
use tui_support::{Start, Tui, text};

#[test]
fn configured_commands_route_output_in_a_terminal() {
    let home = tempfile::tempdir().unwrap();
    let settings = json!({
        "plain": true,
        "status": [],
        "commands": {
            "private": {"description": "Private checklist", "output": "user", "text": "LOCAL ONLY $ARGUMENTS"},
            "review": {"description": "Review changes", "output": "agent", "text": "Review $ARGUMENTS"}
        }
    });
    std::fs::write(home.path().join("tui.json"), settings.to_string()).unwrap();
    let script = json!({"responses": [[text("FIRST MODEL RESPONSE", 1, 0)]]});
    let tui = Tui::start(&script, &Start { env: &[("AIM_HOME", home.path().to_str().unwrap())], ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("/private my checklist");
    tui.send("\r");
    tui.wait_for("LOCAL ONLY my checklist");
    assert!(!home.path().join("history").exists(), "user-only commands do not create prompt history");
    tui.type_text("/status");
    tui.send("\r");
    tui.wait_for("Codex subscription limits are available in a Codex session.");
    assert!(!tui.rows().join("\n").contains("FIRST MODEL RESPONSE"), "neither command starts a model turn");
    tui.type_text("/review this change");
    tui.send("\r");
    tui.wait_for("FIRST MODEL RESPONSE");
    let rows = tui.quit();
    assert!(rows.iter().any(|r| r.contains("Review this change")));
    let history = std::fs::read_to_string(home.path().join("history")).unwrap();
    assert!(history.contains("Review this change"));
    assert!(!history.contains("private") && !history.contains("LOCAL ONLY") && !history.contains("/status"));
}
