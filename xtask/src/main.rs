//! `cargo xtask`: repository invariants that the quality gate enforces.
//!
//! - `cargo xtask check` — ADR hygiene, kernel API rules, LOCKED decision digests.
//! - `cargo xtask locked --update` — re-record LOCKED digests after a maintainer-approved change.
//!
//! These checks turn promises in docs/architecture.md into mechanics: a `LOCKED` spec in
//! `aim-kernel` cannot change (directly or through a helper it references) without its digest
//! changing, and the digest manifest is part of the protected set.
#![expect(clippy::print_stderr, reason = "xtask is a CLI whose output is its findings on stderr")]

mod adr;
mod kernel;
mod locked;
mod source;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn repo_root() -> PathBuf {
    // xtask lives at <root>/xtask; CARGO_MANIFEST_DIR is set by cargo for `cargo xtask`.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map_or_else(|| PathBuf::from("xtask"), PathBuf::from);
    manifest.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn report(findings: &[String]) -> bool {
    for finding in findings {
        eprintln!("xtask: {finding}");
    }
    findings.is_empty()
}

fn main() -> ExitCode {
    let root = repo_root();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ok = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["check"] => {
            let mut findings = adr::check(&root);
            findings.extend(kernel::check(&root));
            findings.extend(locked::check(&root));
            report(&findings)
        }
        ["locked", "--update"] => report(&locked::update(&root)),
        ["locked"] => report(&locked::check(&root)),
        _ => report(&["usage: cargo xtask check | cargo xtask locked [--update]".to_owned()]),
    };
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
