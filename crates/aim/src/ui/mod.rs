//! Agent-authored UI surfaces on the session side (docs/architecture.md §8.2, ADR 0017 and 0064).
//!
//! - [`tools`] — `ui_show`, `ui_update`, `ui_close` and `ui_catalog`, composed into native sessions;
//! - [`validate`] — a message checked against `aim/terminal@1` and the [`limits::Limits`];
//! - [`session`] — a session's surfaces (ownership, rate, accepted and published state) and the
//!   outlet the session actor installs around each turn so the tools reach them.
//!
//! The protocol itself (messages, the shared fold, the catalog, the A2UI adapter) is
//! [`aim_proto::ui`].

pub mod limits;
pub mod session;
pub mod tools;
pub mod validate;

use std::sync::Arc;

pub use session::{SessionUi, scope};

/// The UI tools for every native session ([`crate::host::NativeServices::tools`]).
#[must_use]
pub fn tools_factory() -> crate::host::ToolsFactory {
    Arc::new(|_spec: &aim_proto::daemon::SessionSpec| {
        let host: Arc<dyn crate::agent::ToolHost> = Arc::new(tools::UiTools);
        Box::pin(async move { Some(host) })
    })
}
