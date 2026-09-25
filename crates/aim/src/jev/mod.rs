//! Bounded, optional Jev advice for the native loop (docs/adr/0013).
//!
//! Every question in a step bundle shares one short state. A missing key, invalid reply or late
//! answer leaves the effort unchanged. Blocking HTTP stays on Tokio's blocking pool.
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use typesafe_jev::{Client, Config, Noul, Questions, Score};

/// Deadline for a step bundle; tool completion never waits for it.
pub const DEADLINE: Duration = Duration::from_secs(2);

/// One decision's bounded input. `state` contains no native provider payloads or tool arguments.
#[derive(Clone, Debug)]
pub struct Bundle {
    /// Sanitized digest of the user's goal and recent transcript.
    pub state: String,
    /// Selected model's ordered reasoning efforts.
    pub ladder: Vec<String>,
}

/// Validated, quantized answer to one step bundle.
#[derive(Clone, Debug)]
pub struct Advice {
    /// Raw ordinal score, retained only for offline analysis.
    pub raw_score: f64,
    /// Raw confidence, retained only for offline analysis.
    pub raw_confidence: f64,
    /// Raw per-level probabilities, in ladder order.
    pub raw_probabilities: Vec<f64>,
    /// Raw stuck, progress and past-session probabilities.
    pub raw_noul: [f64; 3],
    /// Score position on the whole ladder, in basis points.
    pub proposed_bp: u32,
    /// Stuck, progress and past-session probabilities in basis points.
    pub noul_bp: [u32; 3],
    /// End-to-end latency.
    pub latency_ms: u64,
    /// Input tokens reported by Jev.
    pub input_tokens: Option<u64>,
    /// Estimated list-price micro-US dollars.
    pub cost_micro_usd: Option<u64>,
}

/// An owned future for one bounded advice request.
pub type DecisionFuture = Pin<Box<dyn Future<Output = Option<Advice>> + Send>>;

/// An optional external adviser. `None` always means keep the effort in force.
pub trait Decider: Send + Sync {
    /// Start one step bundle and return when it succeeds, fails, or reaches its deadline.
    fn decide(&self, bundle: Bundle) -> DecisionFuture;
}

/// Deterministic fallback for private sessions, absent credentials and failed advice.
pub struct FallbackDecider;

impl Decider for FallbackDecider {
    fn decide(&self, _bundle: Bundle) -> DecisionFuture {
        Box::pin(async { None })
    }
}

/// `TypeSafe`'s blocking Jev client, confined to `spawn_blocking`.
pub struct JevDecider;

impl Decider for JevDecider {
    fn decide(&self, bundle: Bundle) -> DecisionFuture {
        Box::pin(async move {
            let started = Instant::now();
            let task = tokio::task::spawn_blocking(move || ask(&bundle));
            tokio::time::timeout(DEADLINE, task).await.ok()?.ok()?.map(|mut advice| {
                advice.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                advice
            })
        })
    }
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "finite input is clamped to 0..=1 before scaling")]
fn basis_points(value: f64) -> Option<u32> {
    if !value.is_finite() {
        return None;
    }
    Some((value.clamp(0.0, 1.0) * 10_000.0).round() as u32)
}

fn ask(bundle: &Bundle) -> Option<Advice> {
    if !(2..=10).contains(&bundle.ladder.len()) || bundle.state.is_empty() {
        return None;
    }
    let config = Config {
        connect_timeout: Duration::from_millis(500),
        timeout: Duration::from_millis(1_500),
        max_retries: 0,
        pool_size: 1,
        ..Config::default()
    };
    // The key is read only here and is never included in the digest, event, error or logs.
    let client = Client::from_env(config).ok()?;
    let last = bundle.ladder.len().saturating_sub(1);
    let levels = bundle
        .ladder
        .iter()
        .enumerate()
        .map(|(position, name)| format!("{name}: reasoning effort {position} of {last}, from quickest to most thorough"));
    let questions = Questions::new()
        .with("effort", Score::new("What reasoning effort should the next model request use?", levels))
        .with("stuck", Noul::new("Is the agent stuck or repeating an unproductive approach?"))
        .with("progress", Noul::new("Is the agent making useful progress toward the user's goal?"))
        .with("past_sessions", Noul::new("Would relevant past sessions help with the current goal?"));
    let response = client.ask(&bundle.state, &questions).ok()?;
    let score = response.score("effort")?;
    if !score.score.is_finite() || !score.confidence.is_finite() || score.probabilities.len() != bundle.ladder.len() {
        return None;
    }
    let mut sum = 0.0;
    let mut raw_probabilities = Vec::with_capacity(bundle.ladder.len());
    for (position, probability) in &score.probabilities {
        if usize::try_from(*position).ok()? >= bundle.ladder.len() || !probability.is_finite() {
            return None;
        }
        sum += probability.clamp(0.0, 1.0);
        raw_probabilities.push(*probability);
    }
    if !(0.99..=1.01).contains(&sum) {
        return None;
    }
    let score_position = (score.score / f64::from(u32::try_from(last).ok()?)).clamp(0.0, 1.0);
    let proposed_bp = basis_points(score_position)?;
    let raw_noul = ["stuck", "progress", "past_sessions"].map(|id| response.noul(id).map(|answer| answer.noul));
    let [Some(stuck_raw), Some(progress_raw), Some(past_sessions_raw)] = raw_noul else {
        return None;
    };
    let raw_noul = [stuck_raw, progress_raw, past_sessions_raw];
    let noul_bp = raw_noul.map(basis_points);
    let [Some(stuck), Some(progress), Some(past_sessions)] = noul_bp else {
        return None;
    };
    let input_tokens = response.usage.input_tokens;
    let cost_micro_usd = input_tokens.map(|tokens| tokens.saturating_mul(42).saturating_add(500) / 1_000);
    Some(Advice {
        raw_score: score.score,
        raw_confidence: score.confidence.clamp(0.0, 1.0),
        raw_probabilities,
        raw_noul,
        proposed_bp,
        noul_bp: [stuck, progress, past_sessions],
        latency_ms: 0,
        input_tokens,
        cost_micro_usd,
    })
}

/// Create a bounded digest. Returns `None` if a known credential marker appears; no content is
/// sent in that case. Native payloads, tool arguments and image bytes never enter `recent`.
#[must_use]
pub fn digest(goal: &str, recent: &[String], counts: (usize, usize)) -> Option<String> {
    let mut state = format!("Goal: {}\nItems: {}, tool calls: {}\n", safe_text(goal, 800)?, counts.0, counts.1);
    for item in recent.iter().rev().take(8).rev() {
        state.push_str(&safe_text(item, 320)?);
        state.push('\n');
    }
    Some(state)
}

fn safe_text(text: &str, limit: usize) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if [
        "password",
        "api_key",
        "api key",
        "secret",
        "token",
        "bearer",
        "authorization",
        "credential",
        "private key",
        "sk-",
        "sk_",
        "ghp_",
        "akia",
        "asia",
        "eyj",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || text.split_whitespace().any(|word| word.chars().filter(char::is_ascii_alphanumeric).count() >= 24)
    {
        return None;
    }
    let cut = text.char_indices().nth(limit).map_or(text.len(), |(index, _)| index);
    Some(text.get(..cut)?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Decider, FallbackDecider, basis_points, digest};

    #[tokio::test]
    async fn fallback_and_digest() {
        let bundle = super::Bundle { state: "goal".into(), ladder: vec!["low".into(), "high".into()] };
        assert!(FallbackDecider.decide(bundle).await.is_none());
        assert!(digest("use Bearer abc", &[], (1, 0)).is_none());
        assert!(digest("goal", &["result metadata".into()], (2, 1)).is_some());
        assert_eq!(basis_points(1.2), Some(10_000));
        assert_eq!(basis_points(f64::NAN), None);
    }

    /// Calls the real hosted Jev service when `TYPESAFE_API_KEY` is available.
    #[tokio::test]
    #[ignore = "requires TYPESAFE_API_KEY and live TypeSafe service"]
    async fn live_jev_batched_bundle() {
        let bundle = super::Bundle {
            state: "Goal: explain a short Rust function. Items: 1, tool calls: 1".into(),
            ladder: vec!["low".into(), "medium".into(), "high".into()],
        };
        let advice = super::JevDecider.decide(bundle).await;
        assert!(advice.is_some(), "live Jev step bundle did not complete within its deadline");
        if let Some(advice) = advice {
            assert!(advice.proposed_bp <= 10_000);
            assert!(advice.input_tokens.is_some());
            eprintln!(
                "live Jev bundle: latency_ms={}, input_tokens={:?}, cost_micro_usd={:?}",
                advice.latency_ms, advice.input_tokens, advice.cost_micro_usd
            );
        }
    }
}
