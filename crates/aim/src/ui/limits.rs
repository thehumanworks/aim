//! How much UI a session may put on screen, and how fast (ADR 0064). The numbers are policy
//! defaults, kept as data.

/// Bounds on a session's surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest serialized message, in bytes.
    pub message_bytes: usize,
    /// Largest serialized surface (components and data), in bytes.
    pub surface_bytes: usize,
    /// Most components in one surface.
    pub components: usize,
    /// Deepest component tree, counted from `root` (which is depth 1).
    pub depth: usize,
    /// Most surfaces a session shows at once.
    pub surfaces: usize,
    /// Messages a session may send in a burst.
    pub burst: u64,
    /// Messages per second the burst refills at.
    pub per_second: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { message_bytes: 64 * 1024, surface_bytes: 128 * 1024, components: 256, depth: 16, surfaces: 16, burst: 20, per_second: 10 }
    }
}

/// A token bucket over milliseconds, in thousandths of a message (integer arithmetic only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    /// Thousandths of a message available.
    milli: u64,
    /// When it was last refilled.
    at_ms: u64,
}

impl Bucket {
    /// A full bucket at `now_ms`.
    #[must_use]
    pub fn full(limits: &Limits, now_ms: u64) -> Self {
        Self { milli: limits.burst.saturating_mul(1_000), at_ms: now_ms }
    }

    /// Takes one message if the bucket (refilled up to `now_ms`) holds one. A clock that went
    /// backwards refills nothing.
    pub fn take(&mut self, limits: &Limits, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.at_ms);
        let capacity = limits.burst.saturating_mul(1_000);
        self.milli = self.milli.saturating_add(elapsed.saturating_mul(limits.per_second)).min(capacity);
        self.at_ms = self.at_ms.max(now_ms);
        if self.milli >= 1_000 {
            self.milli -= 1_000;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_burst_then_the_refill_rate() {
        let limits = Limits { burst: 3, per_second: 2, ..Limits::default() };
        let mut bucket = Bucket::full(&limits, 1_000);
        assert!((0..3).all(|_| bucket.take(&limits, 1_000)));
        assert!(!bucket.take(&limits, 1_000), "the burst is spent");
        assert!(!bucket.take(&limits, 1_400), "0.8 of a message refilled");
        assert!(bucket.take(&limits, 1_500), "one message after 500 ms at 2/s");
        assert!(!bucket.take(&limits, 400), "a clock going backwards refills nothing");
    }

    proptest! {
        /// Over any schedule, no more messages pass than the burst plus what the elapsed time
        /// refilled.
        #[test]
        fn never_more_than_burst_plus_refill(burst in 1_u64..40, per_second in 0_u64..50, gaps in proptest::collection::vec(0_u64..400, 1..200)) {
            let limits = Limits { burst, per_second, ..Limits::default() };
            let mut bucket = Bucket::full(&limits, 0);
            let (mut now, mut passed) = (0_u64, 0_u64);
            for gap in gaps {
                now += gap;
                if bucket.take(&limits, now) {
                    passed += 1;
                }
            }
            prop_assert!(passed <= burst + now * per_second / 1_000);
        }
    }
}
