//! Live smoke test of agent UI surfaces (ADR 0022, ADR 0064): the real `aim` binary in a PTY with
//! a real OpenRouter model and the real `aimx`. Turn one: the model calls `ui_show` to render a
//! table (with a progress bar bound to data). Turn two: it moves the progress with `ui_update`.
//! Run with `cargo test -p aim --test ui_live -- --ignored --nocapture live_`.
//!
//! Needs `OPENROUTER_API_KEY`, the network and `target/<profile>/aimx`. Sessions run `--ephemeral`
//! in a temporary directory.
#![expect(clippy::unwrap_used, reason = "live test driver")]

mod tui_support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aim_proto::tool::ToolSpec;
use tui_support::{Start, Tui};

/// A small, inexpensive tool-calling model (also used by the search live tests).
const MODEL: &str = "openai/gpt-4.1-mini";

fn aimx() -> PathBuf {
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_aim")).with_file_name("aimx");
    assert!(path.exists(), "{} is missing: run `cargo build -p aimx` (same profile) first", path.display());
    path
}

/// Bytes the UI tools add to a first OpenRouter request that already offers the workspace's tools,
/// measured with the provider's own request builder (what goes on the wire).
fn bytes_added() -> usize {
    let provider = aim_llm_openai::OpenAiProvider::new(aim_llm_openai::Profile::openrouter()).unwrap();
    let request = |tools: Vec<ToolSpec>| aim_llm::Request {
        model: MODEL.into(),
        instructions: "x".into(),
        items: Vec::new(),
        tools,
        effort: None,
        tier: None,
        cache_key: None,
        session_id: None,
        turn_id: None,
        parallel_tool_calls: true,
        max_output_tokens: None,
    };
    let size = |tools| serde_json::to_vec(&provider.request_body(&request(tools)).unwrap()).unwrap().len();
    let workspace = ToolSpec {
        name: "read_file".into(),
        description: "Read a file.".into(),
        input_schema: serde_json::json!({"type": "object"}),
        input: aim_proto::tool::ToolInput::Json,
        annotations: aim_proto::tool::ToolAnnotations::default(),
    };
    let mut with = vec![workspace.clone()];
    with.extend(aim::ui::tools::specs());
    size(with) - size(vec![workspace])
}

fn wait_idle_after(tui: &Tui, what: &str, timeout: Duration, pred: impl Fn(&str) -> bool) -> Duration {
    let start = Instant::now();
    tui.wait(what, timeout, |s| pred(s) && s.contains("idle") && !s.contains("running"));
    start.elapsed()
}

#[test]
#[ignore = "live: OPENROUTER_API_KEY, network and a built aimx"]
fn live_ui_surfaces_openrouter() {
    assert!(std::env::var_os("OPENROUTER_API_KEY").is_some_and(|k| !k.is_empty()), "OPENROUTER_API_KEY is required");
    let added = bytes_added();
    assert!(added < 800, "the UI tools add {added} bytes");
    let aimx = aimx();
    let aimx = aimx.to_string_lossy().into_owned();
    let args = ["--ephemeral", "-p", "openrouter", "-m", MODEL, "--aimx", aimx.as_str(), "--max-requests", "8"];
    let tui = Tui::start_live(&Start { rows: 50, cols: 120, args: &args, ..Start::default() });
    let ready = tui.since_spawn_until(Duration::from_secs(60), |s| s.contains("idle")).expect("the session starts");

    tui.type_text(
        "Call ui_show exactly once with surface \"langs\": a Column whose children are a Table (columns Language, Year; \
         rows Rust 2015, Go 2012, Zig 2016) and a Progress whose value is bound to the data path /progress, with data \
         {\"progress\": 20}. Then reply with the single word: shown.",
    );
    tui.send("\r");
    tui.wait("the turn to start", Duration::from_secs(30), |s| s.contains("running"));
    let first = wait_idle_after(&tui, "the table", Duration::from_secs(180), |s| s.contains("2016") && s.contains("20%"));

    tui.type_text("Call ui_update on surface \"langs\" to set the data at /progress to 60. Then reply with the single word: updated.");
    tui.send("\r");
    tui.wait("the turn to start", Duration::from_secs(30), |s| s.contains("running"));
    let second = wait_idle_after(&tui, "the progress update", Duration::from_secs(180), |s| s.contains("60%"));
    let status = tui.rows().into_iter().rfind(|r| r.starts_with("idle ·")).unwrap_or_default();

    let rows = tui.quit();
    let all = rows.join("\n");
    for needle in ["Language", "Year", "Rust", "2015", "Go", "2012", "Zig", "2016"] {
        assert!(all.contains(needle), "`{needle}` from the table is in scrollback:\n{all}");
    }
    assert!(rows.iter().any(|r| r.starts_with("⏺ ui_show")), "the ui_show call row");
    assert!(rows.iter().any(|r| r.starts_with("⏺ ui_update")), "the ui_update call row");
    assert!(rows.iter().any(|r| r.contains(" 60%")), "the updated progress is committed when the turn ends:\n{all}");
    for row in &rows {
        for marker in ["sk-or-", "Bearer ", "eyJ"] {
            assert!(!row.contains(marker), "a row looks like it leaks a credential");
        }
    }
    println!(
        "live openrouter {MODEL}: ui tools add {added} bytes to the first request · ready {:.0} ms · ui_show turn {:.1} s · \
         ui_update turn {:.1} s · last status: {status}",
        ready.as_secs_f64() * 1_000.0,
        first.as_secs_f64(),
        second.as_secs_f64()
    );
    for row in rows.iter().skip_while(|r| !r.starts_with("› Call ui_show")).filter(|r| !r.is_empty()) {
        println!("| {row}");
    }
}
