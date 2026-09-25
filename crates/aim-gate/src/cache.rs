//! One private, copy-on-write baseline target cache per base/evaluator pair.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};

use crate::formats::ensure_private_home;
use crate::runner::validate_sha;

const MAX_CACHE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 300_000;

fn cache_path(home: &Path, base_sha: &str, evaluator_digest: &str) -> Result<PathBuf> {
    ensure_private_home(home)?;
    validate_sha(base_sha)?;
    ensure!(
        evaluator_digest.len() == 64 && evaluator_digest.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid evaluator digest for baseline cache"
    );
    Ok(home.join("baselines").join(format!("{base_sha}-{evaluator_digest}")))
}

fn regular_tree(root: &Path) -> Result<()> {
    ensure!(fs::symlink_metadata(root)?.file_type().is_dir(), "baseline cache root is not a real directory");
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0_usize;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).context("read baseline cache tree")? {
            let entry = entry.context("read baseline cache entry")?;
            let metadata = fs::symlink_metadata(entry.path()).context("inspect baseline cache entry")?;
            entries = entries.saturating_add(1);
            ensure!(entries <= MAX_CACHE_ENTRIES, "baseline cache contains too many entries");
            if metadata.file_type().is_dir() {
                pending.push(entry.path());
            } else {
                ensure!(metadata.file_type().is_file(), "baseline cache contains a symlink or special file");
                bytes = bytes.saturating_add(metadata.len());
                ensure!(bytes <= MAX_CACHE_BYTES, "baseline cache exceeds disk limit");
            }
        }
    }
    Ok(())
}

fn clone_tree(source: &Path, destination: &Path) -> Result<()> {
    regular_tree(source)?;
    ensure!(fs::symlink_metadata(destination)?.file_type().is_dir(), "baseline target destination is not a real directory");
    ensure!(fs::read_dir(destination)?.next().is_none(), "baseline target destination is not empty");
    let status = Command::new("/bin/cp")
        .args(["-cR", "--"])
        .arg(source.join("."))
        .arg(destination)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .status()
        .context("clone baseline target with APFS copy-on-write")?;
    ensure!(status.success(), "baseline target clonefile copy failed");
    Ok(())
}

/// Restore a previously verified baseline target into a fresh, empty clone-local target.
/// Returns false on the first evaluation of this base/evaluator pair.
///
/// # Errors
/// Unsafe cache entry, changed destination, or failed copy-on-write clone.
pub fn restore(home: &Path, base_sha: &str, evaluator_digest: &str, target: &Path) -> Result<bool> {
    let cache = cache_path(home, base_sha, evaluator_digest)?;
    match fs::symlink_metadata(&cache) {
        Ok(_) => {
            clone_tree(&cache, target)?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).context("inspect baseline cache"),
    }
}

/// Persist one completed baseline target. The cache is immutable once published; candidates
/// receive their own clone-local targets and never write the private baseline copy.
///
/// # Errors
/// Unsafe source tree, private-home error, or failed atomic publish.
pub fn store(home: &Path, base_sha: &str, evaluator_digest: &str, target: &Path) -> Result<()> {
    let cache = cache_path(home, base_sha, evaluator_digest)?;
    if cache.exists() {
        regular_tree(&cache)?;
        return Ok(());
    }
    let parent = home.join("baselines");
    fs::create_dir_all(&parent).context("create private baseline cache root")?;
    fs::set_permissions(&parent, Permissions::from_mode(0o700)).context("protect baseline cache root")?;
    let temporary = tempfile::Builder::new().prefix(".baseline-").tempdir_in(&parent).context("create baseline cache staging directory")?;
    clone_tree(target, temporary.path())?;
    let path = temporary.keep();
    if let Err(err) = fs::rename(&path, &cache) {
        let _cleanup = fs::remove_dir_all(&path);
        return Err(err).context("publish baseline cache");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_one_private_baseline_without_mutating_it() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().join("gate");
        let source = temporary.path().join("source");
        let restored = temporary.path().join("restored");
        fs::create_dir(&source)?;
        fs::create_dir(&restored)?;
        fs::write(source.join("artifact"), b"baseline")?;
        let sha = "a".repeat(40);
        let evaluator = "b".repeat(64);
        assert!(!restore(&home, &sha, &evaluator, &restored)?);
        store(&home, &sha, &evaluator, &source)?;
        assert!(restore(&home, &sha, &evaluator, &restored)?);
        assert_eq!(fs::read(restored.join("artifact"))?, b"baseline");
        fs::write(restored.join("artifact"), b"candidate")?;
        assert_eq!(fs::read(cache_path(&home, &sha, &evaluator)?.join("artifact"))?, b"baseline");
        Ok(())
    }

    #[test]
    fn rejects_symlinks_in_cache_source() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().join("gate");
        let target = temporary.path().join("target");
        fs::create_dir(&target)?;
        std::os::unix::fs::symlink("/private/etc", target.join("escape"))?;
        assert!(store(&home, &"a".repeat(40), &"b".repeat(64), &target).is_err());
        Ok(())
    }
}
