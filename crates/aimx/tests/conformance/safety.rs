//! Enforcement (docs/adr/0008, 0021): confinement, symlink escapes, protected paths and read-only
//! principals — whoever the caller is.

use std::os::unix::fs::symlink;

use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    Command, ExecSpawn, ExecSpawnParams, FsCopy, FsCopyParams, FsList, FsListParams, FsMkdir, FsMkdirParams, FsRead, FsReadParams,
    FsRemove, FsRemoveParams, FsRename, FsRenameParams, FsStat, FsStatParams, FsWrite, FsWriteParams, Grep, GrepParams, Precondition,
    ToolsCall, ToolsCallParams,
};
use aim_proto::ids::WorkspaceId;
use aimx::authz::ProtectedPaths;
use serde_json::json;

use crate::common::{Client, env, env_with, key, session, text};

async fn write(client: &Client, ws: &WorkspaceId, path: &str) -> ErrorCode {
    let params = FsWriteParams {
        workspace: ws.clone(),
        path: path.into(),
        content: text("x"),
        precondition: Precondition::Any,
        create_dirs: true,
        idempotency_key: key(),
    };
    match client.peer.call::<FsWrite>(params).await {
        Ok(_) => panic!("write to {path} succeeded"),
        Err(err) => err.code,
    }
}

async fn read(client: &Client, ws: &WorkspaceId, path: &str) -> Result<(), ErrorCode> {
    client.peer.call::<FsRead>(FsReadParams { workspace: ws.clone(), path: path.into(), range: None }).await.map(|_| ()).map_err(|e| e.code)
}

#[tokio::test(flavor = "multi_thread")]
async fn traversal_and_absolute_escapes_are_denied() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    std::fs::write(env.dir.path().join("secret"), "s").unwrap();
    let outside = env.dir.path().join("secret").to_str().unwrap().to_owned();

    for path in ["../secret", "a/../../secret", "..", "/etc/passwd", outside.as_str(), "./../ws/../secret"] {
        assert_eq!(read(&client, &ws, path).await, Err(ErrorCode::Denied), "read {path}");
        assert_eq!(write(&client, &ws, path).await, ErrorCode::Denied, "write {path}");
    }
    let stat = client.peer.call::<FsStat>(FsStatParams { workspace: ws.clone(), path: "../secret".into(), hash: false }).await.unwrap_err();
    assert_eq!(stat.code, ErrorCode::Denied);
    let grep = GrepParams {
        workspace: ws.clone(),
        pattern: "s".into(),
        path: Some("..".into()),
        globs: vec![],
        case: aim_proto::harness::CaseMode::default(),
        fixed_strings: false,
        context: 0,
        max_matches: None,
    };
    assert_eq!(client.peer.call::<Grep>(grep).await.unwrap_err().code, ErrorCode::Denied);
    let spawn = ExecSpawnParams {
        workspace: ws.clone(),
        command: Command::Shell { script: "true".into() },
        cwd: Some("..".into()),
        env: std::collections::BTreeMap::default(),
        pty: None,
        stdin: false,
        timeout_ms: None,
        idempotency_key: key(),
    };
    assert_eq!(client.peer.call::<ExecSpawn>(spawn).await.unwrap_err().code, ErrorCode::Denied);
    // `..` that stays inside is fine.
    std::fs::write(env.path("inside"), "i").unwrap();
    assert_eq!(read(&client, &ws, "sub/../inside").await, Ok(()));
    assert_eq!(std::fs::read_to_string(env.dir.path().join("secret")).unwrap(), "s");
}

#[tokio::test(flavor = "multi_thread")]
async fn symlinks_out_of_the_root_are_denied() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let outside = env.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("target"), "secret").unwrap();
    symlink(outside.join("target"), env.path("file-link")).unwrap();
    symlink(&outside, env.path("dir-link")).unwrap();
    symlink(env.dir.path().join("does-not-exist"), env.path("dangling")).unwrap();
    std::fs::write(env.path("real"), "inside").unwrap();
    symlink(env.path("real"), env.path("inner-link")).unwrap();

    assert_eq!(read(&client, &ws, "file-link").await, Err(ErrorCode::Denied));
    assert_eq!(read(&client, &ws, "dir-link/target").await, Err(ErrorCode::Denied));
    assert_eq!(write(&client, &ws, "file-link").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "dir-link/new").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "dir-link/deep/new").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "dangling").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "dangling/x").await, ErrorCode::Denied);
    let list = FsListParams { workspace: ws.clone(), path: "dir-link".into(), limit: None, page_token: None, include_hidden: false };
    assert_eq!(client.peer.call::<FsList>(list).await.unwrap_err().code, ErrorCode::Denied);
    let mkdir = FsMkdirParams { workspace: ws.clone(), path: "dir-link/sub".into(), idempotency_key: key() };
    assert_eq!(client.peer.call::<FsMkdir>(mkdir).await.unwrap_err().code, ErrorCode::Denied);
    let rename = FsRenameParams {
        workspace: ws.clone(),
        from: "real".into(),
        to: "dir-link/stolen".into(),
        overwrite: false,
        idempotency_key: key(),
    };
    assert_eq!(client.peer.call::<FsRename>(rename).await.unwrap_err().code, ErrorCode::Denied);

    // The link itself is inside the root: stat sees it (without following), remove deletes the
    // link and never the target.
    let meta = client.peer.call::<FsStat>(FsStatParams { workspace: ws.clone(), path: "file-link".into(), hash: false }).await.unwrap();
    assert_eq!(meta.kind, aim_proto::harness::EntryKind::Symlink);
    let remove = FsRemoveParams { workspace: ws.clone(), path: "dir-link".into(), recursive: true, idempotency_key: key() };
    client.peer.call::<FsRemove>(remove).await.unwrap();
    assert!(outside.join("target").exists());

    // Links that stay inside the root are followed.
    assert_eq!(read(&client, &ws, "inner-link").await, Ok(()));
    assert_eq!(std::fs::read_to_string(outside.join("target")).unwrap(), "secret");
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_paths_are_never_written() {
    let env = env_with(|config, root| {
        config.protected = ProtectedPaths::new([root.join("gate").to_str().unwrap().to_owned()]);
    })
    .await;
    std::fs::create_dir(env.path("gate")).unwrap();
    std::fs::write(env.path("gate/policy"), "p").unwrap();
    symlink(env.path("gate"), env.path("gate-link")).unwrap();
    let (client, _, ws) = session(&env).await;

    assert_eq!(read(&client, &ws, "gate/policy").await, Ok(()), "protected paths stay readable");
    assert_eq!(write(&client, &ws, "gate/policy").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "gate/new").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "gate-link/policy").await, ErrorCode::Denied, "a symlinked spelling is still protected");
    let remove = FsRemoveParams { workspace: ws.clone(), path: "gate".into(), recursive: true, idempotency_key: key() };
    assert_eq!(client.peer.call::<FsRemove>(remove).await.unwrap_err().code, ErrorCode::Denied);
    let rename =
        FsRenameParams { workspace: ws.clone(), from: "gate".into(), to: "moved".into(), overwrite: false, idempotency_key: key() };
    assert_eq!(client.peer.call::<FsRename>(rename).await.unwrap_err().code, ErrorCode::Denied);
    let call = ToolsCallParams {
        workspace: ws.clone(),
        name: "Write".into(),
        arguments: json!({"file_path": "gate/policy", "content": "pwned"}),
        idempotency_key: Some(key()),
    };
    assert_eq!(client.peer.call::<ToolsCall>(call).await.unwrap_err().code, ErrorCode::Denied);
    assert_eq!(std::fs::read_to_string(env.path("gate/policy")).unwrap(), "p");
}

#[tokio::test(flavor = "multi_thread")]
async fn dangling_protected_links_guard_their_future_targets() {
    let env = env_with(|config, root| {
        config.protected =
            ProtectedPaths::new([root.join("gate").to_str().unwrap().to_owned(), root.join("deep-gate").to_str().unwrap().to_owned()]);
    })
    .await;
    symlink("future-gate", env.path("gate")).unwrap();
    symlink("missing/sub/future-gate", env.path("deep-gate")).unwrap();
    std::fs::write(env.path("source"), "source").unwrap();
    let (client, _, ws) = session(&env).await;

    for target in ["future-gate", "missing/sub/future-gate"] {
        assert_eq!(write(&client, &ws, target).await, ErrorCode::Denied, "write {target}");
        let mkdir = FsMkdirParams { workspace: ws.clone(), path: target.into(), idempotency_key: key() };
        assert_eq!(client.peer.call::<FsMkdir>(mkdir).await.unwrap_err().code, ErrorCode::Denied, "mkdir {target}");
        let copy = FsCopyParams {
            workspace: ws.clone(),
            from: "source".into(),
            to: target.into(),
            overwrite: false,
            recursive: false,
            idempotency_key: key(),
        };
        assert_eq!(client.peer.call::<FsCopy>(copy).await.unwrap_err().code, ErrorCode::Denied, "copy {target}");
        let rename =
            FsRenameParams { workspace: ws.clone(), from: "source".into(), to: target.into(), overwrite: false, idempotency_key: key() };
        assert_eq!(client.peer.call::<FsRename>(rename).await.unwrap_err().code, ErrorCode::Denied, "rename {target}");
        assert!(!env.path(target).exists(), "protected target remains absent: {target}");
    }
    assert_eq!(std::fs::read_to_string(env.path("source")).unwrap(), "source");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_principals_cannot_mutate() {
    let env = env_with(|config, _| config.principal.read_only = true).await;
    std::fs::write(env.path("f"), "data").unwrap();
    let (client, init, ws) = session(&env).await;
    assert!(init.principal.read_only);

    assert_eq!(read(&client, &ws, "f").await, Ok(()));
    assert_eq!(write(&client, &ws, "f").await, ErrorCode::Denied);
    assert_eq!(write(&client, &ws, "new").await, ErrorCode::Denied);
    let mkdir = FsMkdirParams { workspace: ws.clone(), path: "d".into(), idempotency_key: key() };
    assert_eq!(client.peer.call::<FsMkdir>(mkdir).await.unwrap_err().code, ErrorCode::Denied);
    let spawn = ExecSpawnParams {
        workspace: ws.clone(),
        command: Command::Shell { script: "touch pwned".into() },
        cwd: None,
        env: std::collections::BTreeMap::default(),
        pty: None,
        stdin: false,
        timeout_ms: None,
        idempotency_key: key(),
    };
    assert_eq!(client.peer.call::<ExecSpawn>(spawn).await.unwrap_err().code, ErrorCode::Denied);
    for (name, arguments) in [
        ("Bash", json!({"command": "touch pwned"})),
        ("Write", json!({"file_path": "f", "content": "x"})),
        ("Edit", json!({"file_path": "f", "old_string": "data", "new_string": "x"})),
    ] {
        let call = ToolsCallParams { workspace: ws.clone(), name: name.into(), arguments, idempotency_key: Some(key()) };
        assert_eq!(client.peer.call::<ToolsCall>(call).await.unwrap_err().code, ErrorCode::Denied, "{name}");
    }
    // Read-only tools still work.
    let call = ToolsCallParams { workspace: ws, name: "Read".into(), arguments: json!({"file_path": "f"}), idempotency_key: None };
    let result = client.peer.call::<ToolsCall>(call).await.unwrap();
    assert!(!result.is_error);
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "data");
    assert!(!env.path("pwned").exists());
}
