//! User-owned TUI configuration. Never loads executable project configuration.

use std::collections::BTreeMap;
use std::io::Read as _;

use serde::Deserialize;

/// Personal presentation and command definitions.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub commands: BTreeMap<String, CustomCommand>,
    pub fullscreen: bool,
    pub plain: bool,
    pub status: Vec<StatusField>,
}

/// Optional status-line segments, in the user's chosen order.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusField {
    Tokens,
    Limits,
    Workspace,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            commands: BTreeMap::new(),
            fullscreen: false,
            plain: false,
            status: vec![StatusField::Tokens, StatusField::Limits, StatusField::Workspace],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomCommand {
    pub description: String,
    pub output: Output,
    /// A template, or an optional wrapper around a runtime result (`{{output}}`).
    pub text: Option<String>,
    /// Execute an argv vector through the workspace's Bash tool, with shell-safe quoting.
    pub run: Option<Vec<String>>,
    /// Invoke a harness tool directly, expanding only string argument values.
    pub tool: Option<ToolAction>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    aim_kernel::slash::DEFAULT_TIMEOUT_MS
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAction {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    Agent,
    User,
}

impl Settings {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::metadata(path) {
            Ok(meta) if !meta.is_file() => return Err("TUI settings must be a regular file".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("TUI settings: {e}")),
        }
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("TUI settings: {e}")),
        };
        let mut bytes = Vec::new();
        file.take(65_537).read_to_end(&mut bytes).map_err(|e| format!("TUI settings: {e}"))?;
        if bytes.len() > 65_536 {
            return Err("TUI settings exceed 64 KiB".into());
        }
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let settings: Self = serde_json::from_slice(bytes).map_err(|e| format!("TUI settings: {e}"))?;
        for (name, command) in &settings.commands {
            if name.is_empty()
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || super::commands::find(name).is_some()
            {
                return Err(format!("invalid or reserved slash command name: {name}"));
            }
            if (command.run.is_some() && command.tool.is_some())
                || (command.run.is_none() && command.tool.is_none() && command.text.is_none())
                || command.run.as_ref().is_some_and(|argv| argv.first().is_none_or(|program| program.trim().is_empty()))
                || command.tool.as_ref().is_some_and(|tool| tool.name.trim().is_empty() || !tool.arguments.is_object())
                || aim_kernel::slash::timeout(command.timeout_ms).is_none()
            {
                return Err(format!("/{name}: provide text, run, or tool (run and tool are exclusive), and timeout_ms in 1..=600000"));
            }
        }
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reads_are_optional_and_bounded() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("tui.json");
        assert!(Settings::load(&path).unwrap().commands.is_empty());
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(Settings::load(&path).unwrap().status.len(), 3);
        std::fs::write(&path, vec![b' '; 65_537]).unwrap();
        assert!(Settings::load(&path).err().unwrap().contains("64 KiB"));
        assert!(Settings::load(home.path()).is_err());
    }

    #[test]
    fn runtime_actions_require_a_valid_shape_and_bounded_deadline() {
        for action in [
            serde_json::json!({"run":[]}),
            serde_json::json!({"run":[""]}),
            serde_json::json!({"run":["echo"],"tool":{"name":"Read","arguments":{}}}),
            serde_json::json!({"tool":{"name":"Read","arguments":[]}}),
            serde_json::json!({"run":["echo"],"timeout_ms":0}),
            serde_json::json!({"run":["echo"],"timeout_ms":600_001}),
            serde_json::json!({}),
        ] {
            let mut command = action.as_object().unwrap().clone();
            command.insert("description".into(), "test".into());
            command.insert("output".into(), "user".into());
            let config = serde_json::json!({"commands":{"test":command}});
            assert!(Settings::parse(config.to_string().as_bytes()).is_err());
        }
        for action in [
            serde_json::json!({"run":["git","status"],"timeout_ms":600_000}),
            serde_json::json!({"tool":{"name":"Read","arguments":{"file_path":"$1"}},"text":"{{output}}"}),
        ] {
            let mut command = action.as_object().unwrap().clone();
            command.insert("description".into(), "test".into());
            command.insert("output".into(), "agent".into());
            let config = serde_json::json!({"commands":{"test":command}});
            assert!(Settings::parse(config.to_string().as_bytes()).is_ok());
        }
    }

    #[test]
    fn config_rejects_ambiguous_commands_and_unknown_fields() {
        for name in ["status", "help", "a/b", "", "two words"] {
            let value = serde_json::json!({"commands": {name: {"description": "test", "output": "user", "text": "hello"}}});
            assert!(Settings::parse(value.to_string().as_bytes()).is_err());
        }
        assert!(Settings::parse(br#"{"commands":{"x":{"description":"x","output":"agent","text":"$ARGUMENTS"}}}"#).is_ok());
        assert!(Settings::parse(br#"{"unknown":true}"#).is_err());
        for value in [
            br#"{"commands":{"x":{"description":"x","text":"hello"}}}"#.as_slice(),
            br#"{"commands":{"x":{"description":"x","output":"typo","text":"hello"}}}"#.as_slice(),
            br#"{"commands":{"x":{"description":"x","output":"user","text":"hello","shell":"bad"}}}"#.as_slice(),
            br#"{"status":["unknown"]}"#.as_slice(),
        ] {
            assert!(Settings::parse(value).is_err());
        }
    }
}
