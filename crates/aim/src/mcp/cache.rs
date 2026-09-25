//! Private, bounded last-known MCP tool catalogs. The source grant is checked before a read.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use aim_proto::tool::ToolSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::config::{Location, ServerEntry};

const MAX_CATALOG_BYTES: u64 = 256 * 1024;
const MAX_TOOLS: usize = 64;
const MAX_CATALOGS: usize = 128;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    source_path: String,
    entry_hash: String,
    location: String,
    tools: Vec<ToolSpec>,
}

/// Cache under one user's private aim home. Errors make a cache entry absent, never trusted.
pub(crate) struct CatalogCache {
    aim_home: PathBuf,
}

impl CatalogCache {
    pub(crate) fn new(aim_home: PathBuf) -> Self {
        Self { aim_home }
    }

    pub(crate) fn load(&self, entry: &ServerEntry) -> Vec<ToolSpec> {
        if !entry.trusted || self.prepare(false).is_err() {
            return Vec::new();
        }
        let path = self.path(entry);
        let Ok(file) = OpenOptions::new().read(true).custom_flags(nix::libc::O_NOFOLLOW).open(path) else {
            return Vec::new();
        };
        let Ok(metadata) = file.metadata() else { return Vec::new() };
        if !metadata.is_file()
            || metadata.uid() != current_uid()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > MAX_CATALOG_BYTES
        {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.take(MAX_CATALOG_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Vec::new();
        }
        let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes) else { return Vec::new() };
        if snapshot.source_path != entry.source_path
            || snapshot.entry_hash != entry.hash
            || snapshot.location != location(entry.location)
            || !valid_tools(entry, &snapshot.tools)
        {
            return Vec::new();
        }
        snapshot.tools
    }

    pub(crate) fn save(&self, entry: &ServerEntry, tools: &[ToolSpec]) {
        if !entry.trusted || !valid_tools(entry, tools) || self.prepare(true).is_err() {
            return;
        }
        let snapshot = Snapshot {
            source_path: entry.source_path.clone(),
            entry_hash: entry.hash.clone(),
            location: location(entry.location).to_owned(),
            tools: tools.to_vec(),
        };
        let Ok(bytes) = serde_json::to_vec(&snapshot) else { return };
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return;
        }
        let directory = self.directory();
        let Ok(mut temp) = tempfile::NamedTempFile::new_in(&directory) else { return };
        if temp.as_file().set_permissions(fs::Permissions::from_mode(0o600)).is_err()
            || temp.write_all(&bytes).is_err()
            || temp.as_file().sync_all().is_err()
            || temp.persist(self.path(entry)).is_err()
        {
            return;
        }
        let _sync = File::open(&directory).and_then(|file| file.sync_all());
        prune(&directory);
    }

    fn directory(&self) -> PathBuf {
        self.aim_home.join("cache/mcp")
    }

    fn path(&self, entry: &ServerEntry) -> PathBuf {
        let mut digest = Sha256::new();
        digest.update(entry.source_path.as_bytes());
        digest.update([0]);
        digest.update(entry.hash.as_bytes());
        digest.update([0]);
        digest.update(location(entry.location).as_bytes());
        let filename = format!("{:x}", digest.finalize());
        self.directory().join(format!("{filename}.json"))
    }

    fn prepare(&self, create: bool) -> std::io::Result<()> {
        private_dir(&self.aim_home, create)?;
        private_dir(&self.aim_home.join("cache"), create)?;
        private_dir(&self.directory(), create)
    }
}

fn location(value: Location) -> &'static str {
    match value {
        Location::Workspace => "workspace",
        Location::Local => "local",
    }
}

fn valid_tools(entry: &ServerEntry, tools: &[ToolSpec]) -> bool {
    let prefix = format!("mcp__{}__", entry.name);
    tools.len() <= MAX_TOOLS && tools.iter().all(|tool| tool.name.starts_with(&prefix) && tool.name.len() <= 128)
}

fn current_uid() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

fn private_dir(path: &Path, create: bool) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && create => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
            fs::symlink_metadata(path)?
        }
        Err(err) => return Err(err),
    };
    if !metadata.is_dir() || metadata.uid() != current_uid() {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "unsafe MCP cache directory"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        if !create {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "shared MCP cache directory"));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn prune(directory: &Path) {
    let Ok(iter) = fs::read_dir(directory) else { return };
    let mut entries = iter
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            if !name.to_string_lossy().ends_with(".json") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| (metadata.modified().ok(), entry.path()))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.0);
    let excess = entries.len().saturating_sub(MAX_CATALOGS);
    for (_, path) in entries.into_iter().take(excess) {
        let _removed = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation};

    use super::*;
    use crate::mcp::config::{Origin, Transport};

    fn entry(hash: &str) -> ServerEntry {
        ServerEntry {
            name: "fixture".to_owned(),
            source_path: "/workspace/.agents/mcp.json".to_owned(),
            hash: hash.to_owned(),
            location: Location::Workspace,
            transport: Transport::Stdio { command: "fixture".to_owned(), args: Vec::new(), env: BTreeMap::new() },
            trusted: true,
            origin: Origin::Native,
        }
    }

    #[test]
    fn cached_catalog_is_private_and_bound_to_grant() {
        let home = tempfile::tempdir().unwrap();
        let aim_home = home.path().join("aim");
        let cache = CatalogCache::new(aim_home.clone());
        let tool = ToolSpec {
            name: "mcp__fixture__echo".to_owned(),
            description: "echo".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
            input: ToolInput::Json,
            annotations: ToolAnnotations { location: ToolLocation::Workspace, ..ToolAnnotations::default() },
        };
        cache.save(&entry("sha256:first"), std::slice::from_ref(&tool));
        assert_eq!(cache.load(&entry("sha256:first")), [tool]);
        assert!(cache.load(&entry("sha256:changed")).is_empty());
        let mut untrusted = entry("sha256:first");
        untrusted.trusted = false;
        assert!(cache.load(&untrusted).is_empty());
        let metadata = fs::metadata(cache.path(&entry("sha256:first"))).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(cache.directory()).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
