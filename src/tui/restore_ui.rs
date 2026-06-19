//! Interactive restore: select sessions, edit path mappings, preview, execute.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::claudepath;
use crate::model::Session;
use crate::remap;
use crate::restore::{self, LoadedBackup, RestoreOptions, Results, Status};

use super::{current_user, with_terminal, Tui};

#[derive(PartialEq, Clone, Copy)]
enum Step {
    Select,
    Mapping,
    Preview,
    Done,
}

struct MapRow {
    root: String,
    count: usize,
    value: String,
}

struct RestoreApp {
    loaded: LoadedBackup,
    claude_dir: std::path::PathBuf,
    dry_run: bool,
    rewrite_tool_paths: bool,
    restore_history: bool,
    selected: Vec<bool>,
    state: ListState,
    step: Step,
    rows: Vec<MapRow>,
    focus: usize,
    results: Option<Results>,
    extras_restored: Option<usize>,
    cancelled: bool,
}

impl RestoreApp {
    fn new(
        loaded: LoadedBackup,
        claude_dir: &Path,
        dry_run: bool,
        rewrite_tool_paths: bool,
        restore_history: bool,
    ) -> Self {
        let n = loaded.manifest.sessions.len();
        let mut state = ListState::default();
        if n > 0 {
            state.select(Some(0));
        }
        Self {
            loaded,
            claude_dir: claude_dir.to_path_buf(),
            dry_run,
            rewrite_tool_paths,
            restore_history,
            selected: vec![false; n],
            state,
            step: Step::Select,
            rows: Vec::new(),
            focus: 0,
            results: None,
            extras_restored: None,
            cancelled: false,
        }
    }

    fn sessions(&self) -> &[Session] {
        &self.loaded.manifest.sessions
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&b| b).count()
    }

    fn selected_sessions(&self) -> Vec<Session> {
        self.sessions()
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected[*i])
            .map(|(_, s)| s.clone())
            .collect()
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.sessions().len() as isize;
        if len == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        self.state
            .select(Some((cur + delta).rem_euclid(len) as usize));
    }

    fn build_rows(&mut self) {
        let dirs: Vec<String> = self
            .selected_sessions()
            .iter()
            .map(|s| s.cwd.clone())
            .filter(|c| !c.is_empty())
            .collect();
        let roots = remap::detect_project_roots(&dirs);
        let source_os = self.loaded.manifest.source_os.clone();
        let target_os = std::env::consts::OS;
        let user = current_user();
        self.rows = roots
            .into_iter()
            .map(|r| {
                let value = remap::suggest_mapping(&r.path, &source_os, target_os, &user);
                MapRow {
                    root: r.path,
                    count: r.session_count,
                    value,
                }
            })
            .collect();
        self.focus = 0;
    }

    fn mappings(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for row in &self.rows {
            let v = row.value.trim();
            if !v.is_empty() && v != row.root {
                m.insert(row.root.clone(), v.to_string());
            }
        }
        m
    }

    fn execute(&mut self) -> Result<()> {
        let opts = RestoreOptions {
            claude_dir: self.claude_dir.clone(),
            mappings: self.mappings(),
            dry_run: self.dry_run,
            rewrite_tool_paths: self.rewrite_tool_paths,
            restore_history: self.restore_history,
        };
        let sessions = self.selected_sessions();
        let results = restore::restore_sessions(&sessions, &self.loaded.data, &opts, |_, _, _| {})?;
        self.results = Some(results);
        if !self.loaded.extras.is_empty() {
            self.extras_restored = Some(restore::restore_extras(
                &self.claude_dir,
                &self.loaded,
                self.dry_run,
            ));
        }
        self.step = Step::Done;
        Ok(())
    }

    fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match self.step {
                Step::Select => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        self.cancelled = true;
                        return Ok(());
                    }
                    KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
                    KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
                    KeyCode::Char(' ') => {
                        if let Some(i) = self.state.selected() {
                            self.selected[i] = !self.selected[i];
                        }
                    }
                    KeyCode::Char('a') => self.selected.iter_mut().for_each(|b| *b = true),
                    KeyCode::Char('n') => self.selected.iter_mut().for_each(|b| *b = false),
                    KeyCode::Char('p') | KeyCode::Enter if self.selected_count() > 0 => {
                        self.build_rows();
                        self.step = Step::Mapping;
                    }
                    _ => {}
                },
                Step::Mapping => match key.code {
                    KeyCode::Esc => self.step = Step::Select,
                    KeyCode::Tab | KeyCode::Down => {
                        if !self.rows.is_empty() {
                            self.focus = (self.focus + 1) % self.rows.len();
                        }
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        if !self.rows.is_empty() {
                            self.focus = (self.focus + self.rows.len() - 1) % self.rows.len();
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(row) = self.rows.get_mut(self.focus) {
                            row.value.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(row) = self.rows.get_mut(self.focus) {
                            row.value.push(c);
                        }
                    }
                    KeyCode::Enter => self.step = Step::Preview,
                    _ => {}
                },
                Step::Preview => match key.code {
                    KeyCode::Esc => self.step = Step::Mapping,
                    KeyCode::Enter => self.execute()?,
                    KeyCode::Char('q') => {
                        self.cancelled = true;
                        return Ok(());
                    }
                    _ => {}
                },
                Step::Done => match key.code {
                    KeyCode::Char('q') | KeyCode::Enter | KeyCode::Esc => return Ok(()),
                    _ => {}
                },
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        match self.step {
            Step::Select => self.draw_select(f),
            Step::Mapping => self.draw_mapping(f),
            Step::Preview => self.draw_preview(f),
            Step::Done => self.draw_done(f),
        }
    }

    fn draw_select(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[2, 0, 2]);
        let m = &self.loaded.manifest;
        let header = Paragraph::new(format!(
            "Restore - {}/{} selected | source: {} ({})",
            self.selected_count(),
            self.sessions().len(),
            m.source_host,
            m.source_os
        ))
        .style(bold());
        f.render_widget(header, chunks[0]);

        let items: Vec<ListItem> = self
            .sessions()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let check = if self.selected[i] { "[x]" } else { "[ ]" };
                let cwd = if s.cwd.is_empty() {
                    "<unknown cwd>"
                } else {
                    &s.cwd
                };
                ListItem::new(format!("{check} {cwd}  ({} msgs)", s.message_count))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Sessions"))
            .highlight_style(highlight())
            .highlight_symbol("> ");
        f.render_stateful_widget(list, chunks[1], &mut self.state);

        f.render_widget(
            help("space toggle | a all | n none | p/enter proceed | q quit"),
            chunks[2],
        );
    }

    fn draw_mapping(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[3, 0, 2]);
        let info = Paragraph::new(format!(
            "Path Mapping  (source {} -> target {})\nEdit destination for each project root:",
            self.loaded.manifest.source_os,
            std::env::consts::OS
        ))
        .style(bold());
        f.render_widget(info, chunks[0]);

        let mut lines: Vec<Line> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let marker = if i == self.focus { "> " } else { "  " };
            lines.push(Line::from(vec![Span::styled(
                format!("{}{}  ({} sessions)", marker, row.root, row.count),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )]));
            let cursor = if i == self.focus { "_" } else { "" };
            lines.push(Line::from(format!("    -> {}{}", row.value, cursor)));
            lines.push(Line::from(""));
        }
        let body = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(body, chunks[1]);

        f.render_widget(
            help("tab/down next | shift-tab/up prev | type to edit | enter confirm | esc back"),
            chunks[2],
        );
    }

    fn draw_preview(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[2, 0, 2]);
        let title = Paragraph::new(if self.dry_run {
            "Preview (DRY RUN)"
        } else {
            "Preview"
        })
        .style(bold());
        f.render_widget(title, chunks[0]);

        let mappings = self.mappings();
        let mut lines: Vec<Line> = Vec::new();
        for s in self.selected_sessions() {
            let new_cwd = remap::apply_longest_mapping(&s.cwd, &mappings);
            let encoded = claudepath::encode(&new_cwd);
            lines.push(Line::from(s.cwd.clone()));
            if new_cwd != s.cwd {
                lines.push(Line::from(Span::styled(
                    format!("  -> {new_cwd}"),
                    Style::default().fg(Color::Green),
                )));
            }
            let dest = self
                .claude_dir
                .join("projects")
                .join(&encoded)
                .join(format!("{}.jsonl", s.id));
            let status = if dest.exists() {
                Span::styled("  [SKIP exists]", Style::default().fg(Color::Yellow))
            } else {
                Span::styled("  [OK]", Style::default().fg(Color::Green))
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  encoded: {encoded}"),
                    Style::default().fg(Color::DarkGray),
                ),
                status,
            ]));
            lines.push(Line::from(""));
        }
        let body = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(body, chunks[1]);

        f.render_widget(help("enter execute | esc back | q quit"), chunks[2]);
    }

    fn draw_done(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[2, 0, 2]);
        f.render_widget(Paragraph::new("Restore Complete").style(bold()), chunks[0]);

        let mut lines: Vec<Line> = Vec::new();
        if let Some(r) = &self.results {
            if self.dry_run {
                lines.push(Line::from(Span::styled(
                    "DRY RUN - no changes written",
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(""));
            }
            lines.push(Line::from(format!("Restored: {}", r.restored)));
            lines.push(Line::from(format!("Skipped:  {}", r.skipped)));
            lines.push(Line::from(format!("Failed:   {}", r.failed)));
            let rewritten: usize = r.details.iter().map(|d| d.lines_rewritten).sum();
            lines.push(Line::from(format!(
                "JSONL lines with rewritten cwd: {rewritten}"
            )));
            if let Some(n) = self.extras_restored {
                lines.push(Line::from(format!("Extras restored: {n} files")));
            }
            for d in &r.details {
                if let Some(c) = &d.case_collision {
                    lines.push(Line::from(Span::styled(
                        format!("! case collision: {} vs {}", d.encoded, c),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                if d.status == Status::Failed {
                    lines.push(Line::from(Span::styled(
                        format!("x {}: {}", d.old_cwd, d.note),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
        let body = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(body, chunks[1]);
        f.render_widget(help("enter or q to exit"), chunks[2]);
    }
}

fn vchunks(area: Rect, sizes: &[u16]) -> std::rc::Rc<[Rect]> {
    let constraints: Vec<Constraint> = sizes
        .iter()
        .map(|&s| {
            if s == 0 {
                Constraint::Min(1)
            } else {
                Constraint::Length(s)
            }
        })
        .collect();
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn highlight() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

fn help(text: &str) -> Paragraph<'_> {
    Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
}

pub fn run_restore(
    backup_path: &Path,
    claude_dir: &Path,
    dry_run: bool,
    rewrite_tool_paths: bool,
    restore_history: bool,
    no_verify_checksum: bool,
) -> Result<()> {
    let loaded = restore::load_backup(backup_path)?;

    if !no_verify_checksum {
        let report = restore::verify_checksums(&loaded);
        if report.skipped {
            eprintln!("Note: v1 backup has no per-artifact checksums; skipping verification.");
        } else if !report.ok() {
            for m in &report.mismatches {
                eprintln!("  checksum error: {m}");
            }
            bail!(
                "checksum verification failed ({} of {} artifacts); pass --no-verify-checksum to override",
                report.mismatches.len(),
                report.checked
            );
        } else {
            println!("Checksum OK ({} artifacts).", report.checked);
        }
    }

    let mut app = RestoreApp::new(
        loaded,
        claude_dir,
        dry_run,
        rewrite_tool_paths,
        restore_history,
    );
    with_terminal(|t| app.run(t))?;

    if app.cancelled {
        println!("Cancelled.");
        return Ok(());
    }

    if let Some(r) = &app.results {
        println!(
            "Done. Restored: {} | Skipped: {} | Failed: {}",
            r.restored, r.skipped, r.failed
        );
    }
    Ok(())
}
