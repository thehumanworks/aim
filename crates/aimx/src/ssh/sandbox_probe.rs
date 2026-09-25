//! Child process used by the live SSH test to prove local filesystem isolation.

#[test]
#[ignore = "invoked by the SSH isolation parent inside sandbox-exec"]
fn local_std_fs_denied_at_remote_path() {
    let path = std::env::var_os("AIM_SSH_SANDBOX_PROBE").expect("the isolation parent must supply a remote path");
    assert!(std::fs::read(&path).is_err(), "sandboxed local std::fs read reached the remote path");
    assert!(std::fs::write(&path, b"local overwrite").is_err(), "sandboxed local std::fs write reached the remote path");
}
