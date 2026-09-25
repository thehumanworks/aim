//! Rate-limit windows from the `x-<family>-{primary,secondary}-*` response headers and from
//! in-stream `codex.rate_limits` events (refs:codex-api/src/rate_limits.rs:27-230; research §5).
//!
//! Every metered family is reported (the default `codex` family and e.g. `codex-other`), each
//! window with the id `<family>.<window>` so two families never collide and the two windows of a
//! family are never collapsed.

use std::collections::BTreeSet;

use aim_proto::conversation::{RateLimitWindow, RateLimits};
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

const WINDOWS: [&str; 2] = ["primary", "secondary"];
const DEFAULT_FAMILY: &str = "codex";

/// Rate limits from response headers; `None` when the response carries none.
pub(crate) fn from_headers(headers: &HeaderMap, now: i64) -> Option<RateLimits> {
    let mut families = BTreeSet::new();
    for name in headers.keys() {
        let Some(rest) = name.as_str().strip_prefix("x-") else { continue };
        for window in WINDOWS {
            if let Some(family) = rest.strip_suffix(&format!("-{window}-used-percent")).filter(|family| !family.is_empty()) {
                families.insert(family.to_owned());
            }
        }
    }
    // The default family first, then the others in name order.
    let ordered = families.iter().filter(|f| *f == DEFAULT_FAMILY).chain(families.iter().filter(|f| *f != DEFAULT_FAMILY));
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let mut windows = Vec::new();
    for family in ordered {
        for window in WINDOWS {
            let prefix = format!("x-{family}-{window}-");
            let Some(used_percent) = text(&format!("{prefix}used-percent")).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite())
            else {
                continue;
            };
            let window_minutes = text(&format!("{prefix}window-minutes")).and_then(|v| v.parse::<u64>().ok()).filter(|m| *m > 0);
            let resets_at = text(&format!("{prefix}reset-at")).and_then(|v| v.parse::<i64>().ok()).or_else(|| {
                text(&format!("{prefix}reset-after-seconds"))
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|seconds| *seconds > 0)
                    .map(|seconds| now.saturating_add(seconds))
            });
            // An all-zero window is the server's "not applicable" (codex `has_data`).
            if used_percent > 0.0 || window_minutes.is_some() || resets_at.is_some() {
                windows.push(RateLimitWindow { id: format!("{family}.{window}"), used_percent, window_minutes, resets_at });
            }
        }
    }
    let mut native = Map::new();
    for (name, value) in headers {
        let key = name.as_str();
        let ours = key.starts_with("x-codex-") || families.iter().any(|family| key.starts_with(&format!("x-{family}-")));
        if ours && let Ok(value) = value.to_str() {
            native.insert(key.to_owned(), json!(value));
        }
    }
    if windows.is_empty() && native.is_empty() {
        return None;
    }
    Some(RateLimits { windows, native: (!native.is_empty()).then_some(Value::Object(native)) })
}

/// Rate limits from an in-stream `codex.rate_limits` event (refs:codex-api/src/rate_limits.rs:135-170).
pub(crate) fn from_event(value: &Value) -> RateLimits {
    let family = value
        .get("metered_limit_name")
        .or_else(|| value.get("limit_name"))
        .and_then(Value::as_str)
        .map(|name| name.trim().to_ascii_lowercase().replace('_', "-"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_FAMILY.to_owned());
    let details = value.get("rate_limits");
    let windows = WINDOWS
        .iter()
        .filter_map(|window| {
            let entry = details?.get(*window)?;
            Some(RateLimitWindow {
                id: format!("{family}.{window}"),
                used_percent: entry.get("used_percent").and_then(Value::as_f64).filter(|v| v.is_finite())?,
                window_minutes: entry.get("window_minutes").and_then(Value::as_u64).filter(|m| *m > 0),
                resets_at: entry.get("reset_at").or_else(|| entry.get("resets_at")).and_then(Value::as_i64),
            })
        })
        .collect();
    RateLimits { windows, native: Some(value.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    const NOW: i64 = 1_790_000_000;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(*name), HeaderValue::try_from(*value)) {
                headers.insert(name, value);
            }
        }
        headers
    }

    #[test]
    fn live_headers_fixture() {
        // Captured from the live backend on 2026-09-25 (turn state redacted).
        let fixture: Map<String, Value> = serde_json::from_str(include_str!("../fixtures/rate_limit_headers.json")).unwrap();
        let pairs: Vec<(&str, &str)> = fixture.iter().map(|(k, v)| (k.as_str(), v.as_str().unwrap())).collect();
        let limits = from_headers(&map(&pairs), NOW).unwrap();
        // The secondary window is reported as all zeros ("not applicable") and is skipped.
        assert_eq!(limits.windows.len(), 1);
        let primary = &limits.windows[0];
        assert_eq!(primary.id, "codex.primary");
        assert!((primary.used_percent - 24.0).abs() < f64::EPSILON);
        assert_eq!(primary.window_minutes, Some(10_080));
        assert_eq!(primary.resets_at, Some(1_790_661_960));
        let native = limits.native.unwrap();
        assert_eq!(native["x-codex-plan-type"], "pro");
        assert_eq!(native["x-codex-active-limit"], "premium");
        assert_eq!(native["x-codex-credits-has-credits"], "False");
        assert!(native.get("x-codex-turn-state").is_some());
    }

    #[test]
    fn two_windows_and_two_families_stay_separate() {
        let headers = map(&[
            ("x-codex-primary-used-percent", "21.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1790003600"),
            ("x-codex-secondary-used-percent", "7"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-after-seconds", "600"),
            ("x-codex-other-primary-used-percent", "50"),
            ("x-codex-other-primary-window-minutes", "60"),
            ("x-codex-other-limit-name", "GPT-6 Sol"),
            ("x-unrelated", "1"),
        ]);
        let limits = from_headers(&headers, NOW).unwrap();
        let ids: Vec<&str> = limits.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["codex.primary", "codex.secondary", "codex-other.primary"]);
        assert_eq!(limits.windows[0].window_minutes, Some(300));
        assert_eq!(limits.windows[0].resets_at, Some(1_790_003_600));
        assert_eq!(limits.windows[1].resets_at, Some(NOW + 600), "reset-after is relative to now");
        assert_eq!(limits.windows[2].window_minutes, Some(60));
        let native = limits.native.unwrap();
        assert_eq!(native["x-codex-other-limit-name"], "GPT-6 Sol");
        assert!(native.get("x-unrelated").is_none());
        assert!(from_headers(&map(&[("content-type", "text/event-stream")]), NOW).is_none());
    }

    #[test]
    fn rate_limit_events() {
        let event = serde_json::json!({"type":"codex.rate_limits","plan_type":"pro","metered_limit_name":"codex_other",
            "rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"reset_at":1_790_000_300_i64},
                           "secondary":{"used_percent":3.0,"window_minutes":10080}},
            "credits":{"has_credits":false,"unlimited":false}});
        let limits = from_event(&event);
        assert_eq!(limits.windows.len(), 2);
        assert_eq!(limits.windows[0].id, "codex-other.primary");
        assert_eq!(limits.windows[0].resets_at, Some(1_790_000_300));
        assert_eq!(limits.windows[1].id, "codex-other.secondary");
        assert_eq!(limits.native.as_ref().unwrap()["plan_type"], "pro");
        let default = from_event(&serde_json::json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":1.0}}}));
        assert_eq!(default.windows[0].id, "codex.primary");
    }
}
