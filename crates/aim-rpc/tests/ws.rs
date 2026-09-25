//! WebSocket framing and Peer conformance over the byte-stream adapter.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim_proto::{method, notification};
use aim_rpc::ws::websocket_duplex;
use aim_rpc::{NoHandler, Peer, PeerConfig, Router};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

type Socket = WebSocketStream<tokio::io::DuplexStream>;

method!(
    /// Echoes a JSON value in the WebSocket conformance test.
    Echo = "echo" (serde_json::Value) -> serde_json::Value
);
notification!(
    /// Records a sequence number in the WebSocket conformance test.
    Item = "item" (u64)
);

async fn sockets() -> (Socket, Socket) {
    let (left, right) = tokio::io::duplex(256 * 1024);
    tokio::join!(WebSocketStream::from_raw_socket(left, Role::Client, None), WebSocketStream::from_raw_socket(right, Role::Server, None))
}

fn peer(socket: Socket, router: impl aim_rpc::Handler, max_message_bytes: usize) -> Peer {
    let stream = websocket_duplex(socket, max_message_bytes);
    let (reader, writer) = tokio::io::split(stream);
    Peer::spawn(reader, writer, router, PeerConfig { max_message_bytes, ..PeerConfig::default() })
}

#[tokio::test]
async fn peer_round_trip_and_ordered_notifications() {
    let (client_socket, server_socket) = sockets().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let complete = Arc::new(tokio::sync::Notify::new());
    let router = Router::new((Arc::clone(&seen), Arc::clone(&complete)))
        .method::<Echo, _, _>(|_, _, params| async move { Ok(params) })
        .notification::<Item, _, _>(|state, _, sequence| async move {
            let mut seen = state.0.lock().unwrap();
            seen.push(sequence);
            if seen.len() == 12 {
                state.1.notify_one();
            }
        });
    let _server = peer(server_socket, router, 1024);
    let client = peer(client_socket, NoHandler, 1024);
    for sequence in 0..12 {
        client.notify_raw("item", json!(sequence)).await.unwrap();
    }
    let echoed = client.call_raw("echo", json!({"value": 42})).await.unwrap();
    assert_eq!(echoed, json!({"value": 42}));
    tokio::time::timeout(Duration::from_secs(2), complete.notified()).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), (0..12).collect::<Vec<_>>());
}

#[tokio::test]
async fn each_ndjson_line_is_one_text_message_and_cancellation_follows() {
    let (client_socket, mut remote) = sockets().await;
    let client = peer(client_socket, NoHandler, 1024);
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call_raw("hold", json!({})).await }
    });
    let request = remote.next().await.unwrap().unwrap();
    let Message::Text(body) = request else { panic!("request must be text") };
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["method"], "hold");
    assert!(!body.contains('\n'));
    call.abort();
    let cancel = tokio::time::timeout(Duration::from_secs(2), remote.next()).await.unwrap().unwrap().unwrap();
    let Message::Text(body) = cancel else { panic!("cancellation must be text") };
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["method"], "$/cancel");
}

#[tokio::test]
async fn incoming_text_frame_becomes_exactly_one_ndjson_line() {
    let (socket, mut remote) = sockets().await;
    let stream = websocket_duplex(socket, 32);
    let mut reader = BufReader::new(stream);
    remote.send(Message::text("{\"id\":1}")).await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line, "{\"id\":1}\n");
    reader.get_mut().write_all(b"{\"id\":2}\n").await.unwrap();
    assert_eq!(remote.next().await.unwrap().unwrap(), Message::text("{\"id\":2}"));
}

#[tokio::test]
async fn binary_and_embedded_line_breaks_close_the_connection() {
    for (message, expected_code) in [
        (Message::binary(vec![1, 2, 3]), CloseCode::Unsupported),
        (Message::text("{}\n{}"), CloseCode::Protocol),
        (Message::text("  "), CloseCode::Protocol),
    ] {
        let (socket, mut remote) = sockets().await;
        let mut stream = websocket_duplex(socket, 32);
        remote.send(message).await.unwrap();
        let close = remote.next().await.unwrap().unwrap();
        let Message::Close(Some(frame)) = close else { panic!("expected close frame") };
        assert_eq!(frame.code, expected_code);
        let mut byte = [0];
        assert_eq!(tokio::io::AsyncReadExt::read(&mut stream, &mut byte).await.unwrap(), 0);
    }
}

#[tokio::test]
async fn oversized_incoming_and_outgoing_messages_close_with_size_code() {
    let (socket, mut remote) = sockets().await;
    let mut stream = websocket_duplex(socket, 8);
    remote.send(Message::text("123456789")).await.unwrap();
    let Message::Close(Some(frame)) = remote.next().await.unwrap().unwrap() else { panic!("expected close frame") };
    assert_eq!(frame.code, CloseCode::Size);
    let mut byte = [0];
    assert_eq!(tokio::io::AsyncReadExt::read(&mut stream, &mut byte).await.unwrap(), 0);

    let (socket, mut remote) = sockets().await;
    let mut stream = websocket_duplex(socket, 8);
    stream.write_all(b"123456789\n").await.unwrap();
    let Message::Close(Some(frame)) = remote.next().await.unwrap().unwrap() else { panic!("expected close frame") };
    assert_eq!(frame.code, CloseCode::Size);
}
