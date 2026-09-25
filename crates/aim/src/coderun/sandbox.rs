//! The macOS Seatbelt profile of the code worker (ADR 0066, `REV13a` M1).
//!
//! The profile is deny-default. It grants only what the `QuickJS` worker needs to start and run,
//! and each grant was found by removing it and watching the worker fail:
//! - `process-exec` and `file-read*` of the worker itself, by its canonical path;
//! - `file-read*` of `/usr/lib` (the worker's dylibs) and of the dyld shared-cache directories.
//!   macOS has kept the cache in three places over the years, so all three are listed;
//! - `file-read-data` of `/`, which dyld reads at startup;
//! - `sysctl-read` of `hw.*`, which Rust's runtime reads (CPU count, page size).
//!
//! Everything else is denied:
//! - other files: `/private/tmp`, other users' and agents' directories, `/Volumes`, the per-user
//!   temp and cache directories, and the time zone files, so JS `Date` is UTC;
//! - fork, and exec of any other binary;
//! - network, and mach lookups (keychain and other system daemons);
//! - other processes' arguments (`kern.procargs2`).
//!
//! The worker writes nothing, so no temp directory is granted.
//!
//! [`profile`] is a small pure function, so it converges with W28's reusable Seatbelt module: this
//! profile is that module's `readable` set, plus an exec literal, with network off.

use std::fmt::Write as _;
use std::path::{Component, Path};

/// Where macOS keeps the dyld shared cache: Big Sur and later without cryptexes, then the
/// cryptex paths of Ventura and later (the firmlinked `/System/Cryptexes` and its Preboot volume).
const DYLD_CACHES: [&str; 3] =
    ["/System/Library/dyld", "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld", "/System/Cryptexes/OS/System/Library/dyld"];

/// The profile under which `worker`, a canonical absolute path, runs. `None` when the path
/// cannot be written into a profile safely: not UTF-8, relative, not normalized, or multi-line.
#[must_use]
pub fn profile(worker: &Path) -> Option<String> {
    let worker = quoted(worker)?;
    let mut profile = String::from("(version 1)\n(deny default)\n");
    writeln!(profile, "(allow process-exec (literal {worker}))").ok()?;
    writeln!(profile, "(allow file-read* (literal {worker}))").ok()?;
    profile.push_str("(allow file-read* (subpath \"/usr/lib\")");
    for cache in DYLD_CACHES {
        write!(profile, " (subpath \"{cache}\")").ok()?;
    }
    profile.push_str(")\n(allow file-read-data (literal \"/\"))\n(allow sysctl-read (sysctl-name-prefix \"hw.\"))\n");
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
    fn the_profile_is_deny_default_and_names_only_the_worker() {
        let text = profile(Path::new("/opt/aim/aim-coderun")).unwrap();
        assert!(text.starts_with("(version 1)\n(deny default)\n"));
        assert!(!text.contains("allow default"));
        assert!(!text.contains("network"));
        assert!(!text.contains("process-fork"));
        assert!(!text.contains("mach-lookup"));
        assert!(!text.contains("file-write"));
        assert_eq!(text.matches("process-exec").count(), 1);
        assert!(text.contains("(allow process-exec (literal \"/opt/aim/aim-coderun\"))"));
        assert!(text.contains("(sysctl-name-prefix \"hw.\")"));
    }

    #[test]
    fn unsafe_paths_are_refused() {
        assert!(profile(Path::new("relative/aim-coderun")).is_none());
        assert!(profile(Path::new("/opt/../tmp/aim-coderun")).is_none());
        assert!(profile(Path::new("/opt/aim\n(allow default)")).is_none());
        assert!(profile(Path::new("/")).is_none());
        assert!(profile(Path::new("/opt/a \"quoted\" name")).unwrap().contains("\"/opt/a \\\"quoted\\\" name\""));
    }

    /// Runs `program` as if it were the worker, under exactly the worker's profile (`REV13a` M1).
    #[cfg(target_os = "macos")]
    fn sandboxed(program: &str, args: &[&str]) -> std::process::Output {
        let text = profile(Path::new(program)).unwrap();
        std::process::Command::new("/usr/bin/sandbox-exec").arg("-p").arg(text).arg(program).args(args).output().unwrap()
    }

    #[cfg(target_os = "macos")]
    fn text(output: &std::process::Output) -> String {
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
    }

    /// The `REV13a` M1 probes, run as tests: what the old `(allow default)` profile let through.
    /// Each probe is a bash builtin, because the profile denies fork, and each must be refused.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_sandbox_denies_reads_exec_network_and_the_process_list() {
        let getconf = |name: &str| {
            let output = std::process::Command::new("/usr/bin/getconf").arg(name).output().unwrap();
            String::from_utf8_lossy(&output.stdout).trim().trim_end_matches('/').to_owned()
        };
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_owned());
        let bash = |script: &str| text(&sandboxed("/bin/bash", &["--norc", "--noprofile", "-c", script]));
        for directory in [
            "/private/tmp",
            "/tmp",
            "/Volumes",
            "/opt/homebrew",
            "/Library",
            "/private/var/folders",
            &getconf("DARWIN_USER_TEMP_DIR"),
            &getconf("DARWIN_USER_CACHE_DIR"),
            &home,
            "/System/Volumes/Data/private/tmp",
            "/System/Volumes/Data/Users",
        ] {
            // A glob that expands means the directory could be listed.
            let seen = bash(&format!("set -- \"{directory}\"/*; [ \"$1\" != \"{directory}/*\" ] && echo LISTED"));
            assert!(!seen.contains("LISTED"), "{directory} was listable: {seen}");
        }
        let seen = bash("read -r line < /etc/hosts && echo READ");
        assert!(!seen.contains("READ") && seen.contains("Operation not permitted"), "{seen}");
        let seen = bash("true 3<>/dev/tcp/1.1.1.1/80 && echo NETWORK");
        assert!(!seen.contains("NETWORK") && seen.contains("not permitted"), "{seen}");
        let seen = bash("exec /bin/echo EXECUTED");
        assert!(!seen.contains("EXECUTED") && seen.contains("Operation not permitted"), "{seen}");
        let seen = bash("exec /usr/bin/osascript -e 'return 1+1'");
        assert!(seen.contains("Operation not permitted"), "{seen}");

        // Other processes' arguments: the old profile answered KERN_PROCARGS2.
        let listed = sandboxed("/usr/bin/pgrep", &["-lf", "."]);
        assert!(!listed.status.success(), "the process list was readable: {}", text(&listed));
        // Mach IPC to system daemons: the old profile answered `security list-keychains`.
        let keychains = sandboxed("/usr/bin/security", &["list-keychains"]);
        assert!(!text(&keychains).contains(".keychain"), "keychains were listed: {}", text(&keychains));
    }
}
