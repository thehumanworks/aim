//! Paired `bench:wire` evidence parsing; absent, unmatched or regressed samples fail closed.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;

use crate::formats::{BenchResults, Thresholds};

#[derive(Deserialize)]
struct WireFile {
    tier: String,
    runs: Vec<WireRun>,
}

#[derive(Deserialize)]
struct WireRun {
    harness: String,
    case: String,
    repetition: u32,
    passed: bool,
    first_request_bytes: Option<f64>,
    first_request_ms: Option<f64>,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireUsage {
    cost_usd: Option<f64>,
}

fn selected(bytes: &[u8], harness: &str) -> Result<BTreeMap<(String, u32), WireRun>> {
    let file: WireFile = serde_json::from_slice(bytes).context("decode wire benchmark")?;
    ensure!(file.tier == "wire", "benchmark artifact is not the wire tier");
    let mut rows = BTreeMap::new();
    for run in file.runs.into_iter().filter(|run| run.harness == harness) {
        let key = (run.case.clone(), run.repetition);
        ensure!(rows.insert(key, run).is_none(), "duplicate wire benchmark sample");
    }
    ensure!(rows.len() >= 2, "wire benchmark needs at least two paired runs");
    Ok(rows)
}

fn median(values: &mut [f64]) -> Result<f64> {
    ensure!(!values.is_empty() && values.iter().all(|value| value.is_finite() && *value >= 0.0), "missing or invalid benchmark metric");
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        let left = values.get(middle.saturating_sub(1)).copied().context("median left")?;
        let right = values.get(middle).copied().context("median right")?;
        Ok(f64::midpoint(left, right))
    } else {
        values.get(middle).copied().context("median sample")
    }
}

/// Compare matching baseline/candidate samples against thresholds fixed before evaluation.
///
/// # Errors
/// Missing, malformed, unmatched, or regressed evidence.
pub fn compare(base: &[u8], candidate: &[u8], harness: &str, limits: &Thresholds) -> Result<BenchResults> {
    let base = selected(base, harness)?;
    let candidate = selected(candidate, harness)?;
    ensure!(base.keys().eq(candidate.keys()), "wire benchmark cases or repetitions differ");
    let mut base_bytes = Vec::new();
    let mut candidate_bytes = Vec::new();
    let mut base_ms = Vec::new();
    let mut candidate_ms = Vec::new();
    let mut base_cost = 0.0;
    let mut candidate_cost = 0.0;
    let mut base_passes = 0_usize;
    let mut candidate_passes = 0_usize;
    for (key, left) in &base {
        let right = candidate.get(key).context("paired benchmark sample missing")?;
        base_bytes.push(left.first_request_bytes.context("baseline first request bytes missing")?);
        candidate_bytes.push(right.first_request_bytes.context("candidate first request bytes missing")?);
        base_ms.push(left.first_request_ms.context("baseline first request timing missing")?);
        candidate_ms.push(right.first_request_ms.context("candidate first request timing missing")?);
        base_cost += left.usage.cost_usd.context("baseline cost missing")?;
        candidate_cost += right.usage.cost_usd.context("candidate cost missing")?;
        base_passes += usize::from(left.passed);
        candidate_passes += usize::from(right.passed);
    }
    ensure!(base_cost.is_finite() && candidate_cost.is_finite() && base_cost >= 0.0 && candidate_cost >= 0.0, "invalid benchmark cost");
    let count = base.len();
    let count_u32 = u32::try_from(count).context("too many benchmark samples")?;
    let base_passes_u32 = u32::try_from(base_passes).context("too many baseline passes")?;
    let candidate_passes_u32 = u32::try_from(candidate_passes).context("too many candidate passes")?;
    let results = BenchResults {
        baseline_pass_rate: f64::from(base_passes_u32) / f64::from(count_u32),
        candidate_pass_rate: f64::from(candidate_passes_u32) / f64::from(count_u32),
        baseline_first_request_bytes_p50: median(&mut base_bytes)?,
        candidate_first_request_bytes_p50: median(&mut candidate_bytes)?,
        baseline_first_request_ms_p50: median(&mut base_ms)?,
        candidate_first_request_ms_p50: median(&mut candidate_ms)?,
        baseline_cost_usd: Some(base_cost),
        candidate_cost_usd: Some(candidate_cost),
        repetitions: count,
    };
    ensure!(base_passes == count && candidate_passes == count, "wire quality gate failed");
    ensure!(
        results.candidate_pass_rate * 100.0 + limits.max_quality_drop_pp >= results.baseline_pass_rate * 100.0,
        "wire quality regressed"
    );
    ensure!(
        results.candidate_first_request_bytes_p50
            <= results.baseline_first_request_bytes_p50 * (1.0 + limits.max_first_request_bytes_increase_pct / 100.0),
        "first request bytes regressed"
    );
    ensure!(
        results.candidate_first_request_ms_p50
            <= results.baseline_first_request_ms_p50 * (1.0 + limits.max_first_request_p50_increase_pct / 100.0),
        "first request timing regressed"
    );
    ensure!(candidate_cost <= base_cost * (1.0 + limits.max_cost_increase_pct / 100.0), "wire cost regressed");
    Ok(results)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn file(bytes: [u64; 2]) -> Vec<u8> {
        serde_json::to_vec(&json!({"tier":"wire","runs":[
            {"harness":"aim_openrouter","case":"one","repetition":0,"passed":true,"first_request_bytes":bytes[0],"first_request_ms":10.0,"usage":{"cost_usd":0.0}},
            {"harness":"aim_openrouter","case":"one","repetition":1,"passed":true,"first_request_bytes":bytes[1],"first_request_ms":10.0,"usage":{"cost_usd":0.0}}
        ]})).unwrap()
    }

    #[test]
    fn threshold_regression_and_missing_metrics_are_rejected() {
        let baseline = file([100, 100]);
        assert!(compare(&baseline, &file([101, 101]), "aim_openrouter", &Thresholds::default()).is_ok());
        assert!(compare(&baseline, &file([103, 103]), "aim_openrouter", &Thresholds::default()).is_err());
        let mut missing: serde_json::Value = serde_json::from_slice(&file([100, 100])).unwrap();
        missing["runs"][0]["first_request_ms"] = serde_json::Value::Null;
        assert!(compare(&baseline, &serde_json::to_vec(&missing).unwrap(), "aim_openrouter", &Thresholds::default()).is_err());
    }
}
