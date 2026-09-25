//! `Read`, `Write`, `Edit` and `LS`.

use std::fmt::Write as _;

use aim_proto::content::{Base64Bytes, Content};
use aim_proto::harness::{EntryKind, ExactEdit, Precondition};
use aim_proto::tool::{ToolContent, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolCtx, clip_chars, model_error, parse};
use crate::authz::Access;
use crate::workspace::{EditRequest, ListRequest, Outcome, WriteRequest};

/// Lines returned by default.
const DEFAULT_LINES: usize = 2000;
/// Characters kept per line.
const MAX_LINE_CHARS: usize = 2000;
/// Largest image returned inline.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Entries listed by `LS`.
const LS_LIMIT: u32 = 1000;

pub(super) fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "File to read (relative to the workspace root, or absolute)"},
            "offset": {"type": "integer", "minimum": 1, "description": "First line to return (1-based)"},
            "limit": {"type": "integer", "minimum": 1, "description": "Maximum lines to return"}
        },
        "required": ["file_path"]
    })
}

pub(super) fn write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "File to write"},
            "content": {"type": "string", "description": "The complete new content"}
        },
        "required": ["file_path", "content"]
    })
}

pub(super) fn edit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "File to edit"},
            "old_string": {"type": "string", "description": "Exact text to replace"},
            "new_string": {"type": "string", "description": "Replacement text"},
            "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
        },
        "required": ["file_path", "old_string", "new_string"]
    })
}

pub(super) fn ls_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory to list (default: the workspace root)"}
        }
    })
}

#[derive(Deserialize)]
struct ReadArgs {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The media type of an image format the models accept, by magic bytes.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Numbered lines `offset..offset+limit` of `text`, `cat -n` style, plus a note on what was left
/// out.
fn render_lines(text: &str, offset: usize, limit: usize) -> (String, bool) {
    let total = text.lines().count();
    if total == 0 {
        return ("(empty file)".to_owned(), false);
    }
    if offset > total {
        return (format!("(the file has {total} lines; offset {offset} is past the end)"), true);
    }
    let mut out = String::new();
    let mut last = offset;
    for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let number = index + 1;
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "{number:>6}\t{}", clip_chars(line, MAX_LINE_CHARS));
        last = number;
    }
    if last < total {
        let _ = write!(out, "\n… showing lines {offset}-{last} of {total}; pass offset/limit to read more");
    }
    (out, false)
}

pub(super) async fn read(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: ReadArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let path = ctx.grant.path(&args.file_path, Access::Read)?;
    let display = ctx.grant.display(&path).to_owned();
    let fs = ctx.workspace.fs();
    match fs.stat(&path, false).await {
        Ok(meta) if meta.kind == EntryKind::Dir => return Ok(ToolResult::error(format!("`{display}` is a directory; use LS"))),
        Ok(_) => {}
        Err(err) => return model_error(err),
    }
    let read = match fs.read(&path, None, ctx.max_read_bytes, true).await {
        Ok(read) => read,
        Err(err) => return model_error(err),
    };
    let (size, truncated) = (read.size, read.truncated);
    let bytes = read.content.into_bytes();
    if let Some(media_type) = image_type(&bytes) {
        if truncated || bytes.len() > MAX_IMAGE_BYTES {
            return Ok(ToolResult::error(format!("`{display}` is an image of {size} bytes, too large to return")));
        }
        return Ok(ToolResult {
            content: vec![ToolContent::Image { media_type: media_type.to_owned(), data: Base64Bytes(bytes) }],
            ..ToolResult::default()
        });
    }
    let binary = || ToolResult::error(format!("`{display}` is a binary file ({size} bytes); not shown"));
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Ok(binary());
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        // A read cut at the byte limit may end inside a character: keep the valid prefix.
        Err(err) if truncated && err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        Err(_) => return Ok(binary()),
    };
    let offset = args.offset.unwrap_or(1).max(1);
    let limit = args.limit.unwrap_or(DEFAULT_LINES).max(1);
    let (mut out, is_error) = render_lines(&text, offset, limit);
    if truncated {
        let _ = write!(out, "\n… the file is {size} bytes; only the first {} were read", ctx.max_read_bytes);
    }
    Ok(if is_error { ToolResult::error(out) } else { ToolResult::text(out) })
}

#[derive(Deserialize)]
struct WriteArgs {
    file_path: String,
    content: String,
}

pub(super) async fn write(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: WriteArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let path = ctx.grant.path(&args.file_path, Access::Write)?;
    let display = ctx.grant.display(&path).to_owned();
    let key = ctx.derived_key("write");
    let content = Content::Utf8 { text: args.content };
    let request = WriteRequest { path: &path, content: &content, precondition: &Precondition::Any, create_dirs: true, key: &key };
    match ctx.workspace.fs().write(request).await {
        Ok(outcome) => {
            let verb = if outcome.created { "Created" } else { "Wrote" };
            Ok(ToolResult::text(format!("{verb} {display} ({} bytes)", outcome.size)))
        }
        Err(err) => model_error(err),
    }
}

#[derive(Deserialize)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

pub(super) async fn edit(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: EditArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    if args.old_string.is_empty() {
        return Ok(ToolResult::error("old_string must not be empty (use Write to create a file)"));
    }
    if args.old_string == args.new_string {
        return Ok(ToolResult::error("old_string and new_string are identical"));
    }
    let path = ctx.grant.path(&args.file_path, Access::Write)?;
    // The match result reveals content, so an edit needs read authority too (REV14 F5).
    ctx.grant.path(&args.file_path, Access::Read)?;
    let display = ctx.grant.display(&path).to_owned();
    let key = ctx.derived_key("edit");
    let edits = [ExactEdit { old: args.old_string, new: args.new_string, replace_all: args.replace_all }];
    let request = EditRequest { path: &path, edits: &edits, precondition: &Precondition::Any, key: &key };
    match ctx.workspace.fs().edit(request).await {
        Ok(outcome) => {
            let count = outcome.replacements.first().copied().unwrap_or(0);
            let plural = if count == 1 { "" } else { "s" };
            Ok(ToolResult::text(format!("Replaced {count} occurrence{plural} in {display}")))
        }
        Err(err) if err.code == aim_proto::error::ErrorCode::Conflict => {
            let occurrences = err.detail.as_ref().and_then(|d| d.get("occurrences")).and_then(Value::as_u64);
            Ok(ToolResult::error(match occurrences {
                Some(0) => format!("old_string not found in {display}"),
                Some(n) => {
                    format!("old_string occurs {n} times in {display}; add surrounding context to make it unique, or set replace_all")
                }
                None => err.message,
            }))
        }
        Err(err) => model_error(err),
    }
}

#[derive(Deserialize)]
struct LsArgs {
    #[serde(default)]
    path: Option<String>,
}

pub(super) async fn ls(ctx: &ToolCtx, arguments: Value) -> Outcome<ToolResult> {
    let args: LsArgs = match parse(arguments) {
        Ok(args) => args,
        Err(result) => return Ok(result),
    };
    let path = ctx.grant.path(args.path.as_deref().unwrap_or(""), Access::Read)?;
    let request = ListRequest { path: &path, limit: LS_LIMIT, page_token: None, include_hidden: true };
    let listing = match ctx.workspace.fs().list(request).await {
        Ok(listing) => listing,
        Err(err) => return model_error(err),
    };
    if listing.entries.is_empty() {
        return Ok(ToolResult::text("(empty directory)"));
    }
    let mut out = String::new();
    for entry in &listing.entries {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&entry.name);
        match entry.kind {
            EntryKind::Dir => out.push('/'),
            EntryKind::Symlink => out.push('@'),
            EntryKind::File | EntryKind::Other => {}
        }
    }
    if listing.next_page.is_some() {
        let _ = write!(out, "\n… more than {LS_LIMIT} entries; narrow the path or use Glob");
    }
    Ok(ToolResult::text(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_lines() {
        let (out, err) = render_lines("a\nb\nc\n", 1, 2000);
        assert!(!err);
        assert_eq!(out, "     1\ta\n     2\tb\n     3\tc");
        let (out, _) = render_lines("a\nb\nc", 2, 1);
        assert_eq!(out, "     2\tb\n… showing lines 2-2 of 3; pass offset/limit to read more");
        assert!(render_lines("a", 5, 1).1);
        assert_eq!(render_lines("", 1, 1).0, "(empty file)");
    }

    #[test]
    fn images_by_magic() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8"), Some("image/webp"));
        assert_eq!(image_type(b"plain"), None);
    }
}
