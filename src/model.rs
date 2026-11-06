//! Serializable data model: sessions, artifacts and the backup manifest.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current backup manifest schema version. v1 was the original Go format
/// (path derived from the encoded directory name); v2 stores the real `cwd`
/// extracted from the JSONL plus per-artifact checksums.
pub const SCHEMA_VERSION: u32 = 3;

/// A single file belonging to a session inside the archive, stored under
/// `sessions/<session-id>/<logical_name>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Kind of artifact: "session", "env" or "todos".
    pub kind: ArtifactKind,
    /// File name as it appears in the archive and on disk after restore.
    pub logical_name: String,
    /// SHA-256 of the file contents, hex encoded.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Session,
    Env,
    Todos,
    Project,
    Memory,
}

/// Metadata for one Claude Code session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// The real working directory, read from the JSONL `cwd` field. This is
    /// the source of truth, never derived from the encoded directory name.
    /// `directory` is accepted as an alias so legacy v1 (Go) manifests load.
    #[serde(alias = "directory")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub size_bytes: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_preview: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

impl Session {
    /// True when the real cwd could not be recovered from the JSONL.
    pub fn cwd_unknown(&self) -> bool {
        self.cwd.is_empty()
    }
}

/// Top-level archive manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub schema_version: u32,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_host: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_version: Option<String>,
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub providers: Vec<ProviderManifest>,
    #[serde(default)]
    pub extras: Vec<FileDigest>,
    #[serde(default)]
    pub config: Vec<FileDigest>,
    /// SHA-256 over the manifest-independent content (see `backup`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checksum: String,
    #[serde(default)]
    pub total_size: u64,
}

impl Manifest {
    pub fn is_v1(&self) -> bool {
        self.schema_version < 2
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDigest {
    pub rel: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderManifest {
    pub id: crate::provider::ProviderId,
    #[serde(default)]
    pub source_home: String,
    #[serde(default)]
    pub items: Vec<ArchivedItem>,
    #[serde(default)]
    pub files: Vec<FileDigest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedItem {
    pub id: String,
    pub kind: crate::provider::ItemKind,
    pub label: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub files: Vec<ArchivedFile>,
}

impl ArchivedItem {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedFile {
    #[serde(flatten)]
    pub file: crate::provider::ItemFile,
    #[serde(default)]
    pub sha256: String,
}

impl std::ops::Deref for ArchivedFile {
    type Target = crate::provider::ItemFile;
    fn deref(&self) -> &Self::Target {
        &self.file
    }
}
