//! Trusted self-improvement gate components. Candidate code never imports or controls this crate.

pub mod bench;
pub mod broker;
mod cache;
pub mod checkpoint;
pub mod deploy;
pub mod formats;
pub mod promotion;
pub mod runner;
pub mod runtime;
pub mod sandbox;

#[cfg(test)]
mod runtime_tests;
