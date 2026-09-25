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

/// A spawned `aimx serve --stdio` whose home (and so reservation journal) is `home`.
struct Served {
    child: tokio::process::Child,
    client: crate::common::Client,
    ws: aim_proto::ids::WorkspaceId,
}

async fn serve(home: &std::path::Path, root: &std::path::Path) -> Served {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_aimx"))
        .args(["serve", "--stdio", "--root"])
        .arg(root)
        .env("HOME", home)
        .env("AIMX_LOG", "off")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = crate::common::client_over(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    crate::common::initialize(&client, None).await;
    let ws = crate::common::open(&client, root).await;
    Served { child, client, ws }
}

async fn reserve_at(served: &Served, path: &str) -> String {
    served
        .client
        .peer
        .call::<FsReserve>(FsReserveParams {
            workspace: served.ws.clone(),
            path: path.into(),
            if_absent: true,
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap()
        .reservation
}

fn journal_entries(home: &std::path::Path) -> usize {
    std::fs::read_dir(home.join(".aim/aimx/reservations")).map_or(0, |dir| dir.flatten().count())
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    (dir, home, root)
}

/// `REV13a` M5: the marker of an aimx killed before its session ended is removed at the next open
/// of its root, through the durable journal (ADR 0067).
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_servers_marker_is_swept_at_the_next_open() {
    let (_dir, home, root) = fixture();
    let mut first = serve(&home, &root).await;
    reserve_at(&first, "art/killed.png").await;
    assert!(root.join("art/killed.png").exists());
    assert_eq!(journal_entries(&home), 1);
    let journal = home.join(".aim/aimx/reservations");
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&journal).unwrap().permissions()) & 0o777, 0o700);
    first.child.kill().await.unwrap();
    assert!(root.join("art/killed.png").exists(), "a killed server cannot clean up");
    let second = serve(&home, &root).await;
    assert!(!root.join("art/killed.png").exists(), "the next open sweeps the abandoned marker");
    assert_eq!(journal_entries(&home), 0);
    second.client.peer.close();
}

/// A live reservation of a concurrent aimx is never swept; it finalizes normally.
#[tokio::test(flavor = "multi_thread")]
async fn a_concurrent_servers_live_reservation_is_not_swept() {
    let (_dir, home, root) = fixture();
    let first = serve(&home, &root).await;
    let reservation = reserve_at(&first, "art/live.png").await;
    let second = serve(&home, &root).await;
    assert!(root.join("art/live.png").exists(), "a live reservation keeps its marker");
    first
        .client
        .peer
        .call::<FsFinalize>(FsFinalizeParams {
            workspace: first.ws.clone(),
            reservation,
            content: Content::from_bytes(b"image".to_vec()),
            idempotency_key: key(),
            scope: None,
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(root.join("art/live.png")).unwrap(), b"image");
    assert_eq!(journal_entries(&home), 0);
    second.client.peer.close();
    first.client.peer.close();
}

/// `REV13a` M5: closing the connection lets `aimx serve --stdio` end its session, which cancels
/// its unused markers before the process exits.
#[tokio::test(flavor = "multi_thread")]
async fn stdio_eof_cancels_unused_markers_before_exit() {
    let (_dir, home, root) = fixture();
    let mut served = serve(&home, &root).await;
    reserve_at(&served, "art/unused.png").await;
    served.client.peer.close();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), served.child.wait()).await.unwrap().unwrap();
    assert!(status.success());
    assert!(!root.join("art/unused.png").exists());
    assert_eq!(journal_entries(&home), 0);
}
