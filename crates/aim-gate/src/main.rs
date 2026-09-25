//! Command-line entry point for the pinned self-improvement gate.

use std::io::Write as _;
use std::path::PathBuf;

use aim_gate::runtime::{GateRuntime, InitOptions};
use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "aim-gate", about = "Evaluate and promote aim candidates through the pinned gate")]
struct Cli {
    /// Private gate state directory.
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Initialize the private gate home and evaluator pin.
    Init {
        /// Local source repository containing the candidate commits.
        #[arg(long)]
        repository: PathBuf,
        /// Trusted source tree containing the pinned benchmark evaluator.
        #[arg(long)]
        evaluator_root: PathBuf,
        /// Git remote used for the trial and ledger refs.
        #[arg(long, default_value = "origin")]
        origin: String,
        /// Toolchain or cache root that candidates may read.
        #[arg(long)]
        readable_root: Vec<PathBuf>,
        /// Tool directory that candidates may execute from.
        #[arg(long)]
        executable_root: Vec<PathBuf>,
        /// Read-only macOS SDK used by native builds inside Seatbelt.
        #[arg(long)]
        sdk_root: Option<PathBuf>,
        /// Maximum paid proposal spend in cents; zero keeps the paid tier disabled.
        #[arg(long, default_value_t = 0)]
        paid_spend_cap_cents: u32,
        /// Catalog model id allowed through the paid proposal broker.
        #[arg(long)]
        proposal_model: Option<String>,
    },
    /// Ask the agent for a small improvement in a fresh confined clone.
    Propose {
        /// Concrete improvement goal for the candidate agent.
        #[arg(long)]
        goal: String,
    },
    /// Evaluate a committed candidate against the pinned baseline.
    Evaluate {
        /// Full candidate commit SHA.
        sha: String,
    },
    /// Promote an evaluated candidate to gate/trial and deploy it.
    Promote {
        /// Gate candidate identifier returned by propose.
        id: String,
    },
    /// Restore the predecessor deployment for an active candidate.
    Rollback {
        /// Gate candidate identifier to roll back.
        id: String,
    },
}

fn default_home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME is unset; pass --home")?).join(".aim-gate"))
}

fn emit(value: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(value.as_bytes())?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = match cli.home {
        Some(path) => path,
        None => default_home()?,
    };
    match cli.action {
        Action::Init {
            repository,
            evaluator_root,
            origin,
            readable_root,
            executable_root,
            sdk_root,
            paid_spend_cap_cents,
            proposal_model,
        } => {
            let home_label = home.display().to_string();
            let _gate = GateRuntime::init(InitOptions {
                home,
                repository,
                evaluator_root,
                origin,
                readable_roots: readable_root,
                executable_roots: executable_root,
                sdk_root,
                paid_spend_cap_cents,
                proposal_model,
                commands: None,
                fixture_manifest: None,
            })?;
            emit(&format!("initialized {home_label}"))?;
        }
        Action::Propose { goal } => {
            let gate = GateRuntime::open(&home)?;
            emit(&gate.propose(&goal)?)?;
        }
        Action::Evaluate { sha } => {
            let gate = GateRuntime::open(&home)?;
            emit(&serde_json::to_string_pretty(&gate.evaluate(&sha)?)?)?;
        }
        Action::Promote { id } => {
            GateRuntime::open(&home)?.promote(&id)?;
            emit(&format!("promoted {id}"))?;
        }
        Action::Rollback { id } => {
            GateRuntime::open(&home)?.rollback(&id)?;
            emit(&format!("rolled back {id}"))?;
        }
    }
    Ok(())
}
