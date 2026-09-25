//! `aimx` — aim's execution layer binary.
//!
//! - `aimx serve --unix PATH [--root DIR]… [--read-only]` serves `aim-harness/1` on a unix socket
//!   (many connections; peers must run as the same OS user).
//! - `aimx serve --stdio …` serves one connection on stdin/stdout (what SSH bootstrap runs).
//! - `aimx serve --ws ADDR | --http ADDR` serves authenticated network peers.
//! - `aimx token create --scope read|write` issues an owner token once.
//! - `aimx version` prints the version and the protocol generations.
//!
//! Logs go to stderr (never stdout, which carries the protocol in `--stdio` mode); the level comes
//! from `AIMX_LOG` (default `warn`: aimx is quiet under its client).

use std::path::PathBuf;
use std::process::ExitCode;
use std::process::Stdio;
use std::time::Duration;

use aim_rpc::{NoHandler, Peer, PeerConfig};
use aimx::server::network::{NetworkOptions, NetworkProtocol};
use aimx::server::token::{TokenScope, TokenStore};
use aimx::server::{Server, ServerConfig, default_protected, local_principal};
use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Manage bearer tokens for network harness clients.
    Token {
        #[command(subcommand)]
        command: TokenCmd,
    },
    /// Expose workspace tools as an MCP server.
    Mcp(McpArgs),
    /// Copy stdin/stdout to the resident server, starting it when needed.
    Proxy(ProxyArgs),
    /// Answer one `SSH_ASKPASS` prompt through aim's private callback socket.
    Askpass { prompt: String },
    /// Print the version and the protocol generations spoken.
    Version,
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Create a token, printing the secret once.
    Create {
        /// Authority granted in the served workspace roots.
        #[arg(long, value_enum)]
        scope: TokenScopeArg,
        /// Token lifetime, in seconds.
        #[arg(long, default_value_t = 30 * 24 * 60 * 60)]
        ttl_secs: u64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum TokenScopeArg {
    Read,
    Write,
}

#[derive(Args)]
#[expect(clippy::struct_excessive_bools, reason = "independent command-line switches are represented as booleans by clap")]
struct ServeArgs {
    /// Listen on this unix socket.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["stdio", "resident", "resident_child", "ws", "http"], required_unless_present_any = ["stdio", "resident", "resident_child", "ws", "http"])]
    unix: Option<PathBuf>,
    /// Listen for WebSocket clients on this address.
    #[arg(long, value_name = "ADDR", conflicts_with_all = ["http", "stdio", "resident", "resident_child", "ssh"])]
    ws: Option<std::net::SocketAddr>,
    /// Listen for HTTP JSON-RPC and SSE clients on this address.
    #[arg(long, value_name = "ADDR", conflicts_with_all = ["ws", "stdio", "resident", "resident_child", "ssh"])]
    http: Option<std::net::SocketAddr>,
    /// Direct TLS server certificate (PEM).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// Direct TLS server private key (PEM).
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Declare that a protected reverse proxy supplies TLS for a public bind.
    #[arg(long)]
    behind_proxy: bool,
    /// Exact browser Origin permitted to open a network connection (repeatable).
    #[arg(long = "allow-origin")]
    allowed_origins: Vec<String>,
    /// Maximum simultaneous network connections.
    #[arg(long, default_value_t = 64)]
    max_connections: usize,
    /// Serve one connection on stdin/stdout.
    #[arg(long)]
    stdio: bool,
    /// Start a detached server for one root and return when it answers.
    #[arg(long, conflicts_with_all = ["stdio", "resident_child"])]
    resident: bool,
    /// Run the detached server process (internal).
    #[arg(long, hide = true, conflicts_with = "stdio")]
    resident_child: bool,
    /// Route a stdio harness connection over SSH.
    #[arg(long, requires = "stdio")]
    ssh: Option<String>,
    /// Force agentless operation, or bootstrap the resident binary.
    #[arg(long, default_value = "auto")]
    bootstrap: BootstrapMode,
    /// Caller-verified artifact to install on the SSH host.
    #[arg(long, requires = "sha256")]
    artifact: Option<PathBuf>,
    /// Expected SHA-256 of `--artifact`.
    #[arg(long, requires = "artifact")]
    sha256: Option<String>,
    /// SSH configuration file (also useful for isolated tests).
    #[arg(long)]
    ssh_config: Option<PathBuf>,
    /// Resident idle shutdown delay.
    #[arg(long, default_value_t = 600)]
    idle_secs: u64,
    /// A directory clients may open workspaces under (repeatable; default: the home directory).
    #[arg(long = "root", value_name = "DIR")]
    roots: Vec<PathBuf>,
    /// Grant read access only: no writes, no processes, no mutating tools.
    #[arg(long)]
    read_only: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BootstrapMode {
    Auto,
    Never,
}

#[derive(Args)]
struct ProxyArgs {
    /// Workspace root served by the resident.
    #[arg(long)]
    root: PathBuf,
    /// Resident idle shutdown delay.
    #[arg(long, default_value_t = 600)]
    idle_secs: u64,
}

#[derive(Args)]
struct McpArgs {
    /// Serve MCP over stdin/stdout.
    #[arg(long, required = true)]
    stdio: bool,
    /// Root directory on the workspace host (default: home for local workspaces).
    #[arg(long)]
    root: Option<PathBuf>,
    /// SSH destination; runs every tool on that host.
    #[arg(long)]
    ssh: Option<String>,
    /// SSH configuration file.
    #[arg(long)]
    ssh_config: Option<PathBuf>,
    /// Bootstrap resident aimx when possible, or use agentless SSH.
    #[arg(long, default_value = "auto")]
    bootstrap: BootstrapMode,
    /// Caller-verified artifact for remote installation.
    #[arg(long, requires = "sha256")]
    artifact: Option<PathBuf>,
    /// Expected SHA-256 of `--artifact`.
    #[arg(long, requires = "artifact")]
    sha256: Option<String>,
    /// Grant read access only to a local workspace.
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
    if args.resident || args.resident_child {
        let [root] = args.roots.as_slice() else {
            return Err("resident mode requires exactly one --root".to_owned());
        };
        let idle = Duration::from_secs(args.idle_secs);
        if args.resident_child {
            return aimx::ssh::resident::serve_child(root, idle).await.map_err(|err| err.to_string());
        }
        return aimx::ssh::resident::ensure(root, idle).await.map(|_| ()).map_err(|err| err.to_string());
    }
    if let Some(destination) = &args.ssh {
        let [root] = args.roots.as_slice() else {
            return Err("SSH mode requires exactly one --root".to_owned());
        };
        return aimx::ssh::forward::serve_ssh(aimx::ssh::forward::ForwardOptions {
            destination: destination.clone(),
            root: root.clone(),
            bootstrap: matches!(args.bootstrap, BootstrapMode::Auto),
            artifact: args.artifact.clone(),
            sha256: args.sha256.clone(),
            ssh_config: args.ssh_config.clone(),
            idle: Duration::from_secs(args.idle_secs),
        })
        .await;
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_owned())?;
    let roots = if args.roots.is_empty() { vec![PathBuf::from(&home)] } else { args.roots };
    let principal = local_principal(&roots, args.read_only).map_err(|err| format!("invalid --root: {err}"))?;
    let mut protected = default_protected(&home);
    protected = protected.with([format!("{home}/.aim/tokens.json"), format!("{home}/.aim/tokens.lock")]);
    let config = ServerConfig::new(principal, protected);
    tracing::info!(principal = %config.principal.id, roots = ?config.principal.roots, read_only = config.principal.read_only, "starting aimx");
    let server = Server::new(config);
    if let Some((address, protocol)) =
        args.ws.map(|addr| (addr, NetworkProtocol::WebSocket)).or_else(|| args.http.map(|addr| (addr, NetworkProtocol::Http)))
    {
        let tls = args.tls_cert.zip(args.tls_key);
        let options = NetworkOptions {
            address,
            protocol,
            tls,
            behind_proxy: args.behind_proxy,
            allowed_origins: args.allowed_origins,
            max_connections: args.max_connections,
        };
        let tokens = std::sync::Arc::new(TokenStore::under_home(std::path::Path::new(&home)));
        return tokio::select! {
            outcome = server.serve_network(options, tokens) => outcome.map_err(|err| format!("network listener: {err}")),
            _ = tokio::signal::ctrl_c() => {
                server.shutdown().await;
                Ok(())
            }
        };
    }
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

async fn mcp(args: McpArgs) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_owned())?;
    if args.ssh.is_some() && args.root.is_none() {
        return Err("SSH MCP mode requires --root on the remote host".to_owned());
    }
    let root = args.root.unwrap_or_else(|| PathBuf::from(&home));
    let root_text = root.to_str().ok_or("workspace root is not UTF-8")?;
    let (peer, server, child) = if let Some(destination) = args.ssh {
        if args.read_only {
            return Err("--read-only is currently local-only".to_owned());
        }
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        let mut command = tokio::process::Command::new(exe);
        command.args(["serve", "--stdio", "--ssh", &destination, "--root", root_text]);
        command.arg("--bootstrap").arg(if matches!(args.bootstrap, BootstrapMode::Auto) { "auto" } else { "never" });
        if let Some(config) = args.ssh_config {
            command.arg("--ssh-config").arg(config);
        }
        if let Some(artifact) = args.artifact {
            command.arg("--artifact").arg(artifact);
        }
        if let Some(sha256) = args.sha256 {
            command.arg("--sha256").arg(sha256);
        }
        command.env_clear();
        for key in
            ["PATH", "HOME", "USER", "LOGNAME", "SSH_AUTH_SOCK", "SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "DISPLAY", "TERM", "XDG_RUNTIME_DIR"]
        {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| format!("starting SSH harness: {err}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("SSH harness has no stdio pipes".to_owned());
        };
        (Peer::spawn(stdout, stdin, NoHandler, PeerConfig::default()), None, Some(child))
    } else {
        let principal = local_principal(&[&root], args.read_only).map_err(|err| format!("invalid --root: {err}"))?;
        let server = Server::new(ServerConfig::new(principal, default_protected(&home)));
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let _server_peer = server.connect(server_read, server_write);
        let (client_read, client_write) = tokio::io::split(client_io);
        (Peer::spawn(client_read, client_write, NoHandler, PeerConfig::default()), Some(server), None)
    };
    let adapter = aimx::mcp::Mcp::connect(peer.clone(), root_text).await.map_err(|err| format!("MCP harness: {err}"))?;
    let result = adapter.serve(tokio::io::stdin(), tokio::io::stdout()).await.map_err(|err| err.to_string());
    peer.close();
    if let Some(server) = server {
        server.shutdown().await;
    }
    drop(child);
    result
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Version => {
            version();
            ExitCode::SUCCESS
        }
        Cmd::Token { command: TokenCmd::Create { scope, ttl_secs } } => {
            let scope = match scope {
                TokenScopeArg::Read => TokenScope::Read,
                TokenScopeArg::Write => TokenScope::Write,
            };
            let result = std::env::var("HOME").map_err(|_| "HOME is not set".to_owned()).and_then(|home| {
                TokenStore::under_home(std::path::Path::new(&home))
                    .create(scope, Duration::from_secs(ttl_secs))
                    .map_err(|err| err.to_string())
            });
            match result {
                Ok(token) => {
                    use std::io::Write as _;
                    let mut output = std::io::stdout().lock();
                    if output.write_all(token.as_bytes()).and_then(|()| output.write_all(b"\n")).is_ok() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(err) => {
                    use std::io::Write as _;
                    drop(writeln!(std::io::stderr(), "{err}"));
                    ExitCode::FAILURE
                }
            }
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
        Cmd::Mcp(args) => {
            init_logging();
            match mcp(args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    tracing::error!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Proxy(args) => {
            init_logging();
            match aimx::ssh::resident::proxy(&args.root, Duration::from_secs(args.idle_secs)).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    tracing::error!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Askpass { prompt } => {
            use std::io::Write as _;
            match aimx::ssh::conn::askpass_client(&prompt).await {
                Ok(Some(answer)) => {
                    let mut out = std::io::stdout().lock();
                    if out.write_all(answer.as_bytes()).and_then(|()| out.write_all(b"\n")).is_ok() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Ok(None) | Err(_) => ExitCode::FAILURE,
            }
        }
    }
}
