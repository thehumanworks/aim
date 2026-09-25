//! Versioned gate configuration, signed receipt, and append-only ledger formats (ADR 0062).

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Current persisted format version.
pub const FORMAT_VERSION: u32 = 1;

/// An argv command owned by the gate configuration, never interpreted by a shell.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name or absolute path.
    pub program: String,
    /// Literal arguments.
    #[serde(default)]
    pub args: Vec<String>,
}

impl CommandSpec {
    /// A command with literal arguments.
    #[must_use]
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self { program: program.to_owned(), args: args.iter().map(|arg| (*arg).to_owned()).collect() }
    }
}

/// Paired, predeclared noninferiority limits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Thresholds {
    /// Largest allowed first request byte increase, in percent.
    pub max_first_request_bytes_increase_pct: f64,
    /// Largest allowed first request time p50 increase, in percent.
    pub max_first_request_p50_increase_pct: f64,
    /// Candidate pass rate cannot fall by more than this many percentage points.
    pub max_quality_drop_pp: f64,
    /// Candidate measured cost cannot rise by more than this percent.
    pub max_cost_increase_pct: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            max_first_request_bytes_increase_pct: 2.0,
            max_first_request_p50_increase_pct: 10.0,
            max_quality_drop_pp: 0.0,
            max_cost_increase_pct: 0.0,
        }
    }
}

/// Commands whose outputs the gate independently checks. A protected config may replace them
/// with tiny fake commands in tests, but a candidate cannot edit this file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Commands {
    /// Offline quality gate.
    pub check: CommandSpec,
    /// Verus gate when the kernel changes.
    pub verify: CommandSpec,
    /// Full test-name inventory.
    pub test_inventory: CommandSpec,
    /// Offline wire benchmark.
    pub bench: CommandSpec,
    /// Offline smoke after deploy.
    pub canary: CommandSpec,
}

impl Default for Commands {
    fn default() -> Self {
        Self {
            check: CommandSpec::new("mise", &["run", "check"]),
            verify: CommandSpec::new("mise", &["run", "verify"]),
            test_inventory: CommandSpec::new("cargo", &["test", "--workspace", "--locked", "--", "--list"]),
            bench: CommandSpec::new("mise", &["run", "bench:wire"]),
            canary: CommandSpec::new("mise", &["run", "smoke:offline"]),
        }
    }
}

/// Trusted gate configuration, kept private under the gate home.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateConfig {
    /// Format version.
    pub version: u32,
    /// Local source repository that contains candidate commits.
    pub repository: PathBuf,
    /// Pinned source tree containing the protected bench runner and manifest, outside the gate home.
    pub evaluator_root: PathBuf,
    /// Git remote name or URL used for trial and ledger pushes.
    pub origin: String,
    /// SHA-256 of the pinned evaluator binary, checked on every invocation.
    pub evaluator_digest: String,
    /// Protected evaluator files whose bytes are bound to `evaluator_digest`.
    pub suite_paths: Vec<PathBuf>,
    /// Additional canonical toolchain/cache roots mounted read-only in Seatbelt.
    #[serde(default)]
    pub readable_roots: Vec<PathBuf>,
    /// Evaluator-owned commands.
    #[serde(default)]
    pub commands: Commands,
    /// Relative path to the machine-readable `bench:wire` JSON artifact.
    pub bench_result: PathBuf,
    /// Harness identifier compared in paired wire rows.
    pub bench_harness: String,
    /// Predeclared thresholds.
    #[serde(default)]
    pub thresholds: Thresholds,
    /// Paid OpenRouter budget in cents. Zero disables the paid tier.
    pub paid_spend_cap_cents: u32,
}

impl GateConfig {
    /// Validate inputs before any candidate process can run.
    ///
    /// # Errors
    /// Malformed or unsafe configuration.
    pub fn validate(&self, home: &Path) -> Result<()> {
        ensure!(self.version == FORMAT_VERSION, "unsupported gate config version");
        ensure!(self.repository.is_absolute(), "gate repository must be absolute");
        ensure!(self.evaluator_root.is_absolute(), "evaluator root must be absolute");
        ensure!(!self.origin.is_empty(), "gate origin is missing");
        ensure!(self.evaluator_digest.len() == 64 && hex::decode(&self.evaluator_digest).is_ok(), "invalid evaluator digest");
        ensure!(self.paid_spend_cap_cents <= 50, "paid spend cap exceeds the W28 limit");
        ensure!(
            !self.bench_result.is_absolute() && !self.bench_result.components().any(|c| matches!(c, std::path::Component::ParentDir)),
            "bench result must be workspace-relative"
        );
        ensure!(!self.bench_harness.is_empty(), "bench harness is missing");
        ensure!(!self.suite_paths.is_empty(), "protected evaluator suite list is empty");
        for value in [
            self.thresholds.max_first_request_bytes_increase_pct,
            self.thresholds.max_first_request_p50_increase_pct,
            self.thresholds.max_quality_drop_pp,
            self.thresholds.max_cost_increase_pct,
        ] {
            ensure!(value.is_finite() && value >= 0.0, "invalid gate threshold");
        }
        let home = fs::canonicalize(home).context("gate home must exist")?;
        let evaluator_root = fs::canonicalize(&self.evaluator_root).context("evaluator root unavailable")?;
        ensure!(!evaluator_root.starts_with(&home) && !home.starts_with(&evaluator_root), "evaluator root overlaps gate home");
        for path in &self.suite_paths {
            ensure!(
                !path.is_absolute() && !path.components().any(|c| matches!(c, std::path::Component::ParentDir)),
                "suite path must be relative"
            );
            let full = fs::canonicalize(evaluator_root.join(path)).context("protected suite missing")?;
            ensure!(full.starts_with(&evaluator_root), "protected suite escapes evaluator root");
        }
        for root in &self.readable_roots {
            let real = fs::canonicalize(root).with_context(|| format!("gate read root unavailable: {}", root.display()))?;
            ensure!(!real.starts_with(&home) && !home.starts_with(&real), "read root overlaps gate home");
            ensure!(real != Path::new("/"), "read root cannot be filesystem root");
        }
        for command in
            [&self.commands.check, &self.commands.verify, &self.commands.test_inventory, &self.commands.bench, &self.commands.canary]
        {
            ensure!(!command.program.is_empty(), "empty gate command");
        }
        Ok(())
    }
}

/// Hash the running gate executable and every configured protected evaluator file in declared
/// order. The config pins this digest before any candidate evaluation.
///
/// # Errors
/// Missing or unreadable binary/suite.
pub fn current_evaluator_digest(config: &GateConfig) -> Result<String> {
    let executable = std::env::current_exe().context("find gate executable")?;
    let mut hasher = Sha256::new();
    hasher.update(b"aim-gate-evaluator-v1\0");
    hasher.update(fs::read(executable).context("read gate executable")?);
    for path in &config.suite_paths {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(fs::read(config.evaluator_root.join(path)).context("read protected suite")?);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Paired offline evidence retained in the signed receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchResults {
    /// Baseline pass rate.
    pub baseline_pass_rate: f64,
    /// Candidate pass rate.
    pub candidate_pass_rate: f64,
    /// Baseline first request bytes median.
    pub baseline_first_request_bytes_p50: f64,
    /// Candidate first request bytes median.
    pub candidate_first_request_bytes_p50: f64,
    /// Baseline first request timing median, a startup proxy.
    pub baseline_first_request_ms_p50: f64,
    /// Candidate first request timing median.
    pub candidate_first_request_ms_p50: f64,
    /// Baseline measured USD cost, if present.
    pub baseline_cost_usd: Option<f64>,
    /// Candidate measured USD cost, if present.
    pub candidate_cost_usd: Option<f64>,
    /// Number of paired runs for each side.
    pub repetitions: usize,
}

/// Data covered by a gate receipt signature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptBody {
    /// Format version.
    pub version: u32,
    /// Candidate commit.
    pub candidate_sha: String,
    /// Candidate Git tree.
    pub tree_hash: String,
    /// Baseline commit.
    pub baseline_sha: String,
    /// Pinned gate executable plus protected suite digest.
    pub evaluator_digest: String,
    /// Toolchain pins, OS, architecture and sandbox policy digest.
    pub environment_digest: String,
    /// Independent validator report digest.
    pub validation_digest: String,
    /// SHA-256 of trusted check command output and exit observation.
    pub check_digest: String,
    /// SHA-256 of trusted Verus output, when the kernel changed.
    pub verify_digest: Option<String>,
    /// SHA-256 of baseline and candidate full test inventories.
    pub inventory_digest: String,
    /// SHA-256 of both raw benchmark artifacts.
    pub bench_artifacts_digest: String,
    /// Paired benchmark evidence.
    pub bench: BenchResults,
    /// Evaluator result.
    pub passed: bool,
}

/// Signed receipt; verification uses the gate home's trusted key, never this object's key claim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedReceipt {
    /// Signed data.
    pub body: ReceiptBody,
    /// Hex-encoded ed25519 signature.
    pub signature: String,
}

/// One typed ledger transition.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerEvent {
    /// Candidate recorded by the gate.
    Proposed {
        /// Candidate identifier.
        id: String,
        /// Expected baseline commit.
        base_sha: String,
        /// Candidate commit.
        candidate_sha: String,
    },
    /// Receipt issued after evaluation.
    Evaluated {
        /// Candidate identifier.
        id: String,
        /// Digest of the private signed receipt.
        receipt_digest: String,
    },
    /// Candidate refused.
    Rejected {
        /// Candidate identifier.
        id: String,
        /// Short refusal reason.
        reason: String,
    },
    /// Trial ref moved after base compare-and-swap.
    Merged {
        /// Candidate identifier.
        id: String,
        /// Trial merge commit.
        sha: String,
    },
    /// Remote ref matched after push.
    Pushed {
        /// Candidate identifier.
        id: String,
        /// Remote readback commit.
        sha: String,
    },
    /// Exact artifact staged.
    Deployed {
        /// Candidate identifier.
        id: String,
        /// Exact deployed commit.
        sha: String,
        /// Retained predecessor commit.
        predecessor: String,
    },
    /// Offline canary passed.
    Canary {
        /// Candidate identifier.
        id: String,
    },
    /// Trial became active.
    Active {
        /// Candidate identifier.
        id: String,
    },
    /// Atomic symlink returned to predecessor.
    RolledBack {
        /// Candidate identifier.
        id: String,
        /// Re-activated predecessor commit.
        predecessor: String,
    },
    /// A promotion step failed.
    Failed {
        /// Candidate identifier.
        id: String,
        /// Failing promotion step.
        step: String,
    },
}

/// Hash-chained append-only ledger record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerRecord {
    /// Monotone sequence.
    pub seq: u64,
    /// Previous record hash, all zero for genesis.
    pub prev_hash: String,
    /// Transition.
    pub event: LedgerEvent,
    /// SHA-256 over versioned predecessor, sequence and event.
    pub hash: String,
}

/// SHA-256 hex digest.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Create a private gate home and refuse symlinks or permissive ownership/mode.
///
/// # Errors
/// Filesystem error or unsafe ownership/mode.
pub fn ensure_private_home(home: &Path) -> Result<()> {
    if !home.exists() {
        fs::create_dir_all(home).context("create gate home")?;
        fs::set_permissions(home, fs::Permissions::from_mode(0o700)).context("set gate home mode")?;
    }
    let meta = fs::symlink_metadata(home).context("inspect gate home")?;
    ensure!(meta.file_type().is_dir() && meta.permissions().mode().trailing_zeros() >= 6, "gate home must be a private directory");
    ensure!(meta.uid() == rustix::process::geteuid().as_raw(), "gate home belongs to another user");
    Ok(())
}

/// Refuse a non-private existing file and open it without following a final symlink.
///
/// # Errors
/// Unsafe path or filesystem error.
pub fn private_file(path: &Path) -> Result<File> {
    let file = File::from(open(path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()).context("open private file without symlink")?);
    let meta = file.metadata().context("inspect opened private file")?;
    ensure!(meta.file_type().is_file() && meta.permissions().mode().trailing_zeros() >= 6, "gate file must be private and regular");
    ensure!(meta.uid() == rustix::process::geteuid().as_raw(), "gate file belongs to another user");
    Ok(file)
}

/// Initialize a private ed25519 receipt key once.
///
/// # Errors
/// Entropy or filesystem failure.
pub fn init_receipt_key(home: &Path) -> Result<()> {
    ensure_private_home(home)?;
    let path = home.join("receipt.key");
    ensure!(!path.exists(), "receipt key already exists");
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow::anyhow!("ed25519 key generation failed"))?;
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).context("create receipt key")?;
    file.write_all(pkcs8.as_ref()).context("write receipt key")?;
    file.sync_all().context("sync receipt key")?;
    Ok(())
}

fn key_pair(home: &Path) -> Result<Ed25519KeyPair> {
    let mut bytes = Vec::new();
    private_file(&home.join("receipt.key"))?.read_to_end(&mut bytes).context("read receipt key")?;
    Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| anyhow::anyhow!("invalid receipt key"))
}

/// Sign only a complete evaluator result.
///
/// # Errors
/// Missing key, malformed payload, or serialization failure.
pub fn sign_receipt(home: &Path, body: ReceiptBody) -> Result<SignedReceipt> {
    ensure!(body.version == FORMAT_VERSION && body.passed, "cannot sign a failed evaluation");
    let pair = key_pair(home)?;
    let bytes = serde_json::to_vec(&body).context("serialize receipt")?;
    Ok(SignedReceipt { body, signature: hex::encode(pair.sign(&bytes).as_ref()) })
}

/// Verify against the pinned private key's public half and expected evaluator digest.
///
/// # Errors
/// Forged, altered, stale, or failed receipt.
pub fn verify_receipt(home: &Path, receipt: &SignedReceipt, pinned_evaluator: &str) -> Result<()> {
    ensure!(receipt.body.version == FORMAT_VERSION && receipt.body.passed, "receipt is not a passing v1 result");
    ensure!(receipt.body.evaluator_digest == pinned_evaluator, "receipt evaluator digest mismatch");
    let signature = hex::decode(&receipt.signature).context("decode receipt signature")?;
    let bytes = serde_json::to_vec(&receipt.body).context("serialize receipt")?;
    let key = key_pair(home)?;
    UnparsedPublicKey::new(&ED25519, key.public_key().as_ref())
        .verify(&bytes, &signature)
        .map_err(|_| anyhow::anyhow!("receipt signature invalid"))
}

#[derive(Serialize)]
struct ChainPayload<'a> {
    version: u32,
    seq: u64,
    prev_hash: &'a str,
    event: &'a LedgerEvent,
}

fn record_hash(seq: u64, prev_hash: &str, event: &LedgerEvent) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&ChainPayload { version: FORMAT_VERSION, seq, prev_hash, event }).context("serialize ledger event")?))
}

/// Parse and verify every ledger edge, detecting rewrites and truncated/corrupt entries.
///
/// # Errors
/// Malformed or broken hash chain.
pub fn verify_ledger(bytes: &[u8]) -> Result<Vec<LedgerRecord>> {
    if !bytes.is_empty() {
        ensure!(bytes.last() == Some(&b'\n'), "ledger has an incomplete trailing record");
    }
    let mut previous = "0".repeat(64);
    let mut records = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let record: LedgerRecord = serde_json::from_slice(line).context("decode ledger record")?;
        let seq = u64::try_from(records.len()).context("ledger too long")?;
        ensure!(record.seq == seq && record.prev_hash == previous, "ledger sequence or predecessor mismatch");
        ensure!(record.hash == record_hash(record.seq, &record.prev_hash, &record.event)?, "ledger hash mismatch");
        previous.clone_from(&record.hash);
        records.push(record);
    }
    Ok(records)
}

/// Append a verified event under a stable private lock, then fsync the ledger.
///
/// # Errors
/// Unsafe paths, previous chain corruption, locking or write failure.
pub fn append_ledger(home: &Path, event: LedgerEvent) -> Result<LedgerRecord> {
    ensure_private_home(home)?;
    let lock_path = home.join("ledger.lock");
    let lock_file = File::from(
        open(&lock_path, OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW, Mode::from_raw_mode(0o600)).context("open ledger lock")?,
    );
    let lock_meta = lock_file.metadata().context("inspect ledger lock")?;
    ensure!(lock_meta.file_type().is_file() && lock_meta.permissions().mode().trailing_zeros() >= 6, "unsafe ledger lock");
    ensure!(lock_meta.uid() == rustix::process::geteuid().as_raw(), "ledger lock belongs to another user");
    flock(&lock_file, FlockOperation::LockExclusive).context("lock ledger")?;
    let path = home.join("ledger.jsonl");
    let old = match fs::symlink_metadata(&path) {
        Ok(_) => {
            let mut file = private_file(&path)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).context("read ledger")?;
            bytes
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).context("read ledger"),
    };
    let entries = verify_ledger(&old)?;
    let seq = u64::try_from(entries.len()).context("ledger too long")?;
    let prev_hash = entries.last().map_or_else(|| "0".repeat(64), |record| record.hash.clone());
    let hash = record_hash(seq, &prev_hash, &event)?;
    let record = LedgerRecord { seq, prev_hash, event, hash };
    let mut file = File::from(
        open(&path, OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW, Mode::from_raw_mode(0o600))
            .context("open ledger for append")?,
    );
    serde_json::to_writer(&mut file, &record).context("write ledger")?;
    file.write_all(b"\n").context("write ledger newline")?;
    file.sync_all().context("sync ledger")?;
    Ok(record)
}

/// Read a verified private ledger.
///
/// # Errors
/// Unsafe file or broken chain.
pub fn read_ledger(home: &Path) -> Result<Vec<LedgerRecord>> {
    let path = home.join("ledger.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    private_file(&path)?.read_to_end(&mut bytes).context("read ledger")?;
    verify_ledger(&bytes)
}

/// Write a new private JSON artifact without replacing an existing receipt.
///
/// # Errors
/// Serialization or filesystem error.
pub fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).context("create gate artifact")?;
    serde_json::to_writer(&mut file, value).context("write gate artifact")?;
    file.write_all(b"\n").context("write gate artifact newline")?;
    file.sync_all().context("sync gate artifact")?;
    Ok(())
}

/// Load a private JSON artifact.
///
/// # Errors
/// Unsafe file or invalid JSON.
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = private_file(path)?;
    serde_json::from_reader(file).context("decode gate artifact")
}

/// Load and validate a private gate config.
///
/// # Errors
/// Unsafe path or malformed configuration.
pub fn read_config(home: &Path) -> Result<GateConfig> {
    ensure_private_home(home)?;
    let mut text = String::new();
    private_file(&home.join("config.toml"))?.read_to_string(&mut text).context("read gate config")?;
    let config: GateConfig = toml::from_str(&text).context("parse gate config")?;
    config.validate(home)?;
    Ok(config)
}

/// Write a new private gate config.
///
/// # Errors
/// Unsafe path or encoding failure.
pub fn write_config_new(home: &Path, config: &GateConfig) -> Result<()> {
    ensure_private_home(home)?;
    config.validate(home)?;
    let mut file =
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(home.join("config.toml")).context("create gate config")?;
    file.write_all(toml::to_string_pretty(config).context("encode gate config")?.as_bytes()).context("write gate config")?;
    file.sync_all().context("sync gate config")?;
    Ok(())
}

/// Require a private file to have no symlink and no group/other access.
///
/// # Errors
/// Unsafe file.
pub fn check_private(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("private path missing");
    }
    drop(private_file(path)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn altered_receipt_and_ledger_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("gate");
        init_receipt_key(&home).unwrap();
        let body = ReceiptBody {
            version: FORMAT_VERSION,
            candidate_sha: "a".repeat(40),
            tree_hash: "b".repeat(40),
            baseline_sha: "c".repeat(40),
            evaluator_digest: "d".repeat(64),
            environment_digest: "e".repeat(64),
            validation_digest: "f".repeat(64),
            check_digest: "1".repeat(64),
            verify_digest: None,
            inventory_digest: "2".repeat(64),
            bench_artifacts_digest: "3".repeat(64),
            bench: BenchResults {
                baseline_pass_rate: 1.0,
                candidate_pass_rate: 1.0,
                baseline_first_request_bytes_p50: 100.0,
                candidate_first_request_bytes_p50: 99.0,
                baseline_first_request_ms_p50: 10.0,
                candidate_first_request_ms_p50: 9.0,
                baseline_cost_usd: Some(0.0),
                candidate_cost_usd: Some(0.0),
                repetitions: 2,
            },
            passed: true,
        };
        let signed = sign_receipt(&home, body).unwrap();
        verify_receipt(&home, &signed, &"d".repeat(64)).unwrap();
        let mut altered = signed;
        altered.body.tree_hash = "0".repeat(40);
        assert!(verify_receipt(&home, &altered, &"d".repeat(64)).is_err());
        append_ledger(&home, LedgerEvent::Canary { id: "one".into() }).unwrap();
        append_ledger(&home, LedgerEvent::Active { id: "one".into() }).unwrap();
        assert_eq!(read_ledger(&home).unwrap().len(), 2);
        let path = home.join("ledger.jsonl");
        let mut bytes = fs::read(&path).unwrap();
        bytes[10] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(read_ledger(&home).is_err());
    }
}
