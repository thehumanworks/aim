//! Project programs: plain files in the workspace's `.agents/programs/<slug>/` (ADR 0066,
//! `REV13a` L5).
//!
//! The session reads them through its workspace's [`Files`] and writes them with its workspace's
//! `Write` tool. So aimx's scope, protected paths and deduplication apply, a remote workspace
//! works like a local one, and an agent that may not write files cannot save a project program.
//! There is no nested Git repository: the project's own version control tracks the files.

use std::sync::Arc;

use aim_proto::ids::IdempotencyKey;
use serde_json::json;

use super::{MAX_MANIFEST_BYTES, MAX_SOURCE_BYTES, ProgramError, ProgramLanguage, validate_slug};
use crate::agent::ToolHost;
use crate::resources::files::{Files, Read};

fn text_of(result: &aim_proto::tool::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            aim_proto::tool::ToolContent::Text { text } => Some(text.as_str()),
            aim_proto::tool::ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where project programs live, relative to the workspace root.
pub const PROJECT_PROGRAMS: &str = ".agents/programs";

/// The workspace tool that writes a file (aimx's `Write`).
const WRITE_TOOL: &str = "Write";

/// Most project programs listed.
const LIST_LIMIT: u32 = 512;

/// A program's three files, as stored.
#[derive(Clone, Debug)]
pub struct ProgramFiles {
    /// `program.toml`.
    pub manifest: String,
    /// `main.js` or `main.ts`, as the manifest's language says.
    pub source: String,
    /// `README.md`.
    pub readme: String,
}

/// The project's programs, through the session's workspace.
pub struct ProjectPrograms {
    files: Arc<dyn Files>,
    tools: Arc<dyn ToolHost>,
}

impl ProjectPrograms {
    /// Reads with `files` (the project) and writes with `tools` (the session's workspace tools).
    #[must_use]
    pub fn new(files: Arc<dyn Files>, tools: Arc<dyn ToolHost>) -> Self {
        Self { files, tools }
    }

    fn path(slug: &str, file: &str) -> String {
        format!("{PROJECT_PROGRAMS}/{slug}/{file}")
    }

    async fn read(&self, path: String, max: usize) -> Result<Option<String>, ProgramError> {
        let max = u64::try_from(max).unwrap_or(u64::MAX);
        match self.files.read_many(vec![path.clone()], max.saturating_add(1)).await.into_iter().next() {
            Some(Read::Ok(file)) if file.truncated || file.size > max => {
                Err(ProgramError::Invalid(format!("{path} exceeds its size limit")))
            }
            Some(Read::Ok(file)) => Ok(Some(file.text)),
            Some(Read::Missing) => Ok(None),
            Some(Read::Failed(reason)) => Err(ProgramError::Invalid(format!("{path}: {reason}"))),
            None => Err(ProgramError::Invalid(format!("{path} could not be read"))),
        }
    }

    /// The manifest of `slug`, if the program exists.
    ///
    /// # Errors
    /// An invalid slug, or a manifest that cannot be read.
    pub async fn manifest(&self, slug: &str) -> Result<Option<String>, ProgramError> {
        validate_slug(slug)?;
        self.read(Self::path(slug, "program.toml"), MAX_MANIFEST_BYTES).await
    }

    /// Reads the program `slug`, whose manifest says it is written in `language`.
    ///
    /// # Errors
    /// A missing or unreadable file.
    pub async fn load(&self, slug: &str, manifest: String, language: ProgramLanguage) -> Result<ProgramFiles, ProgramError> {
        validate_slug(slug)?;
        let source = self.read(Self::path(slug, language.filename()), MAX_SOURCE_BYTES).await?;
        let readme = self.read(Self::path(slug, "README.md"), MAX_MANIFEST_BYTES).await?;
        match (source, readme) {
            (Some(source), Some(readme)) => Ok(ProgramFiles { manifest, source, readme }),
            _ => Err(ProgramError::Missing(format!("{PROJECT_PROGRAMS}/{slug}"))),
        }
    }

    /// Writes the program's manifest and source, and its README when it has none, through the
    /// workspace. Returns what is now stored.
    ///
    /// # Errors
    /// The workspace refused a write (for example, the session may not use `Write`).
    pub async fn save(&self, slug: &str, files: ProgramFiles, language: ProgramLanguage) -> Result<ProgramFiles, ProgramError> {
        validate_slug(slug)?;
        let readme = if let Some(existing) = self.read(Self::path(slug, "README.md"), MAX_MANIFEST_BYTES).await? {
            existing
        } else {
            self.write(Self::path(slug, "README.md"), &files.readme).await?;
            files.readme
        };
        self.write(Self::path(slug, language.filename()), &files.source).await?;
        // The manifest goes last: a program is visible only once its source is in place.
        self.write(Self::path(slug, "program.toml"), &files.manifest).await?;
        Ok(ProgramFiles { manifest: files.manifest, source: files.source, readme })
    }

    async fn write(&self, path: String, content: &str) -> Result<(), ProgramError> {
        if !self.tools.specs().iter().any(|spec| spec.name == WRITE_TOOL) {
            return Err(ProgramError::Denied("saving a project program needs the workspace's Write tool, which this session lacks".into()));
        }
        let key = IdempotencyKey::new(uuid::Uuid::now_v7().simple().to_string());
        match self.tools.call(WRITE_TOOL.to_owned(), json!({"file_path": path, "content": content}), key).await {
            Ok(result) if !result.is_error => Ok(()),
            Ok(result) => Err(ProgramError::Denied(format!("the workspace refused to write {path}: {}", text_of(&result)))),
            Err(error) => Err(ProgramError::Denied(format!("the workspace refused to write {path}: {}", error.message))),
        }
    }

    /// The slugs of the project's programs.
    ///
    /// # Errors
    /// The programs directory cannot be listed.
    pub async fn slugs(&self) -> Result<Vec<String>, ProgramError> {
        let listed = self.files.list(PROJECT_PROGRAMS, LIST_LIMIT).await.map_err(ProgramError::Invalid)?;
        let mut slugs: Vec<String> = listed
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.kind == aim_proto::harness::EntryKind::Dir)
            .map(|entry| entry.name)
            .filter(|name| validate_slug(name).is_ok())
            .collect();
        slugs.sort();
        Ok(slugs)
    }
}
