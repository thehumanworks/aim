//! Optional Jev relevance scores for redacted, persistent conversation hits.
//!
//! The search index excludes private and ephemeral sessions. This adapter sends only bounded,
//! freshly redacted query text and snippets, never session IDs or workspace paths.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use typesafe_jev::{Client, Config, Noul, Questions};

use super::Hit;
use super::tools::Reranker;
use crate::agent::tools::BoxFuture;

const MAX_CANDIDATES: usize = 30;
const MAX_QUERY_CHARS: usize = 4_000;
const MAX_SNIPPET_CHARS: usize = 1_200;
const DEADLINE: Duration = Duration::from_secs(2);

/// A batched Jev reranker for persistent search results.
pub struct JevReranker;

impl Reranker for JevReranker {
    fn rerank(&self, query: String, candidates: Vec<Hit>) -> BoxFuture<Result<Vec<f64>, String>> {
        Box::pin(async move {
            if candidates.is_empty() {
                return Ok(Vec::new());
            }
            if std::env::var_os("TYPESAFE_API_KEY").is_none() {
                return Err("Jev key is unavailable".to_owned());
            }
            let task = tokio::task::spawn_blocking(move || {
                let config = Config {
                    connect_timeout: Duration::from_millis(500),
                    timeout: Duration::from_millis(1_500),
                    max_retries: 0,
                    pool_size: 1,
                    ..Config::default()
                };
                let client = Client::from_env(config).map_err(|_| "Jev client is unavailable".to_owned())?;
                ask(&client, &query, &candidates)
            });
            tokio::time::timeout(DEADLINE, task)
                .await
                .map_err(|_| "Jev rerank timed out".to_owned())?
                .map_err(|_| "Jev rerank task failed".to_owned())?
        })
    }
}

fn bounded_redacted(text: &str, max_chars: usize) -> String {
    aim_acp::redact::redact(text).chars().take(max_chars).collect()
}

fn ask(client: &Client, query: &str, candidates: &[Hit]) -> Result<Vec<f64>, String> {
    let count = candidates.len().min(MAX_CANDIDATES);
    let mut excerpts = BTreeMap::new();
    let mut questions = Questions::new();
    for (index, hit) in candidates.iter().take(count).enumerate() {
        let id = format!("c{index}");
        excerpts.insert(id.clone(), bounded_redacted(&hit.snippet, MAX_SNIPPET_CHARS));
        questions
            .insert(id.clone(), Noul::new(format!("Would `candidates.{id}` give information that materially helps the current task?")));
    }
    let state = json!({
        "task": "decide which past-conversation excerpts help the current task",
        "current": bounded_redacted(query, MAX_QUERY_CHARS),
        "candidates": excerpts,
    });
    let response = client.ask(&state, &questions).map_err(|_| "Jev rerank request failed".to_owned())?;
    let mut scores = Vec::with_capacity(candidates.len());
    for index in 0..count {
        let id = format!("c{index}");
        let score = response.noul(&id).ok_or("Jev rerank answer is incomplete")?.noul;
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err("Jev rerank answer is invalid".to_owned());
        }
        scores.push(score);
    }
    // Unsent results have no Jev relevance score.
    scores.resize(candidates.len(), 0.0);
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use typesafe_jev::{Client, Config, Reply};

    use super::{MAX_CANDIDATES, ask};
    use crate::search::Hit;

    fn hit(index: usize, snippet: &str) -> Hit {
        Hit {
            session: format!("session-{index}"),
            seq: 1,
            turn: 1,
            time_ms: 0,
            kind: "user".to_owned(),
            workspace: "/private/workspace".to_owned(),
            snippet: snippet.to_owned(),
            score: 0.1,
        }
    }

    #[test]
    fn one_request_contains_only_bounded_redacted_text_and_thirty_questions() {
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let saved = Arc::clone(&captured);
        let client = Client::with_transport(
            move |body: &[u8]| {
                let request: Value = serde_json::from_slice(body).unwrap();
                saved.lock().unwrap().push(request);
                let answers = (0..MAX_CANDIDATES)
                    .map(|index| {
                        let score = if index % 2 == 0 { 0.8 } else { 0.2 };
                        (format!("c{index}"), json!({"type":"noul","noul":score}))
                    })
                    .collect::<serde_json::Map<_, _>>();
                Ok(Reply { status: 200, retry_after: None, body: json!({"model":"fake","answers":answers}).to_string() })
            },
            Config::default(),
        );
        let candidates = (0..35).map(|index| hit(index, "password: hunter2 useful context")).collect::<Vec<_>>();
        let scores = ask(&client, "Bearer secret-token", &candidates).unwrap();
        assert_eq!(scores.len(), candidates.len());
        assert_eq!(scores.first(), Some(&0.8));
        assert_eq!(scores.get(1), Some(&0.2));
        assert!(scores.iter().skip(MAX_CANDIDATES).all(|score| *score == 0.0));
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request["questions"].as_object().unwrap().len(), MAX_CANDIDATES);
        assert_eq!(request["state"]["candidates"].as_object().unwrap().len(), MAX_CANDIDATES);
        let wire = request.to_string();
        assert!(!wire.contains("hunter2"));
        assert!(!wire.contains("secret-token"));
        assert!(!wire.contains("/private/workspace"));
        assert!(!wire.contains("session-0"));
    }

    #[test]
    fn outbound_text_is_bounded_by_unicode_characters() {
        assert_eq!(super::bounded_redacted(&"é".repeat(1_500), 1_200).chars().count(), 1_200);
    }

    #[test]
    fn missing_or_invalid_answer_fails_closed() {
        for answer in [None, Some(json!({"type":"noul","noul":1.5}))] {
            let client = Client::with_transport(
                move |_body: &[u8]| {
                    let answers =
                        answer.clone().map_or_else(serde_json::Map::new, |value| serde_json::Map::from_iter([("c0".to_owned(), value)]));
                    Ok(Reply { status: 200, retry_after: None, body: json!({"model":"fake","answers":answers}).to_string() })
                },
                Config::default(),
            );
            assert!(ask(&client, "query", &[hit(0, "snippet")]).is_err());
        }
    }

    /// Exercises the hosted Jev service when the key is available.
    #[tokio::test]
    #[ignore = "requires TYPESAFE_API_KEY and live TypeSafe service"]
    async fn live_jev_search_rerank() {
        use super::JevReranker;
        use crate::search::tools::Reranker;

        let scores = JevReranker
            .rerank(
                "How do I inspect a Rust Vec length?".to_owned(),
                vec![hit(0, "In Rust, Vec::len returns the number of elements."), hit(1, "The weather was rainy today.")],
            )
            .await
            .unwrap();
        assert_eq!(scores.len(), 2);
        assert!(scores.iter().all(|score| (0.0..=1.0).contains(score)));
    }
}
