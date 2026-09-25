//! Session configuration options (`session/set_config_option`): model, effort, mode, fast mode.
//! The values are the agent's data, never aim enums (AGENTS.md "Capabilities are data").

use std::collections::HashMap;

use aim_kernel::model_match::{self, Candidate, Request, Tier, Unresolved};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::VariantSyntax;
use crate::error::AcpError;
use crate::redact::redact;
use crate::wire;

/// Which option to set: a semantic key resolved by category (then by conventional id), or an
/// explicit option id.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "key", content = "id", rename_all = "snake_case")]
pub enum ConfigKey {
    /// The model (category `model`, id `model`).
    Model,
    /// Reasoning effort (category `thought_level`, id `effort`).
    Effort,
    /// Permission mode (category `mode`, id `mode`).
    Mode,
    /// An option by id, e.g. `fast`.
    Id(String),
}

impl ConfigKey {
    /// The option category and conventional id this key resolves through.
    const fn lookup(&self) -> (Option<&'static str>, Option<&'static str>) {
        match self {
            Self::Model => (Some("model"), Some("model")),
            Self::Effort => (Some("thought_level"), Some("effort")),
            Self::Mode => (Some("mode"), Some("mode")),
            Self::Id(_) => (None, None),
        }
    }

    /// A short name for errors.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Model => "model",
            Self::Effort => "effort",
            Self::Mode => "mode",
            Self::Id(id) => id,
        }
    }
}

/// One selectable value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValue {
    /// Value id sent on the wire.
    pub value: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Group name, for grouped selects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// An option's kind and current value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigKind {
    /// One of a list of values.
    Select {
        /// Current value id.
        current: String,
        /// Allowed values.
        values: Vec<ConfigValue>,
    },
    /// On/off.
    Boolean {
        /// Current value.
        current: bool,
    },
    /// A kind this client does not know.
    Other {
        /// The ACP `type`.
        option_type: String,
    },
}

/// A session configuration option as advertised by the agent.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOption {
    /// Option id (`model`, `effort`, `mode`, `fast`, …).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Semantic category (`mode`, `model`, `model_config`, `thought_level`, or custom).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Kind and value.
    pub kind: ConfigKind,
    /// How the agent spells variants of model values: its profile's
    /// [`crate::AcpAgentConfig::model_variants`], attached by the session to every option list it
    /// holds, so each caller resolving against them applies the same rule (docs/adr/0075). Never
    /// read from or written to the wire.
    #[serde(skip)]
    pub model_variants: Vec<VariantSyntax>,
}

macro_rules! opaque_config_debug {
    ($($name:ident),+ $(,)?) => {
        $(
            impl core::fmt::Debug for $name {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    f.write_str(concat!(stringify!($name), " { provider value: *** }"))
                }
            }
        )+
    };
}

opaque_config_debug!(ConfigKey, ConfigValue, ConfigKind, ConfigOption);

impl ConfigOption {
    /// The current value as a string (`true`/`false` for booleans).
    #[must_use]
    pub fn current(&self) -> Option<String> {
        match &self.kind {
            ConfigKind::Select { current, .. } => Some(current.clone()),
            ConfigKind::Boolean { current } => Some(current.to_string()),
            ConfigKind::Other { .. } => None,
        }
    }

    /// The allowed values (`true`/`false` for booleans).
    #[must_use]
    pub fn allowed(&self) -> Vec<String> {
        match &self.kind {
            ConfigKind::Select { values, .. } => values.iter().map(|v| v.value.clone()).collect(),
            ConfigKind::Boolean { .. } => vec!["true".into(), "false".into()],
            ConfigKind::Other { .. } => Vec::new(),
        }
    }
}

/// Parses a `configOptions` array, skipping malformed entries.
#[must_use]
pub fn parse_config_options(raw: Option<&Value>) -> Vec<ConfigOption> {
    raw.and_then(Value::as_array).map(|options| options.iter().filter_map(parse_option).collect()).unwrap_or_default()
}

fn parse_option(raw: &Value) -> Option<ConfigOption> {
    let value = |v: &Value, group: Option<&str>| {
        Some(ConfigValue {
            value: wire::string(v, "value")?,
            name: wire::string(v, "name").unwrap_or_default(),
            description: wire::string(v, "description"),
            group: group.map(str::to_owned),
        })
    };
    let kind = match wire::str(raw, "type")? {
        "select" => {
            let mut values = Vec::new();
            for entry in wire::array(raw, "options") {
                if entry.get("group").is_some() {
                    let group = wire::str(entry, "name").or_else(|| wire::str(entry, "group"));
                    values.extend(wire::array(entry, "options").iter().filter_map(|v| value(v, group)));
                } else {
                    values.extend(value(entry, None));
                }
            }
            ConfigKind::Select { current: wire::string(raw, "currentValue")?, values }
        }
        "boolean" => ConfigKind::Boolean { current: raw.get("currentValue").and_then(Value::as_bool)? },
        other => ConfigKind::Other { option_type: other.to_owned() },
    };
    Some(ConfigOption {
        id: wire::string(raw, "id")?,
        name: wire::string(raw, "name").unwrap_or_default(),
        description: wire::string(raw, "description"),
        category: wire::string(raw, "category"),
        kind,
        model_variants: Vec::new(),
    })
}

/// `options` with the variant spellings of the agent's profile attached (docs/adr/0075).
#[must_use]
pub fn with_model_variants(mut options: Vec<ConfigOption>, variants: &[VariantSyntax]) -> Vec<ConfigOption> {
    for option in &mut options {
        option.model_variants = variants.to_vec();
    }
    options
}

/// Finds the option `key` refers to.
#[must_use]
pub fn find<'a>(options: &'a [ConfigOption], key: &ConfigKey) -> Option<&'a ConfigOption> {
    if let ConfigKey::Id(id) = key {
        return options.iter().find(|o| &o.id == id);
    }
    let (category, id) = key.lookup();
    options.iter().find(|o| o.category.as_deref() == category).or_else(|| options.iter().find(|o| Some(o.id.as_str()) == id))
}

/// Resolves `value` for the option `key` to the value the agent advertises (docs/adr/0075).
/// Returns the option id and the advertised value to send: for a select option, the value the
/// request resolves to; for a boolean, `true` or `false`.
///
/// A select value resolves by the first tier that matches, best first: the exact value; the
/// value or display name ignoring case (with the variant spellings the agent's profile declares,
/// such as `-1m` for `[1m]`, folded together); for the model only, the one value of the family,
/// generation and variant the request names (`opus`, `claude-opus-5-5` → `opus[1m]` when that is
/// the only Opus offered). A request naming two families, or a family in two generations without
/// naming one, is ambiguous. The decision is `aim_kernel::model_match`; this shell only
/// normalizes strings into the keys it compares.
///
/// # Errors
///
/// [`AcpError::ConfigUnavailable`] when the option is not advertised,
/// [`AcpError::ConfigValueRejected`] when `value` resolves to none of its values,
/// [`AcpError::ConfigValueAmbiguous`] when it matches several equally well.
pub fn resolve_config_value(options: &[ConfigOption], key: &ConfigKey, value: &str) -> Result<(String, String), AcpError> {
    let option = find(options, key).ok_or_else(|| AcpError::ConfigUnavailable { key: key.label().to_owned() })?;
    let resolved = match &option.kind {
        ConfigKind::Select { values, .. } => resolve_select(depth(options, option), option, values, value)?.value.clone(),
        ConfigKind::Boolean { .. } => match value {
            "true" | "on" => "true".to_owned(),
            "false" | "off" => "false".to_owned(),
            _ => return Err(rejected(option, value, &[])),
        },
        ConfigKind::Other { .. } => return Err(rejected(option, value, &[])),
    };
    Ok((option.id.clone(), resolved))
}

/// Resolves `value` for the option `key` and returns the option id, the resolved value (what the
/// agent must then report as current) and the `session/set_config_option` params (without
/// `sessionId`).
///
/// # Errors
///
/// See [`resolve_config_value`].
pub fn set_params(options: &[ConfigOption], key: &ConfigKey, value: &str) -> Result<(String, String, Value), AcpError> {
    let (id, resolved) = resolve_config_value(options, key, value)?;
    let boolean = find(options, key).is_some_and(|o| matches!(o.kind, ConfigKind::Boolean { .. }));
    let params = if boolean {
        serde_json::json!({"configId": id, "type": "boolean", "value": resolved == "true"})
    } else {
        serde_json::json!({"configId": id, "value": resolved})
    };
    Ok((id, resolved, params))
}

/// The rejection of `value` for `option`, listing what the option offers. The request is
/// redacted too: a mistyped secret must not be echoed.
fn rejected(option: &ConfigOption, value: &str, matches: &[&ConfigValue]) -> AcpError {
    let allowed = match &option.kind {
        ConfigKind::Select { values, .. } => values.iter().map(redacted).collect(),
        ConfigKind::Boolean { .. } => {
            ["true", "false"].map(|v| ConfigValue { value: v.into(), name: String::new(), description: None, group: None }).into()
        }
        ConfigKind::Other { .. } => Vec::new(),
    };
    let (id, value) = (option.id.clone(), redact(value));
    if matches.is_empty() {
        AcpError::ConfigValueRejected { id, value, allowed }
    } else {
        AcpError::ConfigValueAmbiguous { id, value, matches: matches.iter().copied().map(redacted).collect(), allowed }
    }
}

/// An advertised value as errors show it: value and display name, redacted (`crate::redact`).
fn redacted(value: &ConfigValue) -> ConfigValue {
    ConfigValue { value: redact(&value.value), name: redact(&value.name), description: None, group: None }
}

/// How far from the exact bytes a request may resolve for `option`, one of `options`
/// (docs/adr/0075). It asks [`find`], so an option is treated as the model or the effort exactly
/// when a lookup for that key finds it: the model down to its family, the effort ignoring case,
/// anything else (mode, fast, custom ids) exactly.
fn depth(options: &[ConfigOption], option: &ConfigOption) -> Tier {
    let serves = |key: &ConfigKey| find(options, key).is_some_and(|found| core::ptr::eq(found, option));
    if serves(&ConfigKey::Model) {
        Tier::FamilyAnyVariant
    } else if serves(&ConfigKey::Effort) {
        Tier::Folded
    } else {
        Tier::Exact
    }
}

fn resolve_select<'a>(
    deepest: Tier,
    option: &ConfigOption,
    values: &'a [ConfigValue],
    requested: &str,
) -> Result<&'a ConfigValue, AcpError> {
    // Variant spellings are the profile's, and only for the model.
    let variants: &[VariantSyntax] = if deepest == Tier::FamilyAnyVariant { &option.model_variants } else { &[] };
    let mut ids = Ids::default();
    let (request, words) = request_keys(&mut ids, requested, variants);
    let candidates: Vec<Candidate> = values.iter().map(|value| candidate_keys(&mut ids, value, deepest, variants)).collect();
    let pair = |first: usize, second: usize| -> Vec<&ConfigValue> { [first, second].iter().filter_map(|&i| values.get(i)).collect() };
    match model_match::resolve(request, &words, &candidates, deepest) {
        // The kernel proves the index in bounds (`theorem_resolved_is_the_unique_best`).
        Ok(resolved) => values.get(resolved.index).ok_or_else(|| rejected(option, requested, &[])),
        Err(Unresolved::Unknown) => Err(rejected(option, requested, &[])),
        Err(Unresolved::Ambiguous { first, second, .. } | Unresolved::Conflict { first, second }) => {
            Err(rejected(option, requested, &pair(first, second)))
        }
    }
}

/// Shell-assigned ids of normalized strings for one resolution: equal strings share an id,
/// distinct strings never do (the kernel compares ids only).
#[derive(Default)]
struct Ids(HashMap<String, u64>);

impl Ids {
    fn id(&mut self, text: &str) -> u64 {
        let next = u64::try_from(self.0.len()).unwrap_or(u64::MAX);
        *self.0.entry(text.to_owned()).or_insert(next)
    }
}

/// The request's keys and the ids of its words.
fn request_keys(ids: &mut Ids, requested: &str, variants: &[VariantSyntax]) -> (Request, Vec<u64>) {
    let lower = fold_text(requested);
    let (base, variant) = split_variant(&lower, variants);
    let words = words(base).iter().map(|word| ids.id(word)).collect();
    let request = Request {
        value: ids.id(requested),
        folded: ids.id(&fold_value(requested, variants)),
        generation: generation(base),
        variant: variant.map(|v| ids.id(v)),
    };
    (request, words)
}

/// An advertised value's keys. Only the model has a family: other options match exactly or by
/// case.
fn candidate_keys(ids: &mut Ids, value: &ConfigValue, deepest: Tier, variants: &[VariantSyntax]) -> Candidate {
    let lower = fold_text(&value.value);
    let (base, variant) = split_variant(&lower, variants);
    let name = fold_text(&value.name);
    let family = if deepest == Tier::FamilyAnyVariant { family(base, &value.name) } else { None };
    Candidate {
        value: ids.id(&value.value),
        folded: ids.id(&fold_value(&value.value, variants)),
        name: (!name.is_empty()).then(|| ids.id(&name)),
        family: family.map(|word| ids.id(&word)),
        generation: generation(base).or_else(|| generation(&name)),
        variant: variant.map(|v| ids.id(v)),
    }
}

/// Lowercase, trimmed, with inner whitespace collapsed to one space.
fn fold_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A value folded for comparison: [`fold_text`], with a declared variant spelling written one
/// way (`opus-1m` → `opus[1m]` when the profile declares both).
fn fold_value(value: &str, variants: &[VariantSyntax]) -> String {
    let lower = fold_text(value);
    match split_variant(&lower, variants) {
        (base, Some(variant)) => format!("{base}[{variant}]"),
        (base, None) => base.to_owned(),
    }
}

/// Splits a trailing variant off a lowercased value, in the first of the profile's declared
/// spellings that fits (for claude-agent-acp, `opus[1m]` and `opus-1m` are both (`opus`, `1m`)).
/// With no declared spelling nothing is a variant.
fn split_variant<'a>(lower: &'a str, variants: &[VariantSyntax]) -> (&'a str, Option<&'a str>) {
    let size = |hint: &str, unit: char| {
        hint.strip_suffix(unit.to_ascii_lowercase()).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    for syntax in variants {
        let (split, unit) = match *syntax {
            VariantSyntax::Bracketed { unit } => (lower.strip_suffix(']').and_then(|s| s.rsplit_once('[')), unit),
            VariantSyntax::Dashed { unit } => (lower.rsplit_once('-'), unit),
        };
        if let Some((base, hint)) = split
            && size(hint, unit)
        {
            return (base, Some(hint));
        }
    }
    (lower, None)
}

/// Lowercased alphabetic words of at least two letters (`claude-opus-5-5` → `claude`, `opus`).
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().nth(1).is_some() && word.chars().all(char::is_alphabetic))
        .map(str::to_lowercase)
        .collect()
}

/// An advertised value's family: the last word its value (variant removed) shares with its
/// display name (`opus` for `opus[1m]` "Opus 5.5", `fable` for `claude-fable-5-1[1m]` "Fable 5.1").
/// Derived from the agent's own data, so no family or vendor word is built into aim.
fn family(base: &str, name: &str) -> Option<String> {
    let name = words(name);
    words(base).into_iter().rev().find(|word| name.contains(word))
}

/// The first version number in `text` (variant removed), as `major * 1000 + minor`: `5-5` and
/// `5.5` are 5005, `5` and `4-0` are 5000 and 4000. Numbers of more than three digits (dates) are
/// not versions.
fn generation(text: &str) -> Option<u64> {
    let number = |run: &str| {
        if (1..=3).contains(&run.len()) && run.bytes().all(|b| b.is_ascii_digit()) { run.parse::<u64>().ok() } else { None }
    };
    // Alphanumeric runs, each with the separator that follows it.
    let runs: Vec<(&str, Option<char>)> = text
        .split_inclusive(|c: char| !c.is_alphanumeric())
        .map(|piece| match piece.chars().next_back() {
            Some(c) if !c.is_alphanumeric() => (piece.strip_suffix(c).unwrap_or(piece), Some(c)),
            _ => (piece, None),
        })
        .collect();
    runs.iter().enumerate().find_map(|(i, (run, separator))| {
        let major = number(run)?;
        let minor = match separator {
            Some('.' | '-') => runs.get(i + 1).and_then(|(next, _)| number(next)).unwrap_or(0),
            _ => 0,
        };
        Some(major * 1000 + minor)
    })
}

/// Checks that `options` (the agent's answer) report `value` as the current value of `id`.
///
/// # Errors
///
/// [`AcpError::ConfigNotApplied`] when they do not.
pub fn confirm(options: &[ConfigOption], id: &str, value: &str) -> Result<(), AcpError> {
    let current = options.iter().find(|o| o.id == id).and_then(ConfigOption::current);
    let wanted = match value {
        "on" => "true",
        "off" => "false",
        other => other,
    };
    if current.as_deref() == Some(wanted) {
        Ok(())
    } else {
        Err(AcpError::ConfigNotApplied { id: id.to_owned(), requested: value.to_owned(), current })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn options() -> Vec<ConfigOption> {
        parse_config_options(Some(&json!([
            {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "auto",
             "options": [{"value": "auto", "name": "Auto"}, {"value": "plan", "name": "Plan"}]},
            {"id": "speed", "name": "Fast", "type": "boolean", "currentValue": false},
            {"id": "effort", "name": "Effort", "type": "select", "currentValue": "low", "options": [{"value": "low", "name": "Low"}]}
        ])))
    }

    #[test]
    fn keys_resolve_by_category_then_conventional_id() {
        let options = options();
        assert_eq!(find(&options, &ConfigKey::Mode).map(|o| o.id.as_str()), Some("mode"));
        // No `thought_level` category here: the conventional id wins.
        assert_eq!(find(&options, &ConfigKey::Effort).map(|o| o.id.as_str()), Some("effort"));
        assert!(find(&options, &ConfigKey::Model).is_none());
        assert_eq!(find(&options, &ConfigKey::Id("speed".into())).map(|o| o.id.as_str()), Some("speed"));
    }

    #[test]
    fn set_params_follow_the_option_kind() {
        let options = options();
        assert_eq!(
            set_params(&options, &ConfigKey::Mode, "plan").unwrap(),
            ("mode".into(), "plan".into(), json!({"configId": "mode", "value": "plan"}))
        );
        assert_eq!(
            set_params(&options, &ConfigKey::Id("speed".into()), "on").unwrap(),
            ("speed".into(), "true".into(), json!({"configId": "speed", "type": "boolean", "value": true}))
        );
        // Modes match exactly only: the names of permission modes are not aliases.
        assert!(matches!(set_params(&options, &ConfigKey::Mode, "Plan"), Err(AcpError::ConfigValueRejected { .. })));
        assert!(matches!(set_params(&options, &ConfigKey::Id("speed".into()), "maybe"), Err(AcpError::ConfigValueRejected { .. })));
        assert!(matches!(set_params(&options, &ConfigKey::Model, "x"), Err(AcpError::ConfigUnavailable { .. })));
    }

    #[test]
    fn confirmation_reads_the_agents_answer() {
        let options = options();
        assert!(confirm(&options, "mode", "auto").is_ok());
        assert!(confirm(&options, "speed", "off").is_ok());
        assert_eq!(
            confirm(&options, "mode", "plan"),
            Err(AcpError::ConfigNotApplied { id: "mode".into(), requested: "plan".into(), current: Some("auto".into()) })
        );
    }

    /// The options claude-agent-acp 0.81.2 advertised in a live session
    /// (`tests/fixtures/permission_turn.jsonl`, line 6), as a `claude` session holds them.
    fn claude_options() -> Vec<ConfigOption> {
        claude(recorded_options())
    }

    fn recorded_options() -> Vec<ConfigOption> {
        let line = include_str!("../tests/fixtures/permission_turn.jsonl").lines().nth(5).unwrap();
        let frame: Value = serde_json::from_str(line).unwrap();
        parse_config_options(frame["msg"]["result"].get("configOptions"))
    }

    /// `options` with the `claude` profile's variant spellings attached, as its session does.
    fn claude(options: Vec<ConfigOption>) -> Vec<ConfigOption> {
        with_model_variants(options, &crate::AcpAgentConfig::claude().model_variants)
    }

    const CLAUDE_VARIANTS: [VariantSyntax; 2] = [VariantSyntax::Bracketed { unit: 'm' }, VariantSyntax::Dashed { unit: 'm' }];

    fn model(options: &[ConfigOption], requested: &str) -> Result<String, AcpError> {
        resolve_config_value(options, &ConfigKey::Model, requested).map(|(id, value)| {
            assert_eq!(id, "model");
            value
        })
    }

    #[test]
    fn claude_model_aliases_resolve_to_the_advertised_value() {
        let options = claude_options();
        let offered = find(&options, &ConfigKey::Model).unwrap().allowed();
        assert_eq!(offered, ["default", "opus[1m]", "claude-fable-5-1[1m]", "sonnet", "haiku"], "the recorded list");
        let cases = [
            ("opus", "opus[1m]"),
            ("Opus", "opus[1m]"),
            (" OPUS ", "opus[1m]"),
            ("claude-opus-5-5", "opus[1m]"),
            ("claude-opus-5.5", "opus[1m]"),
            ("opus-1m", "opus[1m]"),
            ("opus[1m]", "opus[1m]"),
            ("OPUS[1M]", "opus[1m]"),
            ("Opus 5.5", "opus[1m]"),
            ("claude-opus-5-5[1m]", "opus[1m]"),
            ("claude-opus-5-5-1m", "opus[1m]"),
            ("fable", "claude-fable-5-1[1m]"),
            ("Fable 5.1", "claude-fable-5-1[1m]"),
            ("claude-fable-5-1", "claude-fable-5-1[1m]"),
            ("claude-fable-5-1-1m", "claude-fable-5-1[1m]"),
            ("sonnet", "sonnet"),
            ("Sonnet", "sonnet"),
            ("claude-sonnet-5", "sonnet"),
            ("claude-sonnet-5-0", "sonnet"),
            ("haiku", "haiku"),
            ("claude-haiku-4-5", "haiku"),
            ("claude-haiku-4-5-20251001", "haiku"),
            ("default", "default"),
            ("Default (recommended)", "default"),
        ];
        for (requested, expected) in cases {
            assert_eq!(model(&options, requested).as_deref(), Ok(expected), "{requested}");
        }
    }

    #[test]
    fn unknown_models_are_rejected_with_what_the_agent_offers() {
        let options = claude_options();
        let offered = find(&options, &ConfigKey::Model).unwrap().allowed();
        // Another vendor, a made-up id, a vendor word that is no family, the wrong generation,
        // and a variant the family does not come in.
        for requested in ["gpt-6", "no-such-model", "claude", "claude-opus-4-5", "sonnet-4-5", "opus-2m", "sonnet[1m]", ""] {
            let error = model(&options, requested).unwrap_err();
            let AcpError::ConfigValueRejected { id, value, allowed } = &error else { panic!("{requested}: {error}") };
            assert_eq!((id.as_str(), value.as_str(), allowed.len()), ("model", requested, 5), "{requested}");
            let message = error.to_string();
            for pair in [
                "`default` (Default (recommended))",
                "`opus[1m]` (Opus 5.5)",
                "`claude-fable-5-1[1m]` (Fable 5.1)",
                "`sonnet` (Sonnet 5)",
                "`haiku` (Haiku 4.5)",
            ] {
                assert!(message.contains(pair), "{message}");
            }
            assert!(message.starts_with("the requested model is not offered; the agent offers `default`"), "{message}");
            // The request itself is never repeated: it could be a mistyped secret.
            if !requested.is_empty() && !offered.iter().any(|v| v.contains(requested)) {
                assert!(!message.contains(requested), "{message}");
            }
        }
    }

    #[test]
    fn a_tie_is_ambiguous_and_names_both() {
        let options = claude(parse_config_options(Some(&json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "default",
             "options": [{"value": "default", "name": "Default", "description": "Opus (1M context)"},
                         {"value": "opus[1m]", "name": "Opus 5.5"},
                         {"value": "claude-opus-5-5[1m]", "name": "Opus 5.5 (1M context)"},
                         {"value": "claude-opus-4-5", "name": "Opus 4.5"},
                         {"value": "sonnet", "name": "Sonnet 5"}]}
        ]))));
        let error = model(&options, "claude-opus-5-5").unwrap_err();
        let AcpError::ConfigValueAmbiguous { matches, allowed, .. } = &error else { panic!("{error}") };
        assert_eq!(matches.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(), ["opus[1m]", "claude-opus-5-5[1m]"]);
        assert_eq!(allowed.len(), 5);
        assert!(
            error.to_string().starts_with("the requested model is ambiguous: it matches `opus[1m]` (Opus 5.5), `claude-opus-5-5[1m]`"),
            "{error}"
        );
        // A request that names the generation of only one of them is not a tie; neither is the
        // exact value.
        assert_eq!(model(&options, "claude-opus-4-5").as_deref(), Ok("claude-opus-4-5"));
        assert_eq!(model(&options, "opus-4-5").as_deref(), Ok("claude-opus-4-5"));
        assert_eq!(model(&options, "opus[1m]").as_deref(), Ok("opus[1m]"));
        // `opus` names no generation while Opus comes in 5.5 and 4.5: no silent pick of either
        // (`theorem_unqualified_family_never_picks_a_generation`).
        let error = model(&options, "opus").unwrap_err();
        let AcpError::ConfigValueAmbiguous { matches, .. } = &error else { panic!("{error}") };
        assert_eq!(matches.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(), ["opus[1m]", "claude-opus-4-5"]);
        // Naming the variant narrows it to one generation.
        assert_eq!(model(&options, "opus-1m").as_deref(), Ok("opus[1m]"));
    }

    /// A request that names two advertised families picks neither
    /// (`theorem_two_families_never_resolve`).
    #[test]
    fn two_families_are_ambiguous() {
        let options = claude_options();
        for requested in ["opus sonnet", "sonnet-opus", "Haiku or Fable"] {
            let error = model(&options, requested).unwrap_err();
            assert!(matches!(&error, AcpError::ConfigValueAmbiguous { matches, .. } if matches.len() == 2), "{requested}: {error}");
        }
        let error = model(&options, "opus sonnet").unwrap_err();
        assert!(error.to_string().contains("it matches `opus[1m]` (Opus 5.5), `sonnet` (Sonnet 5)"), "{error}");
    }

    /// Whether an option is the model or the effort is decided by the same lookup that finds it,
    /// so an option found as the model by its id resolves like the model.
    #[test]
    fn an_option_found_as_the_model_resolves_like_the_model() {
        let options = claude(parse_config_options(Some(&json!([
            {"id": "model", "name": "Model", "category": "custom_model", "type": "select", "currentValue": "a",
             "options": [{"value": "opus[1m]", "name": "Opus 5.5"}, {"value": "sonnet", "name": "Sonnet 5"}]},
            {"id": "effort", "name": "Effort", "category": "custom_effort", "type": "select", "currentValue": "low",
             "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}
        ]))));
        assert_eq!(find(&options, &ConfigKey::Model).map(|o| o.id.as_str()), Some("model"));
        assert_eq!(model(&options, "opus").as_deref(), Ok("opus[1m]"));
        assert_eq!(resolve_config_value(&options, &ConfigKey::Id("model".into()), "Sonnet").map(|(_, v)| v).as_deref(), Ok("sonnet"));
        assert_eq!(resolve_config_value(&options, &ConfigKey::Effort, "High").map(|(_, v)| v).as_deref(), Ok("high"));
        // An option with the model's category wins the lookup, and only it resolves like the model.
        let mut both = options.clone();
        both.extend(claude(parse_config_options(Some(&json!([
            {"id": "llm", "name": "LLM", "category": "model", "type": "select", "currentValue": "x",
             "options": [{"value": "x-large", "name": "X Large"}]}
        ])))));
        assert_eq!(resolve_config_value(&both, &ConfigKey::Model, "x large"), Ok(("llm".to_owned(), "x-large".to_owned())));
        assert!(resolve_config_value(&both, &ConfigKey::Id("model".into()), "opus").is_err(), "no longer the model: exact only");
    }

    /// Variant spellings are the profile's: without a declaration nothing is read as a variant.
    #[test]
    fn variants_are_folded_only_when_the_profile_declares_them() {
        let list = json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "claude-opus-5-5",
             "options": [{"value": "claude-opus-5-5", "name": "Opus 5.5"}, {"value": "claude-opus-5-5[1m]", "name": "Opus 5.5 (1M context)"}]}
        ]);
        let declared = claude(parse_config_options(Some(&list)));
        let undeclared = parse_config_options(Some(&list));
        assert!(undeclared[0].model_variants.is_empty());
        // Declared: `-1m` is the `[1m]` variant, and a request without one prefers a value
        // without one.
        assert_eq!(model(&declared, "claude-opus-5-5-1m").as_deref(), Ok("claude-opus-5-5[1m]"));
        assert_eq!(model(&declared, "opus").as_deref(), Ok("claude-opus-5-5"));
        // Undeclared: no suffix means anything, so both are plain Opus 5.5 and neither is picked.
        assert!(matches!(model(&undeclared, "claude-opus-5-5-1m"), Err(AcpError::ConfigValueAmbiguous { .. })));
        assert!(matches!(model(&undeclared, "opus"), Err(AcpError::ConfigValueAmbiguous { .. })));
        // Exact and case-insensitive values still resolve.
        assert_eq!(model(&undeclared, "Claude-Opus-5-5[1M]").as_deref(), Ok("claude-opus-5-5[1m]"));
    }

    #[test]
    fn effort_matches_by_case_but_never_by_family() {
        let options = claude_options();
        let effort = |requested: &str| resolve_config_value(&options, &ConfigKey::Effort, requested).map(|(_, value)| value);
        assert_eq!(effort("high").as_deref(), Ok("high"));
        assert_eq!(effort("High").as_deref(), Ok("high"));
        assert_eq!(effort(" XHIGH ").as_deref(), Ok("xhigh"));
        assert!(matches!(effort("ultra"), Err(AcpError::ConfigValueRejected { allowed, .. }) if allowed.len() == 6));
        // `fast` (category `model_config`) and `mode` match exactly only.
        let fast = |requested: &str| resolve_config_value(&options, &ConfigKey::Id("fast".into()), requested).map(|(_, value)| value);
        assert_eq!(fast("on").as_deref(), Ok("on"));
        assert!(fast("On").is_err());
        assert!(resolve_config_value(&options, &ConfigKey::Mode, "Manual").is_err());
    }

    #[test]
    fn normalization_examples() {
        let claude = &CLAUDE_VARIANTS;
        assert_eq!(split_variant("opus[1m]", claude), ("opus", Some("1m")));
        assert_eq!(split_variant("opus-1m", claude), ("opus", Some("1m")));
        assert_eq!(split_variant("claude-opus-5-5", claude), ("claude-opus-5-5", None));
        assert_eq!(split_variant("opus[m]", claude), ("opus[m]", None));
        assert_eq!(split_variant("opus[2k]", claude), ("opus[2k]", None), "only the declared unit");
        assert_eq!(split_variant("opus[1m]", &[]), ("opus[1m]", None));
        assert_eq!(split_variant("opus-1m", &[VariantSyntax::Bracketed { unit: 'm' }]), ("opus-1m", None));
        assert_eq!(fold_value(" Claude-Opus-5-5-1M ", claude), "claude-opus-5-5[1m]");
        assert_eq!(fold_value(" Claude-Opus-5-5-1M ", &[]), "claude-opus-5-5-1m");
        assert_eq!(words("claude-opus-5-5"), ["claude", "opus"]);
        assert_eq!(words("Default (recommended)"), ["default", "recommended"]);
        assert_eq!(family("claude-fable-5-1", "Fable 5.1").as_deref(), Some("fable"));
        assert_eq!(family("claude-3-5-sonnet-20240620", "Claude 3.5 Sonnet").as_deref(), Some("sonnet"));
        assert_eq!(family("default", "Default (recommended)").as_deref(), Some("default"));
        assert_eq!(family("x", ""), None);
        assert_eq!(generation("claude-opus-5-5"), Some(5005));
        assert_eq!(generation("opus 5.5"), Some(5005));
        assert_eq!(generation("claude-sonnet-4-0"), Some(4000));
        assert_eq!(generation("claude-haiku-4-5-20251001"), Some(4005));
        assert_eq!(generation("claude-3-5-sonnet-20240620"), Some(3005));
        assert_eq!(generation("opus (1m context)"), None);
        assert_eq!(generation("gpt-4o"), None);
        assert_eq!(generation("model-20250101"), None);
    }

    use proptest::prelude::*;

    fn arb_case(text: &'static str) -> impl Strategy<Value = String> {
        proptest::collection::vec(proptest::bool::ANY, text.len())
            .prop_map(move |upper| text.chars().zip(upper).map(|(c, up)| if up { c.to_ascii_uppercase() } else { c }).collect())
    }

    proptest! {
        /// Every advertised value resolves to itself, in any letter case, in any list order.
        #[test]
        fn advertised_values_resolve_to_themselves(pick in 0usize..5, rotate in 0usize..5, case in proptest::bool::ANY) {
            let mut options = claude_options();
            let values = ["default", "opus[1m]", "claude-fable-5-1[1m]", "sonnet", "haiku"];
            if let Some(ConfigOption { kind: ConfigKind::Select { values, .. }, .. }) = options.iter_mut().find(|o| o.id == "model") {
                values.rotate_left(rotate);
            }
            let requested = if case { values[pick].to_uppercase() } else { values[pick].to_owned() };
            prop_assert_eq!(model(&options, &requested), Ok(values[pick].to_owned()));
        }

        /// `opus` in any letter case, with or without a vendor prefix, generation or context
        /// spelling, is the one Opus 5.5 offered, whatever the order of the list.
        #[test]
        fn opus_aliases_resolve_in_any_case_and_order(
            requested in prop_oneof![arb_case("opus"), arb_case("claude-opus-5-5"), arb_case("opus-1m"), arb_case("claude-opus-5.5[1m]")],
            rotate in 0usize..5,
        ) {
            let mut options = claude_options();
            if let Some(ConfigOption { kind: ConfigKind::Select { values, .. }, .. }) = options.iter_mut().find(|o| o.id == "model") {
                values.rotate_left(rotate);
            }
            prop_assert_eq!(model(&options, &requested), Ok("opus[1m]".to_owned()));
        }

        /// A request naming a generation never resolves to a value of another generation.
        #[test]
        fn generations_are_never_crossed(family in prop_oneof![Just("opus"), Just("fable"), Just("sonnet"), Just("haiku")], major in 1u64..10, minor in 0u64..10) {
            let options = claude_options();
            let requested = format!("claude-{family}-{major}-{minor}");
            if let Ok(value) = model(&options, &requested) {
                let ConfigKind::Select { values, .. } = &find(&options, &ConfigKey::Model).unwrap().kind else { panic!("model is a select") };
                let offered = values.iter().find(|v| v.value == value).unwrap();
                prop_assert_eq!(generation(&fold_text(&offered.name)), generation(&requested));
            }
        }

        /// Folding is idempotent and ignores letter case; both context spellings fold alike.
        #[test]
        fn folding_is_stable(base in "[a-zA-Z][a-zA-Z0-9.-]{0,12}", size in 1u32..999) {
            let claude = &CLAUDE_VARIANTS;
            let folded = fold_value(&base, claude);
            prop_assert_eq!(fold_value(&folded, claude), folded.clone());
            prop_assert_eq!(fold_value(&base.to_uppercase(), claude), folded);
            prop_assert_eq!(fold_value(&format!("{base}[{size}m]"), claude), fold_value(&format!("{base}-{size}M"), claude));
        }
    }
}
