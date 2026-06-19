//! Path remapping: longest-prefix substitution, project-root detection for
//! the TUI, OS-aware suggestions, and structured rewriting of `cwd` (and,
//! optionally, embedded tool paths) inside the session JSONL.

use std::collections::BTreeMap;

use serde_json::Value;

/// Applies the mapping whose source prefix is the longest match for `path`.
/// Returns `path` unchanged when nothing matches.
pub fn apply_longest_mapping(path: &str, mappings: &BTreeMap<String, String>) -> String {
    let mut best_src = "";
    let mut best_dst = "";
    for (src, dst) in mappings {
        if is_prefix(src, path) && src.len() > best_src.len() {
            best_src = src;
            best_dst = dst;
        }
    }
    if best_src.is_empty() {
        return path.to_string();
    }
    format!("{}{}", best_dst, &path[best_src.len()..])
}

/// True when `prefix` is a path-boundary prefix of `path` (so `/a/b` matches
/// `/a/b` and `/a/b/c` but not `/a/bc`).
fn is_prefix(prefix: &str, path: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')
}

/// A detected common project root and how many sessions sit under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    pub path: String,
    pub session_count: usize,
}

#[derive(Default)]
struct TrieNode {
    children: BTreeMap<String, TrieNode>,
    count: usize,
}

fn split_path(path: &str) -> Vec<String> {
    path.trim_start_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// Groups a set of session directories into the smallest set of shared roots,
/// so the user maps a handful of roots instead of every directory.
pub fn detect_project_roots(dirs: &[String]) -> Vec<ProjectRoot> {
    if dirs.is_empty() {
        return Vec::new();
    }
    let mut root = TrieNode::default();
    for dir in dirs {
        let mut node = &mut root;
        for part in split_path(dir) {
            node = node.children.entry(part).or_default();
        }
        node.count += 1;
    }

    let mut roots = Vec::new();
    find_roots(&root, &mut Vec::new(), &mut roots);
    roots.sort_by_key(|r| std::cmp::Reverse(r.session_count));
    roots
}

fn count_subtree(node: &TrieNode) -> usize {
    let mut total = node.count;
    for child in node.children.values() {
        total += count_subtree(child);
    }
    total
}

fn find_roots(node: &TrieNode, path: &mut Vec<String>, roots: &mut Vec<ProjectRoot>) {
    if node.children.is_empty() {
        if !path.is_empty() && node.count > 0 {
            roots.push(ProjectRoot {
                path: format!("/{}", path.join("/")),
                session_count: node.count,
            });
        }
        return;
    }
    if node.count > 0 && !path.is_empty() {
        roots.push(ProjectRoot {
            path: format!("/{}", path.join("/")),
            session_count: count_subtree(node),
        });
        return;
    }
    if node.children.len() == 1 {
        for (name, child) in &node.children {
            path.push(name.clone());
            find_roots(child, path, roots);
            path.pop();
        }
        return;
    }
    if path.is_empty() {
        for (name, child) in &node.children {
            let mut p = vec![name.clone()];
            find_roots(child, &mut p, roots);
        }
        return;
    }
    for (name, child) in &node.children {
        let mut child_path = path.clone();
        child_path.push(name.clone());
        roots.push(ProjectRoot {
            path: format!("/{}", child_path.join("/")),
            session_count: count_subtree(child),
        });
    }
}

/// Suggests a destination path when migrating between operating systems.
/// Supports linux <-> darwin home/mount remapping; returns the source path
/// unchanged for unknown structures so the user can edit it.
pub fn suggest_mapping(
    source_path: &str,
    source_os: &str,
    target_os: &str,
    target_user: &str,
) -> String {
    let source_os = normalize_os(source_os);
    let target_os = normalize_os(target_os);
    if source_os == target_os {
        return source_path.to_string();
    }
    let parts = split_path(source_path);
    if parts.is_empty() {
        return source_path.to_string();
    }

    match (source_os, target_os) {
        ("linux", "darwin") => match parts.as_slice() {
            [first, _user, rest @ ..] if first == "home" || first == "mnt" => {
                return join_user("/Users", target_user, rest);
            }
            _ => {}
        },
        ("darwin", "linux") => match parts.as_slice() {
            [first, _user, rest @ ..] if first == "Users" => {
                return join_user("/home", target_user, rest);
            }
            _ => {}
        },
        _ => {}
    }
    source_path.to_string()
}

/// Normalizes OS identifiers so Rust's `macos` and Go's `darwin` are treated
/// the same.
fn normalize_os(os: &str) -> &str {
    match os {
        "macos" | "darwin" => "darwin",
        other => other,
    }
}

fn join_user(base: &str, user: &str, rest: &[String]) -> String {
    if rest.is_empty() {
        format!("{base}/{user}")
    } else {
        format!("{base}/{user}/{}", rest.join("/"))
    }
}

/// Result of rewriting one JSONL file.
pub struct RewriteStats {
    pub lines_total: usize,
    pub lines_changed: usize,
}

/// Rewrites `cwd` (and, when `rewrite_tool_paths`, any embedded absolute path
/// that matches a mapping prefix) for every line of a session JSONL. Lines
/// that need no change are kept byte-for-byte; only modified lines are
/// reserialized, preserving key order.
pub fn rewrite_jsonl(
    content: &str,
    mappings: &BTreeMap<String, String>,
    rewrite_tool_paths: bool,
) -> (String, RewriteStats) {
    let mut out = String::with_capacity(content.len());
    let mut stats = RewriteStats {
        lines_total: 0,
        lines_changed: 0,
    };

    let trailing_newline = content.ends_with('\n');
    let mut lines = content.split('\n').peekable();

    while let Some(line) = lines.next() {
        let is_last = lines.peek().is_none();
        // `split('\n')` yields a trailing empty element when the input ends
        // with a newline; skip emitting that phantom line.
        if is_last && line.is_empty() && trailing_newline {
            break;
        }
        stats.lines_total += 1;

        if !mappings_touch_line(line, mappings) {
            out.push_str(line);
            if !is_last || trailing_newline {
                out.push('\n');
            }
            continue;
        }

        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                let changed = rewrite_value(&mut value, mappings, rewrite_tool_paths);
                if changed {
                    stats.lines_changed += 1;
                    let serialized =
                        serde_json::to_string(&value).unwrap_or_else(|_| line.to_string());
                    out.push_str(&serialized);
                } else {
                    out.push_str(line);
                }
            }
            Err(_) => out.push_str(line),
        }
        if !is_last || trailing_newline {
            out.push('\n');
        }
    }

    (out, stats)
}

/// Cheap pre-filter: does any mapping source prefix appear as a substring?
fn mappings_touch_line(line: &str, mappings: &BTreeMap<String, String>) -> bool {
    mappings.keys().any(|src| line.contains(src.as_str()))
}

fn rewrite_value(
    value: &mut Value,
    mappings: &BTreeMap<String, String>,
    rewrite_tool_paths: bool,
) -> bool {
    let mut changed = false;
    if let Value::Object(map) = value {
        if let Some(Value::String(cwd)) = map.get_mut("cwd") {
            let new_cwd = apply_longest_mapping(cwd, mappings);
            if &new_cwd != cwd {
                *cwd = new_cwd;
                changed = true;
            }
        }
        if rewrite_tool_paths {
            for (key, child) in map.iter_mut() {
                if key == "cwd" {
                    continue;
                }
                changed |= rewrite_tool_paths_recursive(child, mappings);
            }
        }
    }
    changed
}

/// Replaces any string value that begins with a mapped prefix. Only applied
/// under the explicit `--rewrite-tool-paths` opt-in because it is heuristic.
fn rewrite_tool_paths_recursive(value: &mut Value, mappings: &BTreeMap<String, String>) -> bool {
    let mut changed = false;
    match value {
        Value::String(s) => {
            let mapped = apply_longest_mapping(s, mappings);
            if &mapped != s {
                *s = mapped;
                changed = true;
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= rewrite_tool_paths_recursive(item, mappings);
            }
        }
        Value::Object(map) => {
            for child in map.values_mut() {
                changed |= rewrite_tool_paths_recursive(child, mappings);
            }
        }
        _ => {}
    }
    changed
}

/// Rewrites path-like string values in a `session-env` JSON document in a
/// structured way (parse, walk, replace), instead of blind substring
/// substitution over the raw text.
pub fn rewrite_env_json(content: &[u8], mappings: &BTreeMap<String, String>) -> Vec<u8> {
    let mut value: Value = match serde_json::from_slice(content) {
        Ok(v) => v,
        Err(_) => return content.to_vec(),
    };
    rewrite_tool_paths_recursive(&mut value, mappings);
    serde_json::to_vec(&value).unwrap_or_else(|_| content.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn longest_prefix_wins() {
        let m = map(&[("/mnt/e", "/short"), ("/mnt/e/workspace/vyn", "/long")]);
        assert_eq!(
            apply_longest_mapping("/mnt/e/workspace/vyn/project", &m),
            "/long/project"
        );
    }

    #[test]
    fn prefix_respects_boundary() {
        let m = map(&[("/home/n00b", "/Users/vyn")]);
        assert_eq!(apply_longest_mapping("/home/n00b", &m), "/Users/vyn");
        assert_eq!(
            apply_longest_mapping("/home/n00b/.config", &m),
            "/Users/vyn/.config"
        );
        assert_eq!(apply_longest_mapping("/home/n00bxyz", &m), "/home/n00bxyz");
    }

    #[test]
    fn unmapped_path_unchanged() {
        let m = map(&[("/home/n00b", "/Users/vyn")]);
        assert_eq!(apply_longest_mapping("/opt/data", &m), "/opt/data");
    }

    #[test]
    fn suggest_linux_to_darwin() {
        assert_eq!(
            suggest_mapping("/home/n00b", "linux", "darwin", "vyn"),
            "/Users/vyn"
        );
        assert_eq!(
            suggest_mapping("/mnt/e/workspace/vyn", "linux", "darwin", "vyn"),
            "/Users/vyn/workspace/vyn"
        );
        assert_eq!(
            suggest_mapping("/Users/john/app", "darwin", "linux", "n00b"),
            "/home/n00b/app"
        );
    }

    #[test]
    fn detect_roots_groups_by_prefix() {
        let dirs = vec![
            "/mnt/e/workspace/vyn/a".to_string(),
            "/mnt/e/workspace/vyn/b".to_string(),
            "/mnt/e/workspace/Other/c".to_string(),
            "/home/n00b".to_string(),
        ];
        let roots = detect_project_roots(&dirs);
        let lookup: BTreeMap<_, _> = roots
            .iter()
            .map(|r| (r.path.clone(), r.session_count))
            .collect();
        assert_eq!(lookup.get("/mnt/e/workspace/vyn"), Some(&2));
        assert_eq!(lookup.get("/mnt/e/workspace/Other"), Some(&1));
        assert_eq!(lookup.get("/home/n00b"), Some(&1));
    }

    #[test]
    fn rewrite_jsonl_only_changes_cwd_lines() {
        let m = map(&[("/home/n00b", "/Users/vyn")]);
        let content = concat!(
            "{\"type\":\"mode\",\"mode\":\"normal\"}\n",
            "{\"type\":\"user\",\"cwd\":\"/home/n00b\",\"x\":1}\n",
            "{\"type\":\"assistant\",\"cwd\":\"/home/n00b/sub\"}\n"
        );
        let (out, stats) = rewrite_jsonl(content, &m, false);
        assert_eq!(stats.lines_total, 3);
        assert_eq!(stats.lines_changed, 2);
        assert!(out.contains("\"cwd\":\"/Users/vyn\""));
        assert!(out.contains("\"cwd\":\"/Users/vyn/sub\""));
        // untouched control line preserved verbatim
        assert!(out.contains("{\"type\":\"mode\",\"mode\":\"normal\"}"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn rewrite_jsonl_leaves_tool_paths_unless_opted_in() {
        let m = map(&[("/home/n00b", "/Users/vyn")]);
        let content = "{\"type\":\"user\",\"cwd\":\"/home/n00b\",\"file\":\"/home/n00b/x.txt\"}\n";
        let (out, _) = rewrite_jsonl(content, &m, false);
        assert!(out.contains("\"file\":\"/home/n00b/x.txt\""));
        let (out2, _) = rewrite_jsonl(content, &m, true);
        assert!(out2.contains("\"file\":\"/Users/vyn/x.txt\""));
    }
}
