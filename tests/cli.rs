use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use aisession_backup::claudepath;
use aisession_backup::cli::{
    parse_mappings, EXIT_CONFIRM_REQUIRED, EXIT_OK, EXIT_PLAN_REFUSED, EXIT_USAGE,
    EXIT_VERIFY_FAILED,
};
use aisession_backup::discover::DiscoveredSession;
use aisession_backup::model::{Manifest, Session, SCHEMA_VERSION};
use aisession_backup::plan::{Action, Plan, PlannedOp, Risk};
use aisession_backup::provider::{FileClass, Item, ItemFile, ItemKind, ProviderId};
use aisession_backup::tui::{
    check_tty, install_panic_hook, BackupApp, BackupStep, HomeOutcome, HomeScreen, HomeState,
    ProviderEntry, RestoreApp, RestoreFlags, RestoreStep,
};
use aisession_backup::ui;
use ratatui::crossterm::event::KeyCode;

struct TestEnv {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    claude_dir: PathBuf,
    codex_dir: PathBuf,
    maestri_dir: PathBuf,
    data_dir: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        let claude_dir = home.join(".claude");
        let codex_dir = home.join(".codex");
        let maestri_dir = home.join(".maestri");
        let data_dir = root.join("data");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::create_dir_all(&codex_dir).unwrap();
        fs::create_dir_all(&maestri_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(maestri_dir.join("manifest.json"), b"{}").unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
            claude_dir,
            codex_dir,
            maestri_dir,
            data_dir,
        }
    }

    fn write_claude_session(&self, id: &str, cwd: &str, preview: &str) {
        let encoded = claudepath::encode(cwd);
        let dir = self.claude_dir.join("projects").join(encoded);
        fs::create_dir_all(&dir).unwrap();
        let line = serde_json::json!({
            "type": "user",
            "cwd": cwd,
            "sessionId": id,
            "timestamp": "2026-02-01T10:00:00Z",
            "message": {
                "role": "user",
                "content": preview
            }
        });
        fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_aisession-backup"));
        c.current_dir(&self.root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CLAUDE_CONFIG_DIR", &self.claude_dir)
            .env("CODEX_HOME", &self.codex_dir)
            .env("MAESTRI_HOME", &self.maestri_dir)
            .env("AISESSION_BACKUP_DATA_DIR", &self.data_dir)
            .env("NO_COLOR", "1")
            .env_remove("AISESSION_BACKUP_PASSPHRASE")
            .env_remove("CLAUDE_MIGRATE_PASSPHRASE")
            .env_remove("CLAUDE_MIGRATE_DATA_DIR")
            .env_remove("MAESTRI_SOCKET");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }
}

fn code_of(out: &Output) -> u8 {
    out.status.code().unwrap_or(255) as u8
}

#[test]
fn parse_mappings_supports_windows_sources_and_targets() {
    let m = parse_mappings("C:\\Users\\alice\\proj:/home/alice/proj,D:/src:E:\\dst").unwrap();
    assert_eq!(
        m.get("C:\\Users\\alice\\proj").map(String::as_str),
        Some("/home/alice/proj")
    );
    assert_eq!(m.get("D:/src").map(String::as_str), Some("E:\\dst"));
    assert!(parse_mappings("relative:/abs").is_err());
    assert!(parse_mappings("/abs:relative").is_err());
    assert!(parse_mappings("C:\\missing_target").is_err());
}

#[test]
fn sanitize_strips_escape_sequences_and_control_chars() {
    let raw = "hello \x1b[31mred\x1b[0m \x1b]0;evil-title\x07world\x00\x08\nnext\tline";
    let clean = ui::sanitize(raw);
    assert_eq!(clean, "hello red world next line");
    assert!(!clean.contains('\x1b'));
    assert!(!clean.chars().any(char::is_control));
}

#[test]
fn plan_rendering_includes_summary_and_risk_counts() {
    let plan = Plan {
        ops: vec![
            PlannedOp {
                provider: "claude".into(),
                dest: PathBuf::from("/tmp/settings.json"),
                action: Action::Refuse("executable configuration".into()),
                risk: Risk::ExecutesCode,
                size: 128,
                rewrites: 0,
                note: "hooks\x1b[31m".into(),
            },
            PlannedOp {
                provider: "claude".into(),
                dest: PathBuf::from("/tmp/session.jsonl"),
                action: Action::Create,
                risk: Risk::Sensitive,
                size: 1024,
                rewrites: 2,
                note: "session".into(),
            },
        ],
        warnings: vec!["watch out\x1b[2J".into()],
    };
    let text = ui::format_plan(&plan, false);
    assert!(!text.contains('\x1b'));
    assert!(text.contains("REFUSE (executable configuration)"));
    assert!(text.contains("EXECUTES_CODE"));
    assert!(text.contains("SENSITIVE"));
    assert!(text.contains("create=1"));
    assert!(text.contains("refuse=1"));
    assert!(text.contains("sensitive=1"));
}

#[test]
fn no_args_non_tty_prints_help_and_usage_error_exits_2() {
    let env = TestEnv::new();
    let out = env.run(&[]);
    assert_eq!(code_of(&out), EXIT_OK);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:") || stdout.contains("aisession-backup"));

    let bad = env.run(&["--not-a-valid-flag"]);
    assert_eq!(code_of(&bad), EXIT_USAGE);
}

#[test]
fn version_and_completions_commands_succeed() {
    let env = TestEnv::new();
    let ver = env.run(&["version"]);
    assert_eq!(code_of(&ver), EXIT_OK);
    assert!(String::from_utf8_lossy(&ver.stdout).contains("aisession-backup"));

    let comp = env.run(&["completions", "bash"]);
    assert_eq!(code_of(&comp), EXIT_OK);
    assert!(String::from_utf8_lossy(&comp.stdout).contains("aisession-backup"));
}

#[test]
fn doctor_and_scan_respect_isolated_env_and_json_mode() {
    let env = TestEnv::new();
    env.write_claude_session(
        "11111111-1111-1111-1111-111111111111",
        "/work/alpha",
        "first prompt",
    );

    let doc = env.run(&["doctor", "--json"]);
    assert_eq!(code_of(&doc), EXIT_OK);
    let doc_json: serde_json::Value = serde_json::from_slice(&doc.stdout).unwrap();
    assert!(doc_json.is_array());

    let scan = env.run(&["scan", "--provider", "claude", "--json"]);
    assert_eq!(code_of(&scan), EXIT_OK);
    let scan_json: serde_json::Value = serde_json::from_slice(&scan.stdout).unwrap();
    assert_eq!(scan_json[0]["provider"], "claude");
    assert_eq!(scan_json[0]["items"].as_array().unwrap().len(), 1);
}

#[test]
fn confirmation_gate_dry_run_and_exit_codes_for_backup_and_restore() {
    let env = TestEnv::new();
    let sid = "22222222-2222-2222-2222-222222222222";
    env.write_claude_session(sid, "C:\\Users\\win\\project", "hello\x1b[31mworld");

    let archive = env.root.join("out.tar.gz");
    let archive_str = archive.to_str().unwrap();

    let dry = env.run(&["backup", "--output", archive_str, "--dry-run"]);
    assert_eq!(code_of(&dry), EXIT_OK);
    assert!(!archive.exists());

    let no_yes = env.run(&["backup", "--output", archive_str]);
    assert_eq!(code_of(&no_yes), EXIT_CONFIRM_REQUIRED);
    assert!(!archive.exists());

    let ok = env.run(&["backup", "--output", archive_str, "--yes"]);
    assert_eq!(code_of(&ok), EXIT_OK);
    assert!(archive.exists());

    let dup = env.run(&["backup", "--output", archive_str, "--yes"]);
    assert_eq!(code_of(&dup), EXIT_PLAN_REFUSED);

    let ver = env.run(&["verify", archive_str, "--json"]);
    assert_eq!(code_of(&ver), EXIT_OK);
    let ver_json: serde_json::Value = serde_json::from_slice(&ver.stdout).unwrap();
    assert_eq!(ver_json["checked"], 1);

    let list_out = env.run(&["list", archive_str]);
    assert_eq!(code_of(&list_out), EXIT_OK);
    let list_text = String::from_utf8_lossy(&list_out.stdout);
    assert!(!list_text.contains('\x1b'));
    assert!(list_text.contains("helloworld"));

    let dest = env.root.join("restored-claude");
    let dest_str = dest.to_str().unwrap();

    let r_no_yes = env.run(&[
        "restore",
        archive_str,
        "--dest",
        dest_str,
        "--mappings",
        "C:\\Users\\win:/home/linux",
        "--allow-running",
    ]);
    assert_eq!(code_of(&r_no_yes), EXIT_CONFIRM_REQUIRED);
    assert!(!dest.exists());

    let r_dry = env.run(&[
        "restore",
        archive_str,
        "--dest",
        dest_str,
        "--mappings",
        "C:\\Users\\win:/home/linux",
        "--allow-running",
        "--dry-run",
    ]);
    assert_eq!(code_of(&r_dry), EXIT_OK);
    assert!(!dest.exists());

    let r_ok = env.run(&[
        "restore",
        archive_str,
        "--dest",
        dest_str,
        "--mappings",
        "C:\\Users\\win:/home/linux",
        "--allow-running",
        "--force",
    ]);
    assert_eq!(code_of(&r_ok), EXIT_OK);

    let encoded = claudepath::encode("/home/linux/project");
    let restored_file = dest
        .join("projects")
        .join(encoded)
        .join(format!("{sid}.jsonl"));
    assert!(restored_file.exists());

    let hist = env.run(&["history", "--json"]);
    assert_eq!(code_of(&hist), EXIT_OK);
    let hist_json: serde_json::Value = serde_json::from_slice(&hist.stdout).unwrap();
    let jid = hist_json[0]["id"].as_str().unwrap();

    let rb_no_yes = env.run(&["rollback", jid]);
    assert_eq!(code_of(&rb_no_yes), EXIT_CONFIRM_REQUIRED);

    let rb_ok = env.run(&["rollback", jid, "--yes"]);
    assert_eq!(code_of(&rb_ok), EXIT_OK);
}

#[test]
fn encrypted_backup_verify_and_exec_config_refusal() {
    let env = TestEnv::new();
    env.write_claude_session("33333333-3333-3333-3333-333333333333", "/work/sec", "msg");
    fs::write(
        env.claude_dir.join("settings.json"),
        br#"{"hooks":{"PreToolUse":[{"command":"echo hi"}]}}"#,
    )
    .unwrap();

    let enc_archive = env.root.join("backup.tar.gz.age");
    let enc_str = enc_archive.to_str().unwrap();

    let out = env
        .cmd()
        .env(
            "AISESSION_BACKUP_PASSPHRASE",
            "correct-horse-battery-staple",
        )
        .args([
            "backup",
            "--config",
            "--encrypt",
            "--output",
            enc_str,
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(code_of(&out), EXIT_OK);
    assert!(enc_archive.exists());

    let wrong = env
        .cmd()
        .env("AISESSION_BACKUP_PASSPHRASE", "wrong-passphrase")
        .args(["verify", enc_str])
        .output()
        .unwrap();
    assert_eq!(code_of(&wrong), EXIT_VERIFY_FAILED);

    let dest_parent = env.root.join("dest-parent");
    fs::create_dir_all(&dest_parent).unwrap();
    let dest = dest_parent.join(".claude");
    let dest_str = dest.to_str().unwrap();

    let skipped_exec = env
        .cmd()
        .env(
            "AISESSION_BACKUP_PASSPHRASE",
            "correct-horse-battery-staple",
        )
        .args([
            "restore",
            enc_str,
            "--dest",
            dest_str,
            "--config",
            "--allow-running",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(code_of(&skipped_exec), EXIT_OK);
    assert!(!dest.join("settings.json").exists());

    let allowed = env
        .cmd()
        .env(
            "AISESSION_BACKUP_PASSPHRASE",
            "correct-horse-battery-staple",
        )
        .args([
            "restore",
            enc_str,
            "--dest",
            dest_str,
            "--config",
            "--allow-exec-config",
            "--allow-running",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(code_of(&allowed), EXIT_OK);
    assert!(dest.join("settings.json").exists());
}

#[test]
fn audit_reports_counts_without_leaking_secret() {
    let env = TestEnv::new();
    let fake_token = "ghp_1234567890abcdefghijklmnopqrstuv";
    env.write_claude_session(
        "44444444-4444-4444-4444-444444444444",
        "/work/audit",
        fake_token,
    );

    let out = env.run(&["audit", "--json"]);
    assert_eq!(code_of(&out), EXIT_OK);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains(fake_token));
    let findings: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(findings[0]["detector"], "github");
    assert_eq!(findings[0]["count"], 1);
}

#[test]
fn tui_state_machines_and_tty_guard_work_without_terminal() {
    install_panic_hook();
    assert!(check_tty(false, true).is_err());
    assert!(check_tty(true, false).is_err());
    assert!(check_tty(false, false).is_err());
    assert!(check_tty(true, true).is_ok());

    let item = Item {
        id: "s1".into(),
        kind: ItemKind::Session,
        label: "build feature".into(),
        cwd: Some("/work/feature".into()),
        updated_at: None,
        files: vec![ItemFile {
            rel: "projects/x/s1.jsonl".into(),
            size: 256,
            class: FileClass::Plain,
        }],
    };
    let mut home = HomeState::new(vec![ProviderEntry {
        id: ProviderId::Claude,
        display_name: "Claude Code".into(),
        home: Some(PathBuf::from("/tmp/fake-claude")),
        items: vec![item],
        selected: vec![false],
        scan_error: None,
    }]);

    assert_eq!(home.screen, HomeScreen::Providers);
    home.handle_key(KeyCode::Enter);
    assert_eq!(home.screen, HomeScreen::ItemPicker);
    home.handle_key(KeyCode::Char('/'));
    home.handle_key(KeyCode::Char('f'));
    home.handle_key(KeyCode::Enter);
    assert_eq!(home.picker.as_ref().unwrap().rows.len(), 1);
    home.handle_key(KeyCode::Char('a'));
    assert_eq!(home.total_selected_items(), 1);
    home.handle_key(KeyCode::Char('b'));
    assert_eq!(home.screen, HomeScreen::ConfirmBackup);
    home.set_plan(Plan::default());
    home.handle_key(KeyCode::Enter);
    assert_eq!(home.outcome, Some(HomeOutcome::BackupConfirmed));

    let session = Session {
        id: "55555555-5555-5555-5555-555555555555".into(),
        cwd: "/mnt/e/workspace/user/app".into(),
        git_branch: None,
        claude_version: None,
        model: None,
        message_count: 3,
        size_bytes: 512,
        timestamp: chrono::Utc::now(),
        preview: "hi".into(),
        last_preview: "hi".into(),
        artifacts: Vec::new(),
    };
    let mut backup_app = BackupApp::new(vec![DiscoveredSession {
        session: session.clone(),
        jsonl_path: PathBuf::from("/tmp/s.jsonl"),
        companions: Vec::new(),
    }]);
    backup_app.handle_key(KeyCode::Char(' '));
    assert_eq!(backup_app.selected_count(), 1);
    backup_app.handle_key(KeyCode::Char('b'));
    assert_eq!(backup_app.step, BackupStep::Confirm);
    backup_app.set_plan(Plan::default());
    backup_app.handle_key(KeyCode::Char('y'));
    assert_eq!(backup_app.step, BackupStep::Done);

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        created_at: chrono::Utc::now(),
        source_host: "host".into(),
        source_os: "linux".into(),
        claude_version: None,
        sessions: vec![session],
        providers: Vec::new(),
        extras: Vec::new(),
        config: Vec::new(),
        checksum: String::new(),
        total_size: 512,
    };
    let mut restore_app = RestoreApp::new(
        manifest,
        Path::new("/tmp/a.tar.gz"),
        Path::new("/tmp/claude"),
        RestoreFlags::default(),
    );
    restore_app.handle_key(KeyCode::Char('a'));
    restore_app.handle_key(KeyCode::Char('p'));
    assert_eq!(restore_app.step, RestoreStep::Mapping);
    assert_eq!(restore_app.rows.len(), 1);
    restore_app.rows[0].value = "/Users/bob/app".into();
    assert_eq!(
        restore_app.mappings().get("/mnt/e/workspace/user/app"),
        Some(&"/Users/bob/app".to_string())
    );
    restore_app.handle_key(KeyCode::Enter);
    assert_eq!(restore_app.step, RestoreStep::Preview);
    restore_app.set_plan(Plan::default());
    restore_app.handle_key(KeyCode::Enter);
    assert!(restore_app.confirmed);
    assert_eq!(restore_app.step, RestoreStep::Done);
}

fn multi_provider_archive(env: &TestEnv) -> PathBuf {
    env.write_claude_session("11111111-1111-4111-8111-111111111111", "/work/one", "one");
    env.write_claude_session("22222222-2222-4222-8222-222222222222", "/work/two", "two");
    let day = env.codex_dir.join("sessions/2026/01/01");
    fs::create_dir_all(&day).unwrap();
    fs::write(
        day.join("rollout-2026-01-01T00-00-00-33333333-3333-4333-8333-333333333333.jsonl"),
        b"{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/work/one\"}}\n",
    )
    .unwrap();
    let archive = env.root.join("multi.tar.gz");
    let out = env.run(&[
        "backup",
        "--provider",
        "claude,codex",
        "--output",
        archive.to_str().unwrap(),
        "--yes",
    ]);
    assert_eq!(code_of(&out), EXIT_OK, "{out:?}");
    archive
}

fn tree_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| (entry.path().to_path_buf(), fs::read(entry.path()).unwrap()))
        .collect();
    out.sort();
    out
}

#[test]
fn explicit_dest_never_writes_to_real_provider_homes() {
    let env = TestEnv::new();
    let archive = multi_provider_archive(&env);
    let before = tree_snapshot(&env.codex_dir);
    let dest = env.root.join("restored/.claude");
    let args = [
        "restore",
        archive.to_str().unwrap(),
        "--dest",
        dest.to_str().unwrap(),
        "--allow-running",
        "--yes",
    ];
    let refused = env.run(&args);
    assert_eq!(code_of(&refused), EXIT_PLAN_REFUSED, "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stdout).contains("provider-home"));
    assert!(!dest.exists());

    let codex_home = env.root.join("restored/.codex");
    let spec = format!("codex={}", codex_home.display());
    let mut with_home = args.to_vec();
    with_home.extend(["--provider-home", &spec]);
    let done = env.run(&with_home);
    assert_eq!(code_of(&done), EXIT_OK, "{done:?}");
    assert!(!tree_snapshot(&codex_home).is_empty());
    assert!(dest.join("projects").exists());
    assert_eq!(tree_snapshot(&env.codex_dir), before);

    let only_claude = env.root.join("only/.claude");
    let filtered = env.run(&[
        "restore",
        archive.to_str().unwrap(),
        "--dest",
        only_claude.to_str().unwrap(),
        "--provider",
        "claude",
        "--allow-running",
        "--yes",
    ]);
    assert_eq!(code_of(&filtered), EXIT_OK, "{filtered:?}");
    assert_eq!(tree_snapshot(&env.codex_dir), before);
}

#[test]
fn provider_home_values_are_validated() {
    let env = TestEnv::new();
    let archive = multi_provider_archive(&env);
    for bad in [
        "codex",
        "nope=/x",
        "codex=relative/path",
        "claude=/x",
        "codex=/a\u{1b}b",
    ] {
        let out = env.run(&[
            "restore",
            archive.to_str().unwrap(),
            "--provider-home",
            bad,
            "--dry-run",
        ]);
        assert_ne!(code_of(&out), EXIT_OK, "{bad}");
    }
    assert!(parse_mappings("/a\u{1b}[31m:/b").is_err());
}

#[test]
fn verify_missing_archive_is_an_error_not_a_verification_failure() {
    let env = TestEnv::new();
    let out = env.run(&["verify", env.root.join("absent.tar.gz").to_str().unwrap()]);
    assert_eq!(code_of(&out), 1);
}

#[test]
fn tui_restore_request_applies_only_the_selected_sessions() {
    let env = TestEnv::new();
    let archive = multi_provider_archive(&env);
    let staged =
        aisession_backup::archive::stage(&archive, &aisession_backup::engine::Passphrase::None)
            .unwrap();
    let dest = env.root.join("tui/.claude");
    let mut app = RestoreApp::new(
        staged.manifest,
        &archive,
        &dest,
        RestoreFlags {
            allow_running: true,
            ..RestoreFlags::default()
        },
    );
    app.provider_homes
        .insert(ProviderId::Codex, env.root.join("tui/.codex"));
    let wanted = app.sessions()[0].id.clone();
    let other = app.sessions()[1].id.clone();
    app.picker.selected[0] = true;
    let prepared = aisession_backup::engine::prepare_restore(app.build_restore_request()).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    aisession_backup::engine::apply_restore(prepared).unwrap();
    let names: Vec<String> = tree_snapshot(&dest)
        .into_iter()
        .map(|(path, _)| path.display().to_string())
        .collect();
    assert!(names.iter().any(|name| name.contains(&wanted)));
    assert!(!names.iter().any(|name| name.contains(&other)));
    assert!(!env.root.join("tui/.codex").exists());
}

#[test]
fn tui_restore_selects_a_whole_directory_across_providers() {
    let env = TestEnv::new();
    let archive = multi_provider_archive(&env);
    let staged =
        aisession_backup::archive::stage(&archive, &aisession_backup::engine::Passphrase::None)
            .unwrap();
    let dest = env.root.join("tui/.claude");
    let mut app = RestoreApp::new(
        staged.manifest,
        &archive,
        &dest,
        RestoreFlags {
            allow_running: true,
            ..RestoreFlags::default()
        },
    );
    let group = app
        .picker
        .groups
        .iter()
        .position(|group| group.cwd.as_deref() == Some("/work/one"))
        .unwrap();
    assert_eq!(
        app.picker.groups[group].provider_summary(),
        "claude 1, codex 1"
    );
    let row = app
        .picker
        .rows
        .iter()
        .position(|row| *row == aisession_backup::tui::PickRow::Group(group))
        .unwrap();
    app.picker.state.select(Some(row));
    app.handle_key(KeyCode::Char(' '));
    assert_eq!(app.selected_count(), 2);
    app.handle_key(KeyCode::Char('p'));
    assert_eq!(app.step, RestoreStep::Mapping);
    assert_eq!(app.rows.len(), 1);
    assert_eq!(app.rows[0].root, "/work/one");

    let request = app.build_restore_request();
    assert_eq!(request.only_sessions.as_ref().unwrap().len(), 1);
    assert_eq!(request.only_items.as_ref().unwrap().len(), 1);
    assert!(request
        .only_items
        .as_ref()
        .unwrap()
        .iter()
        .all(|(id, _)| *id == ProviderId::Codex));

    let mut without_destination = request.clone();
    without_destination.provider_homes.clear();
    let refused = aisession_backup::engine::prepare_restore(without_destination).unwrap();
    assert!(refused.plan.has_refusals());

    let mut homes = request.clone();
    homes
        .provider_homes
        .insert(ProviderId::Codex, env.root.join("tui/.codex"));
    let prepared = aisession_backup::engine::prepare_restore(homes).unwrap();
    assert!(!prepared.plan.has_refusals(), "{:?}", prepared.plan);
    aisession_backup::engine::apply_restore(prepared).unwrap();
    let codex: Vec<String> = tree_snapshot(&env.root.join("tui/.codex"))
        .into_iter()
        .map(|(path, _)| path.display().to_string())
        .collect();
    assert_eq!(codex.len(), 1, "{codex:?}");
    let claude = tree_snapshot(&dest);
    assert_eq!(claude.len(), 1);
}

#[test]
fn list_and_scan_group_by_directory_and_flat_keeps_the_old_output() {
    let env = TestEnv::new();
    env.write_claude_session("11111111-1111-4111-8111-111111111111", "/work/one", "first");
    env.write_claude_session(
        "22222222-2222-4222-8222-222222222222",
        "/work/one",
        "second",
    );
    env.write_claude_session("33333333-3333-4333-8333-333333333333", "/work/two", "third");

    let grouped = env.run(&["list"]);
    assert_eq!(code_of(&grouped), EXIT_OK);
    let text = String::from_utf8_lossy(&grouped.stdout);
    assert_eq!(text.matches("/work/one | 2 item(s)").count(), 1);
    assert_eq!(text.matches("/work/two | 1 item(s)").count(), 1);

    let flat = env.run(&["list", "--flat"]);
    assert_eq!(code_of(&flat), EXIT_OK);
    let text = String::from_utf8_lossy(&flat.stdout);
    assert_eq!(text.matches("  /work/one\n").count(), 2);

    let scan = env.run(&["scan", "--provider", "claude"]);
    assert_eq!(code_of(&scan), EXIT_OK);
    assert!(String::from_utf8_lossy(&scan.stdout).contains("/work/one | 2 item(s)"));
    let scan_flat = env.run(&["scan", "--provider", "claude", "--flat"]);
    assert_eq!(code_of(&scan_flat), EXIT_OK);
    assert!(String::from_utf8_lossy(&scan_flat.stdout).contains("PROVIDER"));

    let json = env.run(&["list", "--json"]);
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 3);
}

#[test]
fn failed_plan_blocks_confirmation() {
    let plan = Plan::failed("boom");
    assert!(plan.has_refusals());
}

#[test]
fn backup_report_warns_about_secrets_and_changes() {
    let report = aisession_backup::engine::BackupReport {
        output: PathBuf::from("/tmp/a.tar.gz"),
        encrypted: false,
        sessions: 1,
        provider_items: 0,
        files: 1,
        bytes: 1,
        secret_findings: 2,
        changed_during_backup: vec!["evil\u{1b}[31m.jsonl".into()],
    };
    let text = ui::format_backup_report(&report, false);
    assert!(text.contains("--encrypt"));
    assert!(text.contains("1 file(s) changed"));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn tui_preview_toggles_recompute_the_plan_and_explain_refusals() {
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        created_at: chrono::Utc::now(),
        source_host: "host".into(),
        source_os: "linux".into(),
        claude_version: None,
        sessions: Vec::new(),
        providers: Vec::new(),
        extras: Vec::new(),
        config: Vec::new(),
        checksum: String::new(),
        total_size: 0,
    };
    let mut app = RestoreApp::new(
        manifest,
        Path::new("/tmp/a.tar.gz"),
        Path::new("/tmp/claude"),
        RestoreFlags::default(),
    );
    app.step = RestoreStep::Preview;
    app.set_plan(Plan::failed("provider may be running"));
    assert!(app.preview_help().contains("Blocked"));
    app.handle_key(KeyCode::Enter);
    assert!(!app.confirmed);
    app.handle_key(KeyCode::Char('r'));
    assert!(app.flags.allow_running);
    assert!(app.plan.is_none());
    assert!(app.preview_help().contains("allow-running:on"));
    app.rows = vec![aisession_backup::tui::MapRow {
        root: "/a".into(),
        count: 1,
        value: "relative".into(),
    }];
    app.step = RestoreStep::Mapping;
    app.handle_key(KeyCode::Enter);
    assert_eq!(app.step, RestoreStep::Mapping);
    assert!(app.mapping_error.is_some());
}

#[test]
fn legacy_env_and_journal_directory_keep_working() {
    let env = TestEnv::new();
    env.write_claude_session("legacy-s1", "/tmp/legacy", "hello");
    let archive = env.root.join("legacy.tar.gz");
    let archive_arg = archive.to_str().unwrap();
    let backup = env.run(&["backup", "--all", "--output", archive_arg, "--yes"]);
    assert_eq!(code_of(&backup), EXIT_OK);

    let dest = env.root.join("restored/.claude");
    let restore = env.run(&[
        "restore",
        archive_arg,
        "--dest",
        dest.to_str().unwrap(),
        "--allow-running",
        "--yes",
        "--json",
    ]);
    assert_eq!(
        code_of(&restore),
        EXIT_OK,
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&restore.stdout).unwrap();
    let id = report["journal_id"].as_str().unwrap();

    fs::rename(
        env.data_dir.join("aisession-backup"),
        env.data_dir.join("claude-migrate"),
    )
    .unwrap();

    let restored = dest
        .join("projects")
        .join(claudepath::encode("/tmp/legacy"))
        .join("legacy-s1.jsonl");
    assert!(restored.exists());

    let legacy_run = |args: &[&str]| {
        env.cmd()
            .args(args)
            .env_remove("AISESSION_BACKUP_DATA_DIR")
            .env("CLAUDE_MIGRATE_DATA_DIR", &env.data_dir)
            .output()
            .unwrap()
    };

    let history = legacy_run(&["history", "--json"]);
    assert_eq!(code_of(&history), EXIT_OK);
    assert!(String::from_utf8_lossy(&history.stdout).contains(id));

    let rollback = legacy_run(&["rollback", id, "--yes"]);
    assert_eq!(code_of(&rollback), EXIT_OK);
    assert!(!restored.exists());
}

#[test]
fn convert_codex_session_into_claude_and_roll_it_back() {
    let env = TestEnv::new();
    let day = env.codex_dir.join("sessions/2026/01/01");
    fs::create_dir_all(&day).unwrap();
    let id = "44444444-4444-4444-8444-444444444444";
    let lines = [
        serde_json::json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":id,"cwd":"/work/api","timestamp":"2026-01-01T00:00:00Z"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Add a cache layer"}]}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"reasoning","encrypted_content":"x"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done \u{1b}[31m"}]}}),
    ];
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(
        day.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();

    let dry = env.run(&[
        "convert",
        id,
        "--to",
        "claude",
        "-m",
        "/work:/srv",
        "--dry-run",
    ]);
    assert_eq!(
        code_of(&dry),
        EXIT_OK,
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(!env.claude_dir.join("projects/-srv-api").exists());

    let out = env.run(&[
        "convert",
        id,
        "--to",
        "claude",
        "-m",
        "/work:/srv",
        "--yes",
        "--json",
    ]);
    assert_eq!(
        code_of(&out),
        EXIT_OK,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["written"], 1);
    assert_eq!(report["dropped"]["reasoning"], 1);
    let session = report["session_id"].as_str().unwrap();
    let file = env
        .claude_dir
        .join("projects/-srv-api")
        .join(format!("{session}.jsonl"));
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("Add a cache layer"));
    assert!(text.contains("\"cwd\":\"/srv/api\""));

    let again = env.run(&[
        "convert",
        id,
        "--to",
        "claude",
        "-m",
        "/work:/srv",
        "--yes",
        "--json",
    ]);
    let again: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(again["written"], 0);
    assert_eq!(again["already_converted"], true);

    let human = env.run(&[
        "convert",
        file.to_str().unwrap(),
        "--to",
        "codex",
        "--dry-run",
    ]);
    assert_eq!(code_of(&human), EXIT_OK);
    assert!(!String::from_utf8_lossy(&human.stderr).contains('\x1b'));

    let journal = report["journal_id"].as_str().unwrap();
    let rollback = env.run(&["rollback", journal, "--yes"]);
    assert_eq!(code_of(&rollback), EXIT_OK);
    assert!(!file.exists());

    let bad = env.run(&["convert", id, "--to", "gemini"]);
    assert_eq!(code_of(&bad), EXIT_USAGE);
    let missing = env.run(&[
        "convert",
        "55555555-5555-4555-8555-555555555555",
        "--to",
        "claude",
    ]);
    assert_ne!(code_of(&missing), EXIT_OK);
}

#[test]
fn tui_t_key_converts_only_claude_and_codex_sessions() {
    let codex_session = Item {
        id: "x-1".into(),
        kind: ItemKind::Session,
        label: "codex work".into(),
        cwd: Some("/work/api".into()),
        updated_at: None,
        files: vec![ItemFile {
            rel: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-x-1.jsonl".into(),
            size: 10,
            class: FileClass::Plain,
        }],
    };
    let codex_config = Item {
        id: "config".into(),
        kind: ItemKind::Config,
        label: "config".into(),
        cwd: None,
        updated_at: None,
        files: Vec::new(),
    };
    let mut home = HomeState::new(vec![ProviderEntry {
        id: ProviderId::Codex,
        display_name: "Codex CLI".into(),
        home: Some(PathBuf::from("/tmp/fake-codex")),
        items: vec![codex_session, codex_config],
        selected: vec![false, false],
        scan_error: None,
    }]);
    home.handle_key(KeyCode::Enter);
    home.handle_key(KeyCode::Char('t'));
    assert_eq!(
        home.screen,
        HomeScreen::ItemPicker,
        "a directory row is not a session"
    );
    home.handle_key(KeyCode::Enter);
    home.handle_key(KeyCode::Down);
    home.handle_key(KeyCode::Char('t'));
    assert_eq!(
        home.outcome,
        Some(HomeOutcome::Convert {
            source: PathBuf::from(
                "/tmp/fake-codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-x-1.jsonl"
            ),
            from: ProviderId::Codex,
            to: ProviderId::Claude,
        })
    );
}
