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

use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::PrincipalInfo;

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

    /// The default set for a home directory: `~/.aim/gate`, `~/.aim/ledger`, and every path
    /// listed in `~/.aim/protected` (`listing` is that file's content, if it exists). Lines are
    /// absolute paths or `~/`-relative; blank lines and `#` comments are skipped.
    #[must_use]
    pub fn defaults(home: &str, listing: Option<&str>) -> Self {
        let home = home.trim_end_matches('/');
        let mut paths = vec![format!("{home}/.aim/gate"), format!("{home}/.aim/ledger")];
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
}

impl Grant {
    /// The grant of `principal` over the workspace at canonical `root`. `alias` is the root as the
    /// client spelled it when that differs (e.g. `/var/…` for `/private/var/…` on macOS); absolute
    /// paths under the alias are accepted and rebased onto the root.
    #[must_use]
    pub fn new(principal: Arc<Principal>, protected: Arc<ProtectedPaths>, root: String, alias: Option<String>) -> Self {
        let alias = alias.and_then(|a| normalize(&a)).filter(|a| *a != root);
        Self { principal, protected, root, alias }
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
        // TODO(ADR-per-call-scope): intersect with the caller's per-call ceiling (REV4-A finding 4,
        // ADR 0008/0021 narrow-only delegation) once the protocol carries one; until then only the
        // principal's own authority is enforced here and in `mutation`/`exec`.
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
            }
        }
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
        self.mutation()
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

fn protected(path: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Denied, format!("`{path}` is a protected path"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(read_only: bool) -> Grant {
        let principal = Arc::new(Principal { id: "local:1".into(), roots: vec!["/home/u".into()], read_only });
        let protected = Arc::new(ProtectedPaths::defaults("/home/u", Some("# comment\n~/secret\n/home/u/w/locked\nrelative\n")));
        Grant::new(principal, protected, "/home/u/w".into(), Some("/alias/w".into()))
    }

    #[test]
    fn defaults_parse_the_listing() {
        let set = ProtectedPaths::defaults("/home/u/", Some("~/x\n  /abs/y  \n#no\n\nrel"));
        assert_eq!(set.paths(), ["/abs/y", "/home/u/.aim/gate", "/home/u/.aim/ledger", "/home/u/x"]);
    }

    #[test]
    fn roots_bound_workspaces() {
        let principal = Principal { id: "p".into(), roots: vec!["/home/u".into()], read_only: false };
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
        let principal = Arc::new(Principal { id: "p".into(), roots: vec!["/home/u".into()], read_only: false });
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
