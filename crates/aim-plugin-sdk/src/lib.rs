//! Rust guest SDK for the `aim:plugin@0.1.0` WebAssembly component world.
//!
//! Implement [`bindings::exports::aim::plugin::plugin::Guest`] and export the implementation with
//! `aim_plugin_sdk::bindings::export!(YourPlugin with_types_in aim_plugin_sdk::bindings)`.
//! The WIT source in `aim-plugin/wit` is the single canonical ABI definition.

/// Bindings generated from the host's canonical `aim:plugin@0.1.0` WIT world.
#[expect(
    missing_docs,
    clippy::same_length_and_capacity,
    clippy::mem_forget,
    reason = "wit-bindgen emits ABI glue and public types that cannot be edited here"
)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../aim-plugin/wit",
        world: "extension",
        pub_export_macro: true,
    });
}

pub use bindings::aim::plugin::types::{Action, Error, Event, Registration, ToolResult, ToolSpec};
pub use bindings::aim::plugin::{bus, host, kv, session, tools};
pub use bindings::exports::aim::plugin::plugin::Guest;

/// Makes a registration containing tools and no other extension hooks.
#[must_use]
pub fn tools_registration(tools: Vec<ToolSpec>) -> Registration {
    Registration {
        subscriptions: Vec::new(),
        tools,
        commands: Vec::new(),
        keybindings: Vec::new(),
        themes: Vec::new(),
        renders: Vec::new(),
    }
}

/// Makes a plain text result using the host's normalized `ToolResult` JSON shape.
#[must_use]
pub fn text_result(text: impl Into<String>) -> ToolResult {
    let content = serde_json::json!({
        "content": [{"type": "text", "text": text.into()}],
        "is_error": false,
    });
    ToolResult { content: content.to_string(), is_error: false, details: None }
}

#[cfg(test)]
mod tests {
    use super::{text_result, tools_registration};

    #[test]
    fn text_result_matches_normalized_tool_shape() {
        let result = text_result("hello");
        let parsed: serde_json::Value = serde_json::from_str(&result.content).expect("SDK JSON");
        assert_eq!(parsed["content"][0]["text"], "hello");
        assert!(!result.is_error);
    }

    #[test]
    fn registration_only_advertises_given_tools() {
        let registration = tools_registration(Vec::new());
        assert!(registration.tools.is_empty());
        assert!(registration.subscriptions.is_empty());
    }
}
