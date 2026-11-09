use std::path::Path;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;

use crate::backup;
use crate::discover::{self, DiscoveredSession};
use crate::engine::{self, BackupRequest, ClaudeSelection, Passphrase};
use crate::plan::Plan;
use crate::ui::sanitize;

use super::groups::{GroupPicker, PICKER_HELP_KEYS, PICKER_KEYS};
use super::theme::{self, Scroll};
use super::{with_terminal, Tui};

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
    pub show_help: bool,
    plan_scroll: Scroll,
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
            show_help: false,
            plan_scroll: Scroll::default(),
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
        if self.show_help {
            self.show_help = false;
            return;
        }
        if code == KeyCode::Char('?') && !self.picker.filtering && self.step != BackupStep::Done {
            self.show_help = true;
            return;
        }
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
                _ => {
                    self.plan_scroll.handle_key(code);
                }
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
                self.plan_scroll = Scroll::default();
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
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        let selected = self.selected_count();
        if self.step == BackupStep::Confirm {
            f.render_widget(
                theme::header(theme, "Backup plan", &format!("{selected} session(s)")),
                top,
            );
            theme::render_plan(f, body, theme, self.plan.as_ref(), &mut self.plan_scroll);
            let mut keys = theme::PLAN_KEYS.to_vec();
            if !self.plan.as_ref().is_some_and(Plan::has_refusals) {
                keys.insert(0, ("enter", "write backup"));
            }
            keys.extend([("esc", "back"), ("q", "quit")]);
            f.render_widget(theme::footer(theme, &keys), bottom);
        } else {
            f.render_widget(
                theme::header(
                    theme,
                    "Backup",
                    &format!("{selected}/{} session(s) selected", self.sessions.len()),
                ),
                top,
            );
            self.picker.render(f, body, theme, "Directories");
            let mut keys = PICKER_KEYS.to_vec();
            if selected > 0 {
                keys.insert(0, ("b", "backup"));
            }
            keys.push(("q", "quit"));
            f.render_widget(theme::footer(theme, &keys), bottom);
        }
        if self.show_help {
            let keys: Vec<(&str, &str)> = if self.step == BackupStep::Confirm {
                vec![
                    ("j/k", "scroll the plan"),
                    ("g / G", "top or end of the plan"),
                    ("enter / y", "write the backup"),
                    ("esc", "back to the selection"),
                    ("q", "quit without writing"),
                ]
            } else {
                let mut keys = PICKER_HELP_KEYS.to_vec();
                keys.push(("b", "review the backup plan"));
                keys
            };
            theme::render_help(f, theme, "Keys", &keys);
        }
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
