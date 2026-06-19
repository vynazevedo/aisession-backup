//! End-to-end: build a fake `~/.claude`, back it up, restore it onto a fresh
//! directory with a Linux -> macOS path mapping, and assert that nothing is
//! lost and the embedded `cwd` is rewritten.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use claude_migrate::{backup, claudepath, discover, restore};

const SID: &str = "11111111-1111-1111-1111-111111111111";
const OLD_CWD: &str = "/mnt/e/workspace/alianca_divergente/proj";

fn write(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn build_source(claude_dir: &Path) {
    let encoded = claudepath::encode(OLD_CWD);
    let jsonl = format!(
        concat!(
            "{{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"{id}\"}}\n",
            "{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"gitBranch\":\"main\",\"version\":\"2.1.0\",\"sessionId\":\"{id}\",\"timestamp\":\"2026-01-10T14:30:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"hello world\"}}}}\n",
            "{{\"type\":\"assistant\",\"cwd\":\"{cwd}\",\"sessionId\":\"{id}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
        ),
        id = SID,
        cwd = OLD_CWD
    );
    write(
        &claude_dir
            .join("projects")
            .join(&encoded)
            .join(format!("{SID}.jsonl")),
        jsonl.as_bytes(),
    );

    // companion files use the real `<id>-agent-<id>.json` naming the Go tool missed
    let agent_name = format!("{SID}-agent-{SID}.json");
    write(
        &claude_dir.join("session-env").join(&agent_name),
        format!("{{\"PWD\":\"{OLD_CWD}\",\"OTHER\":\"keepme\"}}").as_bytes(),
    );
    write(
        &claude_dir.join("todos").join(&agent_name),
        b"[{\"content\":\"task one\",\"status\":\"pending\"}]",
    );
}

#[test]
fn full_roundtrip_linux_to_macos() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src/.claude");
    let dst = tmp.path().join("dst/.claude");
    let archive = tmp.path().join("backup.tar.gz");

    build_source(&src);

    // discover
    let discovered = discover::load_sessions(&src).unwrap();
    assert_eq!(discovered.len(), 1, "one session discovered");
    let ds = &discovered[0];
    assert_eq!(
        ds.session.cwd, OLD_CWD,
        "cwd read from JSONL, not the dir name"
    );
    assert_eq!(ds.session.message_count, 2, "control lines not counted");
    assert_eq!(ds.session.preview, "hello world");
    assert_eq!(
        ds.companions.len(),
        2,
        "env + todos picked up by prefix glob"
    );

    // backup
    let manifest = backup::create(&discovered, &archive).unwrap();
    assert_eq!(manifest.schema_version, 2);
    assert!(backup::manifest_self_consistent(&manifest));

    // load + verify checksums
    let loaded = restore::load_backup(&archive).unwrap();
    let report = restore::verify_checksums(&loaded);
    assert!(report.ok(), "checksum mismatches: {:?}", report.mismatches);
    assert_eq!(report.checked, 3, "session + env + todos hashed");

    // restore with Linux -> macOS mapping
    let mut mappings = BTreeMap::new();
    mappings.insert(
        "/mnt/e/workspace".to_string(),
        "/Users/vyn/workspace".to_string(),
    );
    let opts = restore::RestoreOptions {
        claude_dir: dst.clone(),
        mappings,
        dry_run: false,
        rewrite_tool_paths: true,
        restore_history: false,
    };
    let results =
        restore::restore_sessions(&loaded.manifest.sessions, &loaded.data, &opts, |_, _, _| {})
            .unwrap();
    assert_eq!(results.restored, 1);
    assert_eq!(results.failed, 0);

    // assert the new encoded directory and rewritten cwd
    let new_cwd = "/Users/vyn/workspace/alianca_divergente/proj";
    let new_encoded = claudepath::encode(new_cwd);
    let restored_jsonl = dst
        .join("projects")
        .join(&new_encoded)
        .join(format!("{SID}.jsonl"));
    assert!(
        restored_jsonl.exists(),
        "restored under remapped encoded dir"
    );

    let body = fs::read_to_string(&restored_jsonl).unwrap();
    assert!(
        body.contains(&format!("\"cwd\":\"{new_cwd}\"")),
        "cwd rewritten"
    );
    assert!(!body.contains(OLD_CWD), "old cwd fully replaced");
    assert!(body.contains("\"type\":\"mode\""), "control line preserved");

    // companions restored under their real names
    let agent_name = format!("{SID}-agent-{SID}.json");
    let env_path = dst.join("session-env").join(&agent_name);
    let todo_path = dst.join("todos").join(&agent_name);
    assert!(env_path.exists(), "env restored");
    assert!(todo_path.exists(), "todos restored");

    let env_body = fs::read_to_string(&env_path).unwrap();
    assert!(
        env_body.contains(new_cwd),
        "env PWD path rewritten structurally"
    );
    assert!(env_body.contains("keepme"), "non-path env values preserved");
}

#[test]
fn duplicate_restore_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src/.claude");
    let dst = tmp.path().join("dst/.claude");
    let archive = tmp.path().join("backup.tar.gz");
    build_source(&src);

    let discovered = discover::load_sessions(&src).unwrap();
    backup::create(&discovered, &archive).unwrap();
    let loaded = restore::load_backup(&archive).unwrap();

    let opts = restore::RestoreOptions {
        claude_dir: dst.clone(),
        mappings: BTreeMap::new(),
        dry_run: false,
        rewrite_tool_paths: false,
        restore_history: false,
    };
    let first =
        restore::restore_sessions(&loaded.manifest.sessions, &loaded.data, &opts, |_, _, _| {})
            .unwrap();
    assert_eq!(first.restored, 1);

    let second =
        restore::restore_sessions(&loaded.manifest.sessions, &loaded.data, &opts, |_, _, _| {})
            .unwrap();
    assert_eq!(second.restored, 0);
    assert_eq!(
        second.skipped, 1,
        "existing session is skipped, not overwritten"
    );
}
