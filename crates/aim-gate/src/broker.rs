//! Gate-owned, single-port OpenRouter proxy for a confined proposal.
//!
//! The candidate only knows a fixed sentinel credential. The real key is installed in an
//! outbound request after request validation and never enters a child environment or argv.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

/// A nonsecret value placed in the candidate's `OPENROUTER_API_KEY`.
pub const SENTINEL_KEY: &str = "gate-broker";
const MAX_BODY: usize = 64 * 1024;
const MAX_HEADERS: usize = 16 * 1024;
const MAX_OUTPUT_TOKENS: u64 = 2048;
const MAX_SSE_LINE: usize = 256 * 1024;
const MICROS_PER_CENT: u64 = 10_000;
// OpenRouter's provider.max_price is in USD per million tokens. At these limits, each
// input byte reserves one micro-USD and each output token reserves two micro-USD.
const INPUT_MICROS_PER_BYTE: u64 = 1;
const OUTPUT_MICROS_PER_TOKEN: u64 = 2;
// Accounts for model framing and provider-side tokens absent from the JSON body.
const TOKEN_OVERHEAD_RESERVE: u64 = 4096;

/// Operator-owned broker configuration. `api_key` must only be read by the trusted gate.
pub struct BrokerConfig {
    /// Real OpenRouter credential, held in gate memory.
    pub api_key: String,
    /// Exact operator-pinned model id permitted for this proposal.
    pub model: String,
    /// Exact upstream chat-completions URL. Production accepts only OpenRouter HTTPS.
    pub upstream_url: String,
    /// Whole-proposal budget in cents; zero disables paid access, 50 is the hard ceiling.
    pub max_cents: u32,
}

struct Budget {
    remaining_micro: u64,
    used_micro: u64,
    billed_micro: u64,
    requests: u32,
    failed_closed: bool,
}

/// Whole-proposal usage. An incomplete response keeps its admission reserve because its
/// eventual upstream charge cannot be inferred from a missing or partial usage frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrokerUsage {
    /// Number of paid requests admitted to the upstream.
    pub requests: u32,
    /// Sum of validated upstream `usage.cost` values, in millionths of a US dollar.
    pub billed_micro_usd: u64,
    /// Conservative reserve still held for attempts without confirmed usage.
    pub reserved_micro_usd: u64,
}

/// A fixed, gate-owned listener. Dropping it stops accepting new requests.
pub struct Broker {
    port: u16,
    budget: Arc<Mutex<Budget>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Broker {
    /// Bind a single numeric loopback port and start proxying in a trusted gate thread.
    ///
    /// # Errors
    /// Disabled/invalid configuration, unavailable port, or HTTP client construction failure.
    pub fn start(config: BrokerConfig) -> Result<Self> {
        ensure!((1..=50).contains(&config.max_cents), "paid proposal cap must be 1..=50 cents");
        ensure!(
            !config.model.is_empty()
                && config.model.len() <= 128
                && config.model.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')),
            "invalid pinned proposal model"
        );
        ensure!(!config.api_key.is_empty() && !config.api_key.contains(['\r', '\n']), "invalid OpenRouter credential");
        valid_upstream(&config.upstream_url)?;
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let budget = Arc::new(Mutex::new(Budget {
            remaining_micro: u64::from(config.max_cents) * MICROS_PER_CENT,
            used_micro: 0,
            billed_micro: 0,
            requests: 0,
            failed_closed: false,
        }));
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_budget = Arc::clone(&budget);
        let thread_stopping = Arc::clone(&stopping);
        let worker = thread::spawn(move || {
            while !thread_stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _result = handle(stream, &client, &config, &thread_budget);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        });
        Ok(Self { port, budget, stopping, worker: Some(worker) })
    }

    /// Exact port granted by the proposal Seatbelt profile.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Loopback provider base URL given to the candidate.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/api/v1", self.port)
    }

    /// Conservative billed-or-reserved amount rounded up to cents. Failed requests retain
    /// their full reservation because upstream billing cannot always be determined.
    #[must_use]
    pub fn spent_cents(&self) -> u32 {
        let budget = self.budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        u32::try_from(budget.used_micro.div_ceil(MICROS_PER_CENT)).unwrap_or(u32::MAX)
    }

    /// Metered usage and any remaining conservative reserve.
    #[must_use]
    pub fn usage(&self) -> BrokerUsage {
        let budget = self.budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        BrokerUsage {
            requests: budget.requests,
            billed_micro_usd: budget.billed_micro,
            reserved_micro_usd: budget.used_micro.saturating_sub(budget.billed_micro),
        }
    }

    /// Stop the listener after the current request, if any, has completed.
    ///
    /// # Errors
    /// The worker panicked before accounting completed.
    pub fn stop(&mut self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| anyhow::anyhow!("broker worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _result = self.shutdown();
    }
}

fn valid_upstream(url: &str) -> Result<()> {
    if url == "https://openrouter.ai/api/v1/chat/completions" {
        return Ok(());
    }
    #[cfg(test)]
    {
        let parsed = reqwest::Url::parse(url)?;
        if parsed.scheme() == "http"
            && parsed.host_str() == Some("127.0.0.1")
            && parsed.path() == "/api/v1/chat/completions"
            && parsed.query().is_none()
            && parsed.fragment().is_none()
        {
            return Ok(());
        }
    }
    anyhow::bail!("upstream must be the pinned OpenRouter HTTPS chat endpoint")
}

fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        ensure!(header.len() < MAX_HEADERS, "request headers too large");
        stream.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let text = std::str::from_utf8(&header)?;
    let mut lines = text.split("\r\n");
    ensure!(lines.next() == Some("POST /api/v1/chat/completions HTTP/1.1"), "unsupported request route");
    let mut length = None;
    let mut auth = false;
    let mut content_type = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or_else(|| anyhow::anyhow!("malformed request header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "duplicate content length");
            length = Some(value.parse::<usize>()?);
        } else if name.eq_ignore_ascii_case("authorization") {
            ensure!(!auth && value == format!("Bearer {SENTINEL_KEY}"), "invalid proxy credential");
            auth = true;
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = value.eq_ignore_ascii_case("application/json");
        } else if name.eq_ignore_ascii_case("transfer-encoding") || name.eq_ignore_ascii_case("expect") {
            anyhow::bail!("unsupported request framing");
        }
    }
    let length = length.ok_or_else(|| anyhow::anyhow!("missing content length"))?;
    ensure!(auth && content_type && length > 0 && length <= MAX_BODY, "invalid or oversized request");
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn prohibited_media(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, child)| {
            matches!(key.as_str(), "image_url" | "input_audio" | "audio" | "file" | "video" | "cache_control") || prohibited_media(child)
        }),
        Value::Array(array) => array.iter().any(prohibited_media),
        _ => false,
    }
}

fn outbound_body(body: &[u8], model: &str) -> Result<(Vec<u8>, u64)> {
    let mut value: Value = serde_json::from_slice(body)?;
    let map = value.as_object_mut().ok_or_else(|| anyhow::anyhow!("request must be an object"))?;
    ensure!(map.get("model").and_then(Value::as_str) == Some(model), "proposal model is not permitted");
    ensure!(map.get("stream") == Some(&Value::Bool(true)), "proposal must stream");
    ensure!(map.get("stream_options").and_then(|value| value.get("include_usage")) == Some(&Value::Bool(true)), "stream usage is required");
    let output = match map.get("max_tokens") {
        Some(value) => value.as_u64().ok_or_else(|| anyhow::anyhow!("max_tokens must be an integer"))?,
        None => MAX_OUTPUT_TOKENS,
    };
    ensure!((1..=MAX_OUTPUT_TOKENS).contains(&output), "max_tokens exceeds proposal cap");
    ensure!(map.get("messages").and_then(Value::as_array).is_some_and(|messages| !messages.is_empty()), "messages are required");
    ensure!(
        map.keys().all(|key| matches!(
            key.as_str(),
            "model" | "messages" | "stream" | "stream_options" | "max_tokens" | "tools" | "parallel_tool_calls" | "cache_control"
        )),
        "unsupported proposal option"
    );
    map.remove("cache_control");
    map.insert("max_tokens".into(), json!(output));
    ensure!(!value.get("messages").is_some_and(prohibited_media), "paid proposal accepts text only");
    let map = value.as_object_mut().ok_or_else(|| anyhow::anyhow!("request must be an object"))?;
    map.insert("provider".into(), json!({"max_price": {"prompt": 1, "completion": 2}}));
    let outbound = serde_json::to_vec(&value)?;
    ensure!(outbound.len() <= MAX_BODY, "outbound request too large");
    // Reserve the raw inbound body length, even if serialization removes whitespace. This is
    // at least as conservative as tokenizing the outbound text at one token per input byte.
    let input_bytes = body.len().max(outbound.len()) as u64;
    let reserve = (input_bytes + TOKEN_OVERHEAD_RESERVE) * INPUT_MICROS_PER_BYTE + output * OUTPUT_MICROS_PER_TOKEN;
    Ok((outbound, reserve))
}

fn reply(stream: &mut TcpStream, status: &str) -> Result<()> {
    let body = b"gate broker rejected request\n";
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    Ok(())
}

fn chunk(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    write!(stream, "{:X}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()?;
    Ok(())
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite nonnegative cost is bounded to 0.5 USD before conversion to microdollars"
)]
fn usage_micro(line: &[u8]) -> Option<u64> {
    let data = line.strip_prefix(b"data: ")?;
    let value: Value = serde_json::from_slice(data).ok()?;
    let usage = value.get("usage")?;
    usage.get("prompt_tokens")?.as_u64()?;
    usage.get("completion_tokens")?.as_u64()?;
    let cost = usage.get("cost")?.as_f64()?;
    if !cost.is_finite() || cost < 0.0 || cost > 0.5 {
        return None;
    }
    Some((cost * 1_000_000.0).ceil() as u64)
}

fn relay(mut upstream: impl std::io::Read, stream: &mut TcpStream) -> Result<u64> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
    let mut buf = [0; 8192];
    let mut line = Vec::new();
    let mut cost = None;
    let mut done = false;
    loop {
        let count = upstream.read(&mut buf)?;
        if count == 0 {
            break;
        }
        for byte in buf.get(..count).context("upstream read exceeded buffer")? {
            line.push(*byte);
            ensure!(line.len() <= MAX_SSE_LINE, "upstream SSE line too large");
            if *byte == b'\n' {
                let no_lf = line.as_slice().strip_suffix(b"\n").unwrap_or(&line);
                let trimmed = no_lf.strip_suffix(b"\r").unwrap_or(no_lf);
                if trimmed == b"data: [DONE]" {
                    ensure!(cost.is_some(), "upstream omitted billable usage");
                    done = true;
                } else if trimmed.starts_with(b"data: ")
                    && trimmed != b"data: [DONE]"
                    && let Some(micro) = usage_micro(trimmed)
                {
                    cost = Some(micro);
                }
                chunk(stream, &line)?;
                line.clear();
            }
        }
    }
    ensure!(done && line.is_empty(), "upstream stream ended without usage and DONE");
    chunk(stream, b"")?;
    cost.context("DONE requires usage")
}

fn handle(mut stream: TcpStream, client: &reqwest::blocking::Client, config: &BrokerConfig, budget: &Mutex<Budget>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let Ok(body) = read_request(&mut stream).and_then(|body| outbound_body(&body, &config.model)) else {
        let _reply = reply(&mut stream, "400 Bad Request");
        return Ok(());
    };
    let (body, reserve) = body;
    {
        let mut state = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.failed_closed || state.remaining_micro < reserve {
            drop(state);
            let _reply = reply(&mut stream, "429 Too Many Requests");
            return Ok(());
        }
        state.remaining_micro -= reserve;
        state.used_micro += reserve;
        state.requests = state.requests.saturating_add(1);
    }
    let response = client
        .post(&config.upstream_url)
        .bearer_auth(&config.api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send();
    let actual = match response {
        Ok(response) if response.status().is_success() => relay(response, &mut stream),
        _ => {
            let _reply = reply(&mut stream, "502 Bad Gateway");
            return Ok(());
        }
    };
    let mut state = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match actual {
        Ok(actual) if actual <= reserve => {
            state.used_micro -= reserve - actual;
            state.remaining_micro += reserve - actual;
            state.billed_micro += actual;
        }
        Ok(actual) => {
            state.used_micro = state.used_micro - reserve + actual;
            state.billed_micro += actual;
            state.failed_closed = true;
        }
        Err(_) => state.failed_closed = true,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MODEL: &str = "openai/gpt-4.1-mini";

    fn fake_upstream() -> (String, JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/api/v1/chat/completions", listener.local_addr().unwrap().port());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let text = String::from_utf8(header).unwrap();
            let length = text
                .lines()
                .find_map(|line| line.strip_prefix("content-length: ").or_else(|| line.strip_prefix("Content-Length: ")))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            let safe = text.contains("Bearer upstream-secret")
                && !text.contains(SENTINEL_KEY)
                && parsed["provider"]["max_price"] == json!({"prompt":1,"completion":2})
                && parsed["model"] == TEST_MODEL
                && parsed["max_tokens"] == MAX_OUTPUT_TOKENS;
            let sse = b"data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"id\":\"r\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"cost\":0.00002}}\n\ndata: [DONE]\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                sse.len()
            )
            .unwrap();
            stream.write_all(sse).unwrap();
            safe
        });
        (url, worker)
    }

    fn call(port: u16, body: &Value) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let data = serde_json::to_vec(body).unwrap();
        write!(stream, "POST /api/v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {SENTINEL_KEY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len()).unwrap();
        stream.write_all(&data).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn streams_usage_and_substitutes_credential() {
        let (upstream_url, worker) = fake_upstream();
        let mut broker =
            Broker::start(BrokerConfig { api_key: "upstream-secret".into(), model: TEST_MODEL.into(), upstream_url, max_cents: 1 })
                .unwrap();
        let body = json!({"model": TEST_MODEL, "stream": true, "stream_options": {"include_usage": true},
            "messages": [{"role":"user","content":"hello"}], "cache_control": {"type":"ephemeral"}});
        let result = call(broker.port(), &body);
        assert!(result.contains("200 OK") && result.contains("data: [DONE]"));
        broker.stop().unwrap();
        assert_eq!(broker.spent_cents(), 1);
        assert_eq!(broker.usage(), BrokerUsage { requests: 1, billed_micro_usd: 20, reserved_micro_usd: 0 });
        assert!(worker.join().unwrap());
    }

    #[test]
    fn rejects_expensive_or_unbounded_request_before_upstream() {
        let broker = Broker::start(BrokerConfig {
            api_key: "upstream-secret".into(),
            model: TEST_MODEL.into(),
            upstream_url: "http://127.0.0.1:9/api/v1/chat/completions".into(),
            max_cents: 1,
        })
        .unwrap();
        let mut body = json!({"model": TEST_MODEL, "stream": true, "stream_options": {"include_usage": true}, "max_tokens": 2048,
            "messages": [{"role":"user","content":"x".repeat(20_000)}]});
        assert!(call(broker.port(), &body).contains("429 Too Many Requests"));
        body["max_tokens"] = json!(2049);
        assert!(call(broker.port(), &body).contains("400 Bad Request"));
        body["max_tokens"] = json!(1);
        body["model"] = json!("openrouter/auto");
        assert!(call(broker.port(), &body).contains("400 Bad Request"));
        assert_eq!(broker.spent_cents(), 0);
    }

    #[test]
    fn missing_usage_closes_budget_after_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_url = format!("http://127.0.0.1:{}/api/v1/chat/completions", listener.local_addr().unwrap().port());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut headers = Vec::new();
            let mut byte = [0];
            while !headers.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let text = String::from_utf8(headers).unwrap();
            let length = text
                .lines()
                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_owned))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let sse = b"data: {\"id\":\"r\",\"choices\":[]}\n\ndata: [DONE]\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                sse.len()
            )
            .unwrap();
            stream.write_all(sse).unwrap();
        });
        let mut broker =
            Broker::start(BrokerConfig { api_key: "upstream-secret".into(), model: TEST_MODEL.into(), upstream_url, max_cents: 1 })
                .unwrap();
        let body = json!({"model": TEST_MODEL, "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role":"user","content":"hello"}]});
        let first = call(broker.port(), &body);
        assert!(!first.contains("data: [DONE]"));
        broker.stop().unwrap();
        assert_eq!(broker.spent_cents(), 1);
        assert_eq!(broker.usage().requests, 1);
        assert_eq!(broker.usage().billed_micro_usd, 0);
        assert!(broker.usage().reserved_micro_usd > 0);
        worker.join().unwrap();
    }
}
