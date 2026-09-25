//! Private runner ownership records, written before Git or session side effects.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use aim_proto::daemon::Location;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Receipt {
    pub job_id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub token: String,
    pub worker: String,
    pub workspace: String,
    pub location: Location,
    pub branch: String,
    pub worktree: String,
    #[serde(default)]
    pub base_commit: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub turn_succeeded: bool,
    #[serde(default)]
    pub session_closed: bool,
}

pub(super) struct ReceiptStore {
    dir: PathBuf,
    _lock: File,
}

pub(super) fn owned_private_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.file_type().is_dir()
                && meta.uid() == nix::unistd::Uid::current().as_raw()
                && meta.permissions().mode().trailing_zeros() >= 6 =>
        {
            Ok(())
        }
        Ok(_) => Err(format!("{} is not an owned private directory", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path).map_err(|cause| format!("creating {}: {cause}", path.display()))
        }
        Err(err) => Err(format!("inspecting {}: {err}", path.display())),
    }
}

fn safe_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err("invalid worker or attempt id".into());
    }
    Ok(())
}

impl ReceiptStore {
    pub(super) fn open(home: &Path, worker: &str) -> Result<Self, String> {
        safe_id(worker)?;
        owned_private_dir(home)?;
        let run = home.join("run");
        owned_private_dir(&run)?;
        let dir = run.join("board-workers");
        owned_private_dir(&dir)?;
        let lock_path = dir.join(format!("{worker}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)
            .map_err(|err| err.to_string())?;
        lock.try_lock().map_err(|err| format!("worker {worker} is already running: {err}"))?;
        Ok(Self { dir, _lock: lock })
    }

    fn path(&self, attempt_id: &str) -> Result<PathBuf, String> {
        safe_id(attempt_id)?;
        Ok(self.dir.join(format!("{attempt_id}.json")))
    }

    pub(super) fn save(&self, receipt: &Receipt) -> Result<(), String> {
        let path = self.path(&receipt.attempt_id)?;
        let temp = self.dir.join(format!(".{}.tmp", Uuid::new_v4()));
        let bytes = serde_json::to_vec(receipt).map_err(|err| err.to_string())?;
        let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&temp).map_err(|err| err.to_string())?;
        file.write_all(&bytes).map_err(|err| err.to_string())?;
        file.sync_all().map_err(|err| err.to_string())?;
        fs::rename(&temp, path).map_err(|err| err.to_string())?;
        File::open(&self.dir).and_then(|dir| dir.sync_all()).map_err(|err| err.to_string())
    }

    pub(super) fn load(&self, attempt_id: &str) -> Result<Option<Receipt>, String> {
        let path = self.path(attempt_id)?;
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.to_string()),
        };
        if !meta.file_type().is_file() || meta.uid() != nix::unistd::Uid::current().as_raw() || meta.permissions().mode() & 0o077 != 0 {
            return Err("worker receipt is not an owned private file".into());
        }
        serde_json::from_slice(&fs::read(path).map_err(|err| err.to_string())?).map(Some).map_err(|err| err.to_string())
    }

    pub(super) fn remove(&self, attempt_id: &str) -> Result<(), String> {
        let path = self.path(attempt_id)?;
        fs::remove_file(path).map_err(|err| err.to_string())
    }
}
