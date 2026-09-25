//! NDJSON framing: one compact JSON message per line.

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec, LinesCodecError};

/// Default maximum size of one message: 16 MiB (advertised to peers at `initialize`).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Reads frames from a byte stream.
pub struct FrameReader<R> {
    inner: FramedRead<R, LinesCodec>,
}

/// Why reading a frame failed.
#[derive(Debug)]
pub enum ReadError {
    /// A message exceeded the maximum size; the connection must be closed.
    TooLarge,
    /// The underlying stream failed.
    Io(std::io::Error),
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Wraps `reader`, refusing messages larger than `max_bytes`.
    pub fn new(reader: R, max_bytes: usize) -> Self {
        Self { inner: FramedRead::new(reader, LinesCodec::new_with_max_length(max_bytes)) }
    }

    /// The next frame, `None` at end of stream.
    pub async fn next(&mut self) -> Option<Result<String, ReadError>> {
        loop {
            return match self.inner.next().await? {
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => Some(Ok(line)),
                Err(LinesCodecError::MaxLineLengthExceeded) => Some(Err(ReadError::TooLarge)),
                Err(LinesCodecError::Io(err)) => Some(Err(ReadError::Io(err))),
            };
        }
    }
}

/// Writes frames to a byte stream.
pub struct FrameWriter<W> {
    inner: FramedWrite<W, LinesCodec>,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Wraps `writer`.
    pub fn new(writer: W) -> Self {
        Self { inner: FramedWrite::new(writer, LinesCodec::new()) }
    }

    /// Writes one frame and flushes it.
    pub async fn send(&mut self, frame: String) -> Result<(), std::io::Error> {
        self.inner.send(frame).await.map_err(|err| match err {
            LinesCodecError::Io(io) => io,
            LinesCodecError::MaxLineLengthExceeded => std::io::Error::other("frame too large"),
        })
    }
}
