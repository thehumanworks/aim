//! `aim` — the agent layer's binary.
//!
//! - `aim` (or `aim tui`) — chat in the terminal (milestone M3).
//! - `aim run [PROMPT]` — one headless turn in the current workspace (milestone M2a).
//! - `aim sessions` — recent sessions.
//! - `aim daemon` — serve local sessions over `aim-daemon/1`.
//! - `aim search`, `aim image`, `aim transcribe` — Codex media services.
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
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    tui: aim::tui::TuiArgs,
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
    /// Search the public web through Codex and print the answer and citations.
    Search {
        /// Search query.
        query: String,
    },
    /// Generate an image through Codex and save it locally.
    Image {
        /// Image prompt.
        prompt: String,
        /// Destination image file.
        #[arg(short, long)]
        output: PathBuf,
        /// Requested dimensions (service-supported size string).
        #[arg(long)]
        size: Option<String>,
        /// Requested quality (service-supported quality string).
        #[arg(long)]
        quality: Option<String>,
    },
    /// Transcribe a WAV file through Codex (server retains the audio for 30 days).
    Transcribe {
        /// WAV file to send.
        wav: PathBuf,
    },
    /// Sign in to a provider.
    Login {
        #[command(subcommand)]
        target: LoginTarget,
    },
    /// Chat in the terminal (the default without a subcommand).
    Tui(aim::tui::TuiArgs),
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

async fn search_cli(query: &str) -> Result<i32, String> {
    let media = aim_llm_codex::media::MediaClient::new().map_err(|error| error.to_string())?;
    let answer = media.web_search(query).await.map_err(|error| error.to_string())?;
    println!("{}", answer.text);
    for citation in answer.citations {
        println!("- {}: {}", citation.title, citation.url);
    }
    Ok(0)
}

async fn image_cli(prompt: &str, output: &PathBuf, size: Option<&str>, quality: Option<&str>) -> Result<i32, String> {
    let media = aim_llm_codex::media::MediaClient::new().map_err(|error| error.to_string())?;
    let image = media.generate_image(prompt, size, quality).await.map_err(|error| error.to_string())?;
    std::fs::write(output, image.bytes).map_err(|error| format!("writing {}: {error}", output.display()))?;
    println!("{}", output.display());
    Ok(0)
}

async fn transcribe_cli(wav: &PathBuf) -> Result<i32, String> {
    let bytes = std::fs::read(wav).map_err(|error| format!("reading {}: {error}", wav.display()))?;
    let media = aim_llm_codex::media::MediaClient::new().map_err(|error| error.to_string())?;
    println!("{}", media.transcribe(&bytes).await.map_err(|error| error.to_string())?);
    Ok(0)
}

/// The TUI on an in-process session host (the daemon client replaces it for persistent runs).
async fn tui(args: aim::tui::TuiArgs) -> Result<i32, String> {
    #[cfg(feature = "test-support")]
    if let Some(script) = &args.script {
        return aim::tui::script::run(script, &args).await;
    }
    let options = args.options()?;
    let store: Arc<dyn SessionStore> = if args.ephemeral {
        Arc::new(aim::store::MemoryStore::default())
    } else {
        Arc::new(SqliteStore::open(&cli::aim_home().join("aim.db")).map_err(|e| e.to_string())?)
    };
    let backends = aim::providers::backends(cli::find_aimx(args.aimx.as_deref()), args.max_requests);
    let host = SessionHost::new(HostConfig { store, backends, update_capacity: 4096 });
    aim::tui::run(Arc::new(host), options).await
}

async fn main_async(args: Args) -> Result<i32, String> {
    let Some(command) = args.command else { return tui(args.tui).await };
    match command {
        Command::Tui(tui_args) => tui(tui_args).await,
        Command::Run { provider: p, model, effort, cwd, ssh, aimx, ephemeral, json, max_requests, prompt } => {
            let prompt = read_prompt(&prompt)?;
            if prompt.trim().is_empty() {
                return Err("empty prompt".to_owned());
            }
            let options = RunOptions { provider: p, model, effort, cwd, ssh, aimx, ephemeral, json, max_requests, prompt };
            cli::run(options, provider).await
        }
        Command::Search { query } => search_cli(&query).await,
        Command::Image { prompt, output, size, quality } => image_cli(&prompt, &output, size.as_deref(), quality.as_deref()).await,
        Command::Transcribe { wav } => transcribe_cli(&wav).await,
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
