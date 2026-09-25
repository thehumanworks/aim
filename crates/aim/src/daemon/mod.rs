//! Local `aim-daemon/1` transport over a protected unix socket.

pub mod client;
pub mod server;
pub mod spawn;

use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use sha2::Digest as _;

/// Longest socket path aim binds: macOS `sun_path` holds 104 bytes including the NUL.
const MAX_SOCKET_PATH: usize = 100;

/// The daemon's socket for an aim home: `<home>/run/daemon.sock`, or — when that path is too long
/// for a unix socket (a deep `AIM_HOME`) — `/tmp/aim-<uid>/<hash of home>.sock`, in a private
/// per-user directory the daemon verifies (0700, owned, not a symlink). The lock and pid files
/// stay in the home either way.
#[must_use]
pub fn socket_path(home: &Path) -> PathBuf {
    let preferred = home.join("run/daemon.sock");
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH {
        return preferred;
    }
    let digest = sha2::Sha256::digest(home.as_os_str().as_bytes());
    let name = digest.iter().take(8).fold(String::with_capacity(16), |mut name, b| {
        let _infallible = write!(name, "{b:02x}");
        name
    });
    PathBuf::from(format!("/tmp/aim-{}", nix::unistd::Uid::current().as_raw())).join(format!("{name}.sock"))
}
