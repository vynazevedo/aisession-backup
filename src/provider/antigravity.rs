use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemKind, LocateEnv, Provider, ProviderId, Rewritten, ScanOptions,
};

const ROOT: &str = "antigravity-cli";

pub struct Antigravity;

#[derive(Default, Clone)]
struct ConversationInfo {
    workspace: Option<String>,
    display: Option<String>,
}

fn history_index(home: &Path) -> BTreeMap<String, ConversationInfo> {
    let mut index: BTreeMap<String, ConversationInfo> = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(home.join(ROOT).join("history.jsonl")) else {
        return index;
    };
    for value in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        let Some(id) = value.get("conversationId").and_then(Value::as_str) else {
            continue;
        };
        let info = index.entry(id.to_string()).or_default();
        if info.workspace.is_none() {
            info.workspace = value
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if info.display.is_none() {
            info.display = value
                .get("display")
                .and_then(Value::as_str)
                .map(|text| text.chars().take(60).collect());
        }
    }
    index
}

fn conversation_items(home: &Path, skipped: &mut usize) -> Vec<Item> {
    let index = history_index(home);
    let conversations = common::walk(home, &format!("{ROOT}/conversations"), |_| {
        FileClass::OpaqueState
    });
    *skipped += conversations.skipped;
    let mut grouped: BTreeMap<String, Vec<super::ItemFile>> = BTreeMap::new();
    for file in conversations.files {
        let name = file.rel.rsplit('/').next().unwrap_or_default();
        let id = name.split(".db").next().unwrap_or(name).to_string();
        grouped.entry(id).or_default().push(file);
    }
    let mut items: Vec<Item> = grouped
        .into_iter()
        .map(|(id, mut files)| {
            let brain = common::walk(home, &format!("{ROOT}/brain/{id}"), |_| FileClass::Plain);
            *skipped += brain.skipped;
            files.extend(brain.files);
            let info = index.get(&id).cloned().unwrap_or_default();
            Item {
                label: info
                    .display
                    .clone()
                    .or_else(|| info.workspace.clone())
                    .unwrap_or_else(|| id.clone()),
                cwd: info.workspace,
                updated_at: common::newest_mtime(home, &files),
                id,
                kind: ItemKind::Session,
                files,
            }
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    items
}

impl Provider for Antigravity {
    fn id(&self) -> ProviderId {
        ProviderId::Antigravity
    }

    fn display_name(&self) -> &'static str {
        "Antigravity CLI"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        env.overrides
            .get(&ProviderId::Antigravity)
            .cloned()
            .or_else(|| {
                common::home_child(env, ".gemini").filter(|gemini| gemini.join(ROOT).is_dir())
            })
    }

    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory> {
        let mut skipped = 0;
        let mut items = conversation_items(home, &mut skipped);

        let settings = format!("{ROOT}/settings.json");
        let keybindings = format!("{ROOT}/keybindings.json");
        let history = format!("{ROOT}/history.jsonl");
        let summaries = format!("{ROOT}/conversation_summaries.db");
        let mut config = common::singles(
            home,
            &[
                (settings.as_str(), FileClass::ExecutesCode),
                (keybindings.as_str(), FileClass::Plain),
                (history.as_str(), FileClass::Sensitive),
                (summaries.as_str(), FileClass::OpaqueState),
                ("config/config.json", FileClass::Plain),
                ("config/mcp_config.json", FileClass::ExecutesCode),
            ],
        );
        let skills = common::walk(home, "config/skills", |_| FileClass::ExecutesCode);
        skipped += skills.skipped;
        config.extend(skills.files);
        let knowledge = common::walk(home, &format!("{ROOT}/knowledge"), |_| FileClass::Plain);
        skipped += knowledge.skipped;
        config.extend(knowledge.files);
        if !config.is_empty() {
            items.push(Item {
                id: "config".into(),
                kind: ItemKind::Config,
                label: "Antigravity settings, MCP, skills, knowledge and history".into(),
                cwd: None,
                updated_at: common::newest_mtime(home, &config),
                files: config,
            });
        }

        if opts.include_state {
            let implicit = common::walk(home, &format!("{ROOT}/implicit"), |_| {
                FileClass::OpaqueState
            });
            skipped += implicit.skipped;
            let mut state = implicit.files;
            let annotations = common::walk(home, &format!("{ROOT}/annotations"), |_| {
                FileClass::OpaqueState
            });
            skipped += annotations.skipped;
            state.extend(annotations.files);
            if !state.is_empty() {
                items.push(Item {
                    id: "state".into(),
                    kind: ItemKind::State,
                    label: "Antigravity implicit context and annotations (opaque)".into(),
                    cwd: None,
                    updated_at: common::newest_mtime(home, &state),
                    files: state,
                });
            }
        }

        if opts.include_credentials {
            let rel = format!("{ROOT}/antigravity-oauth-token");
            if let Some(file) = common::single(home, &rel, FileClass::Credential) {
                items.push(Item {
                    id: "credentials".into(),
                    kind: ItemKind::Credentials,
                    label: "Antigravity login".into(),
                    cwd: None,
                    updated_at: common::mtime(&home.join(&rel)),
                    files: vec![file],
                });
            }
        }

        let mut warnings = vec![
            "Antigravity conversations are SQLite databases copied verbatim: paths inside them are not remapped".to_string(),
        ];
        if skipped > 0 {
            warnings.push(format!(
                "{skipped} entries skipped (symlinks or unsafe names)"
            ));
        }
        Ok(Inventory {
            provider: ProviderId::Antigravity,
            home: home.to_path_buf(),
            items,
            warnings,
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        if rel == "config/config.json" {
            return FileClass::Plain;
        }
        let Some(inner) = rel
            .strip_prefix(ROOT)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return FileClass::ExecutesCode;
        };
        match inner {
            "antigravity-oauth-token" => FileClass::Credential,
            "keybindings.json" => FileClass::Plain,
            "history.jsonl" => FileClass::Sensitive,
            "conversation_summaries.db" => FileClass::OpaqueState,
            _ if ["conversations/", "implicit/", "annotations/"]
                .iter()
                .any(|prefix| inner.starts_with(prefix)) =>
            {
                FileClass::OpaqueState
            }
            _ if inner.starts_with("brain/") || inner.starts_with("knowledge/") => FileClass::Plain,
            _ => FileClass::ExecutesCode,
        }
    }

    fn rewrite(&self, rel: &str, bytes: &[u8], mappings: &Mappings) -> Result<Rewritten> {
        let (out, changes) = if rel == format!("{ROOT}/history.jsonl") {
            common::rewrite_jsonl(bytes, |value| {
                common::map_at(value, &["workspace"], mappings)
            })
        } else if rel == format!("{ROOT}/settings.json") {
            common::rewrite_json(bytes, |value| {
                common::map_array_at(value, &["trustedWorkspaces"], mappings)
            })?
        } else {
            (None, 0)
        };
        Ok(Rewritten {
            rel: rel.to_string(),
            bytes: out,
            changes,
        })
    }

    fn running_hint(&self, _home: &Path) -> Option<String> {
        common::process_running(&["agy", "antigravity"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(ROOT);
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        std::fs::create_dir_all(root.join("brain/c1/scratch")).unwrap();
        std::fs::write(root.join("conversations/c1.db"), "db").unwrap();
        std::fs::write(root.join("conversations/c1.db-wal"), "wal").unwrap();
        std::fs::write(root.join("brain/c1/scratch/n.md"), "note").unwrap();
        std::fs::write(
            root.join("history.jsonl"),
            "{\"display\":\"fix bug\",\"timestamp\":1,\"workspace\":\"/old/app\",\"conversationId\":\"c1\",\"type\":\"x\"}\n",
        )
        .unwrap();
        std::fs::write(root.join("antigravity-oauth-token"), "secret").unwrap();
        std::fs::write(
            root.join("settings.json"),
            r#"{"trustedWorkspaces":["/old/app"]}"#,
        )
        .unwrap();
        tmp
    }

    #[test]
    fn scan_groups_conversation_with_wal_and_brain() {
        let tmp = fixture();
        let inventory = Antigravity
            .scan(tmp.path(), &ScanOptions::default())
            .unwrap();
        let session = inventory
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Session)
            .unwrap();
        assert_eq!(session.id, "c1");
        assert_eq!(session.label, "fix bug");
        assert_eq!(session.cwd.as_deref(), Some("/old/app"));
        assert_eq!(session.files.len(), 3);
        let all: Vec<&str> = inventory
            .items
            .iter()
            .flat_map(|item| item.files.iter().map(|file| file.rel.as_str()))
            .collect();
        assert!(!all.iter().any(|rel| rel.contains("oauth")));
    }

    #[test]
    fn rewrite_history_and_trusted_workspaces() {
        let mappings: Mappings = [("/old".to_string(), "/new".to_string())]
            .into_iter()
            .collect();
        let tmp = fixture();
        let rel = format!("{ROOT}/history.jsonl");
        let out = Antigravity
            .rewrite(
                &rel,
                &std::fs::read(tmp.path().join(&rel)).unwrap(),
                &mappings,
            )
            .unwrap();
        assert!(String::from_utf8(out.bytes.unwrap())
            .unwrap()
            .contains("\"workspace\":\"/new/app\""));
        let rel = format!("{ROOT}/settings.json");
        let out = Antigravity
            .rewrite(
                &rel,
                &std::fs::read(tmp.path().join(&rel)).unwrap(),
                &mappings,
            )
            .unwrap();
        assert_eq!(out.changes, 1);
    }
}
