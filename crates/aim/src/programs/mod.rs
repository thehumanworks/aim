//! Git-backed saved code-mode programs (ADR 0018).
//!
//! A program's source and manifest live in a Git repository, while trusted content hashes live
//! outside that repository. Pulling a changed program therefore cannot import its trust. Running
//! a program must use [`ProgramTools`], so both the recorded grant and the current host's tool
//! catalog constrain nested calls. The underlying host remains responsible for session policy,
//! dispatcher hooks, audit, and aimx workspace enforcement.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Which repository owns a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramScope {
    /// The user's `~/.aim/programs` repository.
    User,
    /// The workspace's `.agents/programs` repository.
    Project,
}

/// Supported source language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramLanguage {
    /// JavaScript source.
    JavaScript,
    /// TypeScript source, stripped before execution.
    TypeScript,
}

impl ProgramLanguage {
    const fn filename(self) -> &'static str {
        match self {
            Self::JavaScript => "main.js",
            Self::TypeScript => "main.ts",
        }
    }
}

/// The names granted when the program was saved. It can only narrow current session authority.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSnapshot {
    /// Tool names that may be called from the saved program.
    pub tools: BTreeSet<String>,
}

/// Where this program came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramProvenance {
    /// Session in which the program was saved.
    pub session_id: String,
    /// Turn number in that session.
    pub turn: u64,
}

/// The versioned, Git-committed `program.toml` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramManifest {
    /// Stable program identifier, supplied by the caller (normally a ULID).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// One-line purpose.
    pub description: String,
    /// Source language.
    pub language: ProgramLanguage,
    /// Version of the code runtime contract used at save time.
    pub runtime_version: String,
    /// JSON Schema for arguments to `main(args)`.
    #[serde(with = "json_schema_string")]
    pub params: Value,
    /// JSON Schema for the returned result.
    #[serde(with = "json_schema_string")]
    pub returns: Value,
    /// Statically discovered `tools.*` references.
    pub tools: BTreeSet<String>,
    /// Authority captured at save time.
    pub grants: GrantSnapshot,
    /// Origin of the saved program.
    pub provenance: ProgramProvenance,
    /// Program semantic version.
    pub version: String,
    /// Retrieval tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

mod json_schema_string {
    use serde::{Deserialize as _, Deserializer, Serializer};
    use serde_json::Value;

    pub(super) fn serialize<S: Serializer>(value: &Value, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(raw) => serde_json::from_str(&raw).map_err(serde::de::Error::custom),
            schema => Ok(schema),
        }
    }
}

/// A source file and manifest loaded from one repository.
#[derive(Clone, Debug)]
pub struct SavedProgram {
    /// Repository holding the program.
    pub scope: ProgramScope,
    /// Directory name and deferred-tool suffix.
    pub slug: String,
    /// Parsed manifest.
    pub manifest: ProgramManifest,
    /// Exact source bytes interpreted as UTF-8.
    pub source: String,
    /// README content, including its skill-compatible frontmatter.
    pub readme: String,
    /// SHA-256 of manifest, source, and README bytes, with lengths to separate fields.
    pub sha256: String,
    /// Whether this exact content hash was saved or explicitly trusted locally.
    pub trusted: bool,
}

impl SavedProgram {
    /// Saved grants intersected with declared references and currently offered tools. An
    /// untrusted program receives no tools; callers should refuse its execution entirely.
    #[must_use]
    pub fn effective_tools(&self, current: &BTreeSet<String>) -> BTreeSet<String> {
        if !self.trusted {
            return BTreeSet::new();
        }
        self.manifest.grants.tools.intersection(&self.manifest.tools).filter(|name| current.contains(*name)).cloned().collect()
    }

    /// Restricts nested calls to this program's effective named-tool grants.
    #[must_use]
    pub fn narrow_host(&self, current: Arc<dyn ToolHost>) -> ProgramTools {
        let names = current.specs().into_iter().map(|spec| spec.name).collect();
        ProgramTools { inner: current, allowed: self.effective_tools(&names) }
    }
}

/// A tool host that denies calls outside a saved program's effective grants, including names
/// not advertised to the model. The inner session host performs the final policy enforcement.
pub struct ProgramTools {
    inner: Arc<dyn ToolHost>,
    allowed: BTreeSet<String>,
}

impl ToolHost for ProgramTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs().into_iter().filter(|spec| self.allowed.contains(&spec.name)).collect()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        if !self.allowed.contains(&name) {
            return Box::pin(async move { Err(ProtoError::new(ErrorCode::Denied, format!("program may not call `{name}`"))) });
        }
        self.inner.call(name, arguments, key)
    }
}

/// Store or Git failure; messages omit command output and remote URLs to avoid credential leaks.
#[derive(Debug)]
pub enum ProgramError {
    /// Invalid input or repository content.
    Invalid(String),
    /// A program or repository was absent.
    Missing(String),
    /// File-system failure.
    Io(std::io::Error),
    /// TOML encoding or parsing failed.
    Toml(String),
    /// Git command failed; stderr is deliberately omitted.
    Git(&'static str),
    /// Remote history diverged and requires an explicit conflict resolution.
    Conflict,
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid program: {reason}"),
            Self::Missing(reason) => write!(f, "missing program: {reason}"),
            Self::Io(error) => write!(f, "program I/O: {error}"),
            Self::Toml(reason) => write!(f, "program TOML: {reason}"),
            Self::Git(operation) => write!(f, "program Git {operation} failed"),
            Self::Conflict => write!(f, "program Git histories diverged; resolve the remote branch explicitly"),
        }
    }
}

impl std::error::Error for ProgramError {}

impl From<std::io::Error> for ProgramError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Explicit GitHub remote configuration. Constructing it does not fetch or push.
#[derive(Clone, Debug)]
pub struct ConfiguredRemote {
    /// GitHub repository URL without credentials or query parameters.
    pub url: String,
    /// Remote branch name, such as `main`.
    pub branch: String,
}

/// Git-backed user and optional project program repositories.
pub struct ProgramStore {
    user_root: PathBuf,
    project_root: Option<PathBuf>,
    trust_path: PathBuf,
    writes: Mutex<()>,
}

impl ProgramStore {
    /// Creates a store rooted at `~/.aim/programs` and, for a project, `.agents/programs`.
    /// Paths are supplied rather than discovered so callers can bind the correct workspace.
    #[must_use]
    pub fn new(user_root: PathBuf, project_root: Option<PathBuf>) -> Self {
        let trust_path = user_root.with_file_name("program-trust.json");
        Self { user_root, project_root, trust_path, writes: Mutex::new(()) }
    }

    /// Saves a program as one Git commit in its scope. This never contacts a remote.
    /// The exact saved content is locally trusted.
    ///
    /// # Errors
    /// Returns [`ProgramError`] for invalid input, unsafe paths, file-system failures, or a
    /// failed local Git operation.
    pub fn save(&self, scope: ProgramScope, slug: &str, manifest: &ProgramManifest, source: &str) -> Result<SavedProgram, ProgramError> {
        let _guard = self.writes.lock().map_err(|_| ProgramError::Invalid("program store lock is poisoned".into()))?;
        validate_slug(slug)?;
        validate_manifest(manifest, source)?;
        let root = self.root(scope)?;
        ensure_repository(root)?;
        let directory = program_directory(root, slug)?;
        fs::create_dir_all(&directory)?;
        if directory.join(".git").symlink_metadata().is_ok() {
            return Err(ProgramError::Invalid("program directory must not be a nested Git repository".into()));
        }
        let manifest_text = toml::to_string_pretty(manifest).map_err(|_| ProgramError::Toml("could not encode manifest".into()))?;
        if manifest_text.len() > MAX_MANIFEST_BYTES {
            return Err(ProgramError::Invalid("manifest exceeds 256 KiB".into()));
        }
        let old_source = match manifest.language {
            ProgramLanguage::JavaScript => directory.join("main.ts"),
            ProgramLanguage::TypeScript => directory.join("main.js"),
        };
        for filename in ["program.toml", "main.js", "main.ts", "README.md"] {
            reject_symlink(&directory.join(filename))?;
        }
        if old_source.exists() {
            fs::remove_file(old_source)?;
        }
        fs::write(directory.join("program.toml"), &manifest_text)?;
        fs::write(directory.join(manifest.language.filename()), source)?;
        let readme = directory.join("README.md");
        if !readme.exists() {
            fs::write(
                &readme,
                format!(
                    "---\nname: {}\ndescription: {}\n---\n\n# {}\n",
                    json_yaml_string(slug),
                    json_yaml_string(&manifest.description),
                    manifest.name
                ),
            )?;
        }
        let readme =
            String::from_utf8(bounded_read(&readme, MAX_MANIFEST_BYTES)?).map_err(|error| ProgramError::Invalid(error.to_string()))?;
        git(root, &["add", "--", slug], "add")?;
        git_commit(root, slug)?;
        let sha256 = content_hash(&[manifest_text.as_bytes(), source.as_bytes(), readme.as_bytes()]);
        self.add_trust(&sha256)?;
        Ok(SavedProgram {
            scope,
            slug: slug.to_owned(),
            manifest: manifest.clone(),
            source: source.to_owned(),
            readme,
            sha256,
            trusted: true,
        })
    }

    /// Loads a program and checks whether its exact content hash is locally trusted.
    ///
    /// # Errors
    /// Returns [`ProgramError`] if the program is missing, malformed, or unreadable.
    pub fn load(&self, scope: ProgramScope, slug: &str) -> Result<SavedProgram, ProgramError> {
        let _guard = self.writes.lock().map_err(|_| ProgramError::Invalid("program store lock is poisoned".into()))?;
        self.load_unlocked(scope, slug)
    }

    fn load_unlocked(&self, scope: ProgramScope, slug: &str) -> Result<SavedProgram, ProgramError> {
        validate_slug(slug)?;
        let root = self.root(scope)?;
        validate_root(root)?;
        let directory = program_directory(root, slug)?;
        let manifest_bytes = bounded_read(&directory.join("program.toml"), MAX_MANIFEST_BYTES)?;
        let manifest_text = String::from_utf8(manifest_bytes.clone()).map_err(|error| ProgramError::Invalid(error.to_string()))?;
        let manifest: ProgramManifest = toml::from_str(&manifest_text).map_err(|_| ProgramError::Toml("invalid manifest".into()))?;
        let source_bytes = bounded_read(&directory.join(manifest.language.filename()), MAX_SOURCE_BYTES)?;
        let source = String::from_utf8(source_bytes.clone()).map_err(|error| ProgramError::Invalid(error.to_string()))?;
        let readme_bytes = bounded_read(&directory.join("README.md"), MAX_MANIFEST_BYTES)?;
        let readme = String::from_utf8(readme_bytes.clone()).map_err(|error| ProgramError::Invalid(error.to_string()))?;
        validate_manifest(&manifest, &source)?;
        let sha256 = content_hash(&[&manifest_bytes, &source_bytes, &readme_bytes]);
        let trusted = self.trusted_hashes()?.contains(&sha256);
        Ok(SavedProgram { scope, slug: slug.to_owned(), manifest, source, readme, sha256, trusted })
    }

    /// Lists all valid programs from user and project repositories, preserving their scope.
    ///
    /// # Errors
    /// Returns [`ProgramError`] if a repository or program cannot be read or parsed.
    pub fn list(&self) -> Result<Vec<SavedProgram>, ProgramError> {
        let _guard = self.writes.lock().map_err(|_| ProgramError::Invalid("program store lock is poisoned".into()))?;
        let mut programs = Vec::new();
        for scope in [ProgramScope::User, ProgramScope::Project] {
            let Ok(root) = self.root(scope) else { continue };
            if !root.exists() {
                continue;
            }
            validate_root(root)?;
            for entry in fs::read_dir(root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let slug = entry.file_name();
                let Some(slug) = slug.to_str() else { continue };
                if validate_slug(slug).is_ok() && entry.path().join("program.toml").is_file() {
                    programs.push(self.load_unlocked(scope, slug)?);
                }
            }
        }
        programs.sort_by(|a, b| (a.scope != ProgramScope::Project, &a.slug).cmp(&(b.scope != ProgramScope::Project, &b.slug)));
        Ok(programs)
    }

    /// Explicitly trusts the exact content currently at this scope and slug. Trust state is
    /// outside the Git repository and will not travel through sync.
    ///
    /// # Errors
    /// Returns [`ProgramError`] if the program cannot be loaded or the local trust registry
    /// cannot be updated.
    pub fn trust(&self, scope: ProgramScope, slug: &str) -> Result<SavedProgram, ProgramError> {
        let _guard = self.writes.lock().map_err(|_| ProgramError::Invalid("program store lock is poisoned".into()))?;
        let mut program = self.load_unlocked(scope, slug)?;
        self.add_trust(&program.sha256)?;
        program.trusted = true;
        Ok(program)
    }

    /// Fetches and fast-forwards, or pushes, the user repository against an explicitly
    /// configured GitHub remote. Diverged histories remain on separate refs for manual review.
    /// This is the only method that contacts a remote or pushes.
    ///
    /// # Errors
    /// Returns [`ProgramError`] for invalid configuration, Git failures, or diverged histories.
    pub fn sync_user(&self, remote: &ConfiguredRemote) -> Result<(), ProgramError> {
        let _guard = self.writes.lock().map_err(|_| ProgramError::Invalid("program store lock is poisoned".into()))?;
        validate_remote(remote)?;
        let root = &self.user_root;
        ensure_repository(root)?;
        git(root, &["check-ref-format", "--branch", &remote.branch], "branch validation")?;
        let url = remote.url.as_str();
        let existing = Command::new("git").arg("-C").arg(root).args(["remote", "get-url", "origin"]).output()?;
        if existing.status.success() {
            let configured = String::from_utf8(existing.stdout).map_err(|error| ProgramError::Invalid(error.to_string()))?;
            if configured.trim() != url {
                return Err(ProgramError::Invalid("origin differs from the configured GitHub remote".into()));
            }
        } else {
            git(root, &["remote", "add", "origin", url], "remote add")?;
        }
        let remote_heads = git_output(root, &["ls-remote", "--heads", "origin", &remote.branch], "remote query")?;
        let local_head = local_head(root)?;
        if remote_heads.is_empty() {
            if local_head.is_some() {
                git(root, &["push", "origin", &format!("HEAD:refs/heads/{}", remote.branch)], "push")?;
            }
            return Ok(());
        }
        git(root, &["fetch", "origin", &remote.branch], "fetch")?;
        let remote_ref = format!("origin/{}", remote.branch);
        let remote_head = git_output(root, &["rev-parse", &remote_ref], "rev-parse")?;
        let Some(local_head) = local_head else {
            git(root, &["checkout", "-B", &remote.branch, &remote_ref], "checkout")?;
            return Ok(());
        };
        if local_head == remote_head {
            return Ok(());
        }
        if git_status(root, &["merge-base", "--is-ancestor", "HEAD", &remote_ref])? {
            git(root, &["merge", "--ff-only", &remote_ref], "fast-forward")?;
            return Ok(());
        }
        if git_status(root, &["merge-base", "--is-ancestor", &remote_ref, "HEAD"])? {
            git(root, &["push", "origin", &format!("HEAD:refs/heads/{}", remote.branch)], "push")?;
            return Ok(());
        }
        Err(ProgramError::Conflict)
    }

    fn root(&self, scope: ProgramScope) -> Result<&Path, ProgramError> {
        match scope {
            ProgramScope::User => Ok(&self.user_root),
            ProgramScope::Project => self.project_root.as_deref().ok_or_else(|| ProgramError::Missing("project program repository".into())),
        }
    }

    fn trusted_hashes(&self) -> Result<BTreeSet<String>, ProgramError> {
        reject_symlink(&self.trust_path)?;
        if let Some(parent) = self.trust_path.parent() {
            reject_symlink(parent)?;
        }
        if !self.trust_path.exists() {
            return Ok(BTreeSet::new());
        }
        let bytes = bounded_read(&self.trust_path, MAX_MANIFEST_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|error| ProgramError::Invalid(format!("trust registry: {error}")))
    }

    fn add_trust(&self, sha256: &str) -> Result<(), ProgramError> {
        let mut hashes = self.trusted_hashes()?;
        if !hashes.insert(sha256.to_owned()) {
            return Ok(());
        }
        if let Some(parent) = self.trust_path.parent() {
            reject_symlink(parent)?;
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&hashes).map_err(|error| ProgramError::Invalid(error.to_string()))?;
        let temporary = self.trust_path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, &self.trust_path)?;
        Ok(())
    }
}

fn validate_slug(slug: &str) -> Result<(), ProgramError> {
    if slug.is_empty() || slug.len() > 64 || !slug.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err(ProgramError::Invalid("slug must use 1–64 ASCII letters, digits, '-' or '_'".into()));
    }
    Ok(())
}

fn validate_manifest(manifest: &ProgramManifest, source: &str) -> Result<(), ProgramError> {
    if manifest.id.is_empty()
        || manifest.name.trim().is_empty()
        || manifest.runtime_version.trim().is_empty()
        || manifest.version.trim().is_empty()
    {
        return Err(ProgramError::Invalid("id, name, runtime_version and version are required".into()));
    }
    if !manifest.params.is_object() || !manifest.returns.is_object() {
        return Err(ProgramError::Invalid("params and returns must be JSON Schema objects".into()));
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err(ProgramError::Invalid("source exceeds 1 MiB".into()));
    }
    if manifest.tools.iter().chain(&manifest.grants.tools).any(|tool| tool.is_empty() || tool.len() > 128) {
        return Err(ProgramError::Invalid("tool names must use 1–128 bytes".into()));
    }
    Ok(())
}

fn program_directory(root: &Path, slug: &str) -> Result<PathBuf, ProgramError> {
    let path = root.join(slug);
    if path.symlink_metadata().is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProgramError::Invalid("program directory is a symlink".into()));
    }
    Ok(path)
}

fn reject_symlink(path: &Path) -> Result<(), ProgramError> {
    if path.symlink_metadata().is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProgramError::Invalid("program file is a symlink".into()));
    }
    Ok(())
}

fn json_yaml_string(value: &str) -> String {
    // JSON strings are also valid YAML strings, and preserve newlines and punctuation safely.
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

fn bounded_read(path: &Path, max: usize) -> Result<Vec<u8>, ProgramError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProgramError::Missing(path.display().to_string())
        } else {
            ProgramError::Io(error)
        }
    })?;
    if !metadata.file_type().is_file() || metadata.len() > max as u64 {
        return Err(ProgramError::Invalid("program file is not regular or exceeds its size limit".into()));
    }
    Ok(fs::read(path)?)
}

fn content_hash(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

fn ensure_repository(root: &Path) -> Result<(), ProgramError> {
    validate_root(root)?;
    fs::create_dir_all(root)?;
    reject_symlink(&root.join(".git"))?;
    if root.join(".git").is_dir() {
        return Ok(());
    }
    if root.join(".git").exists() {
        return Err(ProgramError::Invalid("program repository has an unsupported .git entry".into()));
    }
    git(root, &["init", "--quiet"], "init")
}

fn validate_root(root: &Path) -> Result<(), ProgramError> {
    reject_symlink(root)?;
    if let Some(parent) = root.parent() {
        reject_symlink(parent)?;
    }
    Ok(())
}

fn git(root: &Path, args: &[&str], operation: &'static str) -> Result<(), ProgramError> {
    let output = git_command(root).args(args).output()?;
    if output.status.success() { Ok(()) } else { Err(ProgramError::Git(operation)) }
}

fn git_status(root: &Path, args: &[&str]) -> Result<bool, ProgramError> {
    let output = git_command(root).args(args).output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ProgramError::Git("merge-base")),
    }
}

fn git_output(root: &Path, args: &[&str], operation: &'static str) -> Result<String, ProgramError> {
    let output = git_command(root).args(args).output()?;
    if !output.status.success() {
        return Err(ProgramError::Git(operation));
    }
    String::from_utf8(output.stdout).map(|text| text.trim().to_owned()).map_err(|error| ProgramError::Invalid(error.to_string()))
}

fn local_head(root: &Path) -> Result<Option<String>, ProgramError> {
    let output = git_command(root).args(["rev-parse", "--verify", "HEAD"]).output()?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(|text| Some(text.trim().to_owned()))
            .map_err(|error| ProgramError::Invalid(error.to_string())),
        Some(128) => Ok(None),
        _ => Err(ProgramError::Git("rev-parse")),
    }
}

fn git_commit(root: &Path, slug: &str) -> Result<(), ProgramError> {
    let output = git_command(root)
        .args([
            "-c",
            "user.name=aim",
            "-c",
            "user.email=aim@noreply.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--only",
            "--allow-empty",
            "-m",
            "Save program",
            "--",
            slug,
        ])
        .output()?;
    if output.status.success() { Ok(()) } else { Err(ProgramError::Git("commit")) }
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.args(["-c", "core.hooksPath=/dev/null", "-C"]).arg(root);
    command
}

fn validate_remote(remote: &ConfiguredRemote) -> Result<(), ProgramError> {
    let url = remote.url.as_str();
    let path = url.strip_prefix("git@github.com:").or_else(|| url.strip_prefix("https://github.com/"));
    let Some(path) = path else {
        return Err(ProgramError::Invalid("remote must be a credential-free GitHub SSH or HTTPS URL".into()));
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let valid_component = |part: &str| {
        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.')
    };
    if !parts.next().is_some_and(valid_component) || !parts.next().is_some_and(valid_component) || parts.next().is_some() {
        return Err(ProgramError::Invalid("remote must identify one GitHub owner/repository".into()));
    }
    if remote.branch.is_empty()
        || !remote.branch.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'/')
    {
        return Err(ProgramError::Invalid("invalid Git branch".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
