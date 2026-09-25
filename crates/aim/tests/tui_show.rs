//! Prints the TUI's screens at the stages of a scripted session, for eyeballing:
//! `cargo test -p aim --test tui_show -- --ignored --nocapture`.
#![expect(clippy::print_stdout, reason = "prints screens for a human to read")]

mod tui_support;

use std::time::Duration;

use serde_json::json;
use tui_support::{Start, Tui, text};

fn show(tui: &Tui, stage: &str) {
    let rows = tui.rows();
    println!("==== {stage} ====");
    for row in rows {
        println!("|{row}");
    }
}

#[test]
#[ignore = "prints screens for a human; run with --nocapture"]
fn show_screens() {
    let answer = "## Plan\n\nI'll read the **file** and then `patch` it.\n\n1. read\n2. patch\n\n> note: see [docs](https://example.com)\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
    let script = json!({"responses": [
        [{"kind": "reasoning", "text": "**Looking** at the request."}, text(answer, 12, 40)],
        [{"kind": "call", "name": "echo", "arguments": {"text": "a\nb\nc\nd\ne\nf", "delay_ms": 1500}}],
        [text("All done after steering.", 2, 20)]
    ]});
    let tui = Tui::start(&script, &Start { before: &["$ aim"], ..Start::default() });
    tui.wait_for("idle");
    show(&tui, "first paint");
    tui.type_text("explain the plan");
    tui.send("\r");
    std::thread::sleep(Duration::from_millis(250));
    show(&tui, "streaming");
    tui.wait_for("idle");
    show(&tui, "after turn 1");
    tui.type_text("now run it");
    tui.send("\r");
    tui.wait_for("running…");
    tui.type_text("also check tests");
    tui.send("\r");
    std::thread::sleep(Duration::from_millis(200));
    show(&tui, "tool running + steer queued");
    tui.wait("idle", Duration::from_secs(10), |s| s.contains("All done") && s.contains("idle"));
    show(&tui, "after steering");
    tui.send("/");
    std::thread::sleep(Duration::from_millis(200));
    show(&tui, "slash popup");
    tui.send("\x15@");
    std::thread::sleep(Duration::from_millis(300));
    show(&tui, "file popup");
    tui.send("\x15$");
    std::thread::sleep(Duration::from_millis(300));
    show(&tui, "skill popup");
    tui.send("\x15");
    let rows = tui.quit();
    println!("==== scrollback after exit ====");
    for row in rows {
        println!("|{row}");
    }
}
