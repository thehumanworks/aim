//! Live smoke tests of the TUI (ADR 0022): the real `aim` binary in a PTY, with a real provider
//! and the real `aimx`, through whole turns. Run with `mise run smoke`, or
//! `cargo test -p aim --test tui_live -- --ignored --nocapture live_`.
//!
//! Needs `target/<profile>/aimx` (built by `cargo test --workspace`, or `cargo build -p aimx`),
//! credentials for the provider (codex: aim's login or the Codex CLI's, read-only; openrouter:
//! `OPENROUTER_API_KEY`) and the network. Nothing is written outside a temporary directory:
//! sessions run `--ephemeral`.
#![expect(clippy::expect_used, reason = "live test driver")]
#![expect(clippy::print_stdout, reason = "reports the live run")]

mod tui_support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tui_support::{Start, Tui};

fn aimx() -> PathBuf {
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_aim")).with_file_name("aimx");
    assert!(path.exists(), "{} is missing: run `cargo build -p aimx` (same profile) first", path.display());
    path
}

/// Rows that could only come from a leaked credential.
fn assert_no_secrets(rows: &[String]) {
    for row in rows {
        for marker in ["eyJ", "Bearer ", "sk-or-", "refresh_token", "account_id"] {
            assert!(!row.contains(marker), "a row looks like it leaks a credential ({marker})");
        }
    }
}

fn wait_idle_after(tui: &Tui, needle: &str, timeout: Duration) -> Duration {
    let start = Instant::now();
    tui.wait(needle, timeout, |s| s.contains(needle) && s.contains("idle") && !s.contains("running"));
    start.elapsed()
}

/// One plain turn and one turn that uses a workspace tool, through the TUI.
fn live_turns(provider: &str, extra: &[&str]) {
    let aimx = aimx();
    let aimx = aimx.to_string_lossy().into_owned();
    let mut args = vec!["--ephemeral", "-p", provider, "--aimx", aimx.as_str()];
    args.extend_from_slice(extra);
    let tui = Tui::start_live(&Start { rows: 40, cols: 110, args: &args, ..Start::default() });
    let ready = tui.since_spawn_until(Duration::from_secs(60), |s| s.contains("idle")).expect("the session starts");

    tui.type_text("What is 17 + 25? Reply with only the number.");
    tui.send("\r");
    tui.wait("the turn to start", Duration::from_secs(30), |s| s.contains("running"));
    let first = wait_idle_after(&tui, "42", Duration::from_secs(180));

    tui.type_text("Use your tools to read README.md in this workspace and reply with its first line, verbatim.");
    tui.send("\r");
    tui.wait("the turn to start", Duration::from_secs(30), |s| s.contains("running"));
    let second = wait_idle_after(&tui, "Readme", Duration::from_secs(240));
    let status = tui.rows().into_iter().rfind(|r| r.starts_with("idle ·")).unwrap_or_default();

    let rows = tui.quit();
    let prompt = rows.iter().position(|r| r.starts_with("› What is 17 + 25?")).expect("the prompt is echoed");
    assert!(rows.iter().skip(prompt).any(|r| r.trim() == "42"), "the answer is in scrollback:\n{}", rows.join("\n"));
    let tool = rows.iter().position(|r| r.starts_with("⏺ ")).expect("a tool call row");
    assert!(rows.iter().skip(tool).any(|r| r.contains("⎿")), "the tool call has a result row");
    assert!(rows.iter().skip(tool).any(|r| r.contains("Readme")), "the file's first line came back");
    assert!(!rows.iter().any(|r| r.contains("<environment>") || r.starts_with("workspace: ")), "the environment block stays hidden");
    assert_no_secrets(&rows);
    println!(
        "live {provider}: session ready {:.0} ms after spawn · plain turn {:.1} s · tool turn {:.1} s · last status: {status}",
        ready.as_secs_f64() * 1_000.0,
        first.as_secs_f64(),
        second.as_secs_f64()
    );
    for row in rows.iter().skip(prompt).filter(|r| !r.is_empty()) {
        println!("| {row}");
    }
}

#[test]
#[ignore = "live: codex credentials, network and a built aimx"]
fn live_tui_codex_turn() {
    live_turns("codex", &["-e", "low"]);
}

#[test]
#[ignore = "live: OPENROUTER_API_KEY, network and a built aimx"]
fn live_tui_openrouter_turn() {
    live_turns("openrouter", &[]);
}

