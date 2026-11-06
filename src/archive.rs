use crate::{
    engine::{Passphrase, VerifyReport},
    model::Manifest,
    safety::{self, WriteMode},
};
use anyhow::{bail, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAX_ENTRY_SIZE: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_TOTAL_SIZE: u64 = 64 * 1024 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 1_000_000;

pub fn env_setting(suffix: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("AISESSION_BACKUP_{suffix}"))
        .or_else(|| std::env::var_os(format!("CLAUDE_MIGRATE_{suffix}")))
}

pub fn total_limit() -> Result<u64> {
    let Some(value) = env_setting("MAX_ARCHIVE_BYTES") else {
        return Ok(MAX_TOTAL_SIZE);
    };
    let limit = value
        .to_str()
        .context("AISESSION_BACKUP_MAX_ARCHIVE_BYTES must be valid unicode")?
        .parse::<u64>()
        .context("AISESSION_BACKUP_MAX_ARCHIVE_BYTES must be a positive integer")?;
    if limit == 0 || limit == u64::MAX {
        bail!("invalid archive size limit")
    }
    Ok(limit)
}

pub fn data_dir() -> Result<PathBuf> {
    env_setting("DATA_DIR")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .context("no data directory")
}

pub fn staging_dir() -> Result<tempfile::TempDir> {
    let base = data_dir()?;
    let root = safety::safe_join(&base, "aisession-backup/staging")?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&root)?;
    let mut temp = tempfile::Builder::new();
    temp.prefix("restore-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.permissions(std::fs::Permissions::from_mode(0o700));
    }
    Ok(temp.tempdir_in(root)?)
}

pub(crate) fn report_text(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub struct StagedArchive {
    pub staging: tempfile::TempDir,
    pub manifest: Manifest,
    pub files: BTreeMap<String, PathBuf>,
    pub report: VerifyReport,
}

pub fn stage(path: &Path, passphrase: &Passphrase) -> Result<StagedArchive> {
    stage_in(path, passphrase, staging_dir()?)
}

fn stage_in(
    path: &Path,
    passphrase: &Passphrase,
    staging: tempfile::TempDir,
) -> Result<StagedArchive> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    let limit = total_limit()?;
    let mut input = File::open(path)?;
    let mut magic = [0; crate::crypto::MAGIC.len()];
    let n = input.read(&mut magic)?;
    input.seek(SeekFrom::Start(0))?;
    let encrypted = magic[..n].starts_with(crate::crypto::MAGIC);
    let input = if encrypted {
        let mut plain = tempfile::tempfile_in(staging.path())?;
        crate::crypto::decrypt(input, &mut plain, passphrase)?;
        plain.seek(SeekFrom::Start(0))?;
        plain
    } else {
        input
    };
    let decoder =
        GzDecoder::new(input).take(limit.saturating_add(MAX_ENTRIES as u64 * 1024 + 1024));
    let mut archive = tar::Archive::new(decoder);
    let mut report = VerifyReport {
        encrypted,
        ..Default::default()
    };
    let mut files = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= MAX_ENTRIES {
            bail!("archive exceeds entry count limit")
        }
        let mut entry = entry?;
        let name =
            String::from_utf8(entry.path_bytes().into_owned()).context("non-UTF8 archive path")?;
        let size = entry.size();
        total = total.checked_add(size).context("archive size overflow")?;
        if size > MAX_ENTRY_SIZE {
            bail!("archive entry exceeds maximum entry size ({MAX_ENTRY_SIZE})")
        }
        if total > limit {
            bail!("archive exceeds maximum total size ({limit})")
        }
        if safety::validate_rel(&name).is_err() || !entry.header().entry_type().is_file() {
            report.unsafe_entries.push(name);
            continue;
        }
        if !seen.insert(name.clone()) {
            report
                .unsafe_entries
                .push(format!("duplicate entry: {name}"));
            continue;
        }
        if index == 0 && name != "manifest.json" {
            report
                .unexpected_entries
                .push("manifest.json is not first".into());
        }
        safety::atomic_write_from(staging.path(), &name, &mut entry, WriteMode::CreateNew)?;
        files.insert(name.clone(), staging.path().join(name));
    }
    let manifest: Manifest = serde_json::from_reader(File::open(
        files
            .get("manifest.json")
            .context("missing manifest.json")?,
    )?)?;
    if manifest.schema_version > 3 {
        bail!("unsupported archive schema")
    }
    report.schema_version = manifest.schema_version;
    let mut expected = BTreeMap::new();
    let mut session_ids = BTreeSet::new();
    for session in &manifest.sessions {
        if safety::validate_id(&session.id).is_err() {
            continue;
        }
        if !session_ids.insert(&session.id) {
            report
                .unsafe_entries
                .push("duplicate session identifier".into());
        }
        let main = format!("sessions/{}/{}.jsonl", session.id, session.id);
        if !files.contains_key(&main) {
            report.mismatches.push(format!("{main}: missing session"));
        }
        if manifest.is_v1() {
            for name in [
                format!("{}.jsonl", session.id),
                "env.json".into(),
                "todos.json".into(),
            ] {
                expected.insert(format!("sessions/{}/{name}", session.id), None);
            }
        }
        for artifact in &session.artifacts {
            let name =
                crate::backup::archive_path(artifact.kind, &session.id, &artifact.logical_name);
            if safety::validate_rel(&name).is_err() {
                report.unsafe_entries.push(name);
                continue;
            }
            if expected
                .insert(name.clone(), Some((artifact.sha256.clone(), artifact.size)))
                .is_some()
            {
                report
                    .unsafe_entries
                    .push(format!("duplicate manifest path: {name}"));
            }
        }
    }
    for (prefix, digests) in [("extras", &manifest.extras), ("config", &manifest.config)] {
        for digest in digests {
            expected.insert(
                format!("{prefix}/{}", digest.rel),
                Some((digest.sha256.clone(), digest.size)),
            );
        }
    }
    let mut provider_ids = BTreeSet::new();
    for provider in &manifest.providers {
        if !provider_ids.insert(provider.id) {
            report
                .unsafe_entries
                .push("duplicate provider manifest".into());
        }
        let mut referenced = BTreeSet::new();
        for item in &provider.items {
            for file in &item.files {
                let name = format!("providers/{}/{}", provider.id.as_str(), file.rel);
                if safety::validate_rel(&file.rel).is_err() {
                    report.unsafe_entries.push(name.clone());
                    continue;
                }
                referenced.insert(file.rel.clone());
                if file.sha256.is_empty() {
                    report.mismatches.push(format!("missing digest: {name}"));
                }
                let digest = Some((file.sha256.clone(), file.size));
                if let Some(previous) = expected.insert(name.clone(), digest.clone()) {
                    if previous != digest {
                        report
                            .mismatches
                            .push(format!("conflicting digest: {name}"));
                    }
                }
                if let Some(index) = provider.files.iter().find(|d| d.rel == file.rel) {
                    if index.sha256 != file.sha256 || index.size != file.size {
                        report
                            .mismatches
                            .push(format!("digest index mismatch: {name}"));
                    }
                }
            }
        }
        for digest in &provider.files {
            if safety::validate_rel(&digest.rel).is_err() {
                report.unsafe_entries.push(digest.rel.clone());
            }
            if !referenced.contains(&digest.rel) {
                report
                    .unexpected_entries
                    .push(format!("unreferenced provider digest: {}", digest.rel));
            }
        }
    }
    for (name, digest) in &expected {
        if safety::validate_rel(name).is_err() {
            report.unsafe_entries.push(name.clone());
            continue;
        }
        if let Some((sha, size)) = digest {
            report.checked += 1;
            match files.get(name) {
                Some(path) if crate::backup::hash_file(path)? == (sha.clone(), *size) => {}
                Some(_) => report
                    .mismatches
                    .push(format!("{name}: checksum or size mismatch")),
                None => report.mismatches.push(format!("{name}: missing")),
            }
        }
    }
    for name in files.keys() {
        if name == "manifest.json" || expected.contains_key(name) {
            continue;
        }
        if manifest.schema_version < 3
            && (name.starts_with("extras/") || name.starts_with("config/"))
        {
            continue;
        }
        report.unexpected_entries.push(name.clone());
    }
    if !manifest.is_v1() && !crate::backup::manifest_self_consistent(&manifest) {
        report
            .mismatches
            .push("manifest session checksum mismatch".into());
    }
    for entries in [
        &mut report.mismatches,
        &mut report.unsafe_entries,
        &mut report.unexpected_entries,
    ] {
        for entry in entries {
            *entry = report_text(entry);
        }
    }
    Ok(StagedArchive {
        staging,
        manifest,
        files,
        report,
    })
}

pub fn write(
    output: &Path,
    overwrite: bool,
    manifest: &Manifest,
    files: &[(String, PathBuf)],
    passphrase: &Passphrase,
) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let rel = output
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid output filename")?;
    crate::engine::checked_dest(parent, rel)?;
    if output.exists() && !overwrite {
        bail!("output already exists")
    }
    let mut compressed = tempfile::NamedTempFile::new_in(parent)?;
    {
        let encoder = GzEncoder::new(&mut compressed, Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        append(&mut tar, "manifest.json", bytes.len() as u64, &bytes[..])?;
        let mut seen = BTreeSet::new();
        for (name, path) in files {
            safety::validate_rel(name)?;
            if !seen.insert(name) {
                bail!("duplicate archive entry: {name}")
            }
            ensure_regular(path)?;
            let file = File::open(path)?;
            append(&mut tar, name, file.metadata()?.len(), file)?;
        }
        tar.into_inner()?.finish()?;
    }
    compressed.flush()?;
    let verification = stage_in(
        compressed.path(),
        &Passphrase::None,
        tempfile::tempdir_in(parent)?,
    )?;
    if !verification.report.ok() {
        let reasons: Vec<String> = verification
            .report
            .mismatches
            .iter()
            .cloned()
            .chain(
                verification
                    .report
                    .unsafe_entries
                    .iter()
                    .map(|entry| format!("{entry}: unsafe entry")),
            )
            .chain(
                verification
                    .report
                    .unexpected_entries
                    .iter()
                    .map(|entry| format!("{entry}: unexpected entry")),
            )
            .take(5)
            .collect();
        bail!("archive self-verification failed: {}", reasons.join("; "))
    }
    compressed.seek(SeekFrom::Start(0))?;
    let mode = if overwrite {
        WriteMode::Replace
    } else {
        WriteMode::CreateNew
    };
    if matches!(passphrase, Passphrase::None) {
        safety::atomic_write_from(parent, rel, &mut compressed, mode)?;
    } else {
        let mut encrypted = tempfile::tempfile_in(parent)?;
        crate::crypto::encrypt(compressed, &mut encrypted, passphrase)?;
        encrypted.seek(SeekFrom::Start(0))?;
        safety::atomic_write_from(parent, rel, &mut encrypted, mode)?;
    }
    Ok(())
}

pub fn ensure_regular(path: &Path) -> Result<()> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        bail!("source is not a regular file")
    }
    Ok(())
}

fn append<W: Write, R: Read>(
    tar: &mut tar::Builder<W>,
    name: &str,
    size: u64,
    reader: R,
) -> Result<()> {
    safety::validate_rel(name)?;
    if size > MAX_ENTRY_SIZE {
        bail!("source exceeds maximum entry size")
    }
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(size);
    header.set_cksum();
    tar.append_data(&mut header, name, reader)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        crate::backup::plan(&[]).unwrap().0
    }
    fn raw_archive(path: &Path, names: &[(&str, tar::EntryType, &[u8])]) {
        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let bytes = serde_json::to_vec(&manifest()).unwrap();
        append(
            &mut builder,
            "manifest.json",
            bytes.len() as u64,
            &bytes[..],
        )
        .unwrap();
        for (name, kind, bytes) in names {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_entry_type(*kind);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("outside").unwrap();
            }
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }
    #[test]
    fn rejects_links_duplicates_and_unexpected_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive");
        raw_archive(
            &path,
            &[
                ("extras/link", tar::EntryType::Symlink, b""),
                ("extras/hard", tar::EntryType::Link, b""),
                ("extras/a..b", tar::EntryType::Regular, b"one"),
                ("extras/a..b", tar::EntryType::Regular, b"two"),
            ],
        );
        let staged = stage(&path, &Passphrase::None).unwrap();
        assert_eq!(staged.report.unsafe_entries.len(), 3);
        assert_eq!(staged.report.unexpected_entries, ["extras/a..b"]);
        assert!(!staged.report.ok());
    }
    #[test]
    fn refuses_oversized_entry_before_reading_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bomb");
        let mut gz = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut header = tar::Header::new_gnu();
        header.set_path("manifest.json").unwrap();
        header.set_size(MAX_ENTRY_SIZE + 1);
        header.set_mode(0o600);
        header.set_cksum();
        gz.write_all(header.as_bytes()).unwrap();
        gz.finish().unwrap();
        let error = stage(&path, &Passphrase::None).err().unwrap().to_string();
        assert!(error.contains("maximum entry size"), "{error}");
    }
    #[test]
    fn legacy_v1_session_restores_without_artifact_digests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v1.tar.gz");
        let metadata = serde_json::json!({"created_at":"2026-01-01T00:00:00Z", "sessions":[{"id":"old", "directory":"/synthetic/project", "timestamp":"2026-01-01T00:00:00Z"}]});
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::default(),
        ));
        let bytes = serde_json::to_vec(&metadata).unwrap();
        append(
            &mut builder,
            "manifest.json",
            bytes.len() as u64,
            &bytes[..],
        )
        .unwrap();
        append(&mut builder, "sessions/old/old.jsonl", 3, &b"{}\n"[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let manifest = crate::engine::read_manifest(&path, &Passphrase::None).unwrap();
        assert!(manifest.is_v1());
        assert_eq!(manifest.sessions[0].cwd, "/synthetic/project");
        assert!(stage(&path, &Passphrase::None)
            .unwrap()
            .files
            .contains_key("sessions/old/old.jsonl"));
    }

    #[test]
    fn corrupt_extra_digest_and_missing_config_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("corrupt");
        let mut metadata = manifest();
        metadata.extras.push(crate::model::FileDigest {
            rel: "a..b".into(),
            sha256: "00".repeat(32),
            size: 3,
        });
        metadata.config.push(crate::model::FileDigest {
            rel: "missing.json".into(),
            sha256: "00".repeat(32),
            size: 3,
        });
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::default(),
        ));
        let bytes = serde_json::to_vec(&metadata).unwrap();
        append(
            &mut builder,
            "manifest.json",
            bytes.len() as u64,
            &bytes[..],
        )
        .unwrap();
        append(&mut builder, "extras/a..b", 3, &b"bad"[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let staged = stage(&path, &Passphrase::None).unwrap();
        assert_eq!(staged.report.checked, 2);
        assert_eq!(staged.report.mismatches.len(), 2);
        assert!(staged.report.unsafe_entries.is_empty());
    }

    #[test]
    fn raw_traversal_entry_is_reported_and_never_extracted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hostile");
        let mut gzip = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let bytes = serde_json::to_vec(&manifest()).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_path("manifest.json").unwrap();
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        gzip.write_all(header.as_bytes()).unwrap();
        gzip.write_all(&bytes).unwrap();
        gzip.write_all(&vec![0; (512 - bytes.len() % 512) % 512])
            .unwrap();
        let mut hostile = tar::Header::new_gnu();
        let name = b"../escaped";
        hostile.as_mut_bytes()[..name.len()].copy_from_slice(name);
        hostile.set_size(0);
        hostile.set_entry_type(tar::EntryType::Regular);
        hostile.set_cksum();
        gzip.write_all(hostile.as_bytes()).unwrap();
        gzip.write_all(&[0; 1024]).unwrap();
        gzip.finish().unwrap();
        let staged = stage(&path, &Passphrase::None).unwrap();
        assert_eq!(staged.report.unsafe_entries, ["../escaped"]);
        assert_eq!(staged.files.len(), 1);
        assert!(!temp.path().join("escaped").exists());
    }

    #[test]
    fn atomic_archive_does_not_clobber_and_has_private_headers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive");
        write(&path, false, &manifest(), &[], &Passphrase::None).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(write(&path, false, &manifest(), &[], &Passphrase::None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut tar = tar::Archive::new(GzDecoder::new(File::open(path).unwrap()));
        for entry in tar.entries().unwrap() {
            assert_eq!(entry.unwrap().header().mode().unwrap(), 0o600);
        }
    }
    #[test]
    fn configured_small_total_limit_and_private_staging_location() {
        const CHILD: &str = "CM_FORJA_LIMIT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let root = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
            assert_eq!(total_limit().unwrap(), 1024);
            let path = root.join("oversized.tar.gz");
            raw_archive(
                &path,
                &[("extras/data", tar::EntryType::Regular, &[b'x'; 2048])],
            );
            let error = stage(&path, &Passphrase::None).err().unwrap().to_string();
            assert!(error.contains("maximum total size"), "{error}");
            let plain = root.join("small.tar.gz");
            raw_archive(&plain, &[]);
            let staging_path;
            {
                let staged = stage(&plain, &Passphrase::None).unwrap();
                staging_path = staged.staging.path().to_path_buf();
                assert!(staging_path.starts_with(root.join("data/aisession-backup/staging")));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        std::fs::metadata(&staging_path)
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o700
                    );
                }
            }
            assert!(!staging_path.exists());
            return;
        }
        assert_eq!(MAX_ENTRY_SIZE, 16 * 1024 * 1024 * 1024);
        assert_eq!(MAX_TOTAL_SIZE, 64 * 1024 * 1024 * 1024);
        assert_eq!(MAX_ENTRIES, 1_000_000);
        let temp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "archive::tests::configured_small_total_limit_and_private_staging_location",
            ])
            .env(CHILD, temp.path())
            .env("AISESSION_BACKUP_MAX_ARCHIVE_BYTES", "1024")
            .env("AISESSION_BACKUP_DATA_DIR", temp.path().join("data"))
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn total_limit_can_be_raised_and_backup_uses_output_filesystem() {
        const CHILD: &str = "CM_FORJA_TEMP_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let root = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
            assert_eq!(total_limit().unwrap(), MAX_TOTAL_SIZE * 2);
            let output = root.join("backup.tar.gz");
            write(&output, false, &manifest(), &[], &Passphrase::None).unwrap();
            assert!(output.is_file());
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let missing_tmp = temp.path().join("does-not-exist");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "archive::tests::total_limit_can_be_raised_and_backup_uses_output_filesystem",
            ])
            .env(CHILD, temp.path())
            .env("TMPDIR", missing_tmp)
            .env(
                "AISESSION_BACKUP_MAX_ARCHIVE_BYTES",
                (MAX_TOTAL_SIZE * 2).to_string(),
            )
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[test]
    fn verification_reports_escape_controls_and_self_verification_names_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hostile");
        raw_archive(
            &path,
            &[(
                "extras/escape\u{1b}[31m",
                tar::EntryType::Regular,
                b"fixture",
            )],
        );
        let report = stage(&path, &Passphrase::None).unwrap().report;
        assert!(report
            .unsafe_entries
            .iter()
            .all(|name| !name.chars().any(char::is_control)));
        assert!(report.unsafe_entries[0].contains("\\u{1b}"));
        let source = temp.path().join("source");
        std::fs::write(&source, b"fixture").unwrap();
        let output = temp.path().join("bad.tar.gz");
        let mut metadata = manifest();
        metadata.extras.push(crate::model::FileDigest {
            rel: "payload".into(),
            sha256: "00".repeat(32),
            size: 7,
        });
        let error = write(
            &output,
            false,
            &metadata,
            &[("extras/payload".into(), source)],
            &Passphrase::None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("extras/payload: checksum or size mismatch"),
            "{error}"
        );
        assert!(!output.exists());
    }
}
