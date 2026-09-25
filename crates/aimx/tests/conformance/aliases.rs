//! Protected paths under aliases (REV4-A findings 1 and 2). On a case-insensitive or
//! normalization-insensitive volume (this macOS host's APFS), a spelling that differs only in case
//! or in Unicode normalization names the same file. The backend compares filesystem identity, so
//! no such spelling of a protected path, of one of its ancestors, or of a protected path that does
//! not exist yet can be modified by any mutating request; the policy file `~/.aim/protected` is
//! itself protected. On a case-sensitive volume the other spellings are other files, and the
//! protected bytes must still be untouched.

use std::path::Path;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    ExactEdit, FsCopy, FsCopyParams, FsEdit, FsEditParams, FsMkdir, FsMkdirParams, FsRemove, FsRemoveParams, FsRename, FsRenameParams,
    FsWrite, FsWriteParams, Precondition, ToolsCall, ToolsCallParams,
};
use aim_proto::ids::WorkspaceId;
use aimx::authz::ProtectedPaths;
use aimx::server::default_protected;
use serde_json::json;

use crate::common::{Client, Env, env_with, key, session, text};

/// Whether names in `dir` compare ignoring case (probed on the volume itself).
fn case_insensitive(dir: &Path) -> bool {
    std::fs::write(dir.join(".probe-case"), "").unwrap();
    let insensitive = dir.join(".PROBE-CASE").exists();
    std::fs::remove_file(dir.join(".probe-case")).unwrap();
    insensitive
}

/// Whether names in `dir` compare ignoring Unicode normalization (APFS, HFS+).
fn normalization_insensitive(dir: &Path) -> bool {
    std::fs::write(dir.join(".probe-caf\u{e9}"), "").unwrap();
    let insensitive = dir.join(".probe-cafe\u{301}").exists();
    std::fs::remove_file(dir.join(".probe-caf\u{e9}")).unwrap();
    insensitive
}

/// An outcome that must be `denied` when the alias names the protected file, and may be anything
/// else when it names another file.
fn refused(outcome: Result<(), ProtoError>, aliased: bool, what: &str) {
    if aliased {
        assert_eq!(outcome.map_err(|err| err.code), Err(ErrorCode::Denied), "{what}");
    }
}

async fn write(client: &Client, ws: &WorkspaceId, path: &str, create_dirs: bool) -> Result<(), ProtoError> {
    let params = FsWriteParams {
        workspace: ws.clone(),
        path: path.into(),
        content: text("PWNED"),
        precondition: Precondition::Any,
        create_dirs,
        idempotency_key: key(),
    };
    client.peer.call::<FsWrite>(params).await.map(|_| ())
}

async fn rename(client: &Client, ws: &WorkspaceId, from: &str, to: &str) -> Result<(), ProtoError> {
    let params = FsRenameParams { workspace: ws.clone(), from: from.into(), to: to.into(), overwrite: true, idempotency_key: key() };
    client.peer.call::<FsRename>(params).await
}

async fn remove(client: &Client, ws: &WorkspaceId, path: &str) -> Result<(), ProtoError> {
    client.peer.call::<FsRemove>(FsRemoveParams { workspace: ws.clone(), path: path.into(), recursive: true, idempotency_key: key() }).await
}

async fn copy(client: &Client, ws: &WorkspaceId, from: &str, to: &str) -> Result<(), ProtoError> {
    let params =
        FsCopyParams { workspace: ws.clone(), from: from.into(), to: to.into(), overwrite: true, recursive: true, idempotency_key: key() };
    client.peer.call::<FsCopy>(params).await
}

async fn mkdir(client: &Client, ws: &WorkspaceId, path: &str) -> Result<(), ProtoError> {
    client.peer.call::<FsMkdir>(FsMkdirParams { workspace: ws.clone(), path: path.into(), idempotency_key: key() }).await
}

async fn tool(client: &Client, ws: &WorkspaceId, name: &str, arguments: serde_json::Value) -> Result<(), ProtoError> {
    let call = ToolsCallParams { workspace: ws.clone(), name: name.into(), arguments, idempotency_key: Some(key()) };
    client.peer.call::<ToolsCall>(call).await.map(|_| ())
}

/// A workspace that is also the protected set's home: `.aim/gate/policy` exists, `.aim/ledger`
/// and `locked-dir/absent` do not, `café` (NFC) does not, and `.aim/protected` lists `~/secret`.
async fn home() -> Env {
    env_with(|config, root| {
        std::fs::create_dir_all(root.join(".aim/gate")).unwrap();
        std::fs::write(root.join(".aim/gate/policy"), "GATE").unwrap();
        std::fs::write(root.join(".aim/protected"), "~/secret\n~/locked-dir/absent\n~/caf\u{e9}\n").unwrap();
        std::fs::write(root.join("secret"), "SECRET").unwrap();
        std::fs::write(root.join("other.txt"), "other").unwrap();
        std::fs::create_dir(root.join("otherdir")).unwrap();
        std::fs::write(root.join("otherdir/absent"), "planted").unwrap();
        std::fs::write(root.join("otherdir/policy"), "planted").unwrap();
        config.protected = default_protected(root.to_str().unwrap());
    })
    .await
}

fn untouched(env: &Env) {
    assert_eq!(std::fs::read_to_string(env.path(".aim/gate/policy")).unwrap(), "GATE", "the gate is untouched");
    assert_eq!(
        std::fs::read_to_string(env.path(".aim/protected")).unwrap(),
        "~/secret\n~/locked-dir/absent\n~/caf\u{e9}\n",
        "the policy file is untouched"
    );
    assert_eq!(std::fs::read_to_string(env.path("secret")).unwrap(), "SECRET");
    let names = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir).map(|d| d.map(|e| e.unwrap().file_name().into_string().unwrap()).collect()).unwrap_or_default()
    };
    assert_eq!(names(&env.path(".aim/gate")), ["policy"], "nothing was added to the gate");
    assert!(!names(&env.path(".aim")).iter().any(|n| n == "ledger"), "the ledger was not created");
}

#[tokio::test(flavor = "multi_thread")]
async fn case_aliases_of_an_existing_protected_path_are_denied() {
    let env = home().await;
    let ci = case_insensitive(&env.root);
    let (client, _, ws) = session(&env).await;

    for spelling in [".aim/GATE/policy", ".AIM/gate/policy", ".Aim/Gate/Policy"] {
        refused(write(&client, &ws, spelling, false).await, ci, &format!("fs.write {spelling}"));
    }
    let edit = FsEditParams {
        workspace: ws.clone(),
        path: ".AIM/GATE/POLICY".into(),
        edits: vec![ExactEdit { old: "GATE".into(), new: "PWNED".into(), replace_all: false }],
        precondition: Precondition::Any,
        idempotency_key: key(),
    };
    refused(client.peer.call::<FsEdit>(edit).await.map(|_| ()), ci, "fs.edit");
    refused(mkdir(&client, &ws, ".AIM/GATE/new").await, ci, "fs.mkdir inside");
    refused(copy(&client, &ws, "other.txt", ".AIM/gate/policy").await, ci, "fs.copy onto");
    refused(copy(&client, &ws, "otherdir", ".aim/GATE/sub").await, ci, "fs.copy into");
    refused(rename(&client, &ws, ".AIM/GATE/policy", "stolen").await, ci, "fs.rename from");
    refused(rename(&client, &ws, "other.txt", ".aim/Gate/policy").await, ci, "fs.rename onto");
    refused(remove(&client, &ws, ".AIM/GATE/policy").await, ci, "fs.remove");
    // Ancestors: moving or removing a directory that holds a protected path.
    refused(rename(&client, &ws, ".AIM", "moved").await, ci, "fs.rename of an ancestor");
    refused(rename(&client, &ws, ".aim/GATE", "moved").await, ci, "fs.rename of the protected directory");
    refused(remove(&client, &ws, ".AIM").await, ci, "fs.remove of an ancestor");
    refused(tool(&client, &ws, "Write", json!({"file_path": ".AIM/GATE/policy", "content": "PWNED"})).await, ci, "Write tool");
    let edit = json!({"file_path": ".aim/gate/POLICY", "old_string": "GATE", "new_string": "PWNED"});
    refused(tool(&client, &ws, "Edit", edit).await, ci, "Edit tool");
    untouched(&env);
}

#[tokio::test(flavor = "multi_thread")]
async fn aliases_of_protected_paths_that_do_not_exist_yet_are_denied() {
    let env = home().await;
    let ci = case_insensitive(&env.root);
    let ni = normalization_insensitive(&env.root);
    let (client, _, ws) = session(&env).await;

    refused(write(&client, &ws, ".AIM/LEDGER", false).await, ci, "fs.write creating the ledger");
    refused(write(&client, &ws, ".aim/Ledger/entry", true).await, ci, "fs.write creating the ledger's parent");
    refused(mkdir(&client, &ws, ".aim/LEDGER").await, ci, "fs.mkdir of the ledger");
    refused(rename(&client, &ws, "otherdir", ".AIM/LEDGER").await, ci, "fs.rename onto the ledger");
    refused(copy(&client, &ws, "otherdir", ".Aim/Ledger").await, ci, "fs.copy onto the ledger");
    // A protected path under a directory that does not exist either.
    refused(write(&client, &ws, "LOCKED-DIR/ABSENT", true).await, ci, "fs.write under an absent parent");
    refused(rename(&client, &ws, "otherdir", "Locked-Dir").await, ci, "fs.rename creating the parent");
    refused(copy(&client, &ws, "otherdir", "locked-DIR").await, ci, "fs.copy creating the parent");
    // The exact spellings are denied on every volume.
    refused(write(&client, &ws, ".aim/ledger", true).await, true, "fs.write of the ledger");
    refused(rename(&client, &ws, "otherdir", "locked-dir").await, true, "fs.rename onto an ancestor");
    // A spelling in the other Unicode normalization (NFD for the NFC `café`).
    refused(write(&client, &ws, "cafe\u{301}", false).await, ni, "fs.write in NFD");
    refused(write(&client, &ws, "caf\u{e9}", false).await, true, "fs.write in NFC");
    untouched(&env);
    assert!(!env.path("locked-dir").exists() || !ci, "the protected path's parent was not created");
    assert!(!env.path("caf\u{e9}").exists() || !ni);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_protected_path_policy_file_is_protected() {
    let env = home().await;
    let ci = case_insensitive(&env.root);
    let (client, _, ws) = session(&env).await;
    // The listed path is protected.
    refused(write(&client, &ws, "secret", false).await, true, "a listed path");
    // The policy file cannot be replaced, cleared, removed or moved, under any spelling.
    refused(write(&client, &ws, ".aim/protected", false).await, true, "fs.write of the policy");
    refused(write(&client, &ws, ".AIM/PROTECTED", false).await, ci, "fs.write of an alias of the policy");
    let edit = FsEditParams {
        workspace: ws.clone(),
        path: ".aim/protected".into(),
        edits: vec![ExactEdit { old: "~/secret\n".into(), new: String::new(), replace_all: false }],
        precondition: Precondition::Any,
        idempotency_key: key(),
    };
    refused(client.peer.call::<FsEdit>(edit).await.map(|_| ()), true, "fs.edit of the policy");
    refused(remove(&client, &ws, ".aim/protected").await, true, "fs.remove of the policy");
    refused(rename(&client, &ws, ".aim/protected", "moved").await, true, "fs.rename of the policy");
    refused(rename(&client, &ws, "other.txt", ".aim/protected").await, true, "fs.rename onto the policy");
    refused(copy(&client, &ws, "other.txt", ".Aim/Protected").await, ci, "fs.copy onto an alias of the policy");
    refused(rename(&client, &ws, ".aim", "moved").await, true, "fs.rename of the policy's directory");
    untouched(&env);

    // A server started later still reads the original policy.
    let protected = default_protected(env.root.to_str().unwrap());
    let listed = std::fs::canonicalize(&env.root).unwrap().join("secret");
    assert!(protected.paths().iter().any(|p| Path::new(p) == env.root.join("secret") || Path::new(p) == listed), "{protected:?}");
    assert!(protected.guards(env.root.join(".aim/protected").to_str().unwrap()), "the policy file is in the default set");
    drop(ProtectedPaths::default());
}
