//! Receipt-bound promotion to the private trial ref and a gate-owned deployment.

use std::fs;
use std::path::{Path, PathBuf};

use aim_kernel::gate::{Boundary, Event, GateView, Stage, advance as gate_advance};
use anyhow::{Context as _, Result, anyhow, ensure};

use crate::checkpoint::{checkpoint, push_with_readback, remote_ref};
use crate::deploy::{self, Artifact};
use crate::formats::{
    GateConfig, LedgerEvent, LedgerRecord, SignedReceipt, append_ledger, current_environment_digest, current_evaluator_digest, digest,
    read_json, read_ledger, verify_receipt,
};
use crate::runner::{Sandbox, fresh_checkout, trusted, validate_sha};
use crate::sandbox::Network;

const TRIAL_REF: &str = "refs/heads/gate/trial";
const MAIN_REF: &str = "refs/heads/main";

struct Candidate {
    base: String,
    sha: String,
    tree: String,
    receipt: SignedReceipt,
}

fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid candidate id"
    );
    Ok(())
}

fn token(hex_digest: &str) -> Result<u64> {
    let head = hex_digest.get(..16).context("digest is too short")?;
    Ok(u64::from_str_radix(head, 16).context("invalid digest identity")?.max(1))
}

fn advance(view: GateView, event: Event, boundary: Boundary) -> Result<GateView> {
    gate_advance(view, event, boundary).context("kernel refused gate transition")
}

fn anchor(home: &Path, config: &GateConfig, event: LedgerEvent) -> Result<LedgerRecord> {
    let record = append_ledger(home, event)?;
    checkpoint(&config.origin, &config.repository, home, record.seq, &record.hash)?;
    Ok(record)
}

fn find_candidate(home: &Path, config: &GateConfig, id: &str) -> Result<Candidate> {
    let records = read_ledger(home)?;
    let (base, sha) = records
        .iter()
        .find_map(|record| match &record.event {
            LedgerEvent::Proposed { id: candidate_id, base_sha, candidate_sha } if candidate_id == id => {
                Some((base_sha.clone(), candidate_sha.clone()))
            }
            _ => None,
        })
        .context("candidate has no proposal")?;
    ensure!(
        !records.iter().any(|record| matches!(&record.event,
            LedgerEvent::Rejected { id: candidate_id, .. }
            | LedgerEvent::Merged { id: candidate_id, .. }
            | LedgerEvent::Pushed { id: candidate_id, .. }
            | LedgerEvent::Deployed { id: candidate_id, .. }
            | LedgerEvent::Canary { id: candidate_id }
            | LedgerEvent::Active { id: candidate_id }
            | LedgerEvent::RolledBack { id: candidate_id, .. }
            | LedgerEvent::Failed { id: candidate_id, .. } if candidate_id == id)),
        "candidate is already promoted, rejected, or failed"
    );
    let receipt_hash = records
        .iter()
        .find_map(|record| match &record.event {
            LedgerEvent::Evaluated { id: candidate_id, receipt_digest } if candidate_id == id => Some(receipt_digest.as_str()),
            _ => None,
        })
        .context("candidate has no evaluation")?;
    let receipt: SignedReceipt = read_json(&home.join("receipts").join(format!("{id}.json")))?;
    let serialized = serde_json::to_vec(&receipt).context("serialize signed receipt")?;
    ensure!(digest(&serialized) == receipt_hash, "ledger receipt digest mismatch");
    verify_receipt(home, &receipt, &config.evaluator_digest)?;
    ensure!(receipt.body.candidate_sha == sha && receipt.body.baseline_sha == base, "receipt is bound to another proposal");
    ensure!(receipt.body.passed, "receipt is not passing");
    validate_sha(&base)?;
    validate_sha(&sha)?;
    validate_sha(&receipt.body.tree_hash)?;
    Ok(Candidate { base, sha, tree: receipt.body.tree_hash.clone(), receipt })
}

fn verify_source(config: &GateConfig, candidate: &Candidate) -> Result<()> {
    ensure!(current_evaluator_digest(config)? == config.evaluator_digest, "running evaluator is not pinned");
    ensure!(
        remote_ref(&config.repository, &config.origin, MAIN_REF)?.as_deref() == Some(&candidate.base),
        "main moved since candidate evaluation"
    );
    let ref_tree = format!("{}^{{tree}}", candidate.sha);
    ensure!(trusted("git", &["rev-parse", &ref_tree], &config.repository)? == candidate.tree, "candidate Git tree differs from receipt");
    trusted("git", &["merge-base", "--is-ancestor", &candidate.base, &candidate.sha], &config.repository)
        .context("candidate is not descended from the evaluated main base")?;
    let schema_diff = trusted(
        "git",
        &["diff", "--name-only", &candidate.base, &candidate.sha, "--", "crates/aim/src/store", "crates/aim/src/search/index.rs"],
        &config.repository,
    )?;
    ensure!(schema_diff.is_empty(), "store schema or migration code changed without expand/contract proof");
    Ok(())
}

fn private_deploy_root(home: &Path, config: &GateConfig) -> Result<PathBuf> {
    let canonical_home = fs::canonicalize(home).context("canonicalize gate home")?;
    let root = fs::canonicalize(&config.deploy_root).context("canonicalize gate deployment root")?;
    ensure!(root.starts_with(&canonical_home) && root != canonical_home, "deployment root escapes gate home");
    Ok(root)
}

fn clone_at(config: &GateConfig, home: &Path, sha: &str, destination: &Path) -> Result<()> {
    fresh_checkout(&config.repository, sha, destination, home, config)
}

fn canary(config: &GateConfig, home: &Path, clone: &Path) -> Result<()> {
    let sandbox = Sandbox::new(clone, home, config, Network::Off)?;
    sandbox.run(&config.commands.canary)?;
    Ok(())
}

fn stage_candidate(home: &Path, config: &GateConfig, root: &Path, candidate: &Candidate, temporary: &Path) -> Result<(Artifact, PathBuf)> {
    let clone = temporary.join("candidate-deploy");
    clone_at(config, home, &candidate.sha, &clone)?;
    let artifact = deploy::stage_exact(root, &config.repository, &candidate.sha, &candidate.tree)?;
    Ok((artifact, clone))
}

/// Promote one signed candidate to `gate/trial`, then canary and activate its exact artifact.
/// The private live installation is never a destination.
///
/// # Errors
/// Missing/forged evidence, stale main or trial refs, schema change, failed canary, or unsafe deploy.
pub fn promote(home: &Path, config: &GateConfig, id: &str) -> Result<()> {
    valid_id(id)?;
    let candidate = find_candidate(home, config, id)?;
    verify_source(config, &candidate)?;
    let temporary = tempfile::Builder::new().prefix("aim-gate-promote-").tempdir().context("create promotion clones")?;
    let root = private_deploy_root(home, config)?;
    let predecessor = deploy::read_current(&root)?.context("no runnable rollback target is deployed")?;
    ensure!(candidate.receipt.body.rollback_sha == predecessor, "receipt rollback target differs from current deployment");
    let boundary = Boundary { ceiling: 0, protected_digest: token(&config.evaluator_digest)? };
    let mut view = GateView::proposed(boundary.ceiling, boundary.protected_digest, token(&predecessor)?, true)
        .context("kernel refused proposal without a rollback target")?;
    view = advance(view, Event::Evaluate { signature_valid: true, evaluator_pinned: true, candidate_bound: true, passed: true }, boundary)?;
    let mut failed_step = "merge";
    let mut pointer_swapped = false;
    let result = (|| -> Result<()> {
        let previous_trial = remote_ref(&config.repository, &config.origin, TRIAL_REF)?;
        ensure!(
            remote_ref(&config.repository, &config.origin, MAIN_REF)?.as_deref() == Some(&candidate.base),
            "main changed before trial merge"
        );
        view = advance(view, Event::Merge { base_cas: true, tree_matches: true }, boundary)?;
        anchor(home, config, LedgerEvent::Merged { id: id.to_owned(), sha: candidate.sha.clone() })?;
        failed_step = "push";
        push_with_readback(&config.repository, &config.origin, TRIAL_REF, &candidate.sha, previous_trial.as_deref())?;
        view = advance(view, Event::Push { remote_readback: true }, boundary)?;
        anchor(home, config, LedgerEvent::Pushed { id: id.to_owned(), sha: candidate.sha.clone() })?;
        failed_step = "deploy";
        let (artifact, clone) = stage_candidate(home, config, &root, &candidate, temporary.path())?;
        ensure!(artifact.expected_current.as_deref() == Some(&predecessor), "deployment predecessor changed during promotion");
        let environment_digest = current_environment_digest(config, home)?;
        ensure!(environment_digest == candidate.receipt.body.environment_digest, "runtime environment differs from evaluated environment");
        view = advance(
            view,
            Event::Deploy { exact_sha: true, digests_match: true, predecessor_runnable: true, predecessor_matches: true },
            boundary,
        )?;
        anchor(home, config, LedgerEvent::Deployed { id: id.to_owned(), sha: candidate.sha.clone(), predecessor: predecessor.clone() })?;
        failed_step = "canary";
        canary(config, home, &clone).context("candidate offline canary failed")?;
        view = advance(view, Event::Canary { passed: true }, boundary)?;
        anchor(home, config, LedgerEvent::Canary { id: id.to_owned() })?;
        failed_step = "activate";
        let observed_predecessor = deploy::activate(&root, &artifact)?;
        pointer_swapped = true;
        ensure!(observed_predecessor.as_deref() == Some(&predecessor), "deployment predecessor changed during activation");
        ensure!(deploy::read_current(&root)?.as_deref() == Some(&candidate.sha), "active pointer readback mismatch");
        let _active = advance(view, Event::Activate { pointer_readback: true }, boundary)?;
        anchor(home, config, LedgerEvent::Active { id: id.to_owned() })?;
        Ok(())
    })();
    if let Err(error) = result {
        let failed_anchor = anchor(home, config, LedgerEvent::Failed { id: id.to_owned(), step: failed_step.to_owned() }).err();
        let current = deploy::read_current(&root);
        let restore = (|| -> Result<()> {
            match current {
                Ok(Some(current)) if current == candidate.sha => {
                    deploy::rollback(&root, &predecessor)?;
                    ensure!(deploy::read_current(&root)?.as_deref() == Some(&predecessor), "promotion rollback readback failed");
                    anchor(home, config, LedgerEvent::RolledBack { id: id.to_owned(), predecessor: predecessor.clone() })?;
                    Ok(())
                }
                Ok(Some(current)) if current == predecessor && !pointer_swapped => Ok(()),
                Ok(other) => Err(anyhow!("promotion pointer changed to an unexpected artifact: {other:?}")),
                Err(read_error) => Err(read_error.context("cannot establish promotion pointer after failure")),
            }
        })();
        if let Err(restore_error) = restore {
            return Err(anyhow!("promotion failed at {failed_step}: {error}; recovery failed: {restore_error}"));
        }
        if let Some(anchor_error) = failed_anchor {
            return Err(anyhow!("promotion failed at {failed_step}: {error}; failure checkpoint failed: {anchor_error}"));
        }
        return Err(error);
    }
    Ok(())
}

/// Restore the predecessor recorded for a candidate that reached the active phase.
///
/// # Errors
/// No matching active deployment, missing predecessor, changed pointer, or failed ledger anchor.
pub fn rollback(home: &Path, config: &GateConfig, id: &str) -> Result<()> {
    valid_id(id)?;
    let records = read_ledger(home)?;
    let (candidate, predecessor) = records
        .iter()
        .find_map(|record| match &record.event {
            LedgerEvent::Deployed { id: candidate_id, sha, predecessor } if candidate_id == id => Some((sha.clone(), predecessor.clone())),
            _ => None,
        })
        .context("candidate has no deployed artifact")?;
    ensure!(
        records.iter().any(|record| matches!(&record.event, LedgerEvent::Active { id: candidate_id } if candidate_id == id)),
        "candidate never became active"
    );
    ensure!(
        !records.iter().any(|record| matches!(&record.event, LedgerEvent::RolledBack { id: candidate_id, .. } if candidate_id == id)),
        "candidate was already rolled back"
    );
    let root = private_deploy_root(home, config)?;
    ensure!(deploy::read_current(&root)?.as_deref() == Some(&candidate), "active deployment pointer changed");
    deploy::rollback(&root, &predecessor)?;
    ensure!(deploy::read_current(&root)?.as_deref() == Some(&predecessor), "rollback pointer readback mismatch");
    let boundary = Boundary { ceiling: 0, protected_digest: token(&config.evaluator_digest)? };
    let active = GateView {
        stage: Stage::Active,
        receipt_pinned: true,
        rollback_runnable: true,
        rollback_target: token(&predecessor)?,
        ceiling: boundary.ceiling,
        protected_digest: boundary.protected_digest,
    };
    let _rolled_back = advance(active, Event::Rollback { predecessor_readback: true }, boundary)?;
    anchor(home, config, LedgerEvent::RolledBack { id: id.to_owned(), predecessor })?;
    Ok(())
}
