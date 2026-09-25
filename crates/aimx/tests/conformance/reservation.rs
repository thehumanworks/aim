//! File reservations at the real RPC and local filesystem boundary.

use aim_proto::content::Content;
use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    CallScope, FsCancel, FsCancelParams, FsFinalize, FsFinalizeParams, FsReserve, FsReserveParams, FsWrite, FsWriteParams, Precondition,
};

use crate::common::{env, env_with, key, session};

#[tokio::test(flavor = "multi_thread")]
async fn reserve_refuses_existing_file_and_read_only_grant() {
    let env = env().await;
    std::fs::write(env.path("existing.png"), b"original").unwrap();
    let (client, _, ws) = session(&env).await;
    let existing =
        FsReserveParams { workspace: ws.clone(), path: "existing.png".into(), if_absent: true, idempotency_key: key(), scope: None };
    assert_eq!(client.peer.call::<FsReserve>(existing).await.unwrap_err().code, ErrorCode::PreconditionFailed);
    assert_eq!(std::fs::read(env.path("existing.png")).unwrap(), b"original");

    let read_only = CallScope {
        roots: vec![env.root.canonicalize().unwrap().to_string_lossy().into_owned()],
        ops: vec!["read".into()],
        deny_write: Vec::new(),
        max_processes: None,
        max_output_bytes: None,
    };
    let denied = FsReserveParams { workspace: ws, path: "new.png".into(), if_absent: true, idempotency_key: key(), scope: Some(read_only) };
    assert_eq!(client.peer.call::<FsReserve>(denied).await.unwrap_err().code, ErrorCode::Denied);
    assert!(!env.path("new.png").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn reserve_finalizes_once_and_cancel_preserves_changed_marker() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let reservation = client
        .peer
        .call::<FsReserve>(FsReserveParams {
            workspace: ws.clone(),
            path: "art/image.png".into(),
            if_absent: true,
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap()
        .reservation;
    assert!(env.path("art/image.png").exists());
    let result = client
        .peer
        .call::<FsFinalize>(FsFinalizeParams {
            workspace: ws.clone(),
            reservation: reservation.clone(),
            content: Content::from_bytes(vec![1, 2, 3]),
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap();
    assert_eq!(result.size, 3);
    assert_eq!(std::fs::read(env.path("art/image.png")).unwrap(), [1, 2, 3]);
    assert_eq!(
        client
            .peer
            .call::<FsCancel>(FsCancelParams { workspace: ws.clone(), reservation, idempotency_key: key(), scope: None })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    let reservation = client
        .peer
        .call::<FsReserve>(FsReserveParams {
            workspace: ws.clone(),
            path: "art/changed.png".into(),
            if_absent: true,
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap()
        .reservation;
    client
        .peer
        .call::<FsWrite>(FsWriteParams {
            workspace: ws.clone(),
            path: "art/changed.png".into(),
            content: Content::from_bytes(b"changed".to_vec()),
            precondition: Precondition::Any,
            create_dirs: false,
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap();
    assert_eq!(
        client
            .peer
            .call::<FsCancel>(FsCancelParams { workspace: ws, reservation, idempotency_key: key(), scope: None })
            .await
            .unwrap_err()
            .code,
        ErrorCode::PreconditionFailed
    );
    assert_eq!(std::fs::read(env.path("art/changed.png")).unwrap(), b"changed");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_principal_refuses_reservation() {
    let env = env_with(|config, root| {
        config.principal = aimx::server::local_principal(&[root], true).unwrap();
    })
    .await;
    let (client, _, ws) = session(&env).await;
    assert_eq!(
        client
            .peer
            .call::<FsReserve>(FsReserveParams {
                workspace: ws,
                path: "image.png".into(),
                if_absent: true,
                idempotency_key: key(),
                scope: None,
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );
    assert!(!env.path("image.png").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn abandoned_reservation_is_cleaned_after_resume_window() {
    let env = env_with(|config, _| {
        config.resume_ttl = std::time::Duration::from_millis(200);
    })
    .await;
    let (client, _, ws) = session(&env).await;
    client
        .peer
        .call::<FsReserve>(FsReserveParams {
            workspace: ws,
            path: "abandoned.png".into(),
            if_absent: true,
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap();
    assert!(env.path("abandoned.png").exists());
    client.peer.close();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while env.path("abandoned.png").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
