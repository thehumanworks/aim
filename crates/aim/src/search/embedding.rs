//! Local, checksum-verified `Model2Vec` embeddings for conversation search.
//!
//! The model is pinned to a Hugging Face revision. Its model card declares MIT
//! and is verified alongside the three files needed for inference.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use model2vec_rs::model::StaticModel;
use sha2::{Digest, Sha256};

const REPO: &str = "minishlab/potion-retrieval-32M";
const REVISION: &str = "6fc8051fab2a1e0ee76689cf08c853792ac285e7";
const DIMENSIONS: usize = 512;
const MAX_FILE_BYTES: u64 = 130_000_000;

// SHA-256 of the resolved file bytes at REVISION, not the Git object IDs.
// Small-file hashes were computed from /resolve/REVISION/<name>; the weight
// hash also matches the official LFS oid at:
// https://huggingface.co/api/models/minishlab/potion-retrieval-32M/tree/6fc8051fab2a1e0ee76689cf08c853792ac285e7?recursive=true&expand=true
const FILES: [ModelFile; 4] = [
    ModelFile { name: "config.json", sha256: "63c00d90824c832c04ec1d02b6a983fb90489bf049f29fbff15ba481b8a432ee", size: 202 },
    ModelFile { name: "tokenizer.json", sha256: "7d75cbc54318138807c401b0f0c9721117c628b39de8e8e0edb6cb17e0ee7d18", size: 1_493_150 },
    ModelFile { name: "model.safetensors", sha256: "07609e5bd33aad37900b3fd62f4ec96f6daec88ca4d46b9d8b928bfababf6ea0", size: 129_210_456 },
    ModelFile { name: "README.md", sha256: "dd46f9828de776831d173b39b3dfb5ee9b8e5cb756daa3739afab1ab5d6542fd", size: 17_141 },
];

struct ModelFile {
    name: &'static str,
    sha256: &'static str,
    size: u64,
}

/// The pinned local embedding model. `home` is the aim data directory (`~/.aim` by default).
pub struct Embedder {
    model: StaticModel,
}

impl Embedder {
    /// Open the verified local model, downloading missing or corrupt files once.
    ///
    /// This performs blocking disk and network I/O; call it off the request path.
    ///
    /// # Errors
    /// Returns an error if the cache, download, checksum, license, or model load fails.
    pub fn open(home: &Path) -> Result<Self, String> {
        let models = home.join("models");
        fs::create_dir_all(&models).map_err(|e| format!("cannot create model cache: {e}"))?;
        let lock_path = models.join("potion-retrieval-32M.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(lock_path)
            .map_err(|e| format!("cannot open model cache lock: {e}"))?;
        lock.lock().map_err(|e| format!("cannot lock model cache: {e}"))?;

        let directory = model_directory(home);
        fs::create_dir_all(&directory).map_err(|e| format!("cannot create model directory: {e}"))?;
        for artifact in &FILES {
            let path = directory.join(artifact.name);
            if !verified(&path, artifact)? {
                let http = reqwest::blocking::Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .timeout(Duration::from_secs(600))
                    .build()
                    .map_err(|e| format!("cannot create model downloader: {e}"))?;
                download(&http, &directory, artifact)?;
            }
        }
        verify_license(&directory.join("README.md"))?;
        drop(lock);

        let model = StaticModel::from_pretrained(&directory, None, None, None).map_err(|e| format!("cannot load verified model: {e}"))?;
        if model.encode_single("dimension probe").len() != DIMENSIONS {
            return Err("verified model has unexpected embedding dimensions".to_owned());
        }
        Ok(Self { model })
    }

    /// Check whether all pinned artifacts are present at the expected sizes.
    ///
    /// This performs no network I/O or hashing. `open` verifies their contents
    /// before use, so a damaged cache may still require a repair download.
    #[must_use]
    pub fn cached(home: &Path) -> bool {
        let directory = model_directory(home);
        FILES.iter().all(|artifact| {
            fs::metadata(directory.join(artifact.name)).is_ok_and(|metadata| metadata.is_file() && metadata.len() == artifact.size)
        })
    }

    /// Encode one text with the model's default 512-token truncation and normalization.
    ///
    /// # Errors
    /// Returns an error if the model produces a vector with an unexpected size.
    pub fn encode(&self, text: &str) -> Result<Vec<f32>, String> {
        let vector = self.model.encode_single(text);
        if vector.len() != DIMENSIONS {
            return Err("model returned unexpected embedding dimensions".to_owned());
        }
        Ok(vector)
    }

    /// Return the dimensionality of the pinned model.
    #[must_use]
    pub fn dims(&self) -> usize {
        DIMENSIONS
    }
}

fn model_directory(home: &Path) -> PathBuf {
    home.join("models").join(format!("potion-retrieval-32M-{REVISION}"))
}

fn verified(path: &Path, artifact: &ModelFile) -> Result<bool, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot read cached {}: {e}", artifact.name)),
    };
    if file.metadata().map_err(|e| format!("cannot stat cached {}: {e}", artifact.name))?.len() != artifact.size {
        return Ok(false);
    }
    let digest = hash(file).map_err(|e| format!("cannot hash cached {}: {e}", artifact.name))?;
    Ok(digest == artifact.sha256)
}

fn hash(mut reader: impl Read) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(format!("{:x}", digest.finalize()));
        }
        let bytes = buffer.get(..count).ok_or_else(|| std::io::Error::other("read exceeded buffer"))?;
        digest.update(bytes);
    }
}

fn download(client: &reqwest::blocking::Client, directory: &Path, artifact: &ModelFile) -> Result<(), String> {
    let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{}", artifact.name);
    let mut response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| format!("cannot download {}: {e}", artifact.name))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(|e| format!("cannot stage {}: {e}", artifact.name))?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = response.read(&mut buffer).map_err(|e| format!("cannot read {} download: {e}", artifact.name))?;
        if count == 0 {
            break;
        }
        size = size.saturating_add(u64::try_from(count).map_err(|e| e.to_string())?);
        if size > artifact.size || size > MAX_FILE_BYTES {
            return Err(format!("{} download exceeded expected size", artifact.name));
        }
        let bytes = buffer.get(..count).ok_or("download read exceeded buffer")?;
        digest.update(bytes);
        temporary.write_all(bytes).map_err(|e| format!("cannot stage {}: {e}", artifact.name))?;
    }
    if size != artifact.size || format!("{:x}", digest.finalize()) != artifact.sha256 {
        return Err(format!("{} download failed checksum verification", artifact.name));
    }
    temporary.persist(directory.join(artifact.name)).map_err(|e| format!("cannot install {}: {e}", artifact.name))?;
    Ok(())
}

fn verify_license(path: &Path) -> Result<(), String> {
    let card = fs::read_to_string(path).map_err(|e| format!("cannot read model license: {e}"))?;
    let declared = card.lines().skip(1).take_while(|line| *line != "---").any(|line| line.trim() == "license: mit");
    if !card.starts_with("---\n") || !declared {
        return Err("verified model card does not declare an MIT license".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DIMENSIONS, Embedder, ModelFile, hash, verified, verify_license};
    use std::fs;

    #[test]
    fn checksum_detects_cache_corruption() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.json");
        fs::write(&path, b"hello")?;
        let artifact =
            ModelFile { name: "config.json", sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", size: 5 };
        assert_eq!(hash(b"hello".as_slice())?, artifact.sha256);
        assert!(verified(&path, &artifact)?);
        fs::write(&path, b"HELLO")?;
        assert!(!verified(&path, &artifact)?);
        Ok(())
    }

    #[test]
    fn license_requires_mit_frontmatter() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("README.md");
        fs::write(&path, "---\nlicense: mit\n---\n")?;
        assert!(verify_license(&path).is_ok());
        fs::write(&path, "---\nlicense: apache-2.0\n---\n")?;
        assert!(verify_license(&path).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "downloads the pinned public model on first run"]
    fn live_model_download_and_deterministic_encoding() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        assert!(!Embedder::cached(dir.path()));
        let embedder = Embedder::open(dir.path())?;
        assert!(Embedder::cached(dir.path()));
        let first = embedder.encode("past conversation search")?;
        assert_eq!(first.len(), DIMENSIONS);
        assert_eq!(first, embedder.encode("past conversation search")?);
        assert_eq!(embedder.dims(), DIMENSIONS);
        assert_eq!(Embedder::open(dir.path())?.encode("past conversation search")?, first);
        Ok(())
    }
}
