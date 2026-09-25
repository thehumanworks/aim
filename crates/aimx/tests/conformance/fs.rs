//! `fs.*`: reads, atomic writes, preconditions, exact edits, listing and tree operations.

use std::os::unix::fs::PermissionsExt as _;

use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    ByteRange, ContentHash, EntryKind, ExactEdit, FsEdit, FsEditParams, FsList, FsListParams, FsMkdir, FsMkdirParams, FsRead, FsReadParams,
    FsRemove, FsRemoveParams, FsRename, FsRenameParams, FsStat, FsStatParams, FsWrite, FsWriteParams, Precondition,
};
use aim_proto::ids::WorkspaceId;

use crate::common::{Client, content_string, env, key, session, text};

async fn write(
    client: &Client,
    ws: &WorkspaceId,
    path: &str,
    body: &str,
    precondition: Precondition,
) -> Result<aim_proto::harness::WriteOutcome, aim_proto::error::ProtoError> {
    let params = FsWriteParams {
        workspace: ws.clone(),
        path: path.into(),
        content: text(body),
        precondition,
        create_dirs: true,
        idempotency_key: key(),
    };
    client.peer.call::<FsWrite>(params).await
}

async fn read(client: &Client, ws: &WorkspaceId, path: &str) -> aim_proto::harness::FsReadResult {
    client
        .peer
        .call::<FsRead>(FsReadParams { workspace: ws.clone(), path: path.into(), range: None, scope: None, hash: true })
        .await
        .unwrap()
}

fn edit(old: &str, new: &str) -> ExactEdit {
    ExactEdit { old: old.into(), new: new.into(), replace_all: false }
}

#[tokio::test(flavor = "multi_thread")]
async fn write_read_stat_roundtrip() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;

    let created = write(&client, &ws, "dir/a.txt", "hello\n", Precondition::Any).await.unwrap();
    assert!(created.created);
    assert_eq!(created.size, 6);
    assert_eq!(created.hash.0, "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03");
    assert_eq!(std::fs::read_to_string(env.path("dir/a.txt")).unwrap(), "hello\n");

    let got = read(&client, &ws, "dir/a.txt").await;
    assert_eq!(content_string(got.content), "hello\n");
    assert_eq!(got.hash, Some(created.hash.clone()));
    assert_eq!(got.size, 6);
    assert!(!got.truncated);

    // Absolute paths under the root (as the client spells it) work too.
    let absolute = env.path("dir/a.txt").to_str().unwrap().to_owned();
    assert_eq!(read(&client, &ws, &absolute).await.hash, Some(created.hash.clone()));

    let range = client
        .peer
        .call::<FsRead>(FsReadParams {
            workspace: ws.clone(),
            path: "dir/a.txt".into(),
            range: Some(ByteRange { start: 1, len: 3 }),
            scope: None,
            hash: true,
        })
        .await
        .unwrap();
    assert_eq!(content_string(range.content), "ell");
    assert_eq!(range.hash, Some(created.hash.clone()), "the hash always covers the whole file");

    let meta = client.peer.call::<FsStat>(FsStatParams { workspace: ws.clone(), path: "dir/a.txt".into(), hash: true }).await.unwrap();
    assert_eq!(meta.kind, EntryKind::File);
    assert_eq!(meta.size, 6);
    assert_eq!(meta.hash, Some(created.hash.clone()));
    assert!(meta.mtime_ms.is_some());
    let dir = client.peer.call::<FsStat>(FsStatParams { workspace: ws.clone(), path: "dir".into(), hash: true }).await.unwrap();
    assert_eq!(dir.kind, EntryKind::Dir);
    assert_eq!(dir.hash, None);

    let replaced = write(&client, &ws, "dir/a.txt", "bye", Precondition::Any).await.unwrap();
    assert!(!replaced.created);

    let missing = client
        .peer
        .call::<FsRead>(FsReadParams { workspace: ws.clone(), path: "nope".into(), range: None, scope: None, hash: true })
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    let is_dir = client
        .peer
        .call::<FsRead>(FsReadParams { workspace: ws, path: "dir".into(), range: None, scope: None, hash: true })
        .await
        .unwrap_err();
    assert_eq!(is_dir.code, ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_content_roundtrips_as_base64() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let bytes = vec![0u8, 159, 146, 150, 255];
    let params = FsWriteParams {
        workspace: ws.clone(),
        path: "bin".into(),
        content: aim_proto::content::Content::from_bytes(bytes.clone()),
        precondition: Precondition::Any,
        create_dirs: false,
        idempotency_key: key(),
    };
    client.peer.call::<FsWrite>(params).await.unwrap();
    let got = read(&client, &ws, "bin").await;
    assert!(matches!(got.content, aim_proto::content::Content::Base64 { .. }));
    assert_eq!(got.content.into_bytes(), bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn preconditions_guard_writes() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let first = write(&client, &ws, "f", "one", Precondition::IfAbsent).await.unwrap();
    let err = write(&client, &ws, "f", "two", Precondition::IfAbsent).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PreconditionFailed);

    let stale = ContentHash("sha256:0000".into());
    let err = write(&client, &ws, "f", "three", Precondition::IfHash { hash: stale }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PreconditionFailed);
    assert_eq!(err.detail.unwrap()["current"], first.hash.0.as_str());
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "one");

    write(&client, &ws, "f", "four", Precondition::IfHash { hash: first.hash }).await.unwrap();
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "four");

    let err = write(&client, &ws, "g", "x", Precondition::IfHash { hash: ContentHash("sha256:00".into()) }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PreconditionFailed);
    assert!(!env.path("g").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_are_atomic_and_keep_the_mode() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    std::fs::write(env.path("script.sh"), "#!/bin/sh\necho old\n").unwrap();
    std::fs::set_permissions(env.path("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = std::fs::metadata(env.path("script.sh")).unwrap();
    write(&client, &ws, "script.sh", "#!/bin/sh\necho new\n", Precondition::Any).await.unwrap();
    let after = std::fs::metadata(env.path("script.sh")).unwrap();
    assert_eq!(after.permissions().mode() & 0o777, 0o755);
    // Replaced by rename: a new inode, never a file truncated in place.
    assert_ne!(std::os::unix::fs::MetadataExt::ino(&before), std::os::unix::fs::MetadataExt::ino(&after));

    // Failed writes leave nothing behind; successful ones leave no temporary files.
    write(&client, &ws, "script.sh", "x", Precondition::IfAbsent).await.unwrap_err();
    let names: Vec<String> = std::fs::read_dir(&env.root).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, vec!["script.sh".to_owned()]);

    // Without create_dirs a missing parent is not_found, and nothing is created.
    let params = FsWriteParams {
        workspace: ws,
        path: "no/such/dir/f".into(),
        content: text("x"),
        precondition: Precondition::Any,
        create_dirs: false,
        idempotency_key: key(),
    };
    assert_eq!(client.peer.call::<FsWrite>(params).await.unwrap_err().code, ErrorCode::NotFound);
    assert!(!env.path("no").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_edits_are_all_or_nothing() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    std::fs::write(env.path("e.txt"), "alpha beta beta gamma\n").unwrap();
    let call = |edits: Vec<ExactEdit>, precondition: Precondition| {
        let params = FsEditParams { workspace: ws.clone(), path: "e.txt".into(), edits, precondition, idempotency_key: key() };
        let peer = client.peer.clone();
        async move { peer.call::<FsEdit>(params).await }
    };

    // Zero occurrences.
    let err = call(vec![edit("delta", "x")], Precondition::Any).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.detail.as_ref().unwrap()["occurrences"], 0);
    assert_eq!(err.detail.as_ref().unwrap()["edit_index"], 0);
    // Two occurrences.
    let err = call(vec![edit("beta", "x")], Precondition::Any).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.detail.unwrap()["occurrences"], 2);
    // A later failing edit undoes the earlier ones.
    let err = call(vec![edit("alpha", "A"), edit("zzz", "x")], Precondition::Any).await.unwrap_err();
    assert_eq!(err.detail.unwrap()["edit_index"], 1);
    // Empty `old` is invalid.
    let err = call(vec![edit("", "x")], Precondition::Any).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert_eq!(std::fs::read_to_string(env.path("e.txt")).unwrap(), "alpha beta beta gamma\n", "failed edits leave the file untouched");

    let done = call(
        vec![edit("alpha", "A"), ExactEdit { old: "beta".into(), new: "B".into(), replace_all: true }, edit("A B", "AB")],
        Precondition::Any,
    )
    .await
    .unwrap();
    assert_eq!(done.replacements, vec![1, 2, 1]);
    assert_eq!(std::fs::read_to_string(env.path("e.txt")).unwrap(), "AB B gamma\n");

    let err = call(vec![edit("gamma", "G")], Precondition::IfHash { hash: ContentHash("sha256:00".into()) }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PreconditionFailed);
    let err = client
        .peer
        .call::<FsEdit>(FsEditParams {
            workspace: ws,
            path: "missing".into(),
            edits: vec![edit("a", "b")],
            precondition: Precondition::Any,
            idempotency_key: key(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_is_sorted_paginated_and_filters_hidden() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    for name in ["c", "a", ".hidden", "b"] {
        std::fs::write(env.path(name), name).unwrap();
    }
    std::fs::create_dir(env.path("d")).unwrap();
    let list = |limit: Option<u32>, page_token: Option<String>, include_hidden: bool| {
        let params = FsListParams { workspace: ws.clone(), path: String::new(), limit, page_token, include_hidden };
        let peer = client.peer.clone();
        async move { peer.call::<FsList>(params).await.unwrap() }
    };
    let all = list(None, None, false).await;
    let names: Vec<&str> = all.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "d"]);
    assert_eq!(all.entries[3].kind, EntryKind::Dir);
    assert_eq!(all.entries[0].size, 1);
    assert_eq!(all.next_page, None);

    let hidden = list(None, None, true).await;
    assert_eq!(hidden.entries[0].name, ".hidden");

    let first = list(Some(2), None, false).await;
    assert_eq!(first.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    let second = list(Some(2), first.next_page.clone(), false).await;
    assert_eq!(second.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["c", "d"]);
    assert_eq!(second.next_page, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn mkdir_remove_rename() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    client.peer.call::<FsMkdir>(FsMkdirParams { workspace: ws.clone(), path: "x/y/z".into(), idempotency_key: key() }).await.unwrap();
    assert!(env.path("x/y/z").is_dir());
    // mkdir -p of an existing directory is fine; of an existing file is a conflict.
    client.peer.call::<FsMkdir>(FsMkdirParams { workspace: ws.clone(), path: "x/y".into(), idempotency_key: key() }).await.unwrap();
    std::fs::write(env.path("file"), "f").unwrap();
    let err = client
        .peer
        .call::<FsMkdir>(FsMkdirParams { workspace: ws.clone(), path: "file".into(), idempotency_key: key() })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    let remove = |path: &str, recursive: bool| {
        let params = FsRemoveParams { workspace: ws.clone(), path: path.into(), recursive, idempotency_key: key() };
        let peer = client.peer.clone();
        async move { peer.call::<FsRemove>(params).await }
    };
    assert_eq!(remove("x", false).await.unwrap_err().code, ErrorCode::Conflict);
    assert!(env.path("x").exists());
    remove("x", true).await.unwrap();
    assert!(!env.path("x").exists());
    assert_eq!(remove("x", true).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(remove("", true).await.unwrap_err().code, ErrorCode::Denied, "the root cannot be removed");

    let rename = |from: &str, to: &str, overwrite: bool| {
        let params = FsRenameParams { workspace: ws.clone(), from: from.into(), to: to.into(), overwrite, idempotency_key: key() };
        let peer = client.peer.clone();
        async move { peer.call::<FsRename>(params).await }
    };
    std::fs::write(env.path("other"), "o").unwrap();
    assert_eq!(rename("file", "other", false).await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(std::fs::read_to_string(env.path("other")).unwrap(), "o");
    rename("file", "renamed", false).await.unwrap();
    assert!(!env.path("file").exists());
    rename("renamed", "other", true).await.unwrap();
    assert_eq!(std::fs::read_to_string(env.path("other")).unwrap(), "f");
    assert_eq!(rename("missing", "m2", false).await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_files_and_trees() {
    use aim_proto::harness::{FsCopy, FsCopyParams};
    use std::os::unix::fs::symlink;

    let env = env().await;
    let (client, _, ws) = session(&env).await;
    std::fs::create_dir_all(env.path("tree/sub")).unwrap();
    std::fs::write(env.path("tree/a.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(env.path("tree/a.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(env.path("tree/sub/b"), "b").unwrap();
    symlink(env.dir.path(), env.path("tree/out-link")).unwrap();
    let copy = |from: &str, to: &str, overwrite: bool, recursive: bool| {
        let params = FsCopyParams { workspace: ws.clone(), from: from.into(), to: to.into(), overwrite, recursive, idempotency_key: key() };
        let peer = client.peer.clone();
        async move { peer.call::<FsCopy>(params).await }
    };

    copy("tree/a.sh", "a-copy.sh", false, false).await.unwrap();
    assert_eq!(std::fs::read_to_string(env.path("a-copy.sh")).unwrap(), "#!/bin/sh\n");
    assert_eq!(std::fs::metadata(env.path("a-copy.sh")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(copy("tree/sub/b", "a-copy.sh", false, false).await.unwrap_err().code, ErrorCode::Conflict);
    copy("tree/sub/b", "a-copy.sh", true, false).await.unwrap();
    assert_eq!(std::fs::read_to_string(env.path("a-copy.sh")).unwrap(), "b");

    assert_eq!(copy("tree", "tree2", false, false).await.unwrap_err().code, ErrorCode::Conflict, "directories need recursive");
    copy("tree", "tree2", false, true).await.unwrap();
    assert_eq!(std::fs::read_to_string(env.path("tree2/sub/b")).unwrap(), "b");
    // A link inside the tree is copied as a link, not followed out of the root.
    assert!(std::fs::symlink_metadata(env.path("tree2/out-link")).unwrap().file_type().is_symlink());
    assert_eq!(copy("tree", "tree/sub/inner", false, true).await.unwrap_err().code, ErrorCode::Conflict, "not into itself");
    assert_eq!(copy("tree", "tree2", true, true).await.unwrap_err().code, ErrorCode::Conflict, "never merges into a directory");
    assert_eq!(copy("tree/out-link", "stolen", false, true).await.unwrap_err().code, ErrorCode::Denied);
    assert_eq!(copy("missing", "x", false, false).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(copy("tree/sub/b", "../escaped", false, false).await.unwrap_err().code, ErrorCode::Denied);
    let leftovers: Vec<String> = std::fs::read_dir(&env.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(".aimx-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_many_reports_per_file() {
    use aim_proto::harness::{FsReadMany, FsReadManyParams, ReadManyEntry};

    let env = env().await;
    let (client, _, ws) = session(&env).await;
    std::fs::write(env.path("a"), "alpha").unwrap();
    std::fs::write(env.path("b"), "beta").unwrap();
    let params = FsReadManyParams {
        workspace: ws,
        paths: vec!["a".into(), "missing".into(), "../outside".into(), "b".into()],
        max_bytes_per_file: Some(3),
        scope: None,
        prefix_only: false,
    };
    let result = client.peer.call::<FsReadMany>(params).await.unwrap();
    let summary: Vec<String> = result
        .entries
        .into_iter()
        .map(|entry| match entry {
            ReadManyEntry::Ok { path, read } => format!("{path}={}{}", content_string(read.content), if read.truncated { "…" } else { "" }),
            ReadManyEntry::Error { path, code, .. } => format!("{path}!{code}"),
        })
        .collect();
    assert_eq!(summary, ["a=alp…", "missing!not_found", "../outside!denied", "b=bet…"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn prefix_reads_report_size_without_hashing_the_rest() {
    use aim_proto::harness::{FsReadMany, FsReadManyParams, ReadManyEntry};
    use std::io::{Seek as _, SeekFrom, Write as _};

    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let mut file = std::fs::File::create(env.path("huge")).unwrap();
    file.set_len(1 << 30).unwrap();
    file.seek(SeekFrom::Start(1 << 20)).unwrap();
    file.write_all(b"prefix").unwrap();
    drop(file);

    let result = client
        .peer
        .call::<FsRead>(FsReadParams {
            workspace: ws.clone(),
            path: "huge".into(),
            range: Some(ByteRange { start: 1 << 20, len: 17 }),
            scope: None,
            hash: false,
        })
        .await
        .unwrap();
    assert_eq!(result.content.into_bytes(), [b"prefix".as_slice(), &[0; 11]].concat());
    assert_eq!(result.size, 1 << 30);
    assert!(result.hash.is_none());
    assert!(!result.truncated);

    let past_end = client
        .peer
        .call::<FsRead>(FsReadParams {
            workspace: ws.clone(),
            path: "huge".into(),
            range: Some(ByteRange { start: u64::MAX, len: 4 }),
            scope: None,
            hash: false,
        })
        .await
        .unwrap();
    assert!(past_end.content.into_bytes().is_empty());
    assert!(!past_end.truncated);

    let result = client
        .peer
        .call::<FsReadMany>(FsReadManyParams {
            workspace: ws,
            paths: vec!["huge".into()],
            max_bytes_per_file: Some(7),
            scope: None,
            prefix_only: true,
        })
        .await
        .unwrap();
    let Some(ReadManyEntry::Ok { read, .. }) = result.entries.into_iter().next() else { panic!("expected read result") };
    assert_eq!(read.content.len(), 7);
    assert_eq!(read.size, 1 << 30);
    assert!(read.hash.is_none());
    assert!(read.truncated);
}
