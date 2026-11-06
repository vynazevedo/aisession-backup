use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use owo_colors::OwoColorize;

use crate::ui::{human_size, sanitize};

pub const NO_DIRECTORY: &str = "(no directory)";

#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub id: String,
    pub label: String,
    pub cwd: Option<String>,
    pub provider: String,
    pub detail: String,
    pub size: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct DirGroup {
    pub cwd: Option<String>,
    pub members: Vec<usize>,
    pub size: u64,
    pub latest: Option<DateTime<Utc>>,
    pub providers: BTreeMap<String, usize>,
}

impl DirGroup {
    pub fn title(&self) -> String {
        self.cwd
            .as_deref()
            .map(sanitize)
            .unwrap_or_else(|| NO_DIRECTORY.to_string())
    }

    pub fn provider_summary(&self) -> String {
        self.providers
            .iter()
            .map(|(name, count)| format!("{} {count}", sanitize(name)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn group_entries(entries: &[Entry]) -> Vec<DirGroup> {
    let mut by_cwd: BTreeMap<Option<&str>, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let cwd = entry.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty());
        by_cwd.entry(cwd).or_default().push(index);
    }
    let mut groups: Vec<DirGroup> = by_cwd
        .into_iter()
        .map(|(cwd, mut members)| {
            members.sort_by(|&a, &b| {
                entries[b]
                    .updated_at
                    .cmp(&entries[a].updated_at)
                    .then_with(|| entries[a].id.cmp(&entries[b].id))
            });
            let mut providers = BTreeMap::new();
            for &member in &members {
                *providers
                    .entry(entries[member].provider.clone())
                    .or_insert(0) += 1;
            }
            DirGroup {
                cwd: cwd.map(str::to_string),
                size: members
                    .iter()
                    .fold(0u64, |total, &m| total.saturating_add(entries[m].size)),
                latest: members.iter().filter_map(|&m| entries[m].updated_at).max(),
                providers,
                members,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        a.cwd
            .is_none()
            .cmp(&b.cwd.is_none())
            .then_with(|| b.latest.cmp(&a.latest))
            .then_with(|| a.cwd.cmp(&b.cwd))
    });
    groups
}

pub fn format_date(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn group_line(group: &DirGroup) -> String {
    format!(
        "{} | {} item(s) | {} | last {} | {}",
        group.title(),
        group.members.len(),
        human_size(group.size),
        format_date(group.latest),
        group.provider_summary()
    )
}

fn entry_line(entry: &Entry) -> String {
    let mut parts = vec![
        sanitize(&entry.provider),
        sanitize(&entry.id),
        human_size(entry.size),
        format_date(entry.updated_at),
    ];
    if !entry.detail.is_empty() {
        parts.push(sanitize(&entry.detail));
    }
    if !entry.label.is_empty() {
        parts.push(sanitize(&entry.label));
    }
    parts.join(" | ")
}

pub fn format_groups(entries: &[Entry], color: bool) -> String {
    let groups = group_entries(entries);
    let mut out = String::new();
    for (position, group) in groups.iter().enumerate() {
        if position > 0 {
            out.push('\n');
        }
        let line = group_line(group);
        if color {
            out.push_str(&format!("{}\n", line.bold()));
        } else {
            out.push_str(&line);
            out.push('\n');
        }
        for &member in &group.members {
            out.push_str("  ");
            out.push_str(&entry_line(&entries[member]));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry(id: &str, cwd: Option<&str>, provider: &str, day: Option<u32>) -> Entry {
        Entry {
            id: id.into(),
            label: format!("label {id}"),
            cwd: cwd.map(str::to_string),
            provider: provider.into(),
            size: 10,
            updated_at: day.map(|d| Utc.with_ymd_and_hms(2026, 1, d, 0, 0, 0).unwrap()),
            ..Entry::default()
        }
    }

    #[test]
    fn groups_by_exact_cwd_and_sorts_by_recent_activity() {
        let entries = vec![
            entry("old", Some("/work/old"), "claude", Some(1)),
            entry("a", Some("/work/app"), "claude", Some(2)),
            entry("b", Some("/work/app"), "codex", Some(5)),
            entry("sub", Some("/work/app/sub"), "claude", Some(3)),
            entry("cfg", None, "codex", Some(9)),
            entry("blank", Some("  "), "grok", None),
        ];
        let groups = group_entries(&entries);
        let titles: Vec<String> = groups.iter().map(DirGroup::title).collect();
        assert_eq!(
            titles,
            vec!["/work/app", "/work/app/sub", "/work/old", NO_DIRECTORY]
        );
        assert_eq!(groups[0].members, vec![2, 1]);
        assert_eq!(groups[0].size, 20);
        assert_eq!(groups[0].provider_summary(), "claude 1, codex 1");
        assert_eq!(groups[3].members.len(), 2);
    }

    #[test]
    fn similar_directories_with_the_same_encoding_stay_apart() {
        let entries = vec![
            entry("under", Some("/work/a_b"), "claude", Some(1)),
            entry("dash", Some("/work/a-b"), "claude", Some(1)),
        ];
        assert_eq!(group_entries(&entries).len(), 2);
    }

    #[test]
    fn text_output_is_sanitized() {
        let mut hostile = entry("x\u{1b}[31m", Some("/w\u{1b}[2J"), "claude\u{7}", Some(1));
        hostile.label = "label\u{1b}]0;spoof\u{7}".into();
        hostile.detail = "detail\r".into();
        let text = format_groups(&[hostile], false);
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    }
}
