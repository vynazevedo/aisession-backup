use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemFile, ItemKind, LocateEnv, Provider, ProviderId, Rewritten,
    ScanOptions,
};
use crate::remap::apply_longest_mapping;

const STATE_DATABASES: &[&str] = &[
    "state_5.sqlite",
    "thread_history_1.sqlite",
    "memories_1.sqlite",
    "goals_1.sqlite",
];

pub struct Codex;

fn thread_names(home: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(home.join("session_index.jsonl")) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.to_string();
            let name = value.get("thread_name")?.as_str()?.to_string();
            Some((id, name))
        })
        .collect()
}

fn session_item(home: &Path, file: ItemFile, names: &BTreeMap<String, String>) -> Item {
    let path: PathBuf = home.join(&file.rel);
    let meta = common::read_first_json_line(&path);
    let payload = meta.as_ref().and_then(|value| value.get("payload"));
    let id = payload
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| common::file_name(&path));
    let cwd = payload
        .and_then(|value| value.get("cwd"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let label = names
        .get(&id)
        .cloned()
        .or_else(|| cwd.clone())
        .unwrap_or_else(|| id.clone());
    Item {
        id,
        kind: ItemKind::Session,
        label,
        cwd,
        updated_at: common::last_timestamp(&path).or_else(|| common::mtime(&path)),
        files: vec![file],
    }
}

fn config_class(rel: &str) -> FileClass {
    if rel.starts_with("skills/") || rel.starts_with("rules/") {
        FileClass::ExecutesCode
    } else {
        FileClass::Plain
    }
}

fn rewrite_config_toml(bytes: &[u8], mappings: &Mappings) -> Result<(Option<Vec<u8>>, usize)> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok((None, 0));
    };
    let mut changes = 0;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let key = body
            .strip_prefix("[projects.\"")
            .and_then(|rest| rest.strip_suffix("\"]"))
            .filter(|key| !key.contains('\\'));
        match key {
            Some(key) => {
                let mapped = apply_longest_mapping(key, mappings);
                if common::has_control(&mapped) {
                    bail!("mapped project path contains control characters");
                }
                if mapped != key && !mapped.contains(['\\', '"']) {
                    changes += 1;
                    out.push_str(&format!("[projects.\"{mapped}\"]{ending}"));
                } else {
                    out.push_str(line);
                }
            }
            None => out.push_str(line),
        }
    }
    if changes == 0 {
        Ok((None, 0))
    } else {
        Ok((Some(out.into_bytes()), changes))
    }
}

impl Provider for Codex {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        env.overrides
            .get(&ProviderId::Codex)
            .cloned()
            .or_else(|| {
                std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_dir())
            })
            .or_else(|| common::home_child(env, ".codex"))
    }

    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory> {
        let names = thread_names(home);
        let sessions = common::walk(home, "sessions", |_| FileClass::Plain);
        let mut skipped = sessions.skipped;
        let mut items: Vec<Item> = sessions
            .files
            .into_iter()
            .filter(|file| file.rel.ends_with(".jsonl"))
            .map(|file| session_item(home, file, &names))
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));

        let mut config = common::singles(
            home,
            &[
                ("config.toml", FileClass::ExecutesCode),
                ("AGENTS.md", FileClass::Plain),
                ("session_index.jsonl", FileClass::Plain),
                ("history.jsonl", FileClass::Sensitive),
            ],
        );
        for sub in ["rules", "skills", "memories"] {
            let outcome = common::walk(home, sub, config_class);
            skipped += outcome.skipped;
            config.extend(outcome.files);
        }
        if !config.is_empty() {
            items.push(Item {
                id: "config".into(),
                kind: ItemKind::Config,
                label: "Codex config, rules, skills and history".into(),
                cwd: None,
                updated_at: common::newest_mtime(home, &config),
                files: config,
            });
        }

        if opts.include_state {
            let mut state = Vec::new();
            for database in STATE_DATABASES {
                for suffix in ["", "-wal", "-shm"] {
                    let rel = format!("{database}{suffix}");
                    state.extend(common::single(home, &rel, FileClass::OpaqueState));
                }
            }
            if !state.is_empty() {
                items.push(Item {
                    id: "state".into(),
                    kind: ItemKind::State,
                    label: "Codex SQLite state (opaque, paths are not remapped)".into(),
                    cwd: None,
                    updated_at: common::newest_mtime(home, &state),
                    files: state,
                });
            }
        }

        if opts.include_credentials {
            if let Some(file) = common::single(home, "auth.json", FileClass::Credential) {
                items.push(Item {
                    id: "credentials".into(),
                    kind: ItemKind::Credentials,
                    label: "Codex login".into(),
                    cwd: None,
                    updated_at: common::mtime(&home.join("auth.json")),
                    files: vec![file],
                });
            }
        }

        let mut warnings = Vec::new();
        if skipped > 0 {
            warnings.push(format!(
                "{skipped} entries skipped (symlinks or unsafe names)"
            ));
        }
        warnings.push(
            "Codex keeps a thread index in SQLite that is not rewritten: after remapping, use the rollout files as the source of truth".into(),
        );
        Ok(Inventory {
            provider: ProviderId::Codex,
            home: home.to_path_buf(),
            items,
            warnings,
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        let is_state = STATE_DATABASES.iter().any(|database| {
            rel.strip_prefix(database)
                .is_some_and(|suffix| ["", "-wal", "-shm"].contains(&suffix))
        });
        match rel {
            "auth.json" => FileClass::Credential,
            "AGENTS.md" | "session_index.jsonl" => FileClass::Plain,
            "history.jsonl" => FileClass::Sensitive,
            _ if is_state => FileClass::OpaqueState,
            _ if rel.starts_with("sessions/") || rel.starts_with("memories/") => FileClass::Plain,
            _ => FileClass::ExecutesCode,
        }
    }

    fn rewrite(&self, rel: &str, bytes: &[u8], mappings: &Mappings) -> Result<Rewritten> {
        let (out, changes) = if rel == "config.toml" {
            rewrite_config_toml(bytes, mappings)?
        } else if rel.starts_with("sessions/") && rel.ends_with(".jsonl") {
            common::rewrite_jsonl(bytes, |value| {
                common::map_at(value, &["payload", "cwd"], mappings)
                    + common::map_array_at(value, &["payload", "workspace_roots"], mappings)
                    + common::map_array_at(value, &["payload", "runtime_workspace_roots"], mappings)
            })
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
        common::process_running(&["codex"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mappings() -> Mappings {
        [("/old".to_string(), "/new".to_string())]
            .into_iter()
            .collect()
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("sessions/2026/01/02")).unwrap();
        std::fs::write(
            home.join("sessions/2026/01/02/rollout-x.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\",\"cwd\":\"/old/app\",\"runtime_workspace_roots\":[\"/old/app\"]}}\n{\"type\":\"turn_context\",\"payload\":{\"cwd\":\"/old/app/sub\",\"workspace_roots\":[\"/old/app\"]}}\n",
        )
        .unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"abc\",\"thread_name\":\"my thread\"}\n",
        )
        .unwrap();
        std::fs::write(
            home.join("config.toml"),
            "model = \"x\"\n[projects.\"/old/app\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        std::fs::write(home.join("auth.json"), "{\"token\":\"secret\"}").unwrap();
        std::fs::write(home.join("state_5.sqlite"), "db").unwrap();
        std::fs::write(home.join("logs_2.sqlite"), "db").unwrap();
        tmp
    }

    #[test]
    fn scan_defaults_exclude_credentials_state_and_logs() {
        let tmp = fixture();
        let inventory = Codex.scan(tmp.path(), &ScanOptions::default()).unwrap();
        let rels: Vec<&str> = inventory
            .items
            .iter()
            .flat_map(|item| item.files.iter().map(|file| file.rel.as_str()))
            .collect();
        assert!(rels.contains(&"sessions/2026/01/02/rollout-x.jsonl"));
        assert!(rels.contains(&"config.toml"));
        assert!(!rels.contains(&"auth.json"));
        assert!(!rels.contains(&"state_5.sqlite"));
        assert!(!rels.contains(&"logs_2.sqlite"));
        let session = inventory
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Session)
            .unwrap();
        assert_eq!(session.id, "abc");
        assert_eq!(session.label, "my thread");
        assert_eq!(session.cwd.as_deref(), Some("/old/app"));
    }

    #[test]
    fn scan_flags_opt_in_credentials_and_state() {
        let tmp = fixture();
        let opts = ScanOptions {
            include_state: true,
            include_credentials: true,
        };
        let inventory = Codex.scan(tmp.path(), &opts).unwrap();
        let credentials = inventory
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Credentials)
            .unwrap();
        assert_eq!(credentials.files[0].class, FileClass::Credential);
        assert!(inventory
            .items
            .iter()
            .any(|item| item.kind == ItemKind::State));
    }

    #[test]
    fn config_rewrite_refuses_control_characters_in_mapped_path() {
        let hostile: Mappings = [("/old".to_string(), "/new\nowned = true\n".to_string())]
            .into_iter()
            .collect();
        let toml = b"[projects.\"/old/app\"]\n";
        assert!(Codex.rewrite("config.toml", toml, &hostile).is_err());
    }

    #[test]
    fn rewrite_rollout_and_config() {
        let tmp = fixture();
        let rel = "sessions/2026/01/02/rollout-x.jsonl";
        let out = Codex
            .rewrite(
                rel,
                &std::fs::read(tmp.path().join(rel)).unwrap(),
                &mappings(),
            )
            .unwrap();
        assert_eq!(out.changes, 4);
        let text = String::from_utf8(out.bytes.unwrap()).unwrap();
        assert!(!text.contains("/old"));
        let out = Codex
            .rewrite(
                "config.toml",
                &std::fs::read(tmp.path().join("config.toml")).unwrap(),
                &mappings(),
            )
            .unwrap();
        assert_eq!(out.changes, 1);
        assert!(String::from_utf8(out.bytes.unwrap())
            .unwrap()
            .contains("[projects.\"/new/app\"]"));
    }
}
