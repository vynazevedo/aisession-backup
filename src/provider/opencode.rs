use std::path::{Path, PathBuf};

use anyhow::Result;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemKind, LocateEnv, Provider, ProviderId, Rewritten, ScanOptions,
};

pub struct Opencode;

impl Provider for Opencode {
    fn id(&self) -> ProviderId {
        ProviderId::Opencode
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        env.overrides
            .get(&ProviderId::Opencode)
            .cloned()
            .or_else(|| common::home_child(env, ".local/share/opencode"))
    }

    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory> {
        let storage = common::walk(home, "storage", |_| FileClass::OpaqueState);
        let mut warnings = vec![
            "OpenCode support is verbatim only: its format was not validated and paths are not remapped".to_string(),
        ];
        if storage.skipped > 0 {
            warnings.push(format!(
                "{} entries skipped (symlinks or unsafe names)",
                storage.skipped
            ));
        }
        let mut items = Vec::new();
        if !storage.files.is_empty() {
            items.push(Item {
                id: "storage".into(),
                kind: ItemKind::Session,
                label: "OpenCode storage".into(),
                cwd: None,
                updated_at: common::newest_mtime(home, &storage.files),
                files: storage.files,
            });
        }
        if opts.include_credentials {
            if let Some(file) = common::single(home, "auth.json", FileClass::Credential) {
                items.push(Item {
                    id: "credentials".into(),
                    kind: ItemKind::Credentials,
                    label: "OpenCode login".into(),
                    cwd: None,
                    updated_at: common::mtime(&home.join("auth.json")),
                    files: vec![file],
                });
            }
        }
        Ok(Inventory {
            provider: ProviderId::Opencode,
            home: home.to_path_buf(),
            items,
            warnings,
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        if rel == "auth.json" {
            FileClass::Credential
        } else if rel.starts_with("storage/") {
            FileClass::OpaqueState
        } else {
            FileClass::ExecutesCode
        }
    }

    fn rewrite(&self, rel: &str, _bytes: &[u8], _mappings: &Mappings) -> Result<Rewritten> {
        Ok(Rewritten {
            rel: rel.to_string(),
            bytes: None,
            changes: 0,
        })
    }

    fn running_hint(&self, _home: &Path) -> Option<String> {
        common::process_running(&["opencode"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_warns_and_excludes_auth() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("storage/s")).unwrap();
        std::fs::write(tmp.path().join("storage/s/a.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("auth.json"), "secret").unwrap();
        let inventory = Opencode.scan(tmp.path(), &ScanOptions::default()).unwrap();
        assert_eq!(inventory.items.len(), 1);
        assert!(inventory.warnings[0].contains("verbatim"));
    }
}
