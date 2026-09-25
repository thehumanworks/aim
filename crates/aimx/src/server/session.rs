//! Sessions: what a client owns across connections (workspaces and processes), resume, and the
//! push of process output to whichever connection is attached.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{CallScope, ContentHash, ExecExited, ExecExitedParams, ExecOutput, ExecOutputParams, WorkspaceInfo};
use aim_proto::ids::{ProcId, WorkspaceId};
use aim_rpc::Peer;
use tokio::sync::watch;

use super::{State, lock};
use crate::authz::{Grant, Principal};
use crate::tools::ProcTable;
use crate::workspace::{Outcome, Workspace};
use aim_kernel::policy::Limits as PolicyLimits;

/// Output bytes pushed per `exec.output` batch read.
const PUSH_BYTES: u64 = 256 * 1024;
/// How long one forwarder read waits for new output.
const PUSH_WAIT: Duration = Duration::from_secs(30);

/// A workspace opened in a session.
pub(crate) struct OpenWorkspace {
    pub(crate) id: WorkspaceId,
    pub(crate) info: WorkspaceInfo,
    pub(crate) grant: Grant,
    pub(crate) backend: Arc<dyn Workspace>,
}

/// A claimed file owned by one harness session. The marker is removed only if unchanged.
pub(crate) struct Reservation {
    pub(crate) workspace: WorkspaceId,
    pub(crate) path: String,
    pub(crate) hash: ContentHash,
    pub(crate) backend: Arc<dyn Workspace>,
    pub(crate) active: bool,
}

/// The connection a session is attached to.
#[derive(Clone, Debug)]
struct Attachment {
    conn: u64,
    peer: Peer,
}

/// A client's session.
pub(crate) struct Session {
    pub(crate) token: String,
    pub(crate) principal: Arc<Principal>,
    workspaces: Mutex<HashMap<WorkspaceId, Arc<OpenWorkspace>>>,
    max_workspaces: usize,
    pub(crate) procs: Arc<ProcTable>,
    attached: watch::Sender<Option<Attachment>>,
    detached_at: Mutex<Option<Instant>>,
    ceiling: Mutex<Option<CallScope>>,
    reservations: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Reservation>>>>,
}

impl Session {
    fn new(token: String, principal: Arc<Principal>, procs: ProcTable, max_workspaces: usize) -> Self {
        Self {
            token,
            principal,
            workspaces: Mutex::new(HashMap::new()),
            max_workspaces,
            procs: Arc::new(procs),
            attached: watch::channel(None).0,
            detached_at: Mutex::new(Some(Instant::now())),
            ceiling: Mutex::new(None),
            reservations: Mutex::new(HashMap::new()),
        }
    }

    /// Attaches the session to a connection; returns the peer it was attached to before (which the
    /// caller closes: a resumed session belongs to one connection at a time).
    pub(crate) fn attach(&self, conn: u64, peer: Peer) -> Option<Peer> {
        let mut detached_at = lock(&self.detached_at);
        *detached_at = None;
        self.attached.send_replace(Some(Attachment { conn, peer })).filter(|old| old.conn != conn).map(|old| old.peer)
    }

    /// Detaches connection `conn` (if it is still the attached one) and starts the resume clock.
    pub(crate) fn detach(&self, conn: u64) {
        self.detach_after_clear(conn, || {});
    }

    fn detach_after_clear(&self, conn: u64, after_clear: impl FnOnce()) {
        let mut detached_at = lock(&self.detached_at);
        let detached = self.attached.send_if_modified(|current| {
            if current.as_ref().is_some_and(|a| a.conn == conn) {
                *current = None;
                true
            } else {
                false
            }
        });
        if detached {
            after_clear();
            *detached_at = Some(Instant::now());
        }
    }

    /// Whether the session has been detached for longer than `ttl`.
    pub(crate) fn expired(&self, ttl: Duration) -> bool {
        lock(&self.detached_at).is_some_and(|at| at.elapsed() > ttl)
    }

    pub(crate) fn workspace(&self, id: &WorkspaceId) -> Outcome<Arc<OpenWorkspace>> {
        lock(&self.workspaces).get(id).cloned().ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("unknown workspace `{id}`")))
    }

    pub(crate) fn find_root(&self, root: &str) -> Option<Arc<OpenWorkspace>> {
        lock(&self.workspaces).values().find(|ws| ws.info.root == root).cloned()
    }

    /// The bound ceiling survives reconnection and cannot be omitted by a later call.
    pub(crate) fn ceiling(&self) -> Option<CallScope> {
        lock(&self.ceiling).clone()
    }

    /// Binds or narrows the session ceiling atomically with respect to another workspace open.
    pub(crate) fn bind_ceiling(&self, grant: &Grant, requested: &CallScope, limits: PolicyLimits) -> Outcome<()> {
        let mut canonical = grant.ceiling(requested, limits)?;
        let mut slot = lock(&self.ceiling);
        if let Some(earlier) = slot.as_ref() {
            canonical.deny_write.extend(earlier.deny_write.iter().cloned());
            canonical.deny_write.sort();
            canonical.deny_write.dedup();
            if !grant.ceiling_narrows(&canonical, earlier, limits)? {
                return Err(ProtoError::new(ErrorCode::Denied, "workspace ceiling would widen this session's bound authority"));
            }
        }
        *slot = Some(canonical);
        Ok(())
    }

    /// Whether the session may open another workspace.
    ///
    /// # Errors
    /// `limit_exceeded` when it has as many open as it may.
    pub(crate) fn may_add_workspace(&self) -> Outcome<()> {
        if lock(&self.workspaces).len() >= self.max_workspaces {
            return Err(ProtoError::new(
                ErrorCode::LimitExceeded,
                format!("this session has {} workspaces open, the most it may", self.max_workspaces),
            ));
        }
        Ok(())
    }

    pub(crate) fn add_workspace(&self, workspace: Arc<OpenWorkspace>) -> Outcome<()> {
        let mut workspaces = lock(&self.workspaces);
        if workspaces.len() >= self.max_workspaces {
            return Err(ProtoError::new(ErrorCode::LimitExceeded, "this session has as many workspaces open as it may"));
        }
        workspaces.insert(workspace.id.clone(), workspace);
        Ok(())
    }

    pub(crate) fn add_reservation(&self, id: String, reservation: Reservation) -> Outcome<()> {
        let mut reservations = lock(&self.reservations);
        if reservations.len() >= 64 {
            return Err(ProtoError::new(ErrorCode::LimitExceeded, "too many live file reservations"));
        }
        reservations.insert(id, Arc::new(tokio::sync::Mutex::new(reservation)));
        Ok(())
    }

    pub(crate) fn reservation(&self, id: &str) -> Outcome<Arc<tokio::sync::Mutex<Reservation>>> {
        lock(&self.reservations).get(id).cloned().ok_or_else(|| ProtoError::new(ErrorCode::NotFound, "unknown file reservation"))
    }

    pub(crate) fn remove_reservation(&self, id: &str) {
        lock(&self.reservations).remove(id);
    }

    /// The workspace a process of this session runs in.
    pub(crate) fn proc_workspace(&self, proc: &ProcId) -> Outcome<Arc<OpenWorkspace>> {
        let id = self.procs.workspace(proc).ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("unknown process `{proc}`")))?;
        self.workspace(&id)
    }

    /// Ends the session: releases (kills) its processes and forgets its workspaces.
    pub(crate) async fn close(&self) {
        for (proc, workspace) in self.procs.all() {
            if let Ok(workspace) = self.workspace(&workspace)
                && let Some(exec) = workspace.backend.exec()
                && let Err(err) = exec.release(&proc).await
            {
                tracing::debug!(%err, %proc, "releasing a process at session end failed");
            }
            self.procs.remove(&proc);
        }
        let reservations: Vec<_> = lock(&self.reservations).drain().map(|(_, reservation)| reservation).collect();
        for reservation in reservations {
            let mut claim = reservation.lock().await;
            if claim.active {
                if let Err(err) = claim.backend.fs().cancel_if_hash(&claim.path, &claim.hash).await {
                    tracing::warn!(%err, "could not clean up abandoned file reservation");
                }
                claim.active = false;
            }
        }
        lock(&self.workspaces).clear();
        self.attached.send_replace(None);
    }

    /// Pushes a process's output (`exec.output`) and exit (`exec.exited`) to the attached
    /// connection, in `seq` order. While no connection is attached nothing is consumed; after a
    /// reconnect the push resumes from the last delivered chunk (clients deduplicate by `seq`,
    /// and `exec.read {after_seq}` remains the reconciliation read).
    pub(crate) fn forward(self: &Arc<Self>, backend: Arc<dyn Workspace>, proc: ProcId) {
        tokio::spawn(forward(Arc::downgrade(self), backend, proc));
    }
}

async fn forward(session: Weak<Session>, backend: Arc<dyn Workspace>, proc: ProcId) {
    let Some(mut attached) = session.upgrade().map(|s| s.attached.subscribe()) else { return };
    drop(session);
    let Some(exec) = backend.exec() else { return };
    let mut cursor = 0u64;
    loop {
        let current = attached.borrow_and_update().clone();
        let Some(Attachment { peer, .. }) = current else {
            if attached.changed().await.is_err() {
                return;
            }
            continue;
        };
        let read = tokio::select! {
            read = exec.read(&proc, cursor, PUSH_BYTES, PUSH_WAIT) => read,
            changed = attached.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
        };
        // Released, or the session ended.
        let Ok(read) = read else { return };
        let mut delivered = true;
        for chunk in read.chunks {
            let seq = chunk.seq;
            if peer.notify::<ExecOutput>(ExecOutputParams { proc: proc.clone(), chunk }).await.is_err() {
                delivered = false;
                break;
            }
            cursor = seq;
        }
        if delivered && let Some(status) = read.exit {
            if peer.notify::<ExecExited>(ExecExitedParams { proc: proc.clone(), status, last_seq: cursor }).await.is_ok() {
                return;
            }
            delivered = false;
        }
        // The connection went away: wait for the next one.
        if !delivered && attached.changed().await.is_err() {
            return;
        }
    }
}

impl State {
    /// Checks the resume window and attaches while holding the session-map lock. The reaper holds
    /// that same lock through expiry selection and removal, so it cannot remove a session between
    /// a successful resume lookup and the replacement attachment.
    pub(crate) fn resume_and_attach(
        &self,
        token: &str,
        principal: &Principal,
        conn: u64,
        peer: Peer,
    ) -> Option<(Arc<Session>, Option<Peer>)> {
        let sessions = lock(&self.sessions);
        let session = sessions.get(token).filter(|s| s.principal.id == principal.id && !s.expired(self.config.resume_ttl))?;
        let previous = session.attach(conn, peer);
        Some((Arc::clone(session), previous))
    }

    /// Starts a new session.
    ///
    /// # Errors
    /// `limit_exceeded` when the server holds as many sessions (attached or awaiting resume) as it
    /// may.
    pub(crate) fn new_session(&self, principal: Arc<Principal>) -> Outcome<Arc<Session>> {
        let token = crate::id::secret_hex().ok_or_else(|| ProtoError::new(ErrorCode::Internal, "the OS random number generator failed"))?;
        let procs = ProcTable::new(usize::from(self.config.max_procs_per_session), Arc::clone(&self.procs));
        let session = Arc::new(Session::new(token.clone(), principal, procs, self.config.max_workspaces_per_session));
        let mut sessions = lock(&self.sessions);
        if sessions.len() >= self.config.max_sessions {
            return Err(ProtoError::new(
                ErrorCode::LimitExceeded,
                format!("the server holds {} sessions, the most it may; retry once one ends or its resume window passes", sessions.len()),
            ));
        }
        sessions.insert(token, Arc::clone(&session));
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::mpsc;

    use aim_rpc::{NoHandler, PeerConfig};
    use tokio::sync::Semaphore;

    use super::*;

    fn peer() -> Peer {
        Peer::spawn(tokio::io::empty(), tokio::io::sink(), NoHandler, PeerConfig::default())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacement_attach_outlives_old_detach() {
        let principal = Arc::new(Principal { id: "test".into(), roots: Vec::new(), read_only: false });
        let procs = ProcTable::new(1, Arc::new(Semaphore::new(1)));
        let session = Arc::new(Session::new("token".into(), principal, procs, 1));
        session.attach(1, peer());

        let cleared = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let old = Arc::clone(&session);
        let old_cleared = Arc::clone(&cleared);
        let old_release = Arc::clone(&release);
        let old_detach = std::thread::spawn(move || {
            old.detach_after_clear(1, || {
                old_cleared.wait();
                old_release.wait();
            });
        });
        cleared.wait();

        let (attached_tx, attached_rx) = mpsc::channel();
        let replacement = Arc::clone(&session);
        let new_peer = peer();
        let new_attach = std::thread::spawn(move || {
            replacement.attach(2, new_peer);
            attached_tx.send(()).unwrap();
        });
        // Before the fix, replacement attachment can finish before the old detach writes its
        // timestamp. After the fix it waits for the same lifecycle lock.
        let _ = attached_rx.recv_timeout(Duration::from_secs(1));
        release.wait();
        old_detach.join().unwrap();
        new_attach.join().unwrap();

        assert_eq!(session.attached.borrow().as_ref().map(|a| a.conn), Some(2));
        assert!(!session.expired(Duration::ZERO), "an attached session must never expire");
    }
}
