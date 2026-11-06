use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use sha2::Digest;

use aisession_backup::archive::{MAX_ENTRIES, MAX_ENTRY_SIZE};
use aisession_backup::cli::{parse_provider_homes, EXIT_OK, EXIT_PLAN_REFUSED};
use aisession_backup::engine::{
    apply_restore, audit_selection, list_journal, plan_backup, plan_rollback, prepare_restore,
    read_manifest, rollback, run_backup, verify_archive, BackupRequest, ClaudeSelection,
    Passphrase, ProviderSelection, RestoreRequest,
};
use aisession_backup::plan::{Action, Plan};
use aisession_backup::provider::{FileClass, ItemKind, Provider, ProviderId, ScanOptions};
use aisession_backup::remap::{apply_longest_mapping, rewrite_jsonl};
use aisession_backup::safety::{atomic_write, WriteMode};
use aisession_backup::tui::{MapRow, RestoreApp, RestoreFlags, RestoreStep};
use ratatui::crossterm::event::KeyCode;

static ENV: Mutex<()> = Mutex::new(());

fn isolate(root: &Path) -> MutexGuard<'static, ()> {
    let guard = ENV.lock().unwrap_or_else(|err| err.into_inner());
    for (key, value) in [
        ("HOME", root.join("home")),
        ("USERPROFILE", root.join("home")),
        ("XDG_CONFIG_HOME", root.join("config")),
        ("XDG_DATA_HOME", root.join("data-home")),
        ("AISESSION_BACKUP_DATA_DIR", root.join("migrate-data")),
        ("CLAUDE_CONFIG_DIR", root.join("claude-config")),
        ("CODEX_HOME", root.join("codex-home")),
        ("MAESTRI_HOME", root.join("not-a-maestri-home")),
    ] {
        fs::create_dir_all(&value).unwrap();
        std::env::set_var(key, &value);
    }
    std::env::remove_var("MAESTRI_SOCKET");
    guard
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if !root.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.unwrap();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        let kind = if meta.file_type().is_symlink() {
            format!("symlink:{}", fs::read_link(entry.path()).unwrap().display())
        } else if meta.file_type().is_dir() {
            "dir".into()
        } else if meta.file_type().is_file() {
            format!(
                "file:{:x}",
                sha2::Sha256::digest(fs::read(entry.path()).unwrap())
            )
        } else {
            "other".into()
        };
        out.insert(rel, kind);
    }
    out
}

fn has_control(text: &str) -> bool {
    text.chars().any(|ch| ch.is_control() || ch == '\u{7f}')
}

fn session_bytes(cwd: &str, content: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "type": "user",
        "cwd": cwd,
        "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": content}
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
}

fn put_rel(home: &Path, rel: &str, bytes: &[u8]) {
    atomic_write(home, rel, bytes, WriteMode::CreateNew).unwrap();
}

fn put_session(home: &Path, id: &str, cwd: &str, content: &str) {
    let encoded = aisession_backup::claudepath::encode(cwd);
    put_rel(
        home,
        &format!("projects/{encoded}/{id}.jsonl"),
        &session_bytes(cwd, content),
    );
}

fn backup_request(root: &Path, claude_dir: &Path) -> BackupRequest {
    BackupRequest {
        output: root.join("out.tar.gz"),
        overwrite: false,
        claude: Some(ClaudeSelection {
            claude_dir: claude_dir.to_path_buf(),
            session_ids: None,
            match_path: None,
            include_extras: false,
            include_config: true,
            include_credentials: false,
        }),
        providers: Vec::new(),
        passphrase: Passphrase::None,
    }
}

fn restore_request(_root: &Path, archive: &Path, claude_dir: &Path) -> RestoreRequest {
    RestoreRequest {
        archive: archive.to_path_buf(),
        passphrase: Passphrase::None,
        claude_dir: claude_dir.to_path_buf(),
        provider_homes: BTreeMap::new(),
        only_providers: None,
        only_sessions: None,
        mappings: BTreeMap::new(),
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

fn refused(action: &Action) -> bool {
    matches!(action, Action::Refuse(_))
}

fn skipped_exec(action: &Action) -> bool {
    matches!(action, Action::Skip(reason) if reason.contains("allow-exec-config"))
}

struct EnvGuard(&'static str);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

struct RawEntry {
    name: String,
    typeflag: u8,
    body: Vec<u8>,
    linkname: String,
}

fn put_octal(dst: &mut [u8], value: u64) {
    let text = format!("{value:o}");
    let mut encoded = std::iter::once(0u8).chain(text.bytes().rev().chain(std::iter::repeat(b'0')));
    for slot in dst.iter_mut().rev() {
        *slot = encoded.next().unwrap();
    }
}

fn raw_header(entry: &RawEntry) -> [u8; 512] {
    let mut header = [0u8; 512];
    let name = entry.name.as_bytes();
    assert!(name.len() <= 100, "raw name needs a pax or gnu header");
    header[..name.len()].copy_from_slice(name);
    put_octal(&mut header[100..108], 0o644);
    put_octal(&mut header[108..116], 0);
    put_octal(&mut header[116..124], 0);
    put_octal(&mut header[124..136], entry.body.len() as u64);
    put_octal(&mut header[136..148], 0);
    header[156] = entry.typeflag;
    let link = entry.linkname.as_bytes();
    assert!(link.len() <= 100);
    header[157..157 + link.len()].copy_from_slice(link);
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].fill(b' ');
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    let checksum = format!("{sum:06o}\0 ");
    header[148..156].copy_from_slice(checksum.as_bytes());
    header
}

fn write_tar_gz(path: &Path, entries: &[RawEntry]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let file = File::create(path).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    for entry in entries {
        encoder.write_all(&raw_header(entry)).unwrap();
        encoder.write_all(&entry.body).unwrap();
        let pad = (512 - (entry.body.len() % 512)) % 512;
        encoder.write_all(&vec![0u8; pad]).unwrap();
    }
    encoder.write_all(&[0u8; 1024]).unwrap();
    encoder.finish().unwrap();
}

fn manifest_bytes(sessions: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 3,
        "created_at": "2026-01-01T00:00:00Z",
        "sessions": sessions,
        "providers": [],
        "extras": [],
        "config": [],
        "checksum": "not-a-real-checksum"
    }))
    .unwrap()
}

fn file_entry(name: &str, body: &[u8]) -> RawEntry {
    RawEntry {
        name: name.to_string(),
        typeflag: b'0',
        body: body.to_vec(),
        linkname: String::new(),
    }
}

fn manifest_entry(extra_sessions: serde_json::Value) -> RawEntry {
    file_entry("manifest.json", &manifest_bytes(extra_sessions))
}

fn empty_manifest() -> RawEntry {
    manifest_entry(serde_json::json!([]))
}

fn archive_with(path: &Path, extra: &[RawEntry]) {
    let mut entries = vec![empty_manifest()];
    entries.extend(extra.iter().map(|entry| RawEntry {
        name: entry.name.clone(),
        typeflag: entry.typeflag,
        body: entry.body.clone(),
        linkname: entry.linkname.clone(),
    }));
    write_tar_gz(path, &entries);
}

fn assert_archive_refused(archive: &Path, dest: &Path, outside: &Path) {
    let before_dest = snapshot(dest);
    let before_out = snapshot(outside);
    let report = verify_archive(archive, &Passphrase::None).unwrap_or_else(|err| {
        panic!("verify must return a report for a hostile but well-formed archive, got {err:#}")
    });
    assert!(!report.ok(), "hostile archive verified clean: {report:?}");
    let mut request = restore_request(dest, archive, dest);
    request.archive = archive.to_path_buf();
    request.claude_dir = dest.to_path_buf();
    match prepare_restore(request) {
        Err(err) => {
            assert_eq!(
                snapshot(dest),
                before_dest,
                "prepare error still changed dest: {err:#}"
            );
            assert_eq!(
                snapshot(outside),
                before_out,
                "prepare error escaped the destination: {err:#}"
            );
        }
        Ok(prepared) => {
            assert!(
                prepared.plan.has_refusals(),
                "hostile archive was not refused: {:?}",
                prepared.plan
            );
            assert!(apply_restore(prepared).is_err(), "apply ran a refused plan");
            assert_eq!(snapshot(dest), before_dest, "destination changed");
            assert_eq!(
                snapshot(outside),
                before_out,
                "bytes escaped the destination"
            );
        }
    }
}

#[test]
fn raw_header_roundtrips_through_the_tar_crate() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("plain.tar.gz");
    write_tar_gz(&path, &[file_entry("manifest.json", b"{}")]);
    let file = File::open(path).unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entry = archive.entries().unwrap().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().to_string_lossy(), "manifest.json");
}

#[test]
fn traversal_entry_is_refused_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let archive = tmp.path().join("hostile.tar.gz");
    archive_with(
        &archive,
        &[file_entry(
            "sessions/sess-benign/../../outside/pwned",
            b"pwned",
        )],
    );
    assert_archive_refused(&archive, &dest, &outside);
    assert!(!outside.join("pwned").exists());
}

#[test]
fn absolute_names_via_empty_segment_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let marker = outside.join("pwned");
    let absolute = marker.to_string_lossy().replace('\\', "/");
    let archive = tmp.path().join("absolute.tar.gz");
    archive_with(
        &archive,
        &[
            file_entry(&format!("sessions/sess-benign/env/{absolute}"), b"env"),
            file_entry(&format!("sessions/sess-benign/todos/{absolute}"), b"todo"),
            file_entry(&format!("config/{absolute}"), b"cfg"),
            file_entry("sessions/x/env//etc/x", b"slash"),
            file_entry("config//abs", b"abs"),
        ],
    );
    assert_archive_refused(&archive, &dest, &outside);
    assert!(!marker.exists());
}

#[test]
fn manifest_session_id_traversal_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let body = session_bytes("/work/app", "hi");
    let session = serde_json::json!([{
        "id": "../../outside/pwned",
        "cwd": "/work/app",
        "timestamp": "2026-01-01T00:00:00Z",
        "artifacts": [{
            "kind": "session",
            "logical_name": "../../outside/pwned.jsonl",
            "sha256": hex::encode(sha2::Sha256::digest(&body)),
            "size": body.len()
        }]
    }]);
    let archive = tmp.path().join("id.tar.gz");
    write_tar_gz(
        &archive,
        &[
            manifest_entry(session),
            file_entry(
                "sessions/../../outside/pwned/../../outside/pwned.jsonl",
                &body,
            ),
        ],
    );
    assert_archive_refused(&archive, &dest, &outside);
}

#[test]
fn symlink_hardlink_fifo_and_device_entries_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let archive = tmp.path().join("links.tar.gz");
    let mut entries = Vec::new();
    for (name, flag) in [
        ("extras/sym", b'2'),
        ("extras/hard", b'1'),
        ("extras/fifo", b'6'),
        ("extras/char", b'3'),
        ("extras/block", b'4'),
    ] {
        entries.push(RawEntry {
            name: name.to_string(),
            typeflag: flag,
            body: Vec::new(),
            linkname: "t".into(),
        });
    }
    archive_with(&archive, &entries);
    let report = verify_archive(&archive, &Passphrase::None).unwrap();
    assert!(!report.ok(), "{report:?}");
    assert!(
        report.unsafe_entries.len() >= 5,
        "special files were not all reported: {:?}",
        report.unsafe_entries
    );
    assert_archive_refused(&archive, &dest, &outside);
    let sparse = tmp.path().join("sparse.tar.gz");
    archive_with(
        &sparse,
        &[RawEntry {
            name: "extras/sparse".into(),
            typeflag: b'S',
            body: Vec::new(),
            linkname: "t".into(),
        }],
    );
    let before_dest = snapshot(&dest);
    let before_out = snapshot(&outside);
    match verify_archive(&sparse, &Passphrase::None) {
        Err(_) => {}
        Ok(report) => assert!(!report.ok(), "sparse entry verified: {report:?}"),
    }
    let mut request = restore_request(tmp.path(), &sparse, &dest);
    request.archive = sparse;
    match prepare_restore(request) {
        Err(_) => {}
        Ok(prepared) => {
            assert!(
                prepared.plan.has_refusals(),
                "sparse archive was not refused: {:?}",
                prepared.plan
            );
            assert!(apply_restore(prepared).is_err());
        }
    }
    assert_eq!(snapshot(&dest), before_dest);
    assert_eq!(snapshot(&outside), before_out);
}

#[test]
fn duplicate_entries_cannot_bypass_the_checksum() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let good = b"trusted-env";
    let evil = b"pwned-env";
    let digest = hex::encode(sha2::Sha256::digest(good));
    let session = serde_json::json!([{
        "id": "sess-benign",
        "cwd": "/work/app",
        "timestamp": "2026-01-01T00:00:00Z",
        "artifacts": [{
            "kind": "env",
            "logical_name": "note.json",
            "sha256": digest,
            "size": good.len()
        }]
    }]);
    let archive = tmp.path().join("dup.tar.gz");
    write_tar_gz(
        &archive,
        &[
            manifest_entry(session),
            file_entry("sessions/sess-benign/env/note.json", good),
            file_entry("sessions/sess-benign/env/note.json", evil),
        ],
    );
    let report = verify_archive(&archive, &Passphrase::None).unwrap();
    assert!(!report.ok(), "duplicate env verified: {report:?}");
    assert!(
        report
            .unsafe_entries
            .iter()
            .any(|item| item.contains("duplicate")),
        "{report:?}"
    );
    let mut request = restore_request(tmp.path(), &archive, &dest);
    request.skip_verify = true;
    let prepared = prepare_restore(request).unwrap();
    if !prepared.plan.has_refusals() {
        let applied = apply_restore(prepared).unwrap();
        assert!(applied.failed.is_empty(), "{applied:?}");
    }
    let written = walkdir::WalkDir::new(&dest)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .any(|entry| fs::read(entry.path()).unwrap() == evil);
    assert!(!written, "second duplicate payload was the one restored");
}

#[test]
fn checksum_mismatch_refuses_restore_unless_skip_verify_and_skip_still_blocks_unsafe_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let body = b"tampered";
    let session = serde_json::json!([{
        "id": "sess-benign",
        "cwd": "/work/app",
        "timestamp": "2026-01-01T00:00:00Z",
        "artifacts": [{
            "kind": "session",
            "logical_name": "sess-benign.jsonl",
            "sha256": "deadbeef",
            "size": 1
        }]
    }]);
    let archive = tmp.path().join("mismatch.tar.gz");
    write_tar_gz(
        &archive,
        &[
            manifest_entry(session),
            file_entry("sessions/sess-benign/sess-benign.jsonl", body),
            file_entry("sessions/sess-benign/../../outside/pwned", b"escape"),
        ],
    );
    let report = verify_archive(&archive, &Passphrase::None).unwrap();
    assert!(!report.mismatches.is_empty(), "{report:?}");
    assert!(!report.unsafe_entries.is_empty(), "{report:?}");
    let blocked = restore_request(tmp.path(), &archive, &dest);
    let prepared = prepare_restore(blocked).unwrap();
    assert!(prepared.plan.has_refusals(), "{:?}", prepared.plan);
    assert!(apply_restore(prepared).is_err());
    assert!(!outside.join("pwned").exists());
    let mut skipped = restore_request(tmp.path(), &archive, &dest);
    skipped.skip_verify = true;
    let prepared = prepare_restore(skipped).unwrap();
    if !prepared.plan.has_refusals() {
        let _ = apply_restore(prepared);
    }
    assert!(
        !outside.join("pwned").exists(),
        "skip_verify wrote an unsafe path"
    );
}

#[test]
fn legitimate_double_dot_filename_is_not_treated_as_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "foo..bar", "/work/app", "kept");
    let request = backup_request(tmp.path(), &home);
    let report = run_backup(&request).unwrap();
    assert_eq!(report.sessions, 1);
    let verified = verify_archive(&request.output, &Passphrase::None).unwrap();
    assert!(verified.ok(), "foo..bar was rejected: {verified:?}");
    let dest = tmp.path().join("destination/.claude");
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.allow_running = true;
    let prepared = prepare_restore(restore).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    let encoded = aisession_backup::claudepath::encode("/work/app");
    let restored = dest.join(format!("projects/{encoded}/foo..bar.jsonl"));
    assert!(restored.is_file(), "legitimate name was dropped");
    let bytes = fs::read(&restored).unwrap();
    assert!(
        bytes.windows(4).any(|window| window == b"kept"),
        "legitimate payload was replaced"
    );
}

#[test]
fn oversized_entry_and_entry_count_are_rejected_by_the_public_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let bomb = tmp.path().join("bomb.tar.gz");
    let mut header = tar::Header::new_gnu();
    header.set_path("manifest.json").unwrap();
    header.set_size(MAX_ENTRY_SIZE + 1);
    header.set_mode(0o600);
    header.set_cksum();
    let mut encoder =
        flate2::write::GzEncoder::new(File::create(&bomb).unwrap(), flate2::Compression::default());
    encoder.write_all(header.as_bytes()).unwrap();
    encoder.finish().unwrap();
    let error = verify_archive(&bomb, &Passphrase::None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("maximum entry size"), "{error}");

    std::env::set_var("AISESSION_BACKUP_MAX_ARCHIVE_BYTES", "1024");
    let _limit = EnvGuard("AISESSION_BACKUP_MAX_ARCHIVE_BYTES");
    let total = tmp.path().join("total.tar.gz");
    let mut header = tar::Header::new_gnu();
    header.set_path("manifest.json").unwrap();
    header.set_size(2048);
    header.set_mode(0o600);
    header.set_cksum();
    let mut encoder = flate2::write::GzEncoder::new(
        File::create(&total).unwrap(),
        flate2::Compression::default(),
    );
    encoder.write_all(header.as_bytes()).unwrap();
    encoder.write_all(&[0u8; 16]).unwrap();
    encoder.finish().unwrap();
    let error = verify_archive(&total, &Passphrase::None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("maximum total size"), "{error}");
    drop(_limit);

    let many = tmp.path().join("many.tar.gz");
    let file = File::create(&many).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    for index in 0..=MAX_ENTRIES {
        let entry = file_entry(&format!("../{index:07}"), b"");
        encoder.write_all(&raw_header(&entry)).unwrap();
    }
    encoder.write_all(&[0u8; 1024]).unwrap();
    encoder.finish().unwrap();
    let error = verify_archive(&many, &Passphrase::None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("entry count"), "{error}");
}

#[test]
fn pax_and_not_first_manifest_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let hostile = format!(
        "../{}",
        outside.join("pwned").to_string_lossy().replace('\\', "/")
    );
    let record = pax_path_record(&hostile);
    let archive = tmp.path().join("pax.tar.gz");
    write_tar_gz(
        &archive,
        &[
            RawEntry {
                name: "PaxHeaders/payload".into(),
                typeflag: b'x',
                body: record,
                linkname: String::new(),
            },
            file_entry("sessions/sess-benign/sess-benign.jsonl", b"benign"),
            empty_manifest(),
        ],
    );
    let report = verify_archive(&archive, &Passphrase::None).unwrap();
    assert!(!report.ok(), "pax/misordered archive verified: {report:?}");
    assert!(
        report
            .unsafe_entries
            .iter()
            .chain(report.unexpected_entries.iter())
            .any(|item| item.contains("..") || item.contains("manifest") || item.contains("Pax")),
        "{report:?}"
    );
    assert_archive_refused(&archive, &dest, &outside);
}

fn pax_path_record(path: &str) -> Vec<u8> {
    let rest = format!(" path={path}\n");
    let mut digits = 1usize;
    loop {
        let total = digits + rest.len();
        if total.to_string().len() == digits {
            return format!("{total}{rest}").into_bytes();
        }
        digits += 1;
    }
}

#[test]
fn deep_tool_result_names_survive_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    let id = "sess-long";
    let cwd = "/work/app";
    put_session(&home, id, cwd, "parent");
    let encoded = aisession_backup::claudepath::encode(cwd);
    let long_name = "r".repeat(180);
    put_rel(
        &home,
        &format!("projects/{encoded}/{id}/tool-results/{long_name}.bin"),
        b"artifact-bytes",
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let report = verify_archive(&request.output, &Passphrase::None).unwrap();
    assert!(
        report.ok(),
        "long path was poisoned, unsafe={:?} unexpected={:?}",
        report.unsafe_entries,
        report.unexpected_entries
    );
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    let restored = dest.join(format!(
        "projects/{encoded}/{id}/tool-results/{long_name}.bin"
    ));
    assert_eq!(fs::read(restored).unwrap(), b"artifact-bytes");
}

#[test]
fn subagents_are_artifacts_not_extra_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    let cwd = "/work/app";
    put_session(&home, "parent-sess", cwd, "parent");
    let encoded = aisession_backup::claudepath::encode(cwd);
    put_rel(
        &home,
        &format!("projects/{encoded}/parent-sess/subagents/child.jsonl"),
        &session_bytes(cwd, "child"),
    );
    let request = backup_request(tmp.path(), &home);
    let report = run_backup(&request).unwrap();
    assert_eq!(
        report.sessions, 1,
        "subagent jsonl was promoted to a session"
    );
    let verified = verify_archive(&request.output, &Passphrase::None).unwrap();
    assert!(verified.ok(), "{verified:?}");
}

#[test]
fn hooks_mcp_and_plugins_are_refused_without_allow_exec_config() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(
        &home,
        "settings.json",
        br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"touch pwned"}]}]}}"#,
    );
    put_rel(
        &home,
        "settings.local.json",
        br#"{"statusLine":{"type":"command","command":"touch pwned"},"apiKeyHelper":"touch pwned"}"#,
    );
    put_rel(
        &home,
        "plugins/config.json",
        b"{\"command\":\"touch pwned\"}",
    );
    put_rel(
        home.parent().unwrap(),
        ".claude.json",
        br#"{"theme":"dark","mcpServers":{"evil":{"command":"touch","args":["pwned"]}}}"#,
    );
    let mut request = backup_request(tmp.path(), &home);
    request.claude.as_mut().unwrap().include_config = true;
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    for needle in [
        "settings.json",
        "settings.local.json",
        "plugins/config.json",
        ".claude.json",
    ] {
        let matched: Vec<_> = prepared
            .plan
            .ops
            .iter()
            .filter(|op| op.dest.ends_with(needle))
            .collect();
        assert!(
            !matched.is_empty(),
            "{needle} missing from plan: {:?}",
            prepared.plan
        );
        assert!(
            matched.iter().all(|op| skipped_exec(&op.action)),
            "{needle} was not skipped: {matched:?}"
        );
    }
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    assert!(!dest.join("settings.json").exists());
    assert!(!dest.join("settings.local.json").exists());
    assert!(!dest.join("plugins/config.json").exists());
    assert!(!dest.parent().unwrap().join(".claude.json").exists());
    let encoded = aisession_backup::claudepath::encode("/work/app");
    assert!(dest
        .join(format!("projects/{encoded}/sess-benign.jsonl"))
        .is_file());
}

#[test]
fn jsonc_and_nested_exec_keys_are_still_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(
        &home,
        "settings.json",
        b"{\n  // comment keeps this from being strict JSON\n  \"hooks\": {\"PreToolUse\": [{\"hooks\": [{\"type\": \"command\", \"command\": \"touch pwned\"}]}]}\n}\n",
    );
    put_rel(
        &home,
        "settings.local.json",
        br#"{"env":{"apiKeyHelper":"touch pwned","statusLine":"touch pwned"}}"#,
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    let mut bypassed = Vec::new();
    for needle in ["settings.json", "settings.local.json"] {
        let matched: Vec<_> = prepared
            .plan
            .ops
            .iter()
            .filter(|op| op.dest.ends_with(needle))
            .collect();
        if matched.is_empty() || !matched.iter().all(|op| skipped_exec(&op.action)) {
            bypassed.push(format!("{needle}: {matched:?}"));
        }
    }
    assert!(bypassed.is_empty(), "exec gate bypassed: {bypassed:?}");
}

#[test]
fn oauth_bundle_does_not_smuggle_mcp_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(
        home.parent().unwrap(),
        ".claude.json",
        br#"{"mcpServers":{"evil":{"command":"touch","args":["pwned"]}},"oauthAccount":{"token":"synthetic-oauth-token"}}"#,
    );
    let mut request = backup_request(tmp.path(), &home);
    request.passphrase = Passphrase::Value("correct horse battery".into());
    request.claude.as_mut().unwrap().include_credentials = true;
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.passphrase = Passphrase::Value("correct horse battery".into());
    restore.allow_credentials = true;
    restore.allow_exec_config = false;
    let prepared = prepare_restore(restore).unwrap();
    let claude_json = dest.parent().unwrap().join(".claude.json");
    if prepared.plan.has_refusals() {
        assert!(apply_restore(prepared).is_err());
    } else {
        let applied = apply_restore(prepared).unwrap();
        assert!(applied.failed.is_empty(), "{applied:?}");
    }
    if claude_json.exists() {
        let bytes = fs::read(&claude_json).unwrap();
        assert!(
            !bytes.windows(10).any(|window| window == b"mcpServers"),
            "mcp command was restored with credentials: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn corrupt_claude_json_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(
        home.parent().unwrap(),
        ".claude.json",
        br#"{"mcpServers":{"demo":{"command":"test"}}}"#,
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let corrupt = b"{ this is not json, keep every byte";
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(dest.parent().unwrap().join(".claude.json"), corrupt).unwrap();
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.allow_exec_config = true;
    let prepared = prepare_restore(restore).unwrap();
    assert!(prepared.plan.has_refusals(), "{:?}", prepared.plan);
    assert_eq!(
        fs::read(dest.parent().unwrap().join(".claude.json")).unwrap(),
        corrupt
    );
    assert!(apply_restore(prepared).is_err());
    assert_eq!(
        fs::read(dest.parent().unwrap().join(".claude.json")).unwrap(),
        corrupt
    );
}

#[test]
fn dry_run_planning_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    let request = backup_request(tmp.path(), &home);
    let before_out = request.output.exists();
    let plan = plan_backup(&request).unwrap();
    assert!(!plan.ops.is_empty());
    assert!(!request.output.exists() && !before_out);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    put_rel(&dest, "keep.txt", b"same");
    let before = snapshot(&dest);
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    assert_eq!(snapshot(&dest), before);
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let _ = prepared;
    assert_eq!(snapshot(&dest), before);
}

#[test]
fn wrong_passphrase_and_magic_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    let mut request = backup_request(tmp.path(), &home);
    request.passphrase = Passphrase::Value("correct horse battery".into());
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    fs::create_dir_all(&dest).unwrap();
    let before = snapshot(&dest);
    let wrong = verify_archive(&request.output, &Passphrase::Value("nope".into()));
    assert!(wrong.is_err(), "wrong passphrase verified");
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.passphrase = Passphrase::Value("nope".into());
    assert!(prepare_restore(restore).is_err());
    assert_eq!(snapshot(&dest), before);
    let plain_root = tmp.path().join("plain");
    fs::create_dir_all(&plain_root).unwrap();
    let mut plain_request = backup_request(&plain_root, &home);
    plain_request.output = tmp.path().join("plain/notes.txt");
    run_backup(&plain_request).unwrap();
    let report = verify_archive(&plain_request.output, &Passphrase::None).unwrap();
    assert!(
        !report.encrypted,
        "plaintext was treated as encrypted because of its name"
    );
    assert!(report.ok(), "{report:?}");
    let misnamed = tmp.path().join("plain/notes.tar.gz.age");
    fs::copy(&plain_request.output, &misnamed).unwrap();
    let report = verify_archive(&misnamed, &Passphrase::None).unwrap();
    assert!(
        !report.encrypted,
        "plaintext renamed to .age was treated as encrypted"
    );
    assert!(report.ok(), "{report:?}");
    let encrypted = verify_archive(&request.output, &request.passphrase).unwrap();
    assert!(encrypted.encrypted);
    assert!(encrypted.ok(), "{encrypted:?}");
}

#[test]
fn credentials_without_encryption_fail_before_any_output() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let token = "ghp_abcdefghijklmnopqrstuvwxyz012345";
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(&home, ".credentials.json", token.as_bytes());
    put_rel(&home, "CLAUDE.md", format!("token {token}\n").as_bytes());
    let mut request = backup_request(tmp.path(), &home);
    request.claude.as_mut().unwrap().include_credentials = true;
    let error = run_backup(&request).unwrap_err().to_string();
    assert!(
        error.to_lowercase().contains("encrypt") || error.to_lowercase().contains("passphrase"),
        "{error}"
    );
    assert!(!request.output.exists());
    assert!(!error.contains(token));
    let findings = audit_selection(&request).unwrap();
    let blob = serde_json::to_string(&findings).unwrap();
    assert!(!blob.contains(token), "{blob}");
    assert!(findings.iter().any(|finding| finding.detector == "github"));
    assert!(findings.iter().all(|finding| finding.count >= 1));
}

#[test]
fn rollback_restores_preimages_and_keeps_files_changed_since() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    let cwd = "/work/app";
    let id = "sess-benign";
    put_session(&home, id, cwd, "original-body");
    let encoded = aisession_backup::claudepath::encode(cwd);
    put_rel(
        &home,
        &format!("session-env/{id}/env.json"),
        br#"{"PWD":"/work/app"}"#,
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let existing = format!("projects/{encoded}/{id}.jsonl");
    put_rel(&dest, &existing, b"PREIMAGE");
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.overwrite = true;
    let prepared = prepare_restore(restore).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    let journal_id = applied.journal_id.expect("journal id");
    let created = dest.join(format!("session-env/{id}/env.json"));
    fs::write(&created, b"user changed this after restore").unwrap();
    let plan = plan_rollback(&journal_id).unwrap();
    let before_rollback = fs::read(&created).unwrap();
    assert!(
        plan.ops
            .iter()
            .any(|op| refused(&op.action) && op.dest == created),
        "changed file was not refused: {:?}",
        plan.ops
    );
    let report = rollback(&journal_id).unwrap();
    assert_eq!(fs::read(&created).unwrap(), before_rollback);
    assert!(
        report.failed.iter().any(|item| item.contains("changed")),
        "{report:?}"
    );
    assert_eq!(fs::read(dest.join(&existing)).unwrap(), b"PREIMAGE");
    assert!(rollback("../escape").is_err());
    assert!(list_journal()
        .unwrap()
        .iter()
        .any(|entry| entry.id == journal_id));
}

#[test]
fn ansi_in_manifest_strings_and_verify_reports_is_stripped() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    let hostile_cwd = "/work/\u{1b}[2J\u{1b}]0;owned\u{7}app";
    put_session(&home, "sess-benign", hostile_cwd, "preview \u{1b}[31mred");
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let manifest = read_manifest(&request.output, &Passphrase::None).unwrap();
    assert!(
        manifest
            .sessions
            .iter()
            .any(|session| session.cwd == hostile_cwd),
        "stored cwd changed"
    );
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    let mut leaks = Vec::new();
    for warning in &prepared.plan.warnings {
        if has_control(warning) {
            leaks.push(format!("warning:{warning:?}"));
        }
    }
    for op in &prepared.plan.ops {
        if has_control(&op.note) {
            leaks.push(format!("note:{:?}", op.note));
        }
        let dest_text = op.dest.display().to_string();
        if has_control(&dest_text) {
            leaks.push(format!("dest:{dest_text:?}"));
        }
        if let Action::Skip(reason) | Action::Refuse(reason) = &op.action {
            if has_control(reason) {
                leaks.push(format!("reason:{reason:?}"));
            }
        }
    }
    let plan_debug = format!("{:?}", prepared.plan);
    if has_control(&plan_debug) {
        leaks.push(format!("plan debug:{plan_debug:?}"));
    }
    if prepared.plan.has_refusals() {
        assert!(apply_restore(prepared).is_err());
    } else {
        let applied = apply_restore(prepared).unwrap();
        assert!(applied.failed.is_empty(), "{applied:?}");
        if dest.exists() {
            for entry in walkdir::WalkDir::new(&dest).follow_links(false) {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy();
                if has_control(&name) {
                    leaks.push(format!("restored path:{name:?}"));
                }
            }
        }
    }
    let archive = tmp.path().join("escape.tar.gz");
    archive_with(&archive, &[file_entry("extras/\u{1b}[31mred", b"x")]);
    let report = verify_archive(&archive, &Passphrase::None).unwrap();
    for item in report
        .unsafe_entries
        .iter()
        .chain(report.unexpected_entries.iter())
        .chain(report.mismatches.iter())
    {
        if has_control(item) {
            leaks.push(format!("report:{item:?}"));
        }
    }
    let report_debug = format!("{report:?}");
    if has_control(&report_debug) {
        leaks.push(format!("report debug:{report_debug:?}"));
    }
    if !report
        .unsafe_entries
        .iter()
        .any(|item| item.contains("red") && item.contains("\\u{1b}"))
    {
        leaks.push(format!(
            "hostile name missing from verify report: {report:?}"
        ));
    }
    assert!(leaks.is_empty(), "ansi leaked into the engine: {leaks:?}");
}

#[test]
fn history_file_does_not_contain_raw_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(
        &home,
        "sess-benign",
        "/work/app",
        "hello \u{1b}[2J\u{1b}]0;owned\u{7}",
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.restore_history = true;
    let prepared = prepare_restore(restore).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    let history = fs::read(dest.join("history.jsonl")).unwrap();
    assert!(
        !history.contains(&0x1b),
        "raw ESC stored in history.jsonl: {}",
        String::from_utf8_lossy(&history)
    );
}

#[cfg(unix)]
#[test]
fn symlink_ancestor_of_the_destination_is_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let encoded = aisession_backup::claudepath::encode("/work/app");

    let real = tmp.path().join("real");
    fs::create_dir_all(&real).unwrap();
    let link = tmp.path().join("linked-home");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let through_link = link.join(".claude");
    let prepared =
        prepare_restore(restore_request(tmp.path(), &request.output, &through_link)).unwrap();
    assert!(
        !prepared.plan.has_refusals(),
        "symlinked ancestor was refused: {:?}",
        prepared.plan
    );
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    let restored = real.join(format!(".claude/projects/{encoded}/sess-benign.jsonl"));
    let restored_bytes = fs::read(&restored).unwrap();
    assert!(
        restored_bytes.windows(2).any(|window| window == b"hi"),
        "restore through a symlinked ancestor did not land on the real directory"
    );

    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("marker"), b"keep").unwrap();
    let base = tmp.path().join("plain/.claude");
    fs::create_dir_all(&base).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("projects")).unwrap();
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &base)).unwrap();
    assert!(
        prepared.plan.has_refusals(),
        "symlink below the base was accepted: {:?}",
        prepared.plan
    );
    assert!(apply_restore(prepared).is_err());
    assert_eq!(fs::read(outside.join("marker")).unwrap(), b"keep");
    assert!(
        walkdir::WalkDir::new(&outside)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .all(|entry| entry.file_name() != "sess-benign.jsonl"),
        "session escaped through a symlink below the base"
    );
}

#[cfg(unix)]
#[test]
fn provider_symlinked_walk_root_is_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("codex");
    fs::create_dir_all(home.join("sessions/2026")).unwrap();
    fs::write(
        home.join("sessions/2026/rollout.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\",\"cwd\":\"/work/app\"}}\n",
    )
    .unwrap();
    let outside = tmp.path().join("outside-rules");
    fs::create_dir_all(&outside).unwrap();
    let secret = b"SYNTHETIC_PROVIDER_WALK_SECRET_vigia";
    fs::write(outside.join("secret.jsonl"), secret).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("rules")).unwrap();
    let inventory = aisession_backup::provider::codex::Codex
        .scan(&home, &ScanOptions::default())
        .unwrap();
    let rels: Vec<&str> = inventory
        .items
        .iter()
        .flat_map(|item| item.files.iter().map(|file| file.rel.as_str()))
        .collect();
    assert!(
        !rels.iter().any(|rel| rel.contains("secret")),
        "symlinked provider root was walked: {rels:?}"
    );
    let mut request = backup_request(tmp.path(), &tmp.path().join("unused"));
    request.claude = None;
    request.providers.push(ProviderSelection {
        provider: ProviderId::Codex,
        home,
        item_ids: None,
        include_state: false,
        include_credentials: false,
    });
    let report = run_backup(&request).unwrap();
    assert!(report.provider_items >= 1, "real session was dropped");
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(File::open(&request.output).unwrap())
        .read_to_end(&mut decoded)
        .unwrap();
    assert!(
        !decoded.windows(secret.len()).any(|window| window == secret),
        "symlinked provider root was archived"
    );
}

#[cfg(unix)]
#[test]
fn backup_does_not_follow_symlinked_memory_or_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    let cwd = "/work/app";
    let id = "sess-benign";
    put_session(&home, id, cwd, "hi");
    let encoded = aisession_backup::claudepath::encode(cwd);
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let secret = b"SYNTHETIC_MEMORY_SECRET_vigia";
    fs::write(outside.join("secret.txt"), secret).unwrap();
    std::os::unix::fs::symlink(&outside, home.join(format!("projects/{encoded}/memory"))).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), home.join("settings.json")).unwrap();
    let bundle = aisession_backup::config::collect(&home, false);
    assert!(
        bundle.files.iter().all(|(_, bytes)| bytes != secret),
        "settings symlink was archived"
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let file = File::open(&request.output).unwrap();
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(file)
        .read_to_end(&mut decoded)
        .unwrap();
    assert!(
        !decoded.windows(secret.len()).any(|window| window == secret),
        "symlinked memory directory was archived"
    );
}

#[cfg(unix)]
#[test]
fn claude_json_symlink_and_hardlink_do_not_clobber_the_outside_inode() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(
        home.parent().unwrap(),
        ".claude.json",
        br#"{"mcpServers":{"demo":{"command":"test"}}}"#,
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let precious = outside.join("precious.json");
    fs::write(&precious, b"{\"theme\":\"keep\"}").unwrap();
    let dest = tmp.path().join("destination/.claude");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::hard_link(&precious, dest.parent().unwrap().join(".claude.json")).unwrap();
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.allow_exec_config = true;
    restore.overwrite = true;
    let prepared = prepare_restore(restore).unwrap();
    if !prepared.plan.has_refusals() {
        let applied = apply_restore(prepared).unwrap();
        assert!(applied.failed.is_empty(), "{applied:?}");
    }
    assert_eq!(fs::read(&precious).unwrap(), b"{\"theme\":\"keep\"}");
}

#[cfg(unix)]
#[test]
fn fifo_destination_does_not_block_the_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    put_rel(&home, "settings.json", br#"{"theme":"dark"}"#);
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    fs::create_dir_all(&dest).unwrap();
    let fifo = dest.join("settings.json");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let archive = request.output.clone();
    let dest_thread = dest.clone();
    let root = tmp.path().to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut restore = restore_request(&root, &archive, &dest_thread);
        restore.overwrite = true;
        let result = prepare_restore(restore).and_then(|prepared| {
            if prepared.plan.has_refusals() {
                Ok(())
            } else {
                apply_restore(prepared).map(|_| ())
            }
        });
        let _ = sender.send(result.map(|_| ()).map_err(|err| err.to_string()));
    });
    let finished = receiver.recv_timeout(Duration::from_secs(5));
    if finished.is_err() {
        let _ = File::create(&fifo);
        let _ = receiver.recv_timeout(Duration::from_secs(2));
    }
    assert!(finished.is_ok(), "restore blocked on a fifo destination");
}

#[test]
fn legacy_restore_config_refuses_hooks_and_preserves_corrupt_json() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-ok", "/work/app", "hi");
    put_rel(
        &home,
        "settings.json",
        br#"{"hooks":{"PreToolUse":[{"command":"touch pwned"}]}}"#,
    );
    put_rel(
        home.parent().unwrap(),
        ".claude.json",
        br#"{"mcpServers":{"evil":{"command":"touch","args":["pwned"]}}}"#,
    );
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let corrupt = b"{not-json-keep";
    let claude_json = dest.parent().unwrap().join(".claude.json");
    fs::write(&claude_json, corrupt).unwrap();
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.overwrite = true;
    restore.apply_config = true;
    restore.allow_exec_config = false;
    let prepared = prepare_restore(restore).unwrap();
    assert!(
        !prepared.plan.has_refusals(),
        "default exec gate refused the archive: {:?}",
        prepared.plan
    );
    for needle in ["settings.json", ".claude.json"] {
        let matched: Vec<_> = prepared
            .plan
            .ops
            .iter()
            .filter(|op| op.dest.ends_with(needle))
            .collect();
        assert!(
            !matched.is_empty() && matched.iter().all(|op| skipped_exec(&op.action)),
            "{needle} was not skipped: {matched:?}"
        );
    }
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    assert!(!dest.join("settings.json").exists(), "hooks were written");
    assert_eq!(fs::read(&claude_json).unwrap(), corrupt);
    let mut allowed = restore_request(tmp.path(), &request.output, &dest);
    allowed.overwrite = true;
    allowed.apply_config = true;
    allowed.allow_exec_config = true;
    let prepared = prepare_restore(allowed).unwrap();
    assert!(
        prepared.plan.ops.iter().any(|op| {
            matches!(&op.action, Action::Refuse(reason) if reason.contains("invalid") || reason.contains(".claude.json"))
        }),
        "corrupt merge was not refused: {:?}",
        prepared.plan
    );
    assert_eq!(fs::read(&claude_json).unwrap(), corrupt);
    assert!(apply_restore(prepared).is_err());
    assert_eq!(fs::read(&claude_json).unwrap(), corrupt);
    assert!(
        !dest.join("settings.json").exists(),
        "refused plan wrote hooks"
    );
}

#[test]
fn legacy_restore_sessions_reject_hostile_ids_and_do_not_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("destination/.claude");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let ok_body = session_bytes("/work/app", "ok-body");
    let benign_body = session_bytes("", "empty");
    let hostile_body = session_bytes("/work/app", "x");
    let pwned = b"pwned";
    let sessions = serde_json::json!([
        {
            "id": "../outside",
            "cwd": "/work/app",
            "timestamp": "2026-01-01T00:00:00Z",
            "preview": "\u{1b}[2J",
            "message_count": 1,
            "size_bytes": 1,
            "artifacts": [
                {
                    "kind": "session",
                    "logical_name": "../outside.jsonl",
                    "sha256": hex::encode(sha2::Sha256::digest(&hostile_body)),
                    "size": hostile_body.len()
                },
                {
                    "kind": "env",
                    "logical_name": "../../escaped",
                    "sha256": hex::encode(sha2::Sha256::digest(pwned)),
                    "size": pwned.len()
                }
            ]
        },
        {
            "id": "sess-benign",
            "cwd": "",
            "timestamp": "2026-01-01T00:00:00Z",
            "preview": "\u{1b}[2J",
            "message_count": 1,
            "size_bytes": 1,
            "artifacts": [
                {
                    "kind": "session",
                    "logical_name": "sess-benign.jsonl",
                    "sha256": hex::encode(sha2::Sha256::digest(&benign_body)),
                    "size": benign_body.len()
                },
                {
                    "kind": "env",
                    "logical_name": "../../escaped",
                    "sha256": hex::encode(sha2::Sha256::digest(pwned)),
                    "size": pwned.len()
                }
            ]
        },
        {
            "id": "sess-ok",
            "cwd": "/work/app",
            "timestamp": "2026-01-01T00:00:00Z",
            "preview": "\u{1b}[2J",
            "message_count": 1,
            "size_bytes": 1,
            "artifacts": [
                {
                    "kind": "session",
                    "logical_name": "sess-ok.jsonl",
                    "sha256": hex::encode(sha2::Sha256::digest(&ok_body)),
                    "size": ok_body.len()
                },
                {
                    "kind": "env",
                    "logical_name": "../../escaped",
                    "sha256": hex::encode(sha2::Sha256::digest(pwned)),
                    "size": pwned.len()
                }
            ]
        }
    ]);
    let archive = tmp.path().join("hostile-sessions.tar.gz");
    write_tar_gz(
        &archive,
        &[
            manifest_entry(sessions),
            file_entry("sessions/../outside/../outside.jsonl", &hostile_body),
            file_entry("sessions/sess-benign/sess-benign.jsonl", &benign_body),
            file_entry("sessions/sess-benign/env/../../escaped", pwned),
            file_entry("sessions/sess-ok/sess-ok.jsonl", &ok_body),
            file_entry("sessions/sess-ok/env/../../escaped", pwned),
        ],
    );
    let report = verify_archive(&archive, &Passphrase::None).unwrap();
    assert!(!report.ok(), "hostile session archive verified: {report:?}");
    assert!(
        report
            .unsafe_entries
            .iter()
            .any(|item| item.contains("escaped") || item.contains("..")),
        "hostile names were not reported: {report:?}"
    );
    let before_out = snapshot(&outside);
    let prepared = prepare_restore(restore_request(tmp.path(), &archive, &dest)).unwrap();
    assert!(
        prepared.plan.has_refusals(),
        "hostile session archive was not refused: {:?}",
        prepared.plan
    );
    assert!(apply_restore(prepared).is_err());
    assert_eq!(snapshot(&outside), before_out);
    assert!(
        !walkdir::WalkDir::new(&dest)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == "sess-benign.jsonl"),
        "empty cwd session was written"
    );
    let skipped_dest = tmp.path().join("skipped/.claude");
    fs::create_dir_all(&skipped_dest).unwrap();
    let mut skipped = restore_request(tmp.path(), &archive, &skipped_dest);
    skipped.skip_verify = true;
    skipped.apply_config = false;
    let prepared = prepare_restore(skipped).unwrap();
    for op in &prepared.plan.ops {
        if matches!(
            op.action,
            Action::Create | Action::Overwrite | Action::Merge
        ) {
            let text = op.dest.display().to_string();
            assert!(!text.contains(".."), "planned escape: {text}");
            assert!(
                op.dest.starts_with(&skipped_dest),
                "planned write left the destination: {text}"
            );
            assert_ne!(
                op.dest.file_name().and_then(|name| name.to_str()),
                Some("sess-benign.jsonl")
            );
            assert_ne!(
                op.dest.file_name().and_then(|name| name.to_str()),
                Some("escaped")
            );
        }
    }
    assert!(
        prepared.plan.ops.iter().any(|op| {
            matches!(&op.action, Action::Skip(reason) if reason.to_lowercase().contains("cwd"))
        }),
        "empty cwd was not skipped: {:?}",
        prepared.plan
    );
    assert!(
        !prepared.plan.has_refusals(),
        "skip_verify still refused the archive: {:?}",
        prepared.plan
    );
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    let encoded = aisession_backup::claudepath::encode("/work/app");
    let restored = skipped_dest.join(format!("projects/{encoded}/sess-ok.jsonl"));
    let bytes = fs::read(&restored).unwrap();
    assert!(
        bytes.windows(7).any(|window| window == b"ok-body"),
        "safe session was dropped when verification was skipped"
    );
    assert_eq!(snapshot(&outside), before_out, "skip_verify escaped");
    assert!(
        !walkdir::WalkDir::new(&skipped_dest)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == "sess-benign.jsonl" || entry.file_name() == "escaped"),
        "skip_verify wrote an empty cwd session or the escaped env name"
    );
    for root in [&dest, &skipped_dest, &outside] {
        let leaked = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .any(|entry| fs::read(entry.path()).ok().as_deref() == Some(pwned.as_slice()));
        assert!(!leaked, "env payload was written under {}", root.display());
    }
}

#[test]
fn restored_files_are_private() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let applied = apply_restore(prepared).unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in walkdir::WalkDir::new(&dest).follow_links(false) {
            let entry = entry.unwrap();
            let mode = fs::symlink_metadata(entry.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            if entry.file_type().is_file() {
                assert_eq!(mode, 0o600, "{}", entry.path().display());
            } else if entry.file_type().is_dir() && entry.path() != dest {
                assert_eq!(mode, 0o700, "{}", entry.path().display());
            }
        }
    }
}

#[test]
fn empty_cwd_is_refused_and_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    let id = "sess-empty";
    put_rel(
        &home,
        &format!("projects/unknown/{id}.jsonl"),
        &session_bytes("", ""),
    );
    fs::write(
        {
            let path = home.join(format!("projects/unknown/{id}.jsonl"));
            path
        },
        b"{\"type\":\"user\",\"message\":{\"content\":\"x\"}}\n",
    )
    .unwrap();
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).expect("empty cwd must not abort the backup");
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &request.output, &dest)).unwrap();
    let session_name = format!("{id}.jsonl");
    let wrote_empty = prepared.plan.ops.iter().any(|op| {
        matches!(
            op.action,
            Action::Create | Action::Overwrite | Action::Merge
        ) && op.dest.file_name().and_then(|name| name.to_str()) == Some(session_name.as_str())
    });
    assert!(!wrote_empty, "empty cwd was planned: {:?}", prepared.plan);
    assert!(
        prepared.plan.ops.iter().any(|op| {
            matches!(&op.action, Action::Skip(reason) if reason.to_lowercase().contains("cwd"))
        }),
        "empty cwd was not skipped with a reason: {:?}",
        prepared.plan
    );
    if prepared.plan.has_refusals() {
        assert!(apply_restore(prepared).is_err());
    } else {
        let applied = apply_restore(prepared).unwrap();
        assert!(applied.failed.is_empty(), "{applied:?}");
    }
    if dest.exists() {
        assert!(
            walkdir::WalkDir::new(&dest)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .all(|entry| entry.file_name().to_string_lossy() != format!("{id}.jsonl")),
            "empty cwd session was written"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn running_provider_is_refused_without_allow_running() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("source/.claude");
    put_session(&home, "sess-benign", "/work/app", "hi");
    let request = backup_request(tmp.path(), &home);
    run_backup(&request).unwrap();
    let dest = tmp.path().join("destination/.claude");
    let mut child = spawn_named_sleeper(tmp.path(), "claude");
    let mut restore = restore_request(tmp.path(), &request.output, &dest);
    restore.allow_running = false;
    let prepared = prepare_restore(restore);
    let _ = child.kill();
    let _ = child.wait();
    let prepared = prepared.unwrap();
    assert!(
        prepared
            .plan
            .ops
            .iter()
            .any(|op| { refused(&op.action) && op.provider == "claude" }),
        "running claude was not refused: {:?}",
        prepared.plan
    );
}

#[cfg(target_os = "linux")]
fn spawn_named_sleeper(root: &Path, name: &str) -> std::process::Child {
    let bin = root.join(name);
    fs::copy("/bin/sleep", &bin).unwrap();
    let mut child = std::process::Command::new(&bin).arg("30").spawn().unwrap();
    let comm = format!("/proc/{}/comm", child.id());
    let ready = std::iter::repeat_with(|| {
        std::thread::sleep(Duration::from_millis(20));
        fs::read_to_string(&comm).unwrap_or_default()
    })
    .take(50)
    .any(|text| text.trim() == name);
    assert!(ready, "sleeper comm was not {name}");
    let _ = &mut child;
    child
}

#[test]
fn maestri_sidebar_cannot_launder_credentials_into_the_default_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("maestri");
    fs::create_dir_all(home.join("accounts/providers")).unwrap();
    fs::write(
        home.join("manifest.json"),
        r#"{"app":"Maestri","files":{"sidebarLayout":"accounts/providers/codex.json"}}"#,
    )
    .unwrap();
    fs::write(
        home.join("accounts/providers/codex.json"),
        br#"{"token":"synthetic"}"#,
    )
    .unwrap();
    fs::write(home.join("license.json"), b"secret").unwrap();
    fs::write(
        home.join("manifest.json"),
        r#"{"app":"Maestri","files":{"sidebarLayout":"license.json"}}"#,
    )
    .unwrap();
    let inventory = aisession_backup::provider::maestri::Maestri
        .scan(&home, &ScanOptions::default())
        .unwrap();
    let rels: Vec<&str> = inventory
        .items
        .iter()
        .flat_map(|item| item.files.iter().map(|file| file.rel.as_str()))
        .collect();
    assert!(
        !rels
            .iter()
            .any(|rel| *rel == "license.json" || rel.contains("accounts/")),
        "credential path laundered into the default scan: {rels:?}"
    );
    assert!(inventory
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::Credentials)
        .all(|item| item
            .files
            .iter()
            .all(|file| file.class == FileClass::Credential)));
}

#[test]
fn codex_toml_rewrite_rejects_injected_newlines() {
    let mappings = BTreeMap::from([("/old".to_string(), "/new\nowned = true\n".to_string())]);
    let Ok(out) = aisession_backup::provider::codex::Codex.rewrite(
        "config.toml",
        b"[projects.\"/old/app\"]\ntrust_level = \"trusted\"\n",
        &mappings,
    ) else {
        return;
    };
    if let Some(bytes) = out.bytes {
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("owned"),
            "toml injection landed in the rewrite: {text}"
        );
    }
}

#[test]
fn gemini_project_root_rewrite_rejects_control_characters() {
    let mappings = BTreeMap::from([("/old".to_string(), "/new\nssh-ed25519 AAAA".to_string())]);
    let Ok(out) = aisession_backup::provider::gemini::Gemini.rewrite(
        "tmp/app/.project_root",
        b"/old/a\n",
        &mappings,
    ) else {
        return;
    };
    if let Some(bytes) = out.bytes {
        let text = String::from_utf8(bytes).unwrap();
        let body = text.trim_end_matches(['\n', '\r']);
        assert!(
            !has_control(body) && !body.contains('\n'),
            "project root gained an extra record: {text:?}"
        );
    }
}

#[test]
fn gemini_trust_key_collision_does_not_drop_a_decision() {
    let mappings = BTreeMap::from([
        ("/old/a".to_string(), "/shared".to_string()),
        ("/old/b".to_string(), "/shared".to_string()),
    ]);
    let Ok(out) = aisession_backup::provider::gemini::Gemini.rewrite(
        "trustedFolders.json",
        br#"{"/old/a":"TRUST_FOLDER","/old/b":"BLOCK"}"#,
        &mappings,
    ) else {
        return;
    };
    let Some(bytes) = out.bytes else {
        return;
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let object = value.as_object().expect("object");
    let values: Vec<&str> = object.values().filter_map(|value| value.as_str()).collect();
    assert!(
        values.contains(&"TRUST_FOLDER") && values.contains(&"BLOCK"),
        "trust decision collapsed: {object:?}"
    );
}

#[test]
fn provider_exec_config_is_refused_on_restore_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("codex-home");
    fs::create_dir_all(home.join("sessions/2026")).unwrap();
    fs::write(
        home.join("sessions/2026/rollout.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\",\"cwd\":\"/work/app\"}}\n",
    )
    .unwrap();
    fs::write(
        home.join("config.toml"),
        "[projects.\"/work/app\"]\ntrust_level = \"trusted\"\n",
    )
    .unwrap();
    fs::write(home.join("auth.json"), br#"{"token":"synthetic"}"#).unwrap();
    let mut request = backup_request(tmp.path(), &tmp.path().join("unused"));
    request.claude = None;
    request.providers.push(ProviderSelection {
        provider: ProviderId::Codex,
        home: home.clone(),
        item_ids: None,
        include_state: false,
        include_credentials: false,
    });
    let report = run_backup(&request).unwrap();
    assert!(report.provider_items >= 1);
    let verified = verify_archive(&request.output, &Passphrase::None).unwrap();
    assert!(verified.ok(), "{verified:?}");
    let mut archive_bytes = Vec::new();
    flate2::read::GzDecoder::new(File::open(&request.output).unwrap())
        .read_to_end(&mut archive_bytes)
        .unwrap();
    assert!(!archive_bytes
        .windows(8)
        .any(|window| window == b"synthetic"));
    let dest = tmp.path().join("restored-codex");
    let mut restore = restore_request(tmp.path(), &request.output, &tmp.path().join("unused-dest"));
    restore
        .provider_homes
        .insert(ProviderId::Codex, dest.clone());
    restore.allow_exec_config = false;
    let prepared = prepare_restore(restore).unwrap();
    let config_ops: Vec<_> = prepared
        .plan
        .ops
        .iter()
        .filter(|op| op.dest.ends_with("config.toml"))
        .collect();
    assert!(
        !config_ops.is_empty(),
        "codex config.toml missing from plan: {:?}",
        prepared.plan
    );
    assert!(
        config_ops.iter().all(|op| skipped_exec(&op.action)),
        "codex config.toml was not skipped: {config_ops:?}"
    );
    if !prepared.plan.has_refusals() {
        let applied = apply_restore(prepared).unwrap();
        assert!(applied.failed.is_empty(), "{applied:?}");
    }
    assert!(!dest.join("config.toml").exists());
}

#[test]
fn v1_archive_still_loads_and_a_traversal_inside_it_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let body = session_bytes("/work/app", "legacy");
    let session = serde_json::json!([{
        "id": "legacy-sess",
        "directory": "/work/app",
        "timestamp": "2026-01-01T00:00:00Z"
    }]);
    let good = tmp.path().join("v1.tar.gz");
    write_tar_gz(
        &good,
        &[
            file_entry(
                "manifest.json",
                &serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1,
                    "created_at": "2026-01-01T00:00:00Z",
                    "sessions": session
                }))
                .unwrap(),
            ),
            file_entry("sessions/legacy-sess/legacy-sess.jsonl", &body),
        ],
    );
    let report = verify_archive(&good, &Passphrase::None).unwrap();
    assert!(report.ok(), "v1 archive rejected: {report:?}");
    let dest = tmp.path().join("destination/.claude");
    let prepared = prepare_restore(restore_request(tmp.path(), &good, &dest)).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);

    let bad = tmp.path().join("v1-hostile.tar.gz");
    write_tar_gz(
        &bad,
        &[
            file_entry(
                "manifest.json",
                &serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1,
                    "created_at": "2026-01-01T00:00:00Z",
                    "sessions": [{
                        "id": "legacy-sess",
                        "directory": "/work/app",
                        "timestamp": "2026-01-01T00:00:00Z"
                    }]
                }))
                .unwrap(),
            ),
            file_entry("sessions/legacy-sess/../../outside/pwned", b"x"),
        ],
    );
    let report = verify_archive(&bad, &Passphrase::None).unwrap();
    assert!(!report.ok(), "v1 traversal verified: {report:?}");
}

#[test]
fn execution_json_smuggled_in_the_archive_is_not_run() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let payload = serde_json::json!([{
        "dest": outside.join("pwned"),
        "payload": "execution/0",
        "before": null
    }]);
    let archive = tmp.path().join("exec.tar.gz");
    archive_with(
        &archive,
        &[file_entry(
            "execution.json",
            &serde_json::to_vec(&payload).unwrap(),
        )],
    );
    let mut request = restore_request(tmp.path(), &archive, &dest);
    request.skip_verify = true;
    let prepared = prepare_restore(request);
    if let Ok(prepared) = prepared {
        if !prepared.plan.has_refusals() {
            let _ = apply_restore(prepared);
        }
    }
    assert!(!outside.join("pwned").exists());
}

fn drive_prefix(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(path[..2].to_string())
    } else {
        None
    }
}

fn lexical_collapse(path: &str) -> String {
    let drive = drive_prefix(path);
    let unc = drive.is_none() && path.starts_with(r"\\");
    let absolute = drive.is_some() || path.starts_with('/') || unc;
    let mut parts: Vec<String> = Vec::new();
    if let Some(prefix) = &drive {
        parts.push(prefix.clone());
    }
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if drive.as_deref() == Some(segment) {
            continue;
        }
        if segment == ".." {
            if parts.len() > usize::from(drive.is_some()) {
                parts.pop();
            }
            continue;
        }
        parts.push(segment.to_string());
    }
    if unc {
        return format!(r"\\{}", parts.join(r"\"));
    }
    if let Some(prefix) = &drive {
        let rest = parts.iter().skip(1).cloned().collect::<Vec<_>>().join("/");
        return if rest.is_empty() {
            format!("{prefix}/")
        } else {
            format!("{prefix}/{rest}")
        };
    }
    if absolute {
        return format!("/{}", parts.join("/"));
    }
    parts.join("/")
}

fn stays_inside(path: &str, root: &str) -> bool {
    let path = lexical_collapse(path);
    let root = lexical_collapse(root).trim_end_matches('/').to_string();
    path == root || path.starts_with(&format!("{root}/"))
}

fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

fn cwd_after(line: &str, mappings: &BTreeMap<String, String>) -> (usize, String, String) {
    let (out, stats) = rewrite_jsonl(line, mappings, false);
    let cwd = serde_json::from_str::<serde_json::Value>(out.trim())
        .ok()
        .and_then(|value| {
            value
                .get("cwd")
                .and_then(|cwd| cwd.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    (stats.lines_changed, cwd, out)
}

fn selection(id: ProviderId, home: &Path) -> ProviderSelection {
    ProviderSelection {
        provider: id,
        home: home.to_path_buf(),
        item_ids: None,
        include_state: false,
        include_credentials: false,
    }
}

fn write_under(home: &Path, rel: &str, bytes: &[u8]) {
    let dest = home.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(dest, bytes).unwrap();
}

fn guarded_homes(root: &Path) -> Vec<PathBuf> {
    [
        "home/.codex",
        "home/.grok",
        "home/.gemini",
        "home/.maestri",
        "home/.local/share/opencode",
        "codex-home",
        "not-a-maestri-home",
        "claude-config",
    ]
    .into_iter()
    .map(|rel| root.join(rel))
    .collect()
}

fn plant_markers(root: &Path) {
    for dir in guarded_homes(root) {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("MARKER"), b"marker-keep").unwrap();
    }
}

fn guard_snapshot(root: &Path) -> BTreeMap<String, BTreeMap<String, String>> {
    guarded_homes(root)
        .into_iter()
        .map(|dir| (dir.display().to_string(), snapshot(&dir)))
        .collect()
}

fn run_cli(root: &Path, args: &[&str]) -> std::process::Output {
    let home = root.join("home");
    Command::new(env!("CARGO_BIN_EXE_aisession-backup"))
        .current_dir(root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data-home"))
        .env("AISESSION_BACKUP_DATA_DIR", root.join("migrate-data"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude-config"))
        .env("CODEX_HOME", root.join("codex-home"))
        .env("MAESTRI_HOME", root.join("not-a-maestri-home"))
        .env("NO_COLOR", "1")
        .env_remove("AISESSION_BACKUP_PASSPHRASE")
        .env_remove("MAESTRI_SOCKET")
        .args(args)
        .output()
        .unwrap()
}

fn output_text(out: &std::process::Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn mapping_app(root: &Path, value: &str) -> RestoreApp {
    let manifest = serde_json::from_value(serde_json::json!({
        "schema_version": 3,
        "created_at": "2026-01-01T00:00:00Z",
        "sessions": [{
            "id": "sess-ok",
            "cwd": "/work/app",
            "timestamp": "2026-01-01T00:00:00Z"
        }]
    }))
    .unwrap();
    let mut app = RestoreApp::new(
        manifest,
        &root.join("archive.tar.gz"),
        &root.join("claude-dest"),
        RestoreFlags {
            allow_running: true,
            ..RestoreFlags::default()
        },
    );
    app.step = RestoreStep::Mapping;
    app.rows = vec![MapRow {
        root: "/work/app".into(),
        count: 1,
        value: value.to_string(),
    }];
    app
}

fn downgrade_exec_class(archive: &Path, output: &Path) {
    let staged = aisession_backup::archive::stage(archive, &Passphrase::None).unwrap();
    assert!(staged.report.ok(), "{:?}", staged.report);
    let mut manifest = staged.manifest.clone();
    let mut changed = 0;
    for provider in &mut manifest.providers {
        for item in &mut provider.items {
            for file in &mut item.files {
                if file.rel.ends_with("settings.json") {
                    assert_eq!(file.class, FileClass::ExecutesCode, "{}", file.rel);
                    file.file.class = FileClass::Plain;
                    changed += 1;
                }
            }
        }
    }
    assert!(
        changed >= 2,
        "settings.json was not archived as executable config"
    );
    let files: Vec<(String, PathBuf)> = staged
        .files
        .iter()
        .filter(|(name, _)| name.as_str() != "manifest.json")
        .map(|(name, path)| (name.clone(), path.clone()))
        .collect();
    aisession_backup::archive::write(output, false, &manifest, &files, &Passphrase::None).unwrap();
    let report = verify_archive(output, &Passphrase::None).unwrap();
    assert!(report.ok(), "downgraded archive did not verify: {report:?}");
}

fn settings_on_disk(root: &Path) -> Vec<String> {
    if !root.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "settings.json")
        .map(|entry| entry.path().display().to_string())
        .collect()
}

#[test]
fn remap_keeps_boundaries_and_converts_a_real_unc_prefix() {
    assert_eq!(lexical_collapse("/safe/../../x"), "/x");
    assert_eq!(lexical_collapse("/safe/proj"), "/safe/proj");
    assert!(stays_inside("/safe/proj", "/safe"));
    assert!(!stays_inside("/safe/../../x", "/safe"));
    assert!(!stays_inside("/safe2", "/safe"));

    let drive = pairs(&[("C:\\a", "/safe")]);
    assert_eq!(
        apply_longest_mapping(r"C:\a\secret", &drive),
        "/safe/secret"
    );
    assert_eq!(apply_longest_mapping(r"C:\abc", &drive), r"C:\abc");
    assert_eq!(apply_longest_mapping(r"C:\a.txt", &drive), r"C:\a.txt");

    let unc = pairs(&[("\\\\server\\share", "/safe")]);
    assert_eq!(
        apply_longest_mapping(r"\\server\share\proj", &unc),
        "/safe/proj"
    );
    assert!(stays_inside(
        &apply_longest_mapping(r"\\server\share\proj", &unc),
        "/safe"
    ));
    assert_eq!(
        apply_longest_mapping(r"\\server\share2", &unc),
        r"\\server\share2"
    );
    assert_eq!(apply_longest_mapping(r"\\server\share", &unc), "/safe");
}

#[test]
fn remap_traversal_remainder_stays_inside_the_destination() {
    let drive = pairs(&[("C:\\a", "/safe")]);
    let unc = pairs(&[("\\\\server\\share", "/safe")]);
    let cases = [
        (r"C:\a\..\..\x", &drive),
        (r"C:\a/../b", &drive),
        (r"\\server\share\..\..\x", &unc),
        (r"\\server\share\..\..\..\Windows\System32", &unc),
    ];
    let mut escaped = Vec::new();
    for (path, mappings) in cases {
        let out = apply_longest_mapping(path, mappings);
        if out != path && !stays_inside(&out, "/safe") {
            escaped.push(format!("{path} -> {out}"));
        }
    }
    assert!(
        escaped.is_empty(),
        "remapped paths leave /safe: {escaped:?}"
    );
}

#[test]
fn remap_treats_windows_and_posix_separators_as_the_same_prefix() {
    let drive = pairs(&[("C:\\a", "/safe")]);
    let flipped = pairs(&[("C:/a", "/safe")]);
    let mut missed = Vec::new();
    for (path, mappings, expect) in [
        (r"C:/a/secret", &drive, "/safe/secret"),
        (r"C:\a\secret", &flipped, "/safe/secret"),
    ] {
        let out = apply_longest_mapping(path, mappings);
        if out != expect {
            missed.push(format!("{path} -> {out}"));
        }
    }
    let traversal = apply_longest_mapping(r"C:/a/../../x", &drive);
    assert!(
        traversal == r"C:/a/../../x" || stays_inside(&traversal, "/safe"),
        "escaped: {traversal}"
    );
    assert!(
        missed.is_empty(),
        "separator spellings were not remapped: {missed:?}"
    );
}

#[test]
fn rewrite_jsonl_follows_escaped_json_paths() {
    let posix = pairs(&[("/home/n00b", "/Users/vyn")]);
    let (changed, cwd, _) = cwd_after("{\"cwd\":\"/home/n00b/proj\"}\n", &posix);
    assert_eq!((changed, cwd.as_str()), (1, "/Users/vyn/proj"));
    let windows = pairs(&[("C:\\Users\\win", "/home/linux")]);
    let plain = format!("{}\n", serde_json::json!({"cwd": r"C:\Users\win\proj"}));
    let (changed, cwd, _) = cwd_after(&plain, &windows);
    assert_eq!((changed, cwd.as_str()), (1, "/home/linux/proj"));

    let unicode = "{\"cwd\":\"\\u002fhome\\u002fn00b\\u002fproj\"}\n";
    let slashed = "{\"cwd\":\"\\/home\\/n00b\\/proj\"}\n";
    let win_unicode = "{\"cwd\":\"C:\\u005cUsers\\u005cwin\\u005cproj\"}\n";
    let mut missed = Vec::new();
    for (label, line, mappings, expect) in [
        ("slash-escape", slashed, &posix, "/Users/vyn/proj"),
        ("unicode-slash", unicode, &posix, "/Users/vyn/proj"),
        (
            "unicode-backslash",
            win_unicode,
            &windows,
            "/home/linux/proj",
        ),
    ] {
        let (changed, cwd, out) = cwd_after(line, mappings);
        if changed != 1 || cwd != expect {
            missed.push(format!("{label}: changed={changed} cwd={cwd} out={out}"));
        }
    }
    assert!(
        missed.is_empty(),
        "escaped cwd was not remapped: {missed:?}"
    );
}

#[test]
fn restore_ui_rejects_hostile_mapping_destinations() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let mut accepted = Vec::new();
    for value in [
        "/../../etc",
        r"C:\a\..\..\x",
        "/tmp/safe/../../etc",
        "/tmp/\u{1b}[31mowned",
    ] {
        let mut app = mapping_app(tmp.path(), value);
        app.handle_key(KeyCode::Enter);
        if app.step != RestoreStep::Mapping || app.mapping_error.is_none() {
            accepted.push(format!("{value:?} -> {:?}", app.step));
        }
    }
    let mut relative = mapping_app(tmp.path(), "../etc");
    relative.handle_key(KeyCode::Enter);
    assert_eq!(relative.step, RestoreStep::Mapping);
    assert!(relative.mapping_error.is_some());
    let ok_dest = tmp.path().join("mapped-ok");
    let mut ok = mapping_app(tmp.path(), &ok_dest.display().to_string());
    ok.handle_key(KeyCode::Enter);
    assert_eq!(ok.step, RestoreStep::Preview);
    assert!(ok.mapping_error.is_none());
    assert!(
        accepted.is_empty(),
        "mapping accepted a hostile destination: {accepted:?}"
    );
}

#[test]
fn restore_ui_preview_keys_rebuild_before_confirm() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("src-claude");
    fs::create_dir_all(&home).unwrap();
    put_session(&home, "sess-ok", "/work/app", "body");
    put_rel(
        &home,
        "settings.json",
        br#"{"hooks":{"Start":[{"command":"danger"}]}}"#,
    );
    let mut request = backup_request(tmp.path(), &home);
    request.claude.as_mut().unwrap().include_config = true;
    run_backup(&request).unwrap();
    let dest = tmp.path().join("ui-dest");
    let staged = aisession_backup::archive::stage(&request.output, &Passphrase::None).unwrap();
    assert!(staged.report.ok(), "{:?}", staged.report);
    let mut app = RestoreApp::new(
        staged.manifest,
        &request.output,
        &dest,
        RestoreFlags {
            allow_running: true,
            apply_config: true,
            ..RestoreFlags::default()
        },
    );
    app.selected.fill(true);
    app.step = RestoreStep::Preview;
    app.set_plan(Plan::failed("provider may be running"));
    app.handle_key(KeyCode::Char('y'));
    app.handle_key(KeyCode::Enter);
    assert!(!app.confirmed);
    assert_eq!(app.step, RestoreStep::Preview);

    let first = prepare_restore(app.build_restore_request()).unwrap();
    assert!(
        !first.plan.has_refusals(),
        "fixture refused: {:?}",
        first.plan
    );
    assert!(
        first
            .plan
            .ops
            .iter()
            .any(|op| op.dest.ends_with("settings.json") && skipped_exec(&op.action)),
        "{:?}",
        first.plan
    );
    app.set_plan(first.plan);
    for key in ['x', 'c', 'o'] {
        app.handle_key(KeyCode::Char(key));
        assert!(app.plan.is_none(), "{key} left a plan in place");
        assert!(!app.confirmed);
    }
    let running = app.flags.allow_running;
    app.handle_key(KeyCode::Char('r'));
    assert_ne!(app.flags.allow_running, running);
    assert!(app.plan.is_none());
    assert!(!app.confirmed);
    app.handle_key(KeyCode::Char('r'));
    assert_eq!(app.flags.allow_running, running);
    assert!(app.plan.is_none());
    assert!(app.flags.allow_exec_config);
    assert!(app.flags.allow_credentials);
    assert!(app.flags.overwrite_config);
    let second = prepare_restore(app.build_restore_request()).unwrap();
    assert!(
        second.plan.ops.iter().any(|op| {
            op.dest.ends_with("settings.json")
                && matches!(op.action, Action::Create | Action::Overwrite)
        }),
        "toggled allow-exec still skipped settings.json: {:?}",
        second.plan
    );
    app.set_plan(second.plan);
    app.handle_key(KeyCode::Char('y'));
    assert!(app.confirmed);
    assert_eq!(app.step, RestoreStep::Done);

    app.confirmed = false;
    app.step = RestoreStep::Preview;
    app.handle_key(KeyCode::Esc);
    assert_eq!(app.step, RestoreStep::Mapping);
    assert!(
        app.plan.is_some(),
        "esc dropped the plan before the mapping changed"
    );
    let changed = tmp.path().join("other-root");
    app.rows = vec![MapRow {
        root: "/work/app".into(),
        count: 1,
        value: changed.display().to_string(),
    }];
    app.handle_key(KeyCode::Enter);
    assert_eq!(app.step, RestoreStep::Preview);
    assert!(app.plan.is_none(), "mapping enter kept the previous plan");
    assert!(!app.confirmed);
    let third = prepare_restore(app.build_restore_request()).unwrap();
    let encoded = aisession_backup::claudepath::encode(&changed.display().to_string());
    assert!(
        third
            .plan
            .ops
            .iter()
            .any(|op| op.dest.display().to_string().contains(&encoded)),
        "rebuilt plan ignored the edited mapping: {:?}",
        third.plan
    );
}

#[test]
fn partial_selection_omits_unselected_history_and_extras() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let home = tmp.path().join("src-claude");
    fs::create_dir_all(&home).unwrap();
    put_session(&home, "sess-keep", "/work/keep", "KEEP_PREVIEW");
    put_session(&home, "sess-drop", "/work/drop", "DROP_PREVIEW");
    put_rel(&home, "shell-snapshots/pwn.sh", b"echo pwned\n");
    let mut request = backup_request(tmp.path(), &home);
    request.claude.as_mut().unwrap().include_extras = true;
    request.claude.as_mut().unwrap().include_config = false;
    run_backup(&request).unwrap();

    let full_dest = tmp.path().join("full");
    let mut full = restore_request(tmp.path(), &request.output, &full_dest);
    full.restore_history = true;
    full.allow_exec_config = true;
    full.apply_config = false;
    let prepared = prepare_restore(full).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    apply_restore(prepared).unwrap();
    assert_eq!(
        fs::read(full_dest.join("shell-snapshots/pwn.sh")).unwrap(),
        b"echo pwned\n"
    );
    let full_history = fs::read_to_string(full_dest.join("history.jsonl")).unwrap();
    assert!(full_history.contains("KEEP_PREVIEW"), "{full_history}");
    assert!(full_history.contains("DROP_PREVIEW"), "{full_history}");

    let part_dest = tmp.path().join("partial");
    let mut part = restore_request(tmp.path(), &request.output, &part_dest);
    part.restore_history = true;
    part.allow_exec_config = true;
    part.apply_config = false;
    part.only_sessions = Some(BTreeSet::from(["sess-keep".to_string()]));
    let prepared = prepare_restore(part).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    apply_restore(prepared).unwrap();
    assert!(
        !part_dest.join("shell-snapshots/pwn.sh").exists(),
        "partial selection restored extras"
    );
    let history = fs::read_to_string(part_dest.join("history.jsonl")).unwrap_or_default();
    assert!(history.contains("KEEP_PREVIEW"), "{history}");
    assert!(!history.contains("DROP_PREVIEW"), "{history}");
    assert!(
        !snapshot(&part_dest)
            .keys()
            .any(|name| name.contains("sess-drop")),
        "{:?}",
        snapshot(&part_dest)
    );

    let empty_dest = tmp.path().join("empty-selection");
    let mut empty = restore_request(tmp.path(), &request.output, &empty_dest);
    empty.restore_history = true;
    empty.allow_exec_config = true;
    empty.apply_config = false;
    empty.only_sessions = Some(BTreeSet::new());
    let prepared = prepare_restore(empty).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    let creates = prepared.plan.ops.iter().any(|op| {
        matches!(
            op.action,
            Action::Create | Action::Overwrite | Action::Merge
        )
    });
    if !prepared.plan.has_refusals() {
        apply_restore(prepared).unwrap();
    }
    assert!(!creates, "empty session selection still planned writes");
    assert!(!empty_dest.join("shell-snapshots/pwn.sh").exists());
    assert!(!empty_dest.join("history.jsonl").exists());
}

#[test]
fn provider_home_duplicate_ids_and_dest_do_not_touch_default_homes() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    plant_markers(tmp.path());
    let shared = tmp.path().join("same-home");
    let dup = parse_provider_homes(&[
        format!("agy={}", shared.display()),
        format!("antigravity={}", shared.display()),
    ]);
    assert!(
        dup.unwrap_err().to_string().contains("more than once"),
        "agy and antigravity were both accepted"
    );
    let case = parse_provider_homes(&[
        format!("CODEX={}", shared.display()),
        format!("codex={}", tmp.path().join("other").display()),
    ]);
    assert!(
        case.unwrap_err().to_string().contains("more than once"),
        "provider id case variants were both accepted"
    );
    let same = parse_provider_homes(&[
        format!("gemini={}", shared.display()),
        format!("agy={}", shared.display()),
    ])
    .unwrap();
    assert_eq!(same.len(), 2);
    assert_eq!(
        same.get(&ProviderId::Gemini),
        same.get(&ProviderId::Antigravity)
    );

    let claude = tmp.path().join("src-claude");
    let codex = tmp.path().join("src-codex");
    let gemini = tmp.path().join("src-gemini");
    let agy = tmp.path().join("src-agy");
    let grok = tmp.path().join("src-grok");
    let maestri = tmp.path().join("src-maestri");
    fs::create_dir_all(&claude).unwrap();
    put_session(&claude, "sess-ok", "/work/app", "body");
    write_under(&codex, "AGENTS.md", b"agents-body\n");
    write_under(&gemini, "GEMINI.md", b"gemini-body\n");
    write_under(
        &gemini,
        "settings.json",
        br#"{"hooks":{"Start":[{"command":"echo pwned"}]}}"#,
    );
    write_under(&agy, "antigravity-cli/keybindings.json", b"keys\n");
    write_under(
        &agy,
        "antigravity-cli/settings.json",
        br#"{"hooks":{"Start":[{"command":"echo pwned"}]}}"#,
    );
    write_under(&grok, "config.toml", b"model = \"x\"\n");
    write_under(&maestri, "manifest.json", b"{}\n");
    let mut request = backup_request(tmp.path(), &claude);
    request.claude.as_mut().unwrap().include_config = false;
    request.providers = vec![
        selection(ProviderId::Codex, &codex),
        selection(ProviderId::Gemini, &gemini),
        selection(ProviderId::Antigravity, &agy),
        selection(ProviderId::Grok, &grok),
        selection(ProviderId::Maestri, &maestri),
    ];
    run_backup(&request).unwrap();
    let before = guard_snapshot(tmp.path());
    let archive = request.output.display().to_string();
    let dest = tmp.path().join("sandbox/claude");
    let dest_arg = dest.display().to_string();
    let refused = run_cli(
        tmp.path(),
        &[
            "restore",
            &archive,
            "--dest",
            &dest_arg,
            "--allow-running",
            "--yes",
        ],
    );
    assert_eq!(
        refused.status.code(),
        Some(i32::from(EXIT_PLAN_REFUSED)),
        "{}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("provider-home")
            || output_text(&refused).contains("provider destination"),
        "{}",
        output_text(&refused)
    );
    assert_eq!(guard_snapshot(tmp.path()), before);
    assert!(!dest.exists());

    let sandbox = tmp.path().join("sandbox");
    let homes = [
        ("codex", sandbox.join("codex")),
        ("gemini", sandbox.join("shared")),
        ("agy", sandbox.join("shared")),
        ("grok", sandbox.join("grok")),
        ("maestri", sandbox.join("maestri")),
    ];
    let specs: Vec<String> = homes
        .iter()
        .map(|(id, path)| format!("{id}={}", path.display()))
        .collect();
    let mut args = vec![
        "restore".to_string(),
        archive.clone(),
        "--dest".to_string(),
        dest_arg.clone(),
        "--allow-running".to_string(),
        "--yes".to_string(),
    ];
    for spec in &specs {
        args.push("--provider-home".to_string());
        args.push(spec.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let restored = run_cli(tmp.path(), &arg_refs);
    assert_eq!(
        restored.status.code(),
        Some(i32::from(EXIT_OK)),
        "{}",
        output_text(&restored)
    );
    assert_eq!(guard_snapshot(tmp.path()), before);
    assert_eq!(
        fs::read(sandbox.join("shared/GEMINI.md")).unwrap(),
        b"gemini-body\n"
    );
    assert_eq!(
        fs::read(sandbox.join("shared/antigravity-cli/keybindings.json")).unwrap(),
        b"keys\n"
    );
    assert_eq!(
        fs::read(sandbox.join("codex/AGENTS.md")).unwrap(),
        b"agents-body\n"
    );
    assert_eq!(
        fs::read(sandbox.join("maestri/manifest.json")).unwrap(),
        b"{}\n"
    );
    assert!(
        settings_on_disk(&sandbox.join("shared")).is_empty(),
        "honest exec settings were written: {:?}",
        settings_on_disk(&sandbox.join("shared"))
    );

    let agy_spec = format!("agy={}", sandbox.join("shared").display());
    let agy_again = format!("antigravity={}", sandbox.join("other").display());
    let dup_out = run_cli(
        tmp.path(),
        &[
            "restore",
            &archive,
            "--dest",
            &dest_arg,
            "--provider-home",
            &agy_spec,
            "--provider-home",
            &agy_again,
            "--allow-running",
            "--yes",
        ],
    );
    assert_ne!(
        dup_out.status.code(),
        Some(i32::from(EXIT_OK)),
        "{}",
        output_text(&dup_out)
    );
    assert!(
        output_text(&dup_out).contains("more than once"),
        "{}",
        output_text(&dup_out)
    );
    assert_eq!(guard_snapshot(tmp.path()), before);
}

#[cfg(unix)]
#[test]
fn provider_home_symlink_below_the_base_is_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let src = tmp.path().join("src-codex");
    write_under(&src, "sessions/2026/01/01/rollout.jsonl", b"{}\n");
    write_under(&src, "AGENTS.md", b"agents\n");
    let mut request = backup_request(tmp.path(), &tmp.path().join("unused-claude"));
    request.claude = None;
    request.providers = vec![selection(ProviderId::Codex, &src)];
    run_backup(&request).unwrap();
    let staged = aisession_backup::archive::stage(&request.output, &Passphrase::None).unwrap();
    assert!(staged.report.ok(), "{:?}", staged.report);
    assert!(staged.manifest.providers.iter().any(|provider| {
        provider
            .files
            .iter()
            .any(|file| file.rel.ends_with("rollout.jsonl"))
    }));

    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("MARKER"), b"keep-outside").unwrap();
    let home = tmp.path().join("dest-codex");
    fs::create_dir_all(&home).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("sessions")).unwrap();
    let mut restore = restore_request(tmp.path(), &request.output, &tmp.path().join("unused"));
    restore
        .provider_homes
        .insert(ProviderId::Codex, home.clone());
    restore.only_providers = Some(vec![ProviderId::Codex]);
    restore.allow_running = true;
    restore.allow_exec_config = true;
    let prepared = prepare_restore(restore).unwrap();
    assert!(
        prepared.plan.ops.iter().any(|op| {
            matches!(&op.action, Action::Refuse(reason) if reason.contains("unsafe"))
        }),
        "{:?}",
        prepared.plan
    );
    if !prepared.plan.has_refusals() {
        let _ = apply_restore(prepared);
    }
    assert_eq!(fs::read(outside.join("MARKER")).unwrap(), b"keep-outside");
    let leaked: Vec<_> = walkdir::WalkDir::new(&outside)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().display().to_string())
        .filter(|path| !path.ends_with("MARKER"))
        .collect();
    assert!(
        leaked.is_empty(),
        "payload followed the symlink: {leaked:?}"
    );
}

#[test]
fn archived_plain_class_cannot_smuggle_exec_config() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = isolate(tmp.path());
    let gemini = tmp.path().join("src-gemini");
    let agy = tmp.path().join("src-agy");
    let hooks = br#"{"hooks":{"Start":[{"command":"echo pwned"}]}}"#;
    write_under(&gemini, "GEMINI.md", b"gemini-body\n");
    write_under(&gemini, "settings.json", hooks);
    write_under(&agy, "antigravity-cli/keybindings.json", b"keys\n");
    write_under(&agy, "antigravity-cli/settings.json", hooks);
    let mut request = backup_request(tmp.path(), &tmp.path().join("unused-claude"));
    request.claude = None;
    request.providers = vec![
        selection(ProviderId::Gemini, &gemini),
        selection(ProviderId::Antigravity, &agy),
    ];
    run_backup(&request).unwrap();

    let honest = tmp.path().join("honest-home");
    let mut restore = restore_request(tmp.path(), &request.output, &tmp.path().join("unused"));
    restore
        .provider_homes
        .insert(ProviderId::Gemini, honest.clone());
    restore
        .provider_homes
        .insert(ProviderId::Antigravity, honest.clone());
    restore.only_providers = Some(vec![ProviderId::Gemini, ProviderId::Antigravity]);
    restore.allow_exec_config = false;
    restore.allow_running = true;
    restore.apply_config = false;
    let prepared = prepare_restore(restore).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    assert!(
        prepared.plan.ops.iter().any(|op| skipped_exec(&op.action)),
        "{:?}",
        prepared.plan
    );
    apply_restore(prepared).unwrap();
    assert_eq!(
        fs::read(honest.join("GEMINI.md")).unwrap(),
        b"gemini-body\n"
    );
    assert_eq!(
        fs::read(honest.join("antigravity-cli/keybindings.json")).unwrap(),
        b"keys\n"
    );
    assert!(
        settings_on_disk(&honest).is_empty(),
        "honest restore wrote settings: {:?}",
        settings_on_disk(&honest)
    );

    let hostile_archive = tmp.path().join("downgraded.tar.gz");
    downgrade_exec_class(&request.output, &hostile_archive);
    let shared = tmp.path().join("shared-home");
    let mut attack = restore_request(tmp.path(), &hostile_archive, &tmp.path().join("unused"));
    attack
        .provider_homes
        .insert(ProviderId::Gemini, shared.clone());
    attack
        .provider_homes
        .insert(ProviderId::Antigravity, shared.clone());
    attack.only_providers = Some(vec![ProviderId::Gemini, ProviderId::Antigravity]);
    attack.allow_exec_config = false;
    attack.allow_running = true;
    attack.apply_config = false;
    let prepared = prepare_restore(attack).unwrap();
    assert!(
        !prepared.plan.ops.iter().any(|op| {
            matches!(&op.action, Action::Refuse(reason) if reason.contains("verification"))
        }),
        "downgraded plan was refused before the class gate: {:?}",
        prepared.plan
    );
    if !prepared.plan.has_refusals() {
        apply_restore(prepared).unwrap();
    }
    let written = settings_on_disk(&shared);
    assert!(
        written.is_empty(),
        "plain class wrote executable settings without --allow-exec-config: {written:?}"
    );
}
