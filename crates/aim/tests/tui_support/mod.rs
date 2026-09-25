//! A PTY harness for the TUI: runs the real `aim` binary in `--script` mode (feature
//! `test-support`) inside a pseudo-terminal and parses its output with `vt100`, answering the
//! terminal queries a real terminal would.
#![expect(dead_code, reason = "each test binary uses a different part of the harness")]
#![expect(clippy::unwrap_used, reason = "test harness")]

use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::Value;

/// A running TUI.
pub struct Tui {
    pub parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub dir: PathBuf,
    pub workspace: PathBuf,
    pub started: Instant,
    pub output_bytes: Arc<Mutex<usize>>,
    /// Frames painted (synchronized-output begins seen).
    pub frames: Arc<Mutex<usize>>,
}

/// How to start it.
pub struct Start<'a> {
    pub rows: u16,
    pub cols: u16,
    /// Lines already on the terminal before aim starts (shell output).
    pub before: &'a [&'a str],
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    /// A terminal that answers no queries (DA1, cursor position).
    pub silent: bool,
}

impl Default for Start<'_> {
    fn default() -> Self {
        Self { rows: 24, cols: 80, before: &[], args: &[], env: &[], silent: false }
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aim-tui-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{}-{nanos}", std::process::id())
}

/// A small workspace: a few files and a skill.
pub fn workspace(root: &Path) {
    for (path, text) in [
        ("src/main.rs", "fn main() {}\n"),
        ("src/lib.rs", "pub fn lib() {}\n"),
        ("docs/guide.md", "# Guide\n"),
        ("README.md", "# Readme\n"),
        (".agents/skills/review/SKILL.md", "---\nname: review\ndescription: Review the current diff\n---\nBody.\n"),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
}

/// Answers queries found in `data` (DA1, cursor position) the way a terminal would.
fn replies(data: &[u8], parser: &vt100::Parser) -> Vec<u8> {
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(data);
    if text.contains("\x1b[c") || text.contains("\x1b[0c") {
        out.extend_from_slice(b"\x1b[?62;22c");
    }
    if text.contains("\x1b[6n") {
        let (row, col) = parser.screen().cursor_position();
        out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
    }
    out
}

impl Tui {
    /// The TUI on a scripted provider and a fake workspace (`--script`).
    pub fn start(script: &Value, start: &Start<'_>) -> Self {
        let dir = temp_dir();
        let script_path = dir.join("script.json");
        std::fs::write(&script_path, serde_json::to_vec(script).unwrap()).unwrap();
        Self::spawn(dir, Some(&script_path), start)
    }

    /// The real TUI: real providers and the real `aimx` (pass `--aimx`, provider flags in `args`).
    pub fn start_live(start: &Start<'_>) -> Self {
        Self::spawn(temp_dir(), None, start)
    }

    fn spawn(dir: PathBuf, script: Option<&Path>, start: &Start<'_>) -> Self {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        self::workspace(&workspace);
        let pty = native_pty_system().openpty(PtySize { rows: start.rows, cols: start.cols, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_aim"));
        if let Some(script) = script {
            cmd.arg("--script");
            cmd.arg(script);
        }
        cmd.arg("-C");
        cmd.arg(&workspace);
        for arg in start.args {
            cmd.arg(arg);
        }
        cmd.cwd(&workspace);
        cmd.env("AIM_HOME", dir.join("home"));
        cmd.env("TERM", "xterm-256color");
        cmd.env("AIM_THEME", "dark");
        cmd.env("AIM_TUI_REFLOW", "0");
        cmd.env_remove("NO_COLOR");
        // The pty is not the terminal running the tests: drop what identifies that terminal.
        for key in ["TERM_PROGRAM", "KITTY_WINDOW_ID", "WEZTERM_PANE", "ALACRITTY_WINDOW_ID", "TMUX", "AIM_TUI_KEYBOARD"] {
            cmd.env_remove(key);
        }
        cmd.env_remove("COLORFGBG");
        for (key, value) in start.env {
            cmd.env(key, value);
        }
        let parser = Arc::new(Mutex::new(vt100::Parser::new(start.rows, start.cols, 20_000)));
        {
            let mut p = parser.lock().unwrap();
            for line in start.before {
                p.process(format!("{line}\r\n").as_bytes());
            }
        }
        let started = Instant::now();
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pty.master.take_writer().unwrap()));
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output_bytes = Arc::new(Mutex::new(0_usize));
        let frames = Arc::new(Mutex::new(0_usize));
        let silent = start.silent;
        {
            let parser = Arc::clone(&parser);
            let writer = Arc::clone(&writer);
            let output_bytes = Arc::clone(&output_bytes);
            let frames = Arc::clone(&frames);
            std::thread::spawn(move || {
                let mut buf = vec![0_u8; 65536];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let data = buf.get(..n).unwrap_or_default();
                    *output_bytes.lock().unwrap() += n;
                    *frames.lock().unwrap() += data.windows(8).filter(|w| *w == b"\x1b[?2026h").count();
                    let answer = {
                        let mut p = parser.lock().unwrap();
                        p.process(data);
                        if silent { Vec::new() } else { replies(data, &p) }
                    };
                    if !answer.is_empty() {
                        let mut w = writer.lock().unwrap();
                        let _ignored = w.write_all(&answer).and_then(|()| w.flush());
                    }
                }
            });
        }
        Self { parser, writer, master: pty.master, child, dir, workspace, started, output_bytes, frames }
    }

    pub fn send(&self, bytes: &str) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    /// Types text one key at a time (small gaps, like a person typing fast).
    pub fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.send(&c.to_string());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Waits until the visible screen satisfies `pred`; panics with the screen after `timeout`.
    pub fn wait(&self, what: &str, timeout: Duration, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen();
            if pred(&screen) {
                return;
            }
            assert!(
                Instant::now() <= deadline,
                "timed out waiting for {what}; screen:\n{screen}\n--- history:\n{}",
                self.history().join("\n")
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Polls every 200 µs until `pred` holds; returns the elapsed time (None after `timeout`).
    pub fn time_until(&self, timeout: Duration, pred: impl Fn(&str) -> bool) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred(&self.screen()) {
                return Some(start.elapsed());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        None
    }

    /// Time from spawn until `pred` first holds (polled every 200 µs).
    pub fn since_spawn_until(&self, timeout: Duration, pred: impl Fn(&str) -> bool) -> Option<Duration> {
        while self.started.elapsed() < timeout {
            if pred(&self.screen()) {
                return Some(self.started.elapsed());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        None
    }

    pub fn wait_for(&self, needle: &str) {
        self.wait(needle, Duration::from_secs(15), |s| s.contains(needle));
    }

    /// Every row: scrollback (oldest first) then the screen, trailing spaces trimmed.
    pub fn history(&self) -> Vec<String> {
        let mut p = self.parser.lock().unwrap();
        let (_, cols) = p.screen().size();
        p.screen_mut().set_scrollback(usize::MAX);
        let total = p.screen().scrollback();
        let mut rows = Vec::new();
        for back in (1..=total).rev() {
            p.screen_mut().set_scrollback(back);
            rows.push(p.screen().rows(0, cols).next().unwrap_or_default().trim_end().to_owned());
        }
        p.screen_mut().set_scrollback(0);
        rows.extend(p.screen().rows(0, cols).map(|r| r.trim_end().to_owned()));
        rows
    }

    /// Screen rows (trailing spaces trimmed).
    pub fn rows(&self) -> Vec<String> {
        let p = self.parser.lock().unwrap();
        let (_, cols) = p.screen().size();
        p.screen().rows(0, cols).map(|r| r.trim_end().to_owned()).collect()
    }

    pub fn alternate(&self) -> bool {
        self.parser.lock().unwrap().screen().alternate_screen()
    }

    /// Resizes the pty. Like xterm, a shorter terminal scrolls rows off its top when the cursor
    /// would fall below it (`vt100` alone would cut rows off the bottom).
    pub fn resize(&self, rows: u16, cols: u16) {
        {
            let mut p = self.parser.lock().unwrap();
            let (old_rows, _) = p.screen().size();
            let (cursor_row, cursor_col) = p.screen().cursor_position();
            if rows < old_rows && cursor_row >= rows {
                let up = cursor_row - rows + 1;
                let seq = format!("\x1b[{old_rows};1H{}\x1b[{};{}H", "\n".repeat(usize::from(up)), cursor_row - up + 1, cursor_col + 1);
                p.process(seq.as_bytes());
            }
            p.screen_mut().set_size(rows, cols);
        }
        self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).unwrap();
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Resident set size of the TUI process, in KiB (`ps`).
    pub fn rss_kib(&self) -> u64 {
        let pid = self.pid().unwrap();
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }

    /// Quits with `/quit` and waits for the process to exit.
    pub fn quit(mut self) -> Vec<String> {
        self.send("\x15/quit\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "aim exited with {status:?}");
                break;
            }
            assert!(Instant::now() < deadline, "aim did not exit; screen:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        let history = self.history();
        let _ignored = std::fs::remove_dir_all(&self.dir);
        history
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ignored = self.child.kill();
    }
}

/// A script response streaming `text` in `chunks` pieces `delay_ms` apart.
pub fn text(text: &str, chunks: usize, delay_ms: u64) -> Value {
    serde_json::json!({"kind": "text", "text": text, "chunks": chunks, "delay_ms": delay_ms})
}
