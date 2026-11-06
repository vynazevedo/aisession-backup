use std::path::{Path, PathBuf};

use anyhow::Result;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemFile, ItemKind, LocateEnv, Provider, ProviderId, Rewritten,
    ScanOptions,
};

pub struct Claude;

fn relative_file(home: &Path, path: &Path, class: FileClass) -> Option<ItemFile> {
    let rel = common::rel_of(home, path)?;
    let size = std::fs::symlink_metadata(path).ok()?.len();
    Some(ItemFile { rel, size, class })
}

impl Provider for Claude {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        env.overrides
            .get(&ProviderId::Claude)
            .cloned()
            .or_else(|| {
                std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .filter(|path| path.is_dir())
            })
            .or_else(|| common::home_child(env, ".claude"))
    }

    fn scan(&self, home: &Path, _opts: &ScanOptions) -> Result<Inventory> {
        let mut items = Vec::new();
        for found in crate::discover::load_sessions(home)? {
            let mut files: Vec<ItemFile> = relative_file(home, &found.jsonl_path, FileClass::Plain)
                .into_iter()
                .collect();
            files.extend(
                found
                    .companions
                    .iter()
                    .filter_map(|(_, path)| relative_file(home, path, FileClass::Plain)),
            );
            let session = found.session;
            let label = if session.preview.is_empty() {
                session.cwd.clone()
            } else {
                session.preview.clone()
            };
            items.push(Item {
                id: session.id,
                kind: ItemKind::Session,
                label,
                cwd: (!session.cwd.is_empty()).then_some(session.cwd),
                updated_at: Some(session.timestamp),
                files,
            });
        }
        Ok(Inventory {
            provider: ProviderId::Claude,
            home: home.to_path_buf(),
            items,
            warnings: Vec::new(),
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        if rel == ".credentials.json" {
            FileClass::Credential
        } else if ["projects/", "session-env/", "todos/"]
            .iter()
            .any(|prefix| rel.starts_with(prefix))
        {
            FileClass::Plain
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
        common::process_running(&["claude"])
    }
}
