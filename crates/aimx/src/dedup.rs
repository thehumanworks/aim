//! The idempotency table for mutations (docs/architecture.md §4.1 "Mutation safety", docs/adr/0008).
//!
//! Kernel candidate: a pure state machine with the clock passed in. Keys are scoped by the caller
//! (principal + client key). For each key:
//!
//! ```text
//!   (unseen) --begin--> InFlight --complete--> Done(outcome) --window elapses--> Tombstone --ttl--> (unseen)
//! ```
//!
//! - `begin` on an unseen key starts execution; on an in-flight key the caller waits; on a `Done`
//!   key it replays the recorded outcome; on a tombstoned key it answers `unknown_outcome` —
//!   **never** a second execution while the tombstone lives.
//! - A key reused with a different request (fingerprint) is a `Mismatch`, not a replay.
//! - Records are bounded: past `max_records` the oldest completed record becomes a tombstone
//!   early; past `max_tombstones` the oldest tombstone is forgotten (only then can a very old key
//!   execute again). Tombstones keep a 64-bit digest of the key, not the key or outcome.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

/// Table bounds and lifetimes (times in milliseconds).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DedupConfig {
    /// How long a completed outcome is replayable.
    pub window_ms: u64,
    /// How long a key is remembered as seen after its outcome expired.
    pub tombstone_ms: u64,
    /// Maximum completed records kept.
    pub max_records: usize,
    /// Maximum tombstones kept.
    pub max_tombstones: usize,
}

/// What to do with a request carrying an idempotency key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Begin<O> {
    /// First sight of the key: execute, then call [`DedupTable::complete`].
    Execute,
    /// The same request already completed: return this outcome without executing.
    Replay(O),
    /// The same request is executing now: wait for it to complete, then begin again.
    InFlight,
    /// The key was used, but its outcome expired: answer `unknown_outcome`.
    Expired,
    /// The key was used for a different request: refuse.
    Mismatch,
}

#[derive(Clone, Debug)]
enum Record<O> {
    InFlight { fingerprint: u64 },
    Done { fingerprint: u64, outcome: O, expires: u64 },
}

/// The idempotency table.
#[derive(Clone, Debug)]
pub struct DedupTable<O> {
    config: DedupConfig,
    records: HashMap<String, Record<O>>,
    /// Completed records in completion order (expiry order, since the window is constant).
    done_order: VecDeque<(String, u64)>,
    tombstones: HashMap<u64, u64>,
    tombstone_order: VecDeque<(u64, u64)>,
}

/// The compact digest a tombstone keeps for a key.
fn digest(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

impl<O: Clone> DedupTable<O> {
    /// An empty table.
    #[must_use]
    pub fn new(config: DedupConfig) -> Self {
        Self { config, records: HashMap::new(), done_order: VecDeque::new(), tombstones: HashMap::new(), tombstone_order: VecDeque::new() }
    }

    /// Decides what to do with a request for `key` whose parameters hash to `fingerprint`.
    pub fn begin(&mut self, key: &str, fingerprint: u64, now: u64) -> Begin<O> {
        self.expire(now);
        match self.records.get(key) {
            Some(Record::InFlight { fingerprint: seen }) => {
                if *seen == fingerprint {
                    Begin::InFlight
                } else {
                    Begin::Mismatch
                }
            }
            Some(Record::Done { fingerprint: seen, outcome, .. }) => {
                if *seen == fingerprint {
                    Begin::Replay(outcome.clone())
                } else {
                    Begin::Mismatch
                }
            }
            None if self.tombstones.contains_key(&digest(key)) => Begin::Expired,
            None => {
                self.records.insert(key.to_owned(), Record::InFlight { fingerprint });
                Begin::Execute
            }
        }
    }

    /// Records the outcome of an execution started by [`Begin::Execute`].
    pub fn complete(&mut self, key: &str, outcome: O, now: u64) {
        let fingerprint = match self.records.get(key) {
            Some(Record::InFlight { fingerprint }) => *fingerprint,
            // Completing a key that is not in flight is a caller bug; keep the table unchanged.
            Some(Record::Done { .. }) | None => return,
        };
        let expires = now.saturating_add(self.config.window_ms);
        self.records.insert(key.to_owned(), Record::Done { fingerprint, outcome, expires });
        self.done_order.push_back((key.to_owned(), expires));
        while self.done_order.len() > self.config.max_records {
            if let Some((old, old_expires)) = self.done_order.pop_front() {
                self.bury(&old, old_expires, now);
            }
        }
    }

    /// Forgets an in-flight key whose execution never started (so a retry may execute).
    pub fn abandon(&mut self, key: &str) {
        if matches!(self.records.get(key), Some(Record::InFlight { .. })) {
            self.records.remove(key);
        }
    }

    /// Turns expired records into tombstones and forgets expired tombstones.
    pub fn expire(&mut self, now: u64) {
        while let Some((key, expires)) = self.done_order.front() {
            if *expires > now {
                break;
            }
            let (key, expires) = (key.clone(), *expires);
            self.done_order.pop_front();
            self.bury(&key, expires, now);
        }
        while let Some((hash, expires)) = self.tombstone_order.front() {
            if *expires > now {
                break;
            }
            let (hash, expires) = (*hash, *expires);
            self.tombstone_order.pop_front();
            if self.tombstones.get(&hash) == Some(&expires) {
                self.tombstones.remove(&hash);
            }
        }
    }

    /// Replaces the completed record of `key` that expires at `expires` with a tombstone (an older
    /// queue entry for a key that was completed again later is ignored).
    fn bury(&mut self, key: &str, expires: u64, now: u64) {
        if matches!(self.records.get(key), Some(Record::Done { expires: e, .. }) if *e == expires) {
            self.records.remove(key);
            let hash = digest(key);
            let expires = now.saturating_add(self.config.tombstone_ms);
            self.tombstones.insert(hash, expires);
            self.tombstone_order.push_back((hash, expires));
            while self.tombstone_order.len() > self.config.max_tombstones {
                if let Some((old, old_expires)) = self.tombstone_order.pop_front()
                    && self.tombstones.get(&old) == Some(&old_expires)
                {
                    self.tombstones.remove(&old);
                }
            }
        }
    }

    /// Records (in flight or completed) currently held.
    #[must_use]
    pub fn records(&self) -> usize {
        self.records.len()
    }

    /// Tombstones currently held.
    #[must_use]
    pub fn tombstones(&self) -> usize {
        self.tombstones.len()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const CONFIG: DedupConfig = DedupConfig { window_ms: 100, tombstone_ms: 1_000, max_records: 4, max_tombstones: 8 };

    #[test]
    fn lifecycle() {
        let mut table = DedupTable::new(CONFIG);
        assert_eq!(table.begin("k", 1, 0), Begin::Execute);
        assert_eq!(table.begin("k", 1, 1), Begin::InFlight);
        assert_eq!(table.begin("k", 2, 1), Begin::Mismatch);
        table.complete("k", "done", 10);
        assert_eq!(table.begin("k", 1, 50), Begin::Replay("done"));
        assert_eq!(table.begin("k", 2, 50), Begin::Mismatch);
        assert_eq!(table.begin("k", 1, 110), Begin::Expired);
        assert_eq!(table.records(), 0);
        assert_eq!(table.tombstones(), 1);
        assert_eq!(table.begin("k", 1, 1_200), Begin::Execute);
    }

    #[test]
    fn abandon_allows_retry() {
        let mut table: DedupTable<u8> = DedupTable::new(CONFIG);
        assert_eq!(table.begin("k", 1, 0), Begin::Execute);
        table.abandon("k");
        assert_eq!(table.begin("k", 1, 0), Begin::Execute);
    }

    #[test]
    fn overflow_buries_oldest() {
        let mut table = DedupTable::new(CONFIG);
        for i in 0..5 {
            let key = format!("k{i}");
            assert_eq!(table.begin(&key, 0, 0), Begin::Execute);
            table.complete(&key, i, 0);
        }
        assert_eq!(table.begin("k0", 0, 1), Begin::Expired);
        assert_eq!(table.begin("k4", 0, 1), Begin::Replay(4));
    }

    #[derive(Clone, Debug)]
    enum Op {
        Request { key: u8, fingerprint: u8 },
        Complete { key: u8 },
        Tick(u16),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..6, 0u8..2).prop_map(|(key, fingerprint)| Op::Request { key, fingerprint }),
            (0u8..6).prop_map(|key| Op::Complete { key }),
            (0u16..400).prop_map(Op::Tick),
        ]
    }

    proptest! {
        /// Safety: while a key's tombstone lives (which covers the whole dedup window plus the
        /// tombstone lifetime when the tables do not overflow), it executes at most once; every
        /// replay returns the outcome recorded by that execution.
        #[test]
        fn at_most_once(ops in prop::collection::vec(op(), 0..200)) {
            let config = DedupConfig { window_ms: 100, tombstone_ms: 100_000, max_records: 64, max_tombstones: 64 };
            let mut table = DedupTable::new(config);
            let mut now = 0u64;
            let mut executions: HashMap<u8, u32> = HashMap::new();
            let mut in_flight: HashMap<u8, bool> = HashMap::new();
            for op in ops {
                match op {
                    Op::Tick(ms) => now += u64::from(ms),
                    Op::Request { key, fingerprint } => {
                        let name = format!("k{key}");
                        match table.begin(&name, u64::from(fingerprint), now) {
                            Begin::Execute => {
                                *executions.entry(key).or_default() += 1;
                                in_flight.insert(key, true);
                            }
                            Begin::Replay(outcome) => prop_assert_eq!(outcome, key),
                            Begin::InFlight => prop_assert!(in_flight.get(&key).copied().unwrap_or(false)),
                            Begin::Expired | Begin::Mismatch => {}
                        }
                    }
                    Op::Complete { key } => {
                        if in_flight.remove(&key).unwrap_or(false) {
                            table.complete(&format!("k{key}"), key, now);
                        }
                    }
                }
            }
            for count in executions.values() {
                prop_assert!(*count <= 1, "a key executed {count} times");
            }
        }
    }
}
