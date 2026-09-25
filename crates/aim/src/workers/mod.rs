//! Board worker sessions and serialized integration (ADR 0048).

mod harness;
mod integration;
mod receipt;
mod runner;

pub use integration::Integrator;
pub use runner::{Runner, RunnerOptions, WorkSummary};
