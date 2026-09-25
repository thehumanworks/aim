//! `aim` — the agent layer's binary.
//!
//! - `aim run [PROMPT]` — one headless turn in the current workspace (milestone M2a).
//! - `aim sessions` — recent sessions.
//! - `aim daemon` — serve local sessions over `aim-daemon/1`.
#![expect(clippy::print_stderr, reason = "the CLI reports errors on stderr")]
#![expect(clippy::print_stdout, reason = "daemon status reports to stdout")]

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aim::cli::{self, RunOptions};
use aim::daemon::{client::DaemonClient, server, socket_path};
use aim::host::{HostConfig, SessionClient, SessionHost};
use aim::store::{SessionStore, SqliteStore};
use aim_llm::ModelProvider;
use aim_proto::daemon::SessionListParams;
use clap::{Parser, Subcommand};

/// aim — Agent I am.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum LoginTarget {
    /// ChatGPT (the codex provider): browser sign-in, or `--device` for a code to enter elsewhere.
    Codex {
        /// Use the device-code flow (for machines without a browser).
        #[arg(long)]
        device: bool,
    },
    /// Claude Code (acp:claude), through its own terminal login.
    Claude {
        /// Login method id (default: the adapter's first terminal method).
        #[arg(long)]
        method: Option<String>,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Run one turn headlessly in a workspace.
    Run {
        /// Provider: codex, openrouter, ai-gateway, acp:claude (strict) or acp:claude-native.
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
        /// SSH destination for a remote workspace.
        #[arg(long)]
        ssh: Option<String>,
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
    /// Sign in to a provider.
    Login {
        #[command(subcommand)]
        target: LoginTarget,
    },
    /// List recent sessions.
    Sessions {
        /// How many.
        #[arg(short, long, default_value_t = 20)]
        limit: u32,
    },
    /// Serve or inspect the local session daemon.
    Daemon {
        /// Unix socket path (default: `$AIM_HOME/run/daemon.sock`).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Stop after this many seconds without connections or running turns.
        #[arg(long)]
        idle_exit: Option<u64>,
        /// Inspect or stop the daemon.
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Show the generation, process id and session count.
    Status,
    /// Send SIGTERM to the running daemon.
    Stop,
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
        Command::Run { provider: p, model, effort, cwd, ssh, aimx, ephemeral, json, max_requests, prompt } => {
            let prompt = read_prompt(&prompt)?;
            if prompt.trim().is_empty() {
                return Err("empty prompt".to_owned());
            }
            let options = RunOptions { provider: p, model, effort, cwd, ssh, aimx, ephemeral, json, max_requests, prompt };
            cli::run(options, provider).await
        }
        Command::Login { target } => {
            let mut say = |line: &str| eprintln!("{line}");
            match target {
                LoginTarget::Codex { device } => aim::login::codex(device, &mut say).await?,
                LoginTarget::Claude { method } => aim::login::claude(method.as_deref(), &mut say).await?,
            }
            Ok(0)
        }
        Command::Sessions { limit } => {
            let store = SqliteStore::open(&cli::aim_home().join("aim.db")).map_err(|e| e.to_string())?;
            let sessions = store.list(limit).await.map_err(|e| e.to_string())?;
            for s in sessions {
                eprintln!("{}  {}  {}/{}  {}", s.id, s.created_ms, s.provider, s.model, s.workspace);
            }
            Ok(0)
        }
        Command::Daemon { socket, idle_exit, action } => {
            let home = cli::aim_home();
            let socket = socket.unwrap_or_else(|| socket_path(&home));
            match action {
                Some(DaemonAction::Status) => {
                    let client = DaemonClient::connect(&socket).await.map_err(|e| e.to_string())?;
                    let sessions =
                        client.list(SessionListParams { limit: Some(u32::MAX), workspace: None }).await.map_err(|e| e.to_string())?;
                    let live = sessions.iter().filter(|s| s.state != aim_proto::daemon::SessionState::Closed).count();
                    println!(
                        "generation={} pid={} sessions={}",
                        client.initialize_result().generation,
                        client.initialize_result().pid,
                        live
                    );
                    Ok(0)
                }
                Some(DaemonAction::Stop) => {
                    let client = DaemonClient::connect(&socket).await.map_err(|e| e.to_string())?;
                    let pid = client.initialize_result().pid;
                    let recorded = std::fs::read_to_string(home.join("run/daemon.pid")).map_err(|e| e.to_string())?;
                    if recorded.trim() != pid.to_string() {
                        return Err("daemon pid file differs from the connected process".into());
                    }
                    let status =
                        std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status().map_err(|e| e.to_string())?;
                    if !status.success() {
                        return Err(format!("kill exited with {status}"));
                    }
                    Ok(0)
                }
                None => {
                    let logs = home.join("logs");
                    std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
                    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
                    let file =
                        std::fs::OpenOptions::new().create(true).append(true).open(logs.join("daemon.log")).map_err(|e| e.to_string())?;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
                    let (writer, _guard) = tracing_appender::non_blocking(file);
                    let _ignored = tracing_subscriber::fmt().with_writer(writer).with_ansi(false).try_init();
                    let store = Arc::new(SqliteStore::open(&home.join("aim.db")).map_err(|e| e.to_string())?);
                    let host = Arc::new(SessionHost::new(HostConfig {
                        store,
                        backends: aim::providers::backends(cli::find_aimx(None), 64),
                        update_capacity: 1024,
                    }));
                    let on_shutdown = {
                        let host = Arc::clone(&host);
                        async move { host.shutdown().await }
                    };
                    match server::serve_with_shutdown(
                        &home,
                        &socket,
                        idle_exit.map(std::time::Duration::from_secs),
                        host as Arc<dyn SessionClient>,
                        on_shutdown,
                    )
                    .await
                    {
                        Ok(()) | Err(aim_proto::error::ProtoError { code: aim_proto::error::ErrorCode::Conflict, .. }) => Ok(0),
                        Err(err) => Err(err.to_string()),
                    }
                }
            }
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
