//! The TUI in a real pseudo-terminal (docs/adr/0015 Verification): the actual `aim` binary in
//! `--script` mode (a scripted provider and a fake workspace), its output parsed by `vt100`.

mod tui_support;

use std::time::Duration;

use serde_json::json;
use tui_support::{Start, Tui, text};

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i:02} of the answer")).collect::<Vec<_>>().join("\n\n")
}

fn count(rows: &[String], needle: &str) -> usize {
    rows.iter().filter(|r| r.contains(needle)).count()
}

/// Finished chat lands in native scrollback in order, exactly once, below what the terminal
/// showed before aim started; streaming, a resize mid-stream, a popup and an overlay leave no
/// residue in it.
#[test]
fn inline_scrollback_preserved() {
    let script = json!({"responses": [[{"kind": "reasoning", "text": "Planning the answer."}, text(&numbered(40), 40, 15)]]});
    let before = ["$ echo before", "before-1", "before-2"];
    let tui = Tui::start(&script, &Start { before: &before, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("hello there");
    tui.send("\r");
    tui.wait_for("running");
    std::thread::sleep(Duration::from_millis(150));
    tui.resize(24, 100);
    std::thread::sleep(Duration::from_millis(150));
    tui.resize(24, 90);
    tui.wait("the turn to end", Duration::from_secs(15), |s| s.contains("idle") && !s.contains("running"));
    // A completion popup, then an overlay on the alternate screen.
    tui.send("@");
    tui.wait_for("src/");
    tui.send("\x1b");
    tui.send("\x15");
    tui.send("/sessions\r");
    tui.wait("the picker", Duration::from_secs(10), |s| s.contains("type to filter"));
    tui.send("\x1b");
    tui.wait("the inline view", Duration::from_secs(10), |s| s.contains("idle") && !s.contains("type to filter"));
    let rows = tui.quit();

    let first = rows.iter().position(|r| r == "before-1").expect("pre-existing output kept");
    assert_eq!(rows.get(first + 1).map(String::as_str), Some("before-2"));
    let prompt = rows.iter().position(|r| r.starts_with("› hello there")).expect("the prompt echoed");
    assert!(prompt > first);
    assert_eq!(count(&rows, "› hello there"), 1);
    let mut last = prompt;
    for i in 1..=40 {
        let needle = format!("line {i:02} of the answer");
        assert_eq!(count(&rows, &needle), 1, "{needle} appears exactly once:\n{}", rows.join("\n"));
        let at = rows.iter().position(|r| r.contains(&needle)).unwrap();
        assert!(at > last, "{needle} in order");
        last = at;
    }
    assert_eq!(count(&rows, "Planning the answer."), 1);
    for residue in ["running", "ask anything", "type to filter", "scripted-model"] {
        assert_eq!(count(&rows, residue), 0, "no `{residue}` left in scrollback:\n{}", rows.join("\n"));
    }
}

/// The session picker borrows the alternate screen and gives the inline view back unchanged.
#[test]
fn overlay_restores_inline() {
    let script = json!({"responses": [[text("A short **answer**.", 1, 0)]]});
    let tui = Tui::start(&script, &Start { before: &["$ aim"], ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("first question");
    tui.send("\r");
    tui.wait_for("A short answer.");
    tui.wait_for("idle");
    std::thread::sleep(Duration::from_millis(100));
    let inline = tui.rows();
    tui.type_text("/sessions");
    tui.send("\r");
    tui.wait("the picker", Duration::from_secs(10), |s| s.contains("type to filter"));
    assert!(tui.alternate(), "the picker is on the alternate screen");
    tui.wait_for("1 turns  ");
    tui.send("\x1b");
    tui.wait("the inline view", Duration::from_secs(10), |s| !s.contains("type to filter"));
    assert!(!tui.alternate());
    tui.wait("the same screen", Duration::from_secs(5), |_| tui.rows() == inline);
    assert!(inline.iter().any(|r| r.starts_with("› first question")), "{inline:?}");
    tui.quit();
}

/// Fullscreen renders the same transcript rows the inline view printed into scrollback.
#[test]
fn fullscreen_transcript_parity() {
    let answer = "## Result\n\n- one `code`\n- two\n\n> quoted\n\n```rust\nfn x() {}\n```\n\nDone.";
    let script = json!({"responses": [
        [{"kind": "call", "name": "echo", "arguments": {"text": "tool output\nsecond line"}}],
        [{"kind": "reasoning", "text": "Thinking **briefly**."}, text(answer, 3, 10)]
    ]});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.type_text("show me");
    tui.send("\r");
    tui.wait_for("Done.");
    tui.wait("idle", Duration::from_secs(10), |s| s.contains("idle"));
    std::thread::sleep(Duration::from_millis(100));
    let inline: Vec<String> = {
        let rows = tui.history();
        let from = rows.iter().position(|r| r.starts_with("› show me")).unwrap();
        let to = rows.iter().rposition(|r| r.contains("Done.")).unwrap();
        rows[from..=to].to_vec()
    };
    tui.send("/fullscreen\r");
    tui.wait("fullscreen", Duration::from_secs(10), |_| tui.alternate());
    std::thread::sleep(Duration::from_millis(150));
    let full: Vec<String> = {
        let rows = tui.rows();
        let from = rows.iter().position(|r| r.starts_with("› show me")).unwrap();
        let to = rows.iter().rposition(|r| r.contains("Done.")).unwrap();
        rows[from..=to].to_vec()
    };
    assert_eq!(full, inline, "fullscreen shows the rows scrollback holds");
    assert!(inline.iter().any(|r| r.starts_with("⏺ echo tool output")), "{inline:?}");
    tui.send("/fullscreen\r");
    tui.wait("inline again", Duration::from_secs(10), |_| !tui.alternate());
    tui.quit();
}

/// A slow completion for an old query never replaces the popup of the newer one, even when the
/// broker lets it finish (only the app's generation fence stands in the way).
#[test]
fn completion_stale_result_fenced() {
    let script = json!({"responses": [], "completion_delays": [{"query": "s", "ms": 1200}], "keep_superseded": true});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.send("@s");
    std::thread::sleep(Duration::from_millis(50));
    tui.send("rc/m");
    tui.wait_for("src/main.rs");
    std::thread::sleep(Duration::from_millis(1600));
    let screen = tui.screen();
    assert!(screen.contains("src/main.rs"), "{screen}");
    assert!(!screen.contains("docs/guide.md"), "the stale `@s` answer was fenced:\n{screen}");
    tui.send("\t");
    tui.wait_for("› @src/main.rs");
    tui.quit();
}
