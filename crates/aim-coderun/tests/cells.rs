//! Integration checks for worker cells and their callback channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aim_coderun::protocol::{CallTool, CellOutput, Execute, ExecuteCell, Output, ToolCallResult};
use aim_coderun::runtime::{CodeRuntime, QuickJsRuntime, referenced_tools};
use aim_proto::error::ErrorCode;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use aim_rpc::{NoHandler, Peer, PeerConfig, Router};
use serde_json::{Value, json};

fn spec() -> ToolSpec {
    ToolSpec {
        name: "add".to_owned(),
        description: "Adds two numbers".to_owned(),
        input_schema: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}),
        input: ToolInput::Json,
        annotations: ToolAnnotations::default(),
    }
}

async fn execute(
    code: &str,
    program_args: Option<Value>,
    output_limit: usize,
    timeout_ms: u64,
    memory_limit: usize,
) -> Result<(aim_coderun::protocol::ExecuteResult, Vec<CellOutput>), aim_proto::error::ProtoError> {
    let (worker_stream, parent_stream) = tokio::io::duplex(1 << 20);
    let (worker_read, worker_write) = tokio::io::split(worker_stream);
    let (parent_read, parent_write) = tokio::io::split(parent_stream);
    let events = Arc::new(Mutex::new(Vec::<CellOutput>::new()));
    let event_state = Arc::clone(&events);
    let parent_router = Router::new(event_state)
        .method::<CallTool, _, _>(|_, _, call| async move {
            let a = call.arguments.get("a").and_then(Value::as_i64).unwrap_or_default();
            let b = call.arguments.get("b").and_then(Value::as_i64).unwrap_or_default();
            Ok(ToolCallResult { result: ToolResult::text((a + b).to_string()) })
        })
        .notification::<Output, _, _>(|events, _, event| async move {
            let mut guard = events.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.push(event);
        });
    let worker = Peer::spawn(worker_read, worker_write, NoHandler, PeerConfig::default());
    let _parent = Peer::spawn(parent_read, parent_write, parent_router, PeerConfig::default());
    let request = Execute {
        session_id: "s".to_owned(),
        cell_id: "c".to_owned(),
        code: code.to_owned(),
        program_args,
        timeout_ms,
        memory_limit_bytes: memory_limit,
        output_limit_bytes: output_limit,
        tools: vec![spec()],
        store: HashMap::new(),
    };
    let result = QuickJsRuntime.execute(request, worker).await?;
    let snapshot = events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    Ok((result, snapshot))
}

#[tokio::test]
async fn typescript_nested_calls_and_output() {
    let (result, events) =
        execute("const x: number = 2; const r = await tools.add({a:x,b:3}); text(r.content[0].text);", None, 1024, 2_000, 16 << 20)
            .await
            .unwrap();
    assert_eq!(result.output, "5\n");
    assert_eq!(events.len(), 1);
    assert!(!events[0].immediate);
}

/// ADR 0076: a nested result's `.text` (and its string form) is its text, `Promise.all` runs
/// calls together, and the added fields stay out of the result's JSON.
#[tokio::test]
async fn nested_results_expose_their_text_and_run_together() {
    let (result, _) = execute(
        "const [a, b] = await Promise.all([tools.add({a:1,b:2}), tools.add({a:3,b:4})]); text(a.text + ',' + `${b}` + ',' + JSON.stringify(a).includes('\"text\":\"3\"') + ',' + Object.keys(a).includes('text'));",
        None,
        1024,
        2_000,
        16 << 20,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "3,7,true,false\n");
}

/// ADR 0076, from a live run where a model's cells failed silently: the last expression is the
/// cell's value (a promise chain is awaited, not dropped), `console.log` is `text`, and an
/// exception reaches the model by name and message.
#[tokio::test]
async fn scripts_written_like_node_return_their_output_and_their_errors() {
    let last = execute("const r = await tools.add({a:2,b:2});\n({ sum: Number(r.text) })", None, 1024, 2_000, 16 << 20).await.unwrap();
    assert_eq!((last.0.output.as_str(), last.0.returned), ("{\"sum\":4}", true));
    let chained = execute(
        "async function go() { return (await tools.add({a:1,b:1})).text; }\ngo().then(v => console.log('got', v))",
        None,
        1024,
        2_000,
        16 << 20,
    )
    .await
    .unwrap();
    assert_eq!(chained.0.output, "got 2\n", "the chain was awaited and console.log is text");
    let thrown =
        execute("const { tools: t } = global; await t.add({a:1,b:2}); missing.call();", None, 1024, 2_000, 16 << 20).await.unwrap_err();
    assert!(thrown.message.contains("ReferenceError") && thrown.message.contains("missing"), "{}", thrown.message);
    let node = execute("const fs = require('fs');", None, 1024, 2_000, 16 << 20).await.unwrap_err();
    assert!(node.message.contains("not Node") && node.message.contains("tools.*"), "{}", node.message);
    let returned = execute("return 7", None, 1024, 2_000, 16 << 20).await.unwrap();
    assert_eq!(returned.0.output, "7", "a top-level return still works");
}

/// A script may throw a message of any size; the worker bounds it like output (codex review B1).
#[tokio::test]
async fn a_huge_exception_is_bounded_like_output() {
    for code in ["throw new Error('x'.repeat(1_000_000));", "throw 'é'.repeat(500_000);"] {
        let error = execute(code, None, 1024, 5_000, 64 << 20).await.unwrap_err();
        assert!(error.message.len() <= 1024, "{} bytes", error.message.len());
        assert!(error.message.starts_with("Warning: truncated output"), "{}", error.message);
        assert!(error.message.contains("bytes truncated"), "{}", error.message);
    }
}

#[tokio::test]
async fn global_this_exposes_tools_and_index() {
    let (result, _) = execute(
        "const r = await globalThis.tools.add({a:19,b:23}); text(r.content[0].text + ':' + globalThis.ALL_TOOLS[0].name);",
        None,
        1024,
        2_000,
        16 << 20,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "42:add\n");
}

#[tokio::test]
async fn functions_namespace_alias_preserves_tool_authority() {
    let (result, _) = execute(
        "const a = await tools.functions.add({a:1,b:2}); const b = await tools['functions.add']({a:4,b:5}); text(Number(a.content[0].text)+Number(b.content[0].text));",
        None,
        1024,
        2_000,
        16 << 20,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "12\n");
    let denied = execute("return typeof tools.functions.unknown", None, 1024, 2_000, 16 << 20).await.unwrap().0;
    assert_eq!(denied.output, "undefined");
}

#[tokio::test]
async fn store_and_program_args_and_return() {
    let (result, _) = execute(
        "export default async function main(args: {value: number}) { store('seen', args.value); return args.value * 2; }",
        Some(json!({"value":7})),
        1024,
        2_000,
        16 << 20,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "14");
    assert_eq!(result.store.get("seen"), Some(&json!(7)));
}

#[tokio::test]
async fn output_over_the_limit_is_dropped_and_counted_not_failed() {
    let (result, events) = execute("text('ok'); text('too long'); store('kept', 1); text('x')", None, 5, 2_000, 16 << 20).await.unwrap();
    assert_eq!(result.output, "ok\n", "kept output is a prefix");
    assert_eq!((result.dropped_events, result.dropped_bytes), (2, 9));
    assert_eq!(result.store.get("kept"), Some(&json!(1)), "the store survives an output overflow (REV13a M8)");
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn empty_text_calls_are_charged_against_the_limit() {
    // REV13a M2: each empty text('') used to add an uncharged '\n', so a loop printed 1 MB.
    let code = "for (let i = 0; i < 1000000; i++) { try { text('') } catch (e) {} }";
    let (result, _) = execute(code, None, 40_000, 20_000, 16 << 20).await.unwrap();
    assert!(result.output.len() <= 40_000, "{} bytes", result.output.len());
    assert!(result.dropped_events > 900_000);
}

#[tokio::test]
async fn zero_byte_notifications_are_bounded_and_coalesced() {
    // REV13a M3: a notify('') flood grew the parent's memory without bound.
    let code = "for (let i = 0; i < 200000; i++) { notify(''); yield_control(); } text('done')";
    let (result, events) = execute(code, None, 40_000, 20_000, 16 << 20).await.unwrap();
    assert!(events.len() <= aim_coderun::budget::MAX_EVENTS, "{} events reached the parent", events.len());
    assert_eq!(events.len(), 2, "consecutive markers coalesce into one, then the text");
    assert!(result.yielded);
    assert_eq!(result.output, "done\n");
}

#[tokio::test]
async fn distinct_notifications_stop_at_the_event_cap() {
    let code = "for (let i = 0; i < 10000; i++) { notify(String(i % 10)); }";
    let (result, events) = execute(code, None, 1_000_000, 20_000, 16 << 20).await.unwrap();
    assert_eq!(events.len(), aim_coderun::budget::MAX_EVENTS);
    assert_eq!(result.dropped_events, 10_000 - u64::try_from(aim_coderun::budget::MAX_EVENTS).unwrap());
}

#[tokio::test]
async fn a_large_returned_value_is_cut_not_failed() {
    let (result, _) = execute("return 'x'.repeat(100)", None, 10, 2_000, 16 << 20).await.unwrap();
    assert_eq!(result.output, "x".repeat(10));
    assert!(result.returned);
    assert_eq!(result.dropped_bytes, 90);
}

#[tokio::test]
async fn store_is_bounded_at_store_time_and_undefined_removes_a_key() {
    let limit = aim_coderun::budget::MAX_STORE_BYTES;
    let code = format!(
        "store('a', 1); let refused = false; try {{ store('big', 'x'.repeat({limit})) }} catch (e) {{ refused = String(e.message).includes('store is limited') }} store('gone', 2); store('gone', undefined); text(refused)"
    );
    let (result, _) = execute(&code, None, 1024, 5_000, 64 << 20).await.unwrap();
    assert_eq!(result.output, "true\n");
    assert_eq!(result.store.get("a"), Some(&json!(1)));
    assert!(!result.store.contains_key("big"));
    assert!(!result.store.contains_key("gone"));
}

#[tokio::test]
async fn memory_limit_stops_allocation() {
    let err = execute("const x = Array(1000000).fill('abcdefgh'); text(x.length)", None, 1024, 2_000, 1 << 20).await.unwrap_err();
    assert!(matches!(err.code, ErrorCode::LimitExceeded | ErrorCode::Internal));
}

#[tokio::test]
async fn notify_and_yield_stream_before_completion() {
    let (result, events) = execute("notify('now'); yield_control(); text('later')", None, 1024, 2_000, 16 << 20).await.unwrap();
    assert_eq!(result.output, "later\n");
    assert!(result.yielded);
    assert_eq!(events.len(), 3);
    assert!(events.first().is_some_and(|event| event.immediate && !event.yielded && event.text == "now"));
    assert!(events.get(1).is_some_and(|event| event.yielded));
}

#[tokio::test]
async fn timeout_helpers_resume_awaited_cell() {
    let (result, _) = execute("await new Promise(resolve => setTimeout(resolve, 1)); text(42)", None, 1024, 2_000, 16 << 20).await.unwrap();
    assert_eq!(result.output, "42\n");
}

#[tokio::test]
async fn clear_timeout_cancels_callback() {
    let (result, _) = execute(
        "let fired = false; const id = setTimeout(() => { fired = true }, 1); clearTimeout(id); await new Promise(resolve => setTimeout(resolve, 5)); text(fired)",
        None,
        1024,
        2_000,
        16 << 20,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "false\n");
}

#[tokio::test]
async fn deadline_interrupts_busy_javascript() {
    let err = execute("while (true) {}", None, 1024, 20, 16 << 20).await.unwrap_err();
    assert!(matches!(err.code, ErrorCode::Timeout | ErrorCode::Internal));
}

#[tokio::test]
async fn quickjs_has_no_ambient_io_bindings() {
    let (result, _) =
        execute("return [typeof fetch, typeof require, typeof Deno, typeof process].join(',')", None, 1024, 2_000, 16 << 20).await.unwrap();
    assert_eq!(result.output, "undefined,undefined,undefined,undefined");
}

#[test]
fn extracts_literal_tool_references_from_program() {
    let refs =
        referenced_tools("export default async function main() { await tools.add({a:1,b:2}); await tools['read_file']({}); }").unwrap();
    assert_eq!(refs.into_iter().collect::<Vec<_>>(), vec!["add", "read_file"]);
}

#[tokio::test]
async fn worker_binary_speaks_bidirectional_stdio_rpc() {
    use std::process::Stdio;

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_aim-coderun"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let parent = Peer::spawn(
        stdout,
        stdin,
        Router::new(()).method::<CallTool, _, _>(|_, _, call| async move {
            assert_eq!(call.cell_id, "stdio-cell");
            Ok(ToolCallResult { result: ToolResult::text("42") })
        }),
        PeerConfig::default(),
    );
    let result = parent
        .call::<ExecuteCell>(Execute {
            session_id: "s".to_owned(),
            cell_id: "stdio-cell".to_owned(),
            code: "const r = await tools.add({a:40,b:2}); text(r.content[0].text)".to_owned(),
            program_args: None,
            timeout_ms: 2_000,
            memory_limit_bytes: 16 << 20,
            output_limit_bytes: 1024,
            tools: vec![spec()],
            store: HashMap::new(),
        })
        .await
        .unwrap();
    assert_eq!(result.output, "42\n");
    child.kill().await.unwrap();
}
