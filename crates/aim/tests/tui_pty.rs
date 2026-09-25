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
    tui.resize(24, 60);
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

/// Typing while a tool runs steers the turn: a queued chip, then the text in the transcript, and an
/// empty composer afterwards.
#[test]
fn steering_while_a_tool_runs() {
    let script = json!({"responses": [
        [{"kind": "call", "name": "echo", "arguments": {"text": "slow", "delay_ms": 1200}}],
        [text("Steered answer.", 1, 0)]
    ]});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.type_text("start");
    tui.send("\r");
    tui.wait_for("running…");
    tui.type_text("also this");
    tui.send("\r");
    tui.wait_for("⧗ queued also this");
    tui.wait("the turn to end", Duration::from_secs(15), |s| s.contains("Steered answer.") && s.contains("idle"));
    let rows = tui.quit();
    let tool = rows.iter().position(|r| r.starts_with("⏺ echo")).unwrap();
    let steer = rows.iter().position(|r| r == "› also this").unwrap();
    let answer = rows.iter().position(|r| r == "Steered answer.").unwrap();
    assert!(tool < steer && steer < answer, "{rows:?}");
    assert_eq!(count(&rows, "also this"), 1, "the chip left no trace in scrollback");
}

/// A large bracketed paste collapses into a chip in the composer and is sent in full.
#[test]
fn a_large_paste_is_a_chip_and_sends_in_full() {
    let script = json!({"responses": [[text("Got it.", 1, 0)]]});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    let pasted: Vec<String> = (1..=15).map(|n| format!("pasted row {n}")).collect();
    tui.type_text("see: ");
    tui.send(&format!("\x1b[200~{}\x1b[201~", pasted.join("\r\n")));
    tui.wait_for("› see: [pasted 15 lines]");
    tui.send("\r");
    tui.wait_for("Got it.");
    let rows = tui.quit();
    assert!(rows.iter().any(|r| r == "› see: pasted row 1"), "{rows:?}");
    assert!(rows.iter().any(|r| r == "  pasted row 15"), "every pasted row was sent and echoed");
}

/// Prompts are kept in `AIM_HOME/history` and come back with Up after a restart; ephemeral runs
/// neither read nor write it.
#[test]
fn history_survives_a_restart_unless_ephemeral() {
    let home = std::env::temp_dir().join(format!("aim-tui-history-{}", std::process::id()));
    let _fresh = std::fs::remove_dir_all(&home);
    let home_env = home.to_string_lossy().into_owned();
    let script = json!({"responses": [[text("ok", 1, 0)]]});
    let env = [("AIM_HOME", home_env.as_str())];
    let tui = Tui::start(&script, &Start { env: &env, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("remember me");
    tui.send("\r");
    tui.wait_for("ok");
    tui.quit();
    assert_eq!(std::fs::read_to_string(home.join("history")).unwrap(), "remember me\n");

    let tui = Tui::start(&script, &Start { env: &env, ..Start::default() });
    tui.wait_for("idle");
    tui.send("\x1b[A");
    tui.wait_for("› remember me");
    tui.quit();

    let tui = Tui::start(&script, &Start { env: &env, args: &["--ephemeral"], ..Start::default() });
    tui.wait_for("idle");
    tui.send("\x1b[A");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!tui.screen().contains("› remember me"), "ephemeral runs do not read history");
    tui.type_text("secret");
    tui.send("\r");
    tui.wait_for("ok");
    tui.quit();
    assert_eq!(std::fs::read_to_string(home.join("history")).unwrap(), "remember me\n", "nor write it");
    std::fs::remove_dir_all(home).unwrap();
}

/// Ctrl+C cancels a running turn; the partial answer stays, marked interrupted.
#[test]
fn ctrl_c_cancels_the_running_turn() {
    let long = (0..400).fold(String::new(), |mut text, i| {
        text.push_str("word");
        text.push_str(&i.to_string());
        text.push(' ');
        text
    });
    let script = json!({"responses": [[text(&long, 400, 10)]]});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.type_text("talk");
    tui.send("\r");
    tui.wait_for("word20");
    tui.send("\x03");
    tui.wait("the cancel", Duration::from_secs(10), |s| s.contains("· cancelled") && s.contains("idle"));
    let rows = tui.quit();
    assert!(rows.iter().any(|r| r == "(interrupted)"), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("word399")), "the stream stopped");
}

/// SIGTERM ends the TUI through its normal exit: the block is erased and the transcript stays.
#[test]
fn sigterm_restores_the_terminal() {
    let script = json!({"responses": [[text("Before the signal.", 1, 0)]]});
    let mut tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.type_text("hi");
    tui.send("\r");
    tui.wait_for("Before the signal.");
    tui.wait_for("idle");
    let pid = tui.pid().unwrap();
    let killed = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap();
    assert!(killed.success());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = tui.child.try_wait().unwrap() {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "aim did not exit on SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.exit_code(), 143);
    std::thread::sleep(Duration::from_millis(50));
    let rows = tui.history();
    assert!(rows.iter().any(|r| r == "Before the signal."), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("ask anything") || r.contains("idle ·")), "the block is gone: {rows:?}");
}

// ---- REV10 regressions ----

/// REV10 #1 (blocker): a normal (persistent) TUI attached to another client's ephemeral session
/// neither shows disk history nor writes the session's prompts to it.
#[test]
fn rev10_ephemeral_session_keeps_disk_history_out() {
    let home = std::env::temp_dir().join(format!("aim-tui-rev10-{}", std::process::id()));
    let _fresh = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("history"), "old secret\n").unwrap();
    let home_env = home.to_string_lossy().into_owned();
    let script = json!({"responses": [[text("Private answer.", 1, 0)]], "seed_ephemeral": true});
    let env = [("AIM_HOME", home_env.as_str())];
    let tui = Tui::start(&script, &Start { env: &env, ..Start::default() });
    tui.wait_for("ephemeral");
    tui.wait_for("idle");
    tui.send("\x1b[A");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!tui.screen().contains("old secret"), "disk history is hidden:\n{}", tui.screen());
    tui.type_text("private words");
    tui.send("\r");
    tui.wait_for("Private answer.");
    tui.quit();
    assert_eq!(std::fs::read_to_string(home.join("history")).unwrap(), "old secret\n", "nothing was appended");
    std::fs::remove_dir_all(home).unwrap();
}

/// REV10 #15: `@` completes from the attached session's workspace, not the launch directory.
#[test]
fn rev10_completion_follows_the_attached_workspace() {
    let other = std::env::temp_dir().join(format!("aim-tui-rev10-ws-{}", std::process::id()));
    let _fresh = std::fs::remove_dir_all(&other);
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("only_in_b.txt"), "b").unwrap();
    let workspace = other.to_string_lossy().into_owned();
    let script = json!({"responses": [], "seed_items": 2, "seed_workspace": workspace});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.send("@only");
    tui.wait("B's file", Duration::from_secs(10), |s| s.contains("only_in_b.txt"));
    tui.quit();
    std::fs::remove_dir_all(other).unwrap();
}

/// REV10 #16: a terminal that never answers the keyboard query does not stall the start or keys.
#[test]
fn rev10_a_silent_terminal_does_not_stall_startup_or_keys() {
    let script = json!({"responses": []});
    let tui = Tui::start(&script, &Start { silent: true, ..Start::default() });
    let ready = tui.since_spawn_until(Duration::from_secs(10), |s| s.contains("idle")).unwrap();
    assert!(ready < Duration::from_millis(1_500), "the session was ready after {ready:?}");
    let sent = std::time::Instant::now();
    tui.send("k");
    tui.wait("the key", Duration::from_secs(5), |s| s.contains("› k"));
    assert!(sent.elapsed() < Duration::from_millis(500), "the key painted after {:?}", sent.elapsed());
    tui.quit();
}

// ---- REV12 regressions ----

/// REV12 (residual of REV10 #1): a TUI attached to an ephemeral session never reads the history
/// file; a persistent one reads it once attached.
#[test]
fn rev12_the_history_file_is_read_only_for_persistent_sessions() {
    let home = std::env::temp_dir().join(format!("aim-tui-rev12-{}", std::process::id()));
    let _fresh = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("history"), "old secret\n").unwrap();
    let trace = home.join("trace.log");
    let (home_env, trace_env) = (home.to_string_lossy().into_owned(), trace.to_string_lossy().into_owned());
    let env = [("AIM_HOME", home_env.as_str()), ("AIM_TUI_TRACE", trace_env.as_str())];
    for (ephemeral, expect_read) in [(true, false), (false, true)] {
        let _stale = std::fs::remove_file(&trace);
        let script = json!({"responses": [], "seed_ephemeral": ephemeral});
        let tui = Tui::start(&script, &Start { env: &env, ..Start::default() });
        tui.wait_for("idle");
        std::thread::sleep(Duration::from_millis(150));
        tui.quit();
        let log = std::fs::read_to_string(&trace).unwrap_or_default();
        assert_eq!(log.contains("history loaded"), expect_read, "ephemeral={ephemeral}: {log}");
    }
    std::fs::remove_dir_all(home).unwrap();
}

/// REV12 (residual of REV10 #16): a silent terminal that claims to be kitty stalls nothing.
#[test]
fn rev12_a_silent_terminal_claiming_kitty_does_not_stall() {
    let script = json!({"responses": []});
    let env = [("TERM", "xterm-kitty")];
    let tui = Tui::start(&script, &Start { silent: true, env: &env, ..Start::default() });
    let ready = tui.since_spawn_until(Duration::from_secs(10), |s| s.contains("idle")).unwrap();
    assert!(ready < Duration::from_millis(1_500), "the session was ready after {ready:?}");
    let sent = std::time::Instant::now();
    tui.send("k");
    tui.wait("the key", Duration::from_secs(5), |s| s.contains("› k"));
    assert!(sent.elapsed() < Duration::from_millis(500), "the key painted after {:?}", sent.elapsed());
    tui.quit();
}

// ---- ADR 0074: /provider, /model and /effort completion, /clear ----

/// `/provider ` lists every provider this build knows, each with a line about it.
#[test]
fn provider_popup_lists_the_known_providers() {
    let tui = Tui::start(&json!({"responses": []}), &Start { rows: 30, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("/provider ");
    tui.wait("the provider popup", Duration::from_secs(10), |s| {
        ["openrouter", "ai-gateway", "codex", "acp:claude", "acp:claude-native"].iter().all(|id| s.contains(id))
            && s.contains("OpenRouter gateway")
    });
    tui.send("\x1b");
    tui.send("\x15");
    tui.quit();
}

/// `/model ` lists the session's catalog (hidden models left out, names as details); `/effort `
/// lists the current model's ladder and `auto`, and follows a model change.
#[test]
fn model_popup_lists_the_scripted_catalog_and_efforts_follow_the_model() {
    let script = json!({"responses": [], "catalog": [
        {"id": "scripted-model", "name": "Scripted Model", "efforts": ["low", "high"], "default_effort": "low"},
        {"id": "alpha-1", "name": "Alpha One", "efforts": ["minimal"]},
        {"id": "secret-model", "hidden": true}
    ]});
    let tui = Tui::start(&script, &Start { rows: 30, ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("/model ");
    tui.wait("the model popup", Duration::from_secs(10), |s| {
        s.contains("alpha-1") && s.contains("Alpha One") && s.contains("Scripted Model")
    });
    assert!(!tui.screen().contains("secret-model"), "hidden models are not offered:\n{}", tui.screen());
    tui.send("\x1b");
    tui.send("\x15");
    tui.type_text("/effort ");
    tui.wait("the effort popup", Duration::from_secs(10), |s| s.contains("high") && s.contains("auto") && s.contains("default"));
    tui.send("\x1b");
    tui.send("\x15");
    tui.type_text("/model alpha-1");
    tui.wait_for("Alpha One");
    // Enter takes the highlighted value and runs the command.
    tui.send("\r");
    tui.wait_for("idle · alpha-1");
    tui.type_text("/effort ");
    tui.wait("alpha-1's ladder", Duration::from_secs(10), |s| s.contains("minimal") && s.contains("auto"));
    let screen = tui.screen();
    let popup: Vec<&str> = screen.lines().filter(|l| l.contains("minimal") || l.contains(" high")).collect();
    assert!(!popup.iter().any(|l| l.contains(" high")), "the old model's ladder is gone:\n{screen}");
    tui.send("\x1b");
    tui.send("\x15");
    tui.quit();
}

/// `/clear` wipes the visible screen of the old chat and starts a new session that works.
#[test]
fn clear_wipes_the_screen_and_starts_a_new_session() {
    let script = json!({"responses": [[text("An old answer.", 1, 0)], [text("A fresh answer.", 1, 0)]]});
    let tui = Tui::start(&script, &Start { before: &["$ aim"], ..Start::default() });
    tui.wait_for("idle");
    tui.type_text("old question");
    tui.send("\r");
    tui.wait_for("An old answer.");
    tui.wait_for("idle");
    tui.type_text("/clear");
    tui.send("\r");
    tui.wait("a cleared screen", Duration::from_secs(10), |s| {
        !s.contains("old question") && !s.contains("An old answer.") && !s.contains("$ aim") && s.contains("idle")
    });
    tui.type_text("new question");
    tui.send("\r");
    tui.wait_for("A fresh answer.");
    let screen = tui.screen();
    assert!(!screen.contains("An old answer."), "{screen}");
    tui.quit();
}

/// `/clear` in fullscreen clears the canvas, and the inline view it returns to holds none of the
/// old chat either.
#[test]
fn clear_in_fullscreen_leaves_no_old_rows_on_either_screen() {
    let script = json!({"responses": [[text("An old answer.", 1, 0)]]});
    let tui = Tui::start(&script, &Start::default());
    tui.wait_for("idle");
    tui.type_text("old question");
    tui.send("\r");
    tui.wait_for("An old answer.");
    tui.wait_for("idle");
    tui.send("/fullscreen\r");
    tui.wait("fullscreen", Duration::from_secs(10), |_| tui.alternate());
    tui.wait_for("An old answer.");
    tui.type_text("/clear");
    tui.send("\r");
    tui.wait("a cleared canvas", Duration::from_secs(10), |s| !s.contains("An old answer.") && s.contains("idle"));
    assert!(tui.alternate(), "still fullscreen");
    tui.send("/fullscreen\r");
    tui.wait("inline again", Duration::from_secs(10), |_| !tui.alternate());
    tui.wait_for("idle");
    let screen = tui.screen();
    assert!(!screen.contains("old question") && !screen.contains("An old answer."), "{screen}");
    tui.quit();
}

/// T4c (ADR 0076): the status line shows the code mode the session got from a host whose own mode
/// is off: `--code-mode on`, then this shell's `AIM_CODE_MODE`; an invalid value is a warning in
/// the transcript and fails closed to off.
#[test]
fn the_status_line_shows_the_code_mode_the_client_asked_for() {
    let script = json!({"responses": [], "code_worker": true});
    let tui = Tui::start(&script, &Start { cols: 120, args: &["--code-mode", "on"], ..Start::default() });
    tui.wait_for("code:on");
    tui.quit();
    let tui = Tui::start(&script, &Start { cols: 120, env: &[("AIM_CODE_MODE", "1")], ..Start::default() });
    tui.wait_for("code:on");
    tui.quit();
    let tui = Tui::start(&script, &Start { cols: 120, env: &[("AIM_CODE_MODE", "onn")], ..Start::default() });
    tui.wait_for("code:off");
    tui.wait_for("is not off, on or only");
    tui.quit();
}
