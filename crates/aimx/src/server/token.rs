//! Owner-issued bearer credentials for network harness connections.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::AuthProof;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::authz::Principal;

/// The permission a network bearer grants within the server's configured roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    /// Read workspace files and metadata only.
    Read,
    /// Use the server's configured workspace authority.
    Write,
}

/// A token registry. Only hashes are persisted; the issued token is returned once.
#[derive(Clone, Debug)]
pub struct TokenStore {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Record {
    digest: String,
    scope: TokenScope,
    expires_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    tokens: Vec<Record>,
}

impl TokenStore {
    /// Use the owner's registry below `home`.
    #[must_use]
    pub fn under_home(home: &Path) -> Self {
        Self { path: home.join(".aim/tokens.json") }
    }

    /// Use an explicit registry path, primarily for isolated harness installations and tests.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Issue a random token, persisting only its SHA-256 digest. The caller must display the
    /// returned secret once and must never log it.
    ///
    /// # Errors
    /// Entropy, ownership, permissions, or storage failure.
    pub fn create(&self, scope: TokenScope, ttl: Duration) -> io::Result<String> {
        // The registry is atomically replaced, so lock a stable sibling file across the
        // read/modify/write sequence. The issued token is returned only after its record lands.
        let _registry_lock = self.lock_exclusive()?;
        let mut entropy = [0_u8; 32];
        getrandom::fill(&mut entropy).map_err(io::Error::other)?;
        let token = format!("aimx_{}", hex(&entropy));
        let mut registry = self.read()?;
        registry.tokens.retain(|record| record.expires_ms > now_ms());
        registry.tokens.push(Record {
            digest: digest(&token),
            scope,
            expires_ms: now_ms().saturating_add(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)),
        });
        self.write(&registry)?;
        Ok(token)
    }

    fn lock_exclusive(&self) -> io::Result<fs::File> {
        let parent = self.path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "token registry needs a parent"))?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let lock_path = self.path.with_extension("lock");
        if let Ok(meta) = fs::symlink_metadata(&lock_path)
            && (!meta.file_type().is_file() || meta.permissions().mode() & 0o077 != 0)
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "token lock is not a private regular file"));
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(lock_path)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).map_err(io::Error::other)?;
        Ok(file)
    }

    /// Authenticate an initialize proof and return a principal narrowed by its token scope.
    ///
    /// # Errors
    /// Missing, wrong, or expired credentials; invalid or inaccessible token registry.
    pub fn authenticate(&self, proof: Option<&AuthProof>, base: &Principal) -> Result<Principal, ProtoError> {
        self.authenticate_with_lifetime(proof, base).map(|(principal, _)| principal)
    }

    /// Authenticate and return the remaining lifetime of this bearer, so an attached network
    /// connection cannot outlive its authority.
    ///
    /// # Errors
    /// Missing, wrong, or expired credentials; invalid or inaccessible token registry.
    pub fn authenticate_with_lifetime(&self, proof: Option<&AuthProof>, base: &Principal) -> Result<(Principal, Duration), ProtoError> {
        let Some(AuthProof::Bearer { token }) = proof else {
            return Err(ProtoError::new(ErrorCode::Unauthenticated, "network bearer token required"));
        };
        let registry = self.read().map_err(|_| ProtoError::new(ErrorCode::Unauthenticated, "network bearer unavailable"))?;
        let sought = digest(token);
        let now = now_ms();
        let found =
            registry.tokens.iter().find(|record| constant_time_eq(record.digest.as_bytes(), sought.as_bytes()) && record.expires_ms > now);
        let Some(record) = found else {
            return Err(ProtoError::new(ErrorCode::Unauthenticated, "network bearer invalid or expired"));
        };
        Ok((
            Principal {
                id: format!("token:{}", record.digest),
                roots: base.roots.clone(),
                read_only: base.read_only || record.scope == TokenScope::Read,
            },
            Duration::from_millis(record.expires_ms.saturating_sub(now)),
        ))
    }

    fn read(&self) -> io::Result<Registry> {
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
            Err(err) => return Err(err),
        };
        if !meta.file_type().is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "token registry is not a private regular file"));
        }
        let bytes = fs::read(&self.path)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    fn write(&self, registry: &Registry) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "token registry needs a parent"))?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).map_err(io::Error::other)?;
        let temporary = self.path.with_extension(format!("json.{}.tmp", hex(&suffix)));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
            serde_json::to_writer(&mut file, registry).map_err(io::Error::other)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            drop(fs::remove_file(&temporary));
        }
        result
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn digest(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut result, "{byte:02x}");
    }
    result
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for (a, b) in left.iter().zip(right) {
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_stored_hashed_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::under_home(dir.path());
        let base = Principal { id: "local:1".into(), roots: vec!["/tmp".into()], read_only: false };
        let token = store.create(TokenScope::Read, Duration::from_secs(60)).unwrap();
        let disk = fs::read_to_string(&store.path).unwrap();
        assert!(!disk.contains(&token));
        assert_eq!(fs::metadata(&store.path).unwrap().permissions().mode() & 0o077, 0);
        let auth = AuthProof::Bearer { token: token.clone() };
        assert!(store.authenticate(Some(&auth), &base).unwrap().read_only);
        assert!(store.authenticate(None, &base).is_err());
        assert!(store.authenticate(Some(&AuthProof::Bearer { token: "wrong".into() }), &base).is_err());
        let expired = store.create(TokenScope::Write, Duration::ZERO).unwrap();
        assert!(store.authenticate(Some(&AuthProof::Bearer { token: expired }), &base).is_err());
    }

    #[test]
    fn concurrent_issuance_keeps_both_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::under_home(dir.path());
        let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
        let tasks = (0..2)
            .map(|_| {
                let store = store.clone();
                let gate = std::sync::Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    store.create(TokenScope::Read, Duration::from_secs(60)).unwrap()
                })
            })
            .collect::<Vec<_>>();
        gate.wait();
        let base = Principal { id: "local:1".into(), roots: vec!["/tmp".into()], read_only: false };
        for task in tasks {
            let token = task.join().unwrap();
            assert!(store.authenticate(Some(&AuthProof::Bearer { token }), &base).is_ok());
        }
    }
}
