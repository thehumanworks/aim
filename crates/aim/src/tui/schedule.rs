//! The frame scheduler: keystrokes paint at once, streamed updates coalesce to at most one frame
//! per [`FRAME`], and a burst of resize events settles for [`SETTLE`] before one full repaint.
//! Pure: the shell passes the clock in.

use std::time::{Duration, Instant};

/// Shortest interval between two coalesced frames (about 60 frames per second).
pub const FRAME: Duration = Duration::from_millis(16);
/// Quiet time after the last resize event before repainting.
pub const SETTLE: Duration = Duration::from_millis(75);

/// A frame that is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// An ordinary frame.
    Normal,
    /// The first frame after the terminal size settled.
    Resized,
}

/// When the next frame is due.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scheduler {
    last: Option<Instant>,
    due: Option<Instant>,
    settle: Option<Instant>,
}

impl Scheduler {
    /// Input that must show now (a keystroke).
    pub fn urgent(&mut self, now: Instant) {
        self.due = Some(now);
    }

    /// Streamed output: due one frame interval after the last paint, never later than already due.
    pub fn stream(&mut self, now: Instant) {
        let at = self.last.map_or(now, |last| (last + FRAME).max(now));
        self.due = Some(self.due.map_or(at, |due| due.min(at)));
    }

    /// The terminal was resized: wait for the size to settle.
    pub fn resize(&mut self, now: Instant) {
        self.settle = Some(now + SETTLE);
    }

    /// When the loop must wake up next.
    pub fn deadline(&self) -> Option<Instant> {
        self.settle.or(self.due)
    }

    /// The frame due at `now`, if any (consumed).
    pub fn poll(&mut self, now: Instant) -> Option<Frame> {
        if let Some(settle) = self.settle {
            if now < settle {
                return None;
            }
            self.settle = None;
            self.due = None;
            return Some(Frame::Resized);
        }
        match self.due {
            Some(due) if now >= due => {
                self.due = None;
                Some(Frame::Normal)
            }
            _ => None,
        }
    }

    /// A frame was painted at `now`.
    pub fn painted(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_stream_updates_coalesces_into_one_frame() {
        let start = Instant::now();
        let mut s = Scheduler::default();
        s.painted(start);
        for n in 0..500 {
            s.stream(start + Duration::from_micros(n));
        }
        assert_eq!(s.poll(start + Duration::from_millis(1)), None, "not before the frame interval");
        assert_eq!(s.poll(start + FRAME), Some(Frame::Normal));
        assert_eq!(s.poll(start + FRAME), None, "exactly one frame");
    }

    #[test]
    fn keystrokes_paint_immediately_even_mid_interval() {
        let start = Instant::now();
        let mut s = Scheduler::default();
        s.painted(start);
        s.stream(start);
        s.urgent(start + Duration::from_millis(2));
        assert_eq!(s.poll(start + Duration::from_millis(2)), Some(Frame::Normal));
    }

    #[test]
    fn resize_bursts_settle_into_one_resized_frame() {
        let start = Instant::now();
        let mut s = Scheduler::default();
        for n in 0..20 {
            s.resize(start + Duration::from_millis(n * 10));
            s.urgent(start + Duration::from_millis(n * 10));
            assert_eq!(s.poll(start + Duration::from_millis(n * 10)), None);
        }
        let last = start + Duration::from_millis(190);
        assert_eq!(s.deadline(), Some(last + SETTLE));
        assert_eq!(s.poll(last + SETTLE), Some(Frame::Resized));
        assert_eq!(s.poll(last + SETTLE), None);
    }
}
