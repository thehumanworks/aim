//! A bounded, byte-oriented SSE parser. It yields each event's `data` payload; comments
//! (`: OPENROUTER PROCESSING` keepalives), `event:`, `id:` and `retry:` lines are ignored.

use aim_llm::{LlmError, LlmErrorKind};

/// Longest accepted unterminated line and event payload.
const MAX_EVENT_BYTES: usize = 1_048_576;

fn too_large() -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, "SSE event exceeds 1 MiB")
}

#[derive(Default)]
pub(crate) struct Sse {
    buffer: Vec<u8>,
    data: String,
    has_data: bool,
}

impl Sse {
    /// Feeds bytes; returns the payloads of the events they complete.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, LlmError> {
        self.buffer.extend_from_slice(bytes);
        let buffer = std::mem::take(&mut self.buffer);
        let mut frames = Vec::new();
        let mut start = 0_usize;
        while let Some(rest) = buffer.get(start..)
            && let Some(offset) = rest.iter().position(|byte| *byte == b'\n')
        {
            self.line(rest.get(..offset).unwrap_or_default(), &mut frames)?;
            start = start.saturating_add(offset).saturating_add(1);
        }
        self.buffer = buffer;
        self.buffer.drain(..start.min(self.buffer.len()));
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err(too_large());
        }
        Ok(frames)
    }

    /// Ends the stream: an event left without its terminating blank line is still delivered.
    pub(crate) fn finish(&mut self) -> Result<Vec<String>, LlmError> {
        let mut frames = Vec::new();
        let rest = std::mem::take(&mut self.buffer);
        if !rest.is_empty() {
            self.line(&rest, &mut frames)?;
        }
        self.line(b"", &mut frames)?;
        Ok(frames)
    }

    fn line(&mut self, line: &[u8], frames: &mut Vec<String>) -> Result<(), LlmError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if self.has_data {
                frames.push(std::mem::take(&mut self.data));
                self.has_data = false;
            }
            return Ok(());
        }
        let Some(raw) = line.strip_prefix(b"data:") else { return Ok(()) };
        let raw = raw.strip_prefix(b" ").unwrap_or(raw);
        let text = std::str::from_utf8(raw).map_err(|_| LlmError::new(LlmErrorKind::Protocol, "invalid SSE UTF-8"))?;
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(text);
        self.has_data = true;
        if self.data.len() > MAX_EVENT_BYTES {
            return Err(too_large());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_crlf_and_multiline_data() -> Result<(), LlmError> {
        let mut sse = Sse::default();
        let frames = sse.push(b": OPENROUTER PROCESSING\r\n\r\nevent: x\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\ndata: {\"b\":2}\n\n")?;
        assert_eq!(frames, ["{\"a\":\n1}", "{\"b\":2}"]);
        Ok(())
    }

    #[test]
    fn split_utf8_and_trailing_event_at_eof() -> Result<(), LlmError> {
        let mut sse = Sse::default();
        let bytes = "data: {\"t\":\"é\"}\n\ndata: [DONE]".as_bytes();
        let mut frames = Vec::new();
        for byte in bytes {
            frames.extend(sse.push(std::slice::from_ref(byte))?);
        }
        assert_eq!(frames, ["{\"t\":\"é\"}"]);
        assert_eq!(sse.finish()?, ["[DONE]"]);
        Ok(())
    }

    #[test]
    fn many_small_events_in_one_read_are_accepted() -> Result<(), LlmError> {
        let mut sse = Sse::default();
        let read = "data: {\"x\":1}\n\n".repeat(80_000);
        assert!(read.len() > MAX_EVENT_BYTES);
        assert_eq!(sse.push(read.as_bytes())?.len(), 80_000);
        Ok(())
    }

    #[test]
    fn oversize_and_invalid_input_fail() {
        let mut sse = Sse::default();
        assert!(sse.push(&vec![b'x'; MAX_EVENT_BYTES + 1]).is_err());
        let mut sse = Sse::default();
        let mut event = b"data: ".to_vec();
        event.extend(std::iter::repeat_n(b'y', MAX_EVENT_BYTES + 1));
        event.push(b'\n');
        assert!(sse.push(&event).is_err());
        let mut sse = Sse::default();
        assert_eq!(sse.push(b"data: \xff\xfe\n\n").err().map(|e| e.kind), Some(LlmErrorKind::Protocol));
    }
}
