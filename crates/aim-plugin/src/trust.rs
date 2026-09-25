//! Hash-pinned plugin grants. This file contains capabilities, never credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::PluginError;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct TrustFile {
    #[serde(default)]
    plugins: BTreeMap<String, BTreeSet<String>>,
}

/// Capability grants keyed by the exact SHA-256 plugin identity (manifest and component bytes).
#[derive(Clone, Debug)]
pub struct TrustStore {
    path: PathBuf,
    data: TrustFile,
}

impl TrustStore {
    /// Directory containing the trust file and plugin durable state.
    #[must_use]
    pub fn home(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }
    /// Loads `<home>/trust.toml`, treating a missing file as empty trust.
    ///
    /// # Errors
    /// Returns an error if the trust file cannot be read or parsed.
    pub fn load(home: &Path) -> Result<Self, PluginError> {
        let path = home.join("trust.toml");
        let data = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| PluginError::Invalid(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TrustFile::default(),
            Err(e) => return Err(PluginError::Io(e)),
        };
        Ok(Self { path, data })
    }

    /// Returns the grants for exactly this plugin hash.
    #[must_use]
    pub fn grants(&self, hash: &str) -> Option<&BTreeSet<String>> {
        self.data.plugins.get(hash)
    }

    /// Adds or replaces an exact-hash grant. Edited manifests or components inherit nothing.
    ///
    /// # Errors
    /// Returns an error for an invalid digest or a failed persistence write.
    pub fn grant(&mut self, hash: &str, capabilities: impl IntoIterator<Item = String>) -> Result<(), PluginError> {
        validate_hash(hash)?;
        let grants = capabilities.into_iter().collect();
        self.transaction(|data| {
            data.plugins.insert(hash.to_owned(), grants);
        })
    }

    /// Revokes every grant for one plugin hash.
    ///
    /// # Errors
    /// Returns an error for an invalid digest or a failed persistence write.
    pub fn untrust(&mut self, hash: &str) -> Result<(), PluginError> {
        validate_hash(hash)?;
        self.transaction(|data| {
            data.plugins.remove(hash);
        })
    }

    fn transaction(&mut self, mutate: impl FnOnce(&mut TrustFile)) -> Result<(), PluginError> {
        let parent = self.path.parent().ok_or_else(|| PluginError::Invalid("trust path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let lock_path = self.path.with_extension("lock");
        #[cfg(unix)]
        let lock = {
            use std::os::unix::fs::OpenOptionsExt as _;
            fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(lock_path)?
        };
        #[cfg(not(unix))]
        let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock_path)?;
        lock.lock()?;
        let mut latest = Self::load(parent)?.data;
        mutate(&mut latest);
        Self::write_snapshot(&self.path, &latest)?;
        self.data = latest;
        Ok(())
    }

    fn write_snapshot(path: &Path, data: &TrustFile) -> Result<(), PluginError> {
        let text = toml::to_string_pretty(data).map_err(|e| PluginError::Invalid(e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        fs::write(&tmp, text)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

fn validate_hash(hash: &str) -> Result<(), PluginError> {
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(PluginError::Invalid("plugin hash must be a SHA-256 hex digest".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edited_component_has_no_inherited_grants() {
        let dir = tempfile::tempdir().unwrap();
        let mut trust = TrustStore::load(dir.path()).unwrap();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        trust.grant(&first, ["kv".into()]).unwrap();
        let reloaded = TrustStore::load(dir.path()).unwrap();
        assert!(reloaded.grants(&first).unwrap().contains("kv"));
        assert!(reloaded.grants(&second).is_none());
    }

    #[test]
    fn stale_writers_cannot_restore_revoked_grants_or_drop_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let mut first = TrustStore::load(dir.path()).unwrap();
        let mut stale = TrustStore::load(dir.path()).unwrap();
        first.grant(&a, ["kv".into()]).unwrap();
        stale.grant(&b, ["tools.provide".into()]).unwrap();
        let mut revoker = TrustStore::load(dir.path()).unwrap();
        let mut stale_again = TrustStore::load(dir.path()).unwrap();
        revoker.untrust(&a).unwrap();
        stale_again.grant(&c, ["session.read".into()]).unwrap();
        let reloaded = TrustStore::load(dir.path()).unwrap();
        assert!(reloaded.grants(&a).is_none());
        assert!(reloaded.grants(&b).is_some() && reloaded.grants(&c).is_some());
    }
}
