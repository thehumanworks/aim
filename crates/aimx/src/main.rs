//! `aimx` — aim's execution layer binary.
//!
//! - `aimx serve --unix PATH [--root DIR]… [--read-only]` serves `aim-harness/1` on a unix socket
//!   (many connections; peers must run as the same OS user).
//! - `aimx serve --stdio …` serves one connection on stdin/stdout (what SSH bootstrap runs).
//! - `aimx version` prints the version and the protocol generations.
//!
//! Logs go to stderr (never stdout, which carries the protocol in `--stdio` mode); the level comes
//! from `AIMX_LOG` (default `warn`: aimx is quiet under its client).

use std::path::PathBuf;
use std::process::ExitCode;

use aimx::server::{Server, ServerConfig, default_protected, local_principal};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "aimx", about = "aim's execution layer", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve aim-harness/1.
    Serve(ServeArgs),
    /// Print the version and the protocol generations spoken.
    Version,
}

#[derive(Args)]
struct ServeArgs {
    /// Listen on this unix socket.
    #[arg(long, value_name = "PATH", conflicts_with = "stdio", required_unless_present = "stdio")]
    unix: Option<PathBuf>,
    /// Serve one connection on stdin/stdout.
    #[arg(long)]
    stdio: bool,
    /// A directory clients may open workspaces under (repeatable; default: the home directory).
    #[arg(long = "root", value_name = "DIR")]
    roots: Vec<PathBuf>,
    /// Grant read access only: no writes, no processes, no mutating tools.
    #[arg(long)]
    read_only: bool,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AIMX_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(filter).init();
}

#[expect(clippy::print_stdout, reason = "`aimx version` answers on stdout")]
fn version() {
    let (min, max) = aim_proto::HARNESS_GENERATIONS;
    println!("aimx {} (aim-harness generations {min}..={max})", env!("CARGO_PKG_VERSION"));
}

async fn serve(args: ServeArgs) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_owned())?;
    let roots = if args.roots.is_empty() { vec![PathBuf::from(&home)] } else { args.roots };
    let principal = local_principal(&roots, args.read_only).map_err(|err| format!("invalid --root: {err}"))?;
    let config = ServerConfig::new(principal, default_protected(&home));
    tracing::info!(principal = %config.principal.id, roots = ?config.principal.roots, read_only = config.principal.read_only, "starting aimx");
    let server = Server::new(config);
    match args.unix {
        Some(path) => {
            let listener = Server::bind_unix(&path).map_err(|err| format!("{}: {err}", path.display()))?;
            tracing::info!(socket = %path.display(), "serving aim-harness/1");
            tokio::select! {
                outcome = server.serve_listener(listener) => outcome.map_err(|err| format!("accept: {err}"))?,
                _ = tokio::signal::ctrl_c() => tracing::info!("interrupted; shutting down"),
            }
            server.shutdown().await;
            if let Err(err) = std::fs::remove_file(&path) {
                tracing::debug!(%err, "could not remove the socket");
            }
        }
        None => server.serve_stdio().await,
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Version => {
            version();
            ExitCode::SUCCESS
        }
        Cmd::Serve(args) => {
            init_logging();
            match serve(args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    tracing::error!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
