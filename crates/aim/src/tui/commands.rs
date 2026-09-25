//! Slash commands: the table the composer completes from and the parser the app dispatches on.

/// A slash command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    /// Name without the slash.
    pub name: &'static str,
    /// Argument hint (empty when it takes none).
    pub args: &'static str,
    /// One-line help.
    pub help: &'static str,
}

impl Command {
    /// Whether the command takes an argument.
    pub fn takes_argument(&self) -> bool {
        !self.args.is_empty()
    }
}

/// Every command, in the order the popup lists them.
pub const COMMANDS: &[Command] = &[
    Command { name: "model", args: "<id>", help: "switch model (applies from the next turn when one is running)" },
    Command { name: "effort", args: "<level>", help: "set reasoning effort (auto lets aim choose)" },
    Command { name: "provider", args: "<id>", help: "start a new session on another provider (its default model)" },
    Command { name: "new", args: "", help: "start a new session here; the chat so far stays above" },
    Command { name: "clear", args: "", help: "clear the chat and the screen, then start a new session" },
    Command { name: "sessions", args: "", help: "pick a session to attach or resume" },
    Command { name: "cancel", args: "", help: "cancel the running turn" },
    Command { name: "fullscreen", args: "", help: "toggle the fullscreen layout" },
    Command { name: "dictate", args: "", help: "voice input (arrives in M8)" },
    Command { name: "help", args: "", help: "list commands and keys" },
    Command { name: "quit", args: "", help: "exit aim" },
];

/// Looks a command up by name.
pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Splits `/name arg…` into the command name and its (trimmed) argument.
pub fn parse(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty() && !name.contains('/')).then_some((name, arg.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_splits_name_and_argument() {
        assert_eq!(parse("/model gpt-6"), Some(("model", "gpt-6")));
        assert_eq!(parse("  /quit "), Some(("quit", "")));
        assert_eq!(parse("/etc/hosts is odd"), None, "a path is not a command");
        assert_eq!(parse("hello"), None);
        assert!(find("sessions").is_some());
        assert!(find("model").is_some_and(Command::takes_argument));
        assert!(find("provider").is_some_and(Command::takes_argument));
        assert!(find("clear").is_some_and(|c| !c.takes_argument()));
    }
}
