use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemFile, ItemKind, LocateEnv, Provider, ProviderId, Rewritten,
    ScanOptions,
};
use crate::remap::apply_longest_mapping;

pub struct Gemini;

fn is_hash_dir(name: &str) -> bool {
    name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit())
}

fn chat_label(path: &Path) -> Option<String> {
    let value = common::read_json(path)?;
    let messages = value.get("messages")?.as_array()?;
    messages
        .iter()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(|text| text.chars().take(60).collect())
}

fn project_items(home: &Path, skipped: &mut usize, warnings: &mut Vec<String>) -> Vec<Item> {
    let mut items = Vec::new();
    let mut hash_dirs = 0;
    for project in common::subdirs(&home.join("tmp")) {
        let name = common::file_name(&project);
        if name == "bin" {
            continue;
        }
        let outcome = common::walk(home, &format!("tmp/{name}"), |rel| {
            if rel.ends_with("/logs.json") {
                FileClass::Sensitive
            } else {
                FileClass::Plain
            }
        });
        *skipped += outcome.skipped;
        let files: Vec<ItemFile> = outcome.files;
        if !files.iter().any(|file| file.rel.contains("/chats/")) {
            continue;
        }
        if is_hash_dir(&name) {
            hash_dirs += 1;
        }
        let cwd = std::fs::read_to_string(project.join(".project_root"))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let label = files
            .iter()
            .find(|file| file.rel.contains("/chats/") && file.rel.ends_with(".json"))
            .and_then(|file| chat_label(&home.join(&file.rel)))
            .or_else(|| cwd.clone())
            .unwrap_or_else(|| name.clone());
        items.push(Item {
            id: name,
            kind: ItemKind::Session,
            label,
            cwd,
            updated_at: common::newest_mtime(home, &files),
            files,
        });
    }
    if hash_dirs > 0 {
        warnings.push(format!(
            "{hash_dirs} project dirs are named by a hash of the original path; remapped sessions in them stay under the old hash"
        ));
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    items
}

impl Provider for Gemini {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }

    fn display_name(&self) -> &'static str {
        "Gemini CLI"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        env.overrides
            .get(&ProviderId::Gemini)
            .cloned()
            .or_else(|| common::home_child(env, ".gemini"))
    }

    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory> {
        let mut skipped = 0;
        let mut warnings = Vec::new();
        let mut items = project_items(home, &mut skipped, &mut warnings);

        let config = common::singles(
            home,
            &[
                ("settings.json", FileClass::ExecutesCode),
                ("trustedFolders.json", FileClass::ExecutesCode),
                ("projects.json", FileClass::Plain),
                ("state.json", FileClass::Plain),
                ("GEMINI.md", FileClass::Plain),
            ],
        );
        if !config.is_empty() {
            items.push(Item {
                id: "config".into(),
                kind: ItemKind::Config,
                label: "Gemini settings, trusted folders and projects".into(),
                cwd: None,
                updated_at: common::newest_mtime(home, &config),
                files: config,
            });
        }

        if opts.include_state {
            let history = common::walk(home, "history", |_| FileClass::OpaqueState);
            skipped += history.skipped;
            if !history.files.is_empty() {
                items.push(Item {
                    id: "state".into(),
                    kind: ItemKind::State,
                    label: "Gemini checkpoint history (opaque)".into(),
                    cwd: None,
                    updated_at: common::newest_mtime(home, &history.files),
                    files: history.files,
                });
            }
        }

        if opts.include_credentials {
            let credentials = common::singles(
                home,
                &[
                    ("oauth_creds.json", FileClass::Credential),
                    ("google_accounts.json", FileClass::Credential),
                ],
            );
            if !credentials.is_empty() {
                items.push(Item {
                    id: "credentials".into(),
                    kind: ItemKind::Credentials,
                    label: "Gemini login".into(),
                    cwd: None,
                    updated_at: common::newest_mtime(home, &credentials),
                    files: credentials,
                });
            }
        }

        if skipped > 0 {
            warnings.push(format!(
                "{skipped} entries skipped (symlinks or unsafe names)"
            ));
        }
        Ok(Inventory {
            provider: ProviderId::Gemini,
            home: home.to_path_buf(),
            items,
            warnings,
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        match rel {
            "oauth_creds.json" | "google_accounts.json" => FileClass::Credential,
            "projects.json" | "state.json" | "GEMINI.md" => FileClass::Plain,
            _ if rel.starts_with("history/") => FileClass::OpaqueState,
            _ if rel.starts_with("tmp/") && !rel.starts_with("tmp/bin/") => {
                if rel.ends_with("/logs.json") {
                    FileClass::Sensitive
                } else {
                    FileClass::Plain
                }
            }
            _ => FileClass::ExecutesCode,
        }
    }

    fn rewrite(&self, rel: &str, bytes: &[u8], mappings: &Mappings) -> Result<Rewritten> {
        if rel == "projects.json" || rel == "trustedFolders.json" {
            refuse_key_collisions(rel, bytes, mappings)?;
        }
        let (out, changes) = if rel == "projects.json" {
            common::rewrite_json(bytes, |value| {
                common::map_keys_at(value, &["projects"], mappings)
            })?
        } else if rel == "trustedFolders.json" {
            common::rewrite_json(bytes, |value| common::map_keys_at(value, &[], mappings))?
        } else if rel.starts_with("tmp/") && rel.ends_with("/.project_root") {
            rewrite_project_root(bytes, mappings)?
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
        common::process_running(&["gemini"])
    }
}

fn refuse_key_collisions(rel: &str, bytes: &[u8], mappings: &Mappings) -> Result<()> {
    let value: Value = serde_json::from_slice(bytes)?;
    let path: &[&str] = if rel == "projects.json" {
        &["projects"]
    } else {
        &[]
    };
    if common::keys_collide(&value, path, mappings) {
        bail!("mappings make two entries of {rel} collide");
    }
    Ok(())
}

fn rewrite_project_root(bytes: &[u8], mappings: &Mappings) -> Result<(Option<Vec<u8>>, usize)> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok((None, 0));
    };
    let body = text.trim_end();
    let mapped = apply_longest_mapping(body, mappings);
    if common::has_control(&mapped) {
        bail!("project root contains control characters");
    }
    if mapped == body {
        return Ok((None, 0));
    }
    let ending = &text[body.len()..];
    Ok((Some(format!("{mapped}{ending}").into_bytes()), 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mappings() -> Mappings {
        [("/old".to_string(), "/new".to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn scan_reads_project_root_and_excludes_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let chats = tmp.path().join("tmp/app/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(tmp.path().join("tmp/app/.project_root"), "/old/app\n").unwrap();
        std::fs::write(
            chats.join("s.json"),
            r#"{"messages":[{"type":"user","content":"hi there"}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("tmp/bin")).unwrap();
        std::fs::create_dir_all(tmp.path().join("tmp/nochat")).unwrap();
        std::fs::write(tmp.path().join("oauth_creds.json"), "secret").unwrap();
        let inventory = Gemini.scan(tmp.path(), &ScanOptions::default()).unwrap();
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(inventory.items[0].cwd.as_deref(), Some("/old/app"));
        assert_eq!(inventory.items[0].label, "hi there");
    }

    #[test]
    fn rewrite_projects_trusted_and_root() {
        let out = Gemini
            .rewrite(
                "projects.json",
                br#"{"projects":{"/old/a":"a"}}"#,
                &mappings(),
            )
            .unwrap();
        let value: Value = serde_json::from_slice(&out.bytes.unwrap()).unwrap();
        assert_eq!(value["projects"]["/new/a"], "a");
        let out = Gemini
            .rewrite(
                "trustedFolders.json",
                br#"{"/old/a":"TRUST_FOLDER"}"#,
                &mappings(),
            )
            .unwrap();
        assert_eq!(out.changes, 1);
        let out = Gemini
            .rewrite("tmp/app/.project_root", b"/old/a\n", &mappings())
            .unwrap();
        assert_eq!(out.bytes.unwrap(), b"/new/a\n");
    }

    #[test]
    fn project_root_refuses_control_characters() {
        let hostile: Mappings = [("/old".to_string(), "/new\nssh-ed25519 AAAA".to_string())]
            .into_iter()
            .collect();
        assert!(Gemini
            .rewrite("tmp/app/.project_root", b"/old/a\n", &hostile)
            .is_err());
        assert!(Gemini
            .rewrite("tmp/app/.project_root", b"/old/a\ninjected\n", &mappings())
            .is_err());
    }

    #[test]
    fn trust_key_collision_is_an_error() {
        let bytes = br#"{"/old/a":"TRUST_FOLDER","/new/a":"DO_NOT_TRUST"}"#;
        assert!(Gemini
            .rewrite("trustedFolders.json", bytes, &mappings())
            .is_err());
    }

    #[test]
    fn corrupt_json_is_an_error_not_a_silent_replace() {
        assert!(Gemini
            .rewrite("projects.json", b"{broken", &mappings())
            .is_err());
    }
}
