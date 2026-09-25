//! Local `aim-daemon/1` transport over a protected unix socket.

pub mod client;
pub mod server;
pub mod spawn;

use std::path::{Path, PathBuf};

/// The daemon's default socket path under an aim home.
#[must_use]
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("run/daemon.sock")
}
