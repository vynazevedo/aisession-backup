pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod common;
pub mod gemini;
pub mod grok;
pub mod maestri;
pub mod opencode;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Claude,
    Codex,
    Grok,
    Gemini,
    Antigravity,
    Opencode,
    Maestri,
}

impl ProviderId {
    pub const ALL: [ProviderId; 7] = [
        ProviderId::Claude,
        ProviderId::Codex,
        ProviderId::Grok,
        ProviderId::Gemini,
        ProviderId::Antigravity,
        ProviderId::Opencode,
        ProviderId::Maestri,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Claude => "claude",
            ProviderId::Codex => "codex",
            ProviderId::Grok => "grok",
            ProviderId::Gemini => "gemini",
            ProviderId::Antigravity => "antigravity",
            ProviderId::Opencode => "opencode",
            ProviderId::Maestri => "maestri",
        }
    }

    pub fn parse(raw: &str) -> Option<ProviderId> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "agy" => Some(ProviderId::Antigravity),
            other => ProviderId::ALL.into_iter().find(|id| id.as_str() == other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Session,
    Config,
    Workspace,
    State,
    Credentials,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClass {
    Plain,
    Sensitive,
    ExecutesCode,
    Credential,
    OpaqueState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemFile {
    pub rel: String,
    pub size: u64,
    pub class: FileClass,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: ItemKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub files: Vec<ItemFile>,
}

impl Item {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub provider: ProviderId,
    pub home: PathBuf,
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub include_state: bool,
    pub include_credentials: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Rewritten {
    pub rel: String,
    pub bytes: Option<Vec<u8>>,
    pub changes: usize,
}

pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;
    fn locate(&self, env: &LocateEnv) -> Option<PathBuf>;
    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory>;
    fn classify(&self, rel: &str) -> FileClass;
    fn rewrite(
        &self,
        rel: &str,
        bytes: &[u8],
        mappings: &BTreeMap<String, String>,
    ) -> Result<Rewritten>;
    fn running_hint(&self, home: &Path) -> Option<String> {
        let _ = home;
        None
    }
}

pub fn effective_class(declared: FileClass, derived: FileClass) -> FileClass {
    use FileClass::{Credential, ExecutesCode, Plain};
    match (declared, derived) {
        (Credential, _) | (_, Credential) => Credential,
        (ExecutesCode, _) | (_, ExecutesCode) => ExecutesCode,
        (Plain, derived) => derived,
        (declared, _) => declared,
    }
}

#[derive(Debug, Clone, Default)]
pub struct LocateEnv {
    pub user_home: Option<PathBuf>,
    pub overrides: BTreeMap<ProviderId, PathBuf>,
}

impl LocateEnv {
    pub fn from_system() -> Self {
        Self {
            user_home: dirs::home_dir(),
            overrides: BTreeMap::new(),
        }
    }
}

pub fn registry() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(claude::Claude),
        Box::new(codex::Codex),
        Box::new(grok::Grok),
        Box::new(gemini::Gemini),
        Box::new(antigravity::Antigravity),
        Box::new(opencode::Opencode),
        Box::new(maestri::Maestri),
    ]
}

pub fn find(id: ProviderId) -> Option<Box<dyn Provider>> {
    registry().into_iter().find(|provider| provider.id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_provider() {
        for id in ProviderId::ALL {
            assert_eq!(find(id).map(|provider| provider.id()), Some(id));
        }
    }

    #[test]
    fn parse_accepts_alias_and_rejects_unknown() {
        assert_eq!(ProviderId::parse(" AGY "), Some(ProviderId::Antigravity));
        assert_eq!(ProviderId::parse("maestri"), Some(ProviderId::Maestri));
        assert_eq!(ProviderId::parse("nope"), None);
    }

    #[test]
    fn classify_matches_every_class_reported_by_scan() {
        let fixtures: &[(ProviderId, &[&str])] = &[
            (
                ProviderId::Codex,
                &[
                    "sessions/2026/01/01/rollout-a.jsonl",
                    "config.toml",
                    "AGENTS.md",
                    "session_index.jsonl",
                    "history.jsonl",
                    "rules/a.rules",
                    "skills/a/SKILL.md",
                    "memories/a.md",
                    "state_5.sqlite",
                    "state_5.sqlite-wal",
                    "auth.json",
                ],
            ),
            (
                ProviderId::Grok,
                &[
                    "sessions/%2Fwork/s1/summary.json",
                    "sessions/%2Fwork/s1/nested/events.jsonl",
                    "sessions/%2Fwork/prompt_history.jsonl",
                    "sessions/session_search.sqlite",
                    "config.toml",
                    "trusted_folders.toml",
                    "worktrees.db",
                    "memory-v2/a.db",
                    "auth.json",
                ],
            ),
            (
                ProviderId::Gemini,
                &[
                    "tmp/slug/chats/a.json",
                    "tmp/slug/logs.json",
                    "tmp/slug/.project_root",
                    "settings.json",
                    "trustedFolders.json",
                    "projects.json",
                    "state.json",
                    "GEMINI.md",
                    "history/a/b",
                    "oauth_creds.json",
                    "google_accounts.json",
                ],
            ),
            (
                ProviderId::Antigravity,
                &[
                    "antigravity-cli/conversations/c1.db",
                    "antigravity-cli/brain/c1/note.md",
                    "antigravity-cli/settings.json",
                    "antigravity-cli/keybindings.json",
                    "antigravity-cli/history.jsonl",
                    "antigravity-cli/conversation_summaries.db",
                    "antigravity-cli/knowledge/k.md",
                    "antigravity-cli/implicit/a",
                    "antigravity-cli/annotations/a",
                    "antigravity-cli/antigravity-oauth-token",
                    "config/config.json",
                    "config/mcp_config.json",
                    "config/skills/a/SKILL.md",
                ],
            ),
            (
                ProviderId::Maestri,
                &[
                    "app-state.json",
                    "preferences.json",
                    "sidebar.json",
                    "workspaces/w1/workspace.json",
                    "workspaces/w1/notes/n.md",
                    "prompts/p.md",
                    "backups/a.maestribak",
                    "license.json",
                    ".license-credentials.json",
                    "accounts/a.json",
                    "git-hosts/g.json",
                ],
            ),
            (ProviderId::Opencode, &["storage/s/a.json", "auth.json"]),
        ];
        let opts = ScanOptions {
            include_state: true,
            include_credentials: true,
        };
        for (id, files) in fixtures {
            let provider = find(*id).unwrap();
            let tmp = tempfile::tempdir().unwrap();
            for rel in *files {
                let path = tmp.path().join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"{}").unwrap();
            }
            if *id == ProviderId::Maestri {
                std::fs::write(
                    tmp.path().join("manifest.json"),
                    br#"{"files":{"sidebarLayout":"sidebar.json"}}"#,
                )
                .unwrap();
            }
            let inventory = provider.scan(tmp.path(), &opts).unwrap();
            let scanned: Vec<_> = inventory
                .items
                .iter()
                .flat_map(|item| item.files.iter())
                .collect();
            for rel in *files {
                assert!(
                    scanned.iter().any(|file| file.rel == *rel),
                    "{id:?} fixture {rel} was not scanned"
                );
            }
            for file in scanned {
                assert_eq!(
                    provider.classify(&file.rel),
                    file.class,
                    "{id:?} {}",
                    file.rel
                );
            }
        }
    }

    #[test]
    fn classify_fails_closed_on_unknown_paths() {
        for provider in registry() {
            for rel in ["unknown.sh", "bin/run", "extensions/a/hook.js"] {
                assert_eq!(
                    provider.classify(rel),
                    FileClass::ExecutesCode,
                    "{:?} {rel}",
                    provider.id()
                );
            }
        }
    }

    #[test]
    fn effective_class_never_lowers_a_security_class() {
        use FileClass::*;
        assert_eq!(effective_class(Plain, ExecutesCode), ExecutesCode);
        assert_eq!(effective_class(Plain, Credential), Credential);
        assert_eq!(effective_class(ExecutesCode, Plain), ExecutesCode);
        assert_eq!(effective_class(Credential, ExecutesCode), Credential);
        assert_eq!(effective_class(OpaqueState, Plain), OpaqueState);
        assert_eq!(effective_class(Plain, Sensitive), Sensitive);
    }
}
