//! `aim` — the agent layer's binary.
//!
//! - `aim` (or `aim tui`) — chat in the terminal (milestone M3).
//! - `aim run [PROMPT]` — one headless turn in the current workspace (milestone M2a).
//! - `aim sessions` — recent sessions.
//! - `aim daemon` — serve local sessions over `aim-daemon/1`.
//! - `aim search`, `aim image`, `aim transcribe` — Codex media services.
//! - `aim board` — inspect and mutate the durable blackboard through the daemon.
//! - `aim mcp` — trusted user MCP servers and aim's own MCP service endpoint.
#![expect(clippy::print_stderr, reason = "the CLI reports errors on stderr")]
#![expect(clippy::print_stdout, reason = "CLI results and one-time tokens report to stdout")]

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aim::board::cli as board_cli;
use aim::cli::{self, RunOptions};
use aim::daemon::{client::DaemonClient, server, socket_path};
use aim::host::{HostConfig, SessionClient, SessionHost};
use aim::mcp::{config as mcp_config, server as mcp_server, services::AimServices, trust as mcp_trust};
use aim::resources::HarnessFiles;
use aim::store::{SessionStore, SqliteStore};
use aim_llm::ModelProvider;
use aim_plugin::{PluginManifest, PluginSource, TrustStore};
use aim_proto::daemon::{Location, Persistence, SessionListParams, SessionSpec};
use clap::{Args as ClapArgs, Parser, Subcommand};

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
        #[arg(long, conflicts_with = "remote")]
        ssh: Option<String>,
        /// Network aimx endpoint (bearer from `AIM_REMOTE_TOKEN` or `AIM_REMOTE_TOKEN_FILE`).
        #[arg(long, conflicts_with = "ssh")]
        remote: Option<String>,
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
    /// Inspect and trust user MCP servers, or serve aim's tools to an MCP client.
    Mcp {
        /// Serve aim's local services over MCP stdio.
        #[arg(long)]
        stdio: bool,
        /// Workspace root for project MCP config discovery.
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
        /// SSH destination for the workspace config.
        #[arg(long)]
        ssh: Option<String>,
        /// aimx binary used to read the project config through the harness.
        #[arg(long)]
        aimx: Option<PathBuf>,
        /// Inspect or change one trust grant.
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// Install and trust capability-scoped WebAssembly component plugins.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
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

#[derive(Subcommand)]
enum McpAction {
    /// List discovered server definitions and their trust state without starting them.
    List,
    /// Trust the current hash of one discovered server definition.
    Trust {
        /// Server name.
        name: String,
        /// Select a particular config file when names collide.
        #[arg(long)]
        source: Option<String>,
    },
    /// Remove a trust grant for one discovered server definition.
    Untrust {
        /// Server name.
        name: String,
        /// Select a particular config file when names collide.
        #[arg(long)]
        source: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// List installed or project plugins and their exact-hash trust state.
    List {
        #[command(flatten)]
        project: PluginProject,
    },
    /// Copy a component and its adjacent manifest to the user's plugin directory.
    Install {
        /// Component, manifest, or directory containing `aim-plugin.toml`.
        path: PathBuf,
    },
    /// Grant the installed plugin's requested capabilities to its current component hash.
    Trust {
        /// Installed or project plugin name.
        name: String,
        /// Grant only these requested capabilities (repeatable; defaults to all requested).
        #[arg(long = "cap")]
        capabilities: Vec<String>,
        #[command(flatten)]
        project: PluginProject,
    },
    /// Revoke grants for a plugin's current hash (or an explicit SHA-256 hash).
    Untrust {
        /// Installed/project plugin name or 64-character SHA-256 hex digest.
        name_or_hash: String,
        #[command(flatten)]
        project: PluginProject,
    },
}

#[derive(ClapArgs, Default)]
struct PluginProject {
    /// Project workspace root; read plugins through aimx.
    #[arg(long)]
    project: Option<PathBuf>,
    /// SSH destination for a project workspace.
    #[arg(long, requires = "project", conflicts_with = "remote")]
    ssh: Option<String>,
    /// Authenticated aimx endpoint for a project workspace.
    #[arg(long, requires = "project", conflicts_with = "ssh")]
    remote: Option<String>,
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

const MAX_PLUGIN_COMPONENT_BYTES: u64 = 16 * 1024 * 1024;

fn plugin_name_is_safe(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn plugin_component_path(manifest_dir: &Path, component: &str, installed: bool) -> Result<PathBuf, String> {
    if installed && component != "plugin.wasm" {
        return Err("installed plugin component must be plugin.wasm".into());
    }
    let path = manifest_dir.join(component);
    let metadata = std::fs::metadata(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_PLUGIN_COMPONENT_BYTES {
        return Err(format!("{} must be a regular 1..=16 MiB component", path.display()));
    }
    Ok(path)
}

fn plugin_source_from_manifest(manifest_path: &Path, installed: bool) -> Result<(PluginManifest, PluginSource), String> {
    if std::fs::metadata(manifest_path).map_err(|err| err.to_string())?.len() > 64 * 1024 {
        return Err("plugin manifest exceeds 64 KiB".into());
    }
    let text = std::fs::read_to_string(manifest_path).map_err(|err| format!("{}: {err}", manifest_path.display()))?;
    let manifest = PluginManifest::parse(&text).map_err(|err| err.to_string())?;
    let directory = manifest_path.parent().ok_or("plugin manifest has no directory")?;
    let component_path = plugin_component_path(directory, &manifest.component, installed)?;
    let component = std::fs::read(&component_path).map_err(|err| format!("{}: {err}", component_path.display()))?;
    Ok((manifest, PluginSource { manifest_text: text, component, project: false }))
}

fn installed_plugin(home: &Path, name: &str) -> Result<(PluginManifest, PluginSource), String> {
    if !plugin_name_is_safe(name) {
        return Err("plugin name must contain only letters, digits, hyphen or underscore".into());
    }
    let path = home.join("plugins").join(name).join("aim-plugin.toml");
    let (manifest, source) = plugin_source_from_manifest(&path, true)?;
    if manifest.name != name {
        return Err("installed plugin name differs from its directory".into());
    }
    Ok((manifest, source))
}

fn plugin_install(home: &Path, source_path: &Path) -> Result<i32, String> {
    let manifest_path = if source_path.is_dir() {
        source_path.join("aim-plugin.toml")
    } else if source_path.extension().is_some_and(|ext| ext == "wasm") {
        source_path.parent().ok_or("component has no parent directory")?.join("aim-plugin.toml")
    } else {
        source_path.to_path_buf()
    };
    let (manifest, source) = plugin_source_from_manifest(&manifest_path, false)?;
    if source_path.extension().is_some_and(|ext| ext == "wasm") {
        let component = plugin_component_path(manifest_path.parent().ok_or("manifest has no directory")?, &manifest.component, false)?;
        if source_path.canonicalize().map_err(|err| err.to_string())? != component.canonicalize().map_err(|err| err.to_string())? {
            return Err("component path differs from adjacent manifest".into());
        }
    }
    let directory = home.join("plugins");
    std::fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).map_err(|err| err.to_string())?;
    let installed = directory.join(&manifest.name);
    std::fs::create_dir(&installed).map_err(|err| format!("{}: {err} (existing plugins are never overwritten)", installed.display()))?;
    std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o700)).map_err(|err| err.to_string())?;
    let mut document: toml::Value = toml::from_str(&source.manifest_text).map_err(|err| err.to_string())?;
    document.as_table_mut().ok_or("plugin manifest must be a table")?.insert("component".into(), toml::Value::String("plugin.wasm".into()));
    for (name, bytes) in [
        ("plugin.wasm", source.component),
        ("aim-plugin.toml", toml::to_string_pretty(&document).map_err(|err| err.to_string())?.into_bytes()),
    ] {
        let path = installed.join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        std::io::Write::write_all(&mut file, &bytes).map_err(|err| format!("{}: {err}", path.display()))?;
    }
    println!("installed {} (run `aim plugin trust {}` to grant capabilities)", manifest.name, manifest.name);
    Ok(0)
}

async fn project_plugin_sources(project: &PluginProject) -> Result<Option<Vec<PluginSource>>, String> {
    let Some(root) = &project.project else { return Ok(None) };
    let location = if let Some(destination) = &project.ssh {
        Location::Ssh { destination: destination.clone() }
    } else if let Some(url) = &project.remote {
        Location::Remote { url: url.clone() }
    } else {
        Location::Local
    };
    let spec = SessionSpec {
        workspace: root.to_string_lossy().into_owned(),
        location,
        provider: String::new(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Ephemeral,
    };
    let connect = aim::host::aimx_workspaces(cli::find_aimx(None));
    let connected = connect(&spec).await.map_err(|error| error.to_string())?;
    let aim::host::Connected { tools, project, shutdown, .. } = connected;
    let sources = match project.as_ref() {
        Some(files) => aim::providers::project_plugin_sources(files.as_ref()).await,
        None => Vec::new(),
    };
    drop(project);
    drop(tools);
    shutdown().await;
    Ok(Some(sources))
}

async fn plugin_command(home: &Path, action: PluginAction) -> Result<i32, String> {
    match action {
        PluginAction::Install { path } => plugin_install(home, &path),
        PluginAction::List { project } => {
            if let Some(sources) = project_plugin_sources(&project).await? {
                let trust = TrustStore::load(home).map_err(|err| err.to_string())?;
                for source in sources {
                    let Ok(manifest) = PluginManifest::parse(&source.manifest_text) else { continue };
                    let hash = source.hash();
                    let state = if trust.grants(&hash).is_some() { "trusted" } else { "untrusted" };
                    println!("{} {} {} {}", manifest.name, manifest.version, hash.get(..12).unwrap_or(&hash), state);
                }
                return Ok(0);
            }
            let directory = home.join("plugins");
            let entries = match std::fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(err) => return Err(err.to_string()),
            };
            let trust = TrustStore::load(home).map_err(|err| err.to_string())?;
            let mut names = entries
                .take(64)
                .filter_map(|entry| entry.ok().and_then(|entry| entry.file_name().to_str().map(str::to_owned)))
                .filter(|name| plugin_name_is_safe(name))
                .collect::<Vec<_>>();
            names.sort();
            for name in names {
                let Ok((manifest, source)) = installed_plugin(home, &name) else { continue };
                let hash = source.hash();
                let state = if trust.grants(&hash).is_some() { "trusted" } else { "untrusted" };
                println!("{} {} {} {}", manifest.name, manifest.version, hash.get(..12).unwrap_or(&hash), state);
            }
            Ok(0)
        }
        PluginAction::Trust { name, capabilities, project } => {
            let (manifest, source) = if let Some(sources) = project_plugin_sources(&project).await? {
                let source = sources
                    .into_iter()
                    .find(|source| PluginManifest::parse(&source.manifest_text).is_ok_and(|manifest| manifest.name == name))
                    .ok_or_else(|| format!("project plugin `{name}` was not found"))?;
                let manifest = PluginManifest::parse(&source.manifest_text).map_err(|error| error.to_string())?;
                (manifest, source)
            } else {
                installed_plugin(home, &name)?
            };
            let grants: std::collections::BTreeSet<String> =
                if capabilities.is_empty() { manifest.capabilities.clone() } else { capabilities.into_iter().collect() };
            if !grants.is_subset(&manifest.capabilities) {
                return Err("capabilities must be requested in the plugin manifest".into());
            }
            let mut trust = TrustStore::load(home).map_err(|err| err.to_string())?;
            trust.grant(&source.hash(), grants).map_err(|err| err.to_string())?;
            println!("trusted {name} at its current manifest and component hash");
            Ok(0)
        }
        PluginAction::Untrust { name_or_hash, project } => {
            let hash = if name_or_hash.len() == 64 && name_or_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                name_or_hash
            } else if let Some(sources) = project_plugin_sources(&project).await? {
                sources
                    .into_iter()
                    .find(|source| PluginManifest::parse(&source.manifest_text).is_ok_and(|manifest| manifest.name == name_or_hash))
                    .ok_or_else(|| format!("project plugin `{name_or_hash}` was not found"))?
                    .hash()
            } else {
                installed_plugin(home, &name_or_hash)?.1.hash()
            };
            let mut trust = TrustStore::load(home).map_err(|err| err.to_string())?;
            trust.untrust(&hash).map_err(|err| err.to_string())?;
            println!("revoked plugin grant");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod plugin_tests {
    use super::{PluginAction, PluginProject, TrustStore, installed_plugin, plugin_command};

    #[tokio::test]
    async fn cli_installs_grants_only_requested_capabilities_and_revokes() {
        let home = tempfile::tempdir().unwrap();
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/kv_counter");
        assert_eq!(plugin_command(home.path(), PluginAction::Install { path: source.clone() }).await.unwrap(), 0);
        assert!(plugin_command(home.path(), PluginAction::Install { path: source }).await.is_err(), "never overwrite an installed plugin");
        let (manifest, component) = installed_plugin(home.path(), "kv_counter").unwrap();
        let hash = component.hash();
        assert_eq!(manifest.component, "plugin.wasm");
        assert!(TrustStore::load(home.path()).unwrap().grants(&hash).is_none());
        assert!(
            plugin_command(
                home.path(),
                PluginAction::Trust {
                    name: "kv_counter".into(),
                    capabilities: vec!["net.http:*".into()],
                    project: PluginProject::default()
                }
            )
            .await
            .is_err(),
            "cannot grant capabilities the manifest never requested"
        );
        assert_eq!(
            plugin_command(
                home.path(),
                PluginAction::Trust { name: "kv_counter".into(), capabilities: vec!["kv".into()], project: PluginProject::default() }
            )
            .await
            .unwrap(),
            0
        );
        let trusted = TrustStore::load(home.path()).unwrap();
        assert_eq!(trusted.grants(&hash).unwrap().iter().map(String::as_str).collect::<Vec<_>>(), ["kv"]);
        assert_eq!(
            plugin_command(home.path(), PluginAction::Untrust { name_or_hash: "kv_counter".into(), project: PluginProject::default() })
                .await
                .unwrap(),
            0
        );
        assert!(TrustStore::load(home.path()).unwrap().grants(&hash).is_none());
    }
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

async fn discovered_mcp(cwd: PathBuf, ssh: Option<String>, aimx: Option<PathBuf>) -> Result<Vec<mcp_config::ServerEntry>, String> {
    let aim_home = cli::aim_home();
    let user_home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is unavailable")?;
    let root = if ssh.is_some() { cwd } else { cwd.canonicalize().map_err(|_| "cannot resolve workspace root")? };
    let root = root.to_string_lossy().into_owned();
    let aimx = cli::find_aimx(aimx.as_deref());
    if let Some(destination) = ssh {
        let harness =
            aim::remote::RemoteHarness::connect(&aimx, &destination, &root).await.map_err(|_| "cannot connect to SSH workspace")?;
        let files = HarnessFiles::new(harness.client.peer().clone(), harness.client.workspace().id.clone());
        let found = mcp_config::discover(Some(&files), &user_home, &aim_home, &harness.client.workspace().root).await;
        harness.shutdown().await;
        found
    } else {
        let harness = aim::harness::HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root)
            .await
            .map_err(|_| "cannot connect to workspace harness")?;
        let files = HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone());
        let found = mcp_config::discover(Some(&files), &user_home, &aim_home, &root).await;
        harness.shutdown().await;
        found
    }
}

fn chosen_mcp<'a>(entries: &'a [mcp_config::ServerEntry], name: &str, source: Option<&str>) -> Result<&'a mcp_config::ServerEntry, String> {
    let mut matches = entries.iter().filter(|entry| entry.name == name && source.is_none_or(|path| entry.source_path == path));
    let first = matches.next().ok_or("MCP server name was not discovered")?;
    if matches.next().is_some() {
        return Err("MCP server name is ambiguous; pass --source".to_owned());
    }
    Ok(first)
}

async fn mcp_command(
    stdio: bool,
    cwd: PathBuf,
    ssh: Option<String>,
    aimx: Option<PathBuf>,
    action: Option<McpAction>,
) -> Result<i32, String> {
    if stdio {
        if action.is_some() || ssh.is_some() {
            return Err("--stdio cannot be combined with a subcommand or --ssh".to_owned());
        }
        let aim_home = cli::aim_home();
        let base: Arc<dyn aim::agent::ToolHost> = Arc::new(AimServices::open(&aim_home).await?);
        let host = aim::mcp::services::with_programs(base, &aim_home);
        mcp_server::serve_stdio(host).await.map_err(|_| "MCP stdio transport failed".to_owned())?;
        return Ok(0);
    }
    let action = action.ok_or("choose `mcp list|trust|untrust` or `mcp --stdio`")?;
    let entries = discovered_mcp(cwd, ssh, aimx).await?;
    match action {
        McpAction::List => {
            let shadowing = aim::mcp::config::shadowing(&entries);
            for (entry, shadow) in entries.iter().zip(shadowing) {
                let state = match shadow.and_then(|index| entries.get(index)) {
                    Some(winner) if entry.trusted => format!("shadowed by {}", winner.source_path),
                    _ if entry.trusted => "trusted".to_owned(),
                    _ => "untrusted".to_owned(),
                };
                println!("{}  {:?}  {:?}  {}  {}", entry.name, entry.origin, entry.location, state, entry.source_path);
            }
        }
        McpAction::Trust { name, source } => {
            let entry = chosen_mcp(&entries, &name, source.as_deref())?;
            mcp_trust::trust(&cli::aim_home(), entry)?;
            println!("trusted {} from {}", entry.name, entry.source_path);
        }
        McpAction::Untrust { name, source } => {
            let entry = chosen_mcp(&entries, &name, source.as_deref())?;
            mcp_trust::untrust(&cli::aim_home(), entry)?;
            println!("untrusted {} from {}", entry.name, entry.source_path);
        }
    }
    Ok(0)
}

#[expect(clippy::too_many_lines, reason = "the CLI command dispatcher keeps daemon status and stop adjacent")]
async fn main_async(args: Args) -> Result<i32, String> {
    let Some(command) = args.command else { return tui(args.tui).await };
    match command {
        Command::Tui(tui_args) => tui(tui_args).await,
        Command::Run { provider: p, model, effort, cwd, ssh, remote, aimx, ephemeral, json, max_requests, prompt } => {
            let prompt = read_prompt(&prompt)?;
            if prompt.trim().is_empty() {
                return Err("empty prompt".to_owned());
            }
            let options = RunOptions { provider: p, model, effort, cwd, ssh, remote, aimx, ephemeral, json, max_requests, prompt };
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
        Command::Board { action } => Box::pin(board_cli::run(&cli::aim_home(), action)).await,
        Command::Mcp { stdio, cwd, ssh, aimx, action } => mcp_command(stdio, cwd, ssh, aimx, action).await,
        Command::Plugin { action } => plugin_command(&cli::aim_home(), action).await,
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

#[cfg(test)]
mod remote_tests {
    use clap::Parser as _;

    use super::Args;

    #[test]
    fn run_remote_and_ssh_are_exclusive() {
        assert!(Args::try_parse_from(["aim", "run", "--remote", "wss://example.test", "--ssh", "box", "hi"]).is_err());
        assert!(Args::try_parse_from(["aim", "run", "--remote", "wss://example.test", "hi"]).is_ok());
        assert!(Args::try_parse_from(["aim", "tui", "--remote", "wss://example.test"]).is_ok());
    }
}
