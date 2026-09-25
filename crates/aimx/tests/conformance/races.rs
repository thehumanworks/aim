//! Symlink races (REV4-A finding 3; docs/adr/0008 "symlink-race tests check those shell
//! assumptions"): while another process keeps exchanging an in-root directory with a symlink to
//! a directory outside the root, no request may read, list, search, write, create or run anything
//! outside. Each test swaps for about two seconds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aim_proto::harness::{
    Command, ExecSpawn, ExecSpawnParams, ExecWait, ExecWaitParams, FsCopy, FsCopyParams, FsList, FsListParams, FsMkdir, FsMkdirParams,
    FsRead, FsReadParams, FsStat, FsStatParams, FsWrite, FsWriteParams, Grep, GrepParams, Precondition,
};
use aimx::server::local_principal;

use crate::common::{Env, env_with, key, session, text};

const RACE: Duration = Duration::from_secs(2);

/// A workspace whose `d` (a real directory holding `secret` = `inside`) keeps being exchanged
/// with `alt` (a symlink to `<tmp>/outside`, holding `secret` = the sentinel and `outside-only`).
struct Swapping {
    env: Env,
    sentinel: String,
    outside: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    swaps: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Swapping {
    async fn start(read_only: bool) -> Self {
        let env = env_with(|config, root| config.principal = local_principal(&[root], read_only).unwrap()).await;
        let outside = env.dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = format!("OUTSIDE-SENTINEL-{}", std::process::id());
        std::fs::write(outside.join("secret"), &sentinel).unwrap();
        std::fs::write(outside.join("outside-only"), &sentinel).unwrap();
        std::fs::create_dir(env.path("d")).unwrap();
        std::fs::write(env.path("d/secret"), "inside").unwrap();
        std::os::unix::fs::symlink(&outside, env.path("alt")).unwrap();
        let root = std::fs::File::open(&env.root).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let swaps = Arc::new(AtomicU64::new(0));
        let (stopping, counting) = (Arc::clone(&stop), Arc::clone(&swaps));
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                if rustix::fs::renameat_with(&root, "d", &root, "alt", rustix::fs::RenameFlags::EXCHANGE).is_ok() {
                    counting.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        Self { env, sentinel, outside, stop, swaps, thread: Some(thread) }
    }

    fn outside_entries(&self) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(&self.outside).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    }

    fn finish(mut self) -> (Env, u64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        let swaps = self.swaps.load(Ordering::Relaxed);
        assert!(swaps > 100, "the race ran ({swaps} swaps)");
        (self.env, swaps)
    }
}

fn leaked(text: &str, sentinel: &str) -> bool {
    text.contains(sentinel) || text.contains("outside-only")
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_never_see_outside_the_root_while_it_is_swapped() {
    let race = Swapping::start(true).await;
    let (client, _, ws) = session(&race.env).await;
    let deadline = Instant::now() + RACE;
    let mut answered = 0u32;
    while Instant::now() < deadline {
        let read = client.peer.call::<FsRead>(FsReadParams { workspace: ws.clone(), path: "d/secret".into(), range: None }).await;
        if let Ok(read) = read {
            answered += 1;
            let body = String::from_utf8(read.content.into_bytes()).unwrap();
            assert!(!leaked(&body, &race.sentinel), "fs.read returned outside bytes");
        }
        let list = FsListParams { workspace: ws.clone(), path: "d".into(), limit: None, page_token: None, include_hidden: true };
        if let Ok(list) = client.peer.call::<FsList>(list).await {
            assert!(list.entries.iter().all(|e| e.name != "outside-only"), "fs.list listed outside entries");
        }
        let stat = client.peer.call::<FsStat>(FsStatParams { workspace: ws.clone(), path: "d/outside-only".into(), hash: true }).await;
        assert!(stat.is_err(), "fs.stat reached an outside entry: {stat:?}");
        let grep = GrepParams {
            workspace: ws.clone(),
            pattern: "SENTINEL".into(),
            path: Some("d".into()),
            globs: Vec::new(),
            case: aim_proto::harness::CaseMode::Sensitive,
            fixed_strings: true,
            context: 0,
            max_matches: None,
        };
        if let Ok(found) = client.peer.call::<Grep>(grep).await {
            assert!(found.matches.is_empty(), "grep matched outside content: {:?}", found.matches);
        }
    }
    let (_env, swaps) = race.finish();
    assert!(answered > 0, "some reads succeeded ({swaps} swaps)");
}

#[tokio::test(flavor = "multi_thread")]
async fn mutations_and_processes_never_act_outside_the_root_while_it_is_swapped() {
    let race = Swapping::start(false).await;
    let before = race.outside_entries();
    let (client, _, ws) = session(&race.env).await;
    let deadline = Instant::now() + RACE;
    let mut i = 0u32;
    while Instant::now() < deadline {
        i += 1;
        let write = FsWriteParams {
            workspace: ws.clone(),
            path: format!("d/new-{i}/file"),
            content: text("canary"),
            precondition: Precondition::Any,
            create_dirs: true,
            idempotency_key: key(),
        };
        drop(client.peer.call::<FsWrite>(write).await);
        let write = FsWriteParams {
            workspace: ws.clone(),
            path: "d/secret".into(),
            content: text("overwritten"),
            precondition: Precondition::Any,
            create_dirs: false,
            idempotency_key: key(),
        };
        drop(client.peer.call::<FsWrite>(write).await);
        drop(
            client.peer.call::<FsMkdir>(FsMkdirParams { workspace: ws.clone(), path: format!("d/dir-{i}"), idempotency_key: key() }).await,
        );
        let copy = FsCopyParams {
            workspace: ws.clone(),
            from: "d/secret".into(),
            to: format!("d/copy-{i}"),
            overwrite: false,
            recursive: false,
            idempotency_key: key(),
        };
        drop(client.peer.call::<FsCopy>(copy).await);
        if i.is_multiple_of(8) {
            let spawn = ExecSpawnParams {
                workspace: ws.clone(),
                command: Command::Shell { script: format!("echo canary > spawned-{i}") },
                cwd: Some("d".into()),
                env: std::collections::BTreeMap::default(),
                pty: None,
                stdin: false,
                timeout_ms: None,
                idempotency_key: key(),
            };
            if let Ok(spawned) = client.peer.call::<ExecSpawn>(spawn).await {
                drop(client.peer.call::<ExecWait>(ExecWaitParams { proc: spawned.proc, timeout_ms: Some(5000) }).await);
            }
        }
    }
    let secret_outside = std::fs::read_to_string(race.outside.join("secret")).unwrap();
    let after = race.outside_entries();
    let sentinel = race.sentinel.clone();
    let (_env, _) = race.finish();
    assert_eq!(after, before, "nothing was created outside the root");
    assert_eq!(secret_outside, sentinel, "nothing outside was overwritten");
}
