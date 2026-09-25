//! A process's retained output: a byte-bounded ring of sequence-numbered chunks
//! (docs/architecture.md §4.1, §9).
//!
//! Kernel candidate: pure. Invariants:
//! - `seq` starts at 1 and increases by exactly one per pushed chunk, across all streams;
//! - retained chunks are contiguous in `seq` and end at [`OutputRing::last_seq`];
//! - retained bytes stay within the capacity, except that the newest chunk is always kept (so the
//!   bound is `capacity + largest chunk`);
//! - a reader asking for output after `after_seq` gets the retained chunks with a greater `seq`, in
//!   order, and `dropped_before` whenever chunks it has not seen were evicted.

use std::collections::VecDeque;

use aim_proto::harness::OutputStream;

/// One retained chunk.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chunk {
    /// Sequence number within the process.
    pub seq: u64,
    /// Stream it came from.
    pub stream: OutputStream,
    /// The bytes.
    pub data: Vec<u8>,
}

/// The answer to a read.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Slice {
    /// Chunks after the requested sequence number, in order.
    pub chunks: Vec<Chunk>,
    /// Chunks before this sequence number were evicted before the reader saw them.
    pub dropped_before: Option<u64>,
    /// The slice reaches the newest chunk: nothing retained is left unread.
    pub complete: bool,
}

/// A byte-bounded ring of output chunks.
#[derive(Clone, Debug)]
pub struct OutputRing {
    capacity: usize,
    chunks: VecDeque<Chunk>,
    bytes: usize,
    next_seq: u64,
}

impl OutputRing {
    /// An empty ring keeping about `capacity` bytes.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { capacity, chunks: VecDeque::new(), bytes: 0, next_seq: 1 }
    }

    /// Appends a chunk and returns its sequence number (`None` for empty data, which is not
    /// recorded). Evicts the oldest chunks beyond the capacity.
    pub fn push(&mut self, stream: OutputStream, data: Vec<u8>) -> Option<u64> {
        if data.is_empty() {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.bytes = self.bytes.saturating_add(data.len());
        self.chunks.push_back(Chunk { seq, stream, data });
        while self.bytes > self.capacity && self.chunks.len() > 1 {
            if let Some(evicted) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(evicted.data.len());
            }
        }
        Some(seq)
    }

    /// Sequence number of the newest chunk (0 when nothing was ever pushed).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Sequence number of the oldest retained chunk (`last_seq + 1` when none is retained).
    #[must_use]
    pub fn first_seq(&self) -> u64 {
        self.chunks.front().map_or(self.next_seq, |chunk| chunk.seq)
    }

    /// Bytes currently retained.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// Chunks after `after_seq`, up to `max_bytes` (but always at least one chunk when any is
    /// available, so a reader always makes progress).
    #[must_use]
    pub fn read(&self, after_seq: u64, max_bytes: usize) -> Slice {
        let first = self.first_seq();
        let dropped_before = (after_seq.saturating_add(1) < first).then_some(first);
        let mut chunks = Vec::new();
        let mut total = 0usize;
        for chunk in self.chunks.iter().filter(|chunk| chunk.seq > after_seq) {
            let len = chunk.data.len();
            if !chunks.is_empty() && total.saturating_add(len) > max_bytes {
                break;
            }
            total = total.saturating_add(len);
            chunks.push(chunk.clone());
        }
        let reached = chunks.last().map_or(after_seq, |chunk| chunk.seq);
        Slice { chunks, dropped_before, complete: reached >= self.last_seq() }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn seq_is_shared_across_streams() {
        let mut ring = OutputRing::new(1024);
        assert_eq!(ring.push(OutputStream::Stdout, b"a".to_vec()), Some(1));
        assert_eq!(ring.push(OutputStream::Stderr, b"b".to_vec()), Some(2));
        assert_eq!(ring.push(OutputStream::Stdout, Vec::new()), None);
        assert_eq!(ring.last_seq(), 2);
        let slice = ring.read(0, 1024);
        assert_eq!(slice.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert!(slice.complete);
        assert_eq!(slice.dropped_before, None);
        assert_eq!(ring.read(1, 1024).chunks.len(), 1);
        assert!(ring.read(2, 1024).chunks.is_empty());
    }

    #[test]
    fn eviction_reports_dropped_before() {
        let mut ring = OutputRing::new(4);
        for _ in 0..4 {
            ring.push(OutputStream::Stdout, b"xx".to_vec());
        }
        assert_eq!(ring.first_seq(), 3);
        let slice = ring.read(0, 1024);
        assert_eq!(slice.dropped_before, Some(3));
        assert_eq!(slice.chunks.first().unwrap().seq, 3);
        assert_eq!(ring.read(2, 1024).dropped_before, None);
    }

    #[test]
    fn max_bytes_limits_but_always_progresses() {
        let mut ring = OutputRing::new(1024);
        ring.push(OutputStream::Stdout, vec![b'a'; 10]);
        ring.push(OutputStream::Stdout, vec![b'b'; 10]);
        let slice = ring.read(0, 1);
        assert_eq!(slice.chunks.len(), 1);
        assert!(!slice.complete);
        assert_eq!(ring.read(0, 20).chunks.len(), 2);
    }

    proptest! {
        /// Invariants hold after any sequence of pushes, and every read returns an in-order
        /// suffix of retained chunks after the requested sequence number.
        #[test]
        fn invariants(capacity in 1usize..64, sizes in prop::collection::vec(0usize..16, 0..64), after in 0u64..80, max in 1usize..64) {
            let mut ring = OutputRing::new(capacity);
            let mut pushed = 0u64;
            let mut largest = 0usize;
            for (i, size) in sizes.iter().enumerate() {
                let stream = if i % 2 == 0 { OutputStream::Stdout } else { OutputStream::Stderr };
                if let Some(seq) = ring.push(stream, vec![0; *size]) {
                    pushed += 1;
                    prop_assert_eq!(seq, pushed);
                    largest = largest.max(*size);
                }
                prop_assert!(ring.retained_bytes() <= capacity + largest);
                prop_assert_eq!(ring.last_seq(), pushed);
            }
            let slice = ring.read(after, max);
            let mut expected = after.max(ring.first_seq().saturating_sub(1));
            for chunk in &slice.chunks {
                prop_assert_eq!(chunk.seq, expected + 1);
                expected = chunk.seq;
            }
            prop_assert_eq!(slice.dropped_before.is_some(), after + 1 < ring.first_seq());
            prop_assert_eq!(slice.complete, expected >= ring.last_seq());
        }
    }
}
