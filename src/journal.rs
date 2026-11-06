use crate::{
    engine::{JournalEntry, RestoreReport},
    plan::{Action, Plan, PlannedOp, Risk},
    safety::{self, WriteMode},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct Record {
    dest: PathBuf,
    before: Option<String>,
    after: String,
    done: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Journal {
    id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    archive: String,
    records: Vec<Record>,
    rolled_back: bool,
    #[serde(skip)]
    root: PathBuf,
}

fn root() -> Result<PathBuf> {
    Ok(crate::archive::data_dir()?.join("aisession-backup/journal"))
}

fn legacy_root() -> Result<PathBuf> {
    Ok(crate::archive::data_dir()?.join("claude-migrate/journal"))
}

#[cfg(test)]
fn write_path(path: &Path, bytes: &[u8], mode: WriteMode) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid filename")?;
    safety::atomic_write(parent, name, bytes, mode)
}

impl Journal {
    pub fn new(archive: &Path) -> Result<Self> {
        Self::at(root()?, archive)
    }
    pub(crate) fn at(root: PathBuf, archive: &Path) -> Result<Self> {
        let id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%f"),
            std::process::id()
        );
        crate::engine::checked_dest(&root, &id)?;
        let journal = Self {
            root: root.join(&id),
            id,
            created_at: chrono::Utc::now(),
            archive: archive.display().to_string(),
            records: Vec::new(),
            rolled_back: false,
        };
        journal.save()?;
        Ok(journal)
    }
    fn save(&self) -> Result<()> {
        safety::atomic_write(
            &self.root,
            "journal.json",
            &serde_json::to_vec(self)?,
            WriteMode::Replace,
        )
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn before(&mut self, dest: &Path, after: &[u8]) -> Result<usize> {
        use sha2::Digest;
        self.before_hash(dest, hex::encode(sha2::Sha256::digest(after)))
    }
    pub fn before_hash(&mut self, dest: &Path, after: String) -> Result<usize> {
        let index = self.records.len();
        let before = if dest.exists() {
            crate::archive::ensure_regular(dest)?;
            let name = format!("preimage-{index}");
            safety::atomic_write_from(
                &self.root,
                &name,
                &mut std::fs::File::open(dest)?,
                WriteMode::CreateNew,
            )?;
            Some(name)
        } else {
            None
        };
        self.records.push(Record {
            dest: dest.to_path_buf(),
            before,
            after,
            done: false,
        });
        self.save()?;
        Ok(index)
    }
    pub fn complete(&mut self, index: usize) -> Result<()> {
        self.records[index].done = true;
        self.save()
    }
    fn entry(&self) -> JournalEntry {
        JournalEntry {
            id: self.id.clone(),
            created_at: self.created_at,
            archive: self.archive.clone(),
            ops: self.records.len(),
            rolled_back: self.rolled_back,
        }
    }
    fn plan(&self) -> Result<Plan> {
        let mut plan = Plan::default();
        for record in self.records.iter().rev() {
            let action = if self.rolled_back {
                Action::Skip("already rolled back".into())
            } else if !record.dest.exists() {
                Action::Skip("file absent".into())
            } else if crate::archive::ensure_regular(&record.dest).is_err() {
                Action::Refuse("unsafe destination".into())
            } else if crate::backup::hash_file(&record.dest)?.0 != record.after {
                Action::Refuse("file changed since restore".into())
            } else {
                Action::Overwrite
            };
            plan.ops.push(PlannedOp {
                provider: "journal".into(),
                dest: record.dest.clone(),
                action,
                risk: Risk::Normal,
                size: 0,
                rewrites: 0,
                note: if record.before.is_some() {
                    "restore pre-image"
                } else {
                    "remove unchanged created file"
                }
                .into(),
            });
        }
        Ok(plan)
    }
    fn rollback(&mut self) -> Result<RestoreReport> {
        let plan = self.plan()?;
        let mut report = RestoreReport {
            journal_id: Some(self.id.clone()),
            written: 0,
            merged: 0,
            skipped: 0,
            failed: Vec::new(),
        };
        for (record, op) in self.records.iter().rev().zip(plan.ops) {
            match op.action {
                Action::Skip(_) => {
                    report.skipped += 1;
                    continue;
                }
                Action::Refuse(reason) => {
                    report.failed.push(reason);
                    continue;
                }
                _ => {}
            }
            let result = if let Some(before) = &record.before {
                let path = safety::safe_join(&self.root, before)?;
                safety::atomic_write_from(
                    record.dest.parent().context("missing parent")?,
                    record
                        .dest
                        .file_name()
                        .and_then(|s| s.to_str())
                        .context("invalid filename")?,
                    &mut std::fs::File::open(path)?,
                    WriteMode::Replace,
                )
                .map(|_| ())
            } else {
                std::fs::remove_file(&record.dest).map_err(Into::into)
            };
            match result {
                Ok(()) => report.written += 1,
                Err(e) => report.failed.push(e.to_string()),
            }
        }
        if report.failed.is_empty() {
            self.rolled_back = true;
            self.save()?;
        }
        Ok(report)
    }
}

fn load(id: &str) -> Result<Journal> {
    safety::validate_id(id)?;
    let current = root()?;
    let legacy = legacy_root()?;
    let base = if !current.join(id).exists() && legacy.join(id).exists() {
        legacy
    } else {
        current
    };
    let root = safety::safe_join(&base, id)?;
    let path = safety::safe_join(&root, "journal.json")?;
    let mut journal: Journal = serde_json::from_reader(std::fs::File::open(path)?)?;
    if journal.id != id {
        bail!("journal identifier mismatch")
    }
    journal.root = root;
    Ok(journal)
}
pub fn list() -> Result<Vec<JournalEntry>> {
    let mut ids = std::collections::BTreeSet::new();
    for root in [root()?, legacy_root()?] {
        if !root.exists() {
            continue;
        }
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let id = entry.file_name();
                ids.insert(id.to_str().context("invalid journal id")?.to_owned());
            }
        }
    }
    let mut entries = Vec::new();
    for id in ids {
        entries.push(load(&id)?.entry());
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    Ok(entries)
}
pub fn plan(id: &str) -> Result<Plan> {
    load(id)?.plan()
}
pub fn rollback(id: &str) -> Result<RestoreReport> {
    load(id)?.rollback()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rollback_preserves_modified_files_and_restores_preimages() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("existing");
        let created = temp.path().join("created");
        std::fs::write(&existing, b"before").unwrap();
        let mut journal = Journal::at(temp.path().join("journal"), Path::new("synthetic")).unwrap();
        for path in [&existing, &created] {
            let index = journal.before(path, b"after").unwrap();
            write_path(path, b"after", WriteMode::Replace).unwrap();
            journal.complete(index).unwrap();
        }
        std::fs::write(&created, b"user edit").unwrap();
        let report = journal.rollback().unwrap();
        assert_eq!(report.written, 1);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(std::fs::read(existing).unwrap(), b"before");
        assert_eq!(std::fs::read(created).unwrap(), b"user edit");
    }
}
