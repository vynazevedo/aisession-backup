use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use aisession_backup::model::Manifest;
use aisession_backup::remote::{parse_location, S3Backend};
use aisession_backup::repo::backend::validate_key;
use aisession_backup::repo::{Backend, LocalBackend, Repository};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn manifest() -> Manifest {
    Manifest {
        schema_version: 3,
        created_at: chrono::Utc::now(),
        source_host: "fixture".into(),
        source_os: String::new(),
        claude_version: None,
        sessions: Vec::new(),
        providers: Vec::new(),
        extras: Vec::new(),
        config: Vec::new(),
        checksum: String::new(),
        total_size: 0,
    }
}

fn fresh(password: Option<&str>) -> (tempfile::TempDir, PathBuf, LocalBackend, Repository) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    let backend = LocalBackend::new(&root).unwrap();
    let repo = Repository::init(Box::new(backend.clone()), password).unwrap();
    (temp, root, backend, repo)
}

fn source(dir: &Path, name: &str, bytes: &[u8]) -> (String, PathBuf) {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    (format!("sessions/{name}"), path)
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                out.push(path);
            } else if kind.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn tree_contains(root: &Path, needle: &[u8]) -> bool {
    files_under(root).into_iter().any(|path| {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        fs::read(&path)
            .map(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
            .unwrap_or(false)
    })
}

fn assert_inside(root: &Path, path: &Path) {
    let root = fs::canonicalize(root).unwrap();
    let path = fs::canonicalize(path).unwrap();
    assert!(
        path.starts_with(&root),
        "{} escaped {}",
        path.display(),
        root.display()
    );
}

fn snapshot_file(root: &Path, id: &str) -> PathBuf {
    root.join(format!("snapshots/{id}.json"))
}

fn read_doc(root: &Path, id: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(snapshot_file(root, id)).unwrap()).unwrap()
}

fn write_doc(root: &Path, id: &str, doc: &serde_json::Value) {
    fs::write(snapshot_file(root, id), serde_json::to_vec(doc).unwrap()).unwrap();
}

fn assert_unhealthy_and_unstaged(repo: &Repository, stage: &Path, marker: &[u8]) {
    if let Ok(report) = repo.check(true) {
        assert!(
            !report.missing.is_empty() || !report.corrupt.is_empty(),
            "check reported a healthy repository"
        );
    }
    assert!(
        repo.stage_snapshot("latest", stage).is_err(),
        "tampered snapshot was staged"
    );
    assert!(
        !tree_contains(stage, marker),
        "staged tree contains the payload"
    );
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn digest(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn s3_endpoint() -> Option<String> {
    std::env::var("AISESSION_BACKUP_TEST_S3_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
}

fn s3_at(bucket: &str, prefix: &str) -> S3Backend {
    let location = if prefix.is_empty() {
        format!("s3://{bucket}")
    } else {
        format!("s3://{bucket}/{prefix}")
    };
    S3Backend::new(
        s3_endpoint()
            .expect("AISESSION_BACKUP_TEST_S3_ENDPOINT")
            .parse()
            .unwrap(),
        rusty_s3::UrlStyle::Path,
        parse_location(&location).unwrap(),
        "us-east-1".into(),
        rusty_s3::Credentials::new(
            std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "aisessiontest".into()),
            std::env::var("AWS_SECRET_ACCESS_KEY")
                .unwrap_or_else(|_| "aisessiontest-secret".into()),
        ),
    )
    .unwrap()
}

fn new_bucket(label: &str) -> String {
    let name = unique(label);
    s3_at(&name, "")
        .create_bucket()
        .unwrap_or_else(|err| panic!("create_bucket {name}: {err:#}"));
    name
}

#[test]
fn ingest_and_hand_edited_paths_never_leave_the_repository() {
    let (temp, root, backend, repo) = fresh(None);
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("canary"), b"OUTSIDE-CANARY").unwrap();
    let file = source(temp.path(), "one", b"payload-one");
    let hostile = [
        "../escape",
        "/etc/passwd",
        "nested/../../escape",
        "back\\slash",
        "C:/Windows/system32/evil",
        "C:\\Windows\\system32\\evil",
        "\\\\server\\share\\evil",
        "a//b",
        "a/./b",
        "a/../b",
        "sessions/../../tmp/evil",
        "",
        "bad\nname",
        "sessions/one:stream",
        &"a".repeat(5000),
    ];
    for path in hostile {
        let err = repo
            .write_snapshot(
                &manifest(),
                &[(path.to_string(), file.1.clone())],
                "real-host",
            )
            .unwrap_err();
        assert!(
            repo.list_snapshots().unwrap().is_empty(),
            "{path:?} was stored despite {err:#}"
        );
    }
    assert!(repo
        .write_snapshot(
            &manifest(),
            &[file.clone(), ("sessions/one/child".into(), file.1.clone())],
            "real-host",
        )
        .is_err());
    assert!(repo.list_snapshots().unwrap().is_empty());
    let summary = repo
        .write_snapshot(&manifest(), &[file], "real-host")
        .unwrap();
    let original = fs::read(snapshot_file(&root, &summary.snapshot_id)).unwrap();
    let stage = temp.path().join("stage");
    for path in [
        "../escape",
        "/etc/passwd",
        "nested/../../escape",
        "back\\slash",
        "C:/Windows/system32/evil",
        "C:\\Windows\\system32\\evil",
        "\\\\server\\share\\evil",
        "a//b",
        "a/./b",
        "foo/../bar",
        "bad\nname",
        "sessions/one:stream",
        "",
    ] {
        let mut doc = read_doc(&root, &summary.snapshot_id);
        let entry = doc["files"]["sessions/one"].clone();
        doc["files"] = serde_json::json!({ path: entry });
        write_doc(&root, &summary.snapshot_id, &doc);
        assert!(
            repo.stage_snapshot("latest", &stage).is_err(),
            "{path:?} was staged"
        );
        assert!(!temp.path().join("escape").exists());
        assert!(!temp.path().join("passwd").exists());
        assert!(!temp.path().join("evil").exists());
        assert!(!tree_contains(&stage, b"payload-one"));
        assert_eq!(fs::read(outside.join("canary")).unwrap(), b"OUTSIDE-CANARY");
    }
    fs::write(snapshot_file(&root, &summary.snapshot_id), &original).unwrap();
    let mut both = read_doc(&root, &summary.snapshot_id);
    let entry = both["files"]["sessions/one"].clone();
    both["files"] = serde_json::json!({
        "sessions": entry.clone(),
        "sessions/one": entry,
    });
    write_doc(&root, &summary.snapshot_id, &both);
    assert!(repo.stage_snapshot("latest", &stage).is_err());
    assert!(!stage.join("sessions").join("one").exists());
    assert!(!temp.path().join("sessions").exists());
    fs::write(snapshot_file(&root, &summary.snapshot_id), &original).unwrap();
    let body = String::from_utf8(original.clone()).unwrap();
    let entry_json = serde_json::to_string(
        &serde_json::from_slice::<serde_json::Value>(&original).unwrap()["files"]["sessions/one"],
    )
    .unwrap();
    let needle = format!("\"sessions/one\":{entry_json}");
    assert!(body.contains(&needle), "snapshot shape changed");
    let duplicated = body.replacen(&needle, &format!("{needle},{needle}"), 1);
    fs::write(snapshot_file(&root, &summary.snapshot_id), duplicated).unwrap();
    if let Ok((_, paths)) = repo.stage_snapshot("latest", &stage) {
        assert_eq!(paths.len(), 1);
        let only = paths.values().next().unwrap();
        assert_inside(&stage, only);
        assert_eq!(fs::read(only).unwrap(), b"payload-one");
    }
    assert!(!tree_contains(temp.path(), b"OUTSIDE-CANARY-WRITTEN"));
    assert_eq!(backend.get("writer.lock").unwrap(), None);
    for id in ["../config", "latest/../../x", "C:foo", "a/b", ""] {
        assert!(repo.stage_snapshot(id, &stage).is_err(), "{id}");
    }
}

#[test]
fn size_lies_and_bad_chunk_identifiers_are_rejected() {
    let (temp, root, _backend, repo) = fresh(None);
    let marker = b"size-marker-payload";
    let summary = repo
        .write_snapshot(&manifest(), &[source(temp.path(), "one", marker)], "host")
        .unwrap();
    let original = fs::read(snapshot_file(&root, &summary.snapshot_id)).unwrap();
    let stage = temp.path().join("stage");
    let lies = [
        serde_json::json!(u64::MAX),
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!(4_194_304u64 + 1),
    ];
    for lie in lies {
        let mut doc = read_doc(&root, &summary.snapshot_id);
        doc["files"]["sessions/one"]["size"] = lie;
        write_doc(&root, &summary.snapshot_id, &doc);
        assert_unhealthy_and_unstaged(&repo, &stage, marker);
    }
    fs::write(snapshot_file(&root, &summary.snapshot_id), &original).unwrap();
    let ids = [
        "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "abcd",
        "../data/aa/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ab/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "",
    ];
    for id in ids {
        let mut doc = read_doc(&root, &summary.snapshot_id);
        doc["files"]["sessions/one"]["chunks"] = serde_json::json!([id]);
        write_doc(&root, &summary.snapshot_id, &doc);
        assert!(repo.stage_snapshot("latest", &stage).is_err(), "{id}");
        assert!(!tree_contains(&stage, marker));
    }
    fs::write(snapshot_file(&root, &summary.snapshot_id), &original).unwrap();
    let doc = read_doc(&root, &summary.snapshot_id);
    let id = doc["files"]["sessions/one"]["chunks"][0]
        .as_str()
        .unwrap()
        .to_string();
    let from = root.join("data").join(&id[..2]).join(&id);
    let wrong_prefix = if &id[..2] == "ff" { "aa" } else { "ff" };
    let to = root.join("data").join(wrong_prefix).join(&id);
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(&from, &to).unwrap();
    assert!(repo.stage_snapshot("latest", &stage).is_err());
    assert!(!tree_contains(&stage, marker));
    assert!(to.is_file());
    let outside = temp.path().join("stolen-chunk");
    fs::write(&outside, marker).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), marker);
}

#[test]
fn reordered_truncated_and_expanded_chunks_are_not_staged() {
    let (temp, root, _backend, repo) = fresh(None);
    let mut state = 0x1234_5678_9abcu64;
    let large: Vec<u8> = (0..5_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let summary = repo
        .write_snapshot(&manifest(), &[source(temp.path(), "large", &large)], "host")
        .unwrap();
    let stage = temp.path().join("stage");
    let mut doc = read_doc(&root, &summary.snapshot_id);
    let mut chunks = doc["files"]["sessions/large"]["chunks"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        chunks.len() >= 2,
        "5MB file produced {} chunks",
        chunks.len()
    );
    chunks.swap(0, 1);
    doc["files"]["sessions/large"]["chunks"] = serde_json::Value::Array(chunks);
    write_doc(&root, &summary.snapshot_id, &doc);
    assert_unhealthy_and_unstaged(&repo, &stage, &large[..32]);

    let (temp, _root, backend, repo) = fresh(None);
    let marker = b"alpha-unique-marker";
    let other = b"beta-unique-marker";
    repo.write_snapshot(
        &manifest(),
        &[
            source(temp.path(), "alpha", marker),
            source(temp.path(), "beta", other),
        ],
        "host",
    )
    .unwrap();
    let keys = backend.list("data/").unwrap();
    assert_eq!(keys.len(), 2);
    let first = backend.get(&keys[0]).unwrap().unwrap();
    let second = backend.get(&keys[1]).unwrap().unwrap();
    backend.put(&keys[0], &second).unwrap();
    let stage = temp.path().join("stage");
    assert_unhealthy_and_unstaged(&repo, &stage, marker);
    backend.put(&keys[0], &first).unwrap();
    let mut truncated = first.clone();
    truncated.truncate(4);
    backend.put(&keys[0], &truncated).unwrap();
    assert_unhealthy_and_unstaged(&repo, &stage, marker);
    let mut trailed = first.clone();
    trailed.extend_from_slice(b"TAIL");
    backend.put(&keys[0], &trailed).unwrap();
    assert_unhealthy_and_unstaged(&repo, &stage, marker);
    let mut bomb = DeflateEncoder::new(Vec::new(), Compression::default());
    bomb.write_all(&vec![0u8; 8 * 1024 * 1024]).unwrap();
    let bomb = bomb.finish().unwrap();
    assert!(bomb.len() < 1024 * 1024, "bomb setup did not compress");
    backend.put(&keys[0], &bomb).unwrap();
    let started = std::time::Instant::now();
    assert!(repo.stage_snapshot("latest", &stage).is_err());
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "deflate expansion was not bounded"
    );
    for path in files_under(&stage) {
        let len = fs::symlink_metadata(&path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert!(len < 1024 * 1024, "{} held {len} bytes", path.display());
    }
    assert!(!tree_contains(&stage, marker));
    let huge = vec![b'A'; 5 * 1024 * 1024];
    backend.put(&keys[0], &huge).unwrap();
    assert!(repo.stage_snapshot("latest", &stage).is_err());
    assert!(!tree_contains(&stage, marker));
}

#[test]
fn foreign_chunks_and_replaced_recipients_do_not_decrypt() {
    let marker_a = b"repo-a-plaintext-marker";
    let marker_b = b"repo-b-plaintext-marker";
    let (temp_a, _root_a, backend_a, repo_a) = fresh(None);
    let (temp_b, _root_b, backend_b, repo_b) = fresh(None);
    repo_a
        .write_snapshot(&manifest(), &[source(temp_a.path(), "a", marker_a)], "a")
        .unwrap();
    repo_b
        .write_snapshot(&manifest(), &[source(temp_b.path(), "b", marker_b)], "b")
        .unwrap();
    let key_a = backend_a.list("data/").unwrap().remove(0);
    let foreign = backend_b
        .get(&backend_b.list("data/").unwrap().remove(0))
        .unwrap()
        .unwrap();
    backend_a.put(&key_a, &foreign).unwrap();
    let stage = temp_a.path().join("stage");
    assert_unhealthy_and_unstaged(&repo_a, &stage, marker_b);
    assert!(!tree_contains(&stage, marker_a));

    let (temp, root, backend, _repo) = fresh(Some("correct-horse"));
    let marker = b"encrypted-body-marker";
    Repository::open(Box::new(backend.clone()), Some("correct-horse"))
        .unwrap()
        .write_snapshot(&manifest(), &[source(temp.path(), "one", marker)], "host")
        .unwrap();
    let (attacker_temp, attacker_root, _attacker_backend, _attacker_repo) =
        fresh(Some("attacker-pass"));
    let _ = attacker_temp;
    let mut config: serde_json::Value =
        serde_json::from_slice(&backend.get("config.json").unwrap().unwrap()).unwrap();
    config["encryption"]["recipient"] = serde_json::json!("age1not-the-real-recipient");
    backend
        .put("config.json", &serde_json::to_vec(&config).unwrap())
        .unwrap();
    assert!(
        Repository::open(Box::new(backend.clone()), Some("correct-horse")).is_err(),
        "replaced recipient still unlocked"
    );
    let attacker_config: serde_json::Value =
        serde_json::from_slice(&fs::read(attacker_root.join("config.json")).unwrap()).unwrap();
    config["encryption"] = attacker_config["encryption"].clone();
    backend
        .put("config.json", &serde_json::to_vec(&config).unwrap())
        .unwrap();
    assert!(Repository::open(Box::new(backend.clone()), Some("correct-horse")).is_err());
    let opened = Repository::open(Box::new(backend.clone()), Some("attacker-pass")).unwrap();
    assert!(opened.stage_snapshot("latest", &stage).is_err());
    assert!(!tree_contains(&stage, marker));
    config["encryption"] = serde_json::Value::Null;
    backend
        .put("config.json", &serde_json::to_vec(&config).unwrap())
        .unwrap();
    let plaintext = Repository::open(Box::new(backend.clone()), None).unwrap();
    assert!(plaintext.stage_snapshot("latest", &stage).is_err());
    assert!(!tree_contains(&stage, marker));
    assert!(!tree_contains(&root, marker));

    let (temp_a, _root_a, backend_a, repo_a) = fresh(Some("same-phrase"));
    let (temp_b, _root_b, backend_b, repo_b) = fresh(Some("same-phrase"));
    repo_a
        .write_snapshot(
            &manifest(),
            &[source(temp_a.path(), "a", b"shared-plain-a")],
            "a",
        )
        .unwrap();
    repo_b
        .write_snapshot(
            &manifest(),
            &[source(temp_b.path(), "b", b"shared-plain-b")],
            "b",
        )
        .unwrap();
    let key = backend_a.list("data/").unwrap().remove(0);
    let swapped = backend_b
        .get(&backend_b.list("data/").unwrap().remove(0))
        .unwrap()
        .unwrap();
    backend_a.put(&key, &swapped).unwrap();
    assert!(repo_a
        .stage_snapshot("latest", &temp_a.path().join("stage"))
        .is_err());
    assert!(!tree_contains(
        &temp_a.path().join("stage"),
        b"shared-plain-b"
    ));
}

#[test]
fn forged_locks_block_writers_and_prune_keeps_newer_data() {
    let (temp, root, backend, repo) = fresh(None);
    let file = source(temp.path(), "one", b"lock-body");
    for body in [
        b"2000-01-01T00:00:00Z".as_slice(),
        b"2999-01-01T00:00:00Z".as_slice(),
        b"forged".as_slice(),
        b"".as_slice(),
    ] {
        backend.put("writer.lock", body).unwrap();
        assert!(repo
            .write_snapshot(&manifest(), std::slice::from_ref(&file), "host")
            .is_err());
        assert!(repo.prune(0, true).is_err());
        assert!(repo.prune(0, false).is_err());
        assert_eq!(backend.get("writer.lock").unwrap().unwrap(), body);
        assert!(repo.list_snapshots().unwrap().is_empty());
    }
    backend.delete("writer.lock").unwrap();
    let real = repo
        .write_snapshot(&manifest(), std::slice::from_ref(&file), "real-host")
        .unwrap();
    assert!(!backend.exists("writer.lock").unwrap());
    let newer = repo
        .write_snapshot(
            &manifest(),
            &[source(temp.path(), "two", b"newer-body")],
            "real-host",
        )
        .unwrap();
    let before = backend.list("").unwrap();
    let dry = repo.prune(100, true).unwrap();
    assert!(dry.snapshots_removed.is_empty());
    assert!(dry.chunks_removed.is_empty());
    assert_eq!(backend.list("").unwrap(), before);
    let kept = repo.prune(usize::MAX, false).unwrap();
    assert!(kept.snapshots_removed.is_empty());
    assert!(kept.chunks_removed.is_empty());
    assert_eq!(repo.list_snapshots().unwrap().len(), 2);

    let mut shadow = read_doc(&root, &real.snapshot_id);
    shadow["id"] = serde_json::json!("zzzz-shadow");
    shadow["created_at"] = serde_json::json!("2000-01-01T00:00:00Z");
    shadow["host"] = serde_json::json!("shadow-host");
    fs::write(
        root.join("snapshots/zzzz-shadow.json"),
        serde_json::to_vec(&shadow).unwrap(),
    )
    .unwrap();
    assert!(repo.list_snapshots().is_err());
    assert!(repo
        .stage_snapshot("latest", &temp.path().join("shadow-stage"))
        .is_err());
    assert!(repo.prune(1, false).is_err());
    assert!(backend
        .exists(&format!("snapshots/{}.json", newer.snapshot_id))
        .unwrap());
    assert!(backend
        .exists(&format!("snapshots/{}.json", real.snapshot_id))
        .unwrap());

    let mut renamed = read_doc(&root, &real.snapshot_id);
    let future = "29991231T235959999999999Z-0123456789abcdef";
    renamed["id"] = serde_json::json!(future);
    fs::write(
        root.join(format!("snapshots/{future}.json")),
        serde_json::to_vec(&renamed).unwrap(),
    )
    .unwrap();
    assert!(repo.list_snapshots().is_err());
    fs::remove_file(root.join(format!("snapshots/{future}.json"))).unwrap();
    fs::remove_file(root.join("snapshots/zzzz-shadow.json")).unwrap();

    let listed = repo.list_snapshots().unwrap();
    assert_eq!(
        listed[0].id, newer.snapshot_id,
        "latest id is {}, host {}",
        listed[0].id, listed[0].host
    );
    assert_eq!(listed[0].host, "real-host");
    let pruned = repo.prune(1, false).unwrap();
    assert_eq!(pruned.snapshots_removed, vec![real.snapshot_id.clone()]);
    let stage = temp.path().join("stage");
    let (_manifest, paths) = repo.stage_snapshot(&newer.snapshot_id, &stage).unwrap();
    assert_eq!(fs::read(&paths["sessions/two"]).unwrap(), b"newer-body");
    assert_inside(&stage, &paths["sessions/two"]);
}

#[test]
fn prune_racing_a_writer_leaves_every_remaining_snapshot_readable() {
    let (temp, _root, backend, repo) = fresh(None);
    let original = source(temp.path(), "original", b"original-snapshot-body");
    let first = repo
        .write_snapshot(&manifest(), std::slice::from_ref(&original), "host")
        .unwrap();
    let extra = source(temp.path(), "extra", &vec![7u8; 256 * 1024]);
    let barrier = Arc::new(Barrier::new(2));
    let writer_backend = backend.clone();
    let writer_barrier = barrier.clone();
    let extra_for_writer = extra.clone();
    let writer = std::thread::spawn(move || {
        let repo = Repository::open(Box::new(writer_backend), None).unwrap();
        writer_barrier.wait();
        repo.write_snapshot(&manifest(), &[extra_for_writer], "host")
    });
    let pruner_backend = backend.clone();
    let pruner = std::thread::spawn(move || {
        let repo = Repository::open(Box::new(pruner_backend), None).unwrap();
        barrier.wait();
        repo.prune(1, false)
    });
    let wrote = writer.join().unwrap();
    let pruned = pruner.join().unwrap();
    let _ = (wrote, pruned, first);
    let repo = Repository::open(Box::new(backend.clone()), None).unwrap();
    let listed = repo.list_snapshots().unwrap();
    assert!(!listed.is_empty(), "race removed every snapshot");
    for info in &listed {
        let stage = temp.path().join(format!("stage-{}", info.id));
        let (_manifest, paths) = repo.stage_snapshot(&info.id, &stage).unwrap_or_else(|err| {
            panic!("snapshot {} did not stage after the race: {err:#}", info.id)
        });
        for path in paths.values() {
            assert_inside(&stage, path);
        }
    }
    let report = repo.check(true).unwrap();
    assert!(
        report.missing.is_empty() && report.corrupt.is_empty(),
        "missing {:?} corrupt {:?}",
        report.missing,
        report.corrupt
    );
    assert!(!backend.exists("writer.lock").unwrap());
}

#[cfg(unix)]
#[test]
fn symlinks_and_snapshot_directories_are_not_followed() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    let backend = LocalBackend::new(&root).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let canary = outside.join("canary");
    fs::write(&canary, b"SYMLINK-CANARY-VALUE").unwrap();
    symlink(&canary, root.join("config.json")).unwrap();
    assert!(Repository::init(Box::new(backend), None).is_err());
    assert_eq!(fs::read(&canary).unwrap(), b"SYMLINK-CANARY-VALUE");

    let (temp, root, backend, repo) = fresh(None);
    let marker = b"symlink-snapshot-body";
    let summary = repo
        .write_snapshot(&manifest(), &[source(temp.path(), "one", marker)], "host")
        .unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let canary = outside.join("canary");
    fs::write(&canary, b"SYMLINK-CANARY-VALUE").unwrap();
    let malicious = outside.join("malicious.json");
    fs::write(&malicious, b"{\"files\":{\"../pwned\":{}}}").unwrap();
    let keys = backend.list("data/").unwrap();
    let chunk = root.join(&keys[0]);
    fs::remove_file(&chunk).unwrap();
    symlink(&canary, &chunk).unwrap();
    let stage = temp.path().join("stage");
    assert!(repo.stage_snapshot("latest", &stage).is_err());
    assert!(backend.list("").is_err());
    assert_eq!(fs::read(&canary).unwrap(), b"SYMLINK-CANARY-VALUE");
    assert!(!temp.path().join("pwned").exists());
    assert!(!tree_contains(&stage, marker));
    fs::remove_file(&chunk).unwrap();
    fs::write(&chunk, b"restored-later").unwrap();
    let link_dir = temp.path().join("linked-dir");
    fs::create_dir_all(&link_dir).unwrap();
    symlink(&link_dir, root.join("linked")).unwrap();
    assert!(backend.put("linked/file", b"pwned").is_err());
    assert_eq!(fs::read_dir(&link_dir).unwrap().count(), 0);

    fs::remove_file(root.join("linked")).unwrap();
    let snapshot = snapshot_file(&root, &summary.snapshot_id);
    fs::remove_file(&snapshot).unwrap();
    symlink(&malicious, &snapshot).unwrap();
    assert!(repo.stage_snapshot(&summary.snapshot_id, &stage).is_err());
    assert!(!temp.path().join("pwned").exists());
    assert_eq!(
        fs::read(&malicious).unwrap(),
        b"{\"files\":{\"../pwned\":{}}}"
    );
    fs::remove_file(&snapshot).unwrap();

    let (temp, root, backend, repo) = fresh(None);
    let summary = repo
        .write_snapshot(
            &manifest(),
            &[source(temp.path(), "one", b"directory-entry-body")],
            "host",
        )
        .unwrap();
    let nested = root.join("snapshots/zzzz-dir.json/nested");
    fs::create_dir_all(&nested).unwrap();
    let planted = nested.join("payload");
    fs::write(&planted, b"DIRECTORY-ENTRY-CANARY").unwrap();
    if let Ok(listed) = repo.list_snapshots() {
        assert!(listed.iter().all(|info| !info.id.contains('/')));
        assert!(listed.iter().any(|info| info.id == summary.snapshot_id));
    }
    if let Ok(report) = repo.prune(10, false) {
        assert!(!report
            .snapshots_removed
            .iter()
            .any(|id| id == &summary.snapshot_id));
    }
    let stage = temp.path().join("stage");
    let (_manifest, paths) = repo
        .stage_snapshot(&summary.snapshot_id, &stage)
        .unwrap_or_else(|err| panic!("real snapshot was lost: {err:#}"));
    assert_eq!(
        fs::read(&paths["sessions/one"]).unwrap(),
        b"directory-entry-body"
    );
    assert_eq!(fs::read(&planted).unwrap(), b"DIRECTORY-ENTRY-CANARY");
    assert!(!backend.exists("writer.lock").unwrap());
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    claude: PathBuf,
    data: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        let claude = home.join(".claude");
        let data = root.join("data");
        let repo = root.join("repository");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".maestri")).unwrap();
        fs::create_dir_all(&data).unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
            claude,
            data,
            repo,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_aisession-backup"))
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("MAESTRI_HOME", self.home.join(".maestri"))
            .env("AISESSION_BACKUP_DATA_DIR", &self.data)
            .env("NO_COLOR", "1")
            .env_remove("AISESSION_BACKUP_PASSPHRASE")
            .output()
            .unwrap();
        output
    }
}

fn stored_deflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = data;
    loop {
        let take = rest.len().min(65535);
        let last = take == rest.len();
        out.push(if last { 1 } else { 0 });
        let len = take as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&rest[..take]);
        if last {
            break;
        }
        rest = &rest[take..];
    }
    out
}

#[test]
fn restore_repo_stays_inside_dest_and_gates_exec_and_credentials() {
    let fixture = Fixture::new();
    let id = "11111111-1111-4111-8111-111111111111";
    let project = fixture.claude.join("projects/-work-notes");
    fs::create_dir_all(&project).unwrap();
    let session = format!(
        "{{\"type\":\"user\",\"cwd\":\"/work/notes\",\"sessionId\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"session-body-marker\"}}}}\n"
    );
    fs::write(project.join(format!("{id}.jsonl")), session).unwrap();
    fs::write(
        fixture.claude.join("settings.json"),
        "{\"hooks\":{\"PreToolUse\":[{\"command\":\"HOOK-CMD-should-not-land\"}]}}\n",
    )
    .unwrap();
    fs::create_dir_all(fixture.claude.join("shell-snapshots")).unwrap();
    fs::write(
        fixture.claude.join("shell-snapshots/evil.sh"),
        "echo HOOK-SCRIPT-should-not-land\n",
    )
    .unwrap();
    fs::write(
        fixture.claude.join(".credentials.json"),
        "{\"token\":\"CRED-SECRET-should-not-land\"}\n",
    )
    .unwrap();
    let repo = fixture.repo.to_str().unwrap();
    let init = fixture.run(&["repo", "init", repo]);
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let backup = fixture.run(&[
        "backup",
        "--repo",
        repo,
        "--provider",
        "claude",
        "--config",
        "--full",
        "--yes",
    ]);
    assert!(
        backup.status.success(),
        "backup: {}",
        String::from_utf8_lossy(&backup.stderr)
    );
    assert!(!tree_contains(
        &fixture.repo,
        b"CRED-SECRET-should-not-land"
    ));
    let creds = fixture.run(&[
        "backup",
        "--repo",
        repo,
        "--provider",
        "claude",
        "--include-credentials",
        "--yes",
    ]);
    assert!(
        !creds.status.success(),
        "plaintext repo accepted credentials: {}",
        String::from_utf8_lossy(&creds.stderr)
    );
    assert!(!tree_contains(
        &fixture.repo,
        b"CRED-SECRET-should-not-land"
    ));

    let secret = b"{\"token\":\"CRED-SECRET-should-not-land\"}\n";
    let chunk_id = digest(secret);
    let chunk_path = fixture
        .repo
        .join("data")
        .join(&chunk_id[..2])
        .join(&chunk_id);
    fs::create_dir_all(chunk_path.parent().unwrap()).unwrap();
    fs::write(&chunk_path, stored_deflate(secret)).unwrap();
    let snap_dir = fixture.repo.join("snapshots");
    let snap_name = fs::read_dir(&snap_dir)
        .unwrap()
        .find_map(|entry| {
            let name = entry.ok()?.file_name();
            let name = name.to_str()?.to_string();
            name.ends_with(".json").then_some(name)
        })
        .unwrap();
    let snap_path = snap_dir.join(&snap_name);
    let mut doc: serde_json::Value =
        serde_json::from_slice(&fs::read(&snap_path).unwrap()).unwrap();
    doc["files"]["config/.credentials.json"] = serde_json::json!({
        "size": secret.len(),
        "sha256": chunk_id,
        "chunks": [chunk_id],
    });
    fs::write(&snap_path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let probe = fixture.root.join("probe-stage");
    let opened =
        Repository::open(Box::new(LocalBackend::new(&fixture.repo).unwrap()), None).unwrap();
    let (_manifest, paths) = opened.stage_snapshot("latest", &probe).unwrap();
    assert_eq!(
        fs::read(&paths["config/.credentials.json"]).unwrap(),
        secret
    );

    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("canary"), b"DEST-OUTSIDE-CANARY").unwrap();
    let escaped = fixture.root.join("escaped-map");
    fs::create_dir_all(&escaped).unwrap();
    let bad_dest = fixture.root.join("bad-dest");
    let bad_map = format!(
        "/work/notes:{}",
        fixture.root.join("..").join("escaped-map").display()
    );
    let rejected = fixture.run(&[
        "restore",
        "--repo",
        repo,
        "--dest",
        bad_dest.to_str().unwrap(),
        "--provider",
        "claude",
        "--config",
        "--allow-running",
        "--mappings",
        &bad_map,
        "--yes",
    ]);
    assert!(
        !rejected.status.success(),
        "mapping with .. was accepted: {}",
        String::from_utf8_lossy(&rejected.stdout)
    );
    assert!(!tree_contains(&escaped, b"session-body-marker"));
    assert!(!tree_contains(&bad_dest, b"session-body-marker"));
    assert_eq!(
        fs::read(outside.join("canary")).unwrap(),
        b"DEST-OUTSIDE-CANARY"
    );

    let dest = fixture.root.join("dest");
    let mapped = fixture.root.join("mapped-outside");
    fs::create_dir_all(&mapped).unwrap();
    let mapping = format!("/work/notes:{}", mapped.display());
    let planned = fixture.run(&[
        "restore",
        "--repo",
        repo,
        "--dest",
        dest.to_str().unwrap(),
        "--provider",
        "claude",
        "--config",
        "--allow-running",
        "--mappings",
        &mapping,
        "--json",
        "--dry-run",
    ]);
    let plan = String::from_utf8_lossy(&planned.stdout);
    assert!(
        planned.status.success(),
        "plan failed: {} {plan}",
        String::from_utf8_lossy(&planned.stderr)
    );
    assert!(plan.contains("requires --allow-exec-config"), "{plan}");
    assert!(plan.contains("requires --allow-credentials"), "{plan}");
    let applied = fixture.run(&[
        "restore",
        "--repo",
        repo,
        "--dest",
        dest.to_str().unwrap(),
        "--provider",
        "claude",
        "--config",
        "--allow-running",
        "--mappings",
        &mapping,
        "--yes",
    ]);
    assert!(
        applied.status.success(),
        "restore: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(tree_contains(&dest, b"session-body-marker"));
    assert!(!dest.join("settings.json").exists());
    assert!(!dest.join(".credentials.json").exists());
    assert!(!dest.join("shell-snapshots").join("evil.sh").exists());
    assert!(!tree_contains(&mapped, b"session-body-marker"));
    assert!(!tree_contains(&outside, b"session-body-marker"));
    assert!(!tree_contains(&dest, b"HOOK-CMD-should-not-land"));
    assert!(!tree_contains(&dest, b"CRED-SECRET-should-not-land"));
    assert!(!tree_contains(&dest, b"HOOK-SCRIPT-should-not-land"));
    for path in files_under(&dest) {
        if fs::symlink_metadata(&path).unwrap().is_file() {
            assert_inside(&dest, &path);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let linked = fixture.root.join("linked-dest");
        fs::create_dir_all(&linked).unwrap();
        symlink(&outside, linked.join("projects")).unwrap();
        let through = fixture.run(&[
            "restore",
            "--repo",
            repo,
            "--dest",
            linked.to_str().unwrap(),
            "--provider",
            "claude",
            "--config",
            "--allow-running",
            "--yes",
        ]);
        assert!(
            !through.status.success(),
            "restore followed a symlink: {}",
            String::from_utf8_lossy(&through.stderr)
        );
        assert_eq!(
            fs::read(outside.join("canary")).unwrap(),
            b"DEST-OUTSIDE-CANARY"
        );
        assert!(!tree_contains(&outside, b"session-body-marker"));
    }

    doc["files"] = serde_json::json!({
        "../escape": doc["files"]["config/.credentials.json"].clone(),
    });
    fs::write(&snap_path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let escaped_dest = fixture.root.join("after-tamper");
    let tampered = fixture.run(&[
        "restore",
        "--repo",
        repo,
        "--dest",
        escaped_dest.to_str().unwrap(),
        "--provider",
        "claude",
        "--allow-running",
        "--yes",
    ]);
    assert!(!tampered.status.success());
    assert!(!fixture.root.join("escape").exists());
    assert!(!tree_contains(
        &escaped_dest,
        b"CRED-SECRET-should-not-land"
    ));
    assert!(!tree_contains(&outside, b"CRED-SECRET-should-not-land"));
}

#[test]
fn cli_snapshot_list_strips_terminal_escapes_from_the_host() {
    let fixture = Fixture::new();
    let repo_path = fixture.repo.to_str().unwrap();
    let init = fixture.run(&["repo", "init", repo_path]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let backend = LocalBackend::new(&fixture.repo).unwrap();
    let repo = Repository::open(Box::new(backend), None).unwrap();
    let file = source(fixture.root.as_path(), "one", b"host-body");
    repo.write_snapshot(&manifest(), &[file], "evil\nINJECT\u{1b}[31mRED")
        .unwrap();
    let listed = fixture.run(&["repo", "snapshots", repo_path]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        !listed.stdout.contains(&0x1b),
        "raw escape in {:?}",
        String::from_utf8_lossy(&listed.stdout)
    );
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains("evil INJECT"), "{text}");
    assert!(!text.contains("INJECTRED") || text.contains("evil INJECT"));
}

#[test]
fn s3_keys_prefixes_and_listing_cannot_escape_the_repository() {
    if s3_endpoint().is_none() {
        eprintln!("skipped: AISESSION_BACKUP_TEST_S3_ENDPOINT is not set");
        return;
    }
    for bad in [
        "s3://bucket/../x",
        "s3://bucket/a/../../b",
        "s3://bucket/a\\b",
        "s3://UPPER/x",
        "s3://-bucket/x",
        "s3://bucket/a\u{7}",
        "http://bucket/x",
    ] {
        assert!(parse_location(bad).is_err(), "{bad}");
    }
    assert!(validate_key("../x").is_err());
    assert!(validate_key("/abs").is_err());
    assert!(validate_key("a\\b").is_err());
    assert!(validate_key("C:/windows").is_err());

    let bucket = new_bucket("keys");
    let root = s3_at(&bucket, "");
    root.put("team/repo-evil/secret", b"EVIL-SECRET-BYTES")
        .unwrap();
    root.put("team/other/secret", b"OTHER-SECRET-BYTES")
        .unwrap();
    let store = s3_at(&bucket, "team/repo");
    store.put("inside", b"INSIDE-BYTES").unwrap();
    for bad in [
        "../repo-evil/secret",
        "/team/repo-evil/secret",
        "a/../../other/secret",
        "a\\b",
        "C:/windows",
        "",
        "bad\nkey",
        &"k".repeat(5000),
    ] {
        assert!(store.put(bad, b"nope").is_err(), "put {bad}");
        assert!(store.get(bad).is_err(), "get {bad}");
        assert!(store.delete(bad).is_err(), "delete {bad}");
    }
    let listed = store.list("").unwrap();
    assert!(listed.iter().any(|key| key == "inside"));
    assert!(listed.iter().all(|key| validate_key(key).is_ok()));
    assert!(listed.iter().all(|key| !key.contains("..")));
    assert!(!listed.iter().any(|key| key.contains("repo-evil")));
    assert!(!listed.iter().any(|key| key.contains("other/secret")));
    for key in &listed {
        let bytes = store.get(key).unwrap().unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("EVIL-SECRET-BYTES"), "{key}");
        assert!(!text.contains("OTHER-SECRET-BYTES"), "{key}");
    }
    assert!(store.get("repo-evil/secret").unwrap().is_none());
    if let Ok(keys) = store.list("../") {
        for key in keys {
            assert!(validate_key(&key).is_ok(), "{key}");
            assert!(!key.contains(".."), "{key}");
        }
    }
    let root_keys = root.list("").unwrap();
    assert!(root_keys.iter().any(|key| key == "team/repo-evil/secret"));
    assert!(!root_keys.iter().any(|key| key.contains("..")));

    let page_bucket = new_bucket("page");
    let page = s3_at(&page_bucket, "team/repo");
    s3_at(&page_bucket, "")
        .put("team/repo-evil/secret", b"PAGE-EVIL")
        .unwrap();
    for index in 0..1001 {
        page.put(&format!("p/{index:04}"), b"x").unwrap();
    }
    let page_keys = page.list("p/").unwrap();
    assert_eq!(page_keys.len(), 1001);
    assert!(page_keys.iter().any(|key| key == "p/0000"));
    assert!(page_keys.iter().any(|key| key == "p/1000"));
    assert!(page_keys.iter().all(|key| key.starts_with("p/")));
    assert!(!page.list("").unwrap().iter().any(|key| {
        page.get(key)
            .ok()
            .flatten()
            .is_some_and(|bytes| bytes == b"PAGE-EVIL")
    }));

    assert!(page.put_if_absent("writer.lock", b"owner-a").unwrap());
    assert!(!page.put_if_absent("writer.lock", b"owner-b").unwrap());
    assert_eq!(page.get("writer.lock").unwrap().unwrap(), b"owner-a");
    let barrier = Arc::new(Barrier::new(8));
    let mut threads = Vec::new();
    for index in 0..8 {
        let barrier = barrier.clone();
        let bucket = page_bucket.clone();
        threads.push(std::thread::spawn(move || {
            let store = s3_at(&bucket, "team/repo");
            barrier.wait();
            store.put_if_absent("race.lock", &[index; 32]).unwrap()
        }));
    }
    let winners = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
    let winner = page.get("race.lock").unwrap().unwrap();
    assert_eq!(winner.len(), 32);
    assert!(winner.iter().all(|byte| *byte == winner[0]));
}

#[test]
fn s3_repository_rejects_a_tampered_snapshot_and_a_forged_lock() {
    if s3_endpoint().is_none() {
        eprintln!("skipped: AISESSION_BACKUP_TEST_S3_ENDPOINT is not set");
        return;
    }
    let bucket = new_bucket("repo");
    let location_backend = s3_at(&bucket, "team/repo");
    let repo = Repository::init(Box::new(location_backend), None).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let marker = b"s3-snapshot-marker";
    let summary = repo
        .write_snapshot(
            &manifest(),
            &[source(temp.path(), "one", marker)],
            "s3-host",
        )
        .unwrap();
    let stage = temp.path().join("stage");
    let (_manifest, paths) = repo.stage_snapshot("latest", &stage).unwrap();
    assert_eq!(fs::read(&paths["sessions/one"]).unwrap(), marker);
    assert_inside(&stage, &paths["sessions/one"]);

    let store = s3_at(&bucket, "team/repo");
    let key = format!("snapshots/{}.json", summary.snapshot_id);
    let mut doc: serde_json::Value =
        serde_json::from_slice(&store.get(&key).unwrap().unwrap()).unwrap();
    let entry = doc["files"]["sessions/one"].clone();
    doc["files"] = serde_json::json!({ "../escape": entry });
    store.put(&key, &serde_json::to_vec(&doc).unwrap()).unwrap();
    assert!(repo
        .stage_snapshot("latest", &temp.path().join("bad"))
        .is_err());
    assert!(!temp.path().join("escape").exists());
    assert!(!tree_contains(&temp.path().join("bad"), marker));

    store
        .put("writer.lock", b"stale-2000-01-01T00:00:00Z")
        .unwrap();
    assert!(repo
        .write_snapshot(
            &manifest(),
            &[source(temp.path(), "two", b"should-not-land")],
            "s3-host",
        )
        .is_err());
    assert!(repo.prune(0, false).is_err());
    assert_eq!(
        store.get("writer.lock").unwrap().unwrap(),
        b"stale-2000-01-01T00:00:00Z"
    );
    let listed = store.list("snapshots/").unwrap();
    assert_eq!(listed, vec![key]);
    assert!(!store
        .list("")
        .unwrap()
        .iter()
        .any(|key| key.contains("should-not-land")));
}

#[test]
fn plain_http_to_a_non_loopback_host_is_refused() {
    struct RestoreEnv {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl RestoreEnv {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let saved = pairs
                .iter()
                .map(|(key, value)| {
                    let previous = std::env::var_os(key);
                    std::env::set_var(key, value);
                    (*key, previous)
                })
                .collect();
            Self { saved }
        }
    }
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (key, previous) in &self.saved {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let _guard = RestoreEnv::set(&[
        ("AWS_ACCESS_KEY_ID", "aisessiontest"),
        ("AWS_SECRET_ACCESS_KEY", "aisessiontest-secret"),
        ("AWS_ENDPOINT_URL", "http://example.com"),
        ("AISESSION_BACKUP_S3_ENDPOINT", "http://example.com"),
    ]);
    let text = match S3Backend::from_env("s3://my-bucket/team") {
        Ok(_) => panic!("plain http to example.com was accepted"),
        Err(err) => format!("{err:#}"),
    };
    assert!(text.contains("https"), "{text}");
    drop(_guard);
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() {
        let local = S3Backend::from_env("s3://my-bucket/team");
        assert!(
            local.is_err(),
            "from_env succeeded without an endpoint or credentials"
        );
    }
}
