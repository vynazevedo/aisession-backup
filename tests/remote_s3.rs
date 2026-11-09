use std::fs;
use std::path::Path;
use std::process::Command;

use aisession_backup::remote::{parse_location, S3Backend};
use aisession_backup::repo::Backend;

fn endpoint() -> Option<String> {
    std::env::var("AISESSION_BACKUP_TEST_S3_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
}

fn backend(endpoint: &str, location: &str) -> S3Backend {
    S3Backend::new(
        endpoint.parse().unwrap(),
        rusty_s3::UrlStyle::Path,
        parse_location(location).unwrap(),
        "us-east-1".into(),
        rusty_s3::Credentials::new(
            std::env::var("AWS_ACCESS_KEY_ID").unwrap(),
            std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
        ),
    )
    .unwrap()
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[test]
fn s3_backend_honors_the_repository_contract() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipped: AISESSION_BACKUP_TEST_S3_ENDPOINT is not set");
        return;
    };
    let bucket = unique("contract");
    let root = backend(&endpoint, &format!("s3://{bucket}"));
    root.create_bucket().unwrap();
    let store = backend(&endpoint, &format!("s3://{bucket}/team/repo"));

    assert_eq!(store.get("config.json").unwrap(), None);
    assert!(!store.exists("config.json").unwrap());
    assert!(store.put_if_absent("locks/writer", b"one").unwrap());
    assert!(!store.put_if_absent("locks/writer", b"two").unwrap());
    assert_eq!(store.get("locks/writer").unwrap().unwrap(), b"one");
    store.put("data/ab/x", b"payload").unwrap();
    assert!(store.exists("data/ab/x").unwrap());
    store.delete("data/ab/x").unwrap();
    store.delete("data/ab/x").unwrap();
    assert!(!store.exists("data/ab/x").unwrap());
    root.put("outside/secret", b"not part of the repository")
        .unwrap();

    for index in 0..1100 {
        store.put(&format!("data/{:04}", index), b"x").unwrap();
    }
    let listed = store.list("data/").unwrap();
    assert_eq!(listed.len(), 1100);
    assert!(listed.iter().all(|key| key.starts_with("data/")));
    assert!(!store
        .list("")
        .unwrap()
        .iter()
        .any(|key| key.contains("outside")));
    assert!(store.get("../outside/secret").is_err());
}

#[test]
fn encrypted_repository_round_trip_through_s3() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipped: AISESSION_BACKUP_TEST_S3_ENDPOINT is not set");
        return;
    };
    let bucket = unique("roundtrip");
    backend(&endpoint, &format!("s3://{bucket}"))
        .create_bucket()
        .unwrap();
    let location = format!("s3://{bucket}/ai");

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let claude = home.join(".claude");
    let project = claude.join("projects/-work-s3");
    fs::create_dir_all(&project).unwrap();
    let id = "88888888-8888-4888-8888-888888888888";
    let line = serde_json::json!({"type":"user","cwd":"/work/s3","sessionId":id,"timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"stored in object storage"}});
    fs::write(project.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();

    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_aisession-backup"))
            .args(args)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", home.join(".codex"))
            .env("MAESTRI_HOME", home.join(".maestri"))
            .env("AISESSION_BACKUP_DATA_DIR", tmp.path().join("data"))
            .env("AISESSION_BACKUP_S3_ENDPOINT", &endpoint)
            .env("AISESSION_BACKUP_PASSPHRASE", "remote secret")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    run(&["repo", "init", &location, "--encrypt"]);
    run(&["backup", "--repo", &location, "--yes"]);
    run(&["repo", "check", &location, "--read-data"]);
    let dest = tmp.path().join("restored/.claude");
    run(&[
        "restore",
        "--repo",
        &location,
        "--dest",
        dest.to_str().unwrap(),
        "--provider",
        "claude",
        "--allow-running",
        "--yes",
    ]);
    let restored = fs::read_to_string(
        Path::new(&dest)
            .join("projects/-work-s3")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    assert!(restored.contains("stored in object storage"));

    let raw = backend(&endpoint, &location);
    for key in raw.list("").unwrap() {
        let bytes = raw.get(&key).unwrap().unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("stored in object storage"),
            "{key} holds plaintext"
        );
    }
}
