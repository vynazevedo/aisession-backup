use std::path::Path;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::backup;
use crate::discover::{self, DiscoveredSession};
use crate::engine::{self, BackupRequest, ClaudeSelection, Passphrase};
use crate::plan::Plan;
use crate::ui::{human_size, sanitize};

use super::{render_plan_lines, with_terminal, Tui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupStep {
    Select,
    Confirm,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupAction {
    Confirm,
    Cancel,
}

pub struct BackupApp {
    pub sessions: Vec<DiscoveredSession>,
    pub selected: Vec<bool>,
    pub visible: Vec<usize>,
    pub state: ListState,
    pub filter: String,
    pub filtering: bool,
    pub step: BackupStep,
    pub plan: Option<Plan>,
    pub action: Option<BackupAction>,
}

impl BackupApp {
    pub fn new(sessions: Vec<DiscoveredSession>) -> Self {
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
            step: BackupStep::Select,
            plan: None,
            action: None,
        }
    }

    pub fn recompute_visible(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                needle.is_empty()
                    || s.session.cwd.to_lowercase().contains(&needle)
                    || s.session.preview.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        if self.visible.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&b| b).count()
    }

    pub fn cursor_session(&self) -> Option<usize> {
        self.state
            .selected()
            .and_then(|v| self.visible.get(v))
            .copied()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len() as isize;
        let cur = self.state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len);
        self.state.select(Some(next as usize));
    }

    pub fn selected_sessions(&self) -> Vec<DiscoveredSession> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected[*i])
            .map(|(_, s)| s.clone())
            .collect()
    }

    pub fn selected_session_ids(&self) -> Vec<String> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected[*i])
            .map(|(_, s)| s.session.id.clone())
            .collect()
    }

    pub fn set_plan(&mut self, plan: Plan) {
        self.plan = Some(plan);
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        match self.step {
            BackupStep::Select => {
                if self.filtering {
                    match code {
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
                    return;
                }
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        self.action = Some(BackupAction::Cancel);
                        self.step = BackupStep::Done;
                    }
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
                        self.step = BackupStep::Confirm;
                    }
                    _ => {}
                }
            }
            BackupStep::Confirm => match code {
                KeyCode::Esc => {
                    self.step = BackupStep::Select;
                }
                KeyCode::Char('q') => {
                    self.action = Some(BackupAction::Cancel);
                    self.step = BackupStep::Done;
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    let has_refusals = self.plan.as_ref().map(Plan::has_refusals).unwrap_or(false);
                    if !has_refusals {
                        self.action = Some(BackupAction::Confirm);
                        self.step = BackupStep::Done;
                    }
                }
                _ => {}
            },
            BackupStep::Done => {}
        }
    }

    fn run(
        &mut self,
        terminal: &mut Tui,
        claude_dir: &Path,
        output: &Path,
        opts: backup::BackupOptions,
    ) -> Result<BackupAction> {
        loop {
            if self.step == BackupStep::Confirm && self.plan.is_none() {
                let req = BackupRequest {
                    output: output.to_path_buf(),
                    overwrite: false,
                    claude: Some(ClaudeSelection {
                        claude_dir: claude_dir.to_path_buf(),
                        session_ids: Some(self.selected_session_ids()),
                        match_path: None,
                        include_extras: opts.include_extras,
                        include_config: opts.include_config,
                        include_credentials: opts.include_credentials,
                    }),
                    providers: Vec::new(),
                    passphrase: Passphrase::None,
                };
                self.plan = Some(match engine::plan_backup(&req) {
                    Ok(plan) => plan,
                    Err(err) => Plan::failed(format!("{err:#}")),
                });
            } else if self.step != BackupStep::Confirm {
                self.plan = None;
            }
            terminal.draw(|f| self.draw(f))?;
            if let Some(act) = self.action {
                return Ok(act);
            }
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key.code);
                }
            }
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(f.area());

        if self.step == BackupStep::Confirm {
            let header = Paragraph::new("Confirm Backup Plan")
                .style(Style::default().add_modifier(Modifier::BOLD));
            f.render_widget(header, chunks[0]);

            let body = Paragraph::new(render_plan_lines(self.plan.as_ref()))
                .block(Block::default().borders(Borders::ALL).title("Plan"))
                .wrap(Wrap { trim: false });
            f.render_widget(body, chunks[1]);

            let help = Paragraph::new("enter/y confirm | esc back | q quit")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
            f.render_widget(help, chunks[2]);
            return;
        }

        let header = Paragraph::new(format!(
            "Backup - {}/{} selected{}",
            self.selected_count(),
            self.sessions.len(),
            if self.filtering {
                format!("   filter: {}_", sanitize(&self.filter))
            } else if !self.filter.is_empty() {
                format!("   filter: {}", sanitize(&self.filter))
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
                    "<unknown cwd>".to_string()
                } else {
                    sanitize(&s.cwd)
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
        println!(
            "No sessions found in {}",
            sanitize(&claude_dir.display().to_string())
        );
        return Ok(());
    }

    let mut app = BackupApp::new(sessions);
    let action = with_terminal(|t| app.run(t, claude_dir, output, opts))?;

    if action == BackupAction::Cancel {
        println!("Cancelled.");
        return Ok(());
    }

    let ids = app.selected_session_ids();
    if ids.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let req = BackupRequest {
        output: output.to_path_buf(),
        overwrite: false,
        claude: Some(ClaudeSelection {
            claude_dir: claude_dir.to_path_buf(),
            session_ids: Some(ids),
            match_path: None,
            include_extras: opts.include_extras,
            include_config: opts.include_config,
            include_credentials: opts.include_credentials,
        }),
        providers: Vec::new(),
        passphrase: Passphrase::None,
    };
    let report = engine::run_backup(&req)?;
    print!(
        "{}",
        crate::ui::format_backup_report(&report, crate::ui::color_stdout())
    );
    Ok(())
}
