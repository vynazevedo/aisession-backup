use ratatui::crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{ListItem, ListState};

use crate::grouping::{format_date, group_entries, DirGroup, Entry};
use crate::ui::{human_size, sanitize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickRow {
    Group(usize),
    Entry { group: usize, entry: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Empty,
    Partial,
    Full,
}

impl Mark {
    fn text(self) -> &'static str {
        match self {
            Mark::Empty => "[ ]",
            Mark::Partial => "[-]",
            Mark::Full => "[x]",
        }
    }
}

pub struct GroupPicker {
    pub entries: Vec<Entry>,
    pub selected: Vec<bool>,
    pub groups: Vec<DirGroup>,
    pub rows: Vec<PickRow>,
    pub state: ListState,
    pub filter: String,
    pub filtering: bool,
    expanded: Vec<bool>,
    opened_by_filter: Vec<bool>,
    visible: Vec<Vec<usize>>,
}

impl GroupPicker {
    pub fn new(entries: Vec<Entry>, mut selected: Vec<bool>) -> Self {
        selected.resize(entries.len(), false);
        let groups = group_entries(&entries);
        let count = groups.len();
        let mut picker = Self {
            entries,
            selected,
            groups,
            rows: Vec::new(),
            state: ListState::default(),
            filter: String::new(),
            filtering: false,
            expanded: vec![false; count],
            opened_by_filter: vec![false; count],
            visible: vec![Vec::new(); count],
        };
        picker.apply_filter();
        picker
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&flag| flag).count()
    }

    pub fn current(&self) -> Option<PickRow> {
        self.state
            .selected()
            .and_then(|index| self.rows.get(index))
            .copied()
    }

    pub fn is_open(&self, group: usize) -> bool {
        self.expanded[group] || self.opened_by_filter[group]
    }

    pub fn mark(&self, group: usize) -> Mark {
        let members = &self.groups[group].members;
        let chosen = members.iter().filter(|&&m| self.selected[m]).count();
        if chosen == 0 {
            Mark::Empty
        } else if chosen == members.len() {
            Mark::Full
        } else {
            Mark::Partial
        }
    }

    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        for (index, group) in self.groups.iter().enumerate() {
            let cwd_matches = group
                .cwd
                .as_deref()
                .is_some_and(|cwd| cwd.to_lowercase().contains(&needle));
            if needle.is_empty() || cwd_matches {
                self.visible[index] = group.members.clone();
                self.opened_by_filter[index] = false;
                continue;
            }
            self.visible[index] = group
                .members
                .iter()
                .copied()
                .filter(|&member| {
                    let entry = &self.entries[member];
                    entry.label.to_lowercase().contains(&needle)
                        || entry.id.to_lowercase().contains(&needle)
                })
                .collect();
            self.opened_by_filter[index] = !self.visible[index].is_empty();
        }
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        let current = self.current();
        self.rows.clear();
        for group in 0..self.groups.len() {
            if self.visible[group].is_empty() {
                continue;
            }
            self.rows.push(PickRow::Group(group));
            if self.is_open(group) {
                for &entry in &self.visible[group] {
                    self.rows.push(PickRow::Entry { group, entry });
                }
            }
        }
        let position = current
            .and_then(|row| self.rows.iter().position(|candidate| *candidate == row))
            .or(if self.rows.is_empty() { None } else { Some(0) });
        self.state.select(position);
    }

    fn select_row(&mut self, row: PickRow) {
        if let Some(position) = self.rows.iter().position(|candidate| *candidate == row) {
            self.state.select(Some(position));
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let current = self.state.selected().unwrap_or(0) as isize;
        self.state
            .select(Some((current + delta).rem_euclid(len) as usize));
    }

    pub fn toggle_current(&mut self) {
        match self.current() {
            Some(PickRow::Group(group)) => {
                let members = self.visible[group].clone();
                let all = members.iter().all(|&member| self.selected[member]);
                for member in members {
                    self.selected[member] = !all;
                }
            }
            Some(PickRow::Entry { entry, .. }) => {
                self.selected[entry] = !self.selected[entry];
            }
            None => {}
        }
    }

    pub fn set_visible(&mut self, value: bool) {
        for members in &self.visible {
            for &member in members {
                self.selected[member] = value;
            }
        }
    }

    fn close(&mut self, group: usize) {
        self.expanded[group] = false;
        self.opened_by_filter[group] = false;
        self.rebuild_rows();
        self.select_row(PickRow::Group(group));
    }

    pub fn expand_current(&mut self) {
        if let Some(PickRow::Group(group)) = self.current() {
            self.expanded[group] = true;
            self.rebuild_rows();
        }
    }

    pub fn collapse_current(&mut self) -> bool {
        match self.current() {
            Some(PickRow::Entry { group, .. }) => {
                self.close(group);
                true
            }
            Some(PickRow::Group(group)) if self.is_open(group) => {
                self.close(group);
                true
            }
            _ => false,
        }
    }

    fn toggle_open_current(&mut self) {
        match self.current() {
            Some(PickRow::Group(group)) if !self.is_open(group) => self.expand_current(),
            _ => {
                self.collapse_current();
            }
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> bool {
        if self.filtering {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.apply_filter();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.apply_filter();
                }
                _ => {}
            }
            return true;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Char('a') => self.set_visible(true),
            KeyCode::Char('n') => self.set_visible(false),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Enter => self.toggle_open_current(),
            KeyCode::Right | KeyCode::Char('l') => self.expand_current(),
            KeyCode::Left | KeyCode::Char('h') => return self.collapse_current(),
            _ => return false,
        }
        true
    }

    pub fn filter_suffix(&self) -> String {
        if self.filtering {
            format!(" | filter: {}_", sanitize(&self.filter))
        } else if !self.filter.is_empty() {
            format!(" | filter: {}", sanitize(&self.filter))
        } else {
            String::new()
        }
    }

    pub fn row_text(&self, row: PickRow) -> String {
        match row {
            PickRow::Group(index) => {
                let group = &self.groups[index];
                let chosen = group
                    .members
                    .iter()
                    .filter(|&&member| self.selected[member])
                    .count();
                format!(
                    "{} {} {}  {chosen}/{} | {} | {} | {}",
                    self.mark(index).text(),
                    if self.is_open(index) { "v" } else { ">" },
                    group.title(),
                    group.members.len(),
                    human_size(group.size),
                    format_date(group.latest),
                    group.provider_summary()
                )
            }
            PickRow::Entry { entry, .. } => {
                let item = &self.entries[entry];
                let check = if self.selected[entry] { "[x]" } else { "[ ]" };
                let label = if item.label.is_empty() {
                    sanitize(&item.id)
                } else {
                    sanitize(&item.label)
                };
                format!(
                    "    {check} {label}  ({} | {} | {})",
                    sanitize(&item.provider),
                    human_size(item.size),
                    format_date(item.updated_at)
                )
            }
        }
    }

    pub fn list_items(&self) -> Vec<ListItem<'static>> {
        self.rows
            .iter()
            .map(|&row| {
                let style = match row {
                    PickRow::Group(_) => Style::default().add_modifier(Modifier::BOLD),
                    PickRow::Entry { .. } => Style::default(),
                };
                ListItem::new(self.row_text(row)).style(style)
            })
            .collect()
    }
}

pub const PICKER_HELP: &str = "space toggle | enter/l open | h close | a all | n none | / filter";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn entry(id: &str, cwd: Option<&str>, label: &str, day: u32) -> Entry {
        Entry {
            id: id.into(),
            label: label.into(),
            cwd: cwd.map(str::to_string),
            provider: "claude".into(),
            size: 5,
            updated_at: Some(Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap()),
            ..Entry::default()
        }
    }

    fn picker() -> GroupPicker {
        GroupPicker::new(
            vec![
                entry("a1", Some("/app"), "fix login", 3),
                entry("a2", Some("/app"), "add tests", 2),
                entry("b1", Some("/api"), "fix cache", 1),
                entry("cfg", None, "global settings", 9),
            ],
            Vec::new(),
        )
    }

    #[test]
    fn starts_collapsed_with_one_row_per_directory() {
        let picker = picker();
        assert_eq!(
            picker.rows,
            vec![PickRow::Group(0), PickRow::Group(1), PickRow::Group(2)]
        );
        assert_eq!(picker.groups[0].cwd.as_deref(), Some("/app"));
        assert_eq!(picker.groups[2].cwd, None);
    }

    #[test]
    fn space_on_a_group_selects_every_session_and_shows_partial_state() {
        let mut picker = picker();
        assert_eq!(picker.mark(0), Mark::Empty);
        picker.handle_key(KeyCode::Char(' '));
        assert_eq!(picker.selected, vec![true, true, false, false]);
        assert_eq!(picker.mark(0), Mark::Full);

        picker.handle_key(KeyCode::Enter);
        picker.handle_key(KeyCode::Down);
        assert_eq!(
            picker.current(),
            Some(PickRow::Entry { group: 0, entry: 0 })
        );
        picker.handle_key(KeyCode::Char(' '));
        assert_eq!(picker.mark(0), Mark::Partial);
        assert!(picker.row_text(PickRow::Group(0)).starts_with("[-]"));

        picker.handle_key(KeyCode::Up);
        picker.handle_key(KeyCode::Char(' '));
        assert_eq!(picker.selected, vec![true, true, false, false]);
        picker.handle_key(KeyCode::Char(' '));
        assert_eq!(picker.selected, vec![false, false, false, false]);
    }

    #[test]
    fn enter_opens_and_closes_and_the_cursor_returns_to_the_group() {
        let mut picker = picker();
        picker.handle_key(KeyCode::Enter);
        assert_eq!(picker.rows.len(), 5);
        picker.handle_key(KeyCode::Down);
        picker.handle_key(KeyCode::Enter);
        assert_eq!(picker.rows.len(), 3);
        assert_eq!(picker.current(), Some(PickRow::Group(0)));
        assert!(!picker.handle_key(KeyCode::Left));
    }

    #[test]
    fn filtering_by_label_opens_groups_and_a_group_toggle_only_touches_matches() {
        let mut picker = picker();
        picker.handle_key(KeyCode::Char('/'));
        for c in "fix".chars() {
            picker.handle_key(KeyCode::Char(c));
        }
        picker.handle_key(KeyCode::Enter);
        assert_eq!(
            picker.rows,
            vec![
                PickRow::Group(0),
                PickRow::Entry { group: 0, entry: 0 },
                PickRow::Group(1),
                PickRow::Entry { group: 1, entry: 2 },
            ]
        );
        picker.handle_key(KeyCode::Char(' '));
        assert_eq!(picker.selected, vec![true, false, false, false]);
        assert_eq!(picker.mark(0), Mark::Partial);
        picker.handle_key(KeyCode::Char('a'));
        assert_eq!(picker.selected, vec![true, false, true, false]);
    }

    #[test]
    fn filtering_by_directory_keeps_the_group_closed_with_all_members() {
        let mut picker = picker();
        picker.handle_key(KeyCode::Char('/'));
        picker.handle_key(KeyCode::Char('a'));
        picker.handle_key(KeyCode::Char('p'));
        picker.handle_key(KeyCode::Char('p'));
        picker.handle_key(KeyCode::Esc);
        assert_eq!(picker.rows, vec![PickRow::Group(0)]);
        picker.handle_key(KeyCode::Char(' '));
        assert_eq!(picker.selected, vec![true, true, false, false]);
    }

    #[test]
    fn empty_lists_and_filters_without_matches_do_not_panic() {
        let mut empty = GroupPicker::new(Vec::new(), Vec::new());
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char(' '),
            KeyCode::Enter,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('a'),
        ] {
            empty.handle_key(code);
        }
        assert_eq!(empty.selected_count(), 0);

        let mut picker = picker();
        picker.handle_key(KeyCode::Char('/'));
        picker.handle_key(KeyCode::Char('z'));
        assert!(picker.rows.is_empty());
        assert_eq!(picker.current(), None);
        picker.handle_key(KeyCode::Esc);
        picker.handle_key(KeyCode::Char(' '));
        picker.handle_key(KeyCode::Char('a'));
        assert_eq!(picker.selected_count(), 0);
    }

    #[test]
    fn rendered_rows_are_sanitized() {
        let picker = GroupPicker::new(
            vec![entry(
                "x\u{1b}[31m",
                Some("/w\u{1b}[2J"),
                "l\u{1b}]0;x\u{7}",
                1,
            )],
            Vec::new(),
        );
        let group = picker.row_text(PickRow::Group(0));
        let row = picker.row_text(PickRow::Entry { group: 0, entry: 0 });
        assert!(!group.chars().chain(row.chars()).any(char::is_control));
    }
}
