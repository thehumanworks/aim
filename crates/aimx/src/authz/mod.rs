//! Principals, grants and enforcement (docs/adr/0008, 0021; docs/architecture.md §10.1, §12).
//!
//! aimx enforces every request itself, whoever the caller is: a [`Principal`] (who the connection
//! authenticated as, with the workspace roots it may open and whether it may only read) and a
//! [`Grant`] (that principal's authority over one open workspace) decide every path and mutation
//! before a backend sees it. Protected paths ([`ProtectedPaths`]) are write-denied for everyone.
//!
//! No I/O happens here; canonicalising roots is the server's job.

pub mod confine;
pub mod identity;

use std::os::fd::OwnedFd;
use std::sync::Arc;

use aim_kernel::policy::{self, Limits as PolicyLimits, Op, OpSet, Root, Scope};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{CallScope, PrincipalInfo};

use self::confine::{confine, is_within, normalize, relative_to};

/// An authenticated caller.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Principal {
    /// Stable id, e.g. `local:501`.
    pub id: String,
    /// Canonical absolute directories under which the principal may open workspaces.
    pub roots: Vec<String>,
    /// The principal may only read (no writes, no processes, no mutating tools).
    pub read_only: bool,
    /// A network bearer's bound session ceiling. Local principals have no extra ceiling.
    pub ceiling: Option<CallScope>,
}

impl Principal {
    /// The wire description of the principal.
    #[must_use]
    pub fn info(&self) -> PrincipalInfo {
        PrincipalInfo { id: self.id.clone(), roots: self.roots.clone(), read_only: self.read_only }
    }

    /// Whether the principal may open a workspace rooted at the canonical path `root`: one of its
    /// roots or a directory below one.
    #[must_use]
    pub fn may_open(&self, root: &str) -> bool {
        normalize(root).is_some_and(|root| self.roots.iter().any(|granted| normalize(granted).is_some_and(|g| is_within(&root, &g))))
    }
}

/// Paths nobody may modify through aimx (the gate, the evolution ledger, and user additions).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProtectedPaths {
    paths: Vec<String>,
}

impl ProtectedPaths {
    /// A set of absolute paths (normalised; relative entries are ignored).
    #[must_use]
    pub fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let mut paths: Vec<String> = paths.into_iter().filter_map(|p| normalize(&p)).collect();
        paths.sort();
        paths.dedup();
        Self { paths }
    }

    /// The default set for a home directory: `~/.aim/gate`, `~/.aim/ledger`, the policy file
    /// `~/.aim/protected` itself (so aimx cannot be used to widen its own policy; the owner edits
    /// it directly), and every path listed in it (`listing` is that file's content, if it
    /// exists). Lines are absolute paths or `~/`-relative; blank lines and `#` comments are
    /// skipped. `~/.aim` itself stays writable, but it cannot be removed, moved or replaced, since
    /// it contains protected paths.
    #[must_use]
    pub fn defaults(home: &str, listing: Option<&str>) -> Self {
        let home = home.trim_end_matches('/');
        let mut paths = vec![format!("{home}/.aim/gate"), format!("{home}/.aim/ledger"), format!("{home}/.aim/protected")];
        for line in listing.unwrap_or_default().lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("~/") {
                paths.push(format!("{home}/{rest}"));
            } else if line.starts_with('/') {
                paths.push(line.to_owned());
            }
        }
        Self::new(paths)
    }

    /// Adds more paths.
    #[must_use]
    pub fn with(mut self, more: impl IntoIterator<Item = String>) -> Self {
        self.paths.extend(more.into_iter().filter_map(|p| normalize(&p)));
        self.paths.sort();
        self.paths.dedup();
        self
    }

    /// The protected paths.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Whether writing `path` modifies a protected path (it is one, or lies inside one).
    #[must_use]
    pub fn guards(&self, path: &str) -> bool {
        self.paths.iter().any(|p| is_within(path, p))
    }

    /// Whether removing or moving `path` affects a protected path (it is one, lies inside one, or
    /// contains one).
    #[must_use]
    pub fn guards_tree(&self, path: &str) -> bool {
        self.paths.iter().any(|p| is_within(path, p) || is_within(p, path))
    }
}

/// What a request does to a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Reads it (or runs a process in it).
    Read,
    /// Creates or changes it.
    Write,
    /// Removes or moves it, with everything below it.
    Tree,
}

/// A principal's authority over one open workspace.
#[derive(Clone, Debug)]
pub struct Grant {
    principal: Arc<Principal>,
    protected: Arc<ProtectedPaths>,
    root: String,
    alias: Option<String>,
    /// The same local root descriptor the backend uses; absent for remote backends.
    local_root: Option<Arc<OwnedFd>>,
    effective: Option<Arc<Scope>>,
    tree_denies: Vec<String>,
}

impl Grant {
    /// The grant of `principal` over the workspace at canonical `root`. `alias` is the root as the
    /// client spelled it when that differs (e.g. `/var/…` for `/private/var/…` on macOS); absolute
    /// paths under the alias are accepted and rebased onto the root.
    #[must_use]
    pub fn new(principal: Arc<Principal>, protected: Arc<ProtectedPaths>, root: String, alias: Option<String>) -> Self {
        let alias = alias.and_then(|a| normalize(&a)).filter(|a| *a != root);
        Self { principal, protected, root, alias, local_root: None, effective: None, tree_denies: Vec::new() }
    }

    /// Anchors this local grant to the descriptor selected before authorization.
    pub(crate) fn bind_local_root(mut self, descriptor: Arc<OwnedFd>) -> Self {
        self.local_root = Some(descriptor);
        self
    }

    /// The canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The protected paths.
    #[must_use]
    pub fn protected(&self) -> &Arc<ProtectedPaths> {
        &self.protected
    }

    /// Confines `user_path` to the workspace and checks that `access` is allowed there.
    ///
    /// # Errors
    /// `denied` outside the root, for mutations by a read-only principal, on protected paths and
    /// for removing or moving the root itself; `invalid_params` for malformed paths.
    pub fn path(&self, user_path: &str, access: Access) -> Result<String, ProtoError> {
        let path = self.confine(user_path)?;
        match access {
            Access::Read => {}
            Access::Write => {
                self.mutation()?;
                if self.protected.guards(&path) {
                    return Err(protected(&path));
                }
            }
            Access::Tree => {
                self.mutation()?;
                if path == self.root {
                    return Err(ProtoError::new(ErrorCode::Denied, "the workspace root cannot be removed or moved"));
                }
                if self.protected.guards_tree(&path) {
                    return Err(protected(&path));
                }
                if self.tree_denies.iter().any(|denied| is_within(denied, &path)) {
                    return Err(ProtoError::new(ErrorCode::Denied, format!("`{path}` contains a write-denied scope prefix")));
                }
            }
        }
        self.permit(if access == Access::Read { Op::Read } else { Op::Write }, &path)?;
        Ok(path)
    }

    /// Confines a process cwd and checks executable authority there.
    ///
    /// # Errors
    /// `denied` if the principal or effective call scope forbids execution at this cwd.
    pub fn exec_path(&self, user_path: &str) -> Result<String, ProtoError> {
        let path = self.confine(user_path)?;
        self.mutation()?;
        self.permit(Op::Exec, &path)?;
        Ok(path)
    }

    fn confine(&self, user_path: &str) -> Result<String, ProtoError> {
        match confine(&self.root, user_path) {
            Err(err) if err.code == ErrorCode::Denied && user_path.starts_with('/') => {
                let Some(alias) = &self.alias else { return Err(err) };
                let aliased = confine(alias, user_path).map_err(|_| err)?;
                let rest = relative_to(&aliased, alias).unwrap_or_default();
                confine(&self.root, rest)
            }
            other => other,
        }
    }

    /// Checks that the principal may mutate anything at all.
    ///
    /// # Errors
    /// `denied` for a read-only principal.
    pub fn mutation(&self) -> Result<(), ProtoError> {
        if self.principal.read_only {
            Err(ProtoError::new(ErrorCode::Denied, format!("principal `{}` is read-only", self.principal.id)))
        } else {
            Ok(())
        }
    }

    /// Checks that the principal may run processes (a process can change anything it can reach, so
    /// this is a mutation).
    ///
    /// # Errors
    /// `denied` for a read-only principal.
    pub fn exec(&self) -> Result<(), ProtoError> {
        self.exec_path("").map(|_| ())
    }

    /// Applies the verified policy fold, with the authenticated principal first.
    ///
    /// # Errors
    /// `invalid_params` for malformed scope data; `denied` for a scope that widens its parent.
    pub(crate) fn scoped(&self, session: Option<&CallScope>, call: Option<&CallScope>, limits: PolicyLimits) -> Result<Self, ProtoError> {
        let principal = self.principal_scope(limits)?;
        let mut scopes = vec![principal];
        let mut inherited_denies = self.protected.paths().to_vec();
        let mut tree_denies = Vec::new();
        if let Some(session) = session {
            let parent = scopes.first().ok_or_else(|| ProtoError::new(ErrorCode::Internal, "principal scope is missing"))?;
            let (canonical, parsed) = parse_scope(session, &self.root, parent.limits(), &inherited_denies)?;
            if !parsed.narrows(parent) {
                return Err(ProtoError::new(ErrorCode::Denied, "session ceiling exceeds the authenticated principal grant"));
            }
            scopes.push(parsed);
            inherited_denies.extend(canonical.deny_write.iter().cloned());
            tree_denies.extend(canonical.deny_write);
        }
        if let Some(call) = call {
            let parent = policy::effective(&scopes);
            let (canonical, parsed) = parse_scope(call, &self.root, parent.limits(), &inherited_denies)?;
            if !parsed.narrows(&parent) {
                return Err(ProtoError::new(ErrorCode::Denied, "per-call scope exceeds the principal or session ceiling"));
            }
            scopes.push(parsed);
            tree_denies.extend(canonical.deny_write);
        }
        let mut grant = self.clone();
        grant.effective = Some(Arc::new(policy::effective(&scopes)));
        grant.tree_denies = tree_denies;
        Ok(grant)
    }

    /// Canonicalizes a ceiling at `workspace.open`, filling omitted limits from the principal.
    ///
    /// # Errors
    /// `invalid_params` for malformed scope data; `denied` for a ceiling wider than the principal.
    pub(crate) fn ceiling(&self, requested: &CallScope, limits: PolicyLimits) -> Result<CallScope, ProtoError> {
        let principal = self.principal_scope(limits)?;
        let (canonical, parsed) = parse_scope(requested, &self.root, principal.limits(), self.protected.paths())?;
        if !parsed.narrows(&principal) {
            return Err(ProtoError::new(ErrorCode::Denied, "session ceiling exceeds the authenticated principal grant"));
        }
        Ok(canonical)
    }

    /// Whether a canonical replacement ceiling stays within an earlier bound ceiling.
    pub(crate) fn ceiling_narrows(&self, replacement: &CallScope, earlier: &CallScope, limits: PolicyLimits) -> Result<bool, ProtoError> {
        let (_, earlier_scope) = parse_scope(earlier, &self.root, limits, self.protected.paths())?;
        let mut inherited_denies = self.protected.paths().to_vec();
        inherited_denies.extend(earlier.deny_write.iter().cloned());
        let (_, replacement) = parse_scope(replacement, &self.root, earlier_scope.limits(), &inherited_denies)?;
        Ok(replacement.narrows(&earlier_scope))
    }

    fn principal_scope(&self, limits: PolicyLimits) -> Result<Scope, ProtoError> {
        let roots = self.principal.roots.iter().map(|path| kernel_root(path)).collect::<Result<Vec<_>, _>>()?;
        let denies = self.protected.paths().iter().map(|path| kernel_root(path)).collect::<Result<Vec<_>, _>>()?;
        Ok(Scope::new(roots, OpSet::new(true, !self.principal.read_only, !self.principal.read_only), denies, limits))
    }

    fn permit(&self, op: Op, path: &str) -> Result<(), ProtoError> {
        if let Some(scope) = &self.effective
            && !scope.permits(op, &kernel_root(path)?)
        {
            return Err(ProtoError::new(ErrorCode::Denied, format!("effective authority denies {op:?} at `{path}`")));
        }
        Ok(())
    }

    /// The effective process/output caps after all supplied ceilings are intersected.
    #[must_use]
    pub(crate) fn limits(&self) -> Option<PolicyLimits> {
        self.effective.as_ref().map(|scope| scope.limits())
    }

    /// Canonical write-denied prefixes added by the bound session and this call.
    #[must_use]
    pub(crate) fn write_denies(&self) -> &[String] {
        &self.tree_denies
    }

    /// `path` relative to the workspace root, for display (`.` for the root itself).
    #[must_use]
    pub fn display<'a>(&self, path: &'a str) -> &'a str {
        match relative_to(path, &self.root) {
            Some("") => ".",
            Some(rel) => rel,
            None => path,
        }
    }
}

fn kernel_root(path: &str) -> Result<Root, ProtoError> {
    let segments = path.split('/').filter(|segment| !segment.is_empty()).map(|segment| segment.as_bytes().to_vec()).collect();
    Root::new(segments).ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, format!("invalid scope path `{path}`")))
}

/// A normalized absolute scope, plus its verified policy representation.
fn parse_scope(
    requested: &CallScope,
    workspace: &str,
    inherited: PolicyLimits,
    inherited_denies: &[String],
) -> Result<(CallScope, Scope), ProtoError> {
    let canonical_path = |raw: &str| -> Result<String, ProtoError> {
        if raw == "/" {
            return Ok("/".to_owned());
        }
        let body = raw.strip_prefix('/').unwrap_or(raw);
        if body.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\0')) {
            return Err(ProtoError::new(ErrorCode::InvalidParams, format!("scope path `{raw}` is not normalized")));
        }
        Ok(if raw.starts_with('/') {
            raw.to_owned()
        } else if workspace == "/" {
            format!("/{raw}")
        } else {
            format!("{workspace}/{raw}")
        })
    };
    let roots = requested.roots.iter().map(|path| canonical_path(path)).collect::<Result<Vec<_>, _>>()?;
    let deny_write = requested.deny_write.iter().map(|path| canonical_path(path)).collect::<Result<Vec<_>, _>>()?;
    let mut read = false;
    let mut write = false;
    let mut exec = false;
    for op in &requested.ops {
        match op.as_str() {
            "read" => read = true,
            "write" => write = true,
            "exec" => exec = true,
            _ => return Err(ProtoError::new(ErrorCode::InvalidParams, format!("unknown scope operation `{op}`"))),
        }
    }
    let limits = PolicyLimits {
        max_processes: requested.max_processes.unwrap_or(inherited.max_processes),
        max_output_bytes: requested.max_output_bytes.unwrap_or(inherited.max_output_bytes),
    };
    let mut effective_denies = deny_write.clone();
    effective_denies.extend(inherited_denies.iter().cloned());
    let scope = Scope::new(
        roots.iter().map(|path| kernel_root(path)).collect::<Result<Vec<_>, _>>()?,
        OpSet::new(read, write, exec),
        effective_denies.iter().map(|path| kernel_root(path)).collect::<Result<Vec<_>, _>>()?,
        limits,
    );
    let canonical = CallScope {
        roots,
        ops: requested.ops.clone(),
        deny_write,
        max_processes: Some(limits.max_processes),
        max_output_bytes: Some(limits.max_output_bytes),
    };
    Ok((canonical, scope))
}

fn protected(path: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Denied, format!("`{path}` is a protected path"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(read_only: bool) -> Grant {
        let principal = Arc::new(Principal { id: "local:1".into(), roots: vec!["/home/u".into()], read_only, ceiling: None });
        let protected = Arc::new(ProtectedPaths::defaults("/home/u", Some("# comment\n~/secret\n/home/u/w/locked\nrelative\n")));
        Grant::new(principal, protected, "/home/u/w".into(), Some("/alias/w".into()))
    }

    #[test]
    fn defaults_parse_the_listing() {
        let set = ProtectedPaths::defaults("/home/u/", Some("~/x\n  /abs/y  \n#no\n\nrel"));
        assert_eq!(set.paths(), ["/abs/y", "/home/u/.aim/gate", "/home/u/.aim/ledger", "/home/u/.aim/protected", "/home/u/x"]);
    }

    #[test]
    fn roots_bound_workspaces() {
        let principal = Principal { id: "p".into(), roots: vec!["/home/u".into()], read_only: false, ceiling: None };
        assert!(principal.may_open("/home/u"));
        assert!(principal.may_open("/home/u/w"));
        assert!(!principal.may_open("/home/ux"));
        assert!(!principal.may_open("/"));
    }

    #[test]
    fn paths_are_confined_and_protected() {
        let g = grant(false);
        assert_eq!(g.path("a.txt", Access::Read).unwrap(), "/home/u/w/a.txt");
        assert_eq!(g.path("/alias/w/a.txt", Access::Write).unwrap(), "/home/u/w/a.txt");
        assert_eq!(g.path("../x", Access::Read).unwrap_err().code, ErrorCode::Denied);
        assert_eq!(g.path("/alias/x", Access::Read).unwrap_err().code, ErrorCode::Denied);
        assert_eq!(g.path("locked/f", Access::Write).unwrap_err().code, ErrorCode::Denied);
        assert!(g.path("locked/f", Access::Read).is_ok());
        assert_eq!(g.path("", Access::Tree).unwrap_err().code, ErrorCode::Denied);
        assert_eq!(g.display("/home/u/w/a/b"), "a/b");
        assert_eq!(g.display("/home/u/w"), ".");
    }

    #[test]
    fn tree_operations_cannot_take_protected_children() {
        let principal = Arc::new(Principal { id: "p".into(), roots: vec!["/home/u".into()], read_only: false, ceiling: None });
        let protected = Arc::new(ProtectedPaths::defaults("/home/u", None));
        let g = Grant::new(principal, protected, "/home/u".into(), None);
        assert_eq!(g.path(".aim", Access::Tree).unwrap_err().code, ErrorCode::Denied);
        assert!(g.path(".aim/other", Access::Tree).is_ok());
        assert_eq!(g.path(".aim/gate/x", Access::Write).unwrap_err().code, ErrorCode::Denied);
    }

    #[test]
    fn read_only_principals_cannot_mutate() {
        let g = grant(true);
        assert!(g.path("a", Access::Read).is_ok());
        assert_eq!(g.path("a", Access::Write).unwrap_err().code, ErrorCode::Denied);
        assert_eq!(g.path("a", Access::Tree).unwrap_err().code, ErrorCode::Denied);
        assert_eq!(g.exec().unwrap_err().code, ErrorCode::Denied);
    }
}
