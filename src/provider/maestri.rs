use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::common::{self, Mappings};
use super::{
    FileClass, Inventory, Item, ItemKind, LocateEnv, Provider, ProviderId, Rewritten, ScanOptions,
};

const MARKER: &str = "manifest.json";

pub struct Maestri;

fn is_maestri_home(path: &Path) -> bool {
    path.join(MARKER).is_file()
}

fn wsl_candidates(mount_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for drive in common::subdirs(mount_root) {
        for user in common::subdirs(&drive.join("Users")) {
            let candidate = user.join(".maestri");
            if is_maestri_home(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

fn class_in_workspace(rel: &str) -> FileClass {
    if rel.ends_with("/workspace.json") {
        FileClass::ExecutesCode
    } else {
        FileClass::Plain
    }
}

fn workspace_items(home: &Path, skipped: &mut usize) -> Vec<Item> {
    let mut items = Vec::new();
    for workspace in common::subdirs(&home.join("workspaces")) {
        let id = common::file_name(&workspace);
        let outcome = common::walk(home, &format!("workspaces/{id}"), class_in_workspace);
        *skipped += outcome.skipped;
        let payload = common::read_json(&workspace.join("workspace.json"))
            .and_then(|value| value.get("payload").cloned());
        let text = |key: &str| {
            payload
                .as_ref()
                .and_then(|value| value.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        items.push(Item {
            label: text("name").unwrap_or_else(|| id.clone()),
            cwd: text("workingDirectory"),
            updated_at: common::newest_mtime(home, &outcome.files),
            id,
            kind: ItemKind::Workspace,
            files: outcome.files,
        });
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    items
}

fn top_level_config(home: &Path) -> Vec<super::ItemFile> {
    let mut files = common::singles(
        home,
        &[
            ("manifest.json", FileClass::Plain),
            ("app-state.json", FileClass::Plain),
            ("preferences.json", FileClass::ExecutesCode),
        ],
    );
    let sidebar = common::read_json(&home.join(MARKER)).and_then(|value| {
        value
            .get("files")?
            .get("sidebarLayout")?
            .as_str()
            .map(str::to_string)
    });
    if let Some(rel) = sidebar.filter(|rel| is_plain_sidebar(rel)) {
        files.extend(common::single(home, &rel, FileClass::Plain));
    }
    files
}

fn is_plain_sidebar(rel: &str) -> bool {
    const RESERVED: &[&str] = &[
        "license.json",
        ".license-credentials.json",
        "manifest.json",
        "app-state.json",
        "preferences.json",
    ];
    crate::safety::validate_rel(rel).is_ok()
        && !rel.contains('/')
        && rel.ends_with(".json")
        && !RESERVED.contains(&rel)
}

fn rewrite_workspace(value: &mut Value, mappings: &Mappings) -> usize {
    let mut changes = common::map_at(value, &["payload", "workingDirectory"], mappings);
    if let Some(nodes) = value
        .get_mut("payload")
        .and_then(|payload| payload.get_mut("nodes"))
        .and_then(Value::as_array_mut)
    {
        for node in nodes {
            changes += common::map_at(
                node,
                &["content", "terminal", "_0", "workingDirectory"],
                mappings,
            );
        }
    }
    changes
}

fn rewrite_manifest(value: &mut Value, mappings: &Mappings) -> usize {
    let Some(workspaces) = value.get_mut("workspaces").and_then(Value::as_array_mut) else {
        return 0;
    };
    workspaces
        .iter_mut()
        .map(|workspace| common::map_at(workspace, &["path"], mappings))
        .sum()
}

impl Provider for Maestri {
    fn id(&self) -> ProviderId {
        ProviderId::Maestri
    }

    fn display_name(&self) -> &'static str {
        "Maestri"
    }

    fn locate(&self, env: &LocateEnv) -> Option<PathBuf> {
        if let Some(path) = env.overrides.get(&ProviderId::Maestri) {
            return Some(path.clone());
        }
        if let Some(path) = std::env::var_os("MAESTRI_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(path));
        }
        if let Some(path) = common::home_child(env, ".maestri").filter(|path| is_maestri_home(path))
        {
            return Some(path);
        }
        let mut candidates = wsl_candidates(Path::new("/mnt"));
        (candidates.len() == 1).then(|| candidates.remove(0))
    }

    fn scan(&self, home: &Path, opts: &ScanOptions) -> Result<Inventory> {
        let mut skipped = 0;
        let mut items = workspace_items(home, &mut skipped);

        let mut config = top_level_config(home);
        let prompts = common::walk(home, "prompts", |_| FileClass::ExecutesCode);
        skipped += prompts.skipped;
        config.extend(prompts.files);
        if !config.is_empty() {
            items.push(Item {
                id: "config".into(),
                kind: ItemKind::Config,
                label: "Maestri preferences, agent presets, roles and prompts".into(),
                cwd: None,
                updated_at: common::newest_mtime(home, &config),
                files: config,
            });
        }

        if opts.include_state {
            let backups = common::walk(home, "backups", |_| FileClass::OpaqueState);
            skipped += backups.skipped;
            if !backups.files.is_empty() {
                items.push(Item {
                    id: "state".into(),
                    kind: ItemKind::State,
                    label: "Maestri workspace autosave backups (opaque)".into(),
                    cwd: None,
                    updated_at: common::newest_mtime(home, &backups.files),
                    files: backups.files,
                });
            }
        }

        if opts.include_credentials {
            let mut credentials = common::singles(
                home,
                &[
                    (".license-credentials.json", FileClass::Credential),
                    ("license.json", FileClass::Credential),
                ],
            );
            for sub in ["accounts", "git-hosts"] {
                let outcome = common::walk(home, sub, |_| FileClass::Credential);
                skipped += outcome.skipped;
                credentials.extend(outcome.files);
            }
            if !credentials.is_empty() {
                items.push(Item {
                    id: "credentials".into(),
                    kind: ItemKind::Credentials,
                    label: "Maestri license, accounts and git hosts".into(),
                    cwd: None,
                    updated_at: common::newest_mtime(home, &credentials),
                    files: credentials,
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
            provider: ProviderId::Maestri,
            home: home.to_path_buf(),
            items,
            warnings,
        })
    }

    fn classify(&self, rel: &str) -> FileClass {
        match rel {
            "license.json" | ".license-credentials.json" => FileClass::Credential,
            "manifest.json" | "app-state.json" => FileClass::Plain,
            _ if rel.starts_with("accounts/") || rel.starts_with("git-hosts/") => {
                FileClass::Credential
            }
            _ if rel.starts_with("backups/") => FileClass::OpaqueState,
            _ if rel.starts_with("workspaces/") => class_in_workspace(rel),
            _ if is_plain_sidebar(rel) => FileClass::Plain,
            _ => FileClass::ExecutesCode,
        }
    }

    fn rewrite(&self, rel: &str, bytes: &[u8], mappings: &Mappings) -> Result<Rewritten> {
        let is_workspace = rel.starts_with("workspaces/") && rel.ends_with("/workspace.json");
        let (out, changes) = if is_workspace {
            common::rewrite_json(bytes, |value| rewrite_workspace(value, mappings))?
        } else if rel == MARKER {
            common::rewrite_json(bytes, |value| rewrite_manifest(value, mappings))?
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
        if std::env::var_os("MAESTRI_SOCKET").is_some() {
            return Some(
                "running inside a Maestri terminal; the app autosaves over restored files".into(),
            );
        }
        common::process_running(&["maestri", "Maestri"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let workspace = home.join("workspaces/w1");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(
            workspace.join("workspace.json"),
            r#"{"schemaVersion":1,"type":"workspace","payload":{"name":"proj","workingDirectory":"/old/app","nodes":[{"content":{"terminal":{"_0":{"workingDirectory":"/old/app/sub","command":"claude"}}}},{"content":{"stickyNote":{"_0":{"fileName":"n.md"}}}}]}}"#,
        )
        .unwrap();
        std::fs::write(workspace.join("notes/n.md"), "note").unwrap();
        std::fs::write(
            home.join("manifest.json"),
            r#"{"app":"Maestri","files":{"sidebarLayout":"sidebar.json"},"workspaces":[{"id":"w1","name":"proj","path":"/old/app"}]}"#,
        )
        .unwrap();
        std::fs::write(home.join("sidebar.json"), "{}").unwrap();
        std::fs::write(home.join("preferences.json"), "{}").unwrap();
        std::fs::write(home.join("license.json"), "secret").unwrap();
        std::fs::write(home.join(".license-credentials.json"), "secret").unwrap();
        std::fs::create_dir_all(home.join("accounts/providers")).unwrap();
        std::fs::write(home.join("accounts/providers/codex.json"), "secret").unwrap();
        std::fs::create_dir_all(home.join("backups")).unwrap();
        std::fs::write(home.join("backups/x.maestribak"), "zip").unwrap();
        std::fs::write(home.join("updater.log"), "log").unwrap();
        tmp
    }

    #[test]
    fn scan_defaults_exclude_secrets_logs_and_backups() {
        let tmp = fixture();
        let inventory = Maestri.scan(tmp.path(), &ScanOptions::default()).unwrap();
        let rels: Vec<&str> = inventory
            .items
            .iter()
            .flat_map(|item| item.files.iter().map(|file| file.rel.as_str()))
            .collect();
        for forbidden in [
            "license.json",
            ".license-credentials.json",
            "accounts/providers/codex.json",
            "updater.log",
            "backups/x.maestribak",
        ] {
            assert!(!rels.contains(&forbidden), "{forbidden} leaked");
        }
        assert!(rels.contains(&"sidebar.json"));
        let workspace = inventory
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Workspace)
            .unwrap();
        assert_eq!(workspace.label, "proj");
        assert_eq!(workspace.cwd.as_deref(), Some("/old/app"));
        let json = workspace
            .files
            .iter()
            .find(|file| file.rel.ends_with("workspace.json"))
            .unwrap();
        assert_eq!(json.class, FileClass::ExecutesCode);
    }

    #[test]
    fn credentials_are_classified_when_opted_in() {
        let tmp = fixture();
        let opts = ScanOptions {
            include_state: true,
            include_credentials: true,
        };
        let inventory = Maestri.scan(tmp.path(), &opts).unwrap();
        let credentials = inventory
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Credentials)
            .unwrap();
        assert_eq!(credentials.files.len(), 3);
        assert!(credentials
            .files
            .iter()
            .all(|file| file.class == FileClass::Credential));
    }

    #[test]
    fn sidebar_cannot_launder_credentials_into_the_default_scan() {
        let tmp = fixture();
        for target in ["license.json", "accounts/providers/codex.json"] {
            let manifest = format!(r#"{{"files":{{"sidebarLayout":"{target}"}}}}"#);
            std::fs::write(tmp.path().join("manifest.json"), manifest).unwrap();
            let inventory = Maestri.scan(tmp.path(), &ScanOptions::default()).unwrap();
            let leaked = inventory
                .items
                .iter()
                .flat_map(|item| item.files.iter())
                .any(|file| file.rel == target);
            assert!(!leaked, "{target} leaked");
        }
    }

    #[test]
    fn hostile_sidebar_path_is_ignored() {
        let tmp = fixture();
        std::fs::write(
            tmp.path().join("manifest.json"),
            r#"{"files":{"sidebarLayout":"../../etc/passwd"}}"#,
        )
        .unwrap();
        let files = top_level_config(tmp.path());
        assert!(files.iter().all(|file| !file.rel.contains("..")));
    }

    #[test]
    fn rewrite_workspace_and_manifest_preserve_other_fields() {
        let tmp = fixture();
        let mappings: Mappings = [("/old".to_string(), "/new".to_string())]
            .into_iter()
            .collect();
        let rel = "workspaces/w1/workspace.json";
        let out = Maestri
            .rewrite(
                rel,
                &std::fs::read(tmp.path().join(rel)).unwrap(),
                &mappings,
            )
            .unwrap();
        assert_eq!(out.changes, 2);
        let value: Value = serde_json::from_slice(&out.bytes.unwrap()).unwrap();
        assert_eq!(value["payload"]["workingDirectory"], "/new/app");
        assert_eq!(
            value["payload"]["nodes"][0]["content"]["terminal"]["_0"]["workingDirectory"],
            "/new/app/sub"
        );
        assert_eq!(
            value["payload"]["nodes"][0]["content"]["terminal"]["_0"]["command"],
            "claude"
        );
        let out = Maestri
            .rewrite(
                "manifest.json",
                &std::fs::read(tmp.path().join("manifest.json")).unwrap(),
                &mappings,
            )
            .unwrap();
        let value: Value = serde_json::from_slice(&out.bytes.unwrap()).unwrap();
        assert_eq!(value["workspaces"][0]["path"], "/new/app");
    }

    #[test]
    fn wsl_lookup_requires_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("c/Users/u/.maestri")).unwrap();
        assert!(wsl_candidates(tmp.path()).is_empty());
        std::fs::write(tmp.path().join("c/Users/u/.maestri/manifest.json"), "{}").unwrap();
        assert_eq!(wsl_candidates(tmp.path()).len(), 1);
    }
}
