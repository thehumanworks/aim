//! Pure Seatbelt profile construction shared by the gate and future local executor policy.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

/// Network authority granted to a sandboxed process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// No socket access.
    #[default]
    Off,
    /// Only one numeric loopback TCP port. `listen` is for an evaluator-owned mock proxy.
    LoopbackPort {
        /// The one allowed TCP port.
        port: u16,
        /// Whether the sandbox may bind and accept on that port.
        listen: bool,
    },
    /// Temporary W28 test-only exception for tests that bind dynamic localhost ports.
    TestLoopback,
}

/// Inputs to a Seatbelt profile. The caller must pass canonical absolute paths.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Directories candidates may read.
    pub readable: Vec<PathBuf>,
    /// Directories candidates may mutate.
    pub writable: Vec<PathBuf>,
    /// Explicit private subtrees withheld even if a readable ancestor is present.
    pub denied: Vec<PathBuf>,
    /// Network authority.
    pub network: Network,
}

impl Policy {
    /// Render a complete deny-by-default Seatbelt policy without consulting the filesystem.
    ///
    /// # Errors
    /// A path is relative or contains characters unsafe for Seatbelt profile parsing, or the
    /// requested port is zero.
    pub fn render(&self) -> Result<String, &'static str> {
        let mut result = String::from(
            "(version 1)\n(deny default)\n(allow process-fork)\n(allow process-exec)\n(allow sysctl-read)\n(allow file-read* (literal \"/\"))\n(allow file-write* (literal \"/dev/null\"))\n",
        );
        // Darwin tools need metadata access along each permitted path (for example, Clang
        // resolves its installed directory). Literal parents grant traversal, not subtree reads.
        let mut parents = BTreeSet::new();
        for path in self.readable.iter().chain(&self.writable) {
            quote_path(path)?;
            for parent in path.ancestors().skip(1).filter(|parent| *parent != Path::new("/")) {
                parents.insert(parent.to_path_buf());
            }
        }
        for parent in parents {
            writeln!(&mut result, "(allow file-read* (literal {}))", quote_path(&parent)?)
                .map_err(|_| "Seatbelt output allocation failed")?;
        }
        for path in &self.readable {
            result.push_str("(allow file-read* (subpath ");
            result.push_str(&quote_path(path)?);
            result.push_str("))\n");
        }
        for path in &self.writable {
            result.push_str("(allow file-write* (subpath ");
            result.push_str(&quote_path(path)?);
            result.push_str("))\n");
        }
        for path in &self.denied {
            let path = quote_path(path)?;
            result.push_str("(deny file-read* (subpath ");
            result.push_str(&path);
            result.push_str("))\n(deny file-write* (subpath ");
            result.push_str(&path);
            result.push_str("))\n");
        }
        match self.network {
            Network::Off => {}
            Network::LoopbackPort { port, listen } => {
                if port == 0 {
                    return Err("Seatbelt loopback port must be nonzero");
                }
                let endpoint = format!("\"localhost:{port}\"");
                writeln!(&mut result, "(allow network-outbound (remote ip {endpoint}))")
                    .map_err(|_| "Seatbelt output allocation failed")?;
                if listen {
                    writeln!(&mut result, "(allow network-bind (local ip {endpoint}))").map_err(|_| "Seatbelt output allocation failed")?;
                    writeln!(&mut result, "(allow network-inbound (local ip {endpoint}))")
                        .map_err(|_| "Seatbelt output allocation failed")?;
                }
            }
            Network::TestLoopback => {
                result.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
                result.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
                result.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
                for root in &self.writable {
                    let path = quote_path(root)?;
                    writeln!(&mut result, "(allow network-outbound (remote unix-socket (subpath {path})))")
                        .map_err(|_| "Seatbelt output allocation failed")?;
                    writeln!(&mut result, "(allow network-bind (local unix-socket (subpath {path})))")
                        .map_err(|_| "Seatbelt output allocation failed")?;
                    writeln!(&mut result, "(allow network-inbound (local unix-socket (subpath {path})))")
                        .map_err(|_| "Seatbelt output allocation failed")?;
                }
            }
        }
        Ok(result)
    }
}

fn quote_path(path: &Path) -> Result<String, &'static str> {
    let text = path.to_str().ok_or("Seatbelt path is not UTF-8")?;
    if !path.is_absolute()
        || text.contains(['\n', '\r', '\0'])
        || text == "/"
        || path.components().any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("Seatbelt paths must be specific, absolute, normalized, UTF-8, and single-line");
    }
    Ok(format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_deny_by_default_and_fixed_port() {
        let policy = Policy {
            readable: vec![PathBuf::from("/workspace")],
            writable: vec![PathBuf::from("/workspace/candidate")],
            denied: vec![PathBuf::from("/workspace/.aim-gate")],
            network: Network::LoopbackPort { port: 41111, listen: false },
        };
        let profile = policy.render().unwrap();
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-read* (literal \"/\"))"));
        assert!(profile.contains("(allow file-read* (literal \"/workspace\"))"));
        assert!(profile.contains("(allow sysctl-read)"));
        assert!(profile.contains("(allow file-write* (literal \"/dev/null\"))"));
        assert!(!profile.contains("(subpath \"/\")"));
        assert!(profile.contains("(remote ip \"localhost:41111\")"));
        assert!(!profile.contains("localhost:*"));
        assert!(profile.contains("(deny file-read* (subpath \"/workspace/.aim-gate\"))"));
        assert!(Policy { readable: vec![PathBuf::from("relative")], ..Policy::default() }.render().is_err());
        let test_profile = Policy { network: Network::TestLoopback, ..Policy::default() }.render().unwrap();
        assert!(test_profile.contains("(remote ip \"localhost:*\")"));
        assert!(!test_profile.contains("(allow network*)"));
    }
}
