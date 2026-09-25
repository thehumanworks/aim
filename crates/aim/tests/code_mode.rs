//! `AIM_CODE_MODE` (ADR 0076): what each mode offers `aim mcp`, the `acp:claude` relay
//! (`aim code-mcp`, end to end against a real local aimx and worker) and native sessions.

#![expect(clippy::expect_used, reason = "test driver fails loudly on fixture and protocol errors")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use aim::agent::ToolHost;
use aim::agent::tools::BoxFuture;
use aim::coderun::mode::Mode;
use aim::host::CodeConfig;
use aim::mcp::proxy::CodeRelay;
use aim_proto::daemon::Location;
use aim_proto::error::ProtoError;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

fn sibling(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name(name)
}

fn worker() -> PathBuf {
    std::env::var_os("AIM_CODERUN_BIN").map_or_else(|| sibling("aim-coderun"), PathBuf::from)
}

fn code(mode: Mode) -> CodeConfig {
    CodeConfig { worker: worker(), user_programs: "/missing-test-programs".into(), mode }
}

/// Two services, one of them a search tool the native compact set would hide.
struct Services;

impl ToolHost for Services {
    fn specs(&self) -> Vec<ToolSpec> {
        ["search_sessions", "board_list"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.to_owned(),
                description: format!("The {name} service"),
                input_schema: json!({"type":"object","properties":{"query":{"type":"string"}}}),
                input: ToolInput::Json,
                annotations: ToolAnnotations::default(),
            })
            .collect()
    }

    fn call(&self, name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async move { Ok(ToolResult::text(name)) })
    }
}

fn names(host: &dyn ToolHost) -> Vec<String> {
    host.specs().into_iter().map(|spec| spec.name).collect()
}

#[test]
fn aim_mcp_serves_each_mode_s_tools() {
    let home = Path::new("/missing-aim-home");
    let services = || Arc::new(Services) as Arc<dyn ToolHost>;
    let off = aim::mcp::services::with_code_mode(services(), home, None);
    assert_eq!(names(off.as_ref()), ["search_sessions", "board_list"], "off: the services alone");
    let on = aim::mcp::services::with_code_mode(services(), home, Some(code(Mode::On)));
    assert_eq!(
        names(on.as_ref()),
        ["search_sessions", "board_list", "run_code", "save_program", "run_program", "list_programs"],
        "on: every service stays direct beside the code tools"
    );
    let only = aim::mcp::services::with_code_mode(services(), home, Some(code(Mode::Only)));
    assert_eq!(names(only.as_ref()), ["run_code", "save_program", "run_program", "list_programs"], "only: the code tools alone");
    let description = only.specs().into_iter().find(|spec| spec.name == "run_code").expect("run_code").description;
    assert!(description.contains("search_sessions(args: { query?: string }): R;"), "services are typed for cells: {description}");
    assert!(description.contains("Promise.all"), "{description}");
}

/// An MCP client of the `aim code-mcp` process.
struct Relay {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    next: u64,
}

impl Relay {
    fn spawn(root: &Path, mode: Mode) -> Self {
        let relay = CodeRelay {
            root: root.to_string_lossy().into_owned(),
            location: Location::Local,
            aimx: sibling("aimx"),
            code: CodeConfig { worker: worker(), user_programs: root.join(".programs"), mode },
        };
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
            .args(relay.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("aim code-mcp starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, stdin, stdout, next: 0 }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let mut bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":self.next,"method":method,"params":params})).expect("request");
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await.expect("send");
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(60), self.stdout.read_line(&mut line)).await.expect("reply in time").expect("read");
        let reply: Value = serde_json::from_str(&line).expect("JSON reply");
        assert_eq!(reply.get("id"), Some(&json!(self.next)), "{reply}");
        reply.get("result").cloned().unwrap_or(Value::Null)
    }

    /// Sends a request without waiting for its reply.
    async fn start(&mut self, method: &str, params: Value) {
        self.next += 1;
        let mut bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":self.next,"method":method,"params":params})).expect("request");
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await.expect("send");
    }

    async fn tools(&mut self) -> Vec<String> {
        self.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}))
            .await;
        let listed = self.request("tools/list", json!({})).await;
        let tools = listed.get("tools").and_then(Value::as_array).expect("tools");
        tools.iter().map(|tool| tool.get("name").and_then(Value::as_str).unwrap_or_default().to_owned()).collect()
    }

    async fn close(mut self) {
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await.expect("relay exits").expect("status");
        assert!(status.success(), "{status}");
    }
}

/// The relay in each mode, against a real local aimx and the sandboxed worker: `on` shows the
/// compact direct set (no duplicated lower-case names), `only` the code tools alone, and one
/// `run_code` reads two files together with `Promise.all`.
#[tokio::test]
async fn the_acp_relay_serves_code_mode_over_a_real_workspace() {
    assert!(sibling("aimx").exists(), "build aimx first (`cargo build -p aimx`)");
    assert!(worker().exists(), "build the worker first (`cargo build -p aim-coderun`)");
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("a.txt"), "alpha first\nalpha second\n").expect("fixture a");
    std::fs::write(root.path().join("b.txt"), "beta first\nbeta second\n").expect("fixture b");

    let mut on = Relay::spawn(root.path(), Mode::On);
    let listed = on.tools().await;
    for name in ["Read", "Write", "Edit", "LS", "Bash", "BashOutput", "run_code", "save_program", "run_program", "list_programs"] {
        assert!(listed.iter().any(|tool| tool == name), "on shows {name}: {listed:?}");
    }
    for name in ["Glob", "Grep", "KillShell", "read", "bash"] {
        assert!(!listed.iter().any(|tool| tool == name), "on hides {name}: {listed:?}");
    }
    on.close().await;

    let mut only = Relay::spawn(root.path(), Mode::Only);
    assert_eq!(only.tools().await, ["run_code", "save_program", "run_program", "list_programs"]);
    let code = r#"const [a, b] = await Promise.all([tools.Read({file_path: "a.txt"}), tools.Read({file_path: "b.txt"})]);
const first = (r) => r.text.split("\n")[0];
text(first(a) + " | " + first(b));"#;
    let result = only.request("tools/call", json!({"name":"run_code","arguments":{"code":code}})).await;
    let text = result["content"][0]["text"].as_str().unwrap_or_default().to_owned();
    assert_eq!(result["isError"], false, "{result}");
    assert!(text.contains("alpha first") && text.contains("beta first") && !text.contains("second"), "{text}");
    let direct = only.request("tools/call", json!({"name":"Read","arguments":{"file_path":"a.txt"}})).await;
    assert_eq!(direct["isError"], true, "only: a workspace tool is not callable directly: {direct}");
    only.close().await;
}

/// Every process below `root`, from `ps` (pid → parent).
fn descendants(root: u32) -> Vec<u32> {
    let listing = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output().expect("ps");
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<u32>);
            Some((fields.next()?.ok()?, fields.next()?.ok()?))
        })
        .collect();
    let mut found = vec![root];
    let mut index = 0;
    while let Some(&parent) = found.get(index) {
        found.extend(pairs.iter().filter(|(_, ppid)| *ppid == parent).map(|(pid, _)| *pid));
        index += 1;
    }
    found.remove(0);
    found
}

fn alive(pid: u32) -> bool {
    // A zombie waiting for its reaper has ended; `ps` shows it as `Z`.
    let state = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().expect("ps");
    let state = String::from_utf8_lossy(&state.stdout);
    !state.trim().is_empty() && !state.trim().starts_with('Z')
}

/// Codex review B2: when the client closes the relay's input during a long call, the relay ends
/// the call, its cells and its aimx (with the shell aimx started) within a few seconds, instead
/// of living until the call's 630 s deadline.
#[tokio::test]
async fn closing_the_relay_s_input_ends_a_pending_call_and_its_processes() {
    assert!(sibling("aimx").exists(), "build aimx first (`cargo build -p aimx`)");
    let root = tempfile::tempdir().expect("workspace");
    let mut relay = Relay::spawn(root.path(), Mode::On);
    relay.tools().await;
    relay.start("tools/call", json!({"name":"Bash","arguments":{"command":"sleep 30 && touch survived"}})).await;
    let pid = relay.child.id().expect("relay pid");
    let mut below = Vec::new();
    for _ in 0..100 {
        below = descendants(pid);
        if below.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(below.len() >= 2, "aimx and the shell run below the relay: {below:?}");
    let started = std::time::Instant::now();
    drop(relay.stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), relay.child.wait()).await.expect("the relay exits").expect("status");
    assert!(status.success(), "{status}");
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
    for _ in 0..100 {
        if below.iter().all(|pid| !alive(*pid)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let left: Vec<u32> = below.iter().copied().filter(|pid| alive(*pid)).collect();
    assert!(left.is_empty(), "processes outlived the relay: {left:?}");
    assert!(!root.path().join("survived").exists());
}

/// Codex review N1: an invalid `AIM_CODE_MODE` is said where the user runs aim, once.
#[test]
fn an_invalid_code_mode_is_reported_on_stderr_once() {
    let home = tempfile::tempdir().expect("home");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aim"))
        .args(["run", "--ephemeral", "-p", "no-such-provider", "-C"])
        .arg(home.path())
        .arg("hello")
        .env("AIM_CODE_MODE", "onn")
        .env("AIM_HOME", home.path().join("aim"))
        .output()
        .expect("aim runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warning = "aim: AIM_CODE_MODE=\"onn\" is not off, on or only (or 0, 1, false, true); code mode is off";
    assert_eq!(stderr.matches(warning).count(), 1, "{stderr}");
}

/// A small repository with four TODO comments (and a FIXME that is not one).
fn todo_repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("repository");
    for (path, text) in [
        ("src/main.rs", "fn main() {\n    // TODO: parse command-line flags instead of hardcoding the port\n    server::start(8080);\n}\n"),
        (
            "src/net/server.rs",
            "pub fn start(port: u16) {\n    // TODO: add TLS support\n    println!(\"{port}\");\n    // TODO(alice): handle graceful shutdown on SIGTERM\n}\n",
        ),
        ("src/net/retry.rs", "pub fn retry(n: u32) -> u32 {\n    // FIXME: not exponential backoff yet\n    n.min(5)\n}\n"),
        ("docs/notes.md", "# Notes\n\n- TODO: document the configuration file format\n"),
    ] {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("directory");
        std::fs::write(file, text).expect("fixture");
    }
    root
}

const TODO_PROMPT: &str = "Find all TODO comments across the files in this repository and summarize them.";

/// Runs `aim run --json` as a process with `AIM_CODE_MODE=mode`; returns its session updates.
fn aim_run(mode: &str, args: &[&str], home: Option<&Path>) -> Vec<Value> {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_aim"));
    command.arg("run").args(args).arg("--json").arg(TODO_PROMPT).env("AIM_CODE_MODE", mode).env("AIM_CODERUN", worker());
    if let Some(home) = home {
        command.env("AIM_HOME", home);
    }
    let output = command.output().expect("aim runs");
    assert!(output.status.success(), "aim run failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// The top-level tool calls, the model requests, and the final answer of a run.
fn digest(updates: &[Value]) -> (Vec<String>, usize, String) {
    let field = |update: &Value, name: &str| update.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
    let tools = updates
        .iter()
        .filter(|update| field(update, "type") == "tool_started" && update.get("parent").is_none_or(Value::is_null))
        .map(|update| field(update, "name"))
        .collect();
    let requests = updates.iter().filter(|update| field(update, "type") == "request_started").count();
    let answer = updates
        .iter()
        .filter(|update| field(update, "type") == "item_added")
        .filter_map(|update| update.get("item"))
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("assistant"))
        .flat_map(|item| item.get("parts").and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n");
    (tools, requests, answer)
}

fn names_every_todo(answer: &str) -> bool {
    ["configuration file", "command-line", "TLS", "SIGTERM"].iter().all(|todo| answer.contains(todo))
}

/// ADR 0076 live: with `only`, an OpenRouter model finds and summarizes the TODOs through
/// `run_code` alone.
#[test]
#[ignore = "live: calls OpenRouter (needs OPENROUTER_API_KEY)"]
fn live_openrouter_code_mode_only_summarizes_todos_through_run_code() {
    let repository = todo_repository();
    let root = repository.path().to_string_lossy().into_owned();
    let updates = aim_run("only", &["--ephemeral", "-p", "openrouter", "-m", "openai/gpt-4.1-mini", "-C", &root], None);
    let (tools, requests, answer) = digest(&updates);
    println!("[live_openrouter_code_mode_only] requests {requests}, top-level tools {tools:?}");
    assert!(tools.iter().any(|tool| tool == "run_code"), "{tools:?}");
    assert!(tools.iter().all(|tool| ["run_code", "save_program", "run_program", "list_programs"].contains(&tool.as_str())), "{tools:?}");
    assert!(names_every_todo(&answer), "{answer}");
}

/// ADR 0076 live: a strict `acp:claude` session in `only` starts through aim's relay (its
/// conformance challenge reads through `run_code`) and Claude works through `run_code`.
#[test]
#[ignore = "live: runs Claude Code through claude-agent-acp and aim's code-mode relay"]
fn live_acp_claude_code_mode_only_works_through_the_relay() {
    let repository = todo_repository();
    let root = repository.path().to_string_lossy().into_owned();
    let home = tempfile::tempdir().expect("aim home");
    // The store creates a private (0700) home that does not exist yet.
    let updates = aim_run("only", &["-p", "acp:claude", "-m", "sonnet", "-C", &root], Some(&home.path().join("aim")));
    let (tools, _, answer) = digest(&updates);
    println!("[live_acp_claude_code_mode_only] tools {tools:?}");
    assert!(tools.iter().any(|tool| tool == "mcp__aim__run_code"), "{tools:?}");
    assert!(tools.iter().all(|tool| tool.starts_with("mcp__aim__")), "strict authority: {tools:?}");
    assert!(names_every_todo(&answer), "{answer}");
}
