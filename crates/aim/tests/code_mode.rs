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
