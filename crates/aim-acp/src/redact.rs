//! Best-effort scrubbing of secrets and personal data from agent-provided text (stderr tails,
//! error messages) before aim stores or shows it.
//!
//! It masks bearer tokens, `sk-…` API keys, email addresses and the values of secret-looking
//! keys (`token`, `secret`, `password`, `api_key`, `authorization`, …) in `key=value`,
//! `key: value` and JSON `"key":"value"` forms. It is a safety net, not a guarantee: aim never
//! logs agent traffic by default.

const MASK: &str = "***";

/// Whether a key name looks like it holds a credential.
fn is_secret_key(key: &str) -> bool {
    let key = key.trim_matches(|c: char| c == '"' || c == '\'').to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "api_key", "apikey", "api-key", "authorization", "cookie", "credential"]
        .iter()
        .any(|needle| key.contains(needle))
}

/// Whether a segment looks like an email address.
fn is_email(segment: &str) -> bool {
    let Some((local, domain)) = segment.split_once('@') else { return false };
    let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-');
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && local.chars().all(valid) && domain.chars().all(valid)
}

/// Whether a segment looks like an API key (`sk-…`, `sk-ant-…`).
fn is_api_key(segment: &str) -> bool {
    segment.starts_with("sk-") && segment.len() >= 20
}

/// Splits `text` into alternating runs of delimiter and non-delimiter characters.
fn segments(text: &str) -> Vec<(bool, String)> {
    let is_delimiter = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '{' | '}' | '(' | ')' | '[' | ']' | ';');
    let mut out: Vec<(bool, String)> = Vec::new();
    for c in text.chars() {
        let delimiter = is_delimiter(c);
        match out.last_mut() {
            Some((last_delimiter, run)) if *last_delimiter == delimiter => run.push(c),
            _ => out.push((delimiter, c.to_string())),
        }
    }
    out
}

/// Returns `text` with secrets and email addresses masked.
#[must_use]
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut mask_next = false;
    for (delimiter, segment) in segments(text) {
        if delimiter {
            out.push_str(&segment);
            continue;
        }
        if segment.eq_ignore_ascii_case("bearer") || segment.eq_ignore_ascii_case("basic") {
            out.push_str(&segment);
            mask_next = true;
            continue;
        }
        if mask_next {
            if segment == ":" || segment == "=" {
                out.push_str(&segment);
                continue;
            }
            out.push_str(MASK);
            mask_next = false;
            continue;
        }
        if is_email(&segment) {
            out.push_str("<redacted-email>");
        } else if is_api_key(&segment) {
            out.push_str("sk-***");
        } else if let Some((key, value, separator)) = split_assignment(&segment) {
            out.push_str(key);
            out.push(separator);
            if is_secret_key(key) {
                if value.is_empty() {
                    mask_next = true;
                } else {
                    out.push_str(MASK);
                }
            } else {
                out.push_str(&redact(value));
            }
        } else {
            if is_secret_key(&segment) {
                mask_next = true;
            }
            out.push_str(&segment);
        }
    }
    out
}

/// Splits `key=value` or `key:value` (the first separator wins); `None` for URLs and plain words.
fn split_assignment(segment: &str) -> Option<(&str, &str, char)> {
    let separator = segment.chars().find(|c| *c == '=' || *c == ':')?;
    let (key, value) = segment.split_once(separator)?;
    if key.is_empty() || value.starts_with("//") {
        return None;
    }
    Some((key, value, separator))
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn masks_bearer_tokens_keys_and_emails() {
        assert_eq!(redact("Authorization: Bearer abc.def.ghi"), "Authorization: Bearer ***");
        assert_eq!(redact("key sk-ant-api03-abcdefghijklmnop used"), "key sk-*** used");
        assert_eq!(redact("user someone@example.com logged in"), "user <redacted-email> logged in");
    }

    #[test]
    fn masks_secret_assignments_in_every_form() {
        assert_eq!(redact("ANTHROPIC_API_KEY=abc123 x=1"), "ANTHROPIC_API_KEY=*** x=1");
        assert_eq!(redact(r#"{"access_token":"abc","plan":"max"}"#), r#"{"access_token":"***","plan":"max"}"#);
        assert_eq!(redact("password: hunter2"), "password: ***");
    }

    #[test]
    fn leaves_ordinary_text_and_urls_alone() {
        let text = "fetch https://example.com/a:b failed: code=500 at 12:30";
        assert_eq!(redact(text), text);
    }
}
