//! The idempotency table for mutations (docs/architecture.md §4.1 "Mutation safety", docs/adr/0008).
//!
//! Kernel candidate: a pure state machine with the clock passed in. Keys are scoped by the caller
//! (principal + client key). For each key:
//!
//! ```text
//!   (unseen) --begin--> InFlight --complete--> Done(outcome) --replay window--> Tombstone --horizon--> (forgotten)
//! ```
//!
//! - `begin` on an unseen key starts execution; on an in-flight key the caller waits; on a `Done`
//!   key it replays the recorded outcome; on a tombstoned key it answers `unknown_outcome`.
//! - A key reused with a different request (fingerprint) is a `Mismatch`, not a replay.
//! - **A key is remembered for `horizon_ms` after it was first seen and never forgotten earlier.**
//!   Memory is bounded by admission instead: while the table holds `max_keys` keys (records plus
//!   tombstones), a *new* key is refused (`Full`) until an old one ages out; at most
//!   `max_in_flight` keys execute at once (`Busy`). `max_records` bounds the outcomes kept: past
//!   it the oldest completed record becomes a tombstone early (the outcome is dropped, the key is
//!   still remembered). A sustained rate above `max_keys / horizon` is therefore refused rather
//!   than silently forgetting keys.
//! - Past its horizon a key is forgotten. The caller marks a key `stale` when it carries its
//!   minting time ([`minted_ms`]: a `UUIDv7`) and is older than the horizon, and a stale key that is
//!   not in the table answers `Expired` without executing — so for timestamped keys a retry never
//!   executes twice, whenever it comes. An opaque key retried after its horizon is
//!   indistinguishable from a new one (the documented residual).
//! - Tombstones keep a digest of the key under a per-table random hash key (not the key or the
//!   outcome), so collisions cannot be crafted; a collision only answers `Expired` for a fresh key.

use std::cmp::Reverse;
use std::collections::hash_map::RandomState;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::hash::BuildHasher as _;

/// Table bounds and lifetimes (times in milliseconds).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DedupConfig {
    /// How long a completed outcome is replayable.
    pub replay_ms: u64,
    /// How long a key is remembered after it was first seen (at least the replay window).
    pub horizon_ms: u64,
    /// Maximum completed records (with their outcomes) kept.
    pub max_records: usize,
    /// Maximum keys remembered (records plus tombstones); new keys are refused beyond it.
    pub max_keys: usize,
    /// Maximum keys executing at once; new keys are refused beyond it.
    pub max_in_flight: usize,
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
    /// The key was used (or is too old to tell), but no outcome is kept: answer `unknown_outcome`.
    Expired,
    /// The key was used for a different request: refuse.
    Mismatch,
    /// The table remembers as many keys as it may: refuse the new key (`limit_exceeded`).
    Full,
    /// As many requests execute as may: refuse the new key (`limit_exceeded`).
    Busy,
}

#[derive(Clone, Debug)]
enum Record<O> {
    InFlight { fingerprint: u64, since: u64 },
    Done { fingerprint: u64, outcome: O, expires: u64, since: u64 },
}

/// The idempotency table.
#[derive(Clone, Debug)]
pub struct DedupTable<O> {
    config: DedupConfig,
    hasher: RandomState,
    records: HashMap<String, Record<O>>,
    in_flight: usize,
    /// Completed records in completion order (expiry order, since the window is constant).
    done_order: VecDeque<(String, u64)>,
    /// Digest → expiry of each tombstone.
    tombstones: HashMap<u64, u64>,
    /// Tombstones by expiry, earliest first (entries replaced later are skipped when popped).
    tombstone_order: BinaryHeap<Reverse<(u64, u64)>>,
}

/// The minting time (Unix milliseconds) of a key that starts with a `UUIDv7` (RFC 9562), in its
/// hyphenated or 32-digit form, followed by nothing or by a character that is not a hex digit
/// (so `{uuidv7}/{step}` keys derived from one inherit its time). `None` for any other key.
#[must_use]
pub fn minted_ms(key: &str) -> Option<u64> {
    let hyphenated = key.char_indices().filter(|(_, c)| *c == '-').map(|(i, _)| i).take(4).eq([8, 13, 18, 23]);
    let (digits, rest): (String, &str) = if hyphenated {
        let head = key.get(..36)?;
        (head.chars().filter(|c| *c != '-').collect(), key.get(36..)?)
    } else {
        (key.get(..32)?.to_owned(), key.get(32..)?)
    };
    let well_formed = digits.len() == 32 && digits.chars().all(|c| c.is_ascii_hexdigit());
    let ends = rest.chars().next().is_none_or(|c| !c.is_ascii_hexdigit());
    let version = digits.get(12..13)? == "7";
    let variant = matches!(digits.get(16..17)?, "8" | "9" | "a" | "b" | "A" | "B");
    if !(well_formed && ends && version && variant) {
        return None;
    }
    u64::from_str_radix(digits.get(..12)?, 16).ok()
}

impl<O: Clone> DedupTable<O> {
    /// An empty table.
    #[must_use]
    pub fn new(config: DedupConfig) -> Self {
        Self {
            config,
            hasher: RandomState::new(),
            records: HashMap::new(),
            in_flight: 0,
            done_order: VecDeque::new(),
            tombstones: HashMap::new(),
            tombstone_order: BinaryHeap::new(),
        }
    }

    fn digest(&self, key: &str) -> u64 {
        self.hasher.hash_one(key)
    }

    /// Decides what to do with a request for `key` whose parameters hash to `fingerprint`. `stale`
    /// says the key's own timestamp is older than the horizon: unless the table still holds it,
    /// it answers `Expired` and is not executed.
    pub fn begin(&mut self, key: &str, fingerprint: u64, stale: bool, now: u64) -> Begin<O> {
        self.expire(now);
        match self.records.get(key) {
            Some(Record::InFlight { fingerprint: seen, .. }) => {
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
            None if stale || self.tombstones.contains_key(&self.digest(key)) => Begin::Expired,
            None if self.keys() >= self.config.max_keys => Begin::Full,
            None if self.in_flight >= self.config.max_in_flight => Begin::Busy,
            None => {
                self.records.insert(key.to_owned(), Record::InFlight { fingerprint, since: now });
                self.in_flight += 1;
                Begin::Execute
            }
        }
    }

    /// Records the outcome of an execution started by [`Begin::Execute`].
    pub fn complete(&mut self, key: &str, outcome: O, now: u64) {
        let (fingerprint, since) = match self.records.get(key) {
            Some(Record::InFlight { fingerprint, since }) => (*fingerprint, *since),
            // Completing a key that is not in flight is a caller bug; keep the table unchanged.
            Some(Record::Done { .. }) | None => return,
        };
        self.in_flight = self.in_flight.saturating_sub(1);
        let expires = now.saturating_add(self.config.replay_ms);
        self.records.insert(key.to_owned(), Record::Done { fingerprint, outcome, expires, since });
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
            self.in_flight = self.in_flight.saturating_sub(1);
        }
    }

    /// Turns records past their replay window into tombstones and forgets keys past their horizon.
    pub fn expire(&mut self, now: u64) {
        while let Some((key, expires)) = self.done_order.front() {
            if *expires > now {
                break;
            }
            let (key, expires) = (key.clone(), *expires);
            self.done_order.pop_front();
            self.bury(&key, expires, now);
        }
        while let Some(Reverse((expires, hash))) = self.tombstone_order.peek().copied() {
            if expires > now {
                break;
            }
            self.tombstone_order.pop();
            if self.tombstones.get(&hash) == Some(&expires) {
                self.tombstones.remove(&hash);
            }
        }
    }

    /// Replaces the completed record of `key` that expires at `expires` with a tombstone that
    /// lasts until the key's horizon (an older queue entry for a key completed again later is
    /// ignored).
    fn bury(&mut self, key: &str, expires: u64, now: u64) {
        let since = match self.records.get(key) {
            Some(Record::Done { expires: e, since, .. }) if *e == expires => *since,
            _ => return,
        };
        self.records.remove(key);
        let until = since.saturating_add(self.config.horizon_ms);
        if until > now {
            let hash = self.digest(key);
            let until = self.tombstones.get(&hash).map_or(until, |other| until.max(*other));
            self.tombstones.insert(hash, until);
            self.tombstone_order.push(Reverse((until, hash)));
        }
    }

    /// Keys currently remembered (records in flight or completed, plus tombstones).
    #[must_use]
    pub fn keys(&self) -> usize {
        self.records.len().saturating_add(self.tombstones.len())
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

    const CONFIG: DedupConfig = DedupConfig { replay_ms: 100, horizon_ms: 1_000, max_records: 4, max_keys: 8, max_in_flight: 4 };

    #[test]
    fn lifecycle() {
        let mut table = DedupTable::new(CONFIG);
        assert_eq!(table.begin("k", 1, false, 0), Begin::Execute);
        assert_eq!(table.begin("k", 1, false, 1), Begin::InFlight);
        assert_eq!(table.begin("k", 2, false, 1), Begin::Mismatch);
        table.complete("k", "done", 10);
        assert_eq!(table.begin("k", 1, false, 50), Begin::Replay("done"));
        assert_eq!(table.begin("k", 2, false, 50), Begin::Mismatch);
        assert_eq!(table.begin("k", 1, false, 110), Begin::Expired);
        assert_eq!(table.records(), 0);
        assert_eq!(table.tombstones(), 1);
        // Remembered until the horizon (from first sight at 0) …
        assert_eq!(table.begin("k", 1, false, 999), Begin::Expired);
        // … after which an opaque key is forgotten, but a stale timestamped one is still refused.
        assert_eq!(table.begin("k", 1, true, 1_000), Begin::Expired);
        assert_eq!(table.tombstones(), 0);
        assert_eq!(table.begin("k", 1, false, 1_000), Begin::Execute);
    }

    #[test]
    fn stale_keys_never_execute_but_live_records_still_replay() {
        let mut table = DedupTable::new(CONFIG);
        assert_eq!(table.begin("old", 1, true, 0), Begin::Expired);
        assert_eq!(table.records(), 0);
        assert_eq!(table.begin("k", 1, false, 0), Begin::Execute);
        table.complete("k", 7, 0);
        assert_eq!(table.begin("k", 1, true, 1), Begin::Replay(7));
    }

    #[test]
    fn abandon_allows_retry() {
        let mut table: DedupTable<u8> = DedupTable::new(CONFIG);
        assert_eq!(table.begin("k", 1, false, 0), Begin::Execute);
        table.abandon("k");
        assert_eq!(table.begin("k", 1, false, 0), Begin::Execute);
    }

    #[test]
    fn overflowing_records_become_tombstones_early() {
        let mut table = DedupTable::new(CONFIG);
        for i in 0..5 {
            let key = format!("k{i}");
            assert_eq!(table.begin(&key, 0, false, 0), Begin::Execute);
            table.complete(&key, i, 0);
        }
        assert_eq!(table.begin("k0", 0, false, 1), Begin::Expired);
        assert_eq!(table.begin("k4", 0, false, 1), Begin::Replay(4));
    }

    #[test]
    fn a_full_table_refuses_new_keys_and_keeps_old_ones() {
        let mut table = DedupTable::new(CONFIG);
        for i in 0..8 {
            let key = format!("k{i}");
            assert_eq!(table.begin(&key, 0, false, 0), Begin::Execute);
            table.complete(&key, i, 0);
        }
        assert_eq!(table.begin("new", 0, false, 500), Begin::Full);
        for i in 0..8 {
            assert_eq!(table.begin(&format!("k{i}"), 0, false, 500), Begin::Expired, "k{i} is still remembered");
        }
        // Once the old keys pass their horizon, there is room again.
        assert_eq!(table.begin("new", 0, false, 1_000), Begin::Execute);
    }

    #[test]
    fn in_flight_keys_are_capped() {
        let mut table: DedupTable<u8> = DedupTable::new(CONFIG);
        for i in 0..4 {
            assert_eq!(table.begin(&format!("k{i}"), 0, false, 0), Begin::Execute);
        }
        assert_eq!(table.begin("k4", 0, false, 0), Begin::Busy);
        table.complete("k0", 0, 1);
        assert_eq!(table.begin("k4", 0, false, 1), Begin::Execute);
        table.abandon("k4");
        assert_eq!(table.begin("k5", 0, false, 1), Begin::Execute);
    }

    #[test]
    fn uuid_v7_minting_times() {
        // RFC 9562 appendix A.6's example: 017F22E2-79B0-7CC3-98C4-DC0C0C07398F, 2022-02-22 14:22:22.
        assert_eq!(minted_ms("017f22e2-79b0-7cc3-98c4-dc0c0c07398f"), Some(0x017F_22E2_79B0));
        assert_eq!(minted_ms("017F22E279B07CC398C4DC0C0C07398F"), Some(0x017F_22E2_79B0));
        assert_eq!(minted_ms("017f22e2-79b0-7cc3-98c4-dc0c0c07398f/spawn"), Some(0x017F_22E2_79B0));
        assert_eq!(minted_ms("017f22e279b07cc398c4dc0c0c07398f/spawn"), Some(0x017F_22E2_79B0));
        // Other versions, bad variants, longer hex strings and anything else carry no time.
        assert_eq!(minted_ms("017f22e2-79b0-4cc3-98c4-dc0c0c07398f"), None);
        assert_eq!(minted_ms("017f22e2-79b0-7cc3-c8c4-dc0c0c07398f"), None);
        assert_eq!(minted_ms("017f22e279b07cc398c4dc0c0c07398f0"), None);
        assert_eq!(minted_ms("017f22e2-79b0-7cc3-98c4-dc0c0c07398"), None);
        assert_eq!(minted_ms("k-1-2"), None);
        assert_eq!(minted_ms(""), None);
        assert_eq!(minted_ms("é17f22e2-79b0-7cc3-98c4-dc0c0c07398f"), None);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Request { key: u8, fingerprint: u8 },
        Complete { key: u8 },
        Tick(u16),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..12, 0u8..2).prop_map(|(key, fingerprint)| Op::Request { key, fingerprint }),
            (0u8..12).prop_map(|key| Op::Complete { key }),
            (0u16..400).prop_map(Op::Tick),
        ]
    }

    proptest! {
        /// Safety: within its horizon a key executes at most once, however small the tables are;
        /// every replay returns the outcome recorded by that execution; and the table never
        /// remembers more than `max_keys` keys or runs more than `max_in_flight` at once.
        #[test]
        fn at_most_once_within_the_horizon(ops in prop::collection::vec(op(), 0..300)) {
            let config = DedupConfig { replay_ms: 100, horizon_ms: 100_000, max_records: 3, max_keys: 6, max_in_flight: 2 };
            let mut table = DedupTable::new(config);
            let mut now = 0u64;
            let mut executions: HashMap<u8, u32> = HashMap::new();
            let mut in_flight: HashMap<u8, bool> = HashMap::new();
            for op in ops {
                match op {
                    Op::Tick(ms) => now += u64::from(ms),
                    Op::Request { key, fingerprint } => {
                        let name = format!("k{key}");
                        match table.begin(&name, u64::from(fingerprint), false, now) {
                            Begin::Execute => {
                                *executions.entry(key).or_default() += 1;
                                in_flight.insert(key, true);
                            }
                            Begin::Replay(outcome) => prop_assert_eq!(outcome, key),
                            Begin::InFlight => prop_assert!(in_flight.get(&key).copied().unwrap_or(false)),
                            Begin::Expired | Begin::Mismatch | Begin::Full | Begin::Busy => {}
                        }
                    }
                    Op::Complete { key } => {
                        if in_flight.remove(&key).unwrap_or(false) {
                            table.complete(&format!("k{key}"), key, now);
                        }
                    }
                }
                prop_assert!(table.keys() <= 6);
                prop_assert!(in_flight.len() <= 2);
            }
            for count in executions.values() {
                prop_assert!(*count <= 1, "a key executed {count} times");
            }
        }
    }
}
