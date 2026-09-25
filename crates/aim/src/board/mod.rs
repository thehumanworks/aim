//! The durable blackboard service (ADR 0019).
//!
//! The board owns one actor thread on the daemon's SQLite file. Its mutation methods run the
//! verified kernel transition before writing a projection and an event in one transaction.

pub mod cli;
pub mod integration;
mod service;
mod sqlite;
pub mod tools;

use std::path::Path;

use sqlite::Ledger;
use tokio::sync::broadcast;

use aim_proto::board::BoardEvent;

pub use aim_proto::board::CleanupReceipt;

/// A rejected board operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The addressed run, job, or attempt does not exist.
    NotFound,
    /// The caller provided an invalid or expired claim.
    StaleClaim,
    /// The requested transition was refused by the verified lifecycle.
    Conflict(String),
    /// The request violates a bound or data constraint.
    Invalid(String),
    /// SQLite or the actor failed.
    Storage(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound => f.write_str("board item not found"),
            Self::StaleClaim => f.write_str("stale or invalid claim"),
            Self::Conflict(message) => write!(f, "board conflict: {message}"),
            Self::Invalid(message) => write!(f, "invalid board request: {message}"),
            Self::Storage(message) => write!(f, "board storage: {message}"),
        }
    }
}

impl core::error::Error for Error {}

/// A durable board service. Clones share one SQLite actor and one watch stream.
#[derive(Clone, Debug)]
pub struct Board {
    ledger: Ledger,
    events: broadcast::Sender<BoardEvent>,
}

impl Board {
    /// Opens the ledger in the same `aim.db` file used by the session store.
    ///
    /// # Errors
    /// Opening, securing, or migrating the database failed.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let (events, _) = broadcast::channel(256);
        Ok(Self { ledger: Ledger::open_with_events(path, events.clone())?, events })
    }

    /// Subscribe to committed event hints. Reconcile gaps with [`Self::poll`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<BoardEvent> {
        self.events.subscribe()
    }
}
