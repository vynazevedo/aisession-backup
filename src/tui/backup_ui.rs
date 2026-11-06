use std::path::Path;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, Paragraph, Wrap};

use crate::backup;
use crate::discover::{self, DiscoveredSession};
use crate::engine::{self, BackupRequest, ClaudeSelection, Passphrase};
use crate::plan::Plan;
use crate::ui::sanitize;

use super::groups::{GroupPicker, PICKER_HELP};
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
    pub picker: GroupPicker,
    pub step: BackupStep,
    pub plan: Option<Plan>,
    pub action: Option<BackupAction>,
}

impl BackupApp {
    pub fn new(sessions: Vec<DiscoveredSession>) -> Self {
        let entries = sessions
            .iter()
            .map(|found| crate::grouping::Entry {
                id: found.session.id.clone(),
                label: found.session.preview.clone(),
                cwd: Some(found.session.cwd.clone()),
                provider: crate::provider::ProviderId::Claude.as_str().to_string(),
                detail: format!("{} msgs", found.session.message_count),
                size: found.session.size_bytes,
                updated_at: Some(found.session.timestamp),
            })
            .collect();
        Self {
            sessions,
            picker: GroupPicker::new(entries, Vec::new()),
            step: BackupStep::Select,
            plan: None,
            action: None,
        }
    }

    pub fn selected_count(&self) -> usize {
        self.picker.selected_count()
    }

    pub fn selected_session_ids(&self) -> Vec<String> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| self.picker.selected[*i])
            .map(|(_, s)| s.session.id.clone())
            .collect()
    }

    pub fn set_plan(&mut self, plan: Plan) {
        self.plan = Some(plan);
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        match self.step {
            BackupStep::Select => {
                if self.picker.filtering {
                    self.picker.handle_key(code);
                    return;
                }
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        self.action = Some(BackupAction::Cancel);
                        self.step = BackupStep::Done;
                    }
                    KeyCode::Char('b') => {
                        if self.selected_count() > 0 {
                            self.step = BackupStep::Confirm;
                        }
                    }
                    _ => {
                        self.picker.handle_key(code);
                    }
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
            self.picker.filter_suffix()
        ))
        .style(Style::default().add_modifier(Modifier::BOLD));
        f.render_widget(header, chunks[0]);

        let list = List::new(self.picker.list_items())
            .block(Block::default().borders(Borders::ALL).title("Directories"))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        f.render_stateful_widget(list, chunks[1], &mut self.picker.state);

        let help = Paragraph::new(format!("{PICKER_HELP} | b backup | q quit"))
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
