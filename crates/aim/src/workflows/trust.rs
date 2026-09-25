//! Private, hash-pinned trust for project workflow manifests.
//!
//! The caller reads the manifest through its workspace and supplies its canonical absolute source
//! path and SHA-256 of the complete source bytes. Editing a manifest changes its hash and removes
//! its authority until the new content is trusted.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

const TRUST_FILE: &str = "workflows-trust.toml";
const MAX_TRUST_BYTES: u64 = 64 * 1024;
const MAX_GRANTS: usize = 512;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    #[serde(default)]
    grant: Vec<Grant>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    source_path: String,
    hash: String,
}

/// Check whether these exact manifest bytes at this canonical source path are trusted.
///
/// # Errors
/// Returns an error for an invalid key or unsafe, oversized, or malformed trust storage.
pub fn is_trusted(home: &Path, source_path: &str, hash: &str) -> Result<bool, String> {
    validate_key(source_path, hash)?;
    Ok(read(home)?.grant.iter().any(|grant| grant.source_path == source_path && grant.hash == hash))
}

/// Trust this manifest hash, replacing any prior grant for the same source path.
///
/// # Errors
/// Returns an error for an invalid key or unsafe, full, or unwritable trust storage.
pub fn trust(home: &Path, source_path: &str, hash: &str) -> Result<(), String> {
    update(home, source_path, hash, true)
}

/// Revoke this exact source path and manifest hash.
///
/// # Errors
/// Returns an error for an invalid key or unsafe or unwritable trust storage.
pub fn untrust(home: &Path, source_path: &str, hash: &str) -> Result<(), String> {
    update(home, source_path, hash, false)
}

fn validate_key(source_path: &str, hash: &str) -> Result<(), String> {
    let path = Path::new(source_path);
    if !path.is_absolute()
        || source_path.len() > 4096
        || source_path.contains('\0')
        || source_path.ends_with('/')
        || source_path.split('/').skip(1).any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.components().any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err("workflow trust source must be a canonical absolute path".to_owned());
    }
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
        return Err("workflow trust hash must be a lowercase SHA-256 digest".to_owned());
    }
    Ok(())
}

fn read(home: &Path) -> Result<TrustFile, String> {
    if !check_home(home)? {
        return Ok(TrustFile::default());
    }
    let path = home.join(TRUST_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(TrustFile::default()),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
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
    let opened = file.metadata().map_err(|err| format!("{}: {err}", path.display()))?;
    if !opened.is_file() || opened.uid() != current_uid() || opened.permissions().mode() & 0o077 != 0 || opened.len() > MAX_TRUST_BYTES {
        return Err(format!("{} changed or is not an owned private regular file", path.display()));
    }
    let mut text = String::new();
    file.take(MAX_TRUST_BYTES + 1).read_to_string(&mut text).map_err(|err| format!("{}: {err}", path.display()))?;
    if text.len() as u64 > MAX_TRUST_BYTES {
        return Err(format!("{} exceeds trust file limit", path.display()));
    }
    let parsed: TrustFile = toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    if parsed.grant.len() > MAX_GRANTS {
        return Err(format!("{} exceeds trust grant limit", path.display()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for grant in &parsed.grant {
        validate_key(&grant.source_path, &grant.hash)?;
        if !seen.insert(grant.source_path.as_str()) {
            return Err(format!("{} has duplicate workflow source grants", path.display()));
        }
    }
    Ok(parsed)
}

fn update(home: &Path, source_path: &str, hash: &str, add: bool) -> Result<(), String> {
    validate_key(source_path, hash)?;
    prepare_home(home)?;
    let mut grants = read(home)?.grant;
    if add {
        grants.retain(|grant| grant.source_path != source_path);
        if grants.len() >= MAX_GRANTS {
            return Err("workflow trust grant limit reached".to_owned());
        }
        grants.push(Grant { source_path: source_path.to_owned(), hash: hash.to_owned() });
    } else {
        grants.retain(|grant| grant.source_path != source_path || grant.hash != hash);
    }
    grants.sort_by(|a, b| a.source_path.cmp(&b.source_path));
    let encoded = toml::to_string(&TrustFile { grant: grants }).map_err(|err| format!("serializing workflow trust: {err}"))?;
    if encoded.len() as u64 > MAX_TRUST_BYTES {
        return Err("workflow trust file limit reached".to_owned());
    }
    let mut temp = tempfile::NamedTempFile::new_in(home).map_err(|err| format!("{}: {err}", home.display()))?;
    temp.as_file().set_permissions(fs::Permissions::from_mode(0o600)).map_err(|err| format!("{}: {err}", home.display()))?;
    temp.write_all(encoded.as_bytes()).map_err(|err| format!("{}: {err}", home.display()))?;
    temp.as_file().sync_all().map_err(|err| format!("{}: {err}", home.display()))?;
    let path = home.join(TRUST_FILE);
    temp.persist(&path).map_err(|err| format!("{}: {}", path.display(), err.error))?;
    File::open(home).and_then(|directory| directory.sync_all()).map_err(|err| format!("{}: {err}", home.display()))?;
    Ok(())
}

fn prepare_home(home: &Path) -> Result<(), String> {
    if !check_home_exists(home)? {
        fs::create_dir(home).map_err(|err| format!("{}: {err}", home.display()))?;
    }
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).map_err(|err| format!("{}: {err}", home.display()))?;
    Ok(())
}

fn check_home(home: &Path) -> Result<bool, String> {
    if !check_home_exists(home)? {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(home).map_err(|err| format!("{}: {err}", home.display()))?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} must be a private directory", home.display()));
    }
    Ok(true)
}

fn check_home_exists(home: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(home) {
        Ok(metadata) if metadata.is_dir() && metadata.uid() == current_uid() => Ok(true),
        Ok(_) => Err(format!("{} must be an owned directory, not a symlink", home.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("{}: {err}", home.display())),
    }
}

fn current_uid() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "/tmp/workspace/.agents/workflows/build/workflow.toml";
    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn trust_is_private_hash_pinned_and_revocable() {
        let root = tempfile::tempdir().expect("temporary directory");
        let home = root.path().join("aim-home");
        trust(&home, SOURCE, HASH_A).expect("trust original");
        assert!(is_trusted(&home, SOURCE, HASH_A).expect("read grant"));
        assert!(!is_trusted(&home, SOURCE, HASH_B).expect("read changed hash"));
        trust(&home, SOURCE, HASH_B).expect("trust changed content");
        assert!(!is_trusted(&home, SOURCE, HASH_A).expect("old grant removed"));
        assert!(is_trusted(&home, SOURCE, HASH_B).expect("new grant present"));
        assert_eq!(fs::metadata(&home).expect("home metadata").permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(home.join(TRUST_FILE)).expect("file metadata").permissions().mode() & 0o777, 0o600);
        untrust(&home, SOURCE, HASH_B).expect("untrust");
        assert!(!is_trusted(&home, SOURCE, HASH_B).expect("grant removed"));
    }

    #[test]
    fn symlinks_and_malformed_grants_are_rejected() {
        let root = tempfile::tempdir().expect("temporary directory");
        let home = root.path().join("aim-home");
        let other = root.path().join("other");
        fs::create_dir(&other).expect("other directory");
        std::os::unix::fs::symlink(&other, &home).expect("home symlink");
        assert!(trust(&home, SOURCE, HASH_A).is_err());
        fs::remove_file(&home).expect("remove symlink");
        fs::create_dir(&home).expect("home directory");
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).expect("private home");
        std::os::unix::fs::symlink(other.join(TRUST_FILE), home.join(TRUST_FILE)).expect("file symlink");
        assert!(is_trusted(&home, SOURCE, HASH_A).is_err());
        assert!(trust(&home, SOURCE, HASH_A).is_err());
        fs::remove_file(home.join(TRUST_FILE)).expect("remove symlink");
        fs::write(home.join(TRUST_FILE), "[[grant]]\nsource_path = 'bad'\nhash = 'abc'\n").expect("malformed trust");
        fs::set_permissions(home.join(TRUST_FILE), fs::Permissions::from_mode(0o600)).expect("private trust");
        assert!(is_trusted(&home, SOURCE, HASH_A).is_err());
    }
}
