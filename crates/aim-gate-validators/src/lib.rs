//! Independent validators for candidate source trees.
//!
//! Build this crate into the pinned gate binary. Never load it or its manifest from a candidate.

mod locked;
pub mod source;
mod validation;

pub use locked::{check as check_locked, update as update_locked};
pub use validation::{ProtectedManifest, ValidationConfig, ValidationReport, validate};
