//! Trusted gate orchestration. All final artifact evidence is produced after candidate exit.

use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use aim_gate_validators::{ProtectedManifest, ValidationConfig, validate};
use aim_kernel::gate::{Boundary, Event, GateView, advance};
use anyhow::{Context as _, Result, ensure};
use ring::rand::{SecureRandom as _, SystemRandom};
use rustix::fs::{Mode, OFlags, open};

use crate::bench;
use crate::broker::{Broker, BrokerConfig};
use crate::cache;
use crate::checkpoint;
use crate::deploy;
use crate::formats::{
    Commands, FORMAT_VERSION, GateConfig, LedgerEvent, ReceiptBody, SignedReceipt, Thresholds, append_ledger, current_environment_digest,
    current_evaluator_digest, digest, ensure_private_home, init_receipt_key, read_config, read_ledger, sign_receipt, verify_receipt,
    write_config_new, write_json_new,
};
use crate::promotion;
use crate::runner::{Sandbox, bench_port, fresh_checkout, trusted, validate_sha};
use crate::sandbox::Network;

const MAX_BUNDLE_BYTES: u64 = 128 * 1024 * 1024;

/// Operator-owned bootstrap paths. None may be inside the candidate clone.
#[derive(Clone, Debug)]
pub struct InitOptions {
    /// Private 0700 gate home; normally `~/.aim-gate`.
    pub home: PathBuf,
    /// Source Git repository holding accepted candidate commits.
    pub repository: PathBuf,
    /// Pinned evaluator source tree containing bench scripts/manifest and mise.toml.
    pub evaluator_root: PathBuf,
    /// Git remote name or URL for private gate branches.
    pub origin: String,
    /// Read-only cache/toolchain directories for candidate Seatbelt profiles.
    pub readable_roots: Vec<PathBuf>,
    /// Directories searched for executables inside Seatbelt.
    pub executable_roots: Vec<PathBuf>,
    /// macOS SDK for sandboxed native builds, inside one of the readable roots.
    pub sdk_root: Option<PathBuf>,
    /// Paid proposal cap in cents, at most 50; zero disables paid proposals.
    pub paid_spend_cap_cents: u32,
    /// Catalog model id selected by the operator for the paid tier.
    pub proposal_model: Option<String>,
    /// Gate-owned step overrides for tiny fixture repositories; normal CLI passes `None`.
    pub commands: Option<Commands>,
    /// Fake protected manifest for tiny fixture repos; normal CLI passes `None`.
    pub fixture_manifest: Option<String>,
}

/// Gate initialized from private config and a pinned running binary.
#[derive(Clone, Debug)]
pub struct GateRuntime {
    home: PathBuf,
    config: GateConfig,
}

impl GateRuntime {
    /// Create a new gate home/key/config, pinning the current binary and protected evaluator tree.
    ///
    /// # Errors
    /// Existing config/key, unsafe paths, missing protected evaluator files, or storage failure.
    pub fn init(options: InitOptions) -> Result<Self> {
        if options.fixture_manifest.is_some() {
            ensure!(
                options.commands.is_some() && options.paid_spend_cap_cents == 0,
                "fixture manifest requires fake offline commands and no paid access"
            );
        }
        ensure_private_home(&options.home)?;
        let home = fs::canonicalize(&options.home).context("canonicalize private gate home")?;
        let repository = fs::canonicalize(&options.repository).context("source repository missing")?;
        let evaluator_root = fs::canonicalize(&options.evaluator_root).context("evaluator root missing")?;
        ensure!(!repository.starts_with(&home) && !evaluator_root.starts_with(&home), "source overlaps gate home");
        let suite_paths = protected_suite_paths(&evaluator_root)?;
        let owner_home = std::env::var_os("HOME").map(PathBuf::from);
        let cargo_registry = owner_home.as_ref().map(|path| path.join(".cargo/registry")).filter(|path| path.is_dir());
        let cargo_git = owner_home.as_ref().map(|path| path.join(".cargo/git")).filter(|path| path.is_dir());
        let mise_data = std::env::var_os("MISE_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| owner_home.as_ref().map(|path| path.join(".local/share/mise")))
            .filter(|path| path.is_dir());
        let mut config = GateConfig {
            version: FORMAT_VERSION,
            repository,
            evaluator_root,
            deploy_root: home.join("deploy"),
            origin: options.origin,
            evaluator_digest: "0".repeat(64),
            suite_paths,
            fixture_manifest: options.fixture_manifest,
            readable_roots: options.readable_roots,
            cargo_registry,
            cargo_git,
            mise_data,
            sdk_root: options.sdk_root.map(fs::canonicalize).transpose().context("canonicalize gate SDK root")?,
            executable_roots: options.executable_roots,
            commands: options.commands.unwrap_or_default(),
            bench_result: PathBuf::from("bench/history/latest-wire.json"),
            bench_harness: "aim_openrouter".to_owned(),
            thresholds: Thresholds::default(),
            paid_spend_cap_cents: options.paid_spend_cap_cents,
            proposal_model: options.proposal_model,
        };
        config.validate(&home)?;
        config.evaluator_digest = current_evaluator_digest(&config)?;
        init_receipt_key(&home)?;
        write_config_new(&home, &config)?;
        Ok(Self { home, config })
    }

    /// Open a private gate home only if the running evaluator still matches its pinned digest.
    ///
    /// # Errors
    /// Unsafe config/key or evaluator drift.
    pub fn open(home: &Path) -> Result<Self> {
        let config = read_config(home)?;
        ensure!(current_evaluator_digest(&config)? == config.evaluator_digest, "running evaluator differs from pinned digest");
        Ok(Self { home: home.to_path_buf(), config })
    }

    /// Record an externally produced candidate commit. This never grants promotion without the
    /// independent evaluation and signed receipt. It also supports tiny fixture repositories.
    ///
    /// # Errors
    /// Invalid ids, stale baseline, missing candidate object, or ledger failure.
    pub fn record_candidate(&self, id: &str, base_sha: &str, candidate_sha: &str) -> Result<()> {
        valid_id(id)?;
        validate_sha(base_sha)?;
        validate_sha(candidate_sha)?;
        ensure!(
            !read_ledger(&self.home)?
                .iter()
                .any(|record| matches!(&record.event, LedgerEvent::Proposed { id: existing, .. } if existing == id)),
            "candidate id already exists"
        );
        let main_ref = format!("refs/remotes/{}/main", self.config.origin);
        let main = trusted("git", &["rev-parse", &main_ref], &self.config.repository)?;
        ensure!(base_sha == main, "candidate baseline is stale");
        ensure!(
            trusted("git", &["cat-file", "-t", candidate_sha], &self.config.repository)? == "commit",
            "candidate object is not a commit"
        );
        let record = append_ledger(
            &self.home,
            LedgerEvent::Proposed { id: id.to_owned(), base_sha: base_sha.to_owned(), candidate_sha: candidate_sha.to_owned() },
        )?;
        checkpoint::checkpoint(&self.config.origin, &self.config.repository, &self.home, record.seq, &record.hash)?;
        Ok(())
    }

    /// Run a proposing agent in a fresh confined clone. The paid broker must be configured and
    /// proven before this command can contact a model.
    ///
    /// # Errors
    /// Paid broker unavailable or proposal failed.
    #[expect(clippy::too_many_lines, reason = "keep the confined proposal and broker accounting sequence together")]
    pub fn propose(&self, goal: &str) -> Result<String> {
        ensure!(!goal.trim().is_empty() && goal.len() <= 4096, "proposal goal is empty or too long");
        ensure!(self.config.paid_spend_cap_cents > 0, "paid proposals are disabled in gate config");
        let model = self.config.proposal_model.as_deref().context("paid proposal model is missing")?;
        ensure!(current_evaluator_digest(&self.config)? == self.config.evaluator_digest, "pinned evaluator changed before proposal");
        let mut random = [0_u8; 12];
        SystemRandom::new().fill(&mut random).map_err(|_| anyhow::anyhow!("candidate id entropy unavailable"))?;
        let id = format!("cand-{}", hex::encode(random));
        let base_ref = format!("refs/remotes/{}/main", self.config.origin);
        let base_sha = trusted("git", &["rev-parse", &base_ref], &self.config.repository)?;
        validate_sha(&base_sha)?;
        let temp = tempfile::Builder::new().prefix("aim-gate-propose-").tempdir().context("create private proposal root")?;
        let candidate = temp.path().join("candidate");
        fresh_checkout(&self.config.repository, &base_sha, &candidate, &self.home, &self.config)?;
        let branch = format!("gate/cand/{id}");
        let offline = Sandbox::new(&candidate, &self.home, &self.config, Network::Off)?;
        offline.run(&crate::formats::CommandSpec::new("git", &["switch", "-c", &branch])).context("sandboxed proposal branch creation")?;
        offline.run(&self.config.commands.build).context("sandboxed proposal binary build")?;
        let aim = candidate.join("target/debug/aim");
        let aimx = candidate.join("target/debug/aimx");
        ensure!(aim.is_file() && aimx.is_file(), "proposal build omitted aim or aimx binary");
        let key = std::env::var("OPENROUTER_API_KEY").context("gate process lacks OpenRouter credential")?;
        let mut broker = Broker::start(BrokerConfig {
            api_key: key,
            model: model.to_owned(),
            upstream_url: "https://openrouter.ai/api/v1/chat/completions".to_owned(),
            max_cents: self.config.paid_spend_cap_cents,
        })?;
        let sandbox = Sandbox::new(&candidate, &self.home, &self.config, Network::LoopbackPort { port: broker.port(), listen: false })?;
        let prompt = format!(
            "Improve this repository for this goal: {goal}. Make one focused, testable change. Keep gate/protected files unchanged."
        );
        let command = crate::formats::CommandSpec {
            program: aim.to_str().context("candidate aim path is not UTF-8")?.to_owned(),
            args: vec![
                "run".into(),
                "--provider".into(),
                "openrouter".into(),
                "--model".into(),
                model.into(),
                "-C".into(),
                candidate.to_str().context("candidate path is not UTF-8")?.into(),
                "--aimx".into(),
                aimx.to_str().context("candidate aimx path is not UTF-8")?.into(),
                "--ephemeral".into(),
                "--max-requests".into(),
                "8".into(),
                prompt,
            ],
        };
        let result = sandbox.run_proposal(&command, &broker.base_url());
        broker.stop()?;
        let spent = broker.spent_cents();
        result.context("candidate proposal agent failed")?;
        ensure!(spent <= self.config.paid_spend_cap_cents, "proposal spend exceeded configured cap");
        sandbox.clean_scratch()?;
        let dirty = offline
            .run(&crate::formats::CommandSpec::new(
                "git",
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--",
                    ".",
                    ":(exclude).gate-home",
                    ":(exclude).gate-tmp",
                    ":(exclude).gate-mise",
                    ":(exclude)target",
                ],
            ))?
            .output;
        if !dirty.trim().is_empty() {
            offline.clean_scratch()?;
            offline.run(&crate::formats::CommandSpec::new(
                "git",
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "add",
                    "-A",
                    "--",
                    ".",
                    ":(exclude).gate-home",
                    ":(exclude).gate-tmp",
                    ":(exclude).gate-mise",
                    ":(exclude)target",
                ],
            ))?;
            offline.run(&crate::formats::CommandSpec::new(
                "git",
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "user.name=aim-gate",
                    "-c",
                    "user.email=aim-gate@noreply.local",
                    "commit",
                    "-m",
                    "Record gate candidate",
                ],
            ))?;
        }
        offline.clean_scratch()?;
        let candidate_sha = offline.run(&crate::formats::CommandSpec::new("git", &["rev-parse", "HEAD"]))?.output.trim().to_owned();
        validate_sha(&candidate_sha)?;
        ensure!(candidate_sha != base_sha, "proposal produced no candidate commit");
        let candidate_tree = offline.run(&crate::formats::CommandSpec::new("git", &["rev-parse", "HEAD^{tree}"]))?.output.trim().to_owned();
        validate_sha(&candidate_tree)?;
        let bundle_path = candidate.join(".gate-candidate.bundle");
        match fs::symlink_metadata(&bundle_path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => anyhow::bail!("candidate occupied gate bundle path"),
            Err(err) => return Err(err).context("inspect candidate bundle path"),
        }
        let bundle_source = bundle_path.to_str().context("candidate bundle path is not UTF-8")?;
        offline.run(&crate::formats::CommandSpec::new("git", &["bundle", "create", bundle_source, "HEAD"]))?;
        // Import only bundle data, never run trusted Git against candidate-writable `.git/config`.
        let mut input = File::from(
            open(&bundle_path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()).context("open candidate bundle without symlink")?,
        );
        ensure!(input.metadata()?.is_file(), "candidate bundle is not a regular file");
        let mut private_bundle = tempfile::NamedTempFile::new_in(temp.path()).context("create private gate bundle")?;
        let copied = std::io::copy(&mut input.by_ref().take(MAX_BUNDLE_BYTES + 1), private_bundle.as_file_mut())?;
        ensure!(copied <= MAX_BUNDLE_BYTES, "candidate bundle exceeds gate limit");
        private_bundle.as_file().sync_all().context("sync private gate bundle")?;
        let private_path = private_bundle.path().to_str().context("private bundle path is not UTF-8")?;
        let refspec = format!("HEAD:refs/heads/{branch}");
        trusted("git", &["fetch", "--no-tags", private_path, &refspec], &self.config.repository)?;
        ensure!(
            trusted("git", &["rev-parse", &format!("refs/heads/{branch}")], &self.config.repository)? == candidate_sha,
            "candidate import readback mismatch"
        );
        ensure!(
            trusted("git", &["rev-parse", &format!("{candidate_sha}^{{tree}}")], &self.config.repository)? == candidate_tree,
            "candidate imported tree differs from confined commit"
        );
        self.record_candidate(&id, &base_sha, &candidate_sha)?;
        Ok(id)
    }

    /// Independently evaluate an exact candidate commit and issue a signed receipt.
    ///
    /// # Errors
    /// Any protected validator, sandbox, benchmark, threshold, or signing failure.
    #[expect(clippy::too_many_lines, reason = "keep the ordered signed evaluation evidence in one audit-friendly transaction")]
    pub fn evaluate(&self, sha: &str) -> Result<SignedReceipt> {
        validate_sha(sha)?;
        ensure!(current_evaluator_digest(&self.config)? == self.config.evaluator_digest, "pinned evaluator changed during gate run");
        let (id, base_sha) = self.proposal_for(sha)?;
        let receipts = self.home.join("receipts");
        fs::create_dir_all(&receipts).context("create receipt directory")?;
        fs::set_permissions(&receipts, std::os::unix::fs::PermissionsExt::from_mode(0o700)).context("protect receipt directory")?;
        ensure!(!receipts.join(format!("{id}.json")).exists(), "candidate already has a receipt");
        let temp = tempfile::Builder::new().prefix("aim-gate-evaluate-").tempdir().context("create private evaluation root")?;
        let base = temp.path().join("base");
        let candidate = temp.path().join("candidate");
        fresh_checkout(&self.config.repository, &base_sha, &base, &self.home, &self.config)?;
        fresh_checkout(&self.config.repository, sha, &candidate, &self.home, &self.config)?;
        let _reused_baseline = cache::restore(&self.home, &base_sha, &self.config.evaluator_digest, &base.join("target"))?;
        let base_sandbox = Sandbox::new(&base, &self.home, &self.config, Network::Off)?;
        let candidate_sandbox = Sandbox::new(&candidate, &self.home, &self.config, Network::Off)?;
        let base_inventory = base_sandbox.run(&self.config.commands.test_inventory)?.output;
        let candidate_inventory = candidate_sandbox.run(&self.config.commands.test_inventory)?.output;
        base_sandbox.clean_scratch()?;
        candidate_sandbox.clean_scratch()?;
        let manifest = if let Some(fixture) = &self.config.fixture_manifest {
            ProtectedManifest::parse(fixture).map_err(anyhow::Error::msg)?
        } else {
            ProtectedManifest::embedded().map_err(anyhow::Error::msg)?
        };
        let validation = ValidationConfig {
            manifest,
            baseline_test_inventory: base_inventory.clone(),
            candidate_test_inventory: candidate_inventory.clone(),
        };
        let first = validate(&base, &candidate, &validation).map_err(anyhow::Error::msg)?;
        ensure!(first.passed(), "candidate violates protected validator: {}", first.findings.join("; "));
        let mut evidence = Vec::new();
        for (name, spec) in
            [("rustfmt", &self.config.commands.rustfmt), ("clippy", &self.config.commands.clippy), ("build", &self.config.commands.build)]
        {
            let left = base_sandbox.run(spec)?.output;
            let right = candidate_sandbox.run(spec)?.output;
            evidence.extend_from_slice(name.as_bytes());
            evidence.extend_from_slice(left.as_bytes());
            evidence.extend_from_slice(right.as_bytes());
        }
        let base_test = Sandbox::new(&base, &self.home, &self.config, Network::TestLoopback)?;
        let candidate_test = Sandbox::new(&candidate, &self.home, &self.config, Network::TestLoopback)?;
        let left_test = base_test.run(&self.config.commands.test)?.output;
        let right_test = candidate_test.run(&self.config.commands.test)?.output;
        evidence.extend_from_slice(b"test");
        evidence.extend_from_slice(left_test.as_bytes());
        evidence.extend_from_slice(right_test.as_bytes());
        let left_verusfmt = kernel_verusfmt(&self.config.commands.verusfmt, &base)?;
        let right_verusfmt = kernel_verusfmt(&self.config.commands.verusfmt, &candidate)?;
        evidence.extend_from_slice(base_sandbox.run(&left_verusfmt)?.output.as_bytes());
        evidence.extend_from_slice(candidate_sandbox.run(&right_verusfmt)?.output.as_bytes());
        let verify_digest = if first.changed_paths.iter().any(|path| path.starts_with("crates/aim-kernel/")) {
            let left = base_sandbox.run(&self.config.commands.verify)?.output;
            let right = candidate_sandbox.run(&self.config.commands.verify)?.output;
            Some(digest(format!("{left}\0{right}").as_bytes()))
        } else {
            None
        };
        let port = bench_port()?;
        let baseline_bench = self.run_pinned_bench(&base, port)?;
        let candidate_bench = self.run_pinned_bench(&candidate, port)?;
        let bench = bench::compare(&baseline_bench, &candidate_bench, &self.config.bench_harness, &self.config.thresholds)?;
        for sandbox in [&base_sandbox, &candidate_sandbox] {
            sandbox.clean_scratch()?;
        }
        for clone in [&base, &candidate] {
            let result = clone.join(&self.config.bench_result);
            fs::remove_file(result).context("remove gate-owned benchmark result before final source validation")?;
        }
        let final_report = validate(&base, &candidate, &validation).map_err(anyhow::Error::msg)?;
        ensure!(final_report.passed(), "generated files violate protected validator: {}", final_report.findings.join("; "));
        cache::store(&self.home, &base_sha, &self.config.evaluator_digest, &base.join("target"))?;
        let tree_hash = trusted("git", &["rev-parse", &format!("{sha}^{{tree}}")], &self.config.repository)?;
        validate_sha(&tree_hash)?;
        let rollback_sha = self.ensure_predecessor(&base, &base_sha)?;
        let environment_digest = current_environment_digest(&self.config, &self.home)?;
        let validation_digest = digest(&serde_json::to_vec(&final_report).context("encode validator report")?);
        let inventory_digest = digest(format!("{base_inventory}\0{candidate_inventory}").as_bytes());
        let bench_artifacts_digest = digest(&[baseline_bench.as_slice(), candidate_bench.as_slice()].concat());
        let body = ReceiptBody {
            version: FORMAT_VERSION,
            candidate_sha: sha.to_owned(),
            tree_hash: tree_hash.clone(),
            baseline_sha: base_sha.clone(),
            rollback_sha,
            evaluator_digest: self.config.evaluator_digest.clone(),
            environment_digest,
            validation_digest,
            check_digest: digest(&evidence),
            verify_digest,
            inventory_digest,
            bench_artifacts_digest,
            bench,
            passed: true,
        };
        ensure!(current_evaluator_digest(&self.config)? == self.config.evaluator_digest, "pinned evaluator changed before receipt signing");
        let receipt = sign_receipt(&self.home, body)?;
        verify_receipt(&self.home, &receipt, &self.config.evaluator_digest)?;
        let view = GateView::proposed(u64::MAX, 1, 1, true).context("no runnable gate predecessor")?;
        ensure!(
            advance(
                view,
                Event::Evaluate {
                    signature_valid: true,
                    evaluator_pinned: receipt.body.evaluator_digest == self.config.evaluator_digest,
                    candidate_bound: receipt.body.candidate_sha == sha
                        && receipt.body.tree_hash == tree_hash
                        && receipt.body.baseline_sha == base_sha,
                    passed: receipt.body.passed,
                },
                Boundary { ceiling: view.ceiling, protected_digest: view.protected_digest },
            )
            .is_some(),
            "verified gate policy refused receipt transition"
        );
        write_json_new(&receipts.join(format!("{id}.json")), &receipt)?;
        let receipt_digest = digest(&serde_json::to_vec(&receipt).context("encode signed receipt")?);
        let record = append_ledger(&self.home, LedgerEvent::Evaluated { id, receipt_digest })?;
        checkpoint::checkpoint(&self.config.origin, &self.config.repository, &self.home, record.seq, &record.hash)?;
        Ok(receipt)
    }

    /// Promote an evaluated proposal only to the private trial ref and gate-owned deploy root.
    ///
    /// # Errors
    /// Invalid receipt, stale base, ref readback, canary or deployment failure.
    pub fn promote(&self, id: &str) -> Result<()> {
        promotion::promote(&self.home, &self.config, id)
    }

    /// Restore the retained predecessor under the gate-owned deployment root.
    ///
    /// # Errors
    /// No runnable predecessor, pointer mismatch or ledger failure.
    pub fn rollback(&self, id: &str) -> Result<()> {
        promotion::rollback(&self.home, &self.config, id)
    }

    fn proposal_for(&self, sha: &str) -> Result<(String, String)> {
        let records = read_ledger(&self.home)?;
        let (id, base) = records
            .iter()
            .find_map(|record| match &record.event {
                LedgerEvent::Proposed { id, base_sha, candidate_sha } if candidate_sha == sha => Some((id.clone(), base_sha.clone())),
                _ => None,
            })
            .context("candidate has no gate proposal")?;
        ensure!(
            !records.iter().any(|record| matches!(&record.event, LedgerEvent::Evaluated { id: existing, .. } | LedgerEvent::Rejected { id: existing, .. } if existing == &id)),
            "candidate already evaluated or rejected"
        );
        Ok((id, base))
    }

    fn run_pinned_bench(&self, clone: &Path, port: u16) -> Result<Vec<u8>> {
        ensure!(current_evaluator_digest(&self.config)? == self.config.evaluator_digest, "protected benchmark runner changed");
        let runner = self.config.evaluator_root.join("bench/run.py");
        let output = clone.join(&self.config.bench_result);
        let spec = if self.config.commands.bench.program == "@pinned-bench-wire" {
            crate::formats::CommandSpec {
                program: "python3".to_owned(),
                args: vec![
                    "-B".to_owned(),
                    runner.to_str().context("bench runner path is not UTF-8")?.to_owned(),
                    "wire".to_owned(),
                    "--harnesses".to_owned(),
                    self.config.bench_harness.clone(),
                    "--out".to_owned(),
                    output.to_str().context("bench result path is not UTF-8")?.to_owned(),
                ],
            }
        } else {
            self.config.commands.bench.clone()
        };
        let sandbox = Sandbox::new(clone, &self.home, &self.config, Network::LoopbackPort { port, listen: true })?;
        sandbox.run(&spec)?;
        let meta = fs::metadata(&output).context("protected wire runner omitted result")?;
        ensure!(meta.is_file() && meta.len() <= 16 * 1024 * 1024, "protected wire result exceeds limit");
        fs::read(&output).context("read protected wire result")
    }

    fn ensure_predecessor(&self, base_clone: &Path, base_sha: &str) -> Result<String> {
        let root = &self.config.deploy_root;
        if !root.exists() {
            fs::create_dir(root).context("create gate deploy root")?;
            fs::set_permissions(root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).context("protect gate deploy root")?;
        }
        let real = fs::canonicalize(root).context("resolve gate deploy root")?;
        ensure!(real.starts_with(fs::canonicalize(&self.home)?), "deploy root resolves outside gate home");
        if let Some(current) = deploy::read_current(root)? {
            return Ok(current);
        }
        let sandbox = Sandbox::new(base_clone, &self.home, &self.config, Network::Off)?;
        sandbox.run(&self.config.commands.canary).context("baseline is not a runnable rollback target")?;
        let tree = trusted("git", &["rev-parse", &format!("{base_sha}^{{tree}}")], &self.config.repository)?;
        validate_sha(&tree)?;
        let artifact = deploy::stage_exact(root, &self.config.repository, base_sha, &tree)?;
        ensure!(deploy::activate(root, &artifact)?.is_none(), "baseline deployment raced another writer");
        ensure!(deploy::read_current(root)?.as_deref() == Some(base_sha), "baseline pointer readback mismatch");
        Ok(base_sha.to_owned())
    }
}

fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid candidate id"
    );
    Ok(())
}

fn kernel_verusfmt(base: &crate::formats::CommandSpec, clone: &Path) -> Result<crate::formats::CommandSpec> {
    let mut files = Vec::new();
    for entry in fs::read_dir(clone.join("crates/aim-kernel/src")).context("kernel source directory missing")? {
        let entry = entry.context("read kernel source entry")?;
        if entry.file_type().context("inspect kernel source entry")?.is_file()
            && entry.path().extension().is_some_and(|extension| extension == "rs")
        {
            let path = entry.path();
            let relative = path.strip_prefix(clone).context("kernel source escapes clone")?;
            files.push(relative.to_str().context("kernel source path is not UTF-8")?.to_owned());
        }
    }
    files.sort();
    ensure!(!files.is_empty(), "kernel source list is empty");
    let mut spec = base.clone();
    spec.args.extend(files);
    Ok(spec)
}

fn protected_suite_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![PathBuf::from("mise.toml"), PathBuf::from("mise.lock")];
    let bench = root.join("bench");
    for entry in fs::read_dir(&bench).context("protected bench directory missing")? {
        let entry = entry.context("read protected bench entry")?;
        if entry.file_type().context("inspect protected bench entry")?.is_file() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".py") || name.to_string_lossy().ends_with(".toml") {
                files.push(PathBuf::from("bench").join(name));
            }
        }
    }
    files.sort();
    ensure!(
        files.contains(&PathBuf::from("bench/run.py"))
            && files.contains(&PathBuf::from("bench/proxy.py"))
            && files.contains(&PathBuf::from("bench/manifest.toml")),
        "protected bench suite incomplete"
    );
    Ok(files)
}
