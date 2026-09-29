//! Example plugin that delegates a read to the session's permissioned tool dispatcher.

use aim_plugin_sdk::{Action, Error, Event, Guest, Registration, ToolResult, ToolSpec, tools, tools_registration};
use serde::Deserialize;

struct DelegateRead;

#[derive(Deserialize)]
struct Arguments {
    file_path: String,
}

impl Guest for DelegateRead {
    fn init(_config: String) -> Result<Registration, Error> {
        Ok(tools_registration(vec![ToolSpec {
            name: "delegate_read".to_owned(),
            description: "Read a workspace file through the session's permissioned read tool.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"file_path": {"type": "string"}},
                "required": ["file_path"],
                "additionalProperties": false
            })
            .to_string(),
            sensitive: false,
        }]))
    }

    fn on_event(_ev: Event) -> Vec<Action> {
        Vec::new()
    }

    fn call_tool(name: String, _call_id: String, args: String) -> Result<ToolResult, Error> {
        if name != "delegate_read" {
            return Err(Error::Invalid("unknown tool".to_owned()));
        }
        let Arguments { file_path } = serde_json::from_str(&args).map_err(|_| Error::Invalid("invalid arguments".to_owned()))?;
        let request = serde_json::json!({ "file_path": file_path }).to_string();
        tools::call("read", &request)
    }

    fn run_command(_name: String, _args: String) -> Result<Vec<Action>, Error> {
        Err(Error::Invalid("no commands registered".to_owned()))
    }

    fn complete(_kind: String, _prefix: String) -> Vec<String> {
        Vec::new()
    }

    fn shutdown(_reason: String) {}
}

aim_plugin_sdk::bindings::export!(DelegateRead with_types_in aim_plugin_sdk::bindings);
