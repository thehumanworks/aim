//! What the model is told: stable instructions and the per-session environment
//! (docs/architecture.md §6.4, §6.8).
//!
//! Instructions are the cache-friendly prefix: aim's system prompt (versioned data, a target of
//! self-improvement) plus what the session's resources contribute — the project's `AGENTS.md`
//! walk and `.agents/instructions.md` (read **through the workspace**, so a remote project's
//! instructions apply under SSH), the rules index, the skill catalog, the memory index and a named
//! agent's instructions ([`crate::resources`]). They are fixed at session start. The environment
//! (directory, OS, date) changes between sessions, so it goes in the first user message, never in
//! the prefix.

use std::time::Duration;

use aim_llm::ModelProvider;
use aim_proto::ids::WorkspaceId;
use aim_rpc::Peer;

use crate::resources::agents::AgentDef;
use crate::resources::files::Read;
use crate::resources::instructions::{self, Prefix};
use crate::resources::{Catalog, Files as _, HarnessFiles};

/// aim's built-in system prompt.
pub const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

/// The system prompt's "# Code mode" section (ADR 0076), sent only to sessions that are offered a
/// code tool (`run_code`, or codex's `exec`): a session without one would pay for advice it
/// cannot follow.
pub const CODE_MODE_PROMPT: &str = include_str!("../prompts/code_mode.md");

/// Longest wait for the provider's catalog when sizing the skill catalog.
pub const WINDOW_LOOKUP: Duration = Duration::from_secs(2);

pub use crate::resources::instructions::MAX_PROJECT_INSTRUCTIONS;

/// The workspace root's `AGENTS.md` (else `CLAUDE.md`), read through the harness and capped at
/// [`MAX_PROJECT_INSTRUCTIONS`], as `(file name, text)`. For backends that take project
/// instructions whole (Claude Code over ACP); the native loop uses the full catalog.
pub async fn project_instructions(peer: &Peer, workspace: &WorkspaceId) -> Option<(String, String)> {
    let files = HarnessFiles::new(peer.clone(), workspace.clone());
    let names = ["AGENTS.md", "CLAUDE.md"];
    let reads = files.read_many(names.iter().map(|n| (*n).to_owned()).collect(), MAX_PROJECT_INSTRUCTIONS as u64).await;
    names.into_iter().zip(reads).find_map(|(name, read)| match read {
        Read::Ok(file) => {
            let mut text = file.text;
            if file.truncated {
                text.push_str("\n[… truncated]");
            }
            Some((name.to_owned(), text))
        }
        Read::Missing | Read::Failed(_) => None,
    })
}

/// The full instructions: aim's system prompt (with its "# Code mode" section when `code` says
/// the session is offered a code tool), then the catalog's prefix (project instructions, rules
/// index, skills within `skill_budget` bytes, memory) and `agent`'s instructions.
#[must_use]
pub fn instructions(catalog: &Catalog, agent: Option<&AgentDef>, skill_budget: usize, code: bool) -> Prefix {
    let prefix = instructions::render(catalog, agent, skill_budget);
    let system =
        if code { format!("{}\n\n{}", SYSTEM_PROMPT.trim_end(), CODE_MODE_PROMPT.trim_end()) } else { SYSTEM_PROMPT.trim_end().to_owned() };
    Prefix { text: format!("{system}{}", prefix.text).trim_end().to_owned() + "\n", diagnostics: prefix.diagnostics }
}

/// `model`'s context window in tokens, from `provider`'s catalog, if it answers within
/// [`WINDOW_LOOKUP`].
pub async fn context_window(provider: &dyn ModelProvider, model: &str) -> Option<u64> {
    let models = tokio::time::timeout(WINDOW_LOOKUP, provider.catalog()).await.ok()?.ok()?;
    models.iter().find(|m| m.id == model).and_then(|m| m.context_window)
}

/// The environment block that opens the first user message.
#[must_use]
pub fn environment(root: &str, location: &str, os: &str, date: &str) -> String {
    let remote = if location == "local" { String::new() } else { format!("\nThe workspace is remote ({location}); tools act there.") };
    format!("<environment>\nworkspace: {root}\nos: {os}\ndate: {date}{remote}\n</environment>")
}

#[cfg(test)]
mod tests {
    use super::{CODE_MODE_PROMPT, SYSTEM_PROMPT, instructions};
    use crate::resources::Catalog;

    /// ADR 0076: the code-mode section follows the system prompt exactly when a code tool is
    /// offered, so a code-mode session's prefix is what it was when the section lived in
    /// `system.md`, and a session without code mode does not pay for it.
    #[test]
    fn the_code_mode_section_goes_only_to_sessions_with_a_code_tool() {
        let catalog = Catalog::default();
        let off = instructions(&catalog, None, 4096, false).text;
        let on = instructions(&catalog, None, 4096, true).text;
        assert!(!off.contains("# Code mode") && !off.contains("run_code"), "{off}");
        assert_eq!(off, format!("{}\n", SYSTEM_PROMPT.trim_end()));
        assert_eq!(on, format!("{}\n\n{}\n", SYSTEM_PROMPT.trim_end(), CODE_MODE_PROMPT.trim_end()));
        assert!(CODE_MODE_PROMPT.starts_with("# Code mode\n"));
    }
}
