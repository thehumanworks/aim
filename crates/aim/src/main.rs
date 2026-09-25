//! `aim` — the agent layer's binary.
//!
//! - `aim run [PROMPT]` — one headless turn in the current workspace (milestone M2a).
//! - `aim sessions` — recent sessions.
#![expect(clippy::print_stderr, reason = "the CLI reports errors on stderr")]

use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aim::cli::{self, RunOptions};
use aim::store::{SessionStore, SqliteStore};
use aim_llm::ModelProvider;
use clap::{Parser, Subcommand};

/// aim — Agent I am.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one turn headlessly in a workspace.
    Run {
        /// Provider: openrouter, ai-gateway (codex and acp:claude as they land).
        #[arg(short, long, default_value = "codex")]
        provider: String,
        /// Model id (the provider's default when omitted).
        #[arg(short, long)]
        model: Option<String>,
        /// Reasoning effort (from the model's catalog ladder).
        #[arg(short, long)]
        effort: Option<String>,
        /// Workspace directory.
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
        /// The aimx binary (default: next to aim, else on PATH).
        #[arg(long)]
        aimx: Option<PathBuf>,
        /// Keep nothing on disk.
        #[arg(long)]
        ephemeral: bool,
        /// Print every event as a JSON line.
        #[arg(long)]
        json: bool,
        /// Most model requests in the turn.
        #[arg(long, default_value_t = 64)]
        max_requests: u32,
        /// The prompt (read from stdin when omitted or `-`).
        prompt: Vec<String>,
    },
    /// List recent sessions.
    Sessions {
        /// How many.
        #[arg(short, long, default_value_t = 20)]
        limit: u32,
    },
}

fn provider(name: &str, model: Option<&str>) -> Result<(Arc<dyn ModelProvider>, String), String> {
    aim::providers::build(name, model)
}

fn read_prompt(words: &[String]) -> Result<String, String> {
    let joined = words.join(" ");
    if joined.is_empty() || joined == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(|e| format!("reading stdin: {e}"))?;
        return Ok(text);
    }
    Ok(joined)
}

async fn main_async(args: Args) -> Result<i32, String> {
    match args.command {
        Command::Run { provider: p, model, effort, cwd, aimx, ephemeral, json, max_requests, prompt } => {
            let prompt = read_prompt(&prompt)?;
            if prompt.trim().is_empty() {
                return Err("empty prompt".to_owned());
            }
            let options = RunOptions { provider: p, model, effort, cwd, aimx, ephemeral, json, max_requests, prompt };
            cli::run(options, provider).await
        }
        Command::Sessions { limit } => {
            let store = SqliteStore::open(&cli::aim_home().join("aim.db")).map_err(|e| e.to_string())?;
            let sessions = store.list(limit).await.map_err(|e| e.to_string())?;
            for s in sessions {
                eprintln!("{}  {}  {}/{}  {}", s.id, s.created_ms, s.provider, s.model, s.workspace);
            }
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("aim: cannot start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(main_async(args)) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(message) => {
            eprintln!("aim: {message}");
            ExitCode::FAILURE
        }
    }
}
