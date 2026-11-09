use ratatui::crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::theme::{split_with_details, Icon, Theme};

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
    order: Vec<usize>,
    depth: Vec<usize>,
    multi_provider: bool,
    short_ids: bool,
}

fn normalized(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

fn is_ancestor(parent: &str, child: &str) -> bool {
    let parent = normalized(parent);
    let child = normalized(child);
    child.len() > parent.len()
        && child.starts_with(&parent)
        && child[parent.len()..].starts_with('/')
}

fn tree_order(groups: &[DirGroup]) -> (Vec<usize>, Vec<usize>) {
    let parent: Vec<Option<usize>> = groups
        .iter()
        .enumerate()
        .map(|(own, group)| {
            let cwd = group.cwd.as_deref()?;
            groups
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != own)
                .filter_map(|(index, other)| {
                    let other_cwd = other.cwd.as_deref()?;
                    is_ancestor(other_cwd, cwd).then_some((index, normalized(other_cwd).len()))
                })
                .max_by_key(|(_, len)| *len)
                .map(|(index, _)| index)
        })
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); groups.len()];
    let mut roots = Vec::new();
    for (index, parent) in parent.iter().enumerate() {
        match parent {
            Some(parent) => children[*parent].push(index),
            None => roots.push(index),
        }
    }
    let mut newest: Vec<Option<chrono::DateTime<chrono::Utc>>> =
        groups.iter().map(|group| group.latest).collect();
    let mut changed = true;
    let mut rounds = 0;
    while changed && rounds < groups.len() {
        changed = false;
        rounds += 1;
        for (index, parent) in parent.iter().enumerate() {
            if let Some(parent) = parent {
                if newest[index] > newest[*parent] {
                    newest[*parent] = newest[index];
                    changed = true;
                }
            }
        }
    }
    let key = |index: &usize| {
        (
            groups[*index].cwd.is_none(),
            std::cmp::Reverse(newest[*index]),
            groups[*index].cwd.clone(),
        )
    };
    roots.sort_by_key(key);
    for list in &mut children {
        list.sort_by_key(key);
    }
    let mut order = Vec::with_capacity(groups.len());
    let mut depth = vec![0; groups.len()];
    let mut seen = vec![false; groups.len()];
    let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|root| (root, 0)).collect();
    while let Some((index, level)) = stack.pop() {
        if std::mem::replace(&mut seen[index], true) {
            continue;
        }
        order.push(index);
        depth[index] = level;
        for &child in children[index].iter().rev() {
            if !seen[child] {
                stack.push((child, level + 1));
            }
        }
    }
    for (index, visited) in seen.iter().enumerate() {
        if !visited {
            order.push(index);
        }
    }
    (order, depth)
}

impl GroupPicker {
    pub fn new(entries: Vec<Entry>, mut selected: Vec<bool>) -> Self {
        selected.resize(entries.len(), false);
        let groups = group_entries(&entries);
        let count = groups.len();
        let (order, depth) = tree_order(&groups);
        let multi_provider = entries
            .iter()
            .map(|entry| entry.provider.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
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
            order,
            depth,
            multi_provider,
            short_ids: false,
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
        for position in 0..self.order.len() {
            let group = self.order[position];
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
        if code == KeyCode::Esc && !self.filter.is_empty() {
            self.filter.clear();
            self.apply_filter();
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
            KeyCode::Left | KeyCode::Char('h') => {
                self.collapse_current();
            }
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

    fn mark_span(&self, mark: Mark, theme: &Theme) -> Span<'static> {
        let (icon, style) = match mark {
            Mark::Full => (Icon::Check, theme.success()),
            Mark::Partial => (Icon::Partial, theme.warning()),
            Mark::Empty => (Icon::Empty, theme.muted()),
        };
        Span::styled(theme.icon(icon).to_string(), style)
    }

    fn provider_spans(&self, group: &DirGroup, theme: &Theme) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (name, count) in &group.providers {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{} {count}", sanitize(name)),
                theme.provider(name),
            ));
        }
        spans
    }

    pub fn entry_label(&self, entry: usize) -> String {
        let item = &self.entries[entry];
        let same_as_cwd = item.cwd.as_deref() == Some(item.label.as_str());
        if item.label.trim().is_empty() || same_as_cwd {
            let short: String = item.id.chars().take(8).collect();
            format!("session {}", sanitize(&short))
        } else {
            sanitize(item.label.lines().next().unwrap_or_default())
        }
    }

    fn provider_column_width(&self) -> usize {
        if !self.multi_provider {
            return 0;
        }
        self.groups
            .iter()
            .map(|group| {
                group
                    .providers
                    .iter()
                    .map(|(name, count)| {
                        sanitize(name).chars().count() + count.to_string().len() + 2
                    })
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0)
    }

    pub fn matches(&self) -> usize {
        self.visible.iter().map(Vec::len).sum()
    }

    fn fit(text: String, width: usize, keep_tail: bool) -> String {
        let count = text.chars().count();
        if count <= width {
            return format!("{text:<width$}");
        }
        if keep_tail {
            return super::theme::truncate_left(&text, width);
        }
        if width == 0 {
            return String::new();
        }
        let head: String = text.chars().take(width - 1).collect();
        format!("{head}\u{2026}")
    }

    pub fn row_line(&self, row: PickRow, theme: &Theme) -> Line<'static> {
        self.row_line_width(row, theme, 100)
    }

    pub fn row_line_width(&self, row: PickRow, theme: &Theme, width: usize) -> Line<'static> {
        let right = 9 + 1 + 10;
        match row {
            PickRow::Group(index) => {
                let group = &self.groups[index];
                let chosen = group
                    .members
                    .iter()
                    .filter(|&&member| self.selected[member])
                    .count();
                let folder = if self.is_open(index) {
                    Icon::FolderOpen
                } else {
                    Icon::Folder
                };
                let indent = "  ".repeat(self.depth[index]);
                let count = format!("{:>7}", format!("{chosen}/{}", group.members.len()));
                let providers = if self.multi_provider {
                    self.provider_spans(group, theme)
                } else {
                    Vec::new()
                };
                let provider_width = self.provider_column_width();
                let fixed = indent.len() + 4 + 1 + count.len() + 1 + right + provider_width;
                let name_width = width.saturating_sub(fixed).max(8);
                let mut spans = vec![
                    Span::raw(indent),
                    self.mark_span(self.mark(index), theme),
                    Span::raw(" "),
                    Span::styled(theme.icon(folder).to_string(), theme.accent()),
                    Span::raw(" "),
                    Span::styled(Self::fit(group.title(), name_width, true), theme.strong()),
                    Span::styled(format!(" {count}"), theme.muted()),
                    Span::styled(
                        format!(
                            " {:>9} {:>10}",
                            human_size(group.size),
                            format_date(group.latest)
                        ),
                        theme.muted(),
                    ),
                ];
                spans.extend(providers);
                Line::from(spans)
            }
            PickRow::Entry { group, entry } => {
                let item = &self.entries[entry];
                let mark = if self.selected[entry] {
                    Mark::Full
                } else {
                    Mark::Empty
                };
                let indent = "  ".repeat(self.depth[group] + 2);
                let provider = if self.multi_provider {
                    format!(" {}", sanitize(&item.provider))
                } else {
                    String::new()
                };
                let fixed = indent.len() + 2 + 8 + right + 1 + self.provider_column_width();
                let name_width = width.saturating_sub(fixed).max(8);
                Line::from(vec![
                    Span::raw(indent),
                    self.mark_span(mark, theme),
                    Span::raw(" "),
                    Span::raw(Self::fit(self.entry_label(entry), name_width, false)),
                    Span::styled(
                        if self.short_ids && !self.entry_label(entry).starts_with("session ") {
                            format!(" {:<7}", item.id.chars().take(7).collect::<String>())
                        } else {
                            " ".repeat(8)
                        },
                        theme.muted(),
                    ),
                    Span::styled(
                        format!(
                            " {:>9} {:>10}",
                            human_size(item.size),
                            format_date(item.updated_at)
                        ),
                        theme.muted(),
                    ),
                    Span::styled(provider, theme.provider(&item.provider)),
                ])
            }
        }
    }

    pub fn list_items(&self, theme: &Theme) -> Vec<ListItem<'static>> {
        self.list_items_width(theme, 100)
    }

    pub fn list_items_width(&self, theme: &Theme, width: usize) -> Vec<ListItem<'static>> {
        self.rows
            .iter()
            .map(|&row| ListItem::new(self.row_line_width(row, theme, width)))
            .collect()
    }

    fn field(theme: &Theme, name: &str, value: String) -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{name:<10}"), theme.muted()),
            Span::raw(value),
        ])
    }

    pub fn detail_lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        match self.current() {
            Some(PickRow::Group(index)) => {
                let group = &self.groups[index];
                let chosen = group
                    .members
                    .iter()
                    .filter(|&&member| self.selected[member])
                    .count();
                let mut providers =
                    vec![Span::styled(format!("{:<10}", "providers"), theme.muted())];
                providers.extend(self.provider_spans(group, theme));
                vec![
                    Line::from(Span::styled("Directory", theme.muted())),
                    Line::from(Span::styled(group.title(), theme.accent())),
                    Line::from(""),
                    Self::field(
                        theme,
                        "items",
                        format!("{chosen} of {} selected", group.members.len()),
                    ),
                    Line::from(providers),
                    Self::field(theme, "size", human_size(group.size)),
                    Self::field(theme, "last", format_date(group.latest)),
                ]
            }
            Some(PickRow::Entry { entry, .. }) => {
                let item = &self.entries[entry];
                let mut lines = vec![
                    Line::from(Span::styled(self.entry_label(entry), theme.accent())),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(format!("{:<10}", "provider"), theme.muted()),
                        Span::styled(sanitize(&item.provider), theme.provider(&item.provider)),
                    ]),
                    Self::field(theme, "id", sanitize(&item.id)),
                    Self::field(
                        theme,
                        "directory",
                        item.cwd
                            .as_deref()
                            .map(sanitize)
                            .unwrap_or_else(|| crate::grouping::NO_DIRECTORY.to_string()),
                    ),
                    Self::field(theme, "size", human_size(item.size)),
                    Self::field(theme, "updated", format_date(item.updated_at)),
                ];
                if !item.detail.is_empty() {
                    lines.push(Self::field(theme, "detail", sanitize(&item.detail)));
                }
                if !item.label.is_empty() && item.label.lines().count() > 1 {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        sanitize(&item.label),
                        theme.muted(),
                    )));
                }
                lines
            }
            None => vec![Line::from(Span::styled("Nothing selected.", theme.muted()))],
        }
    }

    fn empty_message(&self) -> String {
        if self.filter.is_empty() {
            "Nothing to show here.".to_string()
        } else {
            format!("No item matches \"{}\".", sanitize(&self.filter))
        }
    }

    pub fn esc_label(&self) -> &'static str {
        if self.filtering {
            "done"
        } else if !self.filter.is_empty() {
            "clear filter"
        } else {
            "back"
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, theme: &Theme, title: &str) {
        let show_filter = self.filtering || !self.filter.is_empty();
        let (filter_area, area) = if show_filter && area.height > 3 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(area);
            (Some(chunks[0]), chunks[1])
        } else {
            (None, area)
        };
        if let Some(filter_area) = filter_area {
            let mut spans = vec![
                Span::styled(" / ", theme.key()),
                Span::raw(" "),
                Span::styled(sanitize(&self.filter), theme.strong()),
            ];
            if self.filtering {
                spans.push(Span::styled("_", theme.accent()));
            }
            let matches = self.matches();
            let count = if matches == 1 {
                "1 match".to_string()
            } else {
                format!("{matches} matches")
            };
            spans.push(Span::styled(format!("   {count}"), theme.muted()));
            spans.push(Span::raw("   "));
            spans.extend(super::theme::chip_spans(
                theme,
                "esc",
                if self.filtering { "done" } else { "clear" },
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), filter_area);
        }
        let (list_area, detail_area) = split_with_details(area);
        self.short_ids = detail_area.is_none();
        let block = theme.panel(title);
        if self.rows.is_empty() {
            let inner = block.inner(list_area);
            f.render_widget(block, list_area);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    self.empty_message(),
                    theme.muted(),
                ))),
                Rect {
                    x: inner.x.saturating_add(1),
                    width: inner.width.saturating_sub(1),
                    height: inner.height.min(1),
                    ..inner
                },
            );
        } else {
            let width = usize::from(list_area.width.saturating_sub(4));
            let list = List::new(self.list_items_width(theme, width))
                .block(block)
                .highlight_style(theme.selected_row())
                .highlight_symbol("> ");
            f.render_stateful_widget(list, list_area, &mut self.state);
        }
        if let Some(detail_area) = detail_area {
            f.render_widget(
                Paragraph::new(self.detail_lines(theme))
                    .block(theme.panel("Details"))
                    .wrap(Wrap { trim: false }),
                detail_area,
            );
        }
    }
}

pub const PICKER_KEYS: [(&str, &str); 4] = [
    ("space", "select"),
    ("enter", "open/close"),
    ("/", "filter"),
    ("?", "help"),
];

pub const PICKER_HELP_KEYS: [(&str, &str); 9] = [
    ("space", "select or clear the directory or item"),
    ("enter", "open or close the directory"),
    ("l / right", "open the directory"),
    ("h / left", "close the directory"),
    ("j/k", "move"),
    ("a / n", "select or clear everything visible"),
    ("/", "filter by directory, name or id"),
    ("esc", "leave the filter, clear it, or go back"),
    ("q", "quit without writing"),
];

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
        let rows = picker.rows.clone();
        assert!(picker.handle_key(KeyCode::Left));
        assert_eq!(picker.rows, rows, "h on a closed group does nothing");
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
    fn picker_renders_with_details_and_on_tiny_terminals() {
        use ratatui::backend::TestBackend;
        let theme = Theme::new(true, crate::tui::theme::IconSet::Nerd);
        let mut picker = picker();
        picker.handle_key(KeyCode::Enter);
        for (width, height) in [(1, 1), (30, 6), (140, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| picker.render(f, f.area(), &theme, "Directories"))
                .unwrap();
        }
        let detail: String = picker
            .detail_lines(&theme)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(detail.contains("/app"));
        let row: String = picker
            .row_line(PickRow::Group(0), &theme)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(row.contains("0/2"));
    }

    #[test]
    fn odd_path_spellings_never_loop_and_every_group_is_listed() {
        let spellings = [
            "/proj/",
            "/proj//",
            "/proj",
            "C:\\work\\",
            "C:/work",
            "C:\\work\\app",
            "/",
            "\\",
            "",
        ];
        let entries: Vec<Entry> = spellings
            .iter()
            .enumerate()
            .map(|(i, cwd)| entry(&format!("e{i}"), Some(cwd), "x", 1 + i as u32))
            .collect();
        let picker = GroupPicker::new(entries, Vec::new());
        let mut listed: Vec<usize> = picker
            .rows
            .iter()
            .filter_map(|row| match row {
                PickRow::Group(g) => Some(*g),
                PickRow::Entry { .. } => None,
            })
            .collect();
        listed.sort_unstable();
        listed.dedup();
        assert_eq!(listed.len(), picker.groups.len());
        let app = picker
            .groups
            .iter()
            .position(|g| g.cwd.as_deref() == Some("C:\\work\\app"))
            .unwrap();
        assert_eq!(picker.depth[app], 1);
    }

    #[test]
    fn subdirectories_nest_under_their_parent_and_esc_clears_the_filter() {
        let mut picker = GroupPicker::new(
            vec![
                entry("child", Some("/w/web/admin"), "admin", 9),
                entry("parent", Some("/w/web"), "web", 1),
                entry("other", Some("/w/api"), "api", 5),
                entry("sibling", Some("/w/website"), "site", 2),
            ],
            Vec::new(),
        );
        let titles: Vec<(String, usize)> = picker
            .rows
            .iter()
            .map(|row| match row {
                PickRow::Group(g) => (picker.groups[*g].title(), picker.depth[*g]),
                PickRow::Entry { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(
            titles,
            vec![
                ("/w/web".to_string(), 0),
                ("/w/web/admin".to_string(), 1),
                ("/w/api".to_string(), 0),
                ("/w/website".to_string(), 0),
            ]
        );
        picker.handle_key(KeyCode::Char('/'));
        picker.handle_key(KeyCode::Char('z'));
        picker.handle_key(KeyCode::Enter);
        assert!(picker.rows.is_empty());
        assert_eq!(picker.empty_message(), "No item matches \"z\".");
        picker.handle_key(KeyCode::Esc);
        assert!(picker.filter.is_empty());
        assert_eq!(picker.rows.len(), 4);
    }

    #[test]
    fn rows_fit_the_width_and_hide_the_provider_on_single_provider_lists() {
        let theme = Theme::new(false, crate::tui::theme::IconSet::Unicode);
        let picker = picker();
        for width in [30usize, 60, 120] {
            let line: String = picker
                .row_line_width(PickRow::Group(0), &theme, width)
                .spans
                .iter()
                .map(|span| span.content.to_string())
                .collect();
            assert!(line.chars().count() <= width.max(42), "{width}: {line}");
            assert!(!line.contains("claude 2"));
        }
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
