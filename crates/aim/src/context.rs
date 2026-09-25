//! What the model is told: stable instructions and the per-session environment
//! (docs/architecture.md §6.4, §6.8).
//!
//! Instructions are the cache-friendly prefix: aim's system prompt (versioned data, a target of
//! self-improvement) plus the project's `AGENTS.md`, read **through the workspace** so a remote
//! project's instructions apply under SSH. The environment (directory, OS, date) changes between
//! sessions, so it goes in the first user message, never in the prefix.

use aim_proto::content::Content;
use aim_proto::harness::{FsRead, FsReadParams};
use aim_proto::ids::WorkspaceId;
use aim_rpc::Peer;

/// aim's built-in system prompt.
pub const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

/// Most bytes of project instructions included (codex uses a 32 KiB cap; tny the same order).
pub const MAX_PROJECT_INSTRUCTIONS: usize = 32 * 1024;

/// Reads `AGENTS.md` (or `CLAUDE.md`) at the workspace root through the harness, capped.
pub async fn project_instructions(peer: &Peer, workspace: &WorkspaceId) -> Option<(String, String)> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let params = FsReadParams { workspace: workspace.clone(), path: name.to_owned(), range: None };
        if let Ok(read) = peer.call::<FsRead>(params).await
            && let Content::Utf8 { text } = read.content
        {
            let mut text = text;
            if text.len() > MAX_PROJECT_INSTRUCTIONS {
                let mut cut = MAX_PROJECT_INSTRUCTIONS;
                while !text.is_char_boundary(cut) {
                    cut = cut.saturating_sub(1);
                }
                text.truncate(cut);
                text.push_str("\n[… truncated]");
            }
            return Some((name.to_owned(), text));
        }
    }
    None
}

/// The full instructions: system prompt, then project instructions under a labelled heading.
#[must_use]
pub fn instructions(project: Option<&(String, String)>, location: &str) -> String {
    let mut out = String::from(SYSTEM_PROMPT);
    if let Some((name, text)) = project {
        out.push_str("\n# Project instructions (");
        out.push_str(name);
        if location != "local" {
            out.push_str(", from the remote workspace");
        }
        out.push_str(")\n\n");
        out.push_str(text.trim());
        out.push('\n');
    }
    out
}

/// The environment block that opens the first user message.
#[must_use]
pub fn environment(root: &str, location: &str, os: &str, date: &str) -> String {
    let remote = if location == "local" { String::new() } else { format!("\nThe workspace is remote ({location}); tools act there.") };
    format!("<environment>\nworkspace: {root}\nos: {os}\ndate: {date}{remote}\n</environment>")
}
