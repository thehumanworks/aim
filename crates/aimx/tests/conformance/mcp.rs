//! The real `aimx mcp --stdio` binary, as Claude Code spawns it for `acp:claude` sessions: tool
//! annotations on the wire, and independent calls that run concurrently.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

const AIMX: &str = env!("CARGO_BIN_EXE_aimx");

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpProcess {
    fn spawn(root: &std::path::Path) -> Self {
        let mut child = tokio::process::Command::new(AIMX)
            .args(["mcp", "--stdio", "--root"])
            .arg(root)
            .env("AIMX_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    async fn send(&mut self, message: &Value) {
        self.stdin.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn reply(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(60), self.stdout.read_line(&mut line)).await.unwrap().unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn initialize(&mut self) {
        self.send(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await;
        assert_eq!(self.reply().await["id"], 0);
        self.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
    }

    async fn end(mut self) {
        drop(self.stdin);
        tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_binary_marks_only_non_mutating_tools_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcp = McpProcess::spawn(dir.path());
    mcp.initialize().await;
    mcp.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
    let list = mcp.reply().await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let read_only = |name: &str| tools.iter().find(|tool| tool["name"] == name).map(|tool| tool["annotations"]["readOnlyHint"].clone());
    for name in ["Read", "read", "Grep", "grep", "Glob", "glob", "LS", "ls"] {
        assert_eq!(read_only(name), Some(json!(true)), "{name}");
    }
    for name in ["Bash", "bash", "Edit", "edit", "Write", "write"] {
        assert_eq!(read_only(name), Some(json!(false)), "{name}");
    }
    mcp.end().await;
}

/// Two calls sent back to back were both in flight at once. This is a rendezvous, not a stopwatch:
/// each call marks its start and then waits (about 10 s) for the other's mark, so both succeed
/// only when they overlap. Run one after the other, the first waits out its limit and fails; a
/// wall-clock ceiling would instead leave a fraction of a second for a loaded machine.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_binary_runs_independent_calls_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcp = McpProcess::spawn(dir.path());
    mcp.initialize().await;
    for (id, me, other) in [(1, "a", "b"), (2, "b", "a")] {
        let command = format!(
            "touch rendezvous-{me}; for i in $(seq 1 200); do [ -f rendezvous-{other} ] && {{ echo met; exit 0; }}; sleep 0.05; done; echo alone; exit 1"
        );
        mcp.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"bash","arguments":{"command":command}}})).await;
    }
    let mut ids = Vec::new();
    for _ in 0..2 {
        let reply = mcp.reply().await;
        let met = reply["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("met"));
        assert!(
            reply["result"]["isError"] == false && met,
            "call {} never saw the other in flight (they ran one after the other): {reply}",
            reply["id"]
        );
        ids.push(reply["id"].as_i64().unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, [1, 2]);
    mcp.end().await;
}
