use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{FileClass, ItemFile};
use crate::remap::apply_longest_mapping;
use crate::safety::validate_rel;

const MAX_HEAD_LINE_BYTES: u64 = 16 * 1024 * 1024;

pub type Mappings = BTreeMap<String, String>;

pub fn is_noise(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.ends_with(".lock")
        || name.ends_with(":Zone.Identifier")
        || name == ".DS_Store"
        || name == "Thumbs.db"
}

#[derive(Default)]
pub struct WalkOutcome {
    pub files: Vec<ItemFile>,
    pub skipped: usize,
}

pub fn walk(home: &Path, sub: &str, class: impl Fn(&str) -> FileClass) -> WalkOutcome {
    let mut outcome = WalkOutcome::default();
    let root = home.join(sub);
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                if root.exists() {
                    outcome.skipped += 1;
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            if entry.file_type().is_symlink() {
                outcome.skipped += 1;
            }
            continue;
        }
        let Some(rel) = rel_of(home, entry.path()) else {
            outcome.skipped += 1;
            continue;
        };
        if is_noise(&rel) {
            continue;
        }
        if validate_rel(&rel).is_err() {
            outcome.skipped += 1;
            continue;
        }
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        let class = class(&rel);
        outcome.files.push(ItemFile { rel, size, class });
    }
    outcome.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    outcome
}

pub fn single(home: &Path, rel: &str, class: FileClass) -> Option<ItemFile> {
    let meta = std::fs::symlink_metadata(home.join(rel)).ok()?;
    meta.file_type().is_file().then(|| ItemFile {
        rel: rel.to_string(),
        size: meta.len(),
        class,
    })
}

pub fn singles(home: &Path, entries: &[(&str, FileClass)]) -> Vec<ItemFile> {
    entries
        .iter()
        .filter_map(|(rel, class)| single(home, rel, *class))
        .collect()
}

pub fn rel_of(home: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(home).ok()?;
    let parts: Option<Vec<&str>> = rel
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect();
    Some(parts?.join("/"))
}

pub fn subdirs(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    out.sort();
    out
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.into())
}

pub fn last_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL).read_to_end(&mut bytes).ok()?;
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines.iter().rev().find_map(|line| {
        let value: Value = serde_json::from_slice(line).ok()?;
        value.get("timestamp")?.as_str()?.parse().ok()
    })
}

pub fn newest_mtime(home: &Path, files: &[ItemFile]) -> Option<DateTime<Utc>> {
    files
        .iter()
        .filter_map(|file| mtime(&home.join(&file.rel)))
        .max()
}

pub fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn read_first_json_line(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file).take(MAX_HEAD_LINE_BYTES);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    serde_json::from_slice(&line).ok()
}

pub fn map_string(value: &mut Value, mappings: &Mappings) -> usize {
    let Value::String(text) = value else {
        return 0;
    };
    let mapped = apply_longest_mapping(text, mappings);
    if mapped == *text {
        return 0;
    }
    *text = mapped;
    1
}

pub fn map_at(value: &mut Value, path: &[&str], mappings: &Mappings) -> usize {
    let mut current = value;
    for key in path {
        match current.get_mut(*key) {
            Some(next) => current = next,
            None => return 0,
        }
    }
    map_string(current, mappings)
}

pub fn map_array_at(value: &mut Value, path: &[&str], mappings: &Mappings) -> usize {
    let mut current = value;
    for key in path {
        match current.get_mut(*key) {
            Some(next) => current = next,
            None => return 0,
        }
    }
    match current.as_array_mut() {
        Some(items) => items
            .iter_mut()
            .map(|item| map_string(item, mappings))
            .sum(),
        None => 0,
    }
}

pub fn map_keys_at(value: &mut Value, path: &[&str], mappings: &Mappings) -> usize {
    let mut current = value;
    for key in path {
        match current.get_mut(*key) {
            Some(next) => current = next,
            None => return 0,
        }
    }
    let Some(object) = current.as_object_mut() else {
        return 0;
    };
    let mut changes = 0;
    let mut remapped = serde_json::Map::with_capacity(object.len());
    for (key, entry) in std::mem::take(object) {
        let mapped = apply_longest_mapping(&key, mappings);
        if mapped != key {
            changes += 1;
        }
        remapped.insert(mapped, entry);
    }
    *object = remapped;
    changes
}

pub fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

pub fn keys_collide(value: &Value, path: &[&str], mappings: &Mappings) -> bool {
    let mut current = value;
    for key in path {
        match current.get(*key) {
            Some(next) => current = next,
            None => return false,
        }
    }
    let Some(object) = current.as_object() else {
        return false;
    };
    let mut seen = std::collections::BTreeSet::new();
    object
        .keys()
        .any(|key| !seen.insert(apply_longest_mapping(key, mappings)))
}

pub fn rewrite_jsonl(
    bytes: &[u8],
    mut rewrite: impl FnMut(&mut Value) -> usize,
) -> (Option<Vec<u8>>, usize) {
    let mut out = Vec::with_capacity(bytes.len());
    let mut changes = 0;
    for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
        let (line, ending) = match chunk.strip_suffix(b"\n") {
            Some(line) => (line, &b"\n"[..]),
            None => (chunk, &b""[..]),
        };
        let mut rewritten = None;
        if let Ok(mut value) = serde_json::from_slice::<Value>(line) {
            let line_changes = rewrite(&mut value);
            if line_changes > 0 {
                if let Ok(serialized) = serde_json::to_vec(&value) {
                    changes += line_changes;
                    rewritten = Some(serialized);
                }
            }
        }
        match rewritten {
            Some(serialized) => out.extend_from_slice(&serialized),
            None => out.extend_from_slice(line),
        }
        out.extend_from_slice(ending);
    }
    if changes == 0 {
        (None, 0)
    } else {
        (Some(out), changes)
    }
}

pub fn rewrite_json(
    bytes: &[u8],
    rewrite: impl FnOnce(&mut Value) -> usize,
) -> Result<(Option<Vec<u8>>, usize)> {
    let mut value: Value = serde_json::from_slice(bytes).context("parsing JSON")?;
    let changes = rewrite(&mut value);
    if changes == 0 {
        return Ok((None, 0));
    }
    Ok((Some(serde_json::to_vec_pretty(&value)?), changes))
}

pub fn process_running(names: &[&str]) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let own_pid = std::process::id().to_string();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name();
        let pid = pid.to_string_lossy();
        if pid == own_pid || !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if let Some(name) = names.iter().find(|name| comm.trim() == **name) {
            return Some(format!("process '{name}' is running (pid {pid})"));
        }
    }
    None
}

pub fn home_child(env: &super::LocateEnv, child: &str) -> Option<PathBuf> {
    let path = env.user_home.as_ref()?.join(child);
    path.is_dir().then_some(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn last_timestamp_reads_the_final_line_even_in_large_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let mut body = String::new();
        body.push_str("{\"timestamp\":\"2026-01-01T00:00:00Z\",\"pad\":\"");
        body.push_str(&"x".repeat(200_000));
        body.push_str("\"}\n");
        body.push_str("{\"timestamp\":\"2026-03-04T05:06:07Z\"}\n");
        body.push_str("not json\n");
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            super::last_timestamp(&path).unwrap().to_rfc3339(),
            "2026-03-04T05:06:07+00:00"
        );
        std::fs::write(&path, "{}\n").unwrap();
        assert!(super::last_timestamp(&path).is_none());
    }

    use super::*;
    use serde_json::json;

    fn mappings() -> Mappings {
        [("/old".to_string(), "/new".to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn jsonl_keeps_unchanged_lines_byte_exact() {
        let input = b"{ \"a\" : 1 }\n{\"cwd\":\"/old/x\"}\nnot json\n{\"cwd\":\"/other\"}";
        let (out, changes) = rewrite_jsonl(input, |value| map_at(value, &["cwd"], &mappings()));
        let out = String::from_utf8(out.unwrap()).unwrap();
        assert_eq!(changes, 1);
        assert_eq!(
            out,
            "{ \"a\" : 1 }\n{\"cwd\":\"/new/x\"}\nnot json\n{\"cwd\":\"/other\"}"
        );
    }

    #[test]
    fn jsonl_without_changes_returns_none() {
        let (out, changes) = rewrite_jsonl(b"{\"cwd\":\"/other\"}\n", |value| {
            map_at(value, &["cwd"], &mappings())
        });
        assert!(out.is_none());
        assert_eq!(changes, 0);
    }

    #[test]
    fn map_keys_preserves_order_and_values() {
        let mut value = json!({"p": {"/old/a": 1, "/z": 2}});
        assert_eq!(map_keys_at(&mut value, &["p"], &mappings()), 1);
        assert_eq!(value["p"]["/new/a"], 1);
        assert_eq!(value["p"]["/z"], 2);
    }

    #[test]
    fn key_collision_is_detected() {
        let value = json!({"/old/a": "TRUST", "/new/a": "BLOCK"});
        assert!(keys_collide(&value, &[], &mappings()));
        assert!(!keys_collide(
            &json!({"/old/a": 1, "/z": 2}),
            &[],
            &mappings()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_a_symlinked_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("d")).unwrap();
        let outcome = walk(tmp.path(), "d", |_| FileClass::Plain);
        assert!(outcome.files.is_empty());
    }

    #[test]
    fn noise_filter() {
        assert!(is_noise("a/b.lock"));
        assert!(is_noise("settings.json:Zone.Identifier"));
        assert!(!is_noise("a/lock.json"));
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d/ok"), b"y").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("d/link")).unwrap();
        let outcome = walk(tmp.path(), "d", |_| FileClass::Plain);
        let rels: Vec<_> = outcome.files.iter().map(|file| file.rel.as_str()).collect();
        assert_eq!(rels, vec!["d/ok"]);
        assert_eq!(outcome.skipped, 1);
    }
}
