use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::plan::Plan;
use crate::provider::ProviderId;

#[derive(Clone)]
pub enum Passphrase {
    None,
    Value(String),
}

#[derive(Clone)]
pub struct ProviderSelection {
    pub provider: ProviderId,
    pub home: PathBuf,
    pub item_ids: Option<Vec<String>>,
    pub include_state: bool,
    pub include_credentials: bool,
}

#[derive(Clone)]
pub struct ClaudeSelection {
    pub claude_dir: PathBuf,
    pub session_ids: Option<Vec<String>>,
    pub match_path: Option<String>,
    pub include_extras: bool,
    pub include_config: bool,
    pub include_credentials: bool,
}

#[derive(Clone)]
pub struct BackupRequest {
    pub output: PathBuf,
    pub overwrite: bool,
    pub claude: Option<ClaudeSelection>,
    pub providers: Vec<ProviderSelection>,
    pub passphrase: Passphrase,
    pub redact: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub output: PathBuf,
    pub encrypted: bool,
    pub sessions: usize,
    pub provider_items: usize,
    pub files: usize,
    pub bytes: u64,
    pub secret_findings: usize,
    pub changed_during_backup: Vec<String>,
    #[serde(default)]
    pub sqlite_snapshots: usize,
    #[serde(default)]
    pub sqlite_fallbacks: Vec<String>,
    #[serde(default)]
    pub redacted_secrets: usize,
}

struct SnapshotSummary {
    changed: Vec<String>,
    sqlite: usize,
    sqlite_fallbacks: Vec<String>,
    redacted: usize,
}

#[derive(Clone)]
pub struct RestoreRequest {
    pub archive: PathBuf,
    pub passphrase: Passphrase,
    pub claude_dir: PathBuf,
    pub provider_homes: BTreeMap<ProviderId, PathBuf>,
    pub only_providers: Option<Vec<ProviderId>>,
    pub only_sessions: Option<std::collections::BTreeSet<String>>,
    pub only_items: Option<std::collections::BTreeSet<(ProviderId, String)>>,
    pub mappings: BTreeMap<String, String>,
    pub rewrite_tool_paths: bool,
    pub restore_history: bool,
    pub apply_config: bool,
    pub overwrite: bool,
    pub allow_exec_config: bool,
    pub allow_credentials: bool,
    pub allow_running: bool,
    pub skip_verify: bool,
}

impl RestoreRequest {
    fn wants_session(&self, id: &str) -> bool {
        self.only_sessions
            .as_ref()
            .is_none_or(|ids| ids.contains(id))
    }

    fn wants_item(&self, provider: ProviderId, id: &str) -> bool {
        self.only_items
            .as_ref()
            .is_none_or(|items| items.contains(&(provider, id.to_string())))
    }
}

pub struct PreparedRestore {
    pub plan: Plan,
    pub request: RestoreRequest,
    pub staging: Option<tempfile::TempDir>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub journal_id: Option<String>,
    pub written: usize,
    pub merged: usize,
    pub skipped: usize,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    pub encrypted: bool,
    pub schema_version: u32,
    pub checked: usize,
    pub mismatches: Vec<String>,
    pub unexpected_entries: Vec<String>,
    pub unsafe_entries: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
            && self.unexpected_entries.is_empty()
            && self.unsafe_entries.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretFinding {
    pub provider: String,
    pub rel: String,
    pub detector: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct JournalEntry {
    pub id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub archive: String,
    pub ops: usize,
    pub rolled_back: bool,
}

struct BackupInputs {
    manifest: crate::model::Manifest,
    files: Vec<(String, PathBuf)>,
    _staging: tempfile::TempDir,
}

fn collect(request: &BackupRequest) -> Result<BackupInputs> {
    collect_inputs(request, true, true)
}

fn collect_inputs(
    request: &BackupRequest,
    hashes: bool,
    enforce_encryption: bool,
) -> Result<BackupInputs> {
    if enforce_encryption
        && matches!(request.passphrase, Passphrase::None)
        && (request
            .claude
            .as_ref()
            .is_some_and(|c| c.include_credentials)
            || request.providers.iter().any(|p| p.include_credentials))
    {
        bail!("credentials require encryption");
    }
    let staging = crate::archive::staging_dir()?;
    let selected = if let Some(c) = &request.claude {
        crate::discover::load_sessions(&c.claude_dir)?
            .into_iter()
            .filter(|s| {
                c.session_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&s.session.id))
                    && c.match_path
                        .as_ref()
                        .is_none_or(|m| s.session.cwd.contains(m))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (mut manifest, mut files) = crate::backup::plan_with_hashes(&selected, hashes)?;
    if let Some(c) = &request.claude {
        crate::backup::collect_additional_with_hashes(
            &c.claude_dir,
            crate::backup::BackupOptions {
                include_extras: c.include_extras,
                include_config: c.include_config,
                include_credentials: c.include_credentials,
            },
            &mut manifest,
            &mut files,
            staging.path(),
            hashes,
        )?;
    }
    for selection in &request.providers {
        let provider = crate::provider::find(selection.provider)
            .ok_or_else(|| anyhow::anyhow!("unknown provider"))?;
        let inventory = provider.scan(
            &selection.home,
            &crate::provider::ScanOptions {
                include_state: selection.include_state,
                include_credentials: selection.include_credentials,
            },
        )?;
        let mut record = crate::model::ProviderManifest {
            id: selection.provider,
            source_home: selection.home.display().to_string(),
            items: Vec::new(),
            files: Vec::new(),
        };
        for item in inventory.items {
            if selection
                .item_ids
                .as_ref()
                .is_some_and(|ids| !ids.contains(&item.id))
            {
                continue;
            }
            for file in &item.files {
                if enforce_encryption
                    && file.class == crate::provider::FileClass::Credential
                    && matches!(request.passphrase, Passphrase::None)
                {
                    bail!("credentials require encryption")
                }
                let path = crate::safety::safe_join(&selection.home, &file.rel)?;
                if hashes {
                    crate::archive::ensure_regular(&path)?;
                }
                if record.files.iter().any(|d| d.rel == file.rel) {
                    continue;
                }
                let (sha256, size) = crate::backup::inventory_digest(&path, hashes)?;
                record.files.push(crate::model::FileDigest {
                    rel: file.rel.clone(),
                    sha256,
                    size,
                });
                files.push((
                    format!("providers/{}/{}", selection.provider.as_str(), file.rel),
                    path,
                ));
            }
            let archived_files = item
                .files
                .into_iter()
                .map(|file| {
                    let sha256 = record
                        .files
                        .iter()
                        .find(|d| d.rel == file.rel)
                        .unwrap()
                        .sha256
                        .clone();
                    crate::model::ArchivedFile { file, sha256 }
                })
                .collect();
            record.items.push(crate::model::ArchivedItem {
                id: item.id,
                kind: item.kind,
                label: item.label,
                cwd: item.cwd,
                updated_at: item.updated_at,
                files: archived_files,
            });
        }
        manifest.providers.push(record);
    }
    for (name, path) in &files {
        crate::safety::validate_rel(name)?;
        if hashes {
            crate::archive::ensure_regular(path)?;
        }
    }
    Ok(BackupInputs {
        manifest,
        files,
        _staging: staging,
    })
}

pub fn plan_backup(request: &BackupRequest) -> Result<Plan> {
    let input = collect(request)?;
    backup_plan(request, &input)
}

fn backup_plan(request: &BackupRequest, input: &BackupInputs) -> Result<Plan> {
    let mut plan = Plan::default();
    let action = if request.output.exists() {
        if request.overwrite {
            crate::plan::Action::Overwrite
        } else {
            crate::plan::Action::Refuse("output already exists".into())
        }
    } else {
        crate::plan::Action::Create
    };
    plan.ops.push(crate::plan::PlannedOp {
        provider: "archive".into(),
        dest: request.output.clone(),
        action,
        risk: crate::plan::Risk::Sensitive,
        size: input
            .files
            .iter()
            .map(|(_, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum(),
        rewrites: 0,
        note: crate::ui::plural(input.files.len(), "file"),
    });
    Ok(plan)
}

pub fn run_backup(request: &BackupRequest) -> Result<BackupReport> {
    run_backup_after_scan(request, |_| Ok(()))
}

pub struct PreparedBackup {
    pub manifest: crate::model::Manifest,
    pub files: Vec<(String, PathBuf)>,
    pub bytes: u64,
    pub secret_findings: usize,
    pub changed_during_backup: Vec<String>,
    pub sqlite_snapshots: usize,
    pub sqlite_fallbacks: Vec<String>,
    pub redacted_secrets: usize,
    _snapshots: tempfile::TempDir,
    _staging: tempfile::TempDir,
}

pub fn prepare_backup(
    request: &BackupRequest,
    workdir: &std::path::Path,
) -> Result<PreparedBackup> {
    prepare_backup_after_scan(request, workdir, |_| Ok(()))
}

fn prepare_backup_after_scan(
    request: &BackupRequest,
    workdir: &std::path::Path,
    after_scan: impl FnOnce(&BackupInputs) -> Result<()>,
) -> Result<PreparedBackup> {
    let mut input = collect_inputs(request, false, true)?;
    let plan = backup_plan(request, &input)?;
    if plan.has_refusals() {
        bail!("backup plan contains refusals")
    }
    after_scan(&input)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".aisession-backup-snapshot-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let snapshots = builder.tempdir_in(workdir)?;
    let summary = snapshot_inputs(&mut input, snapshots.path(), request.redact)?;
    let mut secret_findings = 0;
    let mut bytes = 0;
    for (name, path) in &input.files {
        bytes += std::fs::metadata(path)?.len();
        secret_findings +=
            crate::secrets::scan_reader("archive", name, std::fs::File::open(path)?)?
                .iter()
                .map(|f| f.count)
                .sum::<usize>();
    }
    Ok(PreparedBackup {
        manifest: input.manifest,
        files: input.files,
        bytes,
        secret_findings,
        changed_during_backup: summary.changed,
        sqlite_snapshots: summary.sqlite,
        sqlite_fallbacks: summary.sqlite_fallbacks,
        redacted_secrets: summary.redacted,
        _snapshots: snapshots,
        _staging: input._staging,
    })
}

fn run_backup_after_scan(
    request: &BackupRequest,
    after_scan: impl FnOnce(&BackupInputs) -> Result<()>,
) -> Result<BackupReport> {
    let parent = request
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let prepared = prepare_backup_after_scan(request, parent, after_scan)?;
    crate::archive::write(
        &request.output,
        request.overwrite,
        &prepared.manifest,
        &prepared.files,
        &request.passphrase,
    )?;
    Ok(BackupReport {
        output: request.output.clone(),
        encrypted: matches!(request.passphrase, Passphrase::Value(_)),
        sessions: prepared.manifest.sessions.len(),
        provider_items: prepared
            .manifest
            .providers
            .iter()
            .map(|p| p.items.len())
            .sum(),
        files: prepared.files.len(),
        bytes: prepared.bytes,
        secret_findings: prepared.secret_findings,
        changed_during_backup: prepared.changed_during_backup,
        sqlite_snapshots: prepared.sqlite_snapshots,
        sqlite_fallbacks: prepared.sqlite_fallbacks,
        redacted_secrets: prepared.redacted_secrets,
    })
}

fn redact_snapshot(root: &std::path::Path, rel: &str) -> Result<usize> {
    let path = crate::safety::safe_join(root, rel)?;
    let bytes = std::fs::read(&path)?;
    if !crate::secrets::redactable(rel, &bytes) {
        return Ok(0);
    }
    let (redacted, count) = crate::secrets::redact(&bytes);
    if count > 0 {
        crate::safety::atomic_write(root, rel, &redacted, crate::safety::WriteMode::Replace)?;
    }
    Ok(count)
}

fn snapshot_file(
    source: &std::path::Path,
    root: &std::path::Path,
    rel: &str,
    redact: bool,
) -> Result<Option<(String, u64, bool, usize)>> {
    use std::io::Read;
    for attempt in 0..2 {
        let before = match std::fs::symlink_metadata(source) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => bail!("snapshot source is not a regular file: {rel}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if before.len() > crate::archive::MAX_ENTRY_SIZE {
            bail!("snapshot source exceeds maximum entry size: {rel}")
        }
        let file = match std::fs::File::open(source) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let copied = crate::safety::atomic_write_from(
            root,
            rel,
            &mut file.take(before.len()),
            crate::safety::WriteMode::Replace,
        )?;
        let after = match std::fs::symlink_metadata(source) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => bail!("snapshot source became unsafe: {rel}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let changed = copied != before.len()
            || before.len() != after.len()
            || before.modified()? != after.modified()?;
        if changed && attempt == 0 {
            continue;
        }
        let redacted = if redact {
            redact_snapshot(root, rel)?
        } else {
            0
        };
        let (digest, size) = crate::backup::hash_file(&crate::safety::safe_join(root, rel)?)?;
        return Ok(Some((digest, size, changed, redacted)));
    }
    unreachable!()
}

fn snapshot_inputs(
    input: &mut BackupInputs,
    root: &std::path::Path,
    redact: bool,
) -> Result<SnapshotSummary> {
    let mut redacted = 0;
    use std::collections::BTreeSet;
    let mut changed = BTreeSet::new();
    let mut digests = BTreeMap::new();
    let mut consistent = BTreeSet::new();
    let mut companions = BTreeSet::new();
    let mut sqlite_fallbacks = Vec::new();
    for (rel, source) in &input.files {
        crate::safety::validate_rel(rel)?;
        if consistent.contains(rel) || !crate::sqlite_snapshot::is_sqlite(source) {
            continue;
        }
        let dest = crate::safety::safe_join(root, rel)?;
        match crate::sqlite_snapshot::snapshot(source, &dest) {
            Ok(()) => {
                digests.insert(rel.clone(), crate::backup::hash_file(&dest)?);
                consistent.insert(rel.clone());
                for suffix in crate::sqlite_snapshot::COMPANIONS {
                    companions.insert(format!("{rel}{suffix}"));
                }
            }
            Err(err) => sqlite_fallbacks.push(format!("{rel}: {err:#}")),
        }
    }
    for (rel, source) in &input.files {
        if consistent.contains(rel) || companions.contains(rel) {
            continue;
        }
        if digests.contains_key(rel) {
            bail!("duplicate snapshot entry: {rel}")
        }
        match snapshot_file(source, root, rel, redact)? {
            Some((digest, size, unstable, secrets)) => {
                redacted += secrets;
                digests.insert(rel.clone(), (digest, size));
                if unstable {
                    changed.insert(rel.clone());
                }
            }
            None => {
                changed.insert(rel.clone());
            }
        }
    }
    input.manifest.sessions.retain_mut(|session| {
        let main = format!("sessions/{}/{}.jsonl", session.id, session.id);
        if !digests.contains_key(&main) {
            return false;
        }
        session.artifacts.retain_mut(|artifact| {
            let rel =
                crate::backup::archive_path(artifact.kind, &session.id, &artifact.logical_name);
            if let Some((digest, size)) = digests.get(&rel) {
                artifact.sha256 = digest.clone();
                artifact.size = *size;
                true
            } else {
                false
            }
        });
        session.size_bytes = digests[&main].1;
        true
    });
    for (prefix, entries) in [
        ("extras", &mut input.manifest.extras),
        ("config", &mut input.manifest.config),
    ] {
        entries.retain_mut(|entry| {
            if let Some((digest, size)) = digests.get(&format!("{prefix}/{}", entry.rel)) {
                entry.sha256 = digest.clone();
                entry.size = *size;
                true
            } else {
                false
            }
        });
    }
    for provider in &mut input.manifest.providers {
        let prefix = format!("providers/{}/", provider.id.as_str());
        provider.files.retain_mut(|entry| {
            if let Some((digest, size)) = digests.get(&format!("{prefix}{}", entry.rel)) {
                entry.sha256 = digest.clone();
                entry.size = *size;
                true
            } else {
                false
            }
        });
        provider.items.retain_mut(|item| {
            item.files.retain_mut(|entry| {
                if let Some((digest, size)) = digests.get(&format!("{prefix}{}", entry.rel)) {
                    entry.sha256 = digest.clone();
                    entry.file.size = *size;
                    true
                } else {
                    false
                }
            });
            !item.files.is_empty()
        });
    }
    let mut referenced = BTreeSet::new();
    for session in &input.manifest.sessions {
        for artifact in &session.artifacts {
            referenced.insert(crate::backup::archive_path(
                artifact.kind,
                &session.id,
                &artifact.logical_name,
            ));
        }
    }
    for (prefix, entries) in [
        ("extras", &input.manifest.extras),
        ("config", &input.manifest.config),
    ] {
        for entry in entries {
            referenced.insert(format!("{prefix}/{}", entry.rel));
        }
    }
    for provider in &input.manifest.providers {
        for entry in &provider.files {
            referenced.insert(format!("providers/{}/{}", provider.id.as_str(), entry.rel));
        }
    }
    input.files.retain_mut(|(rel, path)| {
        if referenced.contains(rel) {
            *path = root.join(rel);
            true
        } else {
            changed.insert(rel.clone());
            false
        }
    });
    input.manifest.total_size = input.files.iter().map(|(rel, _)| digests[rel].1).sum();
    input.manifest.checksum = crate::backup::global_checksum(&input.manifest.sessions);
    Ok(SnapshotSummary {
        changed: changed.into_iter().collect(),
        sqlite: consistent.len(),
        sqlite_fallbacks,
        redacted,
    })
}

pub fn audit_selection(request: &BackupRequest) -> Result<Vec<SecretFinding>> {
    let input = collect_inputs(request, false, false)?;
    let mut findings = Vec::new();
    for (name, path) in input.files {
        findings.extend(crate::secrets::scan_reader(
            "archive",
            &name,
            std::fs::File::open(path)?,
        )?);
    }
    Ok(findings)
}

pub fn read_manifest(
    path: &std::path::Path,
    passphrase: &Passphrase,
) -> Result<crate::model::Manifest> {
    Ok(crate::archive::stage(path, passphrase)?.manifest)
}

pub fn verify_archive(archive: &std::path::Path, passphrase: &Passphrase) -> Result<VerifyReport> {
    Ok(crate::archive::stage(archive, passphrase)?.report)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Execution {
    dest: PathBuf,
    base: PathBuf,
    rel: String,
    payload: String,
    before: Option<String>,
}

const MAX_STRUCTURED_BYTES: u64 = 1024 * 1024 * 1024;

fn bounded_bytes(path: &std::path::Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_STRUCTURED_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STRUCTURED_BYTES {
        bail!("too large to remap")
    }
    Ok(bytes)
}

fn config_class(rel: &str, path: &std::path::Path) -> Result<crate::provider::FileClass> {
    use crate::provider::FileClass;
    let lower = rel.to_ascii_lowercase();
    if lower.contains("credential") || lower.contains("oauth") || lower.ends_with("auth.json") {
        return Ok(FileClass::Credential);
    }
    if lower.starts_with("plugins/") || lower.contains("mcp") {
        return Ok(FileClass::ExecutesCode);
    }
    if matches!(
        lower.rsplit(['/', '\\']).next(),
        Some("settings.json" | "settings.local.json")
    ) {
        return Ok(FileClass::ExecutesCode);
    }
    if lower.ends_with(".json") {
        let bytes = bounded_bytes(path)?;
        if serde_json::from_slice::<serde_json::Value>(&bytes)
            .is_ok_and(|value| has_exec_keys(&value))
        {
            return Ok(FileClass::ExecutesCode);
        }
    }
    Ok(FileClass::Plain)
}

fn has_exec_keys(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "hooks" | "statusLine" | "apiKeyHelper" | "mcpServers"
            ) || has_exec_keys(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(has_exec_keys),
        _ => false,
    }
}

#[derive(Clone)]
struct Payload {
    path: PathBuf,
    changes: usize,
}
impl Payload {
    fn original(path: &std::path::Path) -> Self {
        Self {
            path: path.into(),
            changes: 0,
        }
    }
}

struct Planner<'a> {
    request: &'a RestoreRequest,
    staging: &'a std::path::Path,
    plan: Plan,
    execution: Vec<Execution>,
}

impl Planner<'_> {
    fn decision(&mut self, provider: &str, dest: PathBuf, action: crate::plan::Action) {
        self.plan.ops.push(crate::plan::PlannedOp {
            provider: provider.into(),
            dest,
            action,
            risk: crate::plan::Risk::Normal,
            size: 0,
            rewrites: 0,
            note: String::new(),
        });
    }
    fn refuse(&mut self, provider: &str, reason: &str) {
        self.decision(
            provider,
            PathBuf::new(),
            crate::plan::Action::Refuse(reason.into()),
        );
    }
    fn skip(&mut self, reason: &str) {
        self.decision(
            "claude",
            PathBuf::new(),
            crate::plan::Action::Skip(reason.into()),
        );
    }
    fn add(
        &mut self,
        provider: &str,
        base: &std::path::Path,
        rel: &str,
        payload: &Payload,
        class: crate::provider::FileClass,
        policy: (bool, bool),
    ) -> Result<()> {
        use crate::plan::{Action, PlannedOp, Risk};
        let (preserve, merge) = policy;
        let dest = match checked_dest(base, rel) {
            Ok(path) => path,
            Err(_) => {
                self.refuse(provider, "unsafe destination path");
                return Ok(());
            }
        };
        let risk = match class {
            crate::provider::FileClass::ExecutesCode => Risk::ExecutesCode,
            crate::provider::FileClass::Credential | crate::provider::FileClass::Sensitive => {
                Risk::Sensitive
            }
            _ => Risk::Normal,
        };
        let mut action = if dest.exists() {
            if merge {
                Action::Merge
            } else if !preserve && self.request.overwrite {
                Action::Overwrite
            } else {
                Action::Skip("destination exists".into())
            }
        } else {
            Action::Create
        };
        if class == crate::provider::FileClass::Credential && !self.request.allow_credentials {
            action = Action::Skip("requires --allow-credentials".into());
        }
        if risk == Risk::ExecutesCode && !self.request.allow_exec_config {
            action = Action::Skip("requires --allow-exec-config".into());
        }
        if self.execution.iter().any(|e| e.dest == dest) {
            action = Action::Skip("destination already planned".into());
        }
        self.plan.ops.push(PlannedOp {
            provider: provider.into(),
            dest: dest.clone(),
            action: action.clone(),
            risk,
            size: std::fs::metadata(&payload.path)?.len(),
            rewrites: payload.changes,
            note: String::new(),
        });
        if matches!(action, Action::Create | Action::Overwrite | Action::Merge) {
            let source = payload
                .path
                .strip_prefix(self.staging)?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid staging path"))?
                .replace('\\', "/");
            crate::safety::validate_rel(&source)?;
            let before = if dest.exists() {
                Some(crate::backup::hash_file(&dest)?.0)
            } else {
                None
            };
            let base = if base.is_absolute() {
                base.to_path_buf()
            } else {
                std::env::current_dir()?.join(base)
            };
            self.execution.push(Execution {
                dest,
                base,
                rel: rel.into(),
                payload: source,
                before,
            });
        }
        Ok(())
    }
    fn generated(&self, bytes: &[u8], changes: usize) -> Result<Payload> {
        let mut temp = tempfile::NamedTempFile::new_in(self.staging)?;
        use std::io::Write;
        temp.write_all(bytes)?;
        temp.flush()?;
        let (_, path) = temp.keep()?;
        Ok(Payload { path, changes })
    }
    fn lines(
        &mut self,
        path: &std::path::Path,
        mut rewrite: impl FnMut(&str) -> Result<(Vec<u8>, usize)>,
    ) -> Result<Payload> {
        use std::io::{BufRead, Read, Write};
        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut output: Option<tempfile::NamedTempFile> = None;
        let mut line = Vec::new();
        let mut offset = 0;
        let mut changes = 0;
        loop {
            line.clear();
            let n = reader
                .by_ref()
                .take(MAX_STRUCTURED_BYTES + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if n as u64 > MAX_STRUCTURED_BYTES {
                self.plan
                    .warnings
                    .push("Oversized JSONL line: file preserved verbatim".into());
                return Ok(Payload::original(path));
            }
            let text = match std::str::from_utf8(&line) {
                Ok(text) => text,
                Err(_) => {
                    self.plan
                        .warnings
                        .push("Invalid UTF-8 JSONL: file preserved verbatim".into());
                    return Ok(Payload::original(path));
                }
            };
            let (bytes, count) = if self.request.mappings.is_empty() {
                (Vec::new(), 0)
            } else {
                rewrite(text)?
            };
            if count > 0 && bytes != line && output.is_none() {
                let mut temp = tempfile::NamedTempFile::new_in(self.staging)?;
                std::io::copy(&mut std::fs::File::open(path)?.take(offset), &mut temp)?;
                output = Some(temp);
            }
            if let Some(temp) = output.as_mut() {
                temp.write_all(if count > 0 { &bytes } else { &line })?;
            }
            changes += count;
            offset += n as u64;
        }
        if let Some(mut temp) = output {
            temp.flush()?;
            let (_, path) = temp.keep()?;
            Ok(Payload { path, changes })
        } else {
            Ok(Payload {
                path: path.into(),
                changes,
            })
        }
    }
    fn claude_jsonl(&mut self, path: &std::path::Path) -> Result<Payload> {
        let mappings = &self.request.mappings;
        let tools = self.request.rewrite_tool_paths;
        self.lines(path, |text| {
            let (bytes, stats) = crate::remap::rewrite_jsonl(text, mappings, tools);
            Ok((bytes.into_bytes(), stats.lines_changed))
        })
    }
    fn provider_file(
        &mut self,
        provider: &dyn crate::provider::Provider,
        file: &crate::model::ArchivedFile,
        path: &std::path::Path,
        home: &std::path::Path,
    ) -> Result<()> {
        self.provider_file_with_limit(provider, file, path, home, MAX_STRUCTURED_BYTES)
    }
    fn provider_file_with_limit(
        &mut self,
        provider: &dyn crate::provider::Provider,
        file: &crate::model::ArchivedFile,
        path: &std::path::Path,
        home: &std::path::Path,
        limit: u64,
    ) -> Result<()> {
        use crate::provider::FileClass;
        let class = crate::provider::effective_class(file.class, provider.classify(&file.rel));
        if class != FileClass::OpaqueState
            && !self.request.mappings.is_empty()
            && std::fs::metadata(path)?.len() > limit
        {
            let dest = checked_dest(home, &file.rel)?;
            self.decision(
                provider.id().as_str(),
                dest,
                crate::plan::Action::Skip("too large to remap".into()),
            );
            if let Some(op) = self.plan.ops.last_mut() {
                op.risk = match class {
                    FileClass::ExecutesCode => crate::plan::Risk::ExecutesCode,
                    FileClass::Credential | FileClass::Sensitive => crate::plan::Risk::Sensitive,
                    _ => crate::plan::Risk::Normal,
                };
                op.size = std::fs::metadata(path)?.len();
            }
            self.plan.warnings.push(format!(
                "{}: too large to remap; file skipped",
                provider.id().as_str()
            ));
            return Ok(());
        }
        let result = (|| -> Result<(String, Payload)> {
            if class == FileClass::OpaqueState
                || self.request.mappings.is_empty()
                || (class == FileClass::ExecutesCode && !self.request.allow_exec_config)
                || (class == FileClass::Credential && !self.request.allow_credentials)
            {
                return Ok((file.rel.clone(), Payload::original(path)));
            }
            if file.rel.ends_with(".jsonl") {
                let mut rel = file.rel.clone();
                let mappings = &self.request.mappings;
                let payload = self.lines(path, |text| {
                    let rewritten = provider.rewrite(&file.rel, text.as_bytes(), mappings)?;
                    rel = rewritten.rel;
                    Ok((
                        rewritten.bytes.unwrap_or_else(|| text.as_bytes().to_vec()),
                        rewritten.changes,
                    ))
                })?;
                return Ok((rel, payload));
            }
            let bytes = bounded_bytes(path)?;
            let rewritten = provider.rewrite(&file.rel, &bytes, &self.request.mappings)?;
            let payload = match rewritten.bytes {
                Some(after) if after != bytes => self.generated(&after, rewritten.changes)?,
                _ => Payload {
                    path: path.into(),
                    changes: rewritten.changes,
                },
            };
            Ok((rewritten.rel, payload))
        })();
        match result {
            Ok((rel, payload)) => self.add(
                provider.id().as_str(),
                home,
                &rel,
                &payload,
                class,
                (false, false),
            ),
            Err(_) => {
                let dest = checked_dest(home, &file.rel).unwrap_or_else(|_| home.to_path_buf());
                self.decision(
                    provider.id().as_str(),
                    dest,
                    crate::plan::Action::Refuse(
                        "provider rewrite failed; inspect or exclude this file".into(),
                    ),
                );
                Ok(())
            }
        }
    }
}

pub(crate) fn checked_dest(base: &std::path::Path, rel: &str) -> Result<PathBuf> {
    let absolute = if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir()?.join(base)
    };
    crate::safety::safe_join(&absolute, rel)
}

pub fn prepare_restore(request: RestoreRequest) -> Result<PreparedRestore> {
    let staged = crate::archive::stage(&request.archive, &request.passphrase)?;
    prepare_restore_from(request, staged)
}

pub fn prepare_restore_from(
    request: RestoreRequest,
    staged: crate::archive::StagedArchive,
) -> Result<PreparedRestore> {
    use crate::provider::FileClass;
    let mut planner = Planner {
        request: &request,
        staging: staged.staging.path(),
        plan: Plan::default(),
        execution: Vec::new(),
    };
    if !staged.report.ok() && !request.skip_verify {
        planner.refuse("archive", "archive verification failed");
    } else {
        let claude_enabled = request
            .only_providers
            .as_ref()
            .is_none_or(|ids| ids.contains(&ProviderId::Claude));
        let writes_to_claude = request.apply_config
            || request
                .only_sessions
                .as_ref()
                .is_none_or(|ids| !ids.is_empty());
        if claude_enabled
            && writes_to_claude
            && !request.allow_running
            && crate::provider::find(ProviderId::Claude)
                .is_some_and(|p| p.running_hint(&request.claude_dir).is_some())
        {
            planner.refuse("claude", "provider may be running");
        }
        if claude_enabled {
            for session in &staged.manifest.sessions {
                if !request.wants_session(&session.id) {
                    continue;
                }
                if crate::safety::validate_id(&session.id).is_err() || session.cwd.trim().is_empty()
                {
                    planner
                        .skip("invalid session identifier or empty cwd; repair session metadata");
                    continue;
                }
                let cwd = crate::remap::apply_longest_mapping(&session.cwd, &request.mappings);
                if cwd.trim().is_empty() {
                    planner.skip("empty remapped cwd; provide a valid mapping");
                    continue;
                }
                let encoded = crate::claudepath::encode(&cwd);
                let prefix = format!("sessions/{}/", session.id);
                for (name, path) in staged.files.iter().filter(|(n, _)| n.starts_with(&prefix)) {
                    let rel = &name[prefix.len()..];
                    if !staged.manifest.is_v1()
                        && !session.artifacts.iter().any(|a| {
                            crate::backup::archive_path(a.kind, &session.id, &a.logical_name)
                                == *name
                        })
                    {
                        continue;
                    }
                    let mut payload = Payload::original(path);
                    let dest = if rel == format!("{}.jsonl", session.id) {
                        payload = planner.claude_jsonl(path)?;
                        format!("projects/{encoded}/{rel}")
                    } else if rel.starts_with("env/") || rel == "env.json" {
                        if !request.mappings.is_empty()
                            && path.extension().is_some_and(|e| e == "json")
                        {
                            match bounded_bytes(path) {
                                Ok(bytes) => {
                                    let rewritten =
                                        crate::remap::rewrite_env_json(&bytes, &request.mappings);
                                    if rewritten != bytes {
                                        payload = planner.generated(&rewritten, 1)?;
                                    }
                                }
                                Err(_) => planner
                                    .plan
                                    .warnings
                                    .push("Oversized env JSON: file preserved verbatim".into()),
                            }
                        }
                        format!(
                            "session-env/{}",
                            rel.strip_prefix("env/")
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("{}.json", session.id))
                        )
                    } else if rel == "todos.json" {
                        format!("todos/{}.json", session.id)
                    } else if let Some(todo) = rel.strip_prefix("todos/") {
                        format!("todos/{todo}")
                    } else if let Some(project) = rel.strip_prefix("project/") {
                        if project.starts_with("subagents/") && project.ends_with(".jsonl") {
                            payload = planner.claude_jsonl(path)?;
                        }
                        format!("projects/{encoded}/{}/{project}", session.id)
                    } else if let Some(memory) = rel.strip_prefix("memory/") {
                        format!("projects/{encoded}/memory/{memory}")
                    } else {
                        continue;
                    };
                    planner.add(
                        "claude",
                        &request.claude_dir,
                        &dest,
                        &payload,
                        FileClass::Sensitive,
                        (rel.starts_with("memory/"), false),
                    )?;
                }
            }
            let selected_sessions = staged
                .manifest
                .sessions
                .iter()
                .filter(|session| request.wants_session(&session.id))
                .count();
            let every_session_selected = selected_sessions == staged.manifest.sessions.len();
            if request.restore_history && selected_sessions > 0 {
                use std::io::Write;
                let history = checked_dest(&request.claude_dir, "history.jsonl")?;
                let mut temp = tempfile::NamedTempFile::new_in(staged.staging.path())?;
                if history.exists() {
                    std::io::copy(&mut std::fs::File::open(history)?, &mut temp)?;
                    if temp.as_file().metadata()?.len() > 0 {
                        temp.write_all(b"\n")?;
                    }
                }
                for session in &staged.manifest.sessions {
                    if !request.wants_session(&session.id)
                        || session.cwd.trim().is_empty()
                        || crate::safety::validate_id(&session.id).is_err()
                    {
                        continue;
                    }
                    serde_json::to_writer(
                        &mut temp,
                        &serde_json::json!({"display": session.preview,"pastedContents":{},"timestamp": session.timestamp.timestamp_millis(),"project":crate::remap::apply_longest_mapping(&session.cwd, &request.mappings)}),
                    )?;
                    temp.write_all(b"\n")?;
                }
                temp.flush()?;
                let (_, path) = temp.keep()?;
                planner.add(
                    "claude",
                    &request.claude_dir,
                    "history.jsonl",
                    &Payload::original(&path),
                    FileClass::Sensitive,
                    (false, true),
                )?;
            }
            for (name, path) in &staged.files {
                if let Some(rel) = name
                    .strip_prefix("extras/")
                    .filter(|_| every_session_selected)
                {
                    let class = if rel.starts_with("shell-snapshots/") {
                        FileClass::ExecutesCode
                    } else {
                        FileClass::Sensitive
                    };
                    planner.add(
                        "claude",
                        &request.claude_dir,
                        rel,
                        &Payload::original(path),
                        class,
                        (true, false),
                    )?;
                }
                if request.apply_config {
                    if let Some(rel) = name.strip_prefix("config/") {
                        if rel == "mcp-servers.json" || rel == "oauth-account.json" {
                            continue;
                        }
                        match config_class(rel, path) {
                            Ok(class) => planner.add(
                                "claude",
                                &request.claude_dir,
                                rel,
                                &Payload::original(path),
                                class,
                                (false, false),
                            )?,
                            Err(_) => planner
                                .refuse("claude", "cannot inspect config risk; file is too large"),
                        }
                    }
                }
            }
            if request.apply_config {
                plan_global_config(&mut planner, &staged.files)?;
            }
        }
        for record in &staged.manifest.providers {
            if request
                .only_providers
                .as_ref()
                .is_some_and(|ids| !ids.contains(&record.id))
            {
                continue;
            }
            let wanted: Vec<_> = record
                .items
                .iter()
                .filter(|item| request.wants_item(record.id, &item.id))
                .collect();
            if request.only_items.is_some() && wanted.is_empty() {
                continue;
            }
            let Some(home) = request.provider_homes.get(&record.id) else {
                planner.refuse(
                    record.id.as_str(),
                    "provider destination not specified; pass --provider-home <id>=<path> or leave it out with --provider",
                );
                continue;
            };
            let provider = crate::provider::find(record.id)
                .ok_or_else(|| anyhow::anyhow!("unknown provider"))?;
            if !request.allow_running && provider.running_hint(home).is_some() {
                planner.refuse(record.id.as_str(), "provider may be running");
                continue;
            }
            for item in wanted {
                for file in &item.files {
                    let name = format!("providers/{}/{}", record.id.as_str(), file.rel);
                    if crate::safety::validate_rel(&name).is_err() {
                        planner.refuse(record.id.as_str(), "unsafe provider path");
                        continue;
                    }
                    let Some(path) = staged.files.get(&name) else {
                        planner.refuse(record.id.as_str(), "missing provider file");
                        continue;
                    };
                    planner.provider_file(provider.as_ref(), file, path, home)?;
                }
            }
        }
    }
    crate::safety::atomic_write(
        staged.staging.path(),
        "execution.json",
        &serde_json::to_vec(&planner.execution)?,
        crate::safety::WriteMode::CreateNew,
    )?;
    let plan = planner.plan;
    Ok(PreparedRestore {
        plan,
        request,
        staging: Some(staged.staging),
    })
}

fn plan_global_config(planner: &mut Planner<'_>, files: &BTreeMap<String, PathBuf>) -> Result<()> {
    use crate::provider::FileClass;
    let request = planner.request;
    let parent = request
        .claude_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no Claude parent"))?;
    let mut root: Option<serde_json::Value> = None;
    let mut class = FileClass::Plain;
    let mut changes = 0;
    for (name, key, kind, allowed) in [
        (
            "config/mcp-servers.json",
            "mcpServers",
            FileClass::ExecutesCode,
            request.allow_exec_config,
        ),
        (
            "config/oauth-account.json",
            "oauthAccount",
            FileClass::Credential,
            request.allow_credentials,
        ),
    ] {
        let Some(path) = files.get(name) else {
            continue;
        };
        if !allowed {
            planner.add(
                "claude",
                parent,
                ".claude.json",
                &Payload::original(path),
                kind,
                (false, true),
            )?;
            continue;
        }
        let merged = (|| -> Result<()> {
            if root.is_none() {
                let dest = checked_dest(parent, ".claude.json")?;
                let value = if dest.exists() {
                    serde_json::from_slice(&bounded_bytes(&dest)?)?
                } else {
                    serde_json::json!({})
                };
                root = Some(value);
            }
            let object = root
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| anyhow::anyhow!("invalid global config"))?;
            let incoming: serde_json::Value = serde_json::from_slice(&bounded_bytes(path)?)?;
            if key == "mcpServers" {
                let target = object
                    .entry(key)
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("invalid MCP config"))?;
                for (name, value) in incoming
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("invalid MCP payload"))?
                {
                    if request.overwrite || !target.contains_key(name) {
                        target.insert(name.clone(), value.clone());
                        changes += 1;
                    }
                }
            } else if request.overwrite || !object.contains_key(key) {
                object.insert(key.into(), incoming);
                changes += 1;
            }
            if kind == FileClass::ExecutesCode || class != FileClass::ExecutesCode {
                class = kind;
            }
            Ok(())
        })();
        if merged.is_err() {
            planner.refuse(
                "claude",
                "existing .claude.json is invalid or unsafe; merge refused",
            );
            return Ok(());
        }
    }
    if let Some(root) = root {
        let payload = planner.generated(&serde_json::to_vec_pretty(&root)?, changes)?;
        planner.add(
            "claude",
            parent,
            ".claude.json",
            &payload,
            class,
            (false, true),
        )?;
    }
    Ok(())
}

pub fn apply_restore(prepared: PreparedRestore) -> Result<RestoreReport> {
    apply_with_journal(prepared, crate::journal::Journal::new)
}

fn apply_with_journal(
    prepared: PreparedRestore,
    new_journal: impl FnOnce(&std::path::Path) -> Result<crate::journal::Journal>,
) -> Result<RestoreReport> {
    if prepared.plan.has_refusals() {
        bail!("restore plan contains refusals")
    }
    let staging = prepared
        .staging
        .ok_or_else(|| anyhow::anyhow!("missing restore staging"))?;
    let execution: Vec<Execution> =
        serde_json::from_slice(&std::fs::read(staging.path().join("execution.json"))?)?;
    let mut report = RestoreReport {
        journal_id: None,
        written: 0,
        merged: 0,
        skipped: prepared.plan.summary().skip,
        failed: Vec::new(),
    };
    if execution.is_empty() {
        return Ok(report);
    }
    for op in &execution {
        if !prepared.plan.ops.iter().any(|planned| {
            planned.dest == op.dest
                && matches!(
                    planned.action,
                    crate::plan::Action::Create
                        | crate::plan::Action::Overwrite
                        | crate::plan::Action::Merge
                )
        }) {
            bail!("execution does not match approved plan")
        }
    }
    let mut journal = new_journal(&prepared.request.archive)?;
    report.journal_id = Some(journal.id().into());
    for op in execution {
        let result = (|| -> Result<()> {
            let destination = checked_dest(&op.base, &op.rel)?;
            if destination != op.dest {
                bail!("destination differs from plan")
            }
            let current = if op.dest.exists() {
                Some(crate::backup::hash_file(&op.dest)?.0)
            } else {
                None
            };
            if current != op.before {
                bail!("destination changed after planning")
            }
            let source = crate::safety::safe_join(staging.path(), &op.payload)?;
            let index = journal.before_hash(&op.dest, crate::backup::hash_file(&source)?.0)?;
            crate::safety::atomic_write_from(
                &op.base,
                &op.rel,
                &mut std::fs::File::open(source)?,
                if current.is_some() {
                    crate::safety::WriteMode::Replace
                } else {
                    crate::safety::WriteMode::CreateNew
                },
            )?;
            journal.complete(index)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                report.written += 1;
                if prepared
                    .plan
                    .ops
                    .iter()
                    .any(|p| p.dest == op.dest && p.action == crate::plan::Action::Merge)
                {
                    report.merged += 1;
                }
            }
            Err(e) => report.failed.push(format!("{}: {e}", op.dest.display())),
        }
    }
    Ok(report)
}

pub fn list_journal() -> Result<Vec<JournalEntry>> {
    crate::journal::list()
}
pub fn plan_rollback(id: &str) -> Result<Plan> {
    crate::journal::plan(id)
}
pub fn rollback(id: &str) -> Result<RestoreReport> {
    crate::journal::rollback(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{atomic_write, WriteMode};
    use std::path::Path;

    fn backup_request(root: &Path) -> BackupRequest {
        BackupRequest {
            output: root.join("backup.tar.gz"),
            overwrite: false,
            claude: Some(ClaudeSelection {
                claude_dir: root.join("source/.claude"),
                session_ids: None,
                match_path: None,
                include_extras: true,
                include_config: true,
                include_credentials: false,
            }),
            providers: Vec::new(),
            passphrase: Passphrase::None,
            redact: false,
        }
    }
    fn restore_request(root: &Path) -> RestoreRequest {
        RestoreRequest {
            archive: root.join("backup.tar.gz"),
            passphrase: Passphrase::None,
            claude_dir: root.join("destination/.claude"),
            provider_homes: BTreeMap::new(),
            only_providers: None,
            only_sessions: None,
            only_items: None,
            mappings: BTreeMap::from([("/old/project".into(), "/new/project".into())]),
            rewrite_tool_paths: false,
            restore_history: false,
            apply_config: true,
            overwrite: false,
            allow_exec_config: false,
            allow_credentials: false,
            allow_running: true,
            skip_verify: false,
        }
    }
    fn fixture(root: &Path) {
        let home = root.join("source/.claude");
        for (rel, bytes) in [
            (
                "projects/-old-project/session.jsonl",
                br#"{"cwd":"/old/project","type":"user","message":{"content":"hi"}}"#.as_slice(),
            ),
            (
                "projects/-old-project/session/subagents/agent.jsonl",
                br#"{"cwd":"/old/project","type":"assistant"}"#.as_slice(),
            ),
            (
                "projects/-old-project/session/tool-results/raw.bin",
                b"\x00\xffunchanged".as_slice(),
            ),
            (
                "projects/-old-project/session/workflows/task.txt",
                b"workflow".as_slice(),
            ),
            (
                "projects/-old-project/memory/MEMORY.md",
                b"memory".as_slice(),
            ),
            (
                "session-env/session/nested/env.json",
                br#"{"PWD":"/old/project"}"#.as_slice(),
            ),
        ] {
            atomic_write(&home, rel, bytes, WriteMode::CreateNew).unwrap();
        }
    }
    fn tree(root: &Path) -> BTreeMap<String, String> {
        if !root.exists() {
            return BTreeMap::new();
        }
        walkdir::WalkDir::new(root)
            .follow_root_links(false)
            .into_iter()
            .map(|e| e.unwrap())
            .map(|e| {
                (
                    e.path().strip_prefix(root).unwrap().display().to_string(),
                    if e.file_type().is_file() {
                        crate::backup::hash_file(e.path()).unwrap().0
                    } else {
                        "directory".into()
                    },
                )
            })
            .collect()
    }
    #[test]
    fn completeness_and_dry_run_preserve_destination_tree() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let request = backup_request(temp.path());
        let plan = plan_backup(&request).unwrap();
        assert!(!request.output.exists());
        assert_eq!(plan.ops.len(), 1);
        let report = run_backup(&request).unwrap();
        assert_eq!(report.sessions, 1);
        assert_eq!(report.files, 6);
        assert_eq!(
            verify_archive(&request.output, &Passphrase::None)
                .unwrap()
                .checked,
            6
        );
        let restore = restore_request(temp.path());
        atomic_write(
            &restore.claude_dir,
            "existing",
            b"keep",
            WriteMode::CreateNew,
        )
        .unwrap();
        let before = tree(&restore.claude_dir);
        let prepared = prepare_restore(restore).unwrap();
        assert_eq!(before, tree(&prepared.request.claude_dir));
        assert!(!prepared.plan.has_refusals());
        assert_eq!(prepared.plan.summary().create, 6);
        let execution: Vec<Execution> = serde_json::from_slice(
            &std::fs::read(
                prepared
                    .staging
                    .as_ref()
                    .unwrap()
                    .path()
                    .join("execution.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let agent = execution
            .iter()
            .find(|e| e.dest.ends_with("session/subagents/agent.jsonl"))
            .unwrap();
        let bytes = std::fs::read(
            prepared
                .staging
                .as_ref()
                .unwrap()
                .path()
                .join(&agent.payload),
        )
        .unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("/new/project"));
        assert!(execution
            .iter()
            .any(|e| e.dest.ends_with("session-env/session/nested/env.json")));
    }
    #[test]
    fn executable_config_and_corrupt_global_config_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let backup = backup_request(temp.path());
        let home = &backup.claude.as_ref().unwrap().claude_dir;
        atomic_write(
            home,
            "settings.json",
            br#"{"hooks":{"Start":[{"command":"danger"}]}}"#,
            WriteMode::CreateNew,
        )
        .unwrap();
        atomic_write(
            home.parent().unwrap(),
            ".claude.json",
            br#"{"mcpServers":{"demo":{"command":"test"}}}"#,
            WriteMode::CreateNew,
        )
        .unwrap();
        run_backup(&backup).unwrap();
        let mut restore = restore_request(temp.path());
        let prepared = prepare_restore(restore.clone()).unwrap();
        assert!(!prepared.plan.has_refusals());
        assert!(prepared.plan.ops.iter().any(|op| op.risk == crate::plan::Risk::ExecutesCode && matches!(&op.action, crate::plan::Action::Skip(reason) if reason == "requires --allow-exec-config")));
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("journal"), archive)
        })
        .unwrap();
        assert_eq!(report.written, 6);
        assert_eq!(report.skipped, 2);
        assert!(report.failed.is_empty());
        assert!(!restore.claude_dir.join("settings.json").exists());
        restore.allow_exec_config = true;
        atomic_write(
            restore.claude_dir.parent().unwrap(),
            ".claude.json",
            b"invalid JSON",
            WriteMode::CreateNew,
        )
        .unwrap();
        let prepared = prepare_restore(restore.clone()).unwrap();
        assert!(prepared.plan.has_refusals());
        assert_eq!(
            std::fs::read(restore.claude_dir.parent().unwrap().join(".claude.json")).unwrap(),
            b"invalid JSON"
        );
        assert!(apply_restore(prepared).is_err());
    }
    #[test]
    fn credentials_without_encryption_are_rejected_before_output() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = backup_request(temp.path());
        request.claude.as_mut().unwrap().include_credentials = true;
        assert!(run_backup(&request).is_err());
        assert!(!request.output.exists());
    }
    #[test]
    fn provider_only_manifest_and_rewrite_work() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        atomic_write(
            &home,
            "sessions/2026/rollout.jsonl",
            br#"{"type":"session_meta","payload":{"id":"sample","cwd":"/old/project"}}"#,
            WriteMode::CreateNew,
        )
        .unwrap();
        let mut backup = backup_request(temp.path());
        backup.claude = None;
        backup.providers.push(ProviderSelection {
            provider: ProviderId::Codex,
            home,
            item_ids: None,
            include_state: false,
            include_credentials: false,
        });
        let report = run_backup(&backup).unwrap();
        assert_eq!(report.sessions, 0);
        assert_eq!(report.provider_items, 1);
        assert!(verify_archive(&backup.output, &Passphrase::None)
            .unwrap()
            .ok());
        let mut restore = restore_request(temp.path());
        restore
            .provider_homes
            .insert(ProviderId::Codex, temp.path().join("restored-codex"));
        let prepared = prepare_restore(restore).unwrap();
        assert!(!prepared.plan.has_refusals());
        assert_eq!(prepared.plan.summary().create, 1);
        assert_eq!(prepared.plan.ops[0].rewrites, 1);
    }
    #[test]
    fn apply_v2_restore_and_journal_without_touching_system_data() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let backup = backup_request(temp.path());
        let inputs = collect(&backup).unwrap();
        let mut manifest = inputs.manifest;
        manifest.schema_version = 2;
        crate::archive::write(
            &backup.output,
            false,
            &manifest,
            &inputs.files,
            &Passphrase::None,
        )
        .unwrap();
        let mut restore = restore_request(temp.path());
        restore.restore_history = true;
        let destination = restore.claude_dir.clone();
        let prepared = prepare_restore(restore).unwrap();
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("isolated-data"), archive)
        })
        .unwrap();
        assert_eq!(report.written, 7);
        assert!(report.failed.is_empty());
        assert!(report.journal_id.is_some());
        let agent = std::fs::read_to_string(
            destination.join("projects/-new-project/session/subagents/agent.jsonl"),
        )
        .unwrap();
        assert!(agent.contains("/new/project"));
        assert_eq!(
            std::fs::read(destination.join("projects/-new-project/session/tool-results/raw.bin"))
                .unwrap(),
            b"\x00\xffunchanged"
        );
        assert!(destination.join("history.jsonl").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(destination.join("history.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(destination.join("projects/-new-project/session"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn destination_changed_after_plan_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        run_backup(&backup_request(temp.path())).unwrap();
        let prepared = prepare_restore(restore_request(temp.path())).unwrap();
        let dest = prepared.plan.ops[0].dest.clone();
        atomic_write(
            dest.parent().unwrap(),
            dest.file_name().unwrap().to_str().unwrap(),
            b"concurrent edit",
            WriteMode::CreateNew,
        )
        .unwrap();
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("isolated-data"), archive)
        })
        .unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(std::fs::read(dest).unwrap(), b"concurrent edit");
    }

    #[test]
    fn encryption_detected_by_magic_and_credentials_stay_gated() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let mut backup = backup_request(temp.path());
        backup.passphrase = Passphrase::Value("fixture passphrase".into());
        backup.claude.as_mut().unwrap().include_credentials = true;
        let home = &backup.claude.as_ref().unwrap().claude_dir;
        atomic_write(
            home.parent().unwrap(),
            ".claude.json",
            br#"{"mcpServers":{"demo":{"command":"test"}},"oauthAccount":{"token":"synthetic"}}"#,
            WriteMode::CreateNew,
        )
        .unwrap();
        let report = run_backup(&backup).unwrap();
        assert!(report.encrypted);
        let verify = verify_archive(&backup.output, &backup.passphrase).unwrap();
        assert!(verify.ok());
        assert!(verify.encrypted);
        let mut restore = restore_request(temp.path());
        restore.passphrase = backup.passphrase;
        restore.allow_exec_config = true;
        assert!(prepare_restore(restore.clone()).unwrap().plan.ops.iter().any(|op| matches!(&op.action, crate::plan::Action::Skip(reason) if reason == "requires --allow-credentials")));
        restore.allow_credentials = true;
        let prepared = prepare_restore(restore.clone()).unwrap();
        assert!(!prepared.plan.has_refusals());
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("isolated-data"), archive)
        })
        .unwrap();
        assert!(report.failed.is_empty());
        let global: serde_json::Value = serde_json::from_slice(
            &std::fs::read(restore.claude_dir.parent().unwrap().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert!(global.get("mcpServers").is_some());
        assert_eq!(global["oauthAccount"]["token"], "synthetic");
    }

    #[cfg(unix)]
    #[test]
    fn remapped_destination_with_symlink_below_base_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        run_backup(&backup_request(temp.path())).unwrap();
        let restore = restore_request(temp.path());
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir_all(&restore.claude_dir).unwrap();
        std::os::unix::fs::symlink(&outside, restore.claude_dir.join("projects")).unwrap();
        let prepared = prepare_restore(restore).unwrap();
        assert!(prepared.plan.has_refusals());
        assert!(tree(&outside).len() == 1);
    }
    #[test]
    fn tool_results_are_not_config_and_unchanged_payloads_are_not_duplicated() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let backup = backup_request(temp.path());
        let home = &backup.claude.as_ref().unwrap().claude_dir;
        for (name, bytes) in [
            ("mcp-x.txt", b"mcp result".as_slice()),
            (
                "oauth-credential.json",
                br#"{"hooks":{"example":"ordinary tool output"}}"#.as_slice(),
            ),
        ] {
            atomic_write(
                home,
                &format!("projects/-old-project/session/tool-results/{name}"),
                bytes,
                WriteMode::CreateNew,
            )
            .unwrap();
        }
        run_backup(&backup).unwrap();
        let prepared = prepare_restore(restore_request(temp.path())).unwrap();
        assert!(!prepared.plan.has_refusals());
        assert_eq!(prepared.plan.summary().skip, 0);
        let execution: Vec<Execution> = serde_json::from_reader(
            std::fs::File::open(
                prepared
                    .staging
                    .as_ref()
                    .unwrap()
                    .path()
                    .join("execution.json"),
            )
            .unwrap(),
        )
        .unwrap();
        for name in ["mcp-x.txt", "oauth-credential.json", "raw.bin"] {
            let op = execution.iter().find(|op| op.dest.ends_with(name)).unwrap();
            assert_eq!(
                op.payload,
                format!("sessions/session/project/tool-results/{name}")
            );
        }
        assert!(!prepared
            .staging
            .as_ref()
            .unwrap()
            .path()
            .join("execution")
            .exists());
        let destination = prepared.request.claude_dir.clone();
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("journal"), archive)
        })
        .unwrap();
        assert!(report.failed.is_empty());
        assert_eq!(report.written, 8);
        assert_eq!(
            std::fs::read(destination.join("projects/-new-project/session/tool-results/mcp-x.txt"))
                .unwrap(),
            b"mcp result"
        );
    }

    #[test]
    fn empty_cwd_and_invalid_session_metadata_skip_without_blocking_valid_sessions() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let backup = backup_request(temp.path());
        let inputs = collect(&backup).unwrap();
        let mut manifest = inputs.manifest;
        let mut empty = manifest.sessions[0].clone();
        empty.id = "empty".into();
        empty.cwd.clear();
        empty.artifacts.clear();
        let file = temp.path().join("empty.jsonl");
        std::fs::write(&file, b"{}\n").unwrap();
        let (sha256, size) = crate::backup::hash_file(&file).unwrap();
        empty.artifacts.push(crate::model::Artifact {
            kind: crate::model::ArtifactKind::Session,
            logical_name: "empty.jsonl".into(),
            sha256,
            size,
        });
        let mut invalid = empty.clone();
        invalid.id = "../invalid".into();
        invalid.artifacts.clear();
        manifest.sessions.extend([empty, invalid]);
        manifest.checksum = crate::backup::global_checksum(&manifest.sessions);
        let mut files = inputs.files;
        files.push(("sessions/empty/empty.jsonl".into(), file));
        crate::archive::write(&backup.output, false, &manifest, &files, &Passphrase::None).unwrap();
        let prepared = prepare_restore(restore_request(temp.path())).unwrap();
        assert!(!prepared.plan.has_refusals());
        assert_eq!(prepared.plan.summary().skip, 2);
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("journal"), archive)
        })
        .unwrap();
        assert_eq!(report.written, 6);
        assert_eq!(report.skipped, 2);
    }

    #[test]
    fn invalid_utf8_and_empty_mappings_preserve_jsonl_bytes_exactly() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let backup = backup_request(temp.path());
        let home = &backup.claude.as_ref().unwrap().claude_dir;
        let invalid = b"{\"cwd\":\"/old/project\"}\r\n\xffbad\n";
        atomic_write(
            home,
            "projects/-old-project/session/subagents/invalid.jsonl",
            invalid,
            WriteMode::CreateNew,
        )
        .unwrap();
        run_backup(&backup).unwrap();
        for empty in [false, true] {
            let mut request = restore_request(temp.path());
            if empty {
                request.mappings.clear();
            }
            let prepared = prepare_restore(request).unwrap();
            assert!(prepared
                .plan
                .warnings
                .iter()
                .any(|w| w.contains("Invalid UTF-8")));
            let execution: Vec<Execution> = serde_json::from_reader(
                std::fs::File::open(
                    prepared
                        .staging
                        .as_ref()
                        .unwrap()
                        .path()
                        .join("execution.json"),
                )
                .unwrap(),
            )
            .unwrap();
            let op = execution
                .iter()
                .find(|op| op.dest.ends_with("invalid.jsonl"))
                .unwrap();
            assert_eq!(
                op.payload,
                "sessions/session/project/subagents/invalid.jsonl"
            );
            assert_eq!(
                std::fs::read(prepared.staging.as_ref().unwrap().path().join(&op.payload)).unwrap(),
                invalid
            );
            if empty {
                assert!(execution
                    .iter()
                    .all(|op| op.payload.starts_with("sessions/")));
                assert!(prepared.plan.ops.iter().all(|op| op.rewrites == 0));
            } else {
                assert_eq!(
                    prepared
                        .plan
                        .ops
                        .iter()
                        .find(|op| op.dest.ends_with("session.jsonl"))
                        .unwrap()
                        .rewrites,
                    1
                );
                assert_eq!(
                    prepared
                        .plan
                        .ops
                        .iter()
                        .find(|op| op.dest.ends_with("agent.jsonl"))
                        .unwrap()
                        .rewrites,
                    1
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_base_is_supported_but_descendant_symlinks_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let backup = backup_request(temp.path());
        let real = temp.path().join("real-base");
        std::fs::create_dir(&real).unwrap();
        let alias = temp.path().join("base-link");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        std::fs::write(real.join("leaf"), b"safe").unwrap();
        assert!(crate::archive::ensure_regular(&alias.join("leaf")).is_ok());
        run_backup(&backup).unwrap();
        let mut request = restore_request(temp.path());
        request.claude_dir = alias.clone();
        let prepared = prepare_restore(request.clone()).unwrap();
        assert!(!prepared.plan.has_refusals());
        let report = apply_with_journal(prepared, |archive| {
            crate::journal::Journal::at(temp.path().join("journal"), archive)
        })
        .unwrap();
        assert!(report.failed.is_empty());
        assert!(real.join("projects/-new-project/session.jsonl").exists());
        std::os::unix::fs::symlink(&real, alias.join("nested-link")).unwrap();
        assert!(checked_dest(&alias, "nested-link/leaf").is_err());
        assert!(crate::archive::ensure_regular(&alias.join("nested-link")).is_err());
    }

    struct FakeProvider;
    impl crate::provider::Provider for FakeProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Codex
        }
        fn display_name(&self) -> &'static str {
            "Synthetic"
        }
        fn locate(&self, _: &crate::provider::LocateEnv) -> Option<PathBuf> {
            None
        }
        fn scan(
            &self,
            _: &Path,
            _: &crate::provider::ScanOptions,
        ) -> Result<crate::provider::Inventory> {
            bail!("unused fixture scan")
        }
        fn classify(&self, _: &str) -> crate::provider::FileClass {
            crate::provider::FileClass::Plain
        }
        fn rewrite(
            &self,
            rel: &str,
            bytes: &[u8],
            _: &BTreeMap<String, String>,
        ) -> Result<crate::provider::Rewritten> {
            assert_ne!(rel, "state.db", "opaque state must never be rewritten");
            if rel == "bad.json" {
                serde_json::from_slice::<serde_json::Value>(bytes)?;
            }
            if rel == "unsafe.json" {
                return Ok(crate::provider::Rewritten {
                    rel: "../escape".into(),
                    bytes: None,
                    changes: 1,
                });
            }
            Ok(crate::provider::Rewritten {
                rel: rel.into(),
                bytes: Some(b"rewritten".to_vec()),
                changes: 3,
            })
        }
    }
    fn archived_file(rel: &str, class: crate::provider::FileClass) -> crate::model::ArchivedFile {
        crate::model::ArchivedFile {
            file: crate::provider::ItemFile {
                rel: rel.into(),
                size: 0,
                class,
            },
            sha256: String::new(),
        }
    }

    #[test]
    fn provider_errors_are_file_scoped_opaque_bytes_bypass_rewrite_and_counts_propagate() {
        use crate::provider::FileClass;
        let temp = tempfile::tempdir().unwrap();
        let request = restore_request(temp.path());
        let staging = tempfile::tempdir().unwrap();
        let mut planner = Planner {
            request: &request,
            staging: staging.path(),
            plan: Plan::default(),
            execution: Vec::new(),
        };
        let home = temp.path().join("provider");
        for (rel, class) in [
            ("bad.json", FileClass::Plain),
            ("good.json", FileClass::Plain),
            ("state.db", FileClass::OpaqueState),
            ("unsafe.json", FileClass::Plain),
        ] {
            let path = staging.path().join(rel);
            std::fs::write(&path, b"invalid json").unwrap();
            planner
                .provider_file(&FakeProvider, &archived_file(rel, class), &path, &home)
                .unwrap();
        }
        assert_eq!(planner.plan.ops.len(), 4);
        assert_eq!(planner.plan.summary().refuse, 2);
        assert_eq!(planner.plan.summary().create, 2);
        assert!(matches!(
            planner.plan.ops[0].action,
            crate::plan::Action::Refuse(_)
        ));
        assert_eq!(planner.plan.ops[0].dest, home.join("bad.json"));
        assert_eq!(planner.plan.ops[1].rewrites, 3);
        assert_eq!(
            planner
                .execution
                .iter()
                .find(|op| op.dest.ends_with("state.db"))
                .unwrap()
                .payload,
            "state.db"
        );
    }

    #[test]
    fn policy_skips_keep_risk_and_allow_other_provider_files() {
        use crate::provider::FileClass;
        let temp = tempfile::tempdir().unwrap();
        let request = restore_request(temp.path());
        let staging = tempfile::tempdir().unwrap();
        let mut planner = Planner {
            request: &request,
            staging: staging.path(),
            plan: Plan::default(),
            execution: Vec::new(),
        };
        let home = temp.path().join("provider");
        for (rel, class) in [
            ("config.toml", FileClass::ExecutesCode),
            ("auth.json", FileClass::Credential),
            ("mcp-oauth-session.json", FileClass::Plain),
        ] {
            let path = staging.path().join(rel);
            std::fs::write(&path, br#"{"hooks":{}}"#).unwrap();
            planner
                .provider_file(&FakeProvider, &archived_file(rel, class), &path, &home)
                .unwrap();
        }
        assert!(!planner.plan.has_refusals());
        assert_eq!(planner.plan.summary().skip, 2);
        assert_eq!(planner.plan.ops[0].risk, crate::plan::Risk::ExecutesCode);
        assert_eq!(planner.plan.ops[1].risk, crate::plan::Risk::Sensitive);
        assert_eq!(planner.plan.ops[2].risk, crate::plan::Risk::Normal);
        assert_eq!(planner.execution.len(), 1);
    }

    #[test]
    fn lowered_rewrite_bound_skips_only_large_file_with_warning() {
        let temp = tempfile::tempdir().unwrap();
        let request = restore_request(temp.path());
        let staging = tempfile::tempdir().unwrap();
        let mut planner = Planner {
            request: &request,
            staging: staging.path(),
            plan: Plan::default(),
            execution: Vec::new(),
        };
        let home = temp.path().join("provider");
        let large = staging.path().join("large.json");
        let small = staging.path().join("small.json");
        std::fs::write(&large, [b'x'; 128]).unwrap();
        std::fs::write(&small, b"{}").unwrap();
        planner
            .provider_file_with_limit(
                &FakeProvider,
                &archived_file("large.json", crate::provider::FileClass::Plain),
                &large,
                &home,
                64,
            )
            .unwrap();
        planner
            .provider_file_with_limit(
                &FakeProvider,
                &archived_file("small.json", crate::provider::FileClass::Plain),
                &small,
                &home,
                64,
            )
            .unwrap();
        assert!(
            matches!(&planner.plan.ops[0].action, crate::plan::Action::Skip(reason) if reason == "too large to remap")
        );
        assert!(!planner.plan.has_refusals());
        assert_eq!(planner.plan.summary().create, 1);
        assert!(planner
            .plan
            .warnings
            .iter()
            .any(|w| w.contains("too large to remap")));
        assert_eq!(MAX_STRUCTURED_BYTES, 1024 * 1024 * 1024);
    }
    fn no_snapshots(root: &Path) {
        assert!(std::fs::read_dir(root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".aisession-backup-snapshot-")));
    }

    #[test]
    fn live_append_backup_snapshots_verify_and_report_changes() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        let rel = "sessions/2026/live.jsonl";
        atomic_write(
            &home,
            rel,
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"live\",\"cwd\":\"/synthetic\"}}\n",
            WriteMode::CreateNew,
        )
        .unwrap();
        let path = home.join(rel);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&vec![b'\n'; 16 * 1024 * 1024])
            .unwrap();
        let mut request = backup_request(temp.path());
        request.claude = None;
        request.providers.push(ProviderSelection {
            provider: ProviderId::Codex,
            home,
            item_ids: None,
            include_state: false,
            include_credentials: false,
        });
        let stop = AtomicBool::new(false);
        let barrier = std::sync::Barrier::new(2);
        let report = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                file.write_all(b"{}\n").unwrap();
                barrier.wait();
                while !stop.load(Ordering::Relaxed) {
                    file.write_all(b"{}\n").unwrap();
                }
            });
            barrier.wait();
            let result = run_backup(&request);
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            result.unwrap()
        });
        assert!(
            report
                .changed_during_backup
                .contains(&format!("providers/codex/{rel}")),
            "{:?}",
            report.changed_during_backup
        );
        assert!(verify_archive(&request.output, &Passphrase::None)
            .unwrap()
            .ok());
        let manifest = read_manifest(&request.output, &Passphrase::None).unwrap();
        let record = &manifest.providers[0];
        assert_eq!(record.items[0].files[0].size, record.files[0].size);
        assert_eq!(record.items[0].files[0].sha256, record.files[0].sha256);
        assert_eq!(manifest.total_size, report.bytes);
        no_snapshots(temp.path());
    }

    #[test]
    fn file_deleted_after_scan_is_dropped_and_reported() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let request = backup_request(temp.path());
        let vanished = "sessions/session/project/tool-results/raw.bin";
        let report = run_backup_after_scan(&request, |input| {
            let (_, path) = input.files.iter().find(|(rel, _)| rel == vanished).unwrap();
            std::fs::remove_file(path)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(report.files, 5);
        assert_eq!(report.changed_during_backup, [vanished]);
        assert!(verify_archive(&request.output, &Passphrase::None)
            .unwrap()
            .ok());
        let manifest = read_manifest(&request.output, &Passphrase::None).unwrap();
        assert!(!manifest.sessions[0]
            .artifacts
            .iter()
            .any(|a| a.logical_name == "tool-results/raw.bin"));
        no_snapshots(temp.path());
    }

    #[test]
    fn vanished_main_session_removes_orphan_artifacts_and_snapshot_errors_clean_up() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let request = backup_request(temp.path());
        let report = run_backup_after_scan(&request, |input| {
            std::fs::remove_file(
                &input
                    .files
                    .iter()
                    .find(|(rel, _)| rel == "sessions/session/session.jsonl")
                    .unwrap()
                    .1,
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(report.sessions, 0);
        assert_eq!(report.files, 0);
        assert!(verify_archive(&request.output, &Passphrase::None)
            .unwrap()
            .ok());
        assert!(report
            .changed_during_backup
            .contains(&"sessions/session/session.jsonl".into()));
        no_snapshots(temp.path());
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let request = backup_request(temp.path());
        assert!(run_backup_after_scan(&request, |input| {
            let path = &input.files[0].1;
            std::fs::remove_file(path)?;
            std::fs::create_dir(path)?;
            Ok(())
        })
        .is_err());
        assert!(!request.output.exists());
        no_snapshots(temp.path());
    }

    #[test]
    fn credential_audit_without_passphrase_is_read_only_and_reports_counts() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let mut request = backup_request(temp.path());
        request.claude.as_mut().unwrap().include_credentials = true;
        let home = &request.claude.as_ref().unwrap().claude_dir;
        atomic_write(
            home,
            ".credentials.json",
            br#"{"token":"ghp_abcdefghijklmnopqrstuvwxyz"}"#,
            WriteMode::CreateNew,
        )
        .unwrap();
        let before = tree(temp.path());
        let findings = audit_selection(&request).unwrap();
        assert!(findings
            .iter()
            .any(|finding| finding.detector == "github" && finding.count == 1));
        assert_eq!(tree(temp.path()), before);
        assert!(!request.output.exists());
        assert!(run_backup(&request).is_err());
    }

    #[test]
    fn settings_jsonc_and_nested_config_keys_are_executable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("payload");
        std::fs::write(&path, b"// JSONC\n{\"nested\": {\"hooks\": {}}}").unwrap();
        for name in [
            "settings.json",
            "settings.local.json",
            "nested/settings.json",
            "nested/settings.local.json",
        ] {
            assert_eq!(
                config_class(name, &path).unwrap(),
                crate::provider::FileClass::ExecutesCode
            );
        }
        std::fs::write(&path, br#"{"nested":[{"apiKeyHelper":"example"}]}"#).unwrap();
        assert_eq!(
            config_class("other.json", &path).unwrap(),
            crate::provider::FileClass::ExecutesCode
        );
        std::fs::write(&path, br#"{"nested":{"hooksExample":"plain"}}"#).unwrap();
        assert_eq!(
            config_class("other.json", &path).unwrap(),
            crate::provider::FileClass::Plain
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_walk_roots_never_archive_outside_files() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let request = backup_request(temp.path());
        let home = &request.claude.as_ref().unwrap().claude_dir;
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("outside-only.txt"), b"outside fixture").unwrap();
        let memory = home.join("projects/-old-project/memory");
        std::fs::rename(&memory, home.join("old-memory")).unwrap();
        std::os::unix::fs::symlink(&outside, &memory).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("plans")).unwrap();
        let selected = crate::discover::load_sessions(home).unwrap();
        assert!(!selected[0]
            .companions
            .iter()
            .any(|(_, path)| path.ends_with("outside-only.txt")));
        run_backup(&request).unwrap();
        let staged = crate::archive::stage(&request.output, &Passphrase::None).unwrap();
        assert!(!staged.files.keys().any(|rel| rel.contains("outside-only")));
        assert!(staged.manifest.extras.is_empty());
    }
    #[test]
    fn engine_global_config_merge_preserves_keys_and_honors_overwrite() {
        for overwrite in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            fixture(temp.path());
            let backup = backup_request(temp.path());
            let home = &backup.claude.as_ref().unwrap().claude_dir;
            atomic_write(
                home.parent().unwrap(),
                ".claude.json",
                br#"{"mcpServers":{"existing":{"command":"new"},"added":{"command":"fixture"}}}"#,
                WriteMode::CreateNew,
            )
            .unwrap();
            run_backup(&backup).unwrap();
            let mut request = restore_request(temp.path());
            request.allow_exec_config = true;
            request.overwrite = overwrite;
            let dest = request.claude_dir.parent().unwrap().to_path_buf();
            atomic_write(
                &dest,
                ".claude.json",
                br#"{"theme":"dark","mcpServers":{"existing":{"command":"old"}}}"#,
                WriteMode::CreateNew,
            )
            .unwrap();
            let report = apply_with_journal(prepare_restore(request).unwrap(), |archive| {
                crate::journal::Journal::at(temp.path().join("journal"), archive)
            })
            .unwrap();
            assert!(report.failed.is_empty());
            assert_eq!(report.merged, 1);
            let value: serde_json::Value =
                serde_json::from_reader(std::fs::File::open(dest.join(".claude.json")).unwrap())
                    .unwrap();
            assert_eq!(value["theme"], "dark");
            assert_eq!(
                value["mcpServers"]["existing"]["command"],
                if overwrite { "new" } else { "old" }
            );
            assert_eq!(value["mcpServers"]["added"]["command"], "fixture");
        }
    }
}
