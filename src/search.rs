use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use regex::{Regex, RegexBuilder};
use serde::Serialize;

use crate::convert::{Part, Role};
use crate::provider::{ItemKind, LocateEnv, ProviderId, ScanOptions};
use crate::ui::sanitize;

pub const SEARCHABLE: [ProviderId; 2] = [ProviderId::Claude, ProviderId::Codex];
const SNIPPET_CONTEXT: usize = 70;

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub provider: ProviderId,
    pub id: String,
    pub label: String,
    pub cwd: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub matches: usize,
    pub role: Role,
    pub snippet: String,
    pub path: PathBuf,
}

pub struct Query {
    terms: Vec<Regex>,
    raw_filters: Vec<regex::bytes::Regex>,
}

impl Query {
    pub fn parse(raw: &str) -> Result<Self> {
        let words: Vec<&str> = raw.split_whitespace().collect();
        if words.is_empty() {
            bail!("the search query is empty");
        }
        if words.len() > 16 {
            bail!("use at most 16 search terms");
        }
        let terms = words
            .iter()
            .map(|word| {
                RegexBuilder::new(&regex::escape(word))
                    .case_insensitive(true)
                    .size_limit(1 << 20)
                    .build()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let raw_filters = words
            .iter()
            .filter(|word| {
                !word
                    .chars()
                    .any(|c| c == '"' || c == '\\' || c.is_control())
            })
            .map(|word| {
                regex::bytes::RegexBuilder::new(&regex::escape(word))
                    .case_insensitive(true)
                    .size_limit(1 << 20)
                    .build()
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { terms, raw_filters })
    }

    fn first_term(&self) -> &Regex {
        &self.terms[0]
    }
}

fn part_text(part: &Part) -> &str {
    match part {
        Part::Text(text) => text,
        Part::ToolCall { input, .. } => input,
        Part::ToolResult { output, .. } => output,
    }
}

fn snippet(text: &str, start: usize, end: usize) -> String {
    let before: String = text[..start]
        .chars()
        .rev()
        .take(SNIPPET_CONTEXT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = text[end..].chars().take(SNIPPET_CONTEXT).collect();
    let clipped_start = before.chars().count() == SNIPPET_CONTEXT && start > before.len();
    let clipped_end = text.len() > end + after.len();
    let raw = format!(
        "{}{before}{}{after}{}",
        if clipped_start { "\u{2026}" } else { "" },
        &text[start..end],
        if clipped_end { "\u{2026}" } else { "" }
    );
    sanitize(&raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn search_file(
    provider: ProviderId,
    path: &Path,
    query: &Query,
) -> Result<Option<(usize, Role, String)>> {
    let size = std::fs::metadata(path)?.len();
    if size > crate::convert::MAX_SOURCE_BYTES {
        return Ok(None);
    }
    crate::archive::ensure_regular(path)?;
    let bytes = std::fs::read(path)?;
    if !query
        .raw_filters
        .iter()
        .all(|filter| filter.is_match(&bytes))
    {
        return Ok(None);
    }
    let Ok(transcript) = crate::convert::read(provider, &bytes) else {
        return Ok(None);
    };
    let mut present = vec![false; query.terms.len()];
    let mut matches = 0usize;
    let mut best: Option<(Role, String)> = None;
    for turn in &transcript.turns {
        for part in &turn.parts {
            let text = part_text(part);
            for (index, term) in query.terms.iter().enumerate() {
                if !present[index] && term.is_match(text) {
                    present[index] = true;
                }
            }
            let mut found = query.first_term().find_iter(text).peekable();
            if let Some(first) = found.peek() {
                if best.is_none() {
                    best = Some((turn.role, snippet(text, first.start(), first.end())));
                }
            }
            matches += found.count();
        }
    }
    if !present.iter().all(|&hit| hit) {
        return Ok(None);
    }
    Ok(best.map(|(role, text)| (matches, role, text)))
}

pub fn search(
    raw_query: &str,
    env: &LocateEnv,
    providers: &[ProviderId],
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let query = Query::parse(raw_query)?;
    let mut candidates = Vec::new();
    for prov in crate::provider::registry() {
        let id = prov.id();
        if !providers.contains(&id) || !SEARCHABLE.contains(&id) {
            continue;
        }
        let Some(home) = prov.locate(env).filter(|home| home.exists()) else {
            continue;
        };
        let inventory = prov.scan(&home, &ScanOptions::default())?;
        for item in inventory.items {
            if item.kind != ItemKind::Session {
                continue;
            }
            let Some(file) = item.files.iter().find(|file| {
                file.rel.ends_with(".jsonl")
                    && (id == ProviderId::Codex
                        || file.rel.ends_with(&format!("{}.jsonl", item.id)))
            }) else {
                continue;
            };
            candidates.push((id, home.join(&file.rel), item));
        }
    }
    let results = crate::discover::parallel_map(&candidates, |(provider, path, item)| {
        search_file(*provider, path, &query)
            .ok()
            .flatten()
            .map(|(matches, role, snippet)| SearchHit {
                provider: *provider,
                id: item.id.clone(),
                label: item.label.clone(),
                cwd: item.cwd.clone(),
                updated_at: item.updated_at,
                matches,
                role,
                snippet,
                path: path.clone(),
            })
    });
    let mut hits: Vec<SearchHit> = results.into_iter().flatten().collect();
    hits.sort_by(|a, b| {
        b.matches
            .cmp(&a.matches)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    Ok(hits)
}

pub fn format_hits(hits: &[SearchHit], color: bool) -> String {
    use owo_colors::OwoColorize;
    if hits.is_empty() {
        return "No session matches the query.\n".to_string();
    }
    let mut out = String::new();
    for hit in hits {
        let date = hit
            .updated_at
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "-".into());
        let cwd = hit
            .cwd
            .as_deref()
            .map(sanitize)
            .unwrap_or_else(|| "-".into());
        let title = sanitize(hit.label.lines().next().unwrap_or_default());
        let head = format!(
            "{}  {}  {}  {}",
            hit.provider.as_str(),
            date,
            cwd,
            if hit.matches == 1 {
                "1 match".to_string()
            } else {
                format!("{} matches", hit.matches)
            }
        );
        if color {
            out.push_str(&format!("{}\n", head.bold()));
        } else {
            out.push_str(&head);
            out.push('\n');
        }
        out.push_str(&format!("  {}  {}\n", sanitize(&hit.id), title));
        out.push_str(&format!("  {}\n\n", hit.snippet));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn queries_need_words_and_are_literal() {
        assert!(Query::parse("   ").is_err());
        let query = Query::parse("a.b (c").unwrap();
        assert!(query.terms[0].is_match("A.B"));
        assert!(!query.terms[0].is_match("axb"));
        assert!(Query::parse(&"w ".repeat(17)).is_err());
    }

    #[test]
    fn every_term_must_appear_and_snippets_keep_original_case() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        write(
            &path,
            &[
                json!({"type":"user","sessionId":"s","cwd":"/w","message":{"role":"user","content":"Please fix the Redis cache timeout in production"}}),
                json!({"type":"assistant","sessionId":"s","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"The redis client now retries."}]}}),
                json!({"type":"user","sessionId":"s","message":{"role":"user","content":"<system-reminder>redis redis redis</system-reminder>"}}),
            ],
        );
        let (matches, role, snippet) = search_file(
            ProviderId::Claude,
            &path,
            &Query::parse("redis timeout").unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(matches, 2);
        assert_eq!(role, Role::User);
        assert!(snippet.contains("Redis cache timeout"));
        assert!(search_file(
            ProviderId::Claude,
            &path,
            &Query::parse("redis kafka").unwrap()
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn snippets_are_clipped_on_char_boundaries_and_sanitized() {
        let text = format!("{}needle\u{1b}[31m{}", "é".repeat(200), "ü".repeat(200));
        let start = text.find("needle").unwrap();
        let shown = snippet(&text, start, start + 6);
        assert!(shown.starts_with('\u{2026}'));
        assert!(shown.ends_with('\u{2026}'));
        assert!(shown.contains("needle"));
        assert!(!shown.chars().any(char::is_control));
    }

    #[test]
    fn raw_prefilter_never_hides_escaped_or_unicode_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        write(
            &path,
            &[
                json!({"type":"user","sessionId":"s","cwd":"/w","message":{"role":"user","content":"Ação \"quoted\" path C:\\dir é ÉCOLE"}}),
            ],
        );
        for query in ["ação", "\"quoted\"", "C:\\dir", "école"] {
            assert!(
                search_file(ProviderId::Claude, &path, &Query::parse(query).unwrap())
                    .unwrap()
                    .is_some(),
                "{query}"
            );
        }
    }

    #[test]
    fn unreadable_or_foreign_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.jsonl");
        std::fs::write(&path, b"not json\n").unwrap();
        assert!(
            search_file(ProviderId::Claude, &path, &Query::parse("x").unwrap())
                .unwrap()
                .is_none()
        );
    }
}
