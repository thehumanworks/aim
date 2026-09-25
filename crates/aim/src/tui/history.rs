//! Prompt history on disk (`aim_home()/history`): one prompt per line, `\` and newlines escaped,
//! the file private to the user. Ephemeral sessions never read or write it (docs/architecture.md
//! §5.4).

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

/// Most bytes read from the end of the history file.
pub const MAX_READ: u64 = 1024 * 1024;

fn escape(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Past prompts, oldest first (the file's last [`MAX_READ`] bytes; nothing when it is missing).
pub fn load(path: &Path) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let len = file.metadata().map_or(0, |m| m.len());
    let skip_partial = len > MAX_READ;
    if skip_partial && file.seek(SeekFrom::Start(len - MAX_READ)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    text.lines().skip(usize::from(skip_partial)).filter(|l| !l.is_empty()).map(unescape).collect()
}

/// Appends one prompt, creating the file (mode 0600) and its directory when needed.
///
/// # Errors
/// When the file cannot be created or written.
pub fn append(path: &Path, entry: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{}", escape(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("aim-history-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("history");
        assert!(load(&path).is_empty());
        for entry in ["one", "two\nlines", "back\\slash\\n"] {
            append(&path, entry).unwrap();
        }
        assert_eq!(load(&path), ["one", "two\nlines", "back\\slash\\n"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
