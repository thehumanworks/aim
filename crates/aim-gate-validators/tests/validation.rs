//! Tiny repository fixtures for the pinned gate validators.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use aim_gate_validators::{ProtectedManifest, ValidationConfig, ValidationReport, update_locked, validate};

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    candidate: PathBuf,
    config: ValidationConfig,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let base = temp.path().join("base");
        let candidate = temp.path().join("candidate");
        for root in [&base, &candidate] {
            write(root, "Cargo.lock", "version = 4\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n")?;
            write(
                root,
                "crates/aim-kernel/src/decision.rs",
                "/// LOCKED(ADR-0005)\npub open spec fn decision(x: nat) -> bool { x > 0 }\n",
            )?;
            write(root, "docs/adr/0005-fixture.md", "# fixture\n")?;
            write(root, "src/lib.rs", "#[test]\nfn stable() {}\n")?;
            write(root, "xtask/src/main.rs", "fn main() {}\n")?;
        }
        let findings = update_locked(&base);
        if !findings.is_empty() {
            return Err(format!("fixture LOCKED update failed: {findings:?}").into());
        }
        fs::copy(base.join("crates/aim-kernel/LOCKED.toml"), candidate.join("crates/aim-kernel/LOCKED.toml"))?;
        let manifest = ProtectedManifest::embedded()?;
        let config = ValidationConfig {
            manifest,
            baseline_test_inventory: "Running unittests src/lib.rs (target/debug/deps/fixture-1111111111111111)\nstable: test\n".to_owned(),
            candidate_test_inventory: "Running unittests src/lib.rs (target/debug/deps/fixture-2222222222222222)\nstable: test\n"
                .to_owned(),
        };
        Ok(Self { _temp: temp, base, candidate, config })
    }

    fn validate(&self) -> Result<ValidationReport, String> {
        validate(&self.base, &self.candidate, &self.config)
    }
}

fn write(root: &Path, relative: &str, contents: &str) -> Result<(), Box<dyn Error>> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn assert_finding(report: &ValidationReport, fragment: &str) {
    assert!(report.findings.iter().any(|finding| finding.contains(fragment)), "missing {fragment}: {:?}", report.findings);
}

#[test]
fn unchanged_fixture_passes() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    assert!(fixture.validate()?.passed());
    Ok(())
}

#[test]
fn changed_locked_spec_is_rejected() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(
        &fixture.candidate,
        "crates/aim-kernel/src/decision.rs",
        "/// LOCKED(ADR-0005)\npub open spec fn decision(x: nat) -> bool { x >= 0 }\n",
    )?;
    assert_finding(&fixture.validate()?, "LOCKED decision");
    Ok(())
}

#[test]
fn deleted_compiled_test_is_rejected() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config.candidate_test_inventory =
        "Running unittests src/lib.rs (target/debug/deps/fixture-2222222222222222)\nreplacement: test\n".to_owned();
    assert_finding(&fixture.validate()?, "compiled test deleted");
    Ok(())
}

#[test]
fn newly_ignored_test_is_rejected() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.candidate, "src/lib.rs", "#[test]\n#[ignore]\nfn stable() {}\n")?;
    assert_finding(&fixture.validate()?, "newly ignored");
    Ok(())
}

#[test]
fn newly_cfg_gated_test_is_rejected_even_if_listed() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.candidate, "src/lib.rs", "#[cfg(target_os = \"macos\")]\n#[test]\nfn stable() {}\n")?;
    assert_finding(&fixture.validate()?, "newly cfg-gated");
    Ok(())
}

#[test]
fn newly_cfg_gated_test_module_is_rejected_even_if_listed() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.base, "src/lib.rs", "mod tests {\n#[test]\nfn stable() {}\n}\n")?;
    write(&fixture.candidate, "src/lib.rs", "#[cfg(target_os = \"macos\")]\nmod tests {\n#[test]\nfn stable() {}\n}\n")?;
    assert_finding(&fixture.validate()?, "test module src/lib.rs newly cfg-gated");
    Ok(())
}

#[test]
fn same_named_test_in_other_binary_does_not_replace_deleted_test() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config.candidate_test_inventory =
        "Running unittests src/lib.rs (target/debug/deps/other-2222222222222222)\nstable: test\n".to_owned();
    assert_finding(&fixture.validate()?, "compiled test deleted");
    Ok(())
}

#[test]
fn new_dependency_is_rejected() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(
        &fixture.candidate,
        "Cargo.lock",
        "version = 4\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n[[package]]\nname = \"surprise\"\nversion = \"1.0.0\"\nsource = \"registry+https://example.invalid\"\n",
    )?;
    assert_finding(&fixture.validate()?, "new dependency");
    Ok(())
}

#[test]
fn changed_dependency_checksum_is_rejected() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(
        &fixture.base,
        "Cargo.lock",
        "version = 4\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\nsource = \"registry+https://example.invalid\"\nchecksum = \"111\"\n",
    )?;
    write(
        &fixture.candidate,
        "Cargo.lock",
        "version = 4\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\nsource = \"registry+https://example.invalid\"\nchecksum = \"222\"\n",
    )?;
    assert_finding(&fixture.validate()?, "new dependency or source");
    Ok(())
}

#[test]
fn permission_policy_source_is_protected() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.candidate, "crates/aimx/src/authz/mod.rs", "// widened\n")?;
    assert_finding(&fixture.validate()?, "protected path changed: crates/aimx/src/authz/mod.rs");
    Ok(())
}

#[test]
fn edited_xtask_cannot_forge_locked_digest() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.candidate, "xtask/src/main.rs", "fn main() { /* always pass */ }\n")?;
    write(
        &fixture.candidate,
        "crates/aim-kernel/src/decision.rs",
        "/// LOCKED(ADR-0005)\npub open spec fn decision(x: nat) -> bool { x >= 0 }\n",
    )?;
    let report = fixture.validate()?;
    assert_finding(&report, "protected path changed: xtask/src/main.rs");
    assert_finding(&report, "LOCKED decision");
    Ok(())
}

#[test]
fn sensitive_pin_and_generated_source_are_in_full_diff() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.candidate, "mise.toml", "[tools]\nrust = \"1.0\"\n")?;
    write(&fixture.candidate, "generated/source.rs", "pub fn generated() {}\n")?;
    let report = fixture.validate()?;
    assert_finding(&report, "sensitive path changed without allowlist: mise.toml");
    assert!(report.changed_paths.iter().any(|path| path == "generated/source.rs"));
    Ok(())
}

#[test]
fn new_build_script_is_rejected() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new()?;
    write(&fixture.candidate, "crate/build.rs", "fn main() {}\n")?;
    assert_finding(&fixture.validate()?, "new build script");
    Ok(())
}
