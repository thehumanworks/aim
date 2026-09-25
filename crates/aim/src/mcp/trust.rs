//! Private, atomic grants for MCP definitions. A changed entry hash loses its grant.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::config::{Location, ServerEntry};

const MAX_TRUST_BYTES: u64 = 64 * 1024;
const MAX_GRANTS: usize = 512;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    #[serde(default)]
    grant: Vec<Grant>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    source_path: String,
    hash: String,
    location: String,
}

impl Grant {
    fn of(entry: &ServerEntry) -> Self {
        Self { source_path: entry.source_path.clone(), hash: entry.hash.clone(), location: location_name(entry.location).to_owned() }
    }
}

/// Read-only snapshot of trust grants for discovery.
pub struct Grants(Vec<Grant>);

impl Grants {
    /// Whether this source, hash, and execution location was granted.
    pub fn contains(&self, path: &str, hash: &str, location: Location) -> bool {
        self.0.iter().any(|grant| grant.source_path == path && grant.hash == hash && grant.location == location_name(location))
    }
}

/// Read trust grants from `aim_home/trust.toml`; reject symlinks and unsafe files.
///
/// # Errors
/// Returns an error for an unsafe, unreadable, malformed, or oversized trust file.
pub fn grants(aim_home: &Path) -> Result<Grants, String> {
    let dir = aim_home;
    if !check_dir(dir)? {
        return Ok(Grants(Vec::new()));
    }
    let path = dir.join("trust.toml");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Grants(Vec::new())),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let directory = fs::symlink_metadata(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    if directory.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} must be a private directory", dir.display()));
    }
    if !metadata.is_file() || metadata.uid() != current_uid() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} must be an owned private regular file", path.display()));
    }
    if metadata.len() > MAX_TRUST_BYTES {
        return Err(format!("{} exceeds trust file limit", path.display()));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let mut text = String::new();
    file.take(MAX_TRUST_BYTES + 1).read_to_string(&mut text).map_err(|err| format!("{}: {err}", path.display()))?;
    if text.len() as u64 > MAX_TRUST_BYTES {
        return Err(format!("{} exceeds trust file limit", path.display()));
    }
    let parsed: TrustFile = toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    if parsed.grant.len() > MAX_GRANTS {
        return Err(format!("{} exceeds trust grant limit", path.display()));
    }
    Ok(Grants(parsed.grant))
}

/// Grant this exact definition. The operation replaces the trust file atomically.
///
/// # Errors
/// Returns an error if the private store is unsafe, full, or cannot be written.
pub fn trust(aim_home: &Path, entry: &ServerEntry) -> Result<(), String> {
    update(aim_home, entry, true)
}

/// Revoke this exact definition.
///
/// # Errors
/// Returns an error if the private store is unsafe or cannot be written.
pub fn untrust(aim_home: &Path, entry: &ServerEntry) -> Result<(), String> {
    update(aim_home, entry, false)
}

fn update(aim_home: &Path, entry: &ServerEntry, add: bool) -> Result<(), String> {
    if entry.source_path.is_empty() || entry.hash.is_empty() {
        return Err("invalid MCP trust key".to_owned());
    }
    let dir = aim_home;
    prepare_dir(dir)?;
    let mut file = TrustFile { grant: grants(aim_home)?.0 };
    let key = Grant::of(entry);
    file.grant.retain(|grant| !(grant.source_path == key.source_path && grant.hash == key.hash && grant.location == key.location));
    if add {
        if file.grant.len() >= MAX_GRANTS {
            return Err("MCP trust grant limit reached".to_owned());
        }
        file.grant.push(key);
    }
    file.grant.sort_by(|a, b| (&a.source_path, &a.hash, &a.location).cmp(&(&b.source_path, &b.hash, &b.location)));
    let encoded = toml::to_string(&file).map_err(|err| format!("serializing MCP trust: {err}"))?;
    if encoded.len() as u64 > MAX_TRUST_BYTES {
        return Err("MCP trust file limit reached".to_owned());
    }
    let path = dir.join("trust.toml");
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    temp.as_file().set_permissions(fs::Permissions::from_mode(0o600)).map_err(|err| format!("{}: {err}", dir.display()))?;
    temp.write_all(encoded.as_bytes()).map_err(|err| format!("{}: {err}", dir.display()))?;
    temp.as_file().sync_all().map_err(|err| format!("{}: {err}", dir.display()))?;
    temp.persist(&path).map_err(|err| format!("{}: {}", path.display(), err.error))?;
    File::open(dir).and_then(|file| file.sync_all()).map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(())
}

fn prepare_dir(dir: &Path) -> Result<(), String> {
    if !check_dir(dir)? {
        fs::create_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    let metadata = fs::symlink_metadata(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    if !metadata.is_dir() || metadata.uid() != current_uid() {
        return Err(format!("{} must be an owned directory", dir.display()));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(())
}

fn check_dir(dir: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() && metadata.uid() == current_uid() => Ok(true),
        Ok(_) => Err(format!("{} must be an owned directory, not a symlink", dir.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("{}: {err}", dir.display())),
    }
}

fn current_uid() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

fn location_name(location: Location) -> &'static str {
    match location {
        Location::Workspace => "workspace",
        Location::Local => "local",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::{Origin, Transport};
    use std::collections::BTreeMap;

    fn entry() -> ServerEntry {
        ServerEntry {
            name: "docs".to_owned(),
            source_path: "/remote/project/.mcp.json".to_owned(),
            hash: "sha256:abc".to_owned(),
            location: Location::Workspace,
            transport: Transport::Stdio { command: "tool".to_owned(), args: Vec::new(), env: BTreeMap::new() },
            trusted: false,
            origin: Origin::Claude,
        }
    }

    #[test]
    fn grants_are_private_atomic_and_revocable() {
        let root = tempfile::tempdir().unwrap();
        let aim_home = root.path().join("aim-home");
        let entry = entry();
        trust(&aim_home, &entry).unwrap();
        assert!(grants(&aim_home).unwrap().contains(&entry.source_path, &entry.hash, entry.location));
        let metadata = fs::metadata(aim_home.join("trust.toml")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&aim_home).unwrap().permissions().mode() & 0o777, 0o700);
        untrust(&aim_home, &entry).unwrap();
        assert!(!grants(&aim_home).unwrap().contains(&entry.source_path, &entry.hash, entry.location));
    }

    #[test]
    fn symlinks_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let aim_home = root.path().join("aim-home");
        let other = root.path().join("other");
        fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, &aim_home).unwrap();
        assert!(trust(&aim_home, &entry()).is_err());
        fs::remove_file(&aim_home).unwrap();
        fs::create_dir(&aim_home).unwrap();
        std::os::unix::fs::symlink(other.join("trust.toml"), aim_home.join("trust.toml")).unwrap();
        assert!(trust(&aim_home, &entry()).is_err());
    }
}
