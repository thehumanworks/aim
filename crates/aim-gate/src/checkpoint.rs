//! External, append-only anchors for the private gate ledger (ADR 0062).

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::formats::{LedgerRecord, ensure_private_home, read_ledger, verify_ledger};

const LEDGER_REF: &str = "refs/heads/gate/ledger";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    version: u32,
    seq: u64,
    hash: String,
}

/// Anchor the verified private ledger tip in a new commit on the private gate branch.
/// The previous remote commit is the new commit's parent. A lease prevents a concurrent
/// writer from replacing a different checkpoint, and the remote commit is fetched and
/// checked after the push. An already published identical checkpoint is idempotent.
///
/// # Errors
/// Invalid input, a changed local or remote chain, a stale ref, or failed readback.
pub fn checkpoint(origin_remote: &str, source_repo: &Path, ledger_root: &Path, seq: u64, hash: &str) -> Result<()> {
    valid_hash(hash)?;
    ensure_private_home(ledger_root)?;
    let records = read_ledger(ledger_root)?;
    let tip = records.last().context("cannot checkpoint an empty ledger")?;
    ensure!(tip.seq == seq && tip.hash == hash, "checkpoint does not match private ledger tip");
    let remote = remote_url(source_repo, origin_remote)?;

    let work = tempfile::Builder::new().prefix("aim-gate-checkpoint-").tempdir().context("create private checkpoint repository")?;
    fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700)).context("protect checkpoint repository")?;
    git(work.path(), &["init", "-q", "--initial-branch=checkpoint"], None, None)?;
    let previous = remote_head(work.path(), &remote, LEDGER_REF)?;
    if let Some(previous_sha) = previous.as_deref() {
        git(work.path(), &["fetch", "-q", "--no-tags", &remote, LEDGER_REF], None, Some(&remote))?;
        ensure!(git_text(work.path(), &["rev-parse", "FETCH_HEAD"], None, None)? == previous_sha, "checkpoint ref changed during fetch");
        let old = remote_anchor(work.path(), previous_sha, &records)?;
        ensure!(old.seq <= seq, "remote checkpoint is newer than private ledger");
        if old.seq == seq {
            ensure!(old.hash == hash, "remote checkpoint conflicts with private ledger");
            ensure!(
                remote_head(work.path(), &remote, LEDGER_REF)?.as_deref() == Some(previous_sha),
                "remote checkpoint changed on readback"
            );
            return Ok(());
        }
    }

    let anchor = Anchor { version: 1, seq, hash: hash.to_owned() };
    let mut anchor_bytes = serde_json::to_vec(&anchor).context("encode checkpoint")?;
    anchor_bytes.push(b'\n');
    let ledger_bytes = ledger_bytes(&records)?;
    fs::write(work.path().join("checkpoint.json"), anchor_bytes).context("stage checkpoint")?;
    fs::write(work.path().join("ledger.jsonl"), ledger_bytes).context("stage checkpoint ledger")?;
    git(work.path(), &["add", "--", "checkpoint.json", "ledger.jsonl"], None, None)?;
    let tree = git_text(work.path(), &["write-tree"], None, None)?;
    valid_git_oid(&tree)?;
    let mut commit_args = vec!["commit-tree", tree.as_str()];
    if let Some(previous_sha) = previous.as_deref() {
        commit_args.extend(["-p", previous_sha]);
    }
    let commit = git_text(work.path(), &commit_args, Some(b"Anchor gate ledger\n"), None)?;
    valid_git_oid(&commit)?;
    push_url_with_readback(work.path(), &remote, LEDGER_REF, &commit, previous.as_deref())?;

    // Check the object served by the remote, not only its advertised ref.
    git(work.path(), &["fetch", "-q", "--no-tags", &remote, LEDGER_REF], None, Some(&remote))?;
    ensure!(git_text(work.path(), &["rev-parse", "FETCH_HEAD"], None, None)? == commit, "remote checkpoint object changed on readback");
    let published = remote_anchor(work.path(), &commit, &records)?;
    ensure!(published.seq == seq && published.hash == hash, "remote checkpoint content mismatch");
    Ok(())
}

/// Compare-and-swap a private gate ref and read the advertised SHA back. This is also
/// used by trial promotion; it rejects every ref outside `refs/heads/gate/`.
///
/// # Errors
/// Unsafe remote/ref/SHA, a stale lease, push failure, or readback mismatch.
pub(crate) fn push_with_readback(
    repo: &Path,
    remote_name: &str,
    ref_name: &str,
    expected_sha: &str,
    previous_sha: Option<&str>,
) -> Result<()> {
    let remote = remote_url(repo, remote_name)?;
    valid_gate_ref(ref_name)?;
    valid_git_oid(expected_sha)?;
    let work = tempfile::Builder::new().prefix("aim-gate-push-").tempdir().context("create private push repository")?;
    git(work.path(), &["init", "-q"], None, None)?;
    let source = repo.to_str().context("source repository path is not UTF-8")?;
    git(work.path(), &["fetch", "-q", "--no-tags", source, expected_sha], None, None)?;
    ensure!(git_text(work.path(), &["rev-parse", "FETCH_HEAD"], None, None)? == expected_sha, "source commit changed during fetch");
    push_url_with_readback(work.path(), &remote, ref_name, expected_sha, previous_sha)
}

/// Read a trusted remote branch. Only the protected main baseline and gate refs
/// may be queried; this never updates any remote ref.
///
/// # Errors
/// Unsafe remote/ref or failed remote query.
pub(crate) fn remote_ref(repo: &Path, remote_name: &str, ref_name: &str) -> Result<Option<String>> {
    let remote = remote_url(repo, remote_name)?;
    let work = tempfile::Builder::new().prefix("aim-gate-ref-").tempdir().context("create private ref query repository")?;
    git(work.path(), &["init", "-q"], None, None)?;
    remote_head(work.path(), &remote, ref_name)
}

fn push_url_with_readback(repo: &Path, remote: &str, ref_name: &str, expected_sha: &str, previous_sha: Option<&str>) -> Result<()> {
    valid_gate_ref(ref_name)?;
    valid_git_oid(expected_sha)?;
    if let Some(previous_sha) = previous_sha {
        valid_git_oid(previous_sha)?;
    }
    ensure!(remote_head(repo, remote, ref_name)?.as_deref() == previous_sha, "gate ref changed before push");
    let lease = format!("--force-with-lease={ref_name}:{}", previous_sha.unwrap_or(""));
    let update = format!("+{expected_sha}:{ref_name}");
    git(repo, &["push", "--porcelain", &lease, remote, &update], None, Some(remote))?;
    ensure!(remote_head(repo, remote, ref_name)?.as_deref() == Some(expected_sha), "gate ref readback mismatch");
    Ok(())
}

fn remote_anchor(repo: &Path, commit: &str, local: &[LedgerRecord]) -> Result<Anchor> {
    valid_git_oid(commit)?;
    let anchor_bytes = git(repo, &["show", &format!("{commit}:checkpoint.json")], None, None)?;
    let anchor: Anchor = serde_json::from_slice(&anchor_bytes).context("decode remote checkpoint")?;
    ensure!(anchor.version == 1, "unsupported remote checkpoint version");
    valid_hash(&anchor.hash)?;
    let index = usize::try_from(anchor.seq).context("remote checkpoint sequence too large")?;
    ensure!(local.get(index).is_some_and(|record| record.hash == anchor.hash), "remote checkpoint diverges from private ledger");
    let remote_ledger = git(repo, &["show", &format!("{commit}:ledger.jsonl")], None, None)?;
    let remote_records = verify_ledger(&remote_ledger).context("remote checkpoint ledger is invalid")?;
    ensure!(remote_records.len() == index + 1, "remote checkpoint ledger length mismatch");
    let prefix = local.get(..=index).context("remote checkpoint sequence is absent locally")?;
    ensure!(remote_ledger == ledger_bytes(prefix)?, "remote checkpoint ledger diverges from private ledger");
    Ok(anchor)
}

fn ledger_bytes(records: &[LedgerRecord]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record).context("encode checkpoint ledger")?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn remote_url(repo: &Path, name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && name.len() <= 64 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid gate remote name"
    );
    ensure!(
        repo.is_absolute() && fs::canonicalize(repo).context("source repository missing")? == repo,
        "source repository must be canonical"
    );
    let key = format!("remote.{name}.url");
    let url = git_text(repo, &["config", "--local", "--get", &key], None, None)?;
    ensure!(!url.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | b'\0')), "unsafe gate remote URL");
    let github = matches!(
        url.as_str(),
        "https://github.com/thehumanworks/aim" | "https://github.com/thehumanworks/aim.git" | "git@github.com:thehumanworks/aim.git"
    );
    let local = cfg!(test) && Path::new(&url).is_absolute() && fs::canonicalize(&url).is_ok_and(|path| path == Path::new(&url));
    ensure!(github || local, "gate remote must be the private origin or a canonical local repository");
    Ok(url)
}

fn remote_head(repo: &Path, remote: &str, ref_name: &str) -> Result<Option<String>> {
    ensure!(ref_name == "refs/heads/main" || valid_gate_ref(ref_name).is_ok(), "invalid gate ref query");
    let output = git_text(repo, &["ls-remote", "--heads", remote, ref_name], None, Some(remote))?;
    if output.is_empty() {
        return Ok(None);
    }
    let (sha, found_ref) = output.split_once('\t').context("malformed remote ref response")?;
    ensure!(found_ref == ref_name && !sha.contains('\n'), "unexpected remote ref response");
    valid_git_oid(sha)?;
    Ok(Some(sha.to_owned()))
}

fn valid_gate_ref(value: &str) -> Result<()> {
    ensure!(value.starts_with("refs/heads/gate/") && value.len() > "refs/heads/gate/".len(), "only private gate refs may be pushed");
    ensure!(value.len() <= 128 && !value.contains("..") && !value.contains("//"), "invalid gate ref");
    ensure!(value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_')), "invalid gate ref");
    Ok(())
}

fn valid_hash(value: &str) -> Result<()> {
    ensure!(value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()), "invalid ledger hash");
    Ok(())
}

fn valid_git_oid(value: &str) -> Result<()> {
    ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid Git object ID"
    );
    Ok(())
}

fn git_text(repo: &Path, args: &[&str], input: Option<&[u8]>, remote: Option<&str>) -> Result<String> {
    String::from_utf8(git(repo, args, input, remote)?).context("Git returned non-UTF-8 data").map(|value| value.trim().to_owned())
}

fn git(repo: &Path, args: &[&str], input: Option<&[u8]>, remote: Option<&str>) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.current_dir(repo).env_clear();
    for key in ["PATH", "HOME", "TMPDIR", "SSH_AUTH_SOCK"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_AUTHOR_NAME", "aim-gate");
    command.env("GIT_AUTHOR_EMAIL", "aim-gate@localhost");
    command.env("GIT_COMMITTER_NAME", "aim-gate");
    command.env("GIT_COMMITTER_EMAIL", "aim-gate@localhost");
    command.args(["-c", "core.hooksPath=/dev/null", "-c", "protocol.ext.allow=never"]);
    let askpass = if remote.is_some_and(|url| url.starts_with("https://github.com/")) {
        match std::env::var_os("GITHUB_TOKEN") {
            Some(token) if !token.is_empty() => {
                let private = tempfile::Builder::new().prefix("aim-gate-auth-").tempdir().context("create private Git auth directory")?;
                let script = private.path().join("askpass");
                fs::write(&script, b"#!/bin/sh\ncase \"$1\" in\n  *Username*) printf '%s\\n' x-access-token ;;\n  *Password*) printf '%s\\n' \"$GITHUB_TOKEN\" ;;\n  *) exit 1 ;;\nesac\n").context("write Git askpass")?;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).context("protect Git askpass")?;
                command.env("GITHUB_TOKEN", token).env("GIT_ASKPASS", &script);
                Some(private)
            }
            _ => None,
        }
    } else {
        None
    };
    let _askpass = askpass;
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().context("start trusted Git")?;
    if let Some(bytes) = input {
        child.stdin.take().context("Git stdin missing")?.write_all(bytes).context("write Git stdin")?;
    }
    let output = child.wait_with_output().context("wait for trusted Git")?;
    if !output.status.success() {
        bail!("trusted Git operation failed");
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::formats::{LedgerEvent, append_ledger};

    use super::{LEDGER_REF, checkpoint, git, git_text, push_with_readback, remote_head, remote_ref};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let source = root.path().join("source");
        std::fs::create_dir(&remote).unwrap();
        std::fs::create_dir(&source).unwrap();
        let remote = std::fs::canonicalize(remote).unwrap();
        let source = std::fs::canonicalize(source).unwrap();
        let home = source.parent().unwrap().join("private");
        git(&remote, &["init", "-q", "--bare"], None, None).unwrap();
        git(&source, &["init", "-q"], None, None).unwrap();
        git(&source, &["config", "remote.origin.url", remote.to_str().unwrap()], None, None).unwrap();
        (root, source, remote, home)
    }

    fn event(home: &Path, name: &str) -> crate::formats::LedgerRecord {
        append_ledger(home, LedgerEvent::Canary { id: name.to_owned() }).unwrap()
    }

    #[test]
    fn publishes_immutable_parented_checkpoints_and_reads_back_contents() {
        let (_root, source, remote, home) = fixture();
        let first = event(&home, "first");
        checkpoint("origin", &source, &home, first.seq, &first.hash).unwrap();
        let old = remote_head(&source, remote.to_str().unwrap(), LEDGER_REF).unwrap().unwrap();
        checkpoint("origin", &source, &home, first.seq, &first.hash).unwrap();
        assert_eq!(remote_head(&source, remote.to_str().unwrap(), LEDGER_REF).unwrap().unwrap(), old);

        let second = event(&home, "second");
        checkpoint("origin", &source, &home, second.seq, &second.hash).unwrap();
        let new = remote_head(&source, remote.to_str().unwrap(), LEDGER_REF).unwrap().unwrap();
        assert_ne!(new, old);
        let parent = git_text(&source, &["--git-dir", remote.to_str().unwrap(), "rev-parse", &format!("{new}^1")], None, None).unwrap();
        assert_eq!(parent, old);
        assert!(checkpoint("origin", &source, &home, first.seq, &first.hash).is_err());
    }

    #[test]
    fn rejects_corrupt_remote_ledger_and_unsafe_inputs() {
        let (_root, source, remote, home) = fixture();
        let first = event(&home, "first");
        assert!(checkpoint("origin", &source, &home, first.seq, "bad").is_err());
        assert!(checkpoint("--upload-pack=evil", &source, &home, first.seq, &first.hash).is_err());
        checkpoint("origin", &source, &home, first.seq, &first.hash).unwrap();
        let next = event(&home, "next");
        git(&source, &["config", "remote.origin.url", "ext::sh -c evil"], None, None).unwrap();
        assert!(checkpoint("origin", &source, &home, next.seq, &next.hash).is_err());
        git(&source, &["config", "remote.origin.url", remote.to_str().unwrap()], None, None).unwrap();

        // Replace the remote ref with a commit that has no checkpoint files.
        std::fs::write(source.join("unrelated"), b"x").unwrap();
        git(&source, &["add", "unrelated"], None, None).unwrap();
        git(&source, &["commit", "-q", "-m", "unrelated"], None, None).unwrap();
        let unrelated = git_text(&source, &["rev-parse", "HEAD"], None, None).unwrap();
        git(&source, &["push", "-q", "--force", remote.to_str().unwrap(), &format!("{unrelated}:{LEDGER_REF}")], None, None).unwrap();
        assert!(checkpoint("origin", &source, &home, next.seq, &next.hash).is_err());
    }

    #[test]
    fn trial_helper_refuses_main_and_stale_lease() {
        let (_root, source, _remote, _home) = fixture();
        std::fs::write(source.join("candidate"), b"candidate").unwrap();
        git(&source, &["add", "candidate"], None, None).unwrap();
        git(&source, &["commit", "-q", "-m", "candidate"], None, None).unwrap();
        let sha = git_text(&source, &["rev-parse", "HEAD"], None, None).unwrap();
        assert!(push_with_readback(&source, "origin", "refs/heads/main", &sha, None).is_err());
        assert_eq!(remote_ref(&source, "origin", "refs/heads/main").unwrap(), None);
        assert!(push_with_readback(&source, "origin", "refs/heads/gate/trial", &sha, Some(&"a".repeat(40))).is_err());
        push_with_readback(&source, "origin", "refs/heads/gate/trial", &sha, None).unwrap();
        assert_eq!(remote_ref(&source, "origin", "refs/heads/gate/trial").unwrap(), Some(sha.clone()));
        assert!(push_with_readback(&source, "origin", "refs/heads/gate/trial", &sha, None).is_err());
    }
}
