//! NDJSON forwarding that preserves the local client stream across an SSH channel reconnect.

use std::process::Stdio;
use std::time::Duration;

use aim_proto::error::ErrorCode;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::{Instant, MissedTickBehavior};

use super::conn::Connection;
use super::forward::{ForwardOptions, connect_ssh, remote_binary};
use super::quote;

const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 64 * 1024 * 1024;
const WRITE_DEADLINE: Duration = Duration::from_secs(60);
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(20);
const RESUME_ID: &str = "__aimx_ssh_reconnect__";
const HEARTBEAT_ID: &str = "__aimx_ssh_heartbeat__";

struct Channel {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Channel {
    fn open(connection: &Connection, binary: &str, options: &ForwardOptions) -> Result<Self, String> {
        let root = options.root.to_str().ok_or("remote root is not UTF-8")?;
        let script = format!("exec {} proxy --root {} --idle-secs {}", quote(binary), quote(root), options.idle.as_secs());
        let mut command = connection.command(&script, false);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| err.to_string())?;
        let input = child.stdin.take().ok_or("SSH channel has no stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("SSH channel has no stdout")?);
        Ok(Self { child, input, output })
    }

    async fn stop(&mut self) {
        drop(self.child.start_kill());
        drop(tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await);
    }
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>, frame: &mut Vec<u8>) -> Result<Option<Vec<u8>>, String> {
    loop {
        let buffer = reader.fill_buf().await.map_err(|err| err.to_string())?;
        if buffer.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Err("SSH frame ended mid-message".to_owned()) };
        }
        let take = buffer.iter().position(|byte| *byte == b'\n').map_or(buffer.len(), |position| position + 1);
        if frame.len().saturating_add(take) > MAX_FRAME {
            return Err("SSH frame exceeds 16 MiB".to_owned());
        }
        frame.extend_from_slice(buffer.get(..take).unwrap_or(&[]));
        reader.consume(take);
        if frame.last() == Some(&b'\n') {
            return Ok(Some(std::mem::take(frame)));
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &[u8]) -> Result<(), String> {
    tokio::time::timeout(WRITE_DEADLINE, writer.write_all(frame))
        .await
        .map_err(|_| "SSH frame write timed out".to_owned())?
        .map_err(|err| err.to_string())
}

fn message(frame: &[u8]) -> Option<Value> {
    serde_json::from_slice(frame).ok()
}

fn id(frame: &[u8]) -> Option<String> {
    message(frame)?.get("id").filter(|id| !id.is_null()).map(Value::to_string)
}

fn is_method(frame: &[u8], name: &str) -> bool {
    message(frame).and_then(|value| value.get("method").and_then(Value::as_str).map(|method| method == name)).unwrap_or(false)
}

fn response_token(frame: &[u8]) -> Option<String> {
    message(frame)?.get("result")?.get("resume_token")?.as_str().map(str::to_owned)
}

fn reserved_id(frame: &[u8], expected: &str) -> bool {
    message(frame).and_then(|value| value.get("id").and_then(Value::as_str).map(|id| id == expected)).unwrap_or(false)
}

fn forbidden_client_id(frame: &[u8]) -> bool {
    reserved_id(frame, RESUME_ID) || reserved_id(frame, HEARTBEAT_ID)
}

fn replayable(frame: &[u8]) -> bool {
    let Some(value) = message(frame) else { return false };
    let Some(method) = value.get("method").and_then(Value::as_str) else { return false };
    if method == "initialize" {
        return true;
    }
    let keyed = matches!(
        method,
        "fs.write" | "fs.edit" | "fs.mkdir" | "fs.remove" | "fs.rename" | "fs.copy" | "exec.spawn" | "exec.write_stdin" | "tools.call"
    );
    if keyed {
        return value.get("params").and_then(|params| params.get("idempotency_key")).is_some_and(|key| !key.is_null());
    }
    matches!(
        method,
        "workspace.open"
            | "fs.stat"
            | "fs.read"
            | "fs.list"
            | "fs.read_many"
            | "exec.wait"
            | "exec.read"
            | "search.grep"
            | "search.glob"
            | "tools.list"
    )
}

async fn local_error(local_output: &mut tokio::io::Stdout, id: &str, code: ErrorCode, message: &str) -> Result<(), String> {
    let id = serde_json::from_str::<Value>(id).map_err(|err| err.to_string())?;
    let mut frame = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": id,
        "error": {"code": code.number(), "message": message}
    }))
    .map_err(|err| err.to_string())?;
    frame.push(b'\n');
    write_frame(local_output, &frame).await
}

fn remember(pending: &mut Vec<(String, Vec<u8>)>, frame: &[u8]) -> Result<(), String> {
    if let Some(id) = id(frame) {
        pending.push((id, frame.to_vec()));
        if pending.iter().map(|(_, frame)| frame.len()).sum::<usize>() > MAX_PENDING {
            return Err("too many unanswered SSH requests".to_owned());
        }
    }
    Ok(())
}

async fn resume(channel: &mut Channel, original: &[u8], token: &str, local_output: &mut tokio::io::Stdout) -> Result<(), String> {
    let mut init = message(original).ok_or("missing original initialize request")?;
    let object = init.as_object_mut().ok_or("invalid initialize envelope")?;
    object.insert("id".to_owned(), json!(RESUME_ID));
    let params = object.get_mut("params").and_then(Value::as_object_mut).ok_or("initialize has no params")?;
    params.insert("resume".to_owned(), json!(token));
    let mut frame = serde_json::to_vec(&init).map_err(|err| err.to_string())?;
    frame.push(b'\n');
    write_frame(&mut channel.input, &frame).await?;
    let mut partial = Vec::new();
    loop {
        let frame = tokio::time::timeout(HEARTBEAT_DEADLINE, read_frame(&mut channel.output, &mut partial))
            .await
            .map_err(|_| "SSH resume timed out".to_owned())??
            .ok_or("SSH channel closed before resume")?;
        if reserved_id(&frame, RESUME_ID) {
            let result = message(&frame).ok_or("invalid resume response")?;
            if result.get("result").and_then(|result| result.get("resumed")).and_then(Value::as_bool) == Some(true) {
                return Ok(());
            }
            return Err("remote session could not resume".to_owned());
        }
        write_frame(local_output, &frame).await?;
    }
}

async fn reconnect(
    options: &ForwardOptions,
    old: &mut Channel,
    original: Option<&[u8]>,
    token: Option<&str>,
    pending: &mut Vec<(String, Vec<u8>)>,
    local_output: &mut tokio::io::Stdout,
) -> Result<Channel, String> {
    old.stop().await;
    let mut replay = Vec::new();
    for (id, frame) in pending.drain(..) {
        if replayable(&frame) && !(token.is_some() && is_method(&frame, "initialize")) {
            replay.push((id, frame));
        } else if !is_method(&frame, "initialize") {
            local_error(local_output, &id, ErrorCode::UnknownOutcome, "SSH connection dropped; request outcome is unknown").await?;
        }
    }
    *pending = replay;
    let mut delay = Duration::from_secs(1);
    loop {
        let candidate = tokio::time::timeout(Duration::from_secs(120), async {
            let connection = connect_ssh(options).await?;
            let binary = remote_binary(&connection, options).await?;
            let mut channel = Channel::open(&connection, &binary, options)?;
            if let (Some(original), Some(token)) = (original, token) {
                resume(&mut channel, original, token, local_output).await?;
            }
            for (_, frame) in pending.iter() {
                write_frame(&mut channel.input, frame).await?;
            }
            Ok::<_, String>(channel)
        })
        .await
        .unwrap_or_else(|_| Err("SSH reconnect attempt timed out".to_owned()));
        match candidate {
            Ok(channel) => return Ok(channel),
            Err(err) => tracing::warn!(%err, delay_secs = delay.as_secs(), "SSH reconnect failed; retrying"),
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2).min(Duration::from_secs(120));
    }
}

/// Keep aim's one stdio stream alive while replacing dropped SSH channels.
pub(super) async fn relay(options: &ForwardOptions, connection: Connection, binary: String) -> Result<(), String> {
    let mut channel = Channel::open(&connection, &binary, options)?;
    let mut local_input = BufReader::new(tokio::io::stdin());
    let mut local_output = tokio::io::stdout();
    let mut pending = Vec::<(String, Vec<u8>)>::new();
    let mut original_init = None::<Vec<u8>>;
    let mut token = None::<String>;
    let mut local_partial = Vec::new();
    let mut remote_partial = Vec::new();
    let mut last_heartbeat = Instant::now();
    let mut heartbeat = tokio::time::interval_at(Instant::now() + Duration::from_secs(5), Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            input = read_frame(&mut local_input, &mut local_partial) => {
                let Some(frame) = input? else { channel.input.shutdown().await.map_err(|err| err.to_string())?; return Ok(()); };
                if forbidden_client_id(&frame) {
                    if let Some(id) = id(&frame) {
                        local_error(&mut local_output, &id, ErrorCode::InvalidRequest, "request id is reserved for the SSH relay").await?;
                    }
                    continue;
                }
                if is_method(&frame, "initialize") {
                    original_init = Some(frame.clone());
                    last_heartbeat = Instant::now();
                }
                remember(&mut pending, &frame)?;
                if write_frame(&mut channel.input, &frame).await.is_err() {
                    channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &mut pending, &mut local_output).await?;
                    remote_partial.clear();
                    last_heartbeat = Instant::now();
                }
            }
            output = read_frame(&mut channel.output, &mut remote_partial) => {
                if let Ok(Some(frame)) = output {
                    if reserved_id(&frame, HEARTBEAT_ID) {
                        last_heartbeat = Instant::now();
                        continue;
                    }
                    if let Some(id) = id(&frame) {
                        pending.retain(|(pending_id, _)| *pending_id != id);
                        if let Some(resume) = response_token(&frame) {
                            token = Some(resume);
                            last_heartbeat = Instant::now();
                        }
                    }
                    write_frame(&mut local_output, &frame).await?;
                } else {
                    channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &mut pending, &mut local_output).await?;
                    remote_partial.clear();
                    last_heartbeat = Instant::now();
                }
            }
            _ = heartbeat.tick() => {
                if token.is_none() {
                    if original_init.is_some() && last_heartbeat.elapsed() > HEARTBEAT_DEADLINE {
                        channel = reconnect(options, &mut channel, original_init.as_deref(), None, &mut pending, &mut local_output).await?;
                        remote_partial.clear();
                        last_heartbeat = Instant::now();
                    }
                    continue;
                }
                if last_heartbeat.elapsed() > HEARTBEAT_DEADLINE {
                    channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &mut pending, &mut local_output).await?;
                    remote_partial.clear();
                    last_heartbeat = Instant::now();
                    continue;
                }
                let frame = format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{HEARTBEAT_ID}\",\"method\":\"tools.list\",\"params\":{{}}}}\n");
                if write_frame(&mut channel.input, frame.as_bytes()).await.is_err() {
                    channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &mut pending, &mut local_output).await?;
                    remote_partial.clear();
                    last_heartbeat = Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt as _, BufReader};

    use super::{HEARTBEAT_ID, RESUME_ID, forbidden_client_id, read_frame, replayable, reserved_id};

    #[test]
    fn only_idempotent_requests_are_replayed() {
        assert!(replayable(br#"{"id":1,"method":"fs.read","params":{}}"#));
        assert!(replayable(br#"{"id":2,"method":"fs.write","params":{"idempotency_key":"once"}}"#));
        assert!(!replayable(br#"{"id":3,"method":"exec.signal","params":{"idempotency_key":"ignored"}}"#));
        assert!(!replayable(br#"{"id":4,"method":"exec.release","params":{}}"#));
        assert!(!replayable(br#"{"id":5,"method":"fs.write","params":{}}"#));
    }

    #[test]
    fn internal_ids_are_distinguishable() {
        let heartbeat = format!(r#"{{"id":"{HEARTBEAT_ID}"}}"#);
        assert!(reserved_id(heartbeat.as_bytes(), HEARTBEAT_ID));
        assert!(!reserved_id(heartbeat.as_bytes(), RESUME_ID));
        assert!(forbidden_client_id(heartbeat.as_bytes()));
        assert!(forbidden_client_id(format!(r#"{{"id":"{RESUME_ID}"}}"#).as_bytes()));
        assert!(!forbidden_client_id(br#"{"id":"ordinary"}"#));
    }

    #[tokio::test]
    async fn cancelled_frame_read_keeps_partial_bytes() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut reader = BufReader::new(reader);
        let mut partial = Vec::new();
        writer.write_all(br#"{"id":1,"result":{}"#).await.expect("first fragment");
        tokio::select! {
            result = read_frame(&mut reader, &mut partial) => panic!("unexpected complete frame: {result:?}"),
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
        writer.write_all(b"}\n").await.expect("second fragment");
        let frame = read_frame(&mut reader, &mut partial).await.expect("valid frame").expect("frame");
        assert_eq!(frame, b"{\"id\":1,\"result\":{}}\n");
    }
}
