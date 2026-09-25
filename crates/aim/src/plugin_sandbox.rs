//! Deny-default macOS Seatbelt profile for the lazy plugin worker.
//!
//! This mirrors the tested code-worker profile from FIX16. The worker needs two writable trees:
//! private plugin KV and compiled-code cache. The caller creates those owner-private trees,
//! canonicalizes all paths, and launches `sandbox-exec -p <profile> <worker>` with cleared
//! environment and piped stdio. The one `process-exec` grant permits the initial worker launch;
//! Seatbelt does not distinguish that launch from a later attempt to execute the same binary.

use std::fmt::Write as _;
use std::path::{Component, Path};

// dyld shared-cache locations across supported macOS releases.
const DYLD_CACHES: [&str; 3] =
    ["/System/Library/dyld", "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld", "/System/Cryptexes/OS/System/Library/dyld"];

/// Render the worker's profile from canonical absolute paths.
///
/// Returns `None` if a path is unsafe to embed. `kv_dir` and `cache_dir` are dedicated,
/// previously created private directories, never ancestors such as the user's home directory.
#[must_use]
pub fn profile(worker: &Path, kv_dir: &Path, cache_dir: &Path) -> Option<String> {
    let worker = quoted(worker)?;
    let kv_dir = quoted(kv_dir)?;
    let cache_dir = quoted(cache_dir)?;
    let mut profile = String::from("(version 1)\n(deny default)\n");
    writeln!(profile, "(allow process-exec (literal {worker}))").ok()?;
    writeln!(profile, "(allow file-read* (literal {worker}))").ok()?;
    profile.push_str("(allow file-read* (subpath \"/usr/lib\")");
    for cache in DYLD_CACHES {
        write!(profile, " (subpath \"{cache}\")").ok()?;
    }
    profile.push_str(")\n(allow file-read-data (literal \"/\"))\n(allow sysctl-read (sysctl-name-prefix \"hw.\"))\n");
    for private_dir in [kv_dir, cache_dir] {
        writeln!(profile, "(allow file-read* (subpath {private_dir}))").ok()?;
        writeln!(profile, "(allow file-write* (subpath {private_dir}))").ok()?;
    }
    Some(profile)
}

fn quoted(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let normalized = path.is_absolute() && path.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
    if !normalized || text == "/" || text.contains(['\n', '\r', '\0']) {
        return None;
    }
    Some(format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::profile;

    #[test]
    fn profile_grants_only_worker_system_runtime_and_private_store() {
        let text = profile(
            Path::new("/opt/aim/aim-plugind"),
            Path::new("/Users/agent/.aim/plugin-kv"),
            Path::new("/Users/agent/.aim/plugin-cache"),
        )
        .unwrap();
        assert!(text.starts_with("(version 1)\n(deny default)\n"));
        assert_eq!(text.matches("process-exec").count(), 1);
        assert!(text.contains("(allow process-exec (literal \"/opt/aim/aim-plugind\"))"));
        assert!(text.contains("(allow file-read* (subpath \"/Users/agent/.aim/plugin-kv\"))"));
        assert!(text.contains("(allow file-write* (subpath \"/Users/agent/.aim/plugin-kv\"))"));
        assert!(text.contains("(allow file-read* (subpath \"/Users/agent/.aim/plugin-cache\"))"));
        assert!(text.contains("(allow file-write* (subpath \"/Users/agent/.aim/plugin-cache\"))"));
        for forbidden in ["allow default", "process-fork", "network", "mach-lookup", "(subpath \"/Users\")"] {
            assert!(!text.contains(forbidden), "unexpected grant: {forbidden}");
        }
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let worker = Path::new("/opt/aim/aim-plugind");
        for bad in ["relative", "/", "/opt/../Users", "/tmp/\n(allow default)"] {
            assert!(profile(worker, Path::new(bad), Path::new("/opt/aim/plugin-cache")).is_none());
            assert!(profile(worker, Path::new("/opt/aim/plugin-kv"), Path::new(bad)).is_none());
            assert!(profile(Path::new(bad), Path::new("/opt/aim/plugin-kv"), Path::new("/opt/aim/plugin-cache")).is_none());
        }
    }
}
