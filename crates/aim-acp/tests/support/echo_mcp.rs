//! A trivial stdio MCP server for the live aim-tools conformance test: one `echo` tool.
//!
//! Speaks the `initialize` era of MCP (what Claude Code uses) over NDJSON. When
//! `AIM_ACP_ECHO_LOG` is set it appends `start`, each method name and `call <text>` to that file,
//! which is the test's evidence that the adapter really spawned and called it (adapter issue #883).

use std::io::{BufRead as _, Write as _};

use serde_json::{Value, json};

fn log(line: &str) {
    let Some(path) = std::env::var_os("AIM_ACP_ECHO_LOG") else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        drop(writeln!(file, "{line}"));
    }
}

fn respond(id: &Value, result: Result<Value, (i64, &str)>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    }
}

fn handle(method: &str, params: &Value) -> Result<Value, (i64, &'static str)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "aim-acp-echo", "version": env!("CARGO_PKG_VERSION")}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": [{
            "name": "echo",
            "description": "Echoes the given text back unchanged.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string", "description": "Text to echo"}}, "required": ["text"]}
        }]})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let text = params.get("arguments").and_then(|a| a.get("text")).and_then(Value::as_str).unwrap_or_default();
            if name != "echo" {
                return Err((-32602, "unknown tool"));
            }
            log(&format!("call {text}"));
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": false}))
        }
        _ => Err((-32601, "method not found")),
    }
}

fn main() {
    log("start");
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        log(method);
        // Notifications (no id) get no answer.
        let Some(id) = message.get("id") else { continue };
        let reply = respond(id, handle(method, message.get("params").unwrap_or(&Value::Null)));
        if writeln!(stdout, "{reply}").and_then(|()| stdout.flush()).is_err() {
            break;
        }
    }
}
