//! The `aim-harness/1` conformance suite for the local `aimx` (milestone M1a,
//! docs/architecture.md §15): a real server on a tempdir unix socket, driven through
//! `aim_rpc::Peer`, plus the spawned binary over `--unix` and `--stdio`.

// mod binary;
#[cfg(test)]
mod common;
#[cfg(test)]
mod exec;
#[cfg(test)]
mod fs;
#[cfg(test)]
mod handshake;
#[cfg(test)]
mod idempotency;
#[cfg(test)]
mod resume;
#[cfg(test)]
mod safety;
// mod search;
// mod tools;
