pub mod backend;
mod chunk;
mod crypto;

pub use backend::{Backend, LocalBackend};
pub use chunk::{AVG_CHUNK_SIZE, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u32 = 1;
const DOCUMENT_LIMIT: usize = 64 * 1024 * 1024;
const OBJECT_LIMIT: usize = MAX_CHUNK_SIZE + 65536;
const LOCK: &str = "writer.lock";

#[derive(Serialize, Deserialize)]
struct Config {
    format_version: u32,
    chunker: String,
    min_chunk_size: usize,
    avg_chunk_size: usize,
    max_chunk_size: usize,
    encryption: Option<EncryptionConfig>,
}

#[derive(Serialize, Deserialize)]
struct EncryptionConfig {
    recipient: String,
    encrypted_identity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub new_chunks: usize,
    pub reused_chunks: usize,
    pub new_bytes: u64,
    pub total_bytes: u64,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub host: String,
    pub created_at: DateTime<Utc>,
    pub files: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub snapshots_removed: Vec<String>,
    pub chunks_removed: Vec<String>,
    pub bytes_removed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckReport {
    pub snapshots: usize,
    pub chunks: usize,
    pub missing: Vec<String>,
    pub corrupt: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedDocument {
    body: String,
    mac: String,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    format_version: u32,
    id: String,
    manifest: crate::model::Manifest,
    host: String,
    created_at: DateTime<Utc>,
    files: BTreeMap<String, SnapshotFile>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotFile {
    size: u64,
    sha256: String,
    chunks: Vec<String>,
}

pub struct Repository {
    backend: Box<dyn Backend>,
    encryption: Option<crypto::Encryption>,
}

struct WriterLock<'a> {
    backend: &'a dyn Backend,
    held: bool,
}

impl<'a> WriterLock<'a> {
    fn acquire(backend: &'a dyn Backend) -> Result<Self> {
        if !backend.put_if_absent(LOCK, Utc::now().to_rfc3339().as_bytes())? {
            bail!("repository writer lock exists; another writer or prune is active")
        }
        Ok(Self {
            backend,
            held: true,
        })
    }
    fn release(mut self) -> Result<()> {
        self.backend.delete(LOCK)?;
        self.held = false;
        Ok(())
    }
}

impl Drop for WriterLock<'_> {
    fn drop(&mut self) {
        if self.held {
            let _ = self.backend.delete(LOCK);
        }
    }
}

fn valid_digest(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn canonical_snapshot_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 42
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..24].iter().all(u8::is_ascii_digit)
        && bytes[24] == b'Z'
        && bytes[25] == b'-'
        && bytes[26..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

fn snapshot_key(id: &str) -> Result<String> {
    if !canonical_snapshot_id(id) {
        bail!("invalid snapshot id: {}", crate::ui::sanitize(id))
    }
    Ok(format!("snapshots/{id}.json"))
}

fn chunk_key(id: &str) -> Result<String> {
    if !valid_digest(id) {
        bail!("invalid chunk id: {id:?}")
    }
    Ok(format!("data/{}/{id}", &id[..2]))
}

fn validate_paths<'a>(paths: impl IntoIterator<Item = &'a String>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        backend::validate_key(path)?;
        if !seen.insert(path.as_str()) {
            bail!("duplicate archive path: {path:?}")
        }
    }
    for path in &seen {
        for (pos, _) in path.match_indices('/') {
            if seen.contains(&path[..pos]) {
                bail!("conflicting archive paths: {path:?}")
            }
        }
    }
    Ok(())
}

impl Snapshot {
    fn validate(&self, id: &str) -> Result<()> {
        if self.format_version != FORMAT_VERSION || self.id != id {
            bail!("snapshot version or id mismatch")
        }
        if !id.starts_with(&self.created_at.format("%Y%m%dT%H%M%S%9fZ-").to_string()) {
            bail!("snapshot creation time does not match its id")
        }
        validate_paths(self.files.keys())?;
        for file in self.files.values() {
            if !valid_digest(&file.sha256) {
                bail!("invalid whole-file digest")
            }
            if (file.size == 0) != file.chunks.is_empty() {
                bail!("invalid file size or chunk count")
            }
            if file.size > (file.chunks.len() as u64).saturating_mul(MAX_CHUNK_SIZE as u64) {
                bail!("file size exceeds chunk capacity")
            }
            for id in &file.chunks {
                chunk_key(id)?;
            }
        }
        Ok(())
    }
    fn info(&self) -> Result<SnapshotInfo> {
        let total_bytes = self.files.values().try_fold(0u64, |sum, file| {
            sum.checked_add(file.size).context("snapshot size overflow")
        })?;
        Ok(SnapshotInfo {
            id: self.id.clone(),
            host: self.host.clone(),
            created_at: self.created_at,
            files: self.files.len(),
            total_bytes,
        })
    }
}

pub fn requires_passphrase(backend: &dyn Backend) -> Result<bool> {
    let bytes = backend
        .get("config.json")?
        .context("repository config.json is missing")?;
    if bytes.len() > 65536 {
        bail!("repository config is too large")
    }
    let config: Config = serde_json::from_slice(&bytes).context("invalid repository config")?;
    Ok(config.encryption.is_some())
}

impl Repository {
    pub fn init(backend: Box<dyn Backend>, passphrase: Option<&str>) -> Result<Self> {
        if !backend.list("")?.is_empty() {
            bail!("repository already exists or backend is not empty")
        }
        let encryption = passphrase.map(|_| crypto::Encryption::generate());
        let encrypted_config = match (&encryption, passphrase) {
            (Some(enc), Some(password)) => Some(EncryptionConfig {
                recipient: enc.recipient(),
                encrypted_identity: enc.seal_identity(password)?,
            }),
            _ => None,
        };
        let config = Config {
            format_version: FORMAT_VERSION,
            chunker: "gear-v1".into(),
            min_chunk_size: MIN_CHUNK_SIZE,
            avg_chunk_size: AVG_CHUNK_SIZE,
            max_chunk_size: MAX_CHUNK_SIZE,
            encryption: encrypted_config,
        };
        if !backend.put_if_absent("config.json", &serde_json::to_vec_pretty(&config)?)? {
            bail!("repository already exists")
        }
        Ok(Self {
            backend,
            encryption,
        })
    }

    pub fn open(backend: Box<dyn Backend>, passphrase: Option<&str>) -> Result<Self> {
        let bytes = backend
            .get("config.json")?
            .context("repository config.json is missing")?;
        if bytes.len() > 65536 {
            bail!("repository config is too large")
        }
        let config: Config = serde_json::from_slice(&bytes).context("invalid repository config")?;
        if config.format_version != FORMAT_VERSION
            || config.chunker != "gear-v1"
            || config.min_chunk_size != MIN_CHUNK_SIZE
            || config.avg_chunk_size != AVG_CHUNK_SIZE
            || config.max_chunk_size != MAX_CHUNK_SIZE
        {
            bail!("unsupported repository format or chunker")
        }
        let encryption = match config.encryption {
            Some(enc) => Some(crypto::Encryption::unlock(
                &enc.encrypted_identity,
                &enc.recipient,
                passphrase.context("encrypted repository requires a passphrase")?,
            )?),
            None => None,
        };
        Ok(Self {
            backend,
            encryption,
        })
    }

    fn id(&self, bytes: &[u8]) -> String {
        match &self.encryption {
            Some(enc) => enc.id(bytes),
            None => hex::encode(Sha256::digest(bytes)),
        }
    }

    fn encode(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        match &self.encryption {
            Some(enc) => enc.encrypt(bytes),
            None => Ok(bytes.to_vec()),
        }
    }

    fn decode(&self, bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
        if bytes.len() > limit + 65536 {
            bail!("repository object exceeds size limit")
        }
        match &self.encryption {
            Some(enc) => enc.decrypt(bytes, limit),
            None if bytes.len() <= limit => Ok(bytes.to_vec()),
            None => bail!("repository object exceeds size limit"),
        }
    }

    fn read_chunk(&self, id: &str) -> Result<Vec<u8>> {
        let bytes = self
            .backend
            .get(&chunk_key(id)?)?
            .with_context(|| format!("missing chunk {id}"))?;
        let compressed = self.decode(&bytes, OBJECT_LIMIT)?;
        let mut decoder = DeflateDecoder::new(compressed.as_slice());
        let mut plaintext = Vec::new();
        (&mut decoder)
            .take(MAX_CHUNK_SIZE as u64 + 1)
            .read_to_end(&mut plaintext)
            .with_context(|| format!("corrupt deflate chunk {id}"))?;
        if plaintext.is_empty()
            || plaintext.len() > MAX_CHUNK_SIZE
            || decoder.total_in() != compressed.len() as u64
            || self.id(&plaintext) != id
        {
            bail!("chunk digest, length or compression mismatch: {id}")
        }
        Ok(plaintext)
    }

    fn seal_document(&self, document: &[u8]) -> Result<Vec<u8>> {
        match &self.encryption {
            Some(enc) => Ok(serde_json::to_vec(&SealedDocument {
                body: String::from_utf8(document.to_vec())?,
                mac: enc.mac(document),
            })?),
            None => Ok(document.to_vec()),
        }
    }

    fn open_document(&self, plaintext: Vec<u8>) -> Result<Vec<u8>> {
        let Some(enc) = &self.encryption else {
            return Ok(plaintext);
        };
        let sealed: SealedDocument =
            serde_json::from_slice(&plaintext).context("snapshot is not authenticated")?;
        if !enc.verify_mac(sealed.body.as_bytes(), &sealed.mac) {
            bail!("snapshot authentication failed; it was not written with this repository key")
        }
        Ok(sealed.body.into_bytes())
    }

    fn read_snapshot(&self, id: &str) -> Result<Snapshot> {
        let bytes = self
            .backend
            .get(&snapshot_key(id)?)?
            .with_context(|| format!("missing snapshot {id}"))?;
        let document = self.open_document(self.decode(&bytes, DOCUMENT_LIMIT)?)?;
        let snapshot: Snapshot =
            serde_json::from_slice(&document).context("invalid snapshot document")?;
        snapshot.validate(id)?;
        Ok(snapshot)
    }

    fn snapshot_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for key in self.backend.list("snapshots/")? {
            let id = key
                .strip_prefix("snapshots/")
                .and_then(|s| s.strip_suffix(".json"))
                .context("invalid snapshot object name")?;
            snapshot_key(id)?;
            ids.push(id.to_owned());
        }
        ids.sort_by(|a, b| b.cmp(a));
        Ok(ids)
    }

    pub fn write_snapshot(
        &self,
        manifest: &crate::model::Manifest,
        files: &[(String, PathBuf)],
        host: &str,
    ) -> Result<SnapshotSummary> {
        validate_paths(files.iter().map(|(rel, _)| rel))?;
        let lock = WriterLock::acquire(self.backend.as_ref())?;
        let created_at = Utc::now();
        let nonce = crypto::Encryption::generate().recipient();
        let id = format!(
            "{}-{}",
            created_at.format("%Y%m%dT%H%M%S%9fZ"),
            &hex::encode(Sha256::digest(nonce.as_bytes()))[..16]
        );
        let mut summary = SnapshotSummary {
            snapshot_id: id.clone(),
            new_chunks: 0,
            reused_chunks: 0,
            new_bytes: 0,
            total_bytes: 0,
            files: files.len(),
        };
        let mut entries = BTreeMap::new();
        let mut checked = BTreeSet::new();
        let mut reused = BTreeSet::new();
        for (rel, source) in files {
            let metadata = fs::symlink_metadata(source)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("snapshot source is not a regular file: {rel:?}")
            }
            let mut reader = BufReader::new(File::open(source)?);
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut chunks = Vec::new();
            while let Some(bytes) = chunk::next_chunk(&mut reader)? {
                hash.update(&bytes);
                size = size
                    .checked_add(bytes.len() as u64)
                    .context("file size overflow")?;
                let chunk_id = self.id(&bytes);
                let key = chunk_key(&chunk_id)?;
                if !checked.insert(chunk_id.clone()) {
                    reused.insert(chunk_id.clone());
                } else if self.backend.exists(&key)? {
                    self.read_chunk(&chunk_id)?;
                    reused.insert(chunk_id.clone());
                } else {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
                    encoder.write_all(&bytes)?;
                    let stored = self.encode(&encoder.finish()?)?;
                    self.backend.put(&key, &stored)?;
                    summary.new_chunks += 1;
                    summary.new_bytes += stored.len() as u64;
                }
                chunks.push(chunk_id);
            }
            summary.total_bytes = summary
                .total_bytes
                .checked_add(size)
                .context("snapshot size overflow")?;
            entries.insert(
                rel.clone(),
                SnapshotFile {
                    size,
                    sha256: hex::encode(hash.finalize()),
                    chunks,
                },
            );
        }
        summary.reused_chunks = reused.len();
        let snapshot = Snapshot {
            format_version: FORMAT_VERSION,
            id: id.clone(),
            manifest: manifest.clone(),
            host: host.to_owned(),
            created_at,
            files: entries,
        };
        snapshot.validate(&id)?;
        let document = serde_json::to_vec(&snapshot)?;
        if document.len() > DOCUMENT_LIMIT {
            bail!("snapshot document exceeds size limit")
        }
        if !self.backend.put_if_absent(
            &snapshot_key(&id)?,
            &self.encode(&self.seal_document(&document)?)?,
        )? {
            bail!("snapshot id collision")
        }
        lock.release()?;
        Ok(summary)
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        self.snapshot_ids()?
            .iter()
            .map(|id| self.read_snapshot(id)?.info())
            .collect()
    }

    pub fn stage_snapshot(
        &self,
        id: &str,
        staging: &Path,
    ) -> Result<(crate::model::Manifest, BTreeMap<String, PathBuf>)> {
        let id = if id == "latest" {
            self.snapshot_ids()?
                .into_iter()
                .next()
                .context("repository has no snapshots")?
        } else {
            id.to_owned()
        };
        let snapshot = self.read_snapshot(&id)?;
        backend::private_dirs(staging)?;
        for rel in snapshot.files.keys() {
            let dest = backend::joined(staging, rel)?;
            if dest.exists() {
                bail!("staging destination already exists: {rel:?}")
            }
        }
        let temp = tempfile::Builder::new()
            .prefix(".repo-stage-")
            .tempdir_in(staging)?;
        let mut result = BTreeMap::new();
        for (rel, entry) in &snapshot.files {
            let path = backend::joined(temp.path(), rel)?;
            let parent = path.parent().context("staged file has no parent")?;
            backend::private_dirs(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            let mut hash = Sha256::new();
            let mut size = 0u64;
            for chunk_id in &entry.chunks {
                let bytes = self.read_chunk(chunk_id)?;
                size = size
                    .checked_add(bytes.len() as u64)
                    .context("file size overflow")?;
                if size > entry.size {
                    bail!("file size mismatch: {rel:?}")
                }
                hash.update(&bytes);
                file.write_all(&bytes)?;
            }
            if size != entry.size || hex::encode(hash.finalize()) != entry.sha256 {
                bail!("whole-file digest or size mismatch: {rel:?}")
            }
            file.as_file().sync_all()?;
            file.persist_noclobber(path).map_err(|e| e.error)?;
        }
        for rel in snapshot.files.keys() {
            let dest = backend::joined(staging, rel)?;
            let parent = dest.parent().context("staged file has no parent")?;
            backend::private_dirs(parent)?;
            backend::joined(staging, rel)?;
            let source = temp.path().join(rel);
            fs::hard_link(&source, &dest)?;
            result.insert(rel.clone(), dest);
        }
        Ok((snapshot.manifest, result))
    }

    pub fn prune(&self, keep_last: usize, dry_run: bool) -> Result<PruneReport> {
        let lock = if dry_run {
            if self.backend.exists(LOCK)? {
                bail!("repository writer lock exists")
            }
            None
        } else {
            Some(WriterLock::acquire(self.backend.as_ref())?)
        };
        let ids = self.snapshot_ids()?;
        let mut referenced = BTreeSet::new();
        let mut report = PruneReport {
            dry_run,
            snapshots_removed: Vec::new(),
            chunks_removed: Vec::new(),
            bytes_removed: 0,
        };
        for (index, id) in ids.iter().enumerate() {
            let snapshot = self.read_snapshot(id)?;
            if index < keep_last {
                for file in snapshot.files.values() {
                    for id in &file.chunks {
                        referenced.insert(chunk_key(id)?);
                    }
                }
            } else {
                report.snapshots_removed.push(id.clone());
            }
        }
        for key in self.backend.list("data/")? {
            let id = key
                .rsplit('/')
                .next()
                .context("invalid chunk object name")?;
            if chunk_key(id)? != key {
                bail!("invalid chunk object name: {key:?}")
            }
            if !referenced.contains(&key) {
                report.bytes_removed += self
                    .backend
                    .get(&key)?
                    .context("chunk vanished during prune")?
                    .len() as u64;
                report.chunks_removed.push(key);
            }
        }
        if !dry_run {
            for id in &report.snapshots_removed {
                self.backend.delete(&snapshot_key(id)?)?;
            }
            for key in &report.chunks_removed {
                self.backend.delete(key)?;
            }
        }
        if let Some(lock) = lock {
            lock.release()?;
        }
        Ok(report)
    }

    pub fn check(&self, read_data: bool) -> Result<CheckReport> {
        let mut report = CheckReport::default();
        let mut references = BTreeSet::new();
        let mut snapshots = Vec::new();
        for key in self.backend.list("snapshots/")? {
            report.snapshots += 1;
            let id = key
                .strip_prefix("snapshots/")
                .and_then(|s| s.strip_suffix(".json"));
            match id
                .context("invalid snapshot object name")
                .and_then(|id| self.read_snapshot(id))
            {
                Ok(snapshot) => {
                    for file in snapshot.files.values() {
                        references.extend(file.chunks.iter().cloned());
                    }
                    snapshots.push(snapshot);
                }
                Err(error) => report.corrupt.push(format!("{key:?}: {error:#}")),
            }
        }
        let keys: BTreeSet<_> = self.backend.list("data/")?.into_iter().collect();
        report.chunks = keys.len();
        for id in &references {
            let key = chunk_key(id)?;
            if !keys.contains(&key) {
                report.missing.push(key);
            }
        }
        if read_data {
            let mut bad = BTreeSet::new();
            for key in &keys {
                let id = key.rsplit('/').next().unwrap_or_default();
                let checked = chunk_key(id).and_then(|expected| {
                    if expected != *key {
                        bail!("invalid chunk object name")
                    }
                    self.read_chunk(id).map(|_| ())
                });
                if let Err(error) = checked {
                    bad.insert(id.to_owned());
                    report.corrupt.push(format!("{key:?}: {error:#}"));
                }
            }
            for snapshot in snapshots {
                for (rel, file) in snapshot.files {
                    if file.chunks.iter().any(|id| {
                        bad.contains(id) || !keys.contains(&chunk_key(id).unwrap_or_default())
                    }) {
                        continue;
                    }
                    let mut hash = Sha256::new();
                    let mut size = 0u64;
                    for id in file.chunks {
                        let bytes = self.read_chunk(&id)?;
                        size += bytes.len() as u64;
                        hash.update(bytes);
                    }
                    if size != file.size || hex::encode(hash.finalize()) != file.sha256 {
                        report.corrupt.push(format!(
                            "snapshot {:?}, file {rel:?}: whole-file digest or size mismatch",
                            snapshot.id
                        ));
                    }
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
