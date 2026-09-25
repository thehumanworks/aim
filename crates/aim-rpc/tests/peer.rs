//! Behaviour of the JSON-RPC peer over an in-process duplex stream.
#![expect(clippy::unwrap_used, reason = "test fixtures and assertions use known-good values")]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim_proto::error::ErrorCode;
use aim_proto::{method, notification};
use aim_rpc::{NoHandler, Peer, PeerConfig, Router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        self.0.push_str(field.name());
        let _written = write!(self.0, "{value:?}");
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Parameters of the test `add` method.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddParams {
    /// Left operand.
    pub a: u32,
    /// Right operand.
    pub b: u32,
}

method!(
    /// Adds two numbers.
    Add = "test.add" (AddParams) -> u32
);
method!(
    /// Sleeps unless cancelled.
    Sleep = "test.sleep" (u64) -> ()
);
method!(
    /// Echoes a string (served by the client).
    Echo = "test.echo" (String) -> String
);
method!(
    /// Not served by anyone.
    Missing = "test.missing" (()) -> ()
);
method!(
    /// The server calls `Echo` back on the client.
    AskBack = "test.ask_back" (()) -> String
);
method!(
    /// Returns more bytes than a constrained peer allows.
    LargeReply = "test.large_reply" (()) -> String
);
notification!(
    /// Adds to the server's ping counter.
    Ping = "test.ping" (u32)
);
notification!(
    /// Holds a notification handler open for admission testing.
    SlowNotice = "test.slow_notice" (())
);
notification!(
    /// One of two notification methods used to test connection-wide order.
    OrderedOutput = "test.ordered_output" (u32)
);
notification!(
    /// The second notification method in the ordered sequence.
    OrderedUpdate = "test.ordered_update" (u32)
);

#[derive(Default)]
struct OrderState {
    seen: Mutex<Vec<u32>>,
    complete: tokio::sync::Notify,
}

#[derive(Default)]
struct BlockingState {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

async fn record_order(state: Arc<OrderState>, sequence: u32) {
    if sequence == 0 {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut seen = state.seen.lock().unwrap();
    seen.push(sequence);
    if seen.len() == 32 {
        state.complete.notify_one();
    }
}

struct DropFlag {
    state: Arc<State>,
    armed: bool,
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        if self.armed {
            self.state.sleep_cancelled.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Default)]
struct State {
    pings: AtomicU32,
    sleep_cancelled: AtomicBool,
}

fn server_router() -> Router<State> {
    Router::new(State::default())
        .method::<Add, _, _>(|_, _, p| async move { Ok(p.a + p.b) })
        .method::<Sleep, _, _>(|state, _, ms| async move {
            // Records whether the handler future was dropped before finishing.
            let mut guard = DropFlag { state, armed: true };
            tokio::time::sleep(Duration::from_millis(ms)).await;
            guard.armed = false;
            Ok(())
        })
        .method::<AskBack, _, _>(|_, ctx, ()| async move { ctx.peer.call::<Echo>("from server".to_owned()).await })
        .method::<LargeReply, _, _>(|_, _, ()| async move { Ok("x".repeat(300)) })
        .notification::<Ping, _, _>(|state, _, n| async move {
            state.pings.fetch_add(n, Ordering::SeqCst);
        })
}

fn client_router() -> Router<()> {
    Router::new(()).method::<Echo, _, _>(|_, _, s| async move { Ok(format!("client echoes: {s}")) })
}

/// A connected (client, server) pair plus the server's state.
fn pair() -> (Peer, Peer, Arc<State>) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let router = server_router();
    let state = Arc::clone(router.state());
    let server = Peer::spawn(br, bw, router, PeerConfig::default());
    let client = Peer::spawn(ar, aw, client_router(), PeerConfig::default());
    (client, server, state)
}

#[tokio::test]
async fn typed_call_round_trips() {
    let (client, _server, _) = pair();
    assert_eq!(client.call::<Add>(AddParams { a: 2, b: 40 }).await.unwrap(), 42);
}

#[tokio::test]
async fn unknown_methods_and_bad_params_are_typed_errors() {
    let (client, _server, _) = pair();
    assert_eq!(client.call::<Missing>(()).await.unwrap_err().code, ErrorCode::MethodNotFound);
    let err = client.call_raw("test.add", serde_json::json!({"a": "not a number"})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
}

#[tokio::test]
async fn notifications_are_delivered() {
    let (client, _server, state) = pair();
    client.notify::<Ping>(3).await.unwrap();
    client.notify::<Ping>(4).await.unwrap();
    // A call after the notifications proves they were read (the reader is sequential).
    client.call::<Add>(AddParams { a: 0, b: 0 }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(state.pings.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn rapid_notifications_from_multiple_methods_reach_handlers_in_wire_order() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let router = Router::new(OrderState::default())
        .notification::<OrderedOutput, _, _>(|state, _, seq| async move { record_order(state, seq).await })
        .notification::<OrderedUpdate, _, _>(|state, _, seq| async move { record_order(state, seq).await });
    let state = Arc::clone(router.state());
    let server = Peer::spawn(br, bw, router, PeerConfig::default());
    let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
    for seq in 0..32 {
        if seq % 2 == 0 {
            client.notify::<OrderedOutput>(seq).await.unwrap();
        } else {
            client.notify::<OrderedUpdate>(seq).await.unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(1), state.complete.notified()).await.unwrap();
    assert_eq!(*state.seen.lock().unwrap(), (0..32).collect::<Vec<_>>());
    server.close();
}

#[tokio::test]
async fn requests_remain_concurrent_with_an_ordered_notification_handler() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let router = Router::new(BlockingState::default())
        .notification::<SlowNotice, _, _>(|state, _, ()| async move {
            state.started.notify_one();
            state.release.notified().await;
        })
        .method::<Add, _, _>(|_, _, params| async move { Ok(params.a + params.b) });
    let state = Arc::clone(router.state());
    let server = Peer::spawn(br, bw, router, PeerConfig::default());
    let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
    client.notify::<SlowNotice>(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), state.started.notified()).await.unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(1), client.call::<Add>(AddParams { a: 2, b: 40 })).await.unwrap().unwrap();
    assert_eq!(answer, 42);
    state.release.notify_one();
    server.close();
}

#[tokio::test]
async fn dropping_a_call_cancels_it_on_the_server() {
    let (client, _server, state) = pair();
    let call = client.call::<Sleep>(10_000);
    assert!(tokio::time::timeout(Duration::from_millis(50), call).await.is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(state.sleep_cancelled.load(Ordering::SeqCst), "the server dropped the cancelled handler");
    // The connection is still usable afterwards.
    assert_eq!(client.call::<Add>(AddParams { a: 1, b: 1 }).await.unwrap(), 2);
}

#[tokio::test]
async fn closing_fails_pending_calls_with_unavailable() {
    let (client, server, _) = pair();
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Sleep>(10_000).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    server.close();
    let err = pending.await.unwrap().unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    client.closed().await;
    assert_eq!(client.call::<Add>(AddParams { a: 1, b: 1 }).await.unwrap_err().code, ErrorCode::Unavailable);
}

#[tokio::test]
async fn a_server_can_call_back_the_client_while_serving() {
    let (client, _server, _) = pair();
    assert_eq!(client.call::<AskBack>(()).await.unwrap(), "client echoes: from server");
}

#[tokio::test]
async fn garbage_gets_a_parse_error_with_null_id_and_the_connection_survives() {
    let (raw, server_side) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(server_side);
    let _server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    rw.write_all(b"this is not json\n").await.unwrap();
    let reply: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], serde_json::Value::Null);
    assert_eq!(reply["error"]["data"]["kind"], "parse_error");
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"test.add\",\"params\":{\"a\":1,\"b\":2}}\n").await.unwrap();
    let reply: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(reply, serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": 3}));
}

#[tokio::test]
async fn oversized_messages_close_the_connection() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let server = Peer::spawn(br, bw, server_router(), PeerConfig { max_message_bytes: 64, ..PeerConfig::default() });
    let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
    let err = client.call::<Echo>("x".repeat(200)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    tokio::time::timeout(Duration::from_secs(1), server.closed()).await.unwrap();
}

#[tokio::test]
async fn malformed_envelope_gets_invalid_request_with_its_id() {
    let (raw, server_side) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(server_side);
    let _server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    rw.write_all(b"{\"jsonrpc\":\"1.0\",\"id\":7,\"method\":\"test.add\",\"params\":{\"a\":1,\"b\":2}}\n").await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["error"]["data"]["kind"], "invalid_request");
}

#[tokio::test]
async fn valid_json_with_invalid_envelope_field_types_is_invalid_request() {
    let (raw, server_side) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(server_side);
    let _server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":true,\"method\":\"test.add\"}\n").await.unwrap();
    let first: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(first["id"], serde_json::Value::Null);
    assert_eq!(first["error"]["data"]["kind"], "invalid_request");
    rw.write_all(b"{\"jsonrpc\":7,\"id\":9,\"method\":\"test.add\"}\n").await.unwrap();
    let second: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(second["id"], 9);
    assert_eq!(second["error"]["data"]["kind"], "invalid_request");
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"test.add\",\"params\":{\"a\":1,\"b\":2},\"result\":99}\n").await.unwrap();
    let third: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(third["id"], 10);
    assert_eq!(third["error"]["data"]["kind"], "invalid_request");
}

#[tokio::test]
async fn duplicate_live_request_id_is_rejected_without_losing_the_first_cancel() {
    let (raw, server_side) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(server_side);
    let server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"test.sleep\",\"params\":10000}\n";
    rw.write_all(request).await.unwrap();
    rw.write_all(request).await.unwrap();
    let duplicate = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let duplicate: serde_json::Value = serde_json::from_str(&duplicate).unwrap();
    assert_eq!(duplicate["id"], 7);
    assert_eq!(duplicate["error"]["data"]["kind"], "invalid_request");
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"$/cancel\",\"params\":{\"id\":7}}\n").await.unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let cancelled: serde_json::Value = serde_json::from_str(&cancelled).unwrap();
    assert_eq!(cancelled["error"]["data"]["kind"], "cancelled");
    server.close();
}

#[tokio::test]
async fn request_id_stays_reserved_until_its_reply_is_written() {
    let (raw, server_side) = tokio::io::duplex(32);
    let (sr, sw) = tokio::io::split(server_side);
    let server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"test.add\",\"params\":{\"a\":1,\"b\":2}}\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"test.add\",\"params\":{\"a\":5,\"b\":6}}\n").await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let responses =
        [serde_json::from_str::<serde_json::Value>(&first).unwrap(), serde_json::from_str::<serde_json::Value>(&second).unwrap()];
    assert!(responses.iter().any(|response| response["result"] == 3));
    assert!(responses.iter().any(|response| response["error"]["data"]["kind"] == "invalid_request"));
    server.close();
}

#[tokio::test]
async fn too_many_inflight_requests_get_limit_exceeded() {
    let (raw, server_side) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(server_side);
    let server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    for id in 1..=129 {
        rw.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"test.sleep\",\"params\":10000}}\n").as_bytes()).await.unwrap();
    }
    let rejected = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let rejected: serde_json::Value = serde_json::from_str(&rejected).unwrap();
    assert_eq!(rejected["error"]["data"]["kind"], "limit_exceeded");
    server.close();
}

#[tokio::test]
async fn outgoing_request_and_response_limits_are_enforced() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let server = Peer::spawn(br, bw, server_router(), PeerConfig { max_message_bytes: 128, ..PeerConfig::default() });
    let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
    client.set_max_outgoing_bytes(128);
    assert_eq!(client.call::<Echo>("x".repeat(300)).await.unwrap_err().code, ErrorCode::LimitExceeded);
    assert_eq!(client.notify_raw("test.large", serde_json::json!("x".repeat(300))).await.unwrap_err().code, ErrorCode::LimitExceeded);
    let result = client.call::<LargeReply>(()).await.unwrap_err();
    assert_eq!(result.code, ErrorCode::LimitExceeded, "oversized server results become bounded errors");
    server.close();
}

#[tokio::test]
async fn notification_queue_cap_closes_a_flooding_peer() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let router = Router::new(()).notification::<SlowNotice, _, _>(|_, _, ()| async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let server = Peer::spawn(br, bw, router, PeerConfig { notification_queue_capacity: 4, ..PeerConfig::default() });
    let client = Peer::spawn(ar, aw, NoHandler, PeerConfig::default());
    for _ in 0..6 {
        let _sent = client.notify::<SlowNotice>(()).await;
    }
    tokio::time::timeout(Duration::from_secs(1), server.closed()).await.unwrap();
}

#[tokio::test]
async fn cancellation_uses_priority_when_normal_output_is_full() {
    let (raw, peer_side) = tokio::io::duplex(64);
    let (pr, pw) = tokio::io::split(peer_side);
    let peer = Peer::spawn(pr, pw, NoHandler, PeerConfig { outgoing_capacity: 1, ..PeerConfig::default() });
    let (rr, _rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    let pending = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Sleep>(10_000).await }
    });
    let _request = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    peer.notify_raw("test.bulk", serde_json::json!("x".repeat(500))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    peer.notify_raw("test.bulk", serde_json::json!("y".repeat(500))).await.unwrap();
    pending.abort();
    let _aborted = pending.await;
    let _first_bulk = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    let next: serde_json::Value = serde_json::from_str(&next).unwrap();
    assert_eq!(next["method"], "$/cancel", "control must overtake the queued normal frame");
    peer.close();
}

#[tokio::test(start_paused = true)]
async fn stalled_writer_closes_if_a_cancel_cannot_be_delivered() {
    let (raw, peer_side) = tokio::io::duplex(64);
    let (pr, pw) = tokio::io::split(peer_side);
    let peer = Peer::spawn(pr, pw, NoHandler, PeerConfig::default());
    let (rr, _rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    let pending = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Sleep>(10_000).await }
    });
    let _request = lines.next_line().await.unwrap().unwrap();
    peer.notify_raw("test.bulk", serde_json::json!("x".repeat(500))).await.unwrap();
    tokio::task::yield_now().await;
    pending.abort();
    let _aborted = pending.await;
    assert!(!peer.is_closed());
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::timeout(Duration::from_secs(1), peer.closed()).await.unwrap();
}

#[tokio::test]
async fn cancelling_a_queued_request_closes_before_cancel_can_overtake_it() {
    let (_raw, peer_side) = tokio::io::duplex(64);
    let (pr, pw) = tokio::io::split(peer_side);
    let peer = Peer::spawn(pr, pw, NoHandler, PeerConfig { outgoing_capacity: 2, ..PeerConfig::default() });
    peer.notify_raw("test.bulk", serde_json::json!("x".repeat(500))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let pending = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Sleep>(10_000).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    pending.abort();
    let _aborted = pending.await;
    tokio::time::timeout(Duration::from_secs(1), peer.closed()).await.unwrap();
}

#[tokio::test]
async fn uncorrelated_peer_errors_do_not_log_their_payload() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let _subscriber = tracing::subscriber::set_default(Capture(Arc::clone(&logs)));
    let (raw, server_side) = tokio::io::duplex(1 << 16);
    let (sr, sw) = tokio::io::split(server_side);
    let _server = Peer::spawn(sr, sw, server_router(), PeerConfig::default());
    let (rr, mut rw) = tokio::io::split(raw);
    let mut lines = BufReader::new(rr).lines();
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"SECRET-SENTINEL\"}}\n").await.unwrap();
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"test.ping\",\"params\":\"SECRET-SENTINEL\"}\n").await.unwrap();
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"SECRET-SENTINEL\"}\n").await.unwrap();
    rw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"test.add\",\"params\":{\"a\":1,\"b\":1}}\n").await.unwrap();
    let _reply = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await.unwrap().unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let captured = logs.lock().unwrap().join(" ");
    assert!(captured.contains("uncorrelated error"));
    assert!(!captured.contains("SECRET-SENTINEL"), "peer-supplied messages must not reach logs");
}
