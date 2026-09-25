//! Live composed plugin turn. Run with `cargo test -p aim --test plugin_live -- --ignored live_`.

use std::process::Command;
use std::time::Instant;

#[test]
#[ignore = "calls the real OpenRouter service"]
fn live_openrouter_calls_composed_kv_plugin() {
    assert!(std::env::var_os("OPENROUTER_API_KEY").is_some(), "OpenRouter credentials are required");
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let aim = std::path::Path::new(env!("CARGO_BIN_EXE_aim"));
    let aimx = aim.with_file_name("aimx");
    let plugind = aim.with_file_name("aim-plugind");
    assert!(aimx.is_file(), "build aimx before the live plugin test");
    assert!(plugind.is_file(), "build aim-plugind before the live plugin test");
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/kv_counter");
    let invoke = |args: &[&str]| {
        Command::new(aim).args(args).env("AIM_HOME", home.path()).env("AIM_AIMX", &aimx).env("AIM_PLUGIND", &plugind).output().unwrap()
    };
    let installed = invoke(&["plugin", "install", example.to_str().unwrap()]);
    assert!(installed.status.success(), "plugin install failed");
    let trusted = invoke(&["plugin", "trust", "kv_counter"]);
    assert!(trusted.status.success(), "plugin trust failed");

    let started = Instant::now();
    let result = invoke(&[
        "run",
        "--ephemeral",
        "--json",
        "-p",
        "openrouter",
        "--max-requests",
        "4",
        "-C",
        workspace.path().to_str().unwrap(),
        "Call plugin__kv_counter__increment with key live_probe exactly once. Then report the returned count.",
    ]);
    let updates: Vec<serde_json::Value> =
        String::from_utf8_lossy(&result.stdout).lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let called = updates.iter().any(|update| {
        update.get("type").and_then(serde_json::Value::as_str) == Some("tool_finished")
            && update.get("name").and_then(serde_json::Value::as_str) == Some("plugin__kv_counter__increment")
            && update.pointer("/result/is_error").and_then(serde_json::Value::as_bool) == Some(false)
    });
    eprintln!("live_openrouter_plugin_ms={}", started.elapsed().as_millis());
    assert!(result.status.success() && called, "OpenRouter turn did not finish a successful plugin call");
}
