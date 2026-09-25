//! Runtime slash actions. Calls use the user's workspace harness, never the model's turn/log.
use std::time::Duration;

use aim_proto::conversation::RateLimits;
use aim_proto::daemon::SessionSpec;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolContent;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::settings::{CustomCommand, ToolAction};
use crate::host::WorkspaceFactory;
use crate::resources::prompts::{Dialect, expand};

/// Maximum command output delivered to a UI or prompt, including its wrapper.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// A prepared effect, bound to the workspace at invocation time.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Tool { call: ToolAction, timeout_ms: u64 },
    Status,
}

/// A command's successful output; account snapshots never carry opaque provider fields.
#[derive(Debug)]
pub struct ResultText {
    pub text: String,
    pub limits: Option<RateLimits>,
}

/// Replaces only placeholders present in the original template, not in arguments or tool output.
pub fn render(template: Option<&str>, args: &str, output: &str) -> String {
    template.map_or_else(
        || output.to_owned(),
        |template| template.split("{{output}}").map(|part| expand(part, Dialect::Aim, &[], args)).collect::<Vec<_>>().join(output),
    )
}

fn arguments(value: &Value, args: &str) -> Value {
    match value {
        Value::String(text) => Value::String(expand(text, Dialect::Aim, &[], args)),
        Value::Array(items) => Value::Array(items.iter().map(|item| arguments(item, args)).collect()),
        Value::Object(fields) => Value::Object(fields.iter().map(|(key, value)| (key.clone(), arguments(value, args))).collect()),
        other => other.clone(),
    }
}

/// Shell-quote each argv element; user input is data, not shell syntax.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

pub fn prepare(command: &CustomCommand, args: &str) -> Option<Action> {
    let call = if let Some(argv) = &command.run {
        let line = argv.iter().map(|arg| quote(&expand(arg, Dialect::Aim, &[], args))).collect::<Vec<_>>().join(" ");
        ToolAction { name: "Bash".into(), arguments: json!({"command": line, "timeout": command.timeout_ms}) }
    } else {
        let tool = command.tool.as_ref()?;
        ToolAction { name: tool.name.clone(), arguments: arguments(&tool.arguments, args) }
    };
    Some(Action::Tool { call, timeout_ms: command.timeout_ms })
}

pub fn bounded(text: String) -> Result<String, String> {
    if text.len() > MAX_OUTPUT { Err("slash command output exceeds 64 KiB; reduce the command's output".into()) } else { Ok(text) }
}

/// An invocation is never retried automatically: its tool may mutate the workspace.
pub async fn execute(
    action: Action,
    spec: SessionSpec,
    workspaces: WorkspaceFactory,
    cancel: CancellationToken,
) -> Result<ResultText, String> {
    match action {
        Action::Status => {
            let provider = crate::providers::codex()?;
            let limits = tokio::select! {
                () = cancel.cancelled() => return Err("slash command cancelled".into()),
                result = provider.account_limits() => result.map_err(|e| e.to_string())?,
            };
            let text = super::commands::status(&limits);
            Ok(ResultText { text, limits: Some(limits) })
        }
        Action::Tool { call, timeout_ms } => {
            if call.name == "Bash" && call.arguments.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                return Err("slash commands must run in the foreground; background process handles are invocation-local".into());
            }
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
            let connected = tokio::select! {
                () = cancel.cancelled() => return Err("slash command cancelled".into()),
                result = tokio::time::timeout_at(deadline, workspaces(&spec)) =>
                    result.map_err(|_| "slash command timed out connecting to the workspace")?.map_err(|e| e.message)?,
            };
            let result = tokio::select! {
                () = cancel.cancelled() => Err("slash command cancelled".to_owned()),
                result = tokio::time::timeout_at(deadline, connected.tools.call(
                    call.name, call.arguments, IdempotencyKey::new(format!("slash-{}", uuid::Uuid::new_v4())),
                )) => result.map_err(|_| "slash command timed out".to_owned()).and_then(|r| r.map_err(|e| e.message)),
            };
            // Release the tool and project references before shutdown's Arc::try_unwrap. Closing
            // the harness also releases any processes/output handles owned by this invocation.
            drop(connected.tools);
            drop(connected.project);
            (connected.shutdown)().await;
            let result = result?;
            if result.truncated {
                return Err("slash command result was truncated; reduce its output".into());
            }
            let mut parts = Vec::new();
            for content in result.content {
                match content {
                    ToolContent::Text { text } => parts.push(text),
                    ToolContent::Image { .. } => return Err("slash command returned non-text content".into()),
                }
            }
            let text = bounded(parts.join("\n"))?;
            if result.is_error {
                return Err(text);
            }
            Ok(ResultText { text, limits: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_do_not_reinterpret_arguments_or_output() {
        assert_eq!(render(Some("$ARGUMENTS: {{output}}"), "{{output}}", "$ARGUMENTS /quit"), "{{output}}: $ARGUMENTS /quit");
        assert_eq!(quote("a'b; $(echo bad)"), "'a'\\''b; $(echo bad)'");
    }
}
