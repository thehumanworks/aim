//! `aim` — the agent layer's binary.
//!
//! - `aim` (or `aim tui`) — chat in the terminal (milestone M3).
//! - `aim run [PROMPT]` — one headless turn in the current workspace (milestone M2a).
//! - `aim sessions` — recent sessions.
//! - `aim daemon` — serve local sessions over `aim-daemon/1`.
//! - `aim search`, `aim image`, `aim transcribe` — Codex media services.
//! - `aim board` — inspect and mutate the durable blackboard through the daemon.
#![expect(clippy::print_stderr, reason = "the CLI reports errors on stderr")]
#![expect(clippy::print_stdout, reason = "daemon status reports to stdout")]

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aim::board::cli as board_cli;
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
    /// Post and inspect durable blackboard jobs through the local daemon.
    Board {
        /// Board action.
        #[command(subcommand)]
        action: board_cli::BoardAction,
    },
    /// Search the current user's persistent past conversations.
    SearchSessions {
        /// Rebuild the index from the lossless session log.
        #[arg(long)]
        reindex: bool,
        /// Maximum excerpts to show.
        #[arg(short, long, default_value_t = 8)]
        limit: u32,
        /// Restrict results to this workspace root.
        #[arg(long)]
        workspace: Option<String>,
        /// Emit each result as JSON.
        #[arg(long)]
        json: bool,
        /// Search terms; omitted when only reindexing.
        query: Vec<String>,
    },
    /// Serve or inspect the local session daemon.
    Daemon {
        /// Unix socket path (default: `$AIM_HOME/run/daemon.sock`).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Serve the browser app and daemon protocol over WebSocket at this address.
        #[arg(long, value_name = "ADDR")]
        web: Option<std::net::SocketAddr>,
        /// Built browser asset directory (default: this checkout's `crates/aim-web/dist`).
        #[arg(long, value_name = "DIR")]
        web_assets: Option<PathBuf>,
        /// Direct TLS certificate for the web listener (PEM).
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// Direct TLS private key for the web listener (PEM).
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Declare an operator-managed protected TLS reverse proxy for a non-loopback bind.
        #[arg(long)]
        behind_proxy: bool,
        /// Exact browser Origin allowed to open a WebSocket (repeatable).
        #[arg(long = "allow-origin")]
        allowed_origins: Vec<String>,
        /// Maximum concurrent browser connections.
        #[arg(long, default_value_t = 64)]
        max_web_connections: usize,
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
    /// Manage browser access tokens.
    Token {
        #[command(subcommand)]
        action: DaemonTokenAction,
    },
}

#[derive(Subcommand)]
enum DaemonTokenAction {
    /// Issue one bearer and display it once.
    Create {
        /// Bearer lifetime in seconds.
        #[arg(long, default_value_t = 30 * 24 * 60 * 60)]
        ttl_secs: u64,
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

async fn search_cli(query: &str) -> Result<i32, String> {
    let media = aim_llm_codex::media::MediaClient::new().map_err(|error| error.to_string())?;
    let answer = media.web_search(query).await.map_err(|error| error.to_string())?;
    println!("{}", answer.text);
    for citation in answer.citations {
        println!("- {}: {}", citation.title, citation.url);
    }
    Ok(0)
}

async fn show_search(
    engine: Arc<aim::search::SearchEngine>,
    query: String,
    limit: u32,
    workspace: Option<String>,
    json: bool,
) -> Result<i32, String> {
    let hits =
        tokio::task::spawn_blocking(move || engine.search(&query, limit, workspace.as_deref())).await.map_err(|err| err.to_string())??;
    for hit in hits {
        if json {
            println!("{}", serde_json::to_string(&hit).map_err(|err| err.to_string())?);
        } else {
            println!("{}:{} turn {} {} {}", hit.session, hit.seq, hit.turn, hit.kind, hit.snippet);
        }
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

/// The TUI. Persistent sessions live in the daemon (auto-started), so they outlive the terminal
/// and can be re-attached; `--ephemeral` (or an explicit `--aimx`) runs an in-process host.
async fn tui(args: aim::tui::TuiArgs) -> Result<i32, String> {
    #[cfg(feature = "test-support")]
    if let Some(script) = &args.script {
        return aim::tui::script::run(script, &args).await;
    }
    let mut options = args.options()?;
    if !args.ephemeral && args.aimx.is_none() {
        match aim::daemon::spawn::connect_or_spawn(&cli::aim_home()).await {
            Ok(daemon) => {
                // The session keeps running in the daemon when the TUI exits.
                options.close_on_exit = false;
                return aim::tui::run(Arc::new(daemon), options).await;
            }
            Err(e) => eprintln!("aim: the daemon is unavailable ({}); running in process", e.message),
        }
    }
    let store: Arc<dyn SessionStore> = if args.ephemeral {
        Arc::new(aim::store::MemoryStore::default())
    } else {
        Arc::new(SqliteStore::open(&cli::aim_home().join("aim.db")).map_err(|e| e.to_string())?)
    };
    let backends = aim::providers::backends(cli::find_aimx(args.aimx.as_deref()), args.max_requests);
    let host = SessionHost::new(HostConfig { store, backends, update_capacity: 4096 });
    aim::tui::run(Arc::new(host), options).await
}

async fn search_sessions_command(
    reindex: bool,
    limit: u32,
    workspace: Option<String>,
    json: bool,
    query: Vec<String>,
) -> Result<i32, String> {
    if query.is_empty() && !reindex {
        return Err("provide search terms or --reindex".to_owned());
    }
    let database = cli::aim_home().join("aim.db");
    let _store = SqliteStore::open(&database).map_err(|err| err.to_string())?;
    let engine = tokio::task::spawn_blocking(move || aim::search::SearchEngine::open(&database)).await.map_err(|err| err.to_string())??;
    let engine = Arc::new(engine);
    if reindex {
        let again = Arc::clone(&engine);
        let count = tokio::task::spawn_blocking(move || again.reindex()).await.map_err(|err| err.to_string())??;
        if query.is_empty() {
            println!("indexed {count} chunks");
            return Ok(0);
        }
    }
    show_search(engine, query.join(" "), limit, workspace, json).await
}

async fn list_sessions(limit: u32) -> Result<i32, String> {
    let store = SqliteStore::open(&cli::aim_home().join("aim.db")).map_err(|e| e.to_string())?;
    let sessions = store.list(limit).await.map_err(|e| e.to_string())?;
    for s in sessions {
        eprintln!("{}  {}  {}/{}  {}", s.id, s.created_ms, s.provider, s.model, s.workspace);
    }
    Ok(0)
}

#[expect(clippy::too_many_lines, reason = "the CLI command dispatcher keeps daemon status and stop adjacent")]
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
        Command::Sessions { limit } => list_sessions(limit).await,
        Command::SearchSessions { reindex, limit, workspace, json, query } => {
            search_sessions_command(reindex, limit, workspace, json, query).await
        }
        Command::Board { action } => board_cli::run(&cli::aim_home(), action).await,
        Command::Daemon {
            socket,
            web,
            web_assets,
            tls_cert,
            tls_key,
            behind_proxy,
            allowed_origins,
            max_web_connections,
            idle_exit,
            action,
        } => {
            let home = cli::aim_home();
            let socket = socket.unwrap_or_else(|| socket_path(&home));
            match action {
                Some(DaemonAction::Token { action: DaemonTokenAction::Create { ttl_secs } }) => {
                    let token = server::WebTokenStore::under_home(&home)
                        .create(Duration::from_secs(ttl_secs))
                        .map_err(|e| format!("creating daemon web token: {e}"))?;
                    println!("{token}");
                    Ok(0)
                }
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
                    drop(client);
                    let started = Instant::now();
                    while socket.exists() || home.join("run/daemon.pid").exists() {
                        if started.elapsed() >= Duration::from_secs(15) {
                            return Err("daemon did not stop within fifteen seconds".into());
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Ok(0)
                }
                None => {
                    if web.is_some() && idle_exit.is_some() {
                        return Err("--idle-exit is unavailable with --web while browser clients may be attached".into());
                    }
                    if web.is_none() && (web_assets.is_some() || tls_cert.is_some() || behind_proxy || !allowed_origins.is_empty()) {
                        return Err("web asset and security flags require --web".into());
                    }
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
                    let session_client: Arc<dyn SessionClient> = Arc::<SessionHost>::clone(&host);
                    let outcome = if let Some(address) = web {
                        let options = server::WebOptions {
                            address,
                            tls: tls_cert.zip(tls_key),
                            behind_proxy,
                            allowed_origins,
                            asset_dir: web_assets.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../aim-web/dist")),
                            token_store: Arc::new(server::WebTokenStore::under_home(&home)),
                            max_connections: max_web_connections,
                        };
                        let web = server::serve_web(&home, Arc::clone(&session_client), options);
                        let unix = server::serve_with_shutdown(&home, &socket, None, session_client, on_shutdown);
                        tokio::select! {
                            result = unix => result,
                            result = web => {
                                host.shutdown().await.map_err(|err| err.to_string())?;
                                result
                            }
                        }
                    } else {
                        server::serve_with_shutdown(&home, &socket, idle_exit.map(Duration::from_secs), session_client, on_shutdown).await
                    };
                    match outcome {
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
