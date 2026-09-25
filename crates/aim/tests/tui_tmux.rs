//! The inline view in tmux, a terminal that rewraps rows when it narrows (the `Rewraps` model the
//! block's erase must follow; `vt100` in the PTY tests only truncates). Needs `tmux` on `PATH`:
//! `cargo test -p aim --test tui_tmux -- --ignored`.
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test driver")]

mod tui_support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;

struct Tmux {
    socket: String,
    dir: PathBuf,
}

impl Tmux {
    fn run(&self, args: &[&str]) -> String {
        let out = Command::new("tmux").args(["-L", &self.socket]).args(args).output().expect("tmux runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Scrollback and screen, soft-wrapped rows joined (`-J`), trailing spaces trimmed.
    fn capture(&self) -> Vec<String> {
        self.run(&["capture-pane", "-p", "-J", "-t", "t", "-S", "-", "-E", "-"]).lines().map(|l| l.trim_end().to_owned()).collect()
    }

    fn screen(&self) -> String {
        self.run(&["capture-pane", "-p", "-t", "t"])
    }

    fn wait(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let screen = self.screen();
            if pred(&screen) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}:\n{screen}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn keys(&self, text: &str) {
        self.run(&["send-keys", "-t", "t", "-l", text]);
    }

    fn enter(&self) {
        self.run(&["send-keys", "-t", "t", "Enter"]);
    }

    fn resize(&self, cols: u16) {
        self.run(&["resize-window", "-t", "t", "-x", &cols.to_string(), "-y", "24"]);
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ignored = Command::new("tmux").args(["-L", &self.socket, "kill-server"]).output();
        let _ignored = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(script: &serde_json::Value) -> Tmux {
    // Tests run in parallel: each gets its own directory and tmux server.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("aim-tmux-{}-{n}", std::process::id()));
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    tui_support::workspace(&workspace);
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, serde_json::to_vec(script).unwrap()).unwrap();
    let conf = dir.join("tmux.conf");
    std::fs::write(&conf, "set -g history-limit 50000\nset -g remain-on-exit on\nset -g default-terminal tmux-256color\n").unwrap();
    // `AIM_TUI_TRACE` in the test's environment passes through (the writer's erase log).
    let trace: String = ["AIM_TUI_TRACE", "AIM_TUI_TRACE_BYTES"]
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| format!("{key}={value} ")))
        .collect();
    let command = format!(
        "printf 'before-1\\nbefore-2\\n'; {trace}AIM_HOME={home} AIM_THEME=dark exec {aim} --script {script} -C {ws}",
        home = dir.join("home").display(),
        aim = env!("CARGO_BIN_EXE_aim"),
        script = script_path.display(),
        ws = workspace.display()
    );
    let tmux = Tmux { socket: format!("aim-tui-{}-{n}", std::process::id()), dir };
    let conf = conf.to_string_lossy().into_owned();
    let out = Command::new("tmux")
        .args(["-L", &tmux.socket, "-f", &conf, "new-session", "-d", "-s", "t", "-x", "80", "-y", "24", &command])
        .output()
        .expect("tmux starts");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    tmux
}

fn has_tmux() -> bool {
    Command::new("tmux").arg("-V").output().is_ok_and(|o| o.status.success())
}

fn exists(path: &Path) -> bool {
    path.exists()
}

/// Narrowing and widening mid-stream in a rewrapping terminal leaves scrollback in order, each
/// row once, and no block residue.
#[test]
#[ignore = "needs tmux"]
fn tmux_resize_mid_stream_keeps_scrollback_clean() {
    assert!(has_tmux(), "tmux is not installed");
    assert!(exists(Path::new(env!("CARGO_BIN_EXE_aim"))));
    let answer = (1..=40).map(|i| format!("line {i:02} of the answer")).collect::<Vec<_>>().join("\n\n");
    let script = json!({"responses": [[
        {"kind": "reasoning", "text": "Planning the answer."},
        {"kind": "text", "text": answer, "chunks": 40, "delay_ms": 20}
    ]]});
    let tmux = start(&script);
    tmux.wait("idle", |s| s.contains("idle"));
    tmux.keys("hello there, a prompt long enough to wrap when the window gets narrow");
    tmux.enter();
    tmux.wait("running", |s| s.contains("running"));
    std::thread::sleep(Duration::from_millis(200));
    // Resizes faster than tmux tells the application (it reflows at once, `SIGWINCH` follows).
    tmux.resize(50);
    std::thread::sleep(Duration::from_millis(250));
    tmux.resize(100);
    std::thread::sleep(Duration::from_millis(150));
    tmux.resize(44);
    tmux.wait("the turn to end", |s| s.contains("idle") && !s.contains("running"));
    // Narrow again while idle with a popup open (a taller block).
    tmux.keys("@");
    tmux.wait("the popup", |s| s.contains("src/"));
    tmux.resize(30);
    std::thread::sleep(Duration::from_millis(250));
    tmux.run(&["send-keys", "-t", "t", "Escape"]);
    tmux.run(&["send-keys", "-t", "t", "C-u"]);
    tmux.keys("/quit");
    tmux.enter();
    tmux.wait("exit", |s| s.contains("resume: aim --session") || s.contains("Pane is dead"));
    std::thread::sleep(Duration::from_millis(200));
    let rows = tmux.capture();
    let text = rows.join("\n");
    let count = |needle: &str| rows.iter().filter(|r| r.contains(needle)).count();
    let at = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle} missing:\n{text}"));
    assert!(at("before-1") < at("before-2"));
    assert!(at("before-2") < at("› hello there"));
    assert_eq!(count("› hello there"), 1, "{text}");
    let mut last = at("› hello there");
    for i in 1..=40 {
        let needle = format!("line {i:02} of the answer");
        assert_eq!(count(&needle), 1, "{needle} once:\n{text}");
        let here = at(&needle);
        assert!(here > last, "{needle} in order:\n{text}");
        last = here;
    }
    for residue in ["running", "ask anything", "scripted-model", "⧗", "src/main.rs", "──────"] {
        assert_eq!(count(residue), 0, "no `{residue}` left behind:\n{text}");
    }
}

/// `/clear` (ADR 0074) erases the old chat from the screen and from tmux's history (`CSI 3 J`),
/// and the new session works.
#[test]
#[ignore = "needs tmux"]
fn tmux_clear_purges_the_scrollback() {
    assert!(has_tmux(), "tmux is not installed");
    let answer = (1..=40).map(|i| format!("old line {i:02}")).collect::<Vec<_>>().join("\n\n");
    let script = json!({"responses": [
        [{"kind": "text", "text": answer, "chunks": 4, "delay_ms": 5}],
        [{"kind": "text", "text": "A fresh answer.", "chunks": 1, "delay_ms": 0}]
    ]});
    let tmux = start(&script);
    tmux.wait("idle", |s| s.contains("idle"));
    tmux.keys("old question");
    tmux.enter();
    tmux.wait("the old answer", |s| s.contains("old line 40") && s.contains("idle"));
    let before = tmux.capture();
    assert!(before.iter().any(|r| r.contains("old line 01")), "the old answer reached scrollback:\n{}", before.join("\n"));
    tmux.keys("/clear");
    tmux.enter();
    tmux.wait("a cleared screen", |s| !s.contains("old line") && s.contains("idle"));
    tmux.keys("new question");
    tmux.enter();
    tmux.wait("the fresh answer", |s| s.contains("A fresh answer.") && s.contains("idle"));
    let rows = tmux.capture();
    let text = rows.join("\n");
    for gone in ["before-1", "old question", "old line 01", "old line 40"] {
        assert!(!rows.iter().any(|r| r.contains(gone)), "`{gone}` is still in the history:\n{text}");
    }
    assert!(rows.iter().any(|r| r.contains("› new question")), "{text}");
}
