//! What a cell's output costs, and where it stops (`REV13a` M2, M3, M8).
//!
//! Both sides of the worker boundary apply the same budget. The worker charges every helper call
//! before it keeps or streams it. The parent charges again what it receives, so a compromised
//! worker cannot flood it. The rules are pure, and the budget is a kernel candidate (ADR 0005):
//!
//! - a `text` or `notify` event costs its UTF-8 length plus one separator byte, so an empty
//!   `text('')` still costs one byte;
//! - a yield marker costs one byte;
//! - a marker (a yield, or an empty `notify`) right after another kept marker is **coalesced**. It
//!   is not kept and costs nothing;
//! - an event is kept only while the kept bytes stay at most `limit_bytes` and the kept events stay
//!   at most `limit_events`;
//! - once one event is dropped, every later one is dropped too. Kept output is then a prefix of
//!   what the cell emitted;
//! - dropped events and their text bytes are counted, so the model can be told what it lost.
//!
//! Invariant: `kept_bytes <= limit_bytes` and `kept_events <= limit_events` after any sequence of
//! charges.
//!
//! The budget never fails a cell. Output beyond it is dropped and reported with a marker, so a
//! cell's side effects and its `store` are never lost because it printed too much.

/// Most events one cell may keep, whatever their size.
pub const MAX_EVENTS: usize = 2048;

/// The largest serialized `store` a session keeps (`REV13a` L10), well under the 16 MiB RPC frame.
pub const MAX_STORE_BYTES: usize = 4 * 1024 * 1024;

/// What a helper call emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// `text(value)`: part of the cell's output.
    Text,
    /// `notify(value)`: shown to the model as soon as possible.
    Notify,
    /// `yield_control()`: a marker asking the parent to return control to the model.
    Yield,
}

/// The outcome of charging one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charge {
    /// Keep and forward the event.
    Keep,
    /// A marker repeating the previous marker: not kept, and nothing is lost.
    Coalesce,
    /// Over the budget: not kept. It is counted as dropped.
    Drop,
}

/// The output budget of one cell.
#[derive(Clone, Debug)]
pub struct OutputBudget {
    limit_bytes: usize,
    limit_events: usize,
    kept_bytes: usize,
    kept_events: usize,
    dropped_bytes: u64,
    dropped_events: u64,
    exhausted: bool,
    last_was_marker: bool,
}

impl OutputBudget {
    /// A budget of at most `limit_bytes` kept bytes in at most `limit_events` kept events.
    #[must_use]
    pub const fn new(limit_bytes: usize, limit_events: usize) -> Self {
        Self {
            limit_bytes,
            limit_events,
            kept_bytes: 0,
            kept_events: 0,
            dropped_bytes: 0,
            dropped_events: 0,
            exhausted: false,
            last_was_marker: false,
        }
    }

    /// Charges one event of `kind` whose text is `len` bytes long.
    pub fn charge(&mut self, kind: EventKind, len: usize) -> Charge {
        let marker = kind == EventKind::Yield || (kind == EventKind::Notify && len == 0);
        if marker && self.last_was_marker && !self.exhausted {
            return Charge::Coalesce;
        }
        let cost = if kind == EventKind::Yield { 1 } else { len.saturating_add(1) };
        let fits = self.kept_events < self.limit_events && self.kept_bytes.saturating_add(cost) <= self.limit_bytes;
        if self.exhausted || !fits {
            self.exhausted = true;
            self.dropped_events = self.dropped_events.saturating_add(1);
            self.dropped_bytes = self.dropped_bytes.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
            return Charge::Drop;
        }
        self.kept_bytes = self.kept_bytes.saturating_add(cost);
        self.kept_events = self.kept_events.saturating_add(1);
        self.last_was_marker = marker;
        Charge::Keep
    }

    /// Charges a final value (a cell's returned value) that is not an event. Returns how many of
    /// its `len` bytes fit. The rest is counted as dropped.
    pub fn fit(&mut self, len: usize) -> usize {
        let room = if self.exhausted { 0 } else { self.limit_bytes.saturating_sub(self.kept_bytes) };
        let kept = len.min(room);
        self.kept_bytes = self.kept_bytes.saturating_add(kept);
        if kept < len {
            self.exhausted = true;
            self.dropped_bytes = self.dropped_bytes.saturating_add(u64::try_from(len - kept).unwrap_or(u64::MAX));
        }
        kept
    }

    /// Bytes kept so far, separators included.
    #[must_use]
    pub const fn kept_bytes(&self) -> usize {
        self.kept_bytes
    }

    /// Events kept so far.
    #[must_use]
    pub const fn kept_events(&self) -> usize {
        self.kept_events
    }

    /// Text bytes dropped so far.
    #[must_use]
    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// Events dropped so far.
    #[must_use]
    pub const fn dropped_events(&self) -> u64 {
        self.dropped_events
    }
}

/// Cuts `text` to about `max_bytes` for the model. It keeps the head and the tail around an
/// omission marker, and starts with a warning line, as Codex's code mode does
/// (`codex-rs/core/src/tools/code_mode/mod.rs`, `truncate_code_mode_result`).
///
/// Text within the budget is returned unchanged. Otherwise the result is at most `max_bytes`
/// long, unless the budget is smaller than the warning and marker themselves; then only those
/// are returned.
#[must_use]
pub fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let warning = format!("Warning: truncated output (original length: {} bytes)\n", text.len());
    // The marker's length depends on how many bytes it names; the whole length bounds it.
    let longest_marker = format!("\n…{} bytes truncated…\n", text.len()).len();
    let room = max_bytes.saturating_sub(warning.len()).saturating_sub(longest_marker);
    let head = text.floor_char_boundary(room / 2);
    let tail = text.ceil_char_boundary(text.len().saturating_sub(room.saturating_sub(head)));
    let omitted = tail.saturating_sub(head);
    let mut result = String::with_capacity(max_bytes);
    result.push_str(&warning);
    result.push_str(text.get(..head).unwrap_or_default());
    result.push_str("\n…");
    result.push_str(&omitted.to_string());
    result.push_str(" bytes truncated…\n");
    result.push_str(text.get(tail..).unwrap_or_default());
    result
}

/// The note that tells the model a cell emitted more than it could keep.
#[must_use]
pub fn dropped_note(dropped_bytes: u64, dropped_events: u64) -> Option<String> {
    (dropped_events > 0 || dropped_bytes > 0).then(|| {
        format!("[output limit reached: {dropped_events} more output calls ({dropped_bytes} bytes) were dropped; the cell kept running]")
    })
}

#[cfg(test)]
mod tests {
    use super::{Charge, EventKind, OutputBudget, truncate_middle};

    /// A small deterministic generator, so the property test needs no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    #[test]
    fn empty_text_calls_are_charged() {
        let mut budget = OutputBudget::new(3, 100);
        assert_eq!(budget.charge(EventKind::Text, 0), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Text, 0), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Text, 0), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Text, 0), Charge::Drop);
        assert_eq!(budget.kept_bytes(), 3);
        assert_eq!(budget.dropped_events(), 1);
    }

    #[test]
    fn repeated_markers_coalesce_and_the_event_count_is_bounded() {
        let mut budget = OutputBudget::new(1_000_000, 4);
        assert_eq!(budget.charge(EventKind::Yield, 0), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Yield, 0), Charge::Coalesce);
        assert_eq!(budget.charge(EventKind::Notify, 0), Charge::Coalesce);
        assert_eq!(budget.charge(EventKind::Notify, 1), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Notify, 0), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Text, 1), Charge::Keep);
        assert_eq!(budget.charge(EventKind::Text, 1), Charge::Drop);
        assert_eq!(budget.kept_events(), 4);
    }

    #[test]
    fn output_after_a_drop_stays_dropped() {
        let mut budget = OutputBudget::new(10, 100);
        assert_eq!(budget.charge(EventKind::Text, 20), Charge::Drop);
        assert_eq!(budget.charge(EventKind::Text, 1), Charge::Drop, "kept output stays a prefix");
        assert_eq!(budget.dropped_bytes(), 21);
        assert_eq!(budget.fit(5), 0);
    }

    #[test]
    fn a_returned_value_keeps_what_fits() {
        let mut budget = OutputBudget::new(10, 100);
        assert_eq!(budget.fit(4), 4);
        assert_eq!(budget.fit(20), 6);
        assert_eq!(budget.dropped_bytes(), 14);
    }

    #[test]
    fn any_sequence_stays_within_both_limits() {
        let mut random = Lcg(7);
        for _ in 0..200 {
            let limit_bytes = usize::try_from(random.next() % 200).unwrap();
            let limit_events = usize::try_from(random.next() % 20).unwrap();
            let mut budget = OutputBudget::new(limit_bytes, limit_events);
            let mut kept_text = 0_usize;
            for _ in 0..100 {
                let kind = match random.next() % 3 {
                    0 => EventKind::Text,
                    1 => EventKind::Notify,
                    _ => EventKind::Yield,
                };
                let len = usize::try_from(random.next() % 30).unwrap();
                if budget.charge(kind, len) == Charge::Keep && kind != EventKind::Yield {
                    kept_text += len;
                }
                assert!(budget.kept_bytes() <= limit_bytes);
                assert!(budget.kept_events() <= limit_events);
                assert!(kept_text <= budget.kept_bytes());
            }
        }
    }

    #[test]
    fn truncation_keeps_head_and_tail_within_the_budget() {
        let text = "0123456789".repeat(100);
        let cut = truncate_middle(&text, 200);
        assert!(cut.len() <= 200, "{} bytes", cut.len());
        assert!(cut.starts_with("Warning: truncated output (original length: 1000 bytes)\n0123"));
        assert!(cut.ends_with("6789"));
        assert!(cut.contains("bytes truncated…"));
        assert_eq!(truncate_middle("short", 200), "short");
        let multibyte = "é".repeat(100);
        assert!(truncate_middle(&multibyte, 120).contains("bytes truncated"));
    }
}
