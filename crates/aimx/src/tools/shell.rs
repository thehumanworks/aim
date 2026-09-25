//! `Bash`, `BashOutput` and `KillShell`.
//!
//! Commands run with `bash -c` in the workspace root (falling back to the `sh` of
//! `Command::Shell` when the host has no bash), in their own process group, killed at the timeout.
//! Each call starts a fresh shell: `cd` and exported variables do not carry over to the next call,
//! and a foreground call releases its process when it returns, which kills its whole process group
//! (jobs it started with `&` included); a long-lived job needs `run_in_background`.
//! Output beyond a budget keeps its head and tail; the process is then kept (not released) and its
//! id returned as the result's `handle`, so the full retained output stays readable with
//! `exec.read` or `BashOutput`.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::time::Duration;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{Command, ExitStatus, Signal};
use aim_proto::ids::{OutputHandle, ProcId};
use aim_proto::tool::ToolResult;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolCtx, model_error, parse};
use crate::workspace::{Exec, Outcome, SpawnSpec};

/// Default command timeout.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Longest allowed timeout.
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Output bytes kept from the start and from the end of a long output.
const HEAD_BYTES: usize = 15_000;
const TAIL_BYTES: usize = 15_000;
/// Bytes pulled per read.
const READ_BYTES: u64 = 1024 * 1024;

pub(super) fn bash_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "The command to run"},
            "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "description": "Timeout in milliseconds"},
            "run_in_background": {"type": "boolean", "description": "Start it and return its id at once"}
        },
        "required": ["command"]
    })
}

pub(super) fn id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "Id returned by Bash with run_in_background"}
        },
        "required": ["id"]
    })
}

/// Keeps the first and last bytes of a stream, counting everything.
struct HeadTail {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl HeadTail {
    fn new() -> Self {
        Self { head: Vec::new(), tail: VecDeque::new(), total: 0 }
    }

    fn push(&mut self, data: &[u8]) {
        self.total = self.total.saturating_add(data.len());
        let room = HEAD_BYTES.saturating_sub(self.head.len()).min(data.len());
        let (to_head, rest) = data.split_at(room);
        self.head.extend_from_slice(to_head);
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
    }

    fn truncated(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head);
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = String::from_utf8_lossy(&tail);
        if self.truncated() {
            let omitted = self.total - self.head.len() - self.tail.len();
            format!("{head}\n… [{omitted} bytes omitted] …\n{tail}")
        } else {
            format!("{head}{tail}")
        }
    }
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
}

fn exec_of(ctx: &ToolCtx) -> Outcome<&dyn Exec> {
    ctx.workspace.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))
}

/// Spawns `command` with bash, or with the backend's shell when bash is missing.
async fn spawn(ctx: &ToolCtx, exec: &dyn Exec, command: &str, timeout: Option<Duration>) -> Outcome<ProcId> {
    let env = BTreeMap::new();
    let root = ctx.grant.root().to_owned();
    let bash = Command::Argv { argv: vec!["bash".to_owned(), "-c".to_owned(), command.to_owned()] };
    let key = ctx.derived_key("spawn");
    let spec = SpawnSpec { command: &bash, cwd: &root, env: &env, pty: None, stdin: false, timeout, key: &key };
    match exec.spawn(spec).await {
        Err(err) if err.code == ErrorCode::NotFound && err.message.contains("`bash`") => {
            let shell = Command::Shell { script: command.to_owned() };
            let key = ctx.derived_key("spawn-sh");
            exec.spawn(SpawnSpec { command: &shell, cwd: &root, env: &env, pty: None, stdin: false, timeout, key: &key }).await
        }
        other => other,
    }
}

fn describe_exit(exit: ExitStatus, timeout_ms: u64) -> Option<String> {
    match exit {
        ExitStatus::Exited { code: 0 } => None,
        ExitStatus::Exited { code } => Some(format!("[exit code {code}]")),
        ExitStatus::Signaled { signal } => Some(format!("[terminated by signal {signal}]")),
        ExitStatus::TimedOut => Some(format!("[timed out after {timeout_ms} ms; the command was killed]")),
    }
}

async fn release_cancelled(ctx: &ToolCtx, exec: &dyn Exec, proc: &ProcId) {
    match exec.release(proc).await {
        Ok(()) => {
            ctx.procs.remove(proc);
        }
        Err(err) => {
            tracing::debug!(%err, "releasing a cancelled command failed");
        }
    }
}

pub(super) async fn bash(ctx: &ToolCtx, arguments: Value) -> Result<Outcome<ToolResult>, ProtoError> {
    let args: BashArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(Ok(result)),
    };
    if let Err(err) = ctx.grant.exec() {
        return Ok(Err(err));
    }
    let exec = match exec_of(ctx) {
        Ok(exec) => exec,
        Err(err) => return Ok(Err(err)),
    };
    if ctx.cancelled.is_cancelled() {
        return Err(ProtoError::new(ErrorCode::Cancelled, "Bash request cancelled"));
    }
    let slot = ctx.procs.reserve_bounded(ctx.max_processes)?;
    let outcome: Outcome<ToolResult> = async {
        if args.run_in_background {
            let proc = match spawn(ctx, exec, &args.command, None).await {
                Ok(proc) => proc,
                Err(err) => return model_error(err),
            };
            ctx.procs.insert_at(proc.clone(), ctx.workspace_id.clone(), ctx.grant.root().to_owned(), &ctx.grant, slot);
            if ctx.cancelled.is_cancelled() {
                release_cancelled(ctx, exec, &proc).await;
                return Err(ProtoError::new(ErrorCode::Cancelled, "Bash request cancelled"));
            }
            return Ok(ToolResult::text(format!(
                "Started in the background with id {proc}. Read its output with BashOutput; stop it with KillShell."
            )));
        }
        let timeout_ms = args.timeout.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS);
        let proc = match spawn(ctx, exec, &args.command, Some(Duration::from_millis(timeout_ms))).await {
            Ok(proc) => proc,
            Err(err) => return model_error(err),
        };
        ctx.procs.insert_at(proc.clone(), ctx.workspace_id.clone(), ctx.grant.root().to_owned(), &ctx.grant, slot);
        let mut output = HeadTail::new();
        let mut cursor = 0u64;
        let mut dropped = false;
        let exit = loop {
            let read = tokio::select! {
                read = exec.read(&proc, cursor, READ_BYTES, Duration::from_secs(5)) => read,
                () = ctx.cancelled.cancelled() => {
                    release_cancelled(ctx, exec, &proc).await;
                    return Err(ProtoError::new(ErrorCode::Cancelled, "Bash request cancelled"));
                }
            }?;
            dropped |= read.dropped_before.is_some();
            for chunk in read.chunks {
                cursor = chunk.seq;
                output.push(&chunk.data.into_bytes());
            }
            if let Some(exit) = read.exit {
                break exit;
            }
        };
        let mut text = output.render();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if let Some(status) = describe_exit(exit, timeout_ms) {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&status);
        }
        let is_error = !matches!(exit, ExitStatus::Exited { code: 0 });
        if output.truncated() || dropped {
            let _ = write!(text, "\n[output was {} bytes; the full output is kept as handle {proc} (BashOutput/exec.read)]", output.total);
            return Ok(ToolResult { is_error, truncated: true, handle: Some(OutputHandle::new(proc.as_str())), ..ToolResult::text(text) });
        }
        ctx.procs.remove(&proc);
        if let Err(err) = exec.release(&proc).await {
            tracing::debug!(%err, "releasing a finished command failed");
        }
        Ok(ToolResult { is_error, ..ToolResult::text(text) })
    }
    .await;
    Ok(outcome)
}

#[derive(Deserialize)]
struct IdArgs {
    #[serde(alias = "bash_id", alias = "shell_id")]
    id: String,
}

fn owned(ctx: &ToolCtx, id: &str) -> Option<ProcId> {
    let proc = ProcId::new(id);
    (ctx.procs.workspace(&proc).as_ref() == Some(&ctx.workspace_id)).then_some(proc)
}

pub(super) async fn bash_output(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: IdArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let Some(proc) = owned(ctx, &args.id) else {
        return Ok(ToolResult::error(format!("no background command with id {}", args.id)));
    };
    ctx.procs.authorize(&proc, &ctx.grant)?;
    let exec = exec_of(ctx)?;
    let cursor = ctx.procs.cursor(&proc).unwrap_or(0);
    let read = match exec.read(&proc, cursor, ((HEAD_BYTES + TAIL_BYTES) as u64).min(ctx.max_read_bytes), Duration::ZERO).await {
        Ok(read) => read,
        Err(err) => return model_error(err),
    };
    let mut output = HeadTail::new();
    let mut last = cursor;
    for chunk in read.chunks {
        last = chunk.seq;
        output.push(&chunk.data.into_bytes());
    }
    ctx.procs.set_cursor(&proc, last);
    let mut text = output.render();
    if read.dropped_before.is_some() {
        text.insert_str(0, "[earlier output was dropped from the buffer]\n");
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    match read.exit {
        Some(exit) => {
            let status = describe_exit(exit, 0).unwrap_or_else(|| "[exit code 0]".to_owned());
            let _ = write!(text, "[finished] {status}");
        }
        None => text.push_str("[running]"),
    }
    Ok(ToolResult::text(text))
}

pub(super) async fn kill_shell(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: IdArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let Some(proc) = owned(ctx, &args.id) else {
        return Ok(ToolResult::error(format!("no background command with id {}", args.id)));
    };
    ctx.procs.authorize(&proc, &ctx.grant)?;
    let exec = exec_of(ctx)?;
    if let Err(err) = exec.signal(&proc, Signal::Kill).await {
        tracing::debug!(%err, "signalling a background command failed");
    }
    match exec.release(&proc).await {
        Ok(()) => {
            ctx.procs.remove(&proc);
            Ok(ToolResult::text(format!("Killed {proc}")))
        }
        Err(err) => {
            if err.code == ErrorCode::NotFound {
                ctx.procs.remove(&proc);
            }
            model_error(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_keeps_both_ends() {
        let mut out = HeadTail::new();
        out.push(b"small");
        assert!(!out.truncated());
        assert_eq!(out.render(), "small");
        let mut out = HeadTail::new();
        out.push(&vec![b'a'; HEAD_BYTES]);
        out.push(&[b'b'; 10]);
        out.push(&vec![b'c'; TAIL_BYTES]);
        assert!(out.truncated());
        let text = out.render();
        assert!(text.starts_with('a'));
        assert!(text.ends_with('c'));
        assert!(text.contains("[10 bytes omitted]"));
    }
}
