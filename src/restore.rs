//! Restoring sessions from a backup, with structured path remapping, real
//! checksum verification and case-collision detection.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::claudepath;
use crate::model::Manifest;
use crate::remap;

/// Files belonging to one session inside the archive, separated by kind so an
/// env file and a todos file with the same name never clash.
#[derive(Default)]
pub struct SessionFiles {
    pub session_jsonl: Option<Vec<u8>>,
    pub env: Vec<(String, Vec<u8>)>,
    pub todos: Vec<(String, Vec<u8>)>,
}

/// In-memory view of a loaded archive: the manifest, per-session files, and
/// any global `extras/` (relative path under `~/.claude`, bytes).
pub struct LoadedBackup {
    pub manifest: Manifest,
    pub data: BTreeMap<String, SessionFiles>,
    pub extras: Vec<(String, Vec<u8>)>,
}

fn is_path_safe(base: &Path, target: &Path) -> bool {
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    // target may not exist yet; resolve lexically against base.
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };
    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                normalized.pop();
            }
            CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.starts_with(&base)
}

/// Reads and parses the archive into memory. Rejects entries with unsafe
/// names (path traversal).
pub fn load_backup(path: &Path) -> Result<LoadedBackup> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    let mut manifest: Option<Manifest> = None;
    let mut data: BTreeMap<String, SessionFiles> = BTreeMap::new();
    let mut extras: Vec<(String, Vec<u8>)> = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();

        if name.contains("..") {
            continue;
        }

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;

        if name == "manifest.json" {
            manifest = Some(serde_json::from_slice(&bytes).context("parsing manifest.json")?);
            continue;
        }

        if let Some(rel) = name.strip_prefix("extras/") {
            if !rel.is_empty() {
                extras.push((rel.to_string(), bytes));
            }
            continue;
        }

        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() < 3 || parts[0] != "sessions" {
            continue;
        }
        let session_id = parts[1].to_string();
        let files = data.entry(session_id).or_default();

        match parts.as_slice() {
            // v2: kind-namespaced companions
            ["sessions", _, "env", rest @ ..] => {
                files.env.push((rest.join("/"), bytes));
            }
            ["sessions", _, "todos", rest @ ..] => {
                files.todos.push((rest.join("/"), bytes));
            }
            // session jsonl: sessions/<id>/<id>.jsonl
            ["sessions", _, file] if file.ends_with(".jsonl") => {
                files.session_jsonl = Some(bytes);
            }
            // v1 (Go) flat layout: env.json / todos.json / <id>.jsonl
            ["sessions", _, file] => {
                if *file == "env.json" {
                    files.env.push(((*file).to_string(), bytes));
                } else if *file == "todos.json" {
                    files.todos.push(((*file).to_string(), bytes));
                }
            }
            _ => {}
        }
    }

    let manifest = manifest.context("invalid backup: manifest.json not found")?;
    if manifest.sessions.is_empty() {
        anyhow::bail!("invalid backup: no sessions in manifest");
    }

    Ok(LoadedBackup {
        manifest,
        data,
        extras,
    })
}

/// Writes archived `extras/` back under `claude_dir`, never clobbering an
/// existing file. Returns the number of files written. Extras are auxiliary
/// state and are restored verbatim (paths inside are not remapped).
pub fn restore_extras(claude_dir: &Path, backup: &LoadedBackup, dry_run: bool) -> usize {
    let mut written = 0;
    for (rel, bytes) in &backup.extras {
        let dest = claude_dir.join(rel);
        if !is_path_safe(claude_dir, &dest) || dest.exists() {
            continue;
        }
        if dry_run {
            written += 1;
            continue;
        }
        if let Some(parent) = dest.parent() {
            if fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        if fs::write(&dest, bytes).is_ok() {
            written += 1;
        }
    }
    written
}

/// Re-hashes the actual archived bytes and checks them against the per-artifact
/// checksums in the manifest. Returns the list of mismatches (empty == OK).
/// v1 manifests carry no per-artifact checksums and are reported as skipped.
pub fn verify_checksums(backup: &LoadedBackup) -> ChecksumReport {
    let mut report = ChecksumReport::default();
    if backup.manifest.is_v1() {
        report.skipped = true;
        return report;
    }
    for session in &backup.manifest.sessions {
        let files = backup.data.get(&session.id);
        for artifact in &session.artifacts {
            report.checked += 1;
            let actual = files
                .and_then(|f| lookup_artifact(f, artifact))
                .map(|bytes| {
                    let mut h = Sha256::new();
                    h.update(bytes);
                    hex::encode(h.finalize())
                });
            match actual {
                Some(sha) if sha == artifact.sha256 => {}
                Some(_) => report.mismatches.push(format!(
                    "{}/{}: checksum mismatch",
                    session.id, artifact.logical_name
                )),
                None => report.mismatches.push(format!(
                    "{}/{}: missing from archive",
                    session.id, artifact.logical_name
                )),
            }
        }
    }
    report
}

fn lookup_artifact<'a>(
    files: &'a SessionFiles,
    artifact: &crate::model::Artifact,
) -> Option<&'a Vec<u8>> {
    use crate::model::ArtifactKind::*;
    match artifact.kind {
        Session => files.session_jsonl.as_ref(),
        Env => files
            .env
            .iter()
            .find(|(n, _)| n == &artifact.logical_name)
            .map(|(_, b)| b),
        Todos => files
            .todos
            .iter()
            .find(|(n, _)| n == &artifact.logical_name)
            .map(|(_, b)| b),
    }
}

#[derive(Default)]
pub struct ChecksumReport {
    pub checked: usize,
    pub skipped: bool,
    pub mismatches: Vec<String>,
}

impl ChecksumReport {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }
}

#[derive(Clone)]
pub struct RestoreOptions {
    pub claude_dir: PathBuf,
    pub mappings: BTreeMap<String, String>,
    pub dry_run: bool,
    pub rewrite_tool_paths: bool,
    pub restore_history: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Restored,
    Skipped,
    Failed,
    DryRun,
}

#[derive(Debug, Clone)]
pub struct SessionResult {
    pub session_id: String,
    pub old_cwd: String,
    pub new_cwd: String,
    pub encoded: String,
    pub status: Status,
    pub note: String,
    pub lines_rewritten: usize,
    pub case_collision: Option<String>,
}

#[derive(Default)]
pub struct Results {
    pub restored: usize,
    pub skipped: usize,
    pub failed: usize,
    pub details: Vec<SessionResult>,
}

/// Case-insensitive targets (macOS APFS default, Windows) can silently merge
/// two directories that differ only in case.
fn target_is_case_insensitive() -> bool {
    matches!(std::env::consts::OS, "macos" | "windows")
}

fn existing_case_collision(projects_dir: &Path, encoded: &str) -> Option<String> {
    if !target_is_case_insensitive() {
        return None;
    }
    let entries = fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name != encoded && claudepath::case_insensitive_eq(&name, encoded) {
            return Some(name.into_owned());
        }
    }
    None
}

/// Restores the given sessions according to `opts`. Each session's JSONL has
/// its `cwd` rewritten structurally; companions are written under their
/// original names; integrity is the caller's responsibility (verify first).
pub fn restore_sessions<F>(
    sessions: &[crate::model::Session],
    data: &BTreeMap<String, SessionFiles>,
    opts: &RestoreOptions,
    mut progress: F,
) -> Result<Results>
where
    F: FnMut(usize, usize, &str),
{
    let mut results = Results::default();
    let projects_dir = opts.claude_dir.join("projects");

    if !opts.dry_run {
        for sub in ["projects", "session-env", "todos"] {
            fs::create_dir_all(opts.claude_dir.join(sub))
                .with_context(|| format!("creating {sub} dir"))?;
        }
    }

    let total = sessions.len();
    for (i, session) in sessions.iter().enumerate() {
        progress(i + 1, total, &session.cwd);

        let new_cwd = remap::apply_longest_mapping(&session.cwd, &opts.mappings);
        let encoded = claudepath::encode(&new_cwd);
        let project_dir = projects_dir.join(&encoded);
        let session_path = project_dir.join(format!("{}.jsonl", session.id));

        let mut detail = SessionResult {
            session_id: session.id.clone(),
            old_cwd: session.cwd.clone(),
            new_cwd: new_cwd.clone(),
            encoded: encoded.clone(),
            status: Status::DryRun,
            note: String::new(),
            lines_rewritten: 0,
            case_collision: existing_case_collision(&projects_dir, &encoded),
        };

        let files = match data.get(&session.id) {
            Some(f) => f,
            None => {
                detail.status = Status::Failed;
                detail.note = "session data not found in archive".into();
                results.failed += 1;
                results.details.push(detail);
                continue;
            }
        };

        if !opts.dry_run && session_path.exists() {
            detail.status = Status::Skipped;
            detail.note = "session already exists".into();
            results.skipped += 1;
            results.details.push(detail);
            continue;
        }

        if opts.dry_run {
            detail.status = Status::DryRun;
            results.restored += 1;
            results.details.push(detail);
            continue;
        }

        if !is_path_safe(&opts.claude_dir, &project_dir) {
            detail.status = Status::Failed;
            detail.note = "unsafe destination path".into();
            results.failed += 1;
            results.details.push(detail);
            continue;
        }

        if let Err(e) = fs::create_dir_all(&project_dir) {
            detail.status = Status::Failed;
            detail.note = format!("cannot create dir: {e}");
            results.failed += 1;
            results.details.push(detail);
            continue;
        }

        let raw = match &files.session_jsonl {
            Some(b) => b,
            None => {
                detail.status = Status::Failed;
                detail.note = "session jsonl missing in archive".into();
                results.failed += 1;
                results.details.push(detail);
                continue;
            }
        };

        let text = String::from_utf8_lossy(raw);
        let (rewritten, stats) =
            remap::rewrite_jsonl(&text, &opts.mappings, opts.rewrite_tool_paths);
        detail.lines_rewritten = stats.lines_changed;

        if let Err(e) = fs::write(&session_path, rewritten.as_bytes()) {
            detail.status = Status::Failed;
            detail.note = format!("cannot write session: {e}");
            results.failed += 1;
            results.details.push(detail);
            continue;
        }

        // env companions get a structured path rewrite; todos are verbatim.
        for (name, bytes) in &files.env {
            let payload = remap::rewrite_env_json(bytes, &opts.mappings);
            write_file(&opts.claude_dir.join("session-env").join(name), &payload);
        }
        for (name, bytes) in &files.todos {
            write_file(&opts.claude_dir.join("todos").join(name), bytes);
        }

        if opts.restore_history {
            append_history(&opts.claude_dir, &new_cwd, session);
        }

        detail.status = Status::Restored;
        results.restored += 1;
        results.details.push(detail);
    }

    Ok(results)
}

fn write_file(dest: &Path, payload: &[u8]) {
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(dest, payload);
}

fn append_history(claude_dir: &Path, project: &str, session: &crate::model::Session) {
    let entry = serde_json::json!({
        "display": session.preview,
        "pastedContents": {},
        "timestamp": session.timestamp.timestamp_millis(),
        "project": project,
    });
    let path = claude_dir.join("history.jsonl");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        if let Ok(line) = serde_json::to_string(&entry) {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_safe_rejects_traversal() {
        let base = std::env::temp_dir();
        assert!(is_path_safe(&base, &base.join("projects/x")));
        assert!(!is_path_safe(&base, Path::new("/etc/passwd")));
    }
}
