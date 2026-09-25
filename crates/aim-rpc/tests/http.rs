//! HTTP session bridge wire and limit tests.

use std::sync::Arc;
use std::time::Duration;
use std::{future::Future, pin::Pin};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_rpc::http::{HttpConfig, HttpConnection};
use aim_rpc::{Handler, Peer, PeerConfig, RequestCtx};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::Notify;

struct Echo {
    release: Arc<Notify>,
    entered: Arc<Notify>,
}

impl Handler for Echo {
    fn request(&self, ctx: RequestCtx, method: String, params: Value) -> Pin<Box<dyn Future<Output = Result<Value, ProtoError>> + Send>> {
        let wait = Arc::clone(&self.release);
        let entered = Arc::clone(&self.entered);
        Box::pin(async move {
            match method.as_str() {
                "hold" => {
                    entered.notify_one();
                    wait.notified().await;
                    Ok(params)
                }
                "notify" => {
                    let peer = ctx.peer.clone();
                    ctx.after_reply(move || {
                        tokio::spawn(async move {
                            drop(peer.notify_raw("exec.output", params).await);
                        });
                    })?;
                    Ok(json!({"accepted": true}))
                }
                _ => Ok(params),
            }
        })
    }
}

fn setup(config: HttpConfig) -> (HttpConnection, Peer, Arc<Notify>, Arc<Notify>) {
    let (http, reader, writer) = HttpConnection::new(config);
    let wait = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let peer = Peer::spawn(reader, writer, Echo { release: Arc::clone(&wait), entered: Arc::clone(&entered) }, PeerConfig::default());
    (http, peer, wait, entered)
}

#[tokio::test]
async fn correlates_concurrent_posts_by_id() {
    let (http, _peer, _, _) = setup(HttpConfig::default());
    let first = http.post(br#"{"jsonrpc":"2.0","id":1,"method":"echo","params":{"a":1}}"#);
    let second = http.post(br#"{"jsonrpc":"2.0","id":2,"method":"echo","params":{"b":2}}"#);
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&first).unwrap(), json!({"jsonrpc":"2.0","id":1,"result":{"a":1}}));
    assert_eq!(serde_json::from_slice::<Value>(&second).unwrap(), json!({"jsonrpc":"2.0","id":2,"result":{"b":2}}));
}

#[tokio::test]
async fn sse_subscription_gets_notifications_and_can_reconnect() {
    let (http, peer, _, _) = setup(HttpConfig::default());
    let mut first_stream = http.subscribe();
    let response = http.post(br#"{"jsonrpc":"2.0","id":1,"method":"notify","params":{"seq":1}}"#).await.unwrap().unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&response).unwrap()["result"], json!({"accepted":true}));
    let notice = tokio::time::timeout(Duration::from_secs(2), first_stream.recv()).await.unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&notice).unwrap()["params"], json!({"seq":1}));
    drop(first_stream);
    let mut resumed_stream = http.subscribe();
    peer.notify_raw("exec.output", json!({"seq":2})).await.unwrap();
    let resumed = tokio::time::timeout(Duration::from_secs(2), resumed_stream.recv()).await.unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&resumed).unwrap()["params"], json!({"seq":2}));
}

#[tokio::test]
async fn rejects_oversize_duplicate_and_excess_concurrency() {
    let config = HttpConfig { max_message_bytes: 128, max_inflight_requests: 1, ..HttpConfig::default() };
    let (http, _peer, release, entered) = setup(config);
    let oversize = vec![b' '; 129];
    assert_eq!(http.post(&oversize).await.unwrap_err().code, ErrorCode::LimitExceeded);
    let held = tokio::spawn({
        let http = http.clone();
        async move { http.post(br#"{"jsonrpc":"2.0","id":4,"method":"hold"}"#).await }
    });
    entered.notified().await;
    let extra = http.post(br#"{"jsonrpc":"2.0","id":5,"method":"echo"}"#).await.unwrap_err();
    assert_eq!(extra.code, ErrorCode::LimitExceeded);
    release.notify_one();
    assert!(held.await.unwrap().unwrap().is_some());
    assert!(http.post(br#"{"jsonrpc":"2.0","id":6,"method":"echo","params":{}}"#).await.unwrap().is_some());
}

#[tokio::test]
async fn duplicate_active_id_is_refused_without_disrupting_original_request() {
    let config = HttpConfig { max_inflight_requests: 2, ..HttpConfig::default() };
    let (http, _peer, release, entered) = setup(config);
    let original = tokio::spawn({
        let http = http.clone();
        async move { http.post(br#"{"jsonrpc":"2.0","id":"same","method":"hold"}"#).await }
    });
    entered.notified().await;
    let duplicate = http.post(br#"{"jsonrpc":"2.0","id":"same","method":"echo"}"#).await.unwrap_err();
    assert_eq!(duplicate.code, ErrorCode::Conflict);
    release.notify_one();
    assert!(original.await.unwrap().unwrap().is_some());
}

#[tokio::test]
async fn normalizes_multiline_json_and_rejects_responses() {
    let (http, _peer, _, _) = setup(HttpConfig::default());
    let response = http.post(b"{\n\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"echo\",\"params\":{}} ").await.unwrap().unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&response).unwrap()["result"], json!({}));
    assert_eq!(http.post(br#"{"jsonrpc":"2.0","id":1,"result":null}"#).await.unwrap_err().code, ErrorCode::InvalidRequest);
}

#[tokio::test]
async fn timed_out_id_stays_reserved_until_old_response() {
    let config = HttpConfig {
        response_timeout: Duration::from_millis(20),
        max_response_wait: Some(Duration::from_millis(20)),
        ..HttpConfig::default()
    };
    let (http, reader, mut writer) = HttpConnection::new(config);
    let mut lines = BufReader::new(reader).lines();
    let timed_out = tokio::spawn({
        let http = http.clone();
        async move { http.post(br#"{"jsonrpc":"2.0","id":9,"method":"hold"}"#).await }
    });
    assert!(lines.next_line().await.unwrap().is_some());
    assert_eq!(timed_out.await.unwrap().unwrap_err().code, ErrorCode::Timeout);
    let cancel = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&cancel).unwrap()["method"], "$/cancel");
    assert_eq!(http.post(br#"{"jsonrpc":"2.0","id":9,"method":"echo"}"#).await.unwrap_err().code, ErrorCode::Conflict);
    writer.write_all(br#"{"jsonrpc":"2.0","id":9,"result":null}"#).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    let retry = tokio::spawn({
        let http = http.clone();
        async move {
            loop {
                match http.post(br#"{"jsonrpc":"2.0","id":9,"method":"echo"}"#).await {
                    Err(err) if err.code == ErrorCode::Conflict => tokio::task::yield_now().await,
                    result => return result,
                }
            }
        }
    });
    let request = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&request).unwrap()["method"], "echo");
    writer.write_all(br#"{"jsonrpc":"2.0","id":9,"result":true}"#).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    assert!(retry.await.unwrap().unwrap().is_some());
}

#[tokio::test]
async fn sequenced_cancel_cannot_overtake_its_request() {
    let (http, reader, mut writer) = HttpConnection::new(HttpConfig::default());
    let mut lines = BufReader::new(reader).lines();
    let cancel = tokio::spawn({
        let http = http.clone();
        async move { http.post_sequenced(2, br#"{"jsonrpc":"2.0","method":"$/cancel","params":{"id":7}}"#).await }
    });
    tokio::task::yield_now().await;
    let request = tokio::spawn({
        let http = http.clone();
        async move { http.post_sequenced(1, br#"{"jsonrpc":"2.0","id":7,"method":"hold"}"#).await }
    });
    let first = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&first).unwrap()["method"], "hold");
    assert_eq!(serde_json::from_str::<Value>(&second).unwrap()["method"], "$/cancel");
    assert!(cancel.await.unwrap().unwrap().is_none());
    writer.write_all(br#"{"jsonrpc":"2.0","id":7,"result":null}"#).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    assert!(request.await.unwrap().unwrap().is_some());
}

#[tokio::test]
async fn missing_or_far_ahead_sequence_is_bounded() {
    let config = HttpConfig { max_inflight_requests: 2, response_timeout: Duration::from_millis(20), ..HttpConfig::default() };
    let (http, _reader, _writer) = HttpConnection::new(config);
    let frame = br#"{"jsonrpc":"2.0","method":"noop"}"#;
    assert_eq!(http.post_sequenced(5, frame).await.unwrap_err().code, ErrorCode::LimitExceeded);
    assert_eq!(http.post_sequenced(2, frame).await.unwrap_err().code, ErrorCode::Timeout);
    http.closed().await;
}

#[tokio::test]
async fn long_response_wait_is_allowed_after_bounded_admission() {
    let config = HttpConfig { response_timeout: Duration::from_millis(10), max_response_wait: None, ..HttpConfig::default() };
    let (http, reader, mut writer) = HttpConnection::new(config);
    let mut lines = BufReader::new(reader).lines();
    let call = tokio::spawn(async move { http.post(br#"{"jsonrpc":"2.0","id":11,"method":"wait"}"#).await });
    assert!(lines.next_line().await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(30)).await;
    writer.write_all(br#"{"jsonrpc":"2.0","id":11,"result":null}"#).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    assert!(call.await.unwrap().unwrap().is_some());
}

#[tokio::test]
async fn cancel_is_admitted_when_request_slots_are_full() {
    let config = HttpConfig { max_inflight_requests: 1, ..HttpConfig::default() };
    let (http, _peer, _release, entered) = setup(config);
    let held = tokio::spawn({
        let http = http.clone();
        async move { http.post_sequenced(1, br#"{"jsonrpc":"2.0","id":3,"method":"hold"}"#).await }
    });
    entered.notified().await;
    assert!(http.post_sequenced(2, br#"{"jsonrpc":"2.0","method":"$/cancel","params":{"id":3}}"#).await.unwrap().is_none());
    let response = held.await.unwrap().unwrap().unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&response).unwrap()["error"]["data"]["kind"], "cancelled");
}
