//! The real `aimx mcp --stdio` binary, as Claude Code spawns it for `acp:claude` sessions: tool
//! annotations on the wire, and independent calls that run concurrently.

use std::process::Stdio;
use std::time::{Duration, Instant};

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
        tokio::time::timeout(Duration::from_secs(10), self.stdout.read_line(&mut line)).await.unwrap().unwrap();
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

/// Two `sleep 1` calls sent back to back finish in under 1.8 s: run one after the other they
/// would need at least 2 s. The margin (0.8 s over the 1 s floor) absorbs process start-up on a
/// loaded machine.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_binary_runs_independent_calls_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcp = McpProcess::spawn(dir.path());
    mcp.initialize().await;
    let start = Instant::now();
    for id in 1..=2 {
        mcp.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"bash","arguments":{"command":"sleep 1"}}})).await;
    }
    let mut ids = Vec::new();
    for _ in 0..2 {
        let reply = mcp.reply().await;
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        ids.push(reply["id"].as_i64().unwrap());
    }
    let elapsed = start.elapsed();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2]);
    assert!(elapsed >= Duration::from_secs(1), "the calls did run: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1800), "two 1 s calls took {elapsed:?}: they ran one after the other");
    mcp.end().await;
}
