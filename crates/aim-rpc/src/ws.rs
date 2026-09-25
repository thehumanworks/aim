//! WebSocket text messages presented as an NDJSON byte stream for [`crate::Peer`].

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::framing::{FrameReader, ReadError};

const BRIDGE_BUFFER_BYTES: usize = 64 * 1024;

/// Adapts an established WebSocket to the byte stream expected by [`crate::Peer`].
///
/// Each WebSocket text message becomes exactly one NDJSON line, and each outgoing line becomes
/// one text message. Binary messages, embedded line breaks and messages over `max_message_bytes`
/// close the connection. The returned stream and the bridge have a bounded in-memory buffer, so
/// backpressure reaches the WebSocket. The handshake should also set Tungstenite's
/// `max_message_size` and `max_frame_size` to this limit to reject oversized frames before
/// Tungstenite buffers them.
///
/// This function spawns the bridge on the current Tokio runtime. Split the returned stream and
/// pass its halves to [`crate::Peer::spawn`] with the same `max_message_bytes` in `PeerConfig`.
pub fn websocket_duplex<S>(socket: WebSocketStream<S>, max_message_bytes: usize) -> DuplexStream
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (peer_stream, bridge_stream) = tokio::io::duplex(BRIDGE_BUFFER_BYTES);
    tokio::spawn(bridge(socket, bridge_stream, max_message_bytes));
    peer_stream
}

async fn bridge<S>(socket: WebSocketStream<S>, bridge_stream: DuplexStream, max_message_bytes: usize)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut source) = socket.split();
    let (mut peer_reader, mut peer_writer) = tokio::io::split(bridge_stream);
    let close = tokio::select! {
        close = inbound(&mut source, &mut peer_writer, max_message_bytes) => close,
        close = outbound(&mut sink, &mut peer_reader, max_message_bytes) => close,
    };
    drop(peer_reader);
    drop(peer_writer);
    // The opposite direction is cancelled when either side ends. In particular, a rejected
    // inbound message immediately becomes EOF to Peer, failing its outstanding calls.
    let _result = sink.send(Message::Close(close)).await;
}

async fn inbound<S>(
    source: &mut SplitStream<WebSocketStream<S>>,
    peer_writer: &mut WriteHalf<DuplexStream>,
    max_message_bytes: usize,
) -> Option<CloseFrame>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(message) = source.next().await {
        match message {
            Ok(Message::Text(text)) => {
                if text.len() > max_message_bytes {
                    return Some(close_frame(CloseCode::Size, "message too large"));
                }
                if text.trim().is_empty() {
                    return Some(close_frame(CloseCode::Protocol, "empty message"));
                }
                if text.contains(['\n', '\r']) {
                    return Some(close_frame(CloseCode::Protocol, "line break in message"));
                }
                if peer_writer.write_all(text.as_bytes()).await.is_err() || peer_writer.write_all(b"\n").await.is_err() {
                    return None;
                }
            }
            Ok(Message::Binary(_)) => return Some(close_frame(CloseCode::Unsupported, "binary messages unsupported")),
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(Message::Frame(_)) => return Some(close_frame(CloseCode::Protocol, "unexpected frame")),
        }
    }
    None
}

async fn outbound<S>(
    sink: &mut SplitSink<WebSocketStream<S>, Message>,
    peer_reader: &mut ReadHalf<DuplexStream>,
    max_message_bytes: usize,
) -> Option<CloseFrame>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = FrameReader::new(peer_reader, max_message_bytes);
    while let Some(line) = reader.next().await {
        match line {
            Ok(line) => {
                if sink.send(Message::text(line)).await.is_err() {
                    return None;
                }
            }
            Err(ReadError::TooLarge) => return Some(close_frame(CloseCode::Size, "message too large")),
            Err(ReadError::Io(_)) => return None,
        }
    }
    None
}

fn close_frame(code: CloseCode, reason: &'static str) -> CloseFrame {
    CloseFrame { code, reason: reason.into() }
}
