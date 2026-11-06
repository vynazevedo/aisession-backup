use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemKind, LocateEnv, Provider, ProviderId, Rewritten, ScanOptions,
};
use crate::remap::apply_longest_mapping;

pub struct Grok;

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = raw.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn map_session_dir(rel: &str, mappings: &Mappings) -> Option<String> {
    let rest = rel.strip_prefix("sessions/")?;
    let (encoded, tail) = rest.split_once('/')?;
    let cwd = percent_decode(encoded)?;
    if percent_encode(&cwd) != encoded {
        return None;
    }
    let mapped = apply_longest_mapping(&cwd, mappings);
    (mapped != cwd).then(|| format!("sessions/{}/{tail}", percent_encode(&mapped)))
}

fn session_items(home: &Path, skipped: &mut usize) -> Vec<Item> {
    let mut items = Vec::new();
    for project in common::subdirs(&home.join("sessions")) {
        let encoded = common::file_name(&project);
        let cwd = percent_decode(&encoded);
        let project_rel = format!("sessions/{encoded}");
        for session in common::subdirs(&project) {
            let id = common::file_name(&session);
            let outcome = common::walk(home, &format!("{project_rel}/{id}"), |_| FileClass::Plain);
            *skipped += outcome.skipped;
            let summary = common::read_json(&session.join("summary.json"));
            let label = summary
                .as_ref()
                .and_then(|value| value.get("generated_title"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| cwd.clone())
                .unwrap_or_else(|| id.clone());
            items.push(Item {
                updated_at: common::newest_mtime(home, &outcome.files),
                id,
                kind: ItemKind::Session,
                label,
                cwd: cwd.clone(),
                files: outcome.files,
            });
        }
        if let Some(file) = common::single(
            home,
            &format!("{project_rel}/prompt_history.jsonl"),
            FileClass::Sensitive,
        ) {
            items.push(Item {
                id: format!("prompts-{encoded}"),
                kind: ItemKind::Session,
                label: format!("Prompt history of {}", cwd.as_deref().unwrap_or(&encoded)),
                cwd: cwd.clone(),
                updated_at: common::mtime(&home.join(&file.rel)),
                files: vec![file],
            });
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    items
}

impl Provider for Grok {
    fn id(&self) -> ProviderId {
        ProviderId::Grok
    }

    fn display_name(&self) -> &'static str {
        "Grok CLI"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        env.overrides
            .get(&ProviderId::Grok)
            .cloned()
            .or_else(|| common::home_child(env, ".grok"))
    }

    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory> {
        let mut skipped = 0;
        let mut items = session_items(home, &mut skipped);

        let config = common::singles(
            home,
            &[
                ("config.toml", FileClass::ExecutesCode),
                ("trusted_folders.toml", FileClass::ExecutesCode),
            ],
        );
        if !config.is_empty() {
            items.push(Item {
                id: "config".into(),
                kind: ItemKind::Config,
                label: "Grok config and trusted folders".into(),
                cwd: None,
                updated_at: common::newest_mtime(home, &config),
                files: config,
            });
        }

        if opts.include_state {
            let mut state = common::singles(
                home,
                &[
                    ("sessions/session_search.sqlite", FileClass::OpaqueState),
                    ("worktrees.db", FileClass::OpaqueState),
                ],
            );
            let memory = common::walk(home, "memory-v2", |_| FileClass::OpaqueState);
            skipped += memory.skipped;
            state.extend(memory.files);
            if !state.is_empty() {
                items.push(Item {
                    id: "state".into(),
                    kind: ItemKind::State,
                    label: "Grok search index, worktrees and memory (opaque)".into(),
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
                    label: "Grok login".into(),
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
        Ok(Inventory {
            provider: ProviderId::Grok,
            home: home.to_path_buf(),
            items,
            warnings,
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        let session_depth = rel
            .strip_prefix("sessions/")
            .map_or(0, |rest| rest.split('/').count());
        match rel {
            "auth.json" => FileClass::Credential,
            "worktrees.db" | "sessions/session_search.sqlite" => FileClass::OpaqueState,
            _ if rel.starts_with("memory-v2/") => FileClass::OpaqueState,
            _ if session_depth == 2 && rel.ends_with("/prompt_history.jsonl") => {
                FileClass::Sensitive
            }
            _ if session_depth >= 3 => FileClass::Plain,
            _ => FileClass::ExecutesCode,
        }
    }

    fn rewrite(&self, rel: &str, bytes: &[u8], mappings: &Mappings) -> Result<Rewritten> {
        let new_rel = map_session_dir(rel, mappings);
        let mut changes = usize::from(new_rel.is_some());
        let mut out = None;
        if rel.starts_with("sessions/") && rel.ends_with("/summary.json") {
            let (rewritten, count) = common::rewrite_json(bytes, |value| {
                common::map_at(value, &["info", "cwd"], mappings)
                    + common::map_at(value, &["git_root_dir"], mappings)
            })?;
            out = rewritten;
            changes += count;
        }
        Ok(Rewritten {
            rel: new_rel.unwrap_or_else(|| rel.to_string()),
            bytes: out,
            changes,
        })
    }

    fn running_hint(&self, _home: &Path) -> Option<String> {
        common::process_running(&["grok"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mappings() -> Mappings {
        [("/old/app".to_string(), "/new app".to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn percent_roundtrip() {
        assert_eq!(percent_encode("/mnt/e/a-b_c.d"), "%2Fmnt%2Fe%2Fa-b_c.d");
        assert_eq!(percent_decode("%2Fmnt%2Fe").unwrap(), "/mnt/e");
        assert!(percent_decode("%ZZ").is_none());
        assert!(percent_decode("%2").is_none());
    }

    #[test]
    fn scan_groups_sessions_and_excludes_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("sessions/%2Fold%2Fapp/s1");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("summary.json"),
            "{\"generated_title\":\"hello\"}",
        )
        .unwrap();
        std::fs::write(session.join("chat_history.jsonl"), "{}\n").unwrap();
        std::fs::write(session.join("updates.jsonl.lock"), "").unwrap();
        std::fs::write(tmp.path().join("auth.json"), "secret").unwrap();
        let inventory = Grok.scan(tmp.path(), &ScanOptions::default()).unwrap();
        assert_eq!(inventory.items.len(), 1);
        let item = &inventory.items[0];
        assert_eq!(item.label, "hello");
        assert_eq!(item.cwd.as_deref(), Some("/old/app"));
        assert_eq!(item.files.len(), 2);
    }

    #[test]
    fn rewrite_renames_project_dir_and_summary_fields() {
        let bytes = br#"{"info":{"id":"s1","cwd":"/old/app/x"},"git_root_dir":"/old/app"}"#;
        let out = Grok
            .rewrite(
                "sessions/%2Fold%2Fapp%2Fx/s1/summary.json",
                bytes,
                &mappings(),
            )
            .unwrap();
        assert_eq!(out.rel, "sessions/%2Fnew%20app%2Fx/s1/summary.json");
        let value: Value = serde_json::from_slice(&out.bytes.unwrap()).unwrap();
        assert_eq!(value["info"]["cwd"], "/new app/x");
        assert_eq!(value["git_root_dir"], "/new app");
        assert_eq!(out.changes, 3);
    }

    #[test]
    fn rewrite_leaves_non_canonical_dirs_untouched() {
        let out = Grok
            .rewrite("sessions/%2fold%2fapp/s1/x", b"", &mappings())
            .unwrap();
        assert_eq!(out.rel, "sessions/%2fold%2fapp/s1/x");
    }
}
