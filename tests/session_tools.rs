use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use aisession_backup::claudepath;
use aisession_backup::cli::{EXIT_OK, EXIT_PLAN_REFUSED};

const CLAUDE_ID: &str = "51515151-5151-4515-8515-515151515151";
const CODEX_ID: &str = "62626262-6262-4626-8626-626262626262";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        let claude = home.join(".claude");
        let codex = home.join(".codex");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&codex).unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
            claude,
            codex,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aisession-backup"))
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex)
            .env("MAESTRI_HOME", self.home.join(".maestri"))
            .env("AISESSION_BACKUP_DATA_DIR", self.root.join("data"))
            .env("NO_COLOR", "1")
            .env_remove("AISESSION_BACKUP_PASSPHRASE")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(
            out.status.code(),
            Some(EXIT_OK as i32),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn claude_session(&self, cwd: &str, text: &str) -> PathBuf {
        let dir = self.claude.join("projects").join(claudepath::encode(cwd));
        fs::create_dir_all(&dir).unwrap();
        let usage = serde_json::json!({"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":300,"cache_creation_input_tokens":4});
        let lines = [
            serde_json::json!({"type":"user","uuid":"u1","sessionId":CLAUDE_ID,"cwd":cwd,"timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":text}}),
            serde_json::json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":CLAUDE_ID,"cwd":cwd,"timestamp":"2026-11-01T10:00:05Z","message":{"id":"m1","role":"assistant","model":"claude-test","usage":usage,"content":[{"type":"text","text":"answer one"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}),
            serde_json::json!({"type":"assistant","uuid":"a2","parentUuid":"a1","sessionId":CLAUDE_ID,"cwd":cwd,"timestamp":"2026-11-01T10:00:06Z","message":{"id":"m1","role":"assistant","model":"claude-test","usage":usage,"content":[{"type":"text","text":"same message, second block"}]}}),
        ];
        let path = dir.join(format!("{CLAUDE_ID}.jsonl"));
        fs::write(
            &path,
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        path
    }

    fn codex_session(&self, cwd: &str) -> PathBuf {
        let dir = self.codex.join("sessions/2026/11/02");
        fs::create_dir_all(&dir).unwrap();
        let lines = [
            serde_json::json!({"timestamp":"2026-11-02T09:00:00Z","type":"session_meta","payload":{"id":CODEX_ID,"cwd":cwd,"timestamp":"2026-11-02T09:00:00Z"}}),
            serde_json::json!({"timestamp":"2026-11-02T09:00:00Z","type":"turn_context","payload":{"cwd":cwd,"model":"codex-test"}}),
            serde_json::json!({"timestamp":"2026-11-02T09:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"codex task"}]}}),
            serde_json::json!({"timestamp":"2026-11-02T09:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"codex done"}]}}),
            serde_json::json!({"timestamp":"2026-11-02T09:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":50}}}}),
        ];
        let path = dir.join(format!("rollout-2026-11-02T09-00-00-{CODEX_ID}.jsonl"));
        fs::write(
            &path,
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        path
    }
}

fn cwd_of(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    first
        .get("cwd")
        .or_else(|| first.pointer("/payload/cwd"))
        .and_then(|value| value.as_str())
        .unwrap()
        .to_string()
}

#[test]
fn show_renders_safe_markdown_and_html_and_never_overwrites() {
    let env = Env::new();
    let secret = format!("sk-ant-api03-{}AA", "B".repeat(90));
    env.claude_session(
        "/work/show",
        &format!("check this <script>alert(1)</script> \u{1b}]0;title\u{7}\u{1b}[31mred {secret}"),
    );

    let summary = env.ok(&["show", CLAUDE_ID]);
    assert!(summary.contains("Claude Code"));
    assert!(summary.contains("1 from the user"));
    assert!(summary.contains("Tool calls     1"));
    assert!(!summary.contains('\u{1b}'));

    let markdown = env.ok(&["show", CLAUDE_ID, "--format", "markdown", "--redact"]);
    assert!(markdown.contains("**Tool call** `Bash`"));
    assert!(!markdown.contains(&secret));
    assert!(!markdown.contains('\u{1b}'));

    let target = env.root.join("session.html");
    env.ok(&[
        "show",
        CLAUDE_ID,
        "--format",
        "html",
        "-o",
        target.to_str().unwrap(),
    ]);
    let html = fs::read_to_string(&target).unwrap();
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script"));
    assert!(html.contains("default-src 'none'"));
    assert!(!html.contains('\u{1b}'));

    fs::write(&target, b"keep me").unwrap();
    let again = env.run(&[
        "show",
        CLAUDE_ID,
        "--format",
        "html",
        "-o",
        target.to_str().unwrap(),
    ]);
    assert_ne!(again.status.code(), Some(EXIT_OK as i32));
    assert_eq!(fs::read(&target).unwrap(), b"keep me");
}

#[test]
fn stats_counts_tokens_once_per_message_across_tools() {
    let env = Env::new();
    env.claude_session("/work/stats", "hello");
    env.codex_session("/work/api");
    let out = env.ok(&["stats", "--days", "0", "--json"]);
    let stats: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(stats["sessions"], 2);
    assert_eq!(stats["total"]["input"], 10 + 600);
    assert_eq!(stats["total"]["output"], 20 + 50);
    assert_eq!(stats["total"]["cache_read"], 300 + 400);
    assert_eq!(stats["total"]["cache_write"], 4);
    let models: Vec<&str> = stats["by_model"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["key"].as_str().unwrap())
        .collect();
    assert!(models.contains(&"claude-test") && models.contains(&"codex-test"));
    let text = env.ok(&["stats", "--days", "0"]);
    assert!(text.contains("By project"));
}

#[test]
fn resume_blocks_when_the_directory_is_missing() {
    let env = Env::new();
    let missing = env.root.join("gone").to_string_lossy().into_owned();
    env.claude_session(&missing, "hello");
    let blocked = env.run(&["resume", CLAUDE_ID, "--json"]);
    assert_eq!(blocked.status.code(), Some(EXIT_PLAN_REFUSED as i32));
    let plan: serde_json::Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert!(plan["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "directory exists" && check["ok"] == false));

    fs::create_dir_all(&missing).unwrap();
    let ready = env.ok(&["resume", CLAUDE_ID]);
    assert!(ready.contains(&format!("claude --resume {CLAUDE_ID}")));
}

#[test]
fn relocate_moves_sessions_to_the_new_folder_and_rolls_back() {
    let env = Env::new();
    let old_root = "/old/place";
    let new_root = env.root.join("new place").to_string_lossy().into_owned();
    let original = env.claude_session(&format!("{old_root}/app"), "hello");
    let rollout = env.codex_session(&format!("{old_root}/api"));
    fs::create_dir_all(format!("{new_root}/app")).unwrap();
    let before_rollout = fs::read(&rollout).unwrap();

    env.ok(&[
        "relocate",
        old_root,
        &new_root,
        "--allow-running",
        "--dry-run",
    ]);
    assert_eq!(fs::read(&rollout).unwrap(), before_rollout);

    let out = env.ok(&[
        "relocate",
        &format!("{old_root}/"),
        &new_root,
        "--allow-running",
        "--yes",
        "--json",
    ]);
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["sessions"], 2);
    let copy = env
        .claude
        .join("projects")
        .join(claudepath::encode(&format!("{new_root}/app")))
        .join(format!("{CLAUDE_ID}.jsonl"));
    let joined = |leaf: &str| {
        Path::new(&new_root)
            .join(leaf)
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(cwd_of(&copy), joined("app"));
    assert_eq!(cwd_of(&original), format!("{old_root}/app"));
    assert_eq!(cwd_of(&rollout), joined("api"));

    let resume = env.ok(&["resume", CLAUDE_ID]);
    assert!(resume.contains("claude --resume"));

    let again = env.ok(&[
        "relocate",
        old_root,
        &new_root,
        "--allow-running",
        "--yes",
        "--json",
    ]);
    let again: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_eq!(again["written"], 0);

    let journal = report["journal_id"].as_str().unwrap();
    env.ok(&["rollback", journal, "--yes"]);
    assert!(!copy.exists());
    assert_eq!(fs::read(&rollout).unwrap(), before_rollout);
    assert!(original.exists());
}

#[test]
fn relocate_rejects_relative_and_empty_paths() {
    let env = Env::new();
    env.claude_session("/work/x", "hello");
    for (old, new) in [("relative", "/abs"), ("/abs", "relative"), ("", "/abs")] {
        let out = env.run(&["relocate", old, new, "--allow-running", "--yes"]);
        assert_ne!(out.status.code(), Some(EXIT_OK as i32), "{old} -> {new}");
    }
}
