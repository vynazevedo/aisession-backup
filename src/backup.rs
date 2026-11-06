//! Creating a portable `.tar.gz` backup with a v2 manifest and per-artifact
//! checksums.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::discover::DiscoveredSession;
use crate::model::{Artifact, ArtifactKind, Manifest, SCHEMA_VERSION};

/// Computes the SHA-256 of a file by streaming it, returning (hex, size).
pub(crate) fn hash_file(path: &Path) -> Result<(String, u64)> {
    crate::archive::ensure_regular(path)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Deterministic global checksum: SHA-256 over every artifact's
/// `kind:logical_name:sha256` line, sorted, so it is independent of session
/// ordering and covers all session content.
pub(crate) fn global_checksum(sessions: &[crate::model::Session]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for s in sessions {
        for a in &s.artifacts {
            lines.push(format!("{}/{}:{}", s.id, a.logical_name, a.sha256));
        }
    }
    lines.sort();
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Archive path for an artifact. Kinds are namespaced into subdirectories so
/// an env file and a todos file that happen to share a name
/// (`<id>-agent-<id>.json`) never collide inside the archive.
pub(crate) fn archive_path(kind: ArtifactKind, session_id: &str, name: &str) -> String {
    match kind {
        ArtifactKind::Session => format!("sessions/{session_id}/{name}"),
        ArtifactKind::Env => format!("sessions/{session_id}/env/{name}"),
        ArtifactKind::Todos => format!("sessions/{session_id}/todos/{name}"),
        ArtifactKind::Project => format!("sessions/{session_id}/project/{name}"),
        ArtifactKind::Memory => format!("sessions/{session_id}/memory/{name}"),
    }
}

fn logical_name(kind: ArtifactKind, path: &Path, session_id: &str) -> String {
    let marker = match kind {
        ArtifactKind::Session => return format!("{session_id}.jsonl"),
        ArtifactKind::Env => "session-env",
        ArtifactKind::Todos => "todos",
        ArtifactKind::Project => session_id,
        ArtifactKind::Memory => "memory",
    };
    for parent in path.ancestors().skip(1) {
        if parent.file_name().and_then(|s| s.to_str()) == Some(marker) {
            return path
                .strip_prefix(parent)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
        }
    }
    String::new()
}

/// Builds the manifest (with artifact checksums) and the flat list of files to
/// archive, without yet writing anything.
#[cfg(test)]
pub(crate) fn plan(selected: &[DiscoveredSession]) -> Result<(Manifest, Vec<(String, PathBuf)>)> {
    plan_with_hashes(selected, true)
}

pub(crate) fn plan_with_hashes(
    selected: &[DiscoveredSession],
    hashes: bool,
) -> Result<(Manifest, Vec<(String, PathBuf)>)> {
    let mut sessions = Vec::with_capacity(selected.len());
    // (archive path, on-disk path)
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut total_size: u64 = 0;
    let mut claude_version: Option<String> = None;

    for ds in selected {
        let mut session = ds.session.clone();
        session.artifacts.clear();
        if claude_version.is_none() {
            claude_version = session.claude_version.clone();
        }

        let mut all: Vec<(ArtifactKind, PathBuf)> =
            vec![(ArtifactKind::Session, ds.jsonl_path.clone())];
        all.extend(ds.companions.iter().cloned());

        for (kind, path) in all {
            let (sha, size) = inventory_digest(&path, hashes)?;
            let name = logical_name(kind, &path, &session.id);
            total_size += size;
            session.artifacts.push(Artifact {
                kind,
                logical_name: name.clone(),
                sha256: sha,
                size,
            });
            files.push((archive_path(kind, &session.id, &name), path));
        }
        sessions.push(session);
    }

    let checksum = global_checksum(&sessions);
    let hostname = hostname();

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        created_at: Utc::now(),
        source_host: hostname,
        source_os: std::env::consts::OS.to_string(),
        claude_version,
        sessions,
        providers: Vec::new(),
        extras: Vec::new(),
        config: Vec::new(),
        checksum,
        total_size,
    };

    Ok((manifest, files))
}

pub(crate) fn inventory_digest(path: &Path, hashes: bool) -> Result<(String, u64)> {
    if hashes {
        return hash_file(path);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok((String::new(), metadata.len())),
        Ok(_) => anyhow::bail!("source is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((String::new(), 0)),
        Err(error) => Err(error.into()),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default()
}

/// Global auxiliary directories archived under `extras/` when `--full` is set.
/// They are session-adjacent state (shell snapshots referenced by absolute
/// path in the JSONL, file-history undo data, plan-mode notes) that is not
/// tied to a single session id, so it is archived wholesale and verbatim.
pub const EXTRA_DIRS: &[&str] = &["shell-snapshots", "file-history", "plans"];

/// What to include in a backup beyond the selected sessions.
#[derive(Default, Clone, Copy)]
pub struct BackupOptions {
    /// Archive the global `EXTRA_DIRS` (shell-snapshots, file-history, plans).
    pub include_extras: bool,
    /// Archive the global config (settings, CLAUDE.md, plugins, MCP servers).
    pub include_config: bool,
    /// When archiving config, also include login credentials (sensitive).
    pub include_credentials: bool,
}

pub(crate) fn collect_additional_with_hashes(
    claude_dir: &Path,
    opts: BackupOptions,
    manifest: &mut Manifest,
    files: &mut Vec<(String, PathBuf)>,
    staging: &Path,
    hashes: bool,
) -> Result<()> {
    if opts.include_extras {
        for dir in EXTRA_DIRS {
            let root = claude_dir.join(dir);
            if !root.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(root).follow_root_links(false) {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(claude_dir)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let (sha256, size) = inventory_digest(entry.path(), hashes)?;
                manifest.extras.push(crate::model::FileDigest {
                    rel: rel.clone(),
                    sha256,
                    size,
                });
                files.push((format!("extras/{rel}"), entry.into_path()));
            }
        }
    }
    if opts.include_config {
        for (rel, bytes) in crate::config::collect(claude_dir, opts.include_credentials).files {
            let name = format!("config/{rel}");
            crate::safety::atomic_write(
                staging,
                &name,
                &bytes,
                crate::safety::WriteMode::CreateNew,
            )?;
            manifest.config.push(crate::model::FileDigest {
                rel,
                sha256: hex::encode(Sha256::digest(&bytes)),
                size: bytes.len() as u64,
            });
            files.push((name.clone(), staging.join(name)));
        }
    }
    Ok(())
}

/// Reports whether a config backup of `claude_dir` would contain secrets, so
/// the CLI can warn before writing the archive.
pub fn config_has_secrets(claude_dir: &Path, include_credentials: bool) -> bool {
    crate::config::collect(claude_dir, include_credentials).has_secrets
}

pub fn manifest_self_consistent(manifest: &Manifest) -> bool {
    if manifest.checksum.is_empty() {
        return false;
    }
    global_checksum(&manifest.sessions) == manifest.checksum
}

/// Pretty-prints a manifest summary; shared by `list`.
pub fn summarize(manifest: &Manifest) -> String {
    let mut out = String::new();
    out.push_str(&format!("Schema:   v{}\n", manifest.schema_version));
    out.push_str(&format!(
        "Created:  {}\n",
        manifest.created_at.format("%Y-%m-%d %H:%M:%S")
    ));
    out.push_str(&format!(
        "Source:   {} ({})\n",
        crate::archive::report_text(&manifest.source_host),
        crate::archive::report_text(&manifest.source_os)
    ));
    if let Some(v) = &manifest.claude_version {
        out.push_str(&format!("Claude:   {}\n", crate::archive::report_text(v)));
    }
    if !manifest.checksum.is_empty() {
        out.push_str(&format!(
            "Checksum: {}...\n",
            crate::archive::report_text(&manifest.checksum.chars().take(16).collect::<String>())
        ));
    }
    out.push_str(&format!("Sessions: {}\n", manifest.sessions.len()));
    out
}

/// Convenience to collect just the directories of selected sessions, used by
/// callers that build mappings.
pub fn session_dirs(sessions: &[crate::model::Session]) -> BTreeMap<String, ()> {
    sessions.iter().map(|s| (s.cwd.clone(), ())).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn summary_escapes_manifest_controls_and_accepts_unicode_checksum() {
        let mut manifest = super::plan(&[]).unwrap().0;
        manifest.source_host = "host\u{1b}[31m".into();
        manifest.source_os = "os\rspoof".into();
        manifest.claude_version = Some("version\u{7}".into());
        manifest.checksum = "漢".repeat(6);
        let summary = super::summarize(&manifest);
        assert!(!summary.chars().any(|c| c.is_control() && c != '\n'));
        assert!(summary.contains("\\u{1b}"));
    }
}
