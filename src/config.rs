//! Backup and restore of the user's global Claude Code configuration: global
//! settings, the user `CLAUDE.md`, plugin setup, and the global MCP server
//! registrations from `~/.claude.json`.
//!
//! Login credentials (`.credentials.json`, `oauthAccount`) are excluded unless
//! `include_credentials` is set, because they are account/machine bound and
//! carry your session secret.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Plain config files copied verbatim from the Claude directory root.
const PLAIN_FILES: &[&str] = &["settings.json", "settings.local.json", "CLAUDE.md"];

/// Plugin setup files under `<claude>/plugins/` (the cloned repos and caches
/// are intentionally skipped; they are re-fetched on demand).
const PLUGIN_FILES: &[&str] = &[
    "config.json",
    "installed_plugins.json",
    "known_marketplaces.json",
    "blocklist.json",
];

/// Files collected for a config backup, as `(archive-relative name, bytes)`
/// stored under the `config/` prefix in the archive.
pub struct ConfigBundle {
    pub files: Vec<(String, Vec<u8>)>,
    /// True when the bundle contains MCP definitions or credentials, which may
    /// embed API keys or login secrets.
    pub has_secrets: bool,
}

/// The global `~/.claude.json` lives as a sibling of the `.claude` directory.
fn claude_json_path(claude_dir: &Path) -> Option<PathBuf> {
    claude_dir.parent().map(|p| p.join(".claude.json"))
}

/// Gathers the portable configuration from `claude_dir`.
pub fn collect(claude_dir: &Path, include_credentials: bool) -> ConfigBundle {
    let mut files = Vec::new();
    let mut has_secrets = false;

    for name in PLAIN_FILES {
        if let Ok(bytes) = std::fs::read(claude_dir.join(name)) {
            files.push(((*name).to_string(), bytes));
        }
    }
    for name in PLUGIN_FILES {
        if let Ok(bytes) = std::fs::read(claude_dir.join("plugins").join(name)) {
            files.push((format!("plugins/{name}"), bytes));
        }
    }

    if let Some(claude_json) = claude_json_path(claude_dir) {
        if let Ok(text) = std::fs::read(&claude_json) {
            if let Ok(value) = serde_json::from_slice::<Value>(&text) {
                if let Some(mcp) = value.get("mcpServers") {
                    if mcp.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
                        if let Ok(bytes) = serde_json::to_vec_pretty(mcp) {
                            files.push(("mcp-servers.json".to_string(), bytes));
                            has_secrets = true;
                        }
                    }
                }
                if include_credentials {
                    if let Some(account) = value.get("oauthAccount") {
                        if let Ok(bytes) = serde_json::to_vec_pretty(account) {
                            files.push(("oauth-account.json".to_string(), bytes));
                        }
                    }
                }
            }
        }
    }

    if include_credentials {
        if let Ok(bytes) = std::fs::read(claude_dir.join(".credentials.json")) {
            files.push((".credentials.json".to_string(), bytes));
            has_secrets = true;
        }
    }

    ConfigBundle { files, has_secrets }
}

#[derive(Default)]
pub struct ConfigReport {
    pub written: usize,
    pub skipped: usize,
    pub mcp_merged: usize,
}

/// Applies an archived config bundle to `claude_dir`. Plain files are written
/// only when absent unless `overwrite`; MCP servers are merged into the target
/// `~/.claude.json` additively (existing servers kept unless `overwrite`).
pub fn restore_config(
    claude_dir: &Path,
    entries: &[(String, Vec<u8>)],
    overwrite: bool,
    dry_run: bool,
) -> ConfigReport {
    let mut report = ConfigReport::default();
    for (name, bytes) in entries {
        match name.as_str() {
            "mcp-servers.json" => {
                report.mcp_merged += merge_mcp_servers(claude_dir, bytes, overwrite, dry_run);
            }
            "oauth-account.json" => {
                merge_top_level(claude_dir, "oauthAccount", bytes, overwrite, dry_run);
            }
            other => {
                write_plain(claude_dir, other, bytes, overwrite, dry_run, &mut report);
            }
        }
    }
    report
}

fn write_plain(
    claude_dir: &Path,
    rel: &str,
    bytes: &[u8],
    overwrite: bool,
    dry_run: bool,
    report: &mut ConfigReport,
) {
    let dest = claude_dir.join(rel);
    if dest.exists() && !overwrite {
        report.skipped += 1;
        return;
    }
    if dry_run {
        report.written += 1;
        return;
    }
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if std::fs::write(&dest, bytes).is_ok() {
        report.written += 1;
    }
}

/// Merges the archived `mcpServers` object into the target `~/.claude.json`
/// without disturbing any other key. Returns how many servers were added.
fn merge_mcp_servers(claude_dir: &Path, bytes: &[u8], overwrite: bool, dry_run: bool) -> usize {
    let incoming: Map<String, Value> = match serde_json::from_slice(bytes) {
        Ok(Value::Object(m)) => m,
        _ => return 0,
    };
    let path = match claude_json_path(claude_dir) {
        Some(p) => p,
        None => return 0,
    };

    let mut root = read_json_object(&path);
    let servers = root
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = match servers.as_object_mut() {
        Some(s) => s,
        None => return 0,
    };

    let mut merged = 0;
    for (name, def) in incoming {
        if servers.contains_key(&name) && !overwrite {
            continue;
        }
        servers.insert(name, def);
        merged += 1;
    }

    if !dry_run && merged > 0 {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_vec_pretty(&Value::Object(root)) {
            let _ = std::fs::write(&path, text);
        }
    }
    merged
}

fn merge_top_level(claude_dir: &Path, key: &str, bytes: &[u8], overwrite: bool, dry_run: bool) {
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return,
    };
    let path = match claude_json_path(claude_dir) {
        Some(p) => p,
        None => return,
    };
    let mut root = read_json_object(&path);
    if root.contains_key(key) && !overwrite {
        return;
    }
    root.insert(key.to_string(), value);
    if !dry_run {
        if let Ok(text) = serde_json::to_vec_pretty(&Value::Object(root)) {
            let _ = std::fs::write(&path, text);
        }
    }
}

fn read_json_object(path: &Path) -> Map<String, Value> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_adds_new_servers_without_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let claude_json = home.join(".claude.json");
        std::fs::write(
            &claude_json,
            br#"{"theme":"dark","mcpServers":{"existing":{"command":"x"}}}"#,
        )
        .unwrap();

        let incoming = br#"{"existing":{"command":"NEW"},"playwright":{"command":"npx"}}"#;
        let added = merge_mcp_servers(&claude_dir, incoming, false, false);
        assert_eq!(added, 1, "only the new server is added");

        let v: Value = serde_json::from_slice(&std::fs::read(&claude_json).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark", "unrelated keys preserved");
        assert_eq!(v["mcpServers"]["existing"]["command"], "x", "existing kept");
        assert_eq!(
            v["mcpServers"]["playwright"]["command"], "npx",
            "new merged"
        );
    }

    #[test]
    fn merge_overwrite_replaces_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            br#"{"mcpServers":{"a":{"command":"old"}}}"#,
        )
        .unwrap();
        let added = merge_mcp_servers(&claude_dir, br#"{"a":{"command":"new"}}"#, true, false);
        assert_eq!(added, 1);
        let v: Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(v["mcpServers"]["a"]["command"], "new");
    }

    #[test]
    fn merge_creates_file_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let added = merge_mcp_servers(&claude_dir, br#"{"a":{"command":"x"}}"#, false, false);
        assert_eq!(added, 1);
        assert!(tmp.path().join(".claude.json").exists());
    }
}
