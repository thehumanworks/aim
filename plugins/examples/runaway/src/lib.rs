//! Deliberately nonterminating guest fixture for host fuel and epoch regression tests.

use aim_plugin_sdk::{Action, Error, Event, Guest, Registration, ToolResult, ToolSpec, tools_registration};

struct Runaway;

impl Guest for Runaway {
    fn init(_config: String) -> Result<Registration, Error> {
        Ok(tools_registration(vec![ToolSpec {
            name: "runaway".to_owned(),
            description: "Test fixture that loops until the host terminates its call.".to_owned(),
            input_schema: "{\"type\":\"object\"}".to_owned(),
            sensitive: false,
        }]))
    }

    fn on_event(_ev: Event) -> Vec<Action> {
        Vec::new()
    }

    fn call_tool(_name: String, _call_id: String, _args: String) -> Result<ToolResult, Error> {
        loop {
            std::hint::spin_loop();
        }
    }

    fn run_command(_name: String, _args: String) -> Result<Vec<Action>, Error> {
        Err(Error::Invalid("no commands registered".to_owned()))
    }

    fn complete(_kind: String, _prefix: String) -> Vec<String> {
        Vec::new()
    }

    fn shutdown(_reason: String) {}
}

aim_plugin_sdk::bindings::export!(Runaway with_types_in aim_plugin_sdk::bindings);
