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

/// Capability grants keyed by the exact SHA-256 hash of component bytes.
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

    /// Returns the grants for exactly this component hash.
    #[must_use]
    pub fn grants(&self, hash: &str) -> Option<&BTreeSet<String>> {
        self.data.plugins.get(hash)
    }

    /// Adds or replaces an exact-hash grant. A new component hash has no inherited trust.
    ///
    /// # Errors
    /// Returns an error for an invalid digest or a failed persistence write.
    pub fn grant(&mut self, hash: &str, capabilities: impl IntoIterator<Item = String>) -> Result<(), PluginError> {
        validate_hash(hash)?;
        self.data.plugins.insert(hash.to_owned(), capabilities.into_iter().collect());
        self.save()
    }

    /// Revokes every grant for one component hash.
    ///
    /// # Errors
    /// Returns an error for an invalid digest or a failed persistence write.
    pub fn untrust(&mut self, hash: &str) -> Result<(), PluginError> {
        validate_hash(hash)?;
        self.data.plugins.remove(hash);
        self.save()
    }

    /// Persists the store with an atomic replacement.
    ///
    /// # Errors
    /// Returns an error if serialization or the atomic replacement fails.
    pub fn save(&self) -> Result<(), PluginError> {
        let parent = self.path.parent().ok_or_else(|| PluginError::Invalid("trust path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let text = toml::to_string_pretty(&self.data).map_err(|e| PluginError::Invalid(e.to_string()))?;
        let tmp = self.path.with_extension("toml.tmp");
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
        fs::rename(tmp, &self.path)?;
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
}
