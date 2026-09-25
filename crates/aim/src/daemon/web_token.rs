//! Owner-issued bearer tokens for browser daemon connections.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::AuthProof;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Serialize, Deserialize)]
struct Record {
    digest: String,
    expires_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    tokens: Vec<Record>,
}

/// Private registry of SHA-256 token digests below `AIM_HOME`.
#[derive(Clone, Debug)]
pub struct WebTokenStore {
    path: PathBuf,
}

impl WebTokenStore {
    /// Use the daemon's default registry in an aim home.
    #[must_use]
    pub fn under_home(home: &Path) -> Self {
        Self::at(home.join("web-tokens.json"))
    }

    /// Use an explicit registry path for isolated installations and tests.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Issue a random bearer once, persisting only its digest and expiry.
    ///
    /// # Errors
    /// Returns an error if the private registry cannot be locked or written.
    pub fn create(&self, ttl: Duration) -> io::Result<String> {
        if ttl.is_zero() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "token lifetime must be positive"));
        }
        let _lock = self.lock_exclusive()?;
        let token = format!("aim_web_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let now = now_ms();
        let mut registry = self.read()?;
        registry.tokens.retain(|record| record.expires_ms > now);
        registry
            .tokens
            .push(Record { digest: digest(&token), expires_ms: now.saturating_add(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)) });
        self.write(&registry)?;
        Ok(token)
    }

    /// Verify the initialize proof and return the bearer's remaining lifetime.
    ///
    /// # Errors
    /// Missing, wrong, expired, or unreadable credentials fail closed.
    pub fn authenticate(&self, proof: Option<&AuthProof>) -> Result<Duration, ProtoError> {
        let Some(AuthProof::Bearer { token }) = proof else {
            return Err(ProtoError::new(ErrorCode::Unauthenticated, "daemon web bearer required"));
        };
        let registry = self.read().map_err(|_| ProtoError::new(ErrorCode::Unauthenticated, "daemon web bearer unavailable"))?;
        let sought = digest(token);
        let now = now_ms();
        let Some(record) =
            registry.tokens.iter().find(|record| constant_time_eq(record.digest.as_bytes(), sought.as_bytes()) && record.expires_ms > now)
        else {
            return Err(ProtoError::new(ErrorCode::Unauthenticated, "daemon web bearer invalid or expired"));
        };
        Ok(Duration::from_millis(record.expires_ms.saturating_sub(now)))
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
        file.lock()?;
        Ok(file)
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
        serde_json::from_slice(&fs::read(&self.path)?).map_err(io::Error::other)
    }

    fn write(&self, registry: &Registry) -> io::Result<()> {
        let mut file_name = self.path.clone();
        file_name.set_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file_name)?;
            serde_json::to_writer(&mut file, registry).map_err(io::Error::other)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&file_name, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            drop(fs::remove_file(file_name));
        }
        result
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for (a, b) in left.iter().zip(right) {
        diff |= usize::from(a ^ b);
    }
    diff == 0
}
