//! Example plugin that persists counters through the capability-gated host KV interface.

use aim_plugin_sdk::{Action, Error, Event, Guest, Registration, ToolResult, ToolSpec, kv, text_result, tools_registration};
use serde::Deserialize;

struct Counter;

#[derive(Deserialize)]
struct Arguments {
    #[serde(default = "default_key")]
    key: String,
}

fn default_key() -> String {
    "default".to_owned()
}

impl Guest for Counter {
    fn init(_config: String) -> Result<Registration, Error> {
        Ok(tools_registration(vec![ToolSpec {
            name: "increment".to_owned(),
            description: "Increase a named durable counter by one and return its new value.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
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
        if name != "increment" {
            return Err(Error::Invalid("unknown tool".to_owned()));
        }
        let Arguments { key } = serde_json::from_str(&args).map_err(|_| Error::Invalid("invalid arguments".to_owned()))?;
        if key.is_empty() || key.len() > 128 {
            return Err(Error::Invalid("key length must be 1..=128".to_owned()));
        }
        let old = kv::get(&key)?.map_or(Ok(0_u64), |bytes| {
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| Error::Invalid("stored counter is invalid".to_owned()))
        })?;
        let next = old.checked_add(1).ok_or_else(|| Error::Invalid("counter overflow".to_owned()))?;
        kv::set(&key, next.to_string().as_bytes())?;
        Ok(text_result(format!("{key}: {next}")))
    }

    fn run_command(_name: String, _args: String) -> Result<Vec<Action>, Error> {
        Err(Error::Invalid("no commands registered".to_owned()))
    }

    fn complete(_kind: String, _prefix: String) -> Vec<String> {
        Vec::new()
    }

    fn shutdown(_reason: String) {}
}

aim_plugin_sdk::bindings::export!(Counter with_types_in aim_plugin_sdk::bindings);
