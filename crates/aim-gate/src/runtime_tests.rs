//! Small repository and live Seatbelt integration checks for the gate runtime.

use std::error::Error;
use std::fs;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::formats::{
    BenchResults, FORMAT_VERSION, LedgerEvent, ReceiptBody, SignedReceipt, append_ledger, digest, read_config, sign_receipt, write_json_new,
};
use crate::promotion;
use crate::runtime::{GateRuntime, InitOptions};
use crate::sandbox::{Network, Policy};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git").arg("-C").arg(root).args(args).output()?;
    if !output.status.success() {
        return Err(format!("git command failed: {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn commit(root: &Path, message: &str) -> Result<String, Box<dyn Error>> {
    drop(git(root, &["add", "."])?);
    drop(git(root, &["-c", "user.name=Gate Fixture", "-c", "user.email=gate-fixture@noreply.invalid", "commit", "-m", message])?);
    git(root, &["rev-parse", "HEAD"])
}

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    home: PathBuf,
    gate: GateRuntime,
    base: String,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let remote_repo = temp.path().join("remote.git");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo)?;
        let output = Command::new("git").args(["init", "--bare"]).arg(&remote_repo).output()?;
        if !output.status.success() {
            return Err("fixture bare git init failed".into());
        }
        drop(git(&repo, &["init", "-b", "main"])?);
        fs::create_dir(repo.join("bench"))?;
        for name in ["run.py", "proxy.py", "manifest.toml"] {
            fs::write(repo.join("bench").join(name), b"# fixture\n")?;
        }
        fs::write(repo.join("mise.toml"), b"# fixture\n")?;
        fs::write(repo.join("mise.lock"), b"# fixture\n")?;
        fs::write(repo.join("README.md"), b"baseline\n")?;
        let base = commit(&repo, "Initial fixture")?;
        let remote_repo = fs::canonicalize(remote_repo)?;
        let remote_text = remote_repo.to_str().ok_or("non-UTF8 fixture path")?;
        drop(git(&repo, &["remote", "add", "origin", remote_text])?);
        drop(git(&repo, &["push", "-u", "origin", "main"])?);
        let home = temp.path().join("gate-home");
        let gate = GateRuntime::init(InitOptions {
            home: home.clone(),
            repository: repo.clone(),
            evaluator_root: repo.clone(),
            origin: "origin".to_owned(),
            readable_roots: vec![],
            executable_roots: vec![PathBuf::from("/usr/bin")],
            sdk_root: None,
            paid_spend_cap_cents: 0,
            proposal_model: None,
            commands: None,
            fixture_manifest: None,
        })?;
        Ok(Self { _temp: temp, repo, home, gate, base })
    }

    fn candidate(&self) -> Result<String, Box<dyn Error>> {
        drop(git(&self.repo, &["checkout", "-b", "gate/cand/one"])?);
        fs::write(self.repo.join("README.md"), b"candidate improvement\n")?;
        commit(&self.repo, "Improve readme")
    }

    fn evaluated_receipt(&self, candidate: &str, forge: bool) -> Result<(), Box<dyn Error>> {
        let config = read_config(&self.home)?;
        let tree = git(&self.repo, &["rev-parse", &format!("{candidate}^{{tree}}")])?;
        let body = ReceiptBody {
            version: FORMAT_VERSION,
            candidate_sha: candidate.to_owned(),
            tree_hash: tree,
            baseline_sha: self.base.clone(),
            rollback_sha: self.base.clone(),
            evaluator_digest: config.evaluator_digest,
            environment_digest: "0".repeat(64),
            validation_digest: "0".repeat(64),
            check_digest: "0".repeat(64),
            verify_digest: None,
            inventory_digest: "0".repeat(64),
            bench_artifacts_digest: "0".repeat(64),
            bench: BenchResults {
                baseline_pass_rate: 1.0,
                candidate_pass_rate: 1.0,
                baseline_first_request_bytes_p50: 1.0,
                candidate_first_request_bytes_p50: 1.0,
                baseline_first_request_ms_p50: 1.0,
                candidate_first_request_ms_p50: 1.0,
                baseline_cost_usd: Some(0.0),
                candidate_cost_usd: Some(0.0),
                repetitions: 2,
            },
            passed: true,
        };
        let mut receipt: SignedReceipt = sign_receipt(&self.home, body)?;
        if forge {
            receipt.body.tree_hash = "f".repeat(40);
        }
        let receipts = self.home.join("receipts");
        fs::create_dir(&receipts)?;
        write_json_new(&receipts.join("one.json"), &receipt)?;
        let receipt_digest = digest(&serde_json::to_vec(&receipt)?);
        append_ledger(&self.home, LedgerEvent::Evaluated { id: "one".to_owned(), receipt_digest })?;
        Ok(())
    }
}

#[test]
fn stale_main_ref_refuses_a_new_candidate() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    let candidate = fixture.candidate()?;
    fixture.gate.record_candidate("one", &fixture.base, &candidate)?;
    fixture.evaluated_receipt(&candidate, false)?;
    drop(git(&fixture.repo, &["checkout", "main"])?);
    fs::write(fixture.repo.join("README.md"), b"new main\n")?;
    drop(commit(&fixture.repo, "Advance main")?);
    drop(git(&fixture.repo, &["push", "origin", "main"])?);
    assert!(fixture.gate.record_candidate("two", &fixture.base, &candidate).is_err());
    assert!(promotion::promote(&fixture.home, &read_config(&fixture.home)?, "one").is_err());
    assert!(git(&fixture.repo, &["ls-remote", "origin", "refs/heads/gate/trial"])?.is_empty());
    Ok(())
}

#[test]
fn altered_signed_receipt_refuses_promotion() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    let candidate = fixture.candidate()?;
    fixture.gate.record_candidate("one", &fixture.base, &candidate)?;
    fixture.evaluated_receipt(&candidate, true)?;
    assert!(promotion::promote(&fixture.home, &read_config(&fixture.home)?, "one").is_err());
    assert!(git(&fixture.repo, &["ls-remote", "origin", "refs/heads/gate/trial"])?.is_empty());
    Ok(())
}

#[test]
#[ignore = "runs a real macOS Seatbelt profile and local listener"]
fn live_seatbelt_blocks_private_files_and_unapproved_ports() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let temp_root = fs::canonicalize(temp.path())?;
    let candidate = temp_root.join("candidate");
    let private = temp_root.join("private");
    fs::create_dir(&candidate)?;
    fs::create_dir(&private)?;
    fs::write(private.join("secret"), b"fixture-only\n")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let profile = Policy {
        readable: vec![
            PathBuf::from("/System"),
            PathBuf::from("/usr"),
            PathBuf::from("/dev/null"),
            PathBuf::from("/dev/urandom"),
            candidate.clone(),
        ],
        writable: vec![candidate.clone()],
        denied: vec![private.clone()],
        network: Network::LoopbackPort { port, listen: false },
    }
    .render()?;
    let run = |program: &str, args: &[&str]| -> Result<bool, Box<dyn Error>> {
        let status = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, program])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    };
    assert!(run("/usr/bin/true", &[])?);
    let secret = private.join("secret");
    let secret_text = secret.to_str().ok_or("non-UTF8 fixture path")?;
    assert!(!run("/bin/cat", &[secret_text])?);
    assert!(!run("/bin/sh", &["-c", "echo leak > \"$1\"", "sh", secret_text])?);
    std::os::unix::fs::symlink(&private, candidate.join("private-alias"))?;
    let alias = candidate.join("private-alias/secret");
    let alias_text = alias.to_str().ok_or("non-UTF8 fixture path")?;
    assert!(!run("/bin/cat", &[alias_text])?);
    let port_text = port.to_string();
    assert!(run("/usr/bin/nc", &["-G", "1", "-z", "127.0.0.1", &port_text])?);
    let other_port = if port == u16::MAX { port - 1 } else { port + 1 };
    let other_text = other_port.to_string();
    assert!(!run("/usr/bin/nc", &["-G", "1", "-z", "127.0.0.1", &other_text])?);
    assert!(!run("/usr/bin/nc", &["-G", "1", "-z", "1.1.1.1", "443"])?);
    Ok(())
}

#[test]
#[ignore = "runs a real macOS Seatbelt profile with local TCP and Unix listeners"]
fn live_test_loopback_stays_local_and_confines_unix_sockets() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = fs::canonicalize(temp.path())?;
    let candidate = root.join("candidate");
    let outside = root.join("outside");
    fs::create_dir(&candidate)?;
    fs::create_dir(&outside)?;
    let profile = Policy {
        readable: vec![PathBuf::from("/System"), PathBuf::from("/usr"), root],
        writable: vec![candidate.clone()],
        denied: vec![],
        network: Network::TestLoopback,
    }
    .render()?;
    let run = |args: &[&str]| -> Result<bool, Box<dyn Error>> {
        Ok(Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/usr/bin/nc"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success())
    };
    let tcp = TcpListener::bind("127.0.0.1:0")?;
    let port = tcp.local_addr()?.port().to_string();
    assert!(run(&["-G", "1", "-z", "127.0.0.1", &port])?);
    assert!(!run(&["-G", "1", "-z", "1.1.1.1", "443"])?);
    let inside = candidate.join("socket");
    let local = UnixListener::bind(&inside)?;
    let accepted = std::thread::spawn(move || -> std::io::Result<()> {
        let (stream, _) = local.accept()?;
        drop(stream);
        Ok(())
    });
    let inside_text = inside.to_str().ok_or("non-UTF8 fixture path")?;
    assert!(run(&["-U", inside_text])?);
    accepted.join().map_err(|_| "Unix listener thread panicked")??;
    let outside_socket = outside.join("socket");
    let _other = UnixListener::bind(&outside_socket)?;
    let outside_text = outside_socket.to_str().ok_or("non-UTF8 fixture path")?;
    assert!(!run(&["-U", outside_text])?);
    Ok(())
}
