use super::*;
use std::io::Cursor;

fn manifest() -> crate::model::Manifest {
    serde_json::from_value(serde_json::json!({"schema_version":3,"created_at":"2026-01-01T00:00:00Z","source_host":"fixture","sessions":[]})).unwrap()
}

fn setup(password: Option<&str>) -> (tempfile::TempDir, LocalBackend, Repository) {
    let temp = tempfile::tempdir().unwrap();
    let backend = LocalBackend::new(temp.path().join("repository")).unwrap();
    let repo = Repository::init(Box::new(backend.clone()), password).unwrap();
    (temp, backend, repo)
}

fn source(temp: &tempfile::TempDir, name: &str, bytes: &[u8]) -> (String, PathBuf) {
    let path = temp.path().join(name);
    fs::write(&path, bytes).unwrap();
    (format!("sessions/{name}"), path)
}

fn roundtrip(password: Option<&str>) {
    let (temp, backend, repo) = setup(password);
    let bytes = b"synthetic conversation\n\0\xff";
    let files = vec![
        source(&temp, "session.jsonl", bytes),
        source(&temp, "empty", b""),
    ];
    let summary = repo
        .write_snapshot(&manifest(), &files, "test-host")
        .unwrap();
    assert_eq!(summary.files, 2);
    assert_eq!(summary.new_chunks, 1);
    assert_eq!(summary.total_bytes, bytes.len() as u64);
    let repo = Repository::open(Box::new(backend.clone()), password).unwrap();
    let listed = repo.list_snapshots().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, summary.snapshot_id);
    assert_eq!(listed[0].host, "test-host");
    let (restored, paths) = repo
        .stage_snapshot("latest", &temp.path().join("staging"))
        .unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(manifest()).unwrap()
    );
    assert_eq!(fs::read(&paths["sessions/session.jsonl"]).unwrap(), bytes);
    assert!(fs::read(&paths["sessions/empty"]).unwrap().is_empty());
    let check = repo.check(true).unwrap();
    assert_eq!(check.snapshots, 1);
    assert_eq!(check.chunks, 1);
    assert!(check.missing.is_empty());
    assert!(check.corrupt.is_empty());
    if password.is_some() {
        let id = hex::encode(Sha256::digest(bytes));
        assert!(!backend.exists(&chunk_key(&id).unwrap()).unwrap());
        let stored = backend
            .get(&snapshot_key(&summary.snapshot_id).unwrap())
            .unwrap()
            .unwrap();
        assert!(stored.starts_with(b"age-encryption.org/"));
        assert!(!String::from_utf8_lossy(&stored).contains("test-host"));
        let again = repo
            .write_snapshot(&manifest(), &files, "test-host")
            .unwrap();
        assert_eq!(again.new_chunks, 0);
        assert_eq!(again.reused_chunks, 1);
        let key = backend.list("data/").unwrap().remove(0);
        let mut ciphertext = backend.get(&key).unwrap().unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 1;
        backend.put(&key, &ciphertext).unwrap();
        assert_eq!(repo.check(true).unwrap().corrupt.len(), 1);
        assert!(repo
            .stage_snapshot("latest", &temp.path().join("tampered-stage"))
            .is_err());
        assert!(Repository::open(Box::new(backend.clone()), None).is_err());
        assert!(format!(
            "{:#}",
            Repository::open(Box::new(backend), Some("wrong"))
                .err()
                .unwrap()
        )
        .contains("wrong passphrase"));
    }
}

#[test]
fn plain_roundtrip() {
    roundtrip(None);
}

#[test]
fn encrypted_roundtrip_and_wrong_passphrase() {
    roundtrip(Some("synthetic password"));
}

#[test]
fn append_reuses_chunks_and_duplicate_files_store_once() {
    let (temp, backend, repo) = setup(None);
    let mut state = 0x91823abcdefu64;
    let bytes: Vec<u8> = (0..5_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let file = source(&temp, "large", &bytes);
    let first = repo
        .write_snapshot(&manifest(), std::slice::from_ref(&file), "host")
        .unwrap();
    let before = backend.list("data/").unwrap().len();
    let duplicate = source(&temp, "duplicate", &bytes);
    let repeated = repo
        .write_snapshot(&manifest(), &[file.clone(), duplicate], "host")
        .unwrap();
    assert_eq!(repeated.new_chunks, 0);
    assert_eq!(repeated.reused_chunks, first.new_chunks);
    assert_eq!(backend.list("data/").unwrap().len(), before);
    fs::OpenOptions::new()
        .append(true)
        .open(&file.1)
        .unwrap()
        .write_all(&[42; 100])
        .unwrap();
    let second = repo.write_snapshot(&manifest(), &[file], "host").unwrap();
    assert!((1..=2).contains(&second.new_chunks));
    assert!(second.reused_chunks > 0);
    assert_eq!(second.total_bytes, 5_000_100);
    assert_eq!(repo.list_snapshots().unwrap()[0].id, second.snapshot_id);
    let (_, paths) = repo
        .stage_snapshot("latest", &temp.path().join("stage"))
        .unwrap();
    let restored = fs::read(&paths["sessions/large"]).unwrap();
    assert_eq!(&restored[..bytes.len()], bytes);
    assert_eq!(&restored[bytes.len()..], &[42; 100]);
}

#[test]
fn chunk_boundaries_respect_limits_and_buffer_sizes() {
    let bytes = vec![5; 10_000_000];
    let mut reader = BufReader::with_capacity(17, Cursor::new(&bytes));
    let mut chunks = Vec::new();
    while let Some(chunk) = chunk::next_chunk(&mut reader).unwrap() {
        chunks.push(chunk);
    }
    for chunk in &chunks[..chunks.len() - 1] {
        assert!((MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk.len()));
    }
    assert!(chunks.last().unwrap().len() <= MAX_CHUNK_SIZE);
    assert_eq!(chunks.concat(), bytes);
    let mut reader = Cursor::new(bytes);
    for expected in chunks {
        assert_eq!(chunk::next_chunk(&mut reader).unwrap().unwrap(), expected);
    }
    assert!(chunk::next_chunk(&mut reader).unwrap().is_none());
}

#[test]
fn tampered_and_missing_chunks_are_reported_and_not_staged() {
    let (temp, backend, repo) = setup(None);
    let files = vec![source(&temp, "one", b"one"), source(&temp, "two", b"two")];
    repo.write_snapshot(&manifest(), &files, "host").unwrap();
    let keys = backend.list("data/").unwrap();
    backend.put(&keys[0], b"corrupt compressed object").unwrap();
    backend.delete(&keys[1]).unwrap();
    let report = repo.check(true).unwrap();
    assert_eq!(report.missing.len(), 1);
    assert_eq!(report.corrupt.len(), 1);
    let metadata_report = repo.check(false).unwrap();
    assert_eq!(metadata_report.missing.len(), 1);
    assert!(metadata_report.corrupt.is_empty());
    let staging = temp.path().join("staging");
    assert!(repo.stage_snapshot("latest", &staging).is_err());
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
}

#[test]
fn hostile_snapshot_paths_and_ids_are_rejected_before_staging() {
    let (temp, backend, repo) = setup(None);
    let summary = repo
        .write_snapshot(&manifest(), &[source(&temp, "one", b"one")], "host")
        .unwrap();
    let key = snapshot_key(&summary.snapshot_id).unwrap();
    let original = backend.get(&key).unwrap().unwrap();
    for path in [
        "../escape",
        "/absolute",
        "nested/../../escape",
        "back\\slash",
        "bad\nname",
        "C:/escape",
        "a//b",
        "a/./b",
    ] {
        let mut doc: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let entry = doc["files"]["sessions/one"].clone();
        doc["files"] = serde_json::json!({path:entry});
        backend
            .put(&key, &serde_json::to_vec(&doc).unwrap())
            .unwrap();
        assert!(
            repo.stage_snapshot("latest", &temp.path().join("stage"))
                .is_err(),
            "{path:?}"
        );
        assert!(!temp.path().join("stage").exists());
        assert_eq!(repo.check(false).unwrap().corrupt.len(), 1);
    }
    assert!(!temp.path().join("escape").exists());
    assert!(repo
        .stage_snapshot("../config", &temp.path().join("stage"))
        .is_err());
}

#[test]
fn whole_file_hash_and_chunk_order_are_verified() {
    let (temp, backend, repo) = setup(None);
    let summary = repo
        .write_snapshot(&manifest(), &[source(&temp, "one", b"one")], "host")
        .unwrap();
    let key = snapshot_key(&summary.snapshot_id).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&backend.get(&key).unwrap().unwrap()).unwrap();
    doc["files"]["sessions/one"]["sha256"] = serde_json::json!("0".repeat(64));
    backend
        .put(&key, &serde_json::to_vec(&doc).unwrap())
        .unwrap();
    assert!(repo
        .stage_snapshot("latest", &temp.path().join("stage"))
        .is_err());
    assert_eq!(repo.check(true).unwrap().corrupt.len(), 1);
    assert_eq!(fs::read_dir(temp.path().join("stage")).unwrap().count(), 0);
}

#[test]
fn prune_preserves_references_and_dry_run_is_read_only() {
    let (temp, backend, repo) = setup(None);
    let shared = source(&temp, "shared", b"shared");
    let old = source(&temp, "old", b"old");
    let latest = source(&temp, "new", b"new");
    let first = repo
        .write_snapshot(&manifest(), &[shared.clone(), old], "host")
        .unwrap();
    let second = repo
        .write_snapshot(&manifest(), &[shared, latest], "host")
        .unwrap();
    let before = backend.list("").unwrap();
    let dry = repo.prune(1, true).unwrap();
    assert_eq!(dry.snapshots_removed, [first.snapshot_id]);
    assert_eq!(dry.chunks_removed.len(), 1);
    assert_eq!(backend.list("").unwrap(), before);
    let real = repo.prune(1, false).unwrap();
    assert_eq!(real.chunks_removed, dry.chunks_removed);
    assert_eq!(real.bytes_removed, dry.bytes_removed);
    assert_eq!(repo.list_snapshots().unwrap()[0].id, second.snapshot_id);
    assert_eq!(backend.list("data/").unwrap().len(), 2);
    assert!(repo.check(true).unwrap().corrupt.is_empty());
    assert_eq!(repo.prune(0, false).unwrap().chunks_removed.len(), 2);
    assert!(repo.list_snapshots().unwrap().is_empty());
    assert!(repo
        .stage_snapshot("latest", &temp.path().join("stage"))
        .is_err());
}

#[test]
fn locks_exclude_writers_and_prune_and_cleanup_after_error() {
    let (temp, backend, repo) = setup(None);
    backend.put(LOCK, b"other writer").unwrap();
    assert!(repo.write_snapshot(&manifest(), &[], "host").is_err());
    assert!(repo.prune(0, true).is_err());
    assert!(repo.prune(0, false).is_err());
    assert_eq!(backend.get(LOCK).unwrap().unwrap(), b"other writer");
    backend.delete(LOCK).unwrap();
    assert!(repo
        .write_snapshot(
            &manifest(),
            &[("missing".into(), temp.path().join("missing"))],
            "host"
        )
        .is_err());
    assert!(!backend.exists(LOCK).unwrap());
    assert!(repo.list_snapshots().unwrap().is_empty());
    let held = WriterLock::acquire(&backend).unwrap();
    let other = backend.clone();
    assert!(
        std::thread::spawn(move || WriterLock::acquire(&other).is_err())
            .join()
            .unwrap()
    );
    held.release().unwrap();
    repo.write_snapshot(&manifest(), &[], "host").unwrap();
}

#[test]
fn backend_validates_all_operations_and_atomic_creation() {
    let (_temp, backend, _repo) = setup(None);
    for key in [
        "",
        "/absolute",
        "../up",
        "a/../b",
        "a\\b",
        "a\nb",
        "a\0b",
        "a//b",
        "./a",
        "C:/a",
    ] {
        assert!(backend.get(key).is_err());
        assert!(backend.put(key, b"bad").is_err());
        assert!(backend.exists(key).is_err());
        assert!(backend.delete(key).is_err());
        assert!(backend.put_if_absent(key, b"bad").is_err());
        if !key.is_empty() {
            assert!(backend.list(key).is_err());
        }
    }
    backend.put("data/ab/value", b"first").unwrap();
    backend.put("data/ab/value", b"second").unwrap();
    assert!(!backend.put_if_absent("data/ab/value", b"third").unwrap());
    assert_eq!(backend.get("data/ab/value").unwrap().unwrap(), b"second");
    assert_eq!(backend.list("data/ab/").unwrap(), ["data/ab/value"]);
    assert!(backend.put_if_absent("new", b"new").unwrap());
    backend.delete("new").unwrap();
    backend.delete("new").unwrap();
    assert!(backend.get("new").unwrap().is_none());
    assert!(Repository::init(Box::new(backend), None).is_err());
}

#[test]
fn staging_never_clobbers_and_rejects_conflicting_paths() {
    let (temp, _backend, repo) = setup(None);
    let file = source(&temp, "one", b"one");
    assert!(repo
        .write_snapshot(&manifest(), &[file.clone(), file.clone()], "host")
        .is_err());
    assert!(repo
        .write_snapshot(
            &manifest(),
            &[("a".into(), file.1.clone()), ("a/b".into(), file.1.clone())],
            "host"
        )
        .is_err());
    repo.write_snapshot(&manifest(), &[file], "host").unwrap();
    let staging = temp.path().join("stage");
    fs::create_dir_all(staging.join("sessions")).unwrap();
    fs::write(staging.join("sessions/one"), b"keep").unwrap();
    assert!(repo.stage_snapshot("latest", &staging).is_err());
    assert_eq!(fs::read(staging.join("sessions/one")).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn private_permissions_and_symlink_safety() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (temp, backend, repo) = setup(None);
    fs::set_permissions(backend.root(), fs::Permissions::from_mode(0o755)).unwrap();
    LocalBackend::new(backend.root()).unwrap();
    let file = source(&temp, "one", b"one");
    repo.write_snapshot(&manifest(), &[file], "host").unwrap();
    for entry in walkdir::WalkDir::new(backend.root()) {
        let entry = entry.unwrap();
        let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            if entry.file_type().is_dir() {
                0o700
            } else {
                0o600
            }
        );
    }
    let stage = temp.path().join("stage");
    let (_, paths) = repo.stage_snapshot("latest", &stage).unwrap();
    assert_eq!(
        fs::metadata(&paths["sessions/one"])
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(stage.join("sessions"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let evil = temp.path().join("evil-stage");
    fs::create_dir(&evil).unwrap();
    symlink(&outside, evil.join("sessions")).unwrap();
    assert!(repo.stage_snapshot("latest", &evil).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    symlink(&outside, backend.root().join("escape")).unwrap();
    assert!(backend.put("escape/file", b"bad").is_err());
    assert!(backend.get("escape/file").is_err());
    assert!(backend.delete("escape/file").is_err());
    assert!(backend.list("").is_err());
    fs::remove_file(backend.root().join("escape")).unwrap();
    let link = temp.path().join("root-link");
    symlink(backend.root(), &link).unwrap();
    let linked = LocalBackend::new(link).unwrap();
    assert_eq!(linked.list("").unwrap(), backend.list("").unwrap());
}

#[test]
fn corrupted_snapshot_stops_prune_and_bad_config_fails_open() {
    let (temp, backend, repo) = setup(None);
    let summary = repo
        .write_snapshot(&manifest(), &[source(&temp, "one", b"one")], "host")
        .unwrap();
    backend
        .put(&snapshot_key(&summary.snapshot_id).unwrap(), b"not json")
        .unwrap();
    let before = backend.list("").unwrap();
    assert!(repo.prune(0, false).is_err());
    assert_eq!(backend.list("").unwrap(), before);
    assert_eq!(repo.check(false).unwrap().corrupt.len(), 1);
    assert!(!backend.exists(LOCK).unwrap());
    let mut config: serde_json::Value =
        serde_json::from_slice(&backend.get("config.json").unwrap().unwrap()).unwrap();
    config["format_version"] = serde_json::json!(999);
    backend
        .put("config.json", &serde_json::to_vec(&config).unwrap())
        .unwrap();
    assert!(Repository::open(Box::new(backend), None).is_err());
}

#[test]
fn substituted_chunks_are_detected_even_when_deflate_is_valid() {
    let (temp, backend, repo) = setup(None);
    repo.write_snapshot(
        &manifest(),
        &[source(&temp, "one", b"one"), source(&temp, "two", b"two")],
        "host",
    )
    .unwrap();
    let keys = backend.list("data/").unwrap();
    let substitute = backend.get(&keys[1]).unwrap().unwrap();
    backend.put(&keys[0], &substitute).unwrap();
    assert_eq!(repo.check(true).unwrap().corrupt.len(), 1);
    assert!(repo
        .stage_snapshot("latest", &temp.path().join("stage"))
        .is_err());
    assert!(repo
        .write_snapshot(
            &manifest(),
            &[
                source(&temp, "one-again", b"one"),
                source(&temp, "two-again", b"two")
            ],
            "host"
        )
        .is_err());
    assert!(!backend.exists(LOCK).unwrap());
}

#[test]
fn concurrent_atomic_create_has_exactly_one_winner() {
    let (_temp, backend, _repo) = setup(None);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let backend = backend.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                backend.put_if_absent("race", &[i; 1024]).unwrap()
            })
        })
        .collect();
    let winners = threads
        .into_iter()
        .filter_map(|thread| thread.join().unwrap().then_some(()))
        .count();
    assert_eq!(winners, 1);
    let bytes = backend.get("race").unwrap().unwrap();
    assert_eq!(bytes.len(), 1024);
    assert!(bytes.iter().all(|byte| *byte == bytes[0]));
}

#[test]
fn hmac_matches_rfc_4231() {
    assert_eq!(
        hex::encode(crypto::hmac_sha256(
            b"Jefe",
            b"what do ya want for nothing?"
        )),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    let long_key = [0xaau8; 131];
    assert_eq!(
        hex::encode(crypto::hmac_sha256(
            &long_key,
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn forged_snapshots_are_rejected_in_encrypted_repositories() {
    let (temp, backend, repo) = setup(Some("synthetic password"));
    let file = source(&temp, "a", b"genuine content");
    let summary = repo
        .write_snapshot(&manifest(), std::slice::from_ref(&file), "host")
        .unwrap();
    let id = summary.snapshot_id.clone();
    let key = snapshot_key(&id).unwrap();
    let enc = repo.encryption.as_ref().unwrap();
    let stored = backend.get(&key).unwrap().unwrap();
    let sealed: SealedDocument =
        serde_json::from_slice(&enc.decrypt(&stored, DOCUMENT_LIMIT).unwrap()).unwrap();

    let tampered_body = sealed
        .body
        .replace("\"host\":\"host\"", "\"host\":\"attacker\"");
    assert_ne!(tampered_body, sealed.body);
    let tampered = serde_json::to_vec(&SealedDocument {
        body: tampered_body.clone(),
        mac: sealed.mac.clone(),
    })
    .unwrap();
    backend.put(&key, &enc.encrypt(&tampered).unwrap()).unwrap();
    let err = repo
        .stage_snapshot(&id, &temp.path().join("stage1"))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("authentication failed"),
        "{err:#}"
    );

    let recipient: age::x25519::Recipient = enc.recipient().parse().unwrap();
    let forged = serde_json::to_vec(&SealedDocument {
        body: tampered_body,
        mac: "0".repeat(64),
    })
    .unwrap();
    let ciphertext = {
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .unwrap();
        let mut out = Vec::new();
        let mut writer = encryptor.wrap_output(&mut out).unwrap();
        writer.write_all(&forged).unwrap();
        writer.finish().unwrap();
        out
    };
    backend.put(&key, &ciphertext).unwrap();
    assert!(repo
        .stage_snapshot(&id, &temp.path().join("stage2"))
        .is_err());

    let unwrapped = enc.encrypt(sealed.body.as_bytes()).unwrap();
    backend.put(&key, &unwrapped).unwrap();
    let err = repo.list_snapshots().unwrap_err();
    assert!(format!("{err:#}").contains("not authenticated"), "{err:#}");

    let restored = serde_json::to_vec(&sealed).unwrap();
    backend.put(&key, &enc.encrypt(&restored).unwrap()).unwrap();
    assert!(repo
        .stage_snapshot(&id, &temp.path().join("stage3"))
        .is_ok());
}
