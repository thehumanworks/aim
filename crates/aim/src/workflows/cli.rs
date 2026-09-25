//! Human workflow commands. Project definitions are always read through aimx.
#![expect(clippy::print_stdout, reason = "workflow CLI renders user-visible command results")]

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{Map, Value};

use crate::harness::HarnessClient;
use crate::resources::{Files as _, HarnessFiles};
use crate::resources::files::Read;

use super::manifest::WorkflowManifest;
use super::store::{NewRun, NewStep, WorkflowStore};
use super::trust;

/// Workflow operations exposed by `aim workflow`.
#[derive(Subcommand)]
pub enum WorkflowAction {
    /// List project workflow definitions and their trust state.
    List {
        /// Project directory.
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Queue a trusted workflow for daemon execution.
    Run {
        /// Workflow directory name under `.agents/workflows`.
        name: String,
        /// Typed JSON value or literal string, as `key=value`.
        #[arg(long = "param")]
        params: Vec<String>,
        /// Project directory.
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Show one durable run and its steps.
    Status {
        /// Run ID.
        run: String,
    },
    /// Persist cancellation intent for one run.
    Cancel {
        /// Run ID.
        run: String,
    },
    /// Trust the exact hash of a project definition.
    Trust {
        /// Workflow directory name.
        name: String,
        /// Project directory.
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Revoke the exact current hash of a project definition.
    Untrust {
        /// Workflow directory name.
        name: String,
        /// Project directory.
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
    },
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn source_path(root: &Path, name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err("invalid workflow name".into());
    }
    Ok(root.join(".agents/workflows").join(name).join("workflow.toml").to_string_lossy().into_owned())
}

async fn manifest_source(root: &Path, name: &str) -> Result<(String, String, String, WorkflowManifest), String> {
    let source = source_path(root, name)?;
    let aimx = crate::cli::find_aimx(None);
    let client = HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root.to_string_lossy())
        .await
        .map_err(|error| error.to_string())?;
    let files = HarnessFiles::new(client.peer().clone(), client.workspace().id.clone());
    let path = format!(".agents/workflows/{name}/workflow.toml");
    let read = files.read_many(vec![path], 256 * 1024).await;
    client.shutdown().await;
    let Some(Read::Ok(file)) = read.into_iter().next() else {
        return Err(format!("workflow `{name}` is absent or unreadable"));
    };
    if file.truncated {
        return Err("workflow.toml exceeds 256 KiB".into());
    }
    let manifest = WorkflowManifest::parse(&file.text)?;
    if manifest.name != name {
        return Err("workflow directory and manifest name differ".into());
    }
    let hash = file.hash.strip_prefix("sha256:").ok_or("workflow hash is unavailable")?.to_owned();
    Ok((source, hash, file.text, manifest))
}

fn params(values: Vec<String>) -> Result<Value, String> {
    let mut object = Map::new();
    for value in values {
        let (key, raw) = value.split_once('=').ok_or("--param requires key=value")?;
        if !valid_name(key) || object.contains_key(key) {
            return Err("invalid or duplicate parameter name".into());
        }
        let parsed = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
        object.insert(key.into(), parsed);
    }
    Ok(Value::Object(object))
}

async fn list(root: &Path, home: &Path) -> Result<(), String> {
    let aimx = crate::cli::find_aimx(None);
    let client = HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root.to_string_lossy())
        .await
        .map_err(|error| error.to_string())?;
    let files = HarnessFiles::new(client.peer().clone(), client.workspace().id.clone());
    let entries = files.list(".agents/workflows", 256).await?;
    client.shutdown().await;
    for entry in entries.unwrap_or_default() {
        if valid_name(&entry.name) {
            match manifest_source(root, &entry.name).await {
                Ok((source, hash, _, _)) => {
                    let state = if trust::is_trusted(home, &source, &hash)? { "trusted" } else { "untrusted" };
                    println!("{}\t{state}", entry.name);
                }
                Err(_) => println!("{}\tinvalid", entry.name),
            }
        }
    }
    Ok(())
}

fn now_ms() -> Result<i64, String> {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?;
    i64::try_from(elapsed.as_millis()).map_err(|error| error.to_string())
}

/// Executes one human workflow command.
///
/// # Errors
/// Returns a safe validation, trust, storage or daemon error.
pub async fn run(home: &Path, action: WorkflowAction) -> Result<i32, String> {
    match action {
        WorkflowAction::List { cwd } => {
            let root = cwd.canonicalize().map_err(|error| error.to_string())?;
            list(&root, home).await?;
        }
        WorkflowAction::Trust { name, cwd } => {
            let root = cwd.canonicalize().map_err(|error| error.to_string())?;
            let (source, hash, _, _) = manifest_source(&root, &name).await?;
            trust::trust(home, &source, &hash)?;
            println!("trusted {name}");
        }
        WorkflowAction::Untrust { name, cwd } => {
            let root = cwd.canonicalize().map_err(|error| error.to_string())?;
            let (source, hash, _, _) = manifest_source(&root, &name).await?;
            trust::untrust(home, &source, &hash)?;
            println!("untrusted {name}");
        }
        WorkflowAction::Run { name, params: input, cwd } => {
            let root = cwd.canonicalize().map_err(|error| error.to_string())?;
            let (source, hash, text, manifest) = manifest_source(&root, &name).await?;
            if !trust::is_trusted(home, &source, &hash)? {
                return Err(format!("workflow `{name}` needs `aim workflow trust {name}` for its current hash"));
            }
            let params = params(input)?;
            manifest.validate_params(&params)?;
            let store = WorkflowStore::open(&home.join("aim.db")).map_err(|error| error.to_string())?;
            let steps = manifest.steps.iter().map(|step| NewStep { name: step.id.clone(), max_retries: step.retries, board_job_id: None }).collect();
            let run = store.start_run(&NewRun {
                name,
                manifest_hash: hash,
                manifest_text: text,
                manifest_version: manifest.version,
                params,
                steps,
                board_run_id: None,
                workspace_root: root.to_string_lossy().into_owned(),
                now_ms: now_ms()?,
            }).map_err(|error| error.to_string())?;
            let _daemon = crate::daemon::spawn::connect_or_spawn(home).await.map_err(|error| error.to_string())?;
            println!("{}", run.id);
        }
        WorkflowAction::Status { run } => {
            let store = WorkflowStore::open(&home.join("aim.db")).map_err(|error| error.to_string())?;
            let record = store.get_run(&run).map_err(|error| error.to_string())?.ok_or("workflow run not found")?;
            println!("{}\t{}\t{:?}", record.id, record.name, record.state);
            for step in store.list_steps(&run).map_err(|error| error.to_string())? {
                println!("{}\t{:?}\t{}", step.name, step.state, step.attempt);
            }
        }
        WorkflowAction::Cancel { run } => {
            let store = WorkflowStore::open(&home.join("aim.db")).map_err(|error| error.to_string())?;
            let _record = store.request_cancel(&run, now_ms()?).map_err(|error| error.to_string())?;
            println!("cancellation requested for {run}");
        }
    }
    Ok(0)
}
