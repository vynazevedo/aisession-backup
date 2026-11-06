//! Backup and restore of the user's global Claude Code configuration: global
//! settings, the user `CLAUDE.md`, plugin setup, and the global MCP server
//! registrations from `~/.claude.json`.
//!
//! Login credentials (`.credentials.json`, `oauthAccount`) are excluded unless
//! `include_credentials` is set, because they are account/machine bound and
//! carry your session secret.

use std::path::{Path, PathBuf};

use serde_json::Value;

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
fn read_regular(path: &Path) -> anyhow::Result<Vec<u8>> {
    crate::archive::ensure_regular(path)?;
    Ok(std::fs::read(path)?)
}

pub fn collect(claude_dir: &Path, include_credentials: bool) -> ConfigBundle {
    let mut files = Vec::new();
    let mut has_secrets = false;

    for name in PLAIN_FILES {
        if let Ok(bytes) = read_regular(&claude_dir.join(name)) {
            files.push(((*name).to_string(), bytes));
        }
    }
    for name in PLUGIN_FILES {
        if let Ok(bytes) = read_regular(&claude_dir.join("plugins").join(name)) {
            files.push((format!("plugins/{name}"), bytes));
        }
    }

    if let Some(claude_json) = claude_json_path(claude_dir) {
        if let Ok(text) = read_regular(&claude_json) {
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
        if let Ok(bytes) = read_regular(&claude_dir.join(".credentials.json")) {
            files.push((".credentials.json".to_string(), bytes));
            has_secrets = true;
        }
    }

    ConfigBundle { files, has_secrets }
}
