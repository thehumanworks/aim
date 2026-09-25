//! `Glob` and `Grep`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use aim_proto::harness::CaseMode;
use aim_proto::tool::ToolResult;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolCtx, clip_chars, model_error, parse};
use crate::authz::Access;
use crate::workspace::{GlobQuery, GrepQuery, Outcome};

/// Paths returned by `Glob`.
const GLOB_LIMIT: u32 = 1000;
/// Matches collected by `Grep` before output shaping.
const GREP_LIMIT: u32 = 5000;
/// Characters kept per matching line.
const MAX_LINE_CHARS: usize = 500;

pub(super) fn glob_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Glob pattern, e.g. `**/*.rs`"},
            "path": {"type": "string", "description": "Directory to search (default: the workspace root)"}
        },
        "required": ["pattern"]
    })
}

pub(super) fn grep_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regular expression"},
            "path": {"type": "string", "description": "File or directory to search (default: the workspace root)"},
            "glob": {"type": "string", "description": "Only files matching this glob, e.g. `*.rs`"},
            "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
            "-i": {"type": "boolean", "description": "Ignore case"},
            "-n": {"type": "boolean", "description": "Line numbers in content mode (default true)"},
            "-C": {"type": "integer", "minimum": 0, "description": "Context lines in content mode"},
            "head_limit": {"type": "integer", "minimum": 1, "description": "Keep only the first N entries"}
        },
        "required": ["pattern"]
    })
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

pub(super) async fn glob(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: GlobArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let path = ctx.grant.path(args.path.as_deref().unwrap_or(""), Access::Read)?;
    let patterns = [args.pattern];
    let query = GlobQuery { patterns: &patterns, path: &path, max_results: GLOB_LIMIT };
    let found = match ctx.workspace.search().glob(query).await {
        Ok(found) => found,
        Err(err) => return model_error(err),
    };
    if found.paths.is_empty() {
        return Ok(ToolResult::text("No files found"));
    }
    let mut out = found.paths.join("\n");
    if found.truncated {
        let _ = write!(out, "\n… more than {GLOB_LIMIT} files; use a narrower pattern or path");
    }
    Ok(ToolResult::text(out))
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default, rename = "-i")]
    ignore_case: bool,
    #[serde(default, rename = "-n")]
    line_numbers: Option<bool>,
    #[serde(default, rename = "-C")]
    context: Option<u32>,
    #[serde(default)]
    head_limit: Option<usize>,
}

pub(super) async fn grep(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: GrepArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let path = ctx.grant.path(args.path.as_deref().unwrap_or(""), Access::Read)?;
    let globs: Vec<String> = args.glob.iter().cloned().collect();
    let context = if args.output_mode == OutputMode::Content { args.context.unwrap_or(0).min(20) } else { 0 };
    let query = GrepQuery {
        pattern: &args.pattern,
        path: &path,
        globs: &globs,
        case: if args.ignore_case { CaseMode::Insensitive } else { CaseMode::Sensitive },
        fixed_strings: false,
        context,
        max_matches: GREP_LIMIT,
    };
    let found = match ctx.workspace.search().grep(query).await {
        Ok(found) => found,
        Err(err) => return model_error(err),
    };
    if found.matches.is_empty() {
        return Ok(ToolResult::text("No matches found"));
    }
    let mut entries: Vec<String> = Vec::new();
    match args.output_mode {
        OutputMode::FilesWithMatches => {
            for m in &found.matches {
                if entries.last() != Some(&m.path) {
                    entries.push(m.path.clone());
                }
            }
        }
        OutputMode::Count => {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for m in &found.matches {
                *counts.entry(m.path.as_str()).or_default() += 1;
            }
            entries.extend(counts.into_iter().map(|(path, count)| format!("{path}:{count}")));
        }
        OutputMode::Content => {
            let numbers = args.line_numbers.unwrap_or(true);
            let line = |path: &str, number: u64, sep: char, text: &str| {
                let text = clip_chars(text, MAX_LINE_CHARS);
                if numbers { format!("{path}{sep}{number}{sep}{text}") } else { format!("{path}{sep}{text}") }
            };
            // The last line printed per file, so overlapping context is not repeated.
            let mut printed: Option<(&str, u64)> = None;
            for m in &found.matches {
                let first = m.line.saturating_sub(m.before.len() as u64);
                let last_printed = printed.filter(|(path, _)| *path == m.path).map_or(0, |(_, line)| line);
                if context > 0 && printed.is_some() && first > last_printed.saturating_add(1) {
                    entries.push("--".to_owned());
                }
                for (i, text) in m.before.iter().enumerate() {
                    let number = first + i as u64;
                    if number > last_printed {
                        entries.push(line(&m.path, number, '-', text));
                    }
                }
                if m.line > last_printed {
                    entries.push(line(&m.path, m.line, ':', &m.text));
                }
                for (i, text) in m.after.iter().enumerate() {
                    entries.push(line(&m.path, m.line + 1 + i as u64, '-', text));
                }
                printed = Some((m.path.as_str(), m.line + m.after.len() as u64));
            }
        }
    }
    let total = entries.len();
    if let Some(limit) = args.head_limit {
        entries.truncate(limit.max(1));
    }
    let mut out = entries.join("\n");
    if entries.len() < total {
        let _ = write!(out, "\n… {} more entries (head_limit)", total - entries.len());
    }
    if found.truncated {
        let _ = write!(out, "\n… stopped after {GREP_LIMIT} matches; narrow the pattern or path");
    }
    Ok(ToolResult::text(out))
}
