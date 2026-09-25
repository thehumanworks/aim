//! The headless CLI: `aim run` drives one turn end to end (docs/architecture.md §11).
//!
//! It spawns the execution layer (`aimx serve --stdio --root <dir>`), reads the project's
//! instructions through it, records the session (SQLite, or memory with `--ephemeral`), runs the
//! native loop and renders the turn: assistant text on stdout, a compact tool log on stderr — or
//! every event as a JSON line with `--json`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aim_llm::ModelProvider;
use aim_proto::conversation::{Part, StopReason, Usage};
use aim_proto::event::SessionMeta;
use aim_proto::tool::{ToolContent, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentConfig, AgentEvent, ToolHost};
use crate::context;
use crate::harness::HarnessClient;
use crate::session::{self, Recorder};
use crate::store::{MemoryStore, SessionStore, SqliteStore};

/// Options of `aim run`.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Provider id (`codex`, `openrouter`, `ai-gateway`, …).
    pub provider: String,
    /// Model id; the provider's default when absent.
    pub model: Option<String>,
    /// Reasoning effort.
    pub effort: Option<String>,
    /// Workspace directory.
    pub cwd: PathBuf,
    /// Path of the `aimx` binary.
    pub aimx: Option<PathBuf>,
    /// Keep nothing on disk.
    pub ephemeral: bool,
    /// Print every event as a JSON line.
    pub json: bool,
    /// Most model requests in the turn.
    pub max_requests: u32,
    /// The prompt.
    pub prompt: String,
}

/// Builds a provider and resolves its default model.
pub type ProviderFactory = fn(&str, Option<&str>) -> Result<(Arc<dyn ModelProvider>, String), String>;

/// aim's home directory: `$AIM_HOME`, else `~/.aim`.
#[must_use]
pub fn aim_home() -> PathBuf {
    std::env::var_os("AIM_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aim")))
        .unwrap_or_else(|| PathBuf::from(".aim"))
}

/// The `aimx` binary: `--aimx`, `$AIM_AIMX`, next to this executable, else `aimx` on `PATH`.
#[must_use]
pub fn find_aimx(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = std::env::var_os("AIM_AIMX") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("aimx");
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from("aimx")
}

/// Today's date (UTC) as `YYYY-MM-DD`.
#[must_use]
pub fn today() -> String {
    date_from_days(session::now_ms().div_euclid(86_400_000))
}

// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn date_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max { flat } else { format!("{}…", flat.chars().take(max).collect::<String>()) }
}

fn result_summary(result: &ToolResult) -> String {
    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            ToolContent::Text { text } => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .unwrap_or("");
    if result.is_error { format!("error: {}", one_line(text, 160)) } else { format!("ok {}", one_line(text, 80)) }
}

/// Renders events for a human: text to stdout, the tool log to stderr.
#[derive(Default)]
struct Human {
    usage: Usage,
    requests: u32,
}

impl Human {
    fn show(&mut self, event: &AgentEvent) {
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        // Terminal output failures (a closed pipe) leave nothing better to do than carry on.
        let _ignored = match event {
            AgentEvent::TextDelta { delta } => write!(out, "{delta}").and_then(|()| out.flush()),
            AgentEvent::ToolStarted { name, arguments, .. } => {
                writeln!(out).and_then(|()| writeln!(err, "⏺ {name} {}", one_line(arguments, 120)))
            }
            AgentEvent::ToolFinished { result, .. } => writeln!(err, "  ⎿ {}", result_summary(result)),
            AgentEvent::RequestStarted { index } => {
                self.requests = *index;
                Ok(())
            }
            AgentEvent::Usage { usage } => {
                self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
                self.usage.cached_input_tokens = self.usage.cached_input_tokens.saturating_add(usage.cached_input_tokens);
                self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
                self.usage.reasoning_tokens = self.usage.reasoning_tokens.saturating_add(usage.reasoning_tokens);
                Ok(())
            }
            AgentEvent::SteersReturned { steers } => writeln!(err, "({} unsent message(s) returned)", steers.len()),
            AgentEvent::TurnFailed { message } => writeln!(out).and_then(|()| writeln!(err, "✗ turn failed: {message}")),
            AgentEvent::TurnEnded { stop } => {
                let u = &self.usage;
                writeln!(out).and_then(|()| {
                    writeln!(
                        err,
                        "· {:?} · {} request(s) · {} in ({} cached) · {} out ({} reasoning)",
                        stop, self.requests, u.input_tokens, u.cached_input_tokens, u.output_tokens, u.reasoning_tokens
                    )
                })
            }
            AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ItemAdded { .. }
            | AgentEvent::SteerQueued
            | AgentEvent::SteerDelivered { .. }
            | AgentEvent::RateLimits { .. } => Ok(()),
        };
    }
}

/// Runs one turn headlessly. Returns the process exit code.
///
/// # Errors
/// A message for the user when setup fails (harness, provider, store).
pub async fn run(options: RunOptions, factory: ProviderFactory) -> Result<i32, String> {
    let root = options.cwd.canonicalize().map_err(|e| format!("{}: {e}", options.cwd.display()))?;
    let root_str = root.to_string_lossy().into_owned();
    let (provider, default_model) = factory(&options.provider, options.model.as_deref())?;
    let model = options.model.clone().unwrap_or(default_model);

    let aimx = find_aimx(options.aimx.as_deref());
    let harness =
        HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root_str).await.map_err(|e| format!("harness ({}): {e}", aimx.display()))?;
    let project = context::project_instructions(harness.peer(), &harness.workspace().id).await;
    let instructions = context::instructions(project.as_ref(), "local");

    let store: Arc<dyn SessionStore> = if options.ephemeral {
        Arc::new(MemoryStore::default())
    } else {
        Arc::new(SqliteStore::open(&aim_home().join("aim.db")).map_err(|e| e.to_string())?)
    };
    let session_id = session::new_session_id();
    let meta = SessionMeta {
        id: session_id.clone(),
        created_ms: session::now_ms(),
        workspace: root_str.clone(),
        location: "local".to_owned(),
        provider: options.provider.clone(),
        model: model.clone(),
        title: None,
        parent: None,
    };
    let mut recorder = Recorder::create(Arc::clone(&store), meta).await.map_err(|e| e.to_string())?;
    recorder.begin_turn().await.map_err(|e| e.to_string())?;

    let harness = Arc::new(harness);
    let tools: Arc<dyn ToolHost> = Arc::clone(&harness) as Arc<dyn ToolHost>;
    let config = AgentConfig {
        model,
        instructions,
        effort: options.effort.clone(),
        tier: None,
        session_id: session_id.clone(),
        cache_key: Some(format!("aim:{root_str}")),
        parallel_tool_calls: true,
        max_requests: options.max_requests,
    };
    let mut agent = Agent::new(provider, tools, config);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let json = options.json;
    let printer = tokio::spawn(async move {
        let mut human = Human::default();
        let mut failures = 0_u32;
        while let Some(event) = rx.recv().await {
            if json {
                if let Ok(line) = serde_json::to_string(&event) {
                    let mut out = std::io::stdout().lock();
                    let _ignored = writeln!(out, "{line}");
                }
            } else {
                human.show(&event);
            }
            if recorder.observe(&event).await.is_err() {
                failures = failures.saturating_add(1);
            }
        }
        failures
    });

    let cancel = CancellationToken::new();
    let on_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_signal.cancel();
        }
    });

    let env = context::environment(&root_str, "local", std::env::consts::OS, &today());
    let input = vec![Part::Text { text: format!("{env}\n\n{}", options.prompt) }];
    let outcome = agent.run_turn(input, &tx, &cancel).await;
    drop(tx);
    let store_failures = printer.await.unwrap_or(1);
    if store_failures > 0 && !options.ephemeral {
        let mut err = std::io::stderr().lock();
        let _ignored = writeln!(err, "warning: {store_failures} event(s) could not be recorded");
    }
    if !options.json && !options.ephemeral {
        let mut err = std::io::stderr().lock();
        let _ignored = writeln!(err, "· session {session_id}");
    }
    Ok(match outcome {
        Ok(StopReason::Cancelled) => 130,
        Ok(_) => 0,
        Err(_) => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::date_from_days;

    #[test]
    fn civil_dates_from_unix_days() {
        for (days, expected) in [
            (0, "1970-01-01"),
            (11_016, "2000-02-29"),
            (11_017, "2000-03-01"),
            (10_956, "1999-12-31"),
            (10_957, "2000-01-01"),
            (2_932_896, "9999-12-31"),
        ] {
            assert_eq!(date_from_days(days), expected, "day {days}");
        }
    }
}
