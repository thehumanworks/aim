//! Behaviour of the JSON-RPC peer over an in-process duplex stream.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use aim_proto::error::ErrorCode;
use aim_proto::{method, notification};
use aim_rpc::{NoHandler, Peer, PeerConfig, Router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

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
notification!(
    /// Adds to the server's ping counter.
    Ping = "test.ping" (u32)
);

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
