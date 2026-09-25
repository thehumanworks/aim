//! NDJSON forwarding that preserves the local client stream across an SSH channel reconnect.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::{Instant, MissedTickBehavior};

use super::conn::Connection;
use super::forward::{ForwardOptions, connect_ssh, remote_binary};
use super::quote;

const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 64 * 1024 * 1024;
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
        drop(self.child.wait().await);
    }
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Option<Vec<u8>>, String> {
    let mut frame = Vec::new();
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
            return Ok(Some(frame));
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &[u8]) -> Result<(), String> {
    writer.write_all(frame).await.map_err(|err| err.to_string())
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
    loop {
        let frame = read_frame(&mut channel.output).await?.ok_or("SSH channel closed before resume")?;
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
    pending: &[(String, Vec<u8>)],
    local_output: &mut tokio::io::Stdout,
) -> Result<Channel, String> {
    old.stop().await;
    let mut delay = Duration::from_secs(1);
    loop {
        let candidate = async {
            let connection = connect_ssh(options).await?;
            let binary = remote_binary(&connection, options).await?;
            let mut channel = Channel::open(&connection, &binary, options)?;
            if let (Some(original), Some(token)) = (original, token) {
                resume(&mut channel, original, token, local_output).await?;
            }
            for (_, frame) in pending {
                write_frame(&mut channel.input, frame).await?;
            }
            Ok::<_, String>(channel)
        }
        .await;
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
    let mut heartbeat = tokio::time::interval_at(Instant::now() + Duration::from_secs(5), Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            input = read_frame(&mut local_input) => {
                let Some(frame) = input? else { channel.input.shutdown().await.map_err(|err| err.to_string())?; return Ok(()); };
                if is_method(&frame, "initialize") { original_init = Some(frame.clone()); }
                remember(&mut pending, &frame)?;
                if write_frame(&mut channel.input, &frame).await.is_err() {
                    channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &pending, &mut local_output).await?;
                }
            }
            output = read_frame(&mut channel.output) => {
                match output {
                    Ok(Some(frame)) => {
                        if reserved_id(&frame, HEARTBEAT_ID) { continue; }
                        if let Some(id) = id(&frame) {
                            pending.retain(|(pending_id, _)| *pending_id != id);
                            if let Some(resume) = response_token(&frame) { token = Some(resume); }
                        }
                        write_frame(&mut local_output, &frame).await?;
                    }
                    Ok(None) | Err(_) => {
                        channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &pending, &mut local_output).await?;
                    }
                }
            }
            _ = heartbeat.tick(), if token.is_some() => {
                let frame = format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{HEARTBEAT_ID}\",\"method\":\"tools.list\",\"params\":{{}}}}\n");
                if write_frame(&mut channel.input, frame.as_bytes()).await.is_err() {
                    channel = reconnect(options, &mut channel, original_init.as_deref(), token.as_deref(), &pending, &mut local_output).await?;
                }
            }
        }
    }
}
