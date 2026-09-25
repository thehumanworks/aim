//! Exact Git-tree staging and gate-owned trial pointer swaps (ADR 0062).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};

use crate::runner::validate_sha;

/// A tree staged under the gate's private deployment root.
#[derive(Clone, Debug)]
pub struct Artifact {
    /// Exact commit whose Git tree supplied the files.
    pub sha: String,
    /// Exact Git tree object id.
    pub tree: String,
    /// Directory containing the staged files.
    pub path: PathBuf,
    /// Pointer observed before staging, used as an activation compare-and-swap.
    pub expected_current: Option<String>,
}

fn checked_root(root: &Path) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "deployment root must be absolute");
    for component in root.components() {
        ensure!(!matches!(component, Component::CurDir | Component::ParentDir), "deployment root is not normalized");
    }
    ensure!(fs::symlink_metadata(root)?.is_dir(), "deployment root is not a directory");
    let canonical = fs::canonicalize(root).context("canonicalize deployment root")?;
    if let Some(home) = std::env::var_os("HOME") {
        let home = fs::canonicalize(home).context("canonicalize home for live-install guard")?;
        ensure!(!canonical.starts_with(home.join(".aim/bin")), "deployment root overlaps the live aim bin directory");
        if let Ok(live_bin) = fs::canonicalize(home.join(".aim/bin")) {
            ensure!(!canonical.starts_with(live_bin), "deployment root resolves inside the live aim bin directory");
        }
    }
    Ok(canonical)
}

fn git(source: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(source)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin")
        .env("HOME", "/var/empty")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("run trusted Git plumbing")?;
    ensure!(output.status.success(), "trusted Git plumbing failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(output.stdout)
}

fn git_text(source: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(source, args)?).context("Git returned non-UTF-8 object id")?.trim().to_owned())
}

fn lock(root: &Path) -> Result<File> {
    let path = root.join(".deploy.lock");
    let file = File::from(
        open(&path, OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW, Mode::from_raw_mode(0o600)).context("open deployment lock")?,
    );
    ensure!(file.metadata()?.is_file(), "deployment lock is not a regular file");
    flock(&file, FlockOperation::LockExclusive).context("lock deployment root")?;
    Ok(file)
}

fn no_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("deployment destination already exists: {}", path.display()),
        Err(error) => Err(error).with_context(|| format!("inspect deployment destination {}", path.display())),
    }
}

fn exact_source(source: &Path, sha: &str, tree: &str) -> Result<()> {
    ensure!(fs::symlink_metadata(source)?.is_dir(), "trusted source repository must be a real directory");
    let canonical = fs::canonicalize(source).context("canonicalize trusted source repository")?;
    let top = git_text(&canonical, &["rev-parse", "--show-toplevel"])?;
    ensure!(Path::new(&top) == canonical, "trusted source is not a repository root");
    ensure!(git_text(&canonical, &["cat-file", "-t", sha])? == "commit", "requested object is not a commit");
    ensure!(git_text(&canonical, &["rev-parse", &format!("{sha}^{{tree}}")])? == tree, "Git tree differs from expected tree");
    Ok(())
}

fn tree_entries(source: &Path, sha: &str) -> Result<Vec<(PathBuf, String, u32)>> {
    let output = git(source, &["ls-tree", "-r", "-z", "--full-tree", sha])?;
    let mut entries = Vec::new();
    for record in output.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
        let separator = record.iter().position(|byte| *byte == b'\t').context("malformed Git tree entry")?;
        let (header, with_separator) = record.split_at(separator);
        let path = with_separator.get(1..).context("malformed Git tree path")?;
        let mut fields = header.split(|byte| *byte == b' ');
        let mode_field = fields.next().context("missing Git tree mode")?;
        let kind = fields.next().context("missing Git tree kind")?;
        let oid_field = fields.next().context("missing Git tree blob id")?;
        ensure!(fields.next().is_none() && kind == b"blob", "Git tree contains a non-file entry");
        let mode = match mode_field {
            b"100644" => 0o644,
            b"100755" => 0o755,
            b"120000" => 0,
            _ => bail!("Git tree contains a gitlink or unsupported file mode"),
        };
        let oid = std::str::from_utf8(oid_field).context("non-UTF-8 blob id")?;
        validate_sha(oid)?;
        let relative = PathBuf::from(OsStr::from_bytes(path));
        ensure!(relative.components().all(|part| matches!(part, Component::Normal(_))), "Git tree path escapes staging root");
        ensure!(
            !relative.as_os_str().is_empty() && relative.components().next().is_some_and(|part| part.as_os_str() != OsStr::new(".git")),
            "Git tree path conflicts with gate metadata"
        );
        entries.push((relative, oid.to_owned(), mode));
    }
    Ok(entries)
}

fn validate_links(links: &HashMap<PathBuf, PathBuf>, files: &HashSet<PathBuf>, directories: &HashSet<PathBuf>) -> Result<()> {
    for (link, target) in links {
        let mut pending = VecDeque::new();
        if let Some(parent) = link.parent() {
            pending.extend(parent.components().map(|component| component.as_os_str().to_owned()));
        }
        pending.extend(target.components().map(|component| component.as_os_str().to_owned()));
        let mut resolved = PathBuf::new();
        let mut visited = HashSet::new();
        while let Some(component) = pending.pop_front() {
            if component == OsStr::new(".") {
                continue;
            }
            if component == OsStr::new("..") {
                ensure!(resolved.pop(), "symlink target escapes artifact");
                continue;
            }
            resolved.push(component);
            if let Some(next) = links.get(&resolved) {
                ensure!(visited.insert(resolved.clone()), "symlink cycle in artifact");
                resolved.pop();
                for part in next.components().map(|part| part.as_os_str().to_owned()).rev() {
                    pending.push_front(part);
                }
            } else if files.contains(&resolved) {
                ensure!(pending.is_empty(), "symlink target traverses through a file");
            } else if !pending.is_empty() {
                ensure!(directories.contains(&resolved), "symlink target traverses through a missing directory");
            }
        }
        ensure!(files.contains(&resolved) || directories.contains(&resolved), "symlink target is missing from artifact");
        ensure!(!directories.contains(&resolved) || !link.starts_with(&resolved), "symlink target contains its own link");
    }
    Ok(())
}

fn safe_parent(root: &Path, relative: &Path) -> Result<PathBuf> {
    let parent = relative.parent().context("staged path has no parent")?;
    let mut path = root.to_path_buf();
    for component in parent.components() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(), "staged parent is not a real directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path).context("create staged parent")?,
            Err(error) => return Err(error).context("inspect staged parent"),
        }
    }
    Ok(path)
}

/// Stage exactly the requested commit's raw Git blobs under `root/<sha>`.
///
/// `trusted_repo` is the gate's repository outside the candidate sandbox, never a candidate-
/// writable clone. Only raw objects at `sha` are staged; working-tree files are ignored.
/// Relative symlinks must resolve inside the staged tree; gitlinks are refused. The caller owns
/// `root`.
///
/// # Errors
/// Invalid root, SHA/tree, trusted repository, tree entry, or write failure.
pub fn stage_exact(root: &Path, trusted_repo: &Path, sha: &str, expected_tree: &str) -> Result<Artifact> {
    validate_sha(sha)?;
    validate_sha(expected_tree)?;
    let root = checked_root(root)?;
    let source = fs::canonicalize(trusted_repo).context("trusted source repository missing")?;
    ensure!(!root.starts_with(&source) && !source.starts_with(&root), "trusted source overlaps deployment root");
    exact_source(trusted_repo, sha, expected_tree)?;
    let entries = tree_entries(&source, sha)?;
    let mut links = HashMap::new();
    let mut files = HashSet::new();
    let mut directories = HashSet::from([PathBuf::new()]);
    for (relative, oid, mode) in &entries {
        for ancestor in relative.ancestors().skip(1) {
            directories.insert(ancestor.to_path_buf());
        }
        if *mode == 0 {
            let bytes = git(&source, &["cat-file", "blob", oid])?;
            ensure!(!bytes.is_empty() && !bytes.contains(&0), "invalid symlink target");
            let target = PathBuf::from(OsStr::from_bytes(&bytes));
            ensure!(!target.is_absolute(), "absolute symlink target escapes artifact");
            links.insert(relative.clone(), target);
        } else {
            files.insert(relative.clone());
        }
    }
    validate_links(&links, &files, &directories)?;
    let _guard = lock(&root)?;
    let expected_current = read_current(&root)?;
    let destination = root.join(sha);
    no_existing(&destination)?;
    let temporary = tempfile::Builder::new().prefix(".stage-").tempdir_in(&root).context("create staging directory")?;
    for (relative, oid, mode) in entries.iter().filter(|(_, _, mode)| *mode != 0) {
        let path = temporary.path().join(relative);
        safe_parent(temporary.path(), relative)?;
        let bytes = git(&source, &["cat-file", "blob", oid])?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path).context("create staged file")?;
        file.write_all(&bytes).context("write staged blob")?;
        file.set_permissions(fs::Permissions::from_mode(*mode)).context("set staged mode")?;
        file.sync_all().context("sync staged blob")?;
    }
    for (relative, _, _) in entries.iter().filter(|(_, _, mode)| *mode == 0) {
        safe_parent(temporary.path(), relative)?;
        let path = temporary.path().join(relative);
        no_existing(&path)?;
        let target = links.get(relative).context("validated symlink target missing")?;
        symlink(target, &path).context("create staged symlink")?;
    }
    let temporary_path = temporary.keep();
    if let Err(error) = fs::rename(&temporary_path, &destination) {
        let _cleanup = fs::remove_dir_all(&temporary_path);
        return Err(error).context("publish staged artifact");
    }
    File::open(&root)?.sync_all().context("sync deployment root")?;
    Ok(Artifact { sha: sha.to_owned(), tree: expected_tree.to_owned(), path: destination, expected_current })
}

/// Read the current trial SHA after validating the relative symlink and artifact directory.
///
/// # Errors
/// Unsafe root, malformed pointer, missing artifact, or I/O failure.
pub fn read_current(root: &Path) -> Result<Option<String>> {
    let root = checked_root(root)?;
    let pointer = root.join("current");
    let metadata = match fs::symlink_metadata(&pointer) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect current deployment pointer"),
    };
    ensure!(metadata.file_type().is_symlink(), "current deployment pointer is not a symlink");
    let target = fs::read_link(&pointer).context("read current deployment pointer")?;
    let sha = target.to_str().context("current pointer is not UTF-8")?;
    validate_sha(sha)?;
    ensure!(target.components().count() == 1, "current pointer escapes deployment root");
    ensure!(fs::symlink_metadata(root.join(sha))?.is_dir(), "current artifact is not a real directory");
    Ok(Some(sha.to_owned()))
}

fn swap_current(root: &Path, sha: &str) -> Result<()> {
    let temporary = tempfile::Builder::new().prefix(".current-").tempfile_in(root).context("reserve temporary pointer")?;
    let path = temporary.path().to_path_buf();
    drop(temporary);
    no_existing(&path)?;
    symlink(sha, &path).context("create temporary current pointer")?;
    if let Err(error) = fs::rename(&path, root.join("current")) {
        let _cleanup = fs::remove_file(&path);
        return Err(error).context("swap current deployment pointer");
    }
    File::open(root)?.sync_all().context("sync deployment pointer")
}

/// Atomically make a staged artifact current if the pointer still matches staging's observation.
/// Returns the retained predecessor SHA, if any.
///
/// # Errors
/// Stale current pointer, invalid artifact, or swap failure.
pub fn activate(root: &Path, artifact: &Artifact) -> Result<Option<String>> {
    let root = checked_root(root)?;
    validate_sha(&artifact.sha)?;
    validate_sha(&artifact.tree)?;
    ensure!(artifact.path == root.join(&artifact.sha), "artifact is outside deployment root");
    let _guard = lock(&root)?;
    let predecessor = read_current(&root)?;
    ensure!(predecessor == artifact.expected_current, "current deployment changed since staging");
    ensure!(fs::symlink_metadata(&artifact.path)?.is_dir(), "staged artifact is not a real directory");
    ensure!(predecessor.as_deref() != Some(&artifact.sha), "artifact is already current");
    swap_current(&root, &artifact.sha)?;
    ensure!(read_current(&root)?.as_deref() == Some(&artifact.sha), "deployment pointer readback mismatch");
    Ok(predecessor)
}

/// Restore a previously retained, real artifact directory by atomic pointer swap.
///
/// # Errors
/// Invalid predecessor, missing artifact, or pointer swap/readback failure.
pub fn rollback(root: &Path, predecessor: &str) -> Result<()> {
    validate_sha(predecessor)?;
    let root = checked_root(root)?;
    let _guard = lock(&root)?;
    ensure!(read_current(&root)?.is_some(), "no current artifact to roll back");
    ensure!(fs::symlink_metadata(root.join(predecessor))?.is_dir(), "predecessor artifact is not a real directory");
    swap_current(&root, predecessor)?;
    ensure!(read_current(&root)?.as_deref() == Some(predecessor), "rollback pointer readback mismatch");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run_git(path: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git").args(args).current_dir(path).output().context("run test Git")?;
        ensure!(output.status.success(), "test Git failed: {}", String::from_utf8_lossy(&output.stderr));
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        fs::create_dir(&repository)?;
        run_git(&repository, &["init", "-q"])?;
        let root = temporary.path().join("gate-deploy");
        fs::create_dir(&root)?;
        Ok((temporary, repository, root))
    }

    fn commit(repository: &Path, content: &str) -> Result<(String, String)> {
        fs::write(repository.join("app.txt"), content)?;
        run_git(repository, &["add", "--", "app.txt"])?;
        run_git(repository, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"])?;
        let sha = run_git(repository, &["rev-parse", "HEAD"])?;
        let tree = run_git(repository, &["rev-parse", "HEAD^{tree}"])?;
        Ok((sha, tree))
    }

    fn clone_at(repository: &Path, destination: &Path) -> Result<()> {
        let source = repository.to_str().context("test repository path is not UTF-8")?;
        let target = destination.to_str().context("test clone path is not UTF-8")?;
        run_git(repository, &["clone", "-q", "--", source, target])?;
        Ok(())
    }

    #[test]
    fn stages_exact_tree_and_restores_predecessor() -> Result<()> {
        let (temporary, repository, root) = fixture()?;
        let (first_sha, first_tree) = commit(&repository, "first")?;
        let first_clone = temporary.path().join("first-clone");
        clone_at(&repository, &first_clone)?;
        fs::write(first_clone.join("app.txt"), "dirty working tree")?;
        fs::write(first_clone.join("untracked.txt"), "not committed")?;
        let first = stage_exact(&root, &first_clone, &first_sha, &first_tree)?;
        assert_eq!(fs::read_to_string(first.path.join("app.txt"))?, "first");
        assert!(!first.path.join("untracked.txt").exists());
        assert_eq!(activate(&root, &first)?, None);
        assert_eq!(read_current(&root)?, Some(first_sha.clone()));

        let (second_sha, second_tree) = commit(&repository, "second")?;
        let second_clone = temporary.path().join("second-clone");
        clone_at(&repository, &second_clone)?;
        let second = stage_exact(&root, &second_clone, &second_sha, &second_tree)?;
        assert_eq!(activate(&root, &second)?, Some(first_sha.clone()));
        assert_eq!(read_current(&root)?, Some(second_sha));
        rollback(&root, &first_sha)?;
        assert_eq!(read_current(&root)?, Some(first_sha));
        assert_eq!(fs::read_to_string(first.path.join("app.txt"))?, "first");
        Ok(())
    }

    #[test]
    fn stages_non_head_commit_from_trusted_repository() -> Result<()> {
        let (_temporary, repository, root) = fixture()?;
        let (first_sha, first_tree) = commit(&repository, "first")?;
        let (_second_sha, _second_tree) = commit(&repository, "second")?;
        let staged = stage_exact(&root, &repository, &first_sha, &first_tree)?;
        assert_eq!(fs::read_to_string(staged.path.join("app.txt"))?, "first");
        Ok(())
    }

    #[test]
    fn rejects_stale_current_and_wrong_tree() -> Result<()> {
        let (temporary, repository, root) = fixture()?;
        let (first_sha, first_tree) = commit(&repository, "first")?;
        let first_clone = temporary.path().join("first-clone");
        clone_at(&repository, &first_clone)?;
        assert!(stage_exact(&root, &first_clone, &first_sha, &"0".repeat(40)).is_err());
        let first = stage_exact(&root, &first_clone, &first_sha, &first_tree)?;
        let (second_sha, second_tree) = commit(&repository, "second")?;
        let second_clone = temporary.path().join("second-clone");
        clone_at(&repository, &second_clone)?;
        let stale = stage_exact(&root, &second_clone, &second_sha, &second_tree)?;
        activate(&root, &first)?;
        assert!(activate(&root, &stale).is_err());
        assert_eq!(read_current(&root)?, Some(first_sha));
        Ok(())
    }

    #[test]
    fn stages_safe_relative_symlinks() -> Result<()> {
        let (temporary, repository, root) = fixture()?;
        fs::write(repository.join("AGENTS.md"), "instructions")?;
        fs::create_dir(repository.join("docs"))?;
        symlink("AGENTS.md", repository.join("CLAUDE.md"))?;
        symlink("../CLAUDE.md", repository.join("docs/GUIDE.md"))?;
        run_git(&repository, &["add", "-A"])?;
        run_git(&repository, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"])?;
        let sha = run_git(&repository, &["rev-parse", "HEAD"])?;
        let tree = run_git(&repository, &["rev-parse", "HEAD^{tree}"])?;
        let clone = temporary.path().join("clone");
        clone_at(&repository, &clone)?;
        let artifact = stage_exact(&root, &clone, &sha, &tree)?;
        assert_eq!(fs::read_link(artifact.path.join("CLAUDE.md"))?, Path::new("AGENTS.md"));
        assert_eq!(fs::read_link(artifact.path.join("docs/GUIDE.md"))?, Path::new("../CLAUDE.md"));
        assert_eq!(fs::read_to_string(artifact.path.join("docs/GUIDE.md"))?, "instructions");
        Ok(())
    }

    #[test]
    fn rejects_escaping_symlinks_and_pointer_escape() -> Result<()> {
        let (temporary, repository, root) = fixture()?;
        fs::write(repository.join("app.txt"), "safe")?;
        symlink("../outside", repository.join("escape"))?;
        run_git(&repository, &["add", "--", "app.txt", "escape"])?;
        run_git(&repository, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"])?;
        let sha = run_git(&repository, &["rev-parse", "HEAD"])?;
        let tree = run_git(&repository, &["rev-parse", "HEAD^{tree}"])?;
        let clone = temporary.path().join("clone");
        clone_at(&repository, &clone)?;
        assert!(stage_exact(&root, &clone, &sha, &tree).is_err());
        symlink("../outside", root.join("current"))?;
        assert!(read_current(&root).is_err());
        Ok(())
    }

    #[test]
    fn rejects_absolute_chain_and_cycle() -> Result<()> {
        for (target, chained) in [("/tmp/outside", false), ("../outside", true), ("cycle", false)] {
            let (temporary, repository, root) = fixture()?;
            fs::write(repository.join("app.txt"), "safe")?;
            if chained {
                symlink(target, repository.join("second"))?;
                symlink("second", repository.join("first"))?;
            } else {
                symlink(target, repository.join("first"))?;
            }
            if target == "cycle" {
                symlink("first", repository.join("cycle"))?;
            }
            run_git(&repository, &["add", "-A"])?;
            run_git(&repository, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"])?;
            let sha = run_git(&repository, &["rev-parse", "HEAD"])?;
            let tree = run_git(&repository, &["rev-parse", "HEAD^{tree}"])?;
            let clone = temporary.path().join("clone");
            clone_at(&repository, &clone)?;
            assert!(stage_exact(&root, &clone, &sha, &tree).is_err());
        }
        Ok(())
    }
}
