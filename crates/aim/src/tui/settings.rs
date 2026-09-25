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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomCommand {
    pub description: String,
    pub output: Output,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
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
        for name in settings.commands.keys() {
            if name.is_empty()
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || super::commands::find(name).is_some()
            {
                return Err(format!("invalid or reserved slash command name: {name}"));
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
