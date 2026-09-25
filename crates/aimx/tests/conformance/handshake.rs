//! `initialize`, `workspace.open`, and the principal's workspace grants.

use aim_proto::error::ErrorCode;
use aim_proto::harness::{BackendSpec, BootstrapPolicy, FsStat, FsStatParams, Initialize, WorkspaceOpen, WorkspaceOpenParams};
use aim_proto::ids::WorkspaceId;

use crate::common::{connect, env, init_params, initialize, open};

#[tokio::test(flavor = "multi_thread")]
async fn initialize_negotiates_and_reports_limits() {
    let env = env().await;
    let client = connect(&env.socket).await;
    let init = client.peer.call::<Initialize>(init_params(0, 7, None)).await.unwrap();
    assert_eq!(init.generation, 1);
    assert_eq!(init.server.name, "aimx");
    assert!(!init.resumed);
    assert_eq!(init.resume_token.as_str().len(), 64);
    assert!(init.principal.id.starts_with("local:"));
    assert!(!init.principal.read_only);
    assert_eq!(init.limits.resume_ttl_secs, 1800);
    assert_eq!(init.limits.output_ring_bytes, 8 * 1024 * 1024);
    // Even fully escaped (`\u0000` is six bytes per input byte) a read fits in one message.
    assert!(init.limits.max_read_bytes * 6 < init.limits.max_message_bytes);

    let again = client.peer.call::<Initialize>(init_params(1, 1, None)).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::InvalidRequest);
}

#[tokio::test(flavor = "multi_thread")]
async fn disjoint_generations_are_refused() {
    let env = env().await;
    let client = connect(&env.socket).await;
    let err = client.peer.call::<Initialize>(init_params(2, 5, None)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsupportedGeneration);
    assert_eq!(err.detail.unwrap()["server"]["max"], 1);
    let err = client.peer.call::<Initialize>(init_params(3, 2, None)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    // A refused handshake leaves the connection usable for a correct one.
    initialize(&client, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_before_initialize_are_unauthenticated() {
    let env = env().await;
    let client = connect(&env.socket).await;
    let params = WorkspaceOpenParams { root: env.root.to_str().unwrap().into(), backend: BackendSpec::Local };
    let err = client.peer.call::<WorkspaceOpen>(params).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_open_confines_to_granted_roots() {
    let env = env().await;
    let client = connect(&env.socket).await;
    initialize(&client, None).await;

    let root = env.root.to_str().unwrap().to_owned();
    let info = client.peer.call::<WorkspaceOpen>(WorkspaceOpenParams { root: root.clone(), backend: BackendSpec::Local }).await.unwrap();
    let canonical = std::fs::canonicalize(&env.root).unwrap();
    assert_eq!(info.root, canonical.to_str().unwrap());
    assert!(info.caps.exec && info.caps.pty && info.caps.native_search && info.caps.resumable);
    // Reopening the same root reuses the workspace.
    assert_eq!(open(&client, &env.root).await, info.id);

    // A subdirectory of a granted root is fine; its parent is not.
    std::fs::create_dir(env.path("sub")).unwrap();
    open(&client, &env.path("sub")).await;
    let err = client
        .peer
        .call::<WorkspaceOpen>(WorkspaceOpenParams { root: env.dir.path().to_str().unwrap().into(), backend: BackendSpec::Local })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Denied);

    let err = client
        .peer
        .call::<WorkspaceOpen>(WorkspaceOpenParams { root: format!("{root}/missing"), backend: BackendSpec::Local })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    let ssh = BackendSpec::Ssh { destination: "host".into(), bootstrap: BootstrapPolicy::Auto };
    let err = client.peer.call::<WorkspaceOpen>(WorkspaceOpenParams { root, backend: ssh }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
}

#[tokio::test(flavor = "multi_thread")]
async fn workspaces_are_scoped_to_their_session() {
    let env = env().await;
    let first = connect(&env.socket).await;
    initialize(&first, None).await;
    let ws = open(&first, &env.root).await;

    let second = connect(&env.socket).await;
    initialize(&second, None).await;
    let err = second.peer.call::<FsStat>(FsStatParams { workspace: ws, path: ".".into(), hash: false }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    let err = second
        .peer
        .call::<FsStat>(FsStatParams { workspace: WorkspaceId::new("w-made-up"), path: ".".into(), hash: false })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}
