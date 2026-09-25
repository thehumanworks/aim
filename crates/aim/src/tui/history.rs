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

/// Opens the history file only if it is a regular file and not a symbolic link, and only if the
/// file opened is the one named (checked after opening, so a swap in between is refused).
fn open_checked(path: &Path, options: &std::fs::OpenOptions) -> std::io::Result<std::fs::File> {
    let refuse = |why: &str| std::io::Error::other(format!("{}: {why}", path.display()));
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(refuse("is a symbolic link"));
        }
        if !meta.is_file() {
            return Err(refuse("is not a regular file"));
        }
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let opened = file.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        if named.file_type().is_symlink() || opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err(refuse("changed while it was opened"));
        }
    }
    Ok(file)
}

/// Past prompts, oldest first (the file's last [`MAX_READ`] bytes; nothing when it is missing or
/// not a plain file).
pub fn load(path: &Path) -> Vec<String> {
    let Ok(mut file) = open_checked(path, std::fs::OpenOptions::new().read(true)) else { return Vec::new() };
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

/// Appends one prompt, creating the file and its directory when needed. The file is owner-only
/// (0600) afterwards, even if it existed with wider permissions; a symbolic link is refused.
///
/// # Errors
/// When the file cannot be created, secured or written, or is a symbolic link.
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
    let mut file = open_checked(path, &options)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
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

    /// REV10 #14: appending makes an existing history file private and refuses symlinks.
    #[cfg(unix)]
    #[test]
    fn rev10_existing_files_become_private_and_symlinks_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("aim-history-perm-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append(&path, "new").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let target = dir.join("elsewhere");
        std::fs::write(&target, "").unwrap();
        let link = dir.join("linked-history");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(append(&link, "secret").is_err(), "a symlinked history is refused");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "", "nothing was written through the link");
        assert!(load(&link).is_empty(), "nor read through it");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
