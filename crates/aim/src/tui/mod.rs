//! The terminal UI (docs/architecture.md §11, docs/adr/0015): chat inline in native scrollback
//! with a pinned bottom block, an alternate-screen session picker, and an optional fullscreen
//! layout over the same transcript model.
//!
//! The TUI talks to sessions only through [`SessionClient`]: in process (a
//! [`SessionHost`](crate::host::SessionHost)) or over `aim-daemon/1`. It is a pure core with a thin
//! shell:
//!
//! - `app` — the state machine (session updates and keys in, a view model and effects out);
//! - `transcript` — the semantic transcript behind inline scrollback, fullscreen and reattach;
//! - `markdown`, `text` — Markdown to styled, wrapped rows; `theme` — colours in one place;
//! - `composer` — the multiline editor, paste chips, history and reverse search;
//! - `complete` — the async completion broker and its sources; `commands` — slash commands;
//! - `view` — the pinned block, fullscreen and picker layouts;
//! - `inline` — the scrollback writer; `schedule` — the frame scheduler;
//! - `shell` — terminal I/O and effects; `history` — the prompt history file.

mod app;
mod commands;
mod complete;
mod composer;
mod history;
mod inline;
mod markdown;
mod schedule;
#[cfg(feature = "test-support")]
pub mod script;
mod shell;
mod text;
mod theme;
mod transcript;
mod view;

use std::path::PathBuf;
use std::sync::Arc;

use aim_proto::daemon::{Location, Persistence, SessionSpec};

pub use complete::{Candidate, Kind, Request, Source, SourceFactory, Sources};
pub use shell::Options;

use crate::host::SessionClient;

/// Arguments of `aim` (no subcommand) and `aim tui`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct TuiArgs {
    /// Provider: codex, openrouter, ai-gateway, acp:claude.
    #[arg(short, long, default_value = "codex")]
    pub provider: String,
    /// Model id (the provider's default when omitted).
    #[arg(short, long)]
    pub model: Option<String>,
    /// Reasoning effort (from the model's catalog ladder).
    #[arg(short, long)]
    pub effort: Option<String>,
    /// Workspace directory.
    #[arg(short = 'C', long, default_value = ".")]
    pub cwd: PathBuf,
    /// Network aimx endpoint (bearer from `AIM_REMOTE_TOKEN` or `AIM_REMOTE_TOKEN_FILE`).
    #[arg(long)]
    pub remote: Option<String>,
    /// The aimx binary (default: next to aim, else on PATH).
    #[arg(long)]
    pub aimx: Option<PathBuf>,
    /// Keep nothing on disk: no database, no prompt history.
    #[arg(long)]
    pub ephemeral: bool,
    /// Start in the fullscreen layout (toggle with /fullscreen).
    #[arg(long)]
    pub fullscreen: bool,
    /// Attach to (or resume) this session instead of starting one.
    #[arg(long)]
    pub session: Option<String>,
    /// Most model requests per turn.
    #[arg(long, default_value_t = 64)]
    pub max_requests: u32,
    /// Test only: run against a scripted provider and a fake workspace described by this file.
    #[cfg(feature = "test-support")]
    #[arg(long, hide = true)]
    pub script: Option<PathBuf>,
}

impl TuiArgs {
    /// The workspace root (canonical locally, unchanged for a remote host).
    ///
    /// # Errors
    /// When a local directory does not exist.
    pub fn root(&self) -> Result<PathBuf, String> {
        if self.remote.is_some() {
            Ok(self.cwd.clone())
        } else {
            self.cwd.canonicalize().map_err(|e| format!("{}: {e}", self.cwd.display()))
        }
    }

    /// Options for [`run`] with local completion sources and the history file under `aim_home`.
    ///
    /// # Errors
    /// When a local workspace directory does not exist.
    pub fn options(&self) -> Result<Options, String> {
        let root = self.root()?;
        let home = crate::cli::aim_home();
        let spec = SessionSpec {
            workspace: root.to_string_lossy().into_owned(),
            location: self.remote.as_ref().map_or(Location::Local, |url| Location::Remote { url: url.clone() }),
            provider: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            agent: None,
            persistence: if self.ephemeral { Persistence::Ephemeral } else { Persistence::Persistent },
        };
        Ok(Options {
            spec,
            attach: self.session.clone(),
            fullscreen: self.fullscreen,
            history: (!self.ephemeral).then(|| home.join("history")),
            sources: Sources::factory(Some(home.join("skills"))),
            close_on_exit: true,
            keep_superseded_completions: false,
        })
    }
}

/// Runs the TUI against `client` until the user quits; returns the exit code.
///
/// # Errors
/// When the terminal cannot be used.
pub async fn run(client: Arc<dyn SessionClient>, options: Options) -> Result<i32, String> {
    shell::run(client, options).await
}

#[cfg(test)]
mod remote_tests {
    use std::path::PathBuf;

    use aim_proto::daemon::Location;

    use super::TuiArgs;

    #[test]
    fn remote_root_does_not_require_a_local_directory() {
        let args =
            TuiArgs { cwd: PathBuf::from("/remote-only/project"), remote: Some("wss://example.test/rpc".to_owned()), ..TuiArgs::default() };
        let options = args.options().unwrap();
        assert_eq!(options.spec.workspace, "/remote-only/project");
        assert_eq!(options.spec.location, Location::Remote { url: "wss://example.test/rpc".to_owned() });
    }
}
