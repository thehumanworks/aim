//! The `x-codex-turn-state` sticky-routing token, scoped to one turn.
//!
//! Codex's contract: the token is received at turn start, sent back unchanged on every request of
//! that turn, and never sent on a different turn (refs:core/src/client.rs:283-304, where it lives in
//! a per-turn `OnceLock`, so the first value wins). aim keys it by
//! `(Request.session_id, Request.turn_id)`; a request without both never sends or stores one.
//! One entry per session (a new turn replaces the old one) and at most `capacity` sessions, oldest
//! evicted first, so the map is bounded.

use std::collections::VecDeque;

struct Entry {
    session: String,
    turn: String,
    state: String,
}

/// Per-turn routing tokens (pure bookkeeping, no I/O).
pub(crate) struct TurnStates {
    entries: VecDeque<Entry>,
    capacity: usize,
}

impl TurnStates {
    /// An empty map holding at most `capacity` sessions.
    pub(crate) fn new(capacity: usize) -> Self {
        Self { entries: VecDeque::new(), capacity: capacity.max(1) }
    }

    fn position(&self, session: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.session == session)
    }

    /// The token to echo on a request of `turn`. A token of an earlier turn of the same session
    /// is forgotten, never sent.
    pub(crate) fn get(&mut self, session: &str, turn: &str) -> Option<String> {
        let index = self.position(session)?;
        if self.entries.get(index).is_some_and(|entry| entry.turn == turn) {
            return self.entries.get(index).map(|entry| entry.state.clone());
        }
        self.entries.remove(index);
        None
    }

    /// Records the token a response of `turn` carried; the first token of a turn wins.
    pub(crate) fn record(&mut self, session: &str, turn: &str, state: String) {
        if let Some(index) = self.position(session) {
            if self.entries.get(index).is_some_and(|entry| entry.turn == turn) {
                return;
            }
            self.entries.remove(index);
        }
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry { session: session.to_owned(), turn: turn.to_owned(), state });
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_value_wins_within_a_turn_and_never_crosses_turns() {
        let mut states = TurnStates::new(8);
        assert_eq!(states.get("s", "t1"), None);
        states.record("s", "t1", "first".into());
        states.record("s", "t1", "second".into());
        assert_eq!(states.get("s", "t1").as_deref(), Some("first"));
        // A new turn never sees the old token, and the old one is gone for good.
        assert_eq!(states.get("s", "t2"), None);
        assert_eq!(states.get("s", "t1"), None);
        states.record("s", "t2", "third".into());
        assert_eq!(states.get("s", "t2").as_deref(), Some("third"));
        // Sessions are independent.
        states.record("other", "t1", "x".into());
        assert_eq!(states.get("s", "t2").as_deref(), Some("third"));
        assert_eq!(states.get("other", "t1").as_deref(), Some("x"));
    }

    #[test]
    fn bounded() {
        let mut states = TurnStates::new(3);
        for session in 0..10 {
            states.record(&session.to_string(), "t", "v".into());
        }
        assert_eq!(states.len(), 3);
        assert_eq!(states.get("0", "t"), None);
        assert_eq!(states.get("9", "t").as_deref(), Some("v"));
        // A later turn of a session replaces its entry instead of adding one.
        states.record("9", "t2", "w".into());
        assert_eq!(states.len(), 3);
    }
}
