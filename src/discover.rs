//! Session discovery driven by the JSONL contents.
//!
//! The authoritative `cwd` of a session lives in the JSONL, not in the
//! encoded directory name (which is lossy). We therefore read each
//! `projects/**/*.jsonl`, extract the first real `cwd`, and correlate the
//! companion `session-env` and `todos` files by session-id prefix.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::model::{ArtifactKind, Session};

/// Line `type` values that are control records, not conversation messages.
const CONTROL_TYPES: &[&str] = &[
    "mode",
    "permission-mode",
    "file-history-snapshot",
    "summary",
];

#[derive(Default)]
struct Meta {
    cwd: Option<String>,
    git_branch: Option<String>,
    version: Option<String>,
    model: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    message_count: usize,
    first_preview: String,
    last_preview: String,
}

fn truncate_preview(text: &str) -> String {
    let trimmed = text.trim();
    let mut out = String::new();
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= 57 {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

/// Pulls human-readable text out of a message `content` field, which may be a
/// plain string or an array of typed blocks.
fn extract_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        return Some(t.to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn read_meta(path: &Path) -> Meta {
    let mut meta = Meta::default();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return meta,
    };
    let reader = BufReader::new(file);

    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if meta.cwd.is_none() {
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                if !cwd.is_empty() {
                    meta.cwd = Some(cwd.to_string());
                }
            }
        }
        if meta.git_branch.is_none() {
            if let Some(b) = value.get("gitBranch").and_then(Value::as_str) {
                if !b.is_empty() {
                    meta.git_branch = Some(b.to_string());
                }
            }
        }
        if meta.version.is_none() {
            if let Some(v) = value.get("version").and_then(Value::as_str) {
                if !v.is_empty() {
                    meta.version = Some(v.to_string());
                }
            }
        }

        let line_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if CONTROL_TYPES.contains(&line_type) {
            continue;
        }
        if line_type != "user" && line_type != "assistant" {
            continue;
        }

        meta.message_count += 1;

        if let Some(ts) = value.get("timestamp").and_then(Value::as_str) {
            if let Ok(parsed) = DateTime::parse_from_rfc3339(ts) {
                meta.timestamp = Some(parsed.with_timezone(&Utc));
            }
        }

        let message = match value.get("message") {
            Some(m) => m,
            None => continue,
        };
        if meta.model.is_none() {
            if let Some(m) = message.get("model").and_then(Value::as_str) {
                meta.model = Some(m.to_string());
            }
        }
        if line_type == "user" {
            if let Some(content) = message.get("content") {
                if let Some(text) = extract_text(content) {
                    if !text.trim().is_empty() {
                        let preview = truncate_preview(&text);
                        if meta.first_preview.is_empty() {
                            meta.first_preview = preview.clone();
                        }
                        meta.last_preview = preview;
                    }
                }
            }
        }
    }

    meta
}

fn file_mtime_utc(path: &Path) -> DateTime<Utc> {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    match modified {
        Some(d) => Utc
            .timestamp_opt(d.as_secs() as i64, d.subsec_nanos())
            .single()
            .unwrap_or_else(Utc::now),
        None => Utc::now(),
    }
}

/// Glob companion files for a session id by prefix. This is what makes
/// sub-agent artifacts (`<id>-agent-<id>.json`) get picked up; the Go tool
/// looked for an exact `<id>.json` and silently missed all of them.
fn glob_by_prefix(dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") {
            continue;
        }
        // Match `<id>.json` and `<id>-agent-*.json`, but not a different id
        // that merely has this id as a prefix of a longer uuid segment.
        let stem = &name[..name.len() - ".json".len()];
        if stem == session_id || stem.starts_with(&format!("{session_id}-")) {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

/// Collects all artifacts (env, todos) for a session present on disk. The
/// session JSONL itself is added by the caller, which already holds its path.
pub fn collect_companion_artifacts(
    claude_dir: &Path,
    session_id: &str,
) -> Vec<(ArtifactKind, PathBuf)> {
    let mut out = Vec::new();
    for path in glob_by_prefix(&claude_dir.join("session-env"), session_id) {
        out.push((ArtifactKind::Env, path));
    }
    for path in glob_by_prefix(&claude_dir.join("todos"), session_id) {
        out.push((ArtifactKind::Todos, path));
    }
    out
}

/// A discovered session together with the on-disk paths of its files.
#[derive(Clone)]
pub struct DiscoveredSession {
    pub session: Session,
    pub jsonl_path: PathBuf,
    /// (kind, on-disk path) for every companion file.
    pub companions: Vec<(ArtifactKind, PathBuf)>,
}

/// Scans `<claude_dir>/projects/**/*.jsonl` and returns one discovered
/// session per JSONL file.
pub fn load_sessions(claude_dir: &Path) -> Result<Vec<DiscoveredSession>> {
    let projects = claude_dir.join("projects");
    let mut discovered = Vec::new();

    if !projects.is_dir() {
        return Ok(discovered);
    }

    for entry in walkdir::WalkDir::new(&projects)
        .follow_root_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if entry.depth() != 2
            || !entry.file_type().is_file()
            || path.extension().and_then(|e| e.to_str()) != Some("jsonl")
        {
            continue;
        }

        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        crate::archive::ensure_regular(path)?;
        let meta = read_meta(path);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let timestamp = meta.timestamp.unwrap_or_else(|| file_mtime_utc(path));

        let session = Session {
            id: session_id.clone(),
            cwd: meta.cwd.unwrap_or_default(),
            git_branch: meta.git_branch,
            claude_version: meta.version,
            model: meta.model,
            message_count: meta.message_count,
            size_bytes: size,
            timestamp,
            preview: meta.first_preview,
            last_preview: meta.last_preview,
            artifacts: Vec::new(),
        };

        crate::safety::validate_id(&session_id)?;
        let mut companions = collect_companion_artifacts(claude_dir, &session_id);
        for (kind, root) in [
            (
                ArtifactKind::Env,
                claude_dir.join("session-env").join(&session_id),
            ),
            (
                ArtifactKind::Project,
                path.parent().unwrap().join(&session_id),
            ),
            (ArtifactKind::Memory, path.parent().unwrap().join("memory")),
        ] {
            if !root.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root).follow_root_links(false) {
                let entry = entry?;
                if entry.file_type().is_file() {
                    companions.push((kind, entry.into_path()));
                }
            }
        }

        discovered.push(DiscoveredSession {
            session,
            jsonl_path: path.to_path_buf(),
            companions,
        });
    }

    discovered.sort_by_key(|d| std::cmp::Reverse(d.session.timestamp));
    Ok(discovered)
}
