//! Bounded, read-only MCP server discovery. No server is started here.

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use serde_json::Value;

use crate::resources::files::{Files, LocalFiles, Read, sha256};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES_PER_FILE: usize = 64;
const MAX_FIELD_BYTES: usize = 16 * 1024;

/// Where an MCP server would run, independently of the file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    /// The selected workspace, including an SSH workspace.
    Workspace,
    /// The machine running aim.
    Local,
}

/// The source format of a server definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// aim's own MCP config.
    Native,
    /// Claude Code config.
    Claude,
    /// Codex config.
    Codex,
    /// Cursor config.
    Cursor,
}

/// Server connection details. Its `Debug` implementation deliberately hides all values.
#[derive(Clone, PartialEq, Eq)]
pub enum Transport {
    /// A subprocess launched in the selected location.
    Stdio {
        /// Executable path or name.
        command: String,
        /// Arguments passed verbatim to the executable.
        args: Vec<String>,
        /// Environment variables; values must never be shown in listings.
        env: BTreeMap<String, String>,
    },
    /// A streamable HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Request headers; values must never be shown in listings.
        headers: BTreeMap<String, String>,
    },
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio { .. } => f.write_str("Stdio { ... }"),
            Self::Http { .. } => f.write_str("Http { ... }"),
        }
    }
}

/// One discovered definition. Colliding names remain separate and retain provenance.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerEntry {
    /// Server name within its source.
    pub name: String,
    /// Absolute local path or workspace path, used as part of the trust key.
    pub source_path: String,
    /// SHA-256 of the complete named entry, including env and headers.
    pub hash: String,
    /// Execution location for stdio servers.
    pub location: Location,
    /// Protocol and connection details.
    pub transport: Transport,
    /// Whether this exact entry has an active grant.
    pub trusted: bool,
    /// Format of the source file.
    pub origin: Origin,
}

impl std::fmt::Debug for ServerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerEntry")
            .field("name", &self.name)
            .field("source_path", &self.source_path)
            .field("hash", &self.hash)
            .field("location", &self.location)
            .field("transport", &self.transport)
            .field("trusted", &self.trusted)
            .field("origin", &self.origin)
            .finish()
    }
}

struct SourceSpec<'a> {
    files: &'a dyn Files,
    path: &'static str,
    source_path: String,
    location: Location,
    origin: Origin,
    toml: bool,
}

/// Discover native and foreign definitions without launching or connecting to them.
/// Project reads always use `project`; this remains correct for SSH workspaces.
///
/// # Errors
/// Returns an error for unreadable, malformed or oversized source files and unsafe native files.
pub async fn discover(
    project: Option<&dyn Files>,
    user_home: &Path,
    aim_home: &Path,
    workspace_root: &str,
) -> Result<Vec<ServerEntry>, String> {
    let local = LocalFiles::new(user_home);
    let native_local = LocalFiles::new(aim_home);
    let mut specs = Vec::new();
    if let Some(files) = project {
        for (path, origin, toml) in [
            (".agents/mcp.json", Origin::Native, false),
            (".mcp.json", Origin::Claude, false),
            (".codex/config.toml", Origin::Codex, true),
            (".cursor/mcp.json", Origin::Cursor, false),
        ] {
            specs.push(SourceSpec {
                files,
                path,
                source_path: project_path(workspace_root, path),
                location: Location::Workspace,
                origin,
                toml,
            });
        }
    }
    specs.push(SourceSpec {
        files: &native_local,
        path: "mcp.json",
        source_path: aim_home.join("mcp.json").to_string_lossy().into_owned(),
        location: Location::Local,
        origin: Origin::Native,
        toml: false,
    });
    for (path, origin, toml) in
        [(".claude.json", Origin::Claude, false), (".codex/config.toml", Origin::Codex, true), (".cursor/mcp.json", Origin::Cursor, false)]
    {
        specs.push(SourceSpec {
            files: &local,
            path,
            source_path: user_home.join(path).to_string_lossy().into_owned(),
            location: Location::Local,
            origin,
            toml,
        });
    }
    let grants = super::trust::grants(aim_home)?;
    let mut entries = Vec::new();
    for spec in specs {
        entries.extend(read_source(&spec, aim_home, workspace_root, &grants).await?);
    }
    Ok(entries)
}

async fn read_source(
    spec: &SourceSpec<'_>,
    aim_home: &Path,
    workspace_root: &str,
    grants: &super::trust::Grants,
) -> Result<Vec<ServerEntry>, String> {
    if spec.origin == Origin::Native && spec.location == Location::Local {
        let path = aim_home.join(spec.path);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file()
                    && metadata.uid() == nix::unistd::Uid::current().as_raw()
                    && metadata.permissions().mode().trailing_zeros() >= 6 =>
            {
                let directory = std::fs::symlink_metadata(aim_home).map_err(|err| format!("{}: {err}", aim_home.display()))?;
                if !directory.is_dir()
                    || directory.uid() != nix::unistd::Uid::current().as_raw()
                    || directory.permissions().mode() & 0o077 != 0
                {
                    return Err(format!("{} must be an owned private directory", aim_home.display()));
                }
            }
            Ok(_) => return Err(format!("{} must be an owned private regular file", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
    }
    let Some(read) = spec.files.read_many(vec![spec.path.to_owned()], MAX_FILE_BYTES + 1).await.into_iter().next() else {
        return Err(format!("{}: missing read result", spec.source_path));
    };
    let text = match read {
        Read::Missing => return Ok(Vec::new()),
        Read::Failed(reason) => return Err(format!("{}: {reason}", spec.source_path)),
        Read::Ok(file) if file.truncated || file.size > MAX_FILE_BYTES => {
            return Err(format!("{} exceeds MCP config size limit", spec.source_path));
        }
        Read::Ok(file) => file.text,
    };
    let value = if spec.toml {
        let parsed: toml::Value = toml::from_str(&text).map_err(|err| format!("{}: {err}", spec.source_path))?;
        serde_json::to_value(parsed).map_err(|err| format!("{}: {err}", spec.source_path))?
    } else {
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", spec.source_path))?
    };
    parse_entries(spec, &value, workspace_root, grants)
}

fn parse_entries(
    spec: &SourceSpec<'_>,
    value: &Value,
    workspace_root: &str,
    grants: &super::trust::Grants,
) -> Result<Vec<ServerEntry>, String> {
    let key = if spec.origin == Origin::Codex { "mcp_servers" } else { "mcpServers" };
    let mut maps = Vec::new();
    if let Some(servers) = value.get(key) {
        maps.push(servers.as_object().ok_or_else(|| format!("{}: {key} must be a map", spec.source_path))?);
    }
    if spec.origin == Origin::Claude
        && spec.location == Location::Local
        && let Some(servers) = value.get("projects").and_then(|projects| projects.get(workspace_root)).and_then(|project| project.get(key))
    {
        maps.push(servers.as_object().ok_or_else(|| format!("{}: projects.{workspace_root}.{key} must be a map", spec.source_path))?);
    }
    if maps.iter().map(|map| map.len()).sum::<usize>() > MAX_ENTRIES_PER_FILE {
        return Err(format!("{} exceeds MCP entry limit", spec.source_path));
    }
    let mut entries = Vec::new();
    for (name, definition) in maps.into_iter().flat_map(|map| map.iter()) {
        if name.is_empty() || name.len() > 128 || name.contains('/') || name.contains('\0') {
            continue;
        }
        if definition.get("enabled").and_then(Value::as_bool) == Some(false)
            || definition.get("disabled").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(transport) = parse_transport(definition) else { continue };
        let canonical = serde_json::to_vec(&(name, definition)).map_err(|err| err.to_string())?;
        let hash = sha256(&canonical);
        let trusted =
            spec.origin == Origin::Native && spec.location == Location::Local || grants.contains(&spec.source_path, &hash, spec.location);
        entries.push(ServerEntry {
            name: name.clone(),
            source_path: spec.source_path.clone(),
            hash,
            location: spec.location,
            transport,
            trusted,
            origin: spec.origin,
        });
    }
    Ok(entries)
}

fn project_path(root: &str, path: &str) -> String {
    format!("{}/{path}", root.trim_end_matches('/'))
}

fn parse_transport(value: &Value) -> Option<Transport> {
    let explicit_type = value.get("type").and_then(Value::as_str);
    if let Some(url) = value.get("url").and_then(Value::as_str) {
        if !matches!(explicit_type, None | Some("http" | "streamable-http"))
            || !bounded(url)
            || !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return None;
        }
        let headers = string_map(value.get("headers").or_else(|| value.get("http_headers")))?;
        return Some(Transport::Http { url: url.to_owned(), headers });
    }
    if !matches!(explicit_type, None | Some("stdio")) {
        return None;
    }
    let command = value.get("command")?.as_str()?;
    if command.is_empty() || !bounded(command) {
        return None;
    }
    let args = match value.get("args") {
        None => Vec::new(),
        Some(args) => args.as_array()?.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?.into_iter().map(str::to_owned).collect(),
    };
    if args.len() > 128 || args.iter().any(|arg| !bounded(arg)) {
        return None;
    }
    let env = string_map(value.get("env"))?;
    Some(Transport::Stdio { command: command.to_owned(), args, env })
}

fn string_map(value: Option<&Value>) -> Option<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Some(BTreeMap::new());
    };
    let map = value.as_object()?;
    if map.len() > 128 {
        return None;
    }
    map.iter()
        .map(|(key, value)| {
            let value = value.as_str()?;
            (bounded(key) && bounded(value)).then(|| (key.clone(), value.to_owned()))
        })
        .collect()
}

fn bounded(value: &str) -> bool {
    value.len() <= MAX_FIELD_BYTES && !value.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::MemoryFiles;

    #[tokio::test]
    async fn remote_project_uses_files_and_grant_tracks_entry_hash() {
        let home = tempfile::tempdir().unwrap();
        let aim_home = home.path().join("custom-aim-home");
        let first =
            MemoryFiles::new([(".agents/mcp.json", r#"{"mcpServers":{"docs":{"command":"remote-server","env":{"SECRET":"hidden"}}}}"#)]);
        let entries = discover(Some(&first), home.path(), &aim_home, "/remote/project").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].location, Location::Workspace);
        assert_eq!(entries[0].source_path, "/remote/project/.agents/mcp.json");
        assert!(!entries[0].trusted);
        assert!(!format!("{:?}", entries[0]).contains("hidden"));
        super::super::trust::trust(&aim_home, &entries[0]).unwrap();
        let granted = discover(Some(&first), home.path(), &aim_home, "/remote/project").await.unwrap();
        assert!(granted[0].trusted);
        let changed =
            MemoryFiles::new([(".agents/mcp.json", r#"{"mcpServers":{"docs":{"command":"remote-server","env":{"SECRET":"changed"}}}}"#)]);
        assert!(!discover(Some(&changed), home.path(), &aim_home, "/remote/project").await.unwrap()[0].trusted);
        super::super::trust::untrust(&aim_home, &entries[0]).unwrap();
        assert!(!discover(Some(&first), home.path(), &aim_home, "/remote/project").await.unwrap()[0].trusted);
    }

    #[tokio::test]
    async fn native_user_and_foreign_formats_keep_separate_provenance() {
        let home = tempfile::tempdir().unwrap();
        let aim_home = home.path().join(".aim");
        std::fs::create_dir(&aim_home).unwrap();
        std::fs::set_permissions(&aim_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(home.path().join(".codex")).unwrap();
        std::fs::create_dir(home.path().join(".cursor")).unwrap();
        std::fs::write(home.path().join(".aim/mcp.json"), r#"{"mcpServers":{"native":{"command":"tool"}}}"#).unwrap();
        std::fs::set_permissions(home.path().join(".aim/mcp.json"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(home.path().join(".claude.json"), r#"{"mcpServers":{"claude":{"type":"http","url":"https://example.invalid/mcp"}},"projects":{"/project":{"mcpServers":{"scoped":{"command":"project-tool"}}}}}"#).unwrap();
        std::fs::write(home.path().join(".codex/config.toml"), "[mcp_servers.codex]\ncommand = 'tool'\nargs = ['--safe']\n").unwrap();
        std::fs::write(home.path().join(".cursor/mcp.json"), r#"{"mcpServers":{"cursor":{"command":"tool"}}}"#).unwrap();
        let entries = discover(None, home.path(), &aim_home, "/project").await.unwrap();
        assert_eq!(
            entries.iter().map(|e| (e.name.as_str(), e.origin, e.trusted)).collect::<Vec<_>>(),
            [
                ("native", Origin::Native, true),
                ("claude", Origin::Claude, false),
                ("scoped", Origin::Claude, false),
                ("codex", Origin::Codex, false),
                ("cursor", Origin::Cursor, false),
            ]
        );
        assert!(matches!(entries[1].transport, Transport::Http { .. }));
    }

    #[tokio::test]
    async fn oversize_file_and_unknown_transport_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let aim_home = home.path().join(".aim");
        let files = MemoryFiles::new([(".agents/mcp.json", "x".repeat(usize::try_from(MAX_FILE_BYTES).unwrap() + 1))]);
        assert!(discover(Some(&files), home.path(), &aim_home, "/project").await.is_err());
        let files = MemoryFiles::new([(
            ".agents/mcp.json",
            r#"{"mcpServers":{"unknown":{"type":"sse","url":"https://example.invalid"},"valid":{"command":"tool"}}}"#,
        )]);
        let entries = discover(Some(&files), home.path(), &aim_home, "/project").await.unwrap();
        assert_eq!(entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), ["valid"]);
    }
}
