//! Creating a portable `.tar.gz` backup with a v2 manifest and per-artifact
//! checksums.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use crate::discover::DiscoveredSession;
use crate::model::{Artifact, ArtifactKind, Manifest, SCHEMA_VERSION};

/// Computes the SHA-256 of a file by streaming it, returning (hex, size).
fn hash_file(path: &Path) -> Result<(String, u64)> {
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
fn global_checksum(sessions: &[crate::model::Session]) -> String {
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
fn archive_path(kind: ArtifactKind, session_id: &str, name: &str) -> String {
    match kind {
        ArtifactKind::Session => format!("sessions/{session_id}/{name}"),
        ArtifactKind::Env => format!("sessions/{session_id}/env/{name}"),
        ArtifactKind::Todos => format!("sessions/{session_id}/todos/{name}"),
    }
}

fn logical_name(kind: ArtifactKind, path: &Path, session_id: &str) -> String {
    match kind {
        ArtifactKind::Session => format!("{session_id}.jsonl"),
        ArtifactKind::Env | ArtifactKind::Todos => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Builds the manifest (with artifact checksums) and the flat list of files to
/// archive, without yet writing anything.
fn plan(selected: &[DiscoveredSession]) -> Result<(Manifest, Vec<(String, PathBuf)>)> {
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
            if !path.is_file() {
                continue;
            }
            let (sha, size) = hash_file(&path)?;
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
        checksum,
        total_size,
    };

    Ok((manifest, files))
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default()
}

fn write_entry<W: Write>(builder: &mut tar::Builder<W>, name: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(Utc::now().timestamp() as u64);
    header.set_cksum();
    builder.append_data(&mut header, name, data)?;
    Ok(())
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

/// Creates the archive at `output` for the selected sessions, plus whatever
/// `opts` requests. Returns the manifest and whether secret-bearing config was
/// included (so the caller can warn).
pub fn create(
    selected: &[DiscoveredSession],
    output: &Path,
    claude_dir: &Path,
    opts: BackupOptions,
) -> Result<Manifest> {
    let (manifest, files) = plan(selected)?;

    let out_file =
        File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let encoder = GzEncoder::new(out_file, Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let manifest_json = serde_json::to_vec_pretty(&manifest)?;
    write_entry(&mut builder, "manifest.json", &manifest_json)?;

    for (archive_name, disk_path) in &files {
        let mut header = tar::Header::new_gnu();
        let meta = std::fs::metadata(disk_path)?;
        header.set_size(meta.len());
        header.set_mode(0o644);
        header.set_mtime(Utc::now().timestamp() as u64);
        header.set_cksum();
        let file = File::open(disk_path)?;
        builder.append_data(&mut header, archive_name, file)?;
    }

    if opts.include_extras {
        archive_extras(&mut builder, claude_dir)?;
    }

    if opts.include_config {
        let bundle = crate::config::collect(claude_dir, opts.include_credentials);
        for (name, bytes) in &bundle.files {
            write_entry(&mut builder, &format!("config/{name}"), bytes)?;
        }
    }

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(manifest)
}

/// Reports whether a config backup of `claude_dir` would contain secrets, so
/// the CLI can warn before writing the archive.
pub fn config_has_secrets(claude_dir: &Path, include_credentials: bool) -> bool {
    crate::config::collect(claude_dir, include_credentials).has_secrets
}

/// Walks each `EXTRA_DIRS` entry and appends its files under `extras/<dir>/...`.
fn archive_extras<W: Write>(builder: &mut tar::Builder<W>, claude_dir: &Path) -> Result<()> {
    for dir in EXTRA_DIRS {
        let base = claude_dir.join(dir);
        if !base.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(claude_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let archive_name = format!("extras/{}", rel.to_string_lossy().replace('\\', "/"));
            let mut header = tar::Header::new_gnu();
            let meta = std::fs::metadata(path)?;
            header.set_size(meta.len());
            header.set_mode(0o644);
            header.set_mtime(Utc::now().timestamp() as u64);
            header.set_cksum();
            let file = File::open(path)?;
            builder.append_data(&mut header, &archive_name, file)?;
        }
    }
    Ok(())
}

/// Verifies that a manifest's recorded global checksum matches its artifacts.
/// (The artifact list is part of the manifest, so this guards against a
/// tampered or truncated manifest, while restore re-hashes the actual bytes.)
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
        manifest.source_host, manifest.source_os
    ));
    if let Some(v) = &manifest.claude_version {
        out.push_str(&format!("Claude:   {v}\n"));
    }
    if !manifest.checksum.is_empty() {
        out.push_str(&format!(
            "Checksum: {}...\n",
            &manifest.checksum[..16.min(manifest.checksum.len())]
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
