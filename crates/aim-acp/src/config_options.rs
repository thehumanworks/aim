//! Session configuration options (`session/set_config_option`): model, effort, mode, fast mode.
//! The values are the agent's data, never aim enums (AGENTS.md "Capabilities are data").

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AcpError;
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
    })
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

/// Validates `value` for the option `key` and returns the `session/set_config_option` params
/// (without `sessionId`) plus the resolved option id.
///
/// # Errors
///
/// [`AcpError::ConfigUnavailable`] when the option is not advertised,
/// [`AcpError::ConfigValueRejected`] when `value` is not one of its values.
pub fn set_params(options: &[ConfigOption], key: &ConfigKey, value: &str) -> Result<(String, Value), AcpError> {
    let option = find(options, key).ok_or_else(|| AcpError::ConfigUnavailable { key: key.label().to_owned() })?;
    let rejected = || AcpError::ConfigValueRejected { id: option.id.clone(), value: value.to_owned(), allowed: option.allowed() };
    let params = match &option.kind {
        ConfigKind::Select { values, .. } => {
            if !values.iter().any(|v| v.value == value) {
                return Err(rejected());
            }
            serde_json::json!({"configId": option.id, "value": value})
        }
        ConfigKind::Boolean { .. } => {
            let flag = match value {
                "true" | "on" => true,
                "false" | "off" => false,
                _ => return Err(rejected()),
            };
            serde_json::json!({"configId": option.id, "type": "boolean", "value": flag})
        }
        ConfigKind::Other { .. } => return Err(rejected()),
    };
    Ok((option.id.clone(), params))
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
        assert_eq!(set_params(&options, &ConfigKey::Mode, "plan").unwrap(), ("mode".into(), json!({"configId": "mode", "value": "plan"})));
        assert_eq!(
            set_params(&options, &ConfigKey::Id("speed".into()), "on").unwrap().1,
            json!({"configId": "speed", "type": "boolean", "value": true})
        );
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
}
