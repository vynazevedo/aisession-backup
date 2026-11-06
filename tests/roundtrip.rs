//! End-to-end: build a fake `~/.claude`, back it up, restore it onto a fresh
//! directory with a Linux -> macOS path mapping, and assert that nothing is
//! lost and the embedded `cwd` is rewritten.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use aisession_backup::{backup, claudepath, discover, engine};

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

fn isolated(name: &str, test: impl FnOnce(&Path)) {
    if std::env::var("CM_ROUNDTRIP_CASE").ok().as_deref() == Some(name) {
        let root = std::path::PathBuf::from(std::env::var_os("CM_ROUNDTRIP_ROOT").unwrap());
        test(&root);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name])
        .env("CM_ROUNDTRIP_CASE", name)
        .env("CM_ROUNDTRIP_ROOT", temp.path())
        .env("AISESSION_BACKUP_DATA_DIR", temp.path().join("data"))
        .status()
        .unwrap();
    assert!(status.success());
}

fn backup_request(root: &Path, extras: bool) -> engine::BackupRequest {
    engine::BackupRequest {
        output: root.join("backup.tar.gz"),
        overwrite: false,
        providers: Vec::new(),
        passphrase: engine::Passphrase::None,
        claude: Some(engine::ClaudeSelection {
            claude_dir: root.join("src/.claude"),
            session_ids: None,
            match_path: None,
            include_extras: extras,
            include_config: false,
            include_credentials: false,
        }),
    }
}
fn restore_request(root: &Path) -> engine::RestoreRequest {
    engine::RestoreRequest {
        archive: root.join("backup.tar.gz"),
        passphrase: engine::Passphrase::None,
        claude_dir: root.join("dst/.claude"),
        provider_homes: BTreeMap::new(),
        only_providers: None,
        only_sessions: None,
        only_items: None,
        mappings: BTreeMap::new(),
        rewrite_tool_paths: true,
        restore_history: false,
        apply_config: false,
        overwrite: false,
        allow_exec_config: true,
        allow_credentials: false,
        allow_running: true,
        skip_verify: false,
    }
}

#[test]
fn full_roundtrip_linux_to_macos() {
    isolated("full_roundtrip_linux_to_macos", |root| {
        let src = root.join("src/.claude");
        let dst = root.join("dst/.claude");
        build_source(&src);
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
        let backup = backup_request(root, false);
        engine::run_backup(&backup).unwrap();
        let manifest = engine::read_manifest(&backup.output, &engine::Passphrase::None).unwrap();
        assert_eq!(manifest.schema_version, 3);
        assert!(backup::manifest_self_consistent(&manifest));
        let report = engine::verify_archive(&backup.output, &engine::Passphrase::None).unwrap();
        assert!(report.ok(), "checksum mismatches: {:?}", report.mismatches);
        assert_eq!(report.checked, 3, "session + env + todos hashed");
        let mut request = restore_request(root);
        request
            .mappings
            .insert("/mnt/e/workspace".into(), "/Users/vyn/workspace".into());
        let results = engine::apply_restore(engine::prepare_restore(request).unwrap()).unwrap();
        assert_eq!(results.written, 3);
        assert!(results.failed.is_empty());
        let new_cwd = "/Users/vyn/workspace/alianca_divergente/proj";
        let restored_jsonl = dst
            .join("projects")
            .join(claudepath::encode(new_cwd))
            .join(format!("{SID}.jsonl"));
        assert!(
            restored_jsonl.exists(),
            "restored under remapped encoded dir"
        );
        let body = fs::read_to_string(restored_jsonl).unwrap();
        assert!(
            body.contains(&format!("\"cwd\":\"{new_cwd}\"")),
            "cwd rewritten"
        );
        assert!(!body.contains(OLD_CWD), "old cwd fully replaced");
        assert!(body.contains("\"type\":\"mode\""), "control line preserved");
        let agent_name = format!("{SID}-agent-{SID}.json");
        let env_path = dst.join("session-env").join(&agent_name);
        let todo_path = dst.join("todos").join(&agent_name);
        assert!(env_path.exists(), "env restored");
        assert!(todo_path.exists(), "todos restored");
        let env_body = fs::read_to_string(env_path).unwrap();
        assert!(
            env_body.contains(new_cwd),
            "env PWD path rewritten structurally"
        );
        assert!(env_body.contains("keepme"), "non-path env values preserved");
    });
}

#[test]
fn full_backup_includes_and_restores_extras() {
    isolated("full_backup_includes_and_restores_extras", |root| {
        let src = root.join("src/.claude");
        let dst = root.join("dst/.claude");
        build_source(&src);
        write(
            &src.join("shell-snapshots/snapshot-zsh-1.sh"),
            b"export FOO=bar\n",
        );
        write(&src.join("plans/nice-plan.md"), b"# plan\n");
        write(&src.join("file-history/abc/0.json"), b"{\"v\":1}");
        let backup = backup_request(root, true);
        engine::run_backup(&backup).unwrap();
        let manifest = engine::read_manifest(&backup.output, &engine::Passphrase::None).unwrap();
        assert_eq!(manifest.extras.len(), 3, "three extra files archived");
        let results =
            engine::apply_restore(engine::prepare_restore(restore_request(root)).unwrap()).unwrap();
        assert!(results.failed.is_empty());
        assert_eq!(
            results.written - 3,
            3,
            "three extra files restored alongside session and companions"
        );
        assert!(dst.join("shell-snapshots/snapshot-zsh-1.sh").exists());
        assert!(dst.join("plans/nice-plan.md").exists());
        assert!(dst.join("file-history/abc/0.json").exists());
        let again =
            engine::apply_restore(engine::prepare_restore(restore_request(root)).unwrap()).unwrap();
        assert_eq!(again.written, 0, "existing extras are not overwritten");
        write(&dst.join("plans/nice-plan.md"), b"user edit");
        let mut request = restore_request(root);
        request.overwrite = true;
        engine::apply_restore(engine::prepare_restore(request).unwrap()).unwrap();
        assert_eq!(
            fs::read(dst.join("plans/nice-plan.md")).unwrap(),
            b"user edit"
        );
    });
}

#[test]
fn duplicate_restore_is_skipped() {
    isolated("duplicate_restore_is_skipped", |root| {
        build_source(&root.join("src/.claude"));
        engine::run_backup(&backup_request(root, false)).unwrap();
        let first =
            engine::apply_restore(engine::prepare_restore(restore_request(root)).unwrap()).unwrap();
        assert_eq!(first.written, 3);
        let prepared = engine::prepare_restore(restore_request(root)).unwrap();
        assert!(prepared
            .plan
            .ops
            .iter()
            .any(|op| op.dest.ends_with(format!("{SID}.jsonl"))
                && matches!(op.action, aisession_backup::plan::Action::Skip(_))));
        let second = engine::apply_restore(prepared).unwrap();
        assert_eq!(second.written, 0);
        assert_eq!(
            second.skipped, 3,
            "existing session and companions skipped, not overwritten"
        );
    });
}

#[test]
fn restores_independent_v2_archive_fixture() {
    isolated("restores_independent_v2_archive_fixture", |root| {
        use sha2::{Digest, Sha256};
        let bytes = br#"{"cwd":"/legacy/project","type":"user"}"#;
        let sha = hex::encode(Sha256::digest(bytes));
        let checksum = hex::encode(Sha256::digest(format!("legacy/legacy.jsonl:{sha}\n")));
        let manifest: aisession_backup::model::Manifest = serde_json::from_value(serde_json::json!({
            "schema_version":2,"created_at":"2026-01-01T00:00:00Z", "checksum":checksum,
            "sessions":[{"id":"legacy","cwd":"/legacy/project","timestamp":"2026-01-01T00:00:00Z","artifacts":[{"kind":"session","logical_name":"legacy.jsonl","sha256":sha,"size":bytes.len()}]}]
        })).unwrap();
        let source = root.join("legacy.jsonl");
        fs::write(&source, bytes).unwrap();
        let archive = root.join("backup.tar.gz");
        aisession_backup::archive::write(
            &archive,
            false,
            &manifest,
            &[("sessions/legacy/legacy.jsonl".into(), source)],
            &engine::Passphrase::None,
        )
        .unwrap();
        assert_eq!(
            engine::read_manifest(&archive, &engine::Passphrase::None)
                .unwrap()
                .schema_version,
            2
        );
        assert!(engine::verify_archive(&archive, &engine::Passphrase::None)
            .unwrap()
            .ok());
        let results =
            engine::apply_restore(engine::prepare_restore(restore_request(root)).unwrap()).unwrap();
        assert_eq!(results.written, 1);
        assert!(results.failed.is_empty());
        assert_eq!(
            fs::read(root.join("dst/.claude/projects/-legacy-project/legacy.jsonl")).unwrap(),
            bytes
        );
    });
}
