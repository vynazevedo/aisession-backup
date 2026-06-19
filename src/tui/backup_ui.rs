//! Interactive session selection for `backup`.

use std::path::Path;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::backup;
use crate::cli::human_size;
use crate::discover::{self, DiscoveredSession};

use super::{with_terminal, Tui};

#[derive(PartialEq)]
enum Action {
    Confirm,
    Cancel,
}

struct BackupApp {
    sessions: Vec<DiscoveredSession>,
    selected: Vec<bool>,
    visible: Vec<usize>,
    state: ListState,
    filter: String,
    filtering: bool,
}

impl BackupApp {
    fn new(sessions: Vec<DiscoveredSession>) -> Self {
        let selected = vec![false; sessions.len()];
        let visible = (0..sessions.len()).collect();
        let mut state = ListState::default();
        if !sessions.is_empty() {
            state.select(Some(0));
        }
        Self {
            sessions,
            selected,
            visible,
            state,
            filter: String::new(),
            filtering: false,
        }
    }

    fn recompute_visible(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| needle.is_empty() || s.session.cwd.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.visible.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&b| b).count()
    }

    fn cursor_session(&self) -> Option<usize> {
        self.state
            .selected()
            .and_then(|v| self.visible.get(v))
            .copied()
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len() as isize;
        let cur = self.state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len);
        self.state.select(Some(next as usize));
    }

    fn selected_sessions(&self) -> Vec<DiscoveredSession> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected[*i])
            .map(|(_, s)| s.clone())
            .collect()
    }

    fn run(&mut self, terminal: &mut Tui) -> Result<Action> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if self.filtering {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                        KeyCode::Backspace => {
                            self.filter.pop();
                            self.recompute_visible();
                        }
                        KeyCode::Char(c) => {
                            self.filter.push(c);
                            self.recompute_visible();
                        }
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Cancel),
                    KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
                    KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
                    KeyCode::Char(' ') => {
                        if let Some(idx) = self.cursor_session() {
                            self.selected[idx] = !self.selected[idx];
                        }
                    }
                    KeyCode::Char('a') => self.selected.iter_mut().for_each(|b| *b = true),
                    KeyCode::Char('n') => self.selected.iter_mut().for_each(|b| *b = false),
                    KeyCode::Char('/') => self.filtering = true,
                    KeyCode::Char('b') | KeyCode::Enter if self.selected_count() > 0 => {
                        return Ok(Action::Confirm);
                    }
                    _ => {}
                }
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(f.area());

        let header = Paragraph::new(format!(
            "Backup - {}/{} selected{}",
            self.selected_count(),
            self.sessions.len(),
            if self.filtering {
                format!("   filter: {}_", self.filter)
            } else if !self.filter.is_empty() {
                format!("   filter: {}", self.filter)
            } else {
                String::new()
            }
        ))
        .style(Style::default().add_modifier(Modifier::BOLD));
        f.render_widget(header, chunks[0]);

        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let s = &self.sessions[i].session;
                let check = if self.selected[i] { "[x]" } else { "[ ]" };
                let cwd = if s.cwd.is_empty() {
                    "<unknown cwd>"
                } else {
                    &s.cwd
                };
                let line = format!(
                    "{check} {cwd}  ({} msgs, {})",
                    s.message_count,
                    human_size(s.size_bytes)
                );
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Sessions"))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        f.render_stateful_widget(list, chunks[1], &mut self.state);

        let help =
            Paragraph::new("space toggle | a all | n none | / filter | b/enter backup | q quit")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
        f.render_widget(help, chunks[2]);
    }
}

pub fn run_backup(claude_dir: &Path, output: &Path, opts: backup::BackupOptions) -> Result<()> {
    let sessions = discover::load_sessions(claude_dir)?;
    if sessions.is_empty() {
        println!("No sessions found in {}", claude_dir.display());
        return Ok(());
    }

    let mut app = BackupApp::new(sessions);
    let action = with_terminal(|t| app.run(t))?;

    if action == Action::Cancel {
        println!("Cancelled.");
        return Ok(());
    }

    let selected = app.selected_sessions();
    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    println!("Creating backup of {} sessions...", selected.len());
    let manifest = backup::create(&selected, output, claude_dir, opts)?;
    let mut tags = Vec::new();
    if opts.include_extras {
        tags.push("extras");
    }
    if opts.include_config {
        tags.push("config");
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!(", with {}", tags.join(" + "))
    };
    println!(
        "Backup written to {} ({} sessions, {}{}).",
        output.display(),
        manifest.sessions.len(),
        human_size(manifest.total_size),
        suffix
    );
    Ok(())
}
