use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, Paragraph, Wrap};

use crate::engine::{self, Passphrase, PreparedRestore, RestoreReport, RestoreRequest};
use crate::model::{Manifest, Session};
use crate::plan::Plan;
use crate::provider::ProviderId;
use crate::remap;
use crate::ui::sanitize;

use super::groups::{GroupPicker, PICKER_HELP};
use super::{current_user, render_plan_lines, with_terminal, Tui};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RestoreStep {
    Select,
    Mapping,
    Preview,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapRow {
    pub root: String,
    pub count: usize,
    pub value: String,
}

pub struct RestoreApp {
    pub manifest: Manifest,
    pub archive_path: PathBuf,
    pub claude_dir: PathBuf,
    pub flags: super::RestoreFlags,
    pub picker: GroupPicker,
    item_refs: Vec<(ProviderId, String, Option<String>)>,
    pub step: RestoreStep,
    pub rows: Vec<MapRow>,
    pub focus: usize,
    pub plan: Option<Plan>,
    pub report: Option<RestoreReport>,
    pub confirmed: bool,
    pub cancelled: bool,
    pub passphrase: Passphrase,
    pub provider_homes: BTreeMap<ProviderId, PathBuf>,
    pub mapping_error: Option<String>,
    prepared: Option<PreparedRestore>,
}

impl RestoreApp {
    pub fn new(
        manifest: Manifest,
        archive_path: &Path,
        claude_dir: &Path,
        flags: super::RestoreFlags,
    ) -> Self {
        let picker = GroupPicker::new(crate::cli::manifest_entries(&manifest), Vec::new());
        let item_refs = manifest
            .providers
            .iter()
            .flat_map(|provider| {
                provider
                    .items
                    .iter()
                    .map(|item| (provider.id, item.id.clone(), item.cwd.clone()))
            })
            .collect();
        Self {
            manifest,
            archive_path: archive_path.to_path_buf(),
            claude_dir: claude_dir.to_path_buf(),
            flags,
            picker,
            item_refs,
            step: RestoreStep::Select,
            rows: Vec::new(),
            focus: 0,
            plan: None,
            report: None,
            confirmed: false,
            cancelled: false,
            passphrase: Passphrase::None,
            provider_homes: BTreeMap::new(),
            mapping_error: None,
            prepared: None,
        }
    }

    pub fn sessions(&self) -> &[Session] {
        &self.manifest.sessions
    }

    pub fn selected_count(&self) -> usize {
        self.picker.selected_count()
    }

    pub fn selected_sessions(&self) -> Vec<Session> {
        self.sessions()
            .iter()
            .enumerate()
            .filter(|(i, _)| self.picker.selected[*i])
            .map(|(_, s)| s.clone())
            .collect()
    }

    fn selected_item_refs(&self) -> impl Iterator<Item = &(ProviderId, String, Option<String>)> {
        let offset = self.manifest.sessions.len();
        self.item_refs
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.picker.selected[offset + i])
            .map(|(_, item)| item)
    }

    pub fn build_rows(&mut self) {
        let mut dirs: Vec<String> = self
            .selected_sessions()
            .iter()
            .map(|s| s.cwd.clone())
            .filter(|c| !c.is_empty())
            .collect();
        dirs.extend(
            self.selected_item_refs()
                .filter_map(|(_, _, cwd)| cwd.clone())
                .filter(|cwd| !cwd.is_empty()),
        );
        let roots = remap::detect_project_roots(&dirs);
        let source_os = self.manifest.source_os.clone();
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

    pub fn mappings(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for row in &self.rows {
            let v = row.value.trim();
            if !v.is_empty() && v != row.root {
                m.insert(row.root.clone(), v.to_string());
            }
        }
        m
    }

    pub fn set_plan(&mut self, plan: Plan) {
        self.plan = Some(plan);
    }

    pub fn build_restore_request(&self) -> RestoreRequest {
        RestoreRequest {
            archive: self.archive_path.clone(),
            passphrase: self.passphrase.clone(),
            claude_dir: self.claude_dir.clone(),
            provider_homes: self.provider_homes.clone(),
            only_providers: None,
            only_sessions: Some(
                self.selected_sessions()
                    .into_iter()
                    .map(|session| session.id)
                    .collect(),
            ),
            only_items: Some(
                self.selected_item_refs()
                    .map(|(provider, id, _)| (*provider, id.clone()))
                    .collect(),
            ),
            mappings: self.mappings(),
            rewrite_tool_paths: self.flags.rewrite_tool_paths,
            restore_history: self.flags.restore_history,
            apply_config: self.flags.apply_config,
            overwrite: self.flags.overwrite_config,
            allow_exec_config: self.flags.allow_exec_config,
            allow_credentials: self.flags.allow_credentials,
            allow_running: self.flags.allow_running,
            skip_verify: self.flags.no_verify_checksum,
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        match self.step {
            RestoreStep::Select => {
                if self.picker.filtering {
                    self.picker.handle_key(code);
                    return;
                }
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        self.cancelled = true;
                        self.step = RestoreStep::Done;
                    }
                    KeyCode::Char('p') => {
                        if self.selected_count() > 0 {
                            self.build_rows();
                            self.step = RestoreStep::Mapping;
                        }
                    }
                    _ => {
                        self.picker.handle_key(code);
                    }
                }
            }
            RestoreStep::Mapping => match code {
                KeyCode::Esc => self.step = RestoreStep::Select,
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
                KeyCode::Enter => {
                    let invalid = self.rows.iter().find_map(|row| {
                        let value = row.value.trim();
                        if value.is_empty() {
                            return None;
                        }
                        crate::cli::validate_mapping_target(value)
                            .err()
                            .map(|reason| format!("{reason}: {value}"))
                    });
                    if let Some(reason) = invalid {
                        self.mapping_error = Some(reason);
                    } else {
                        self.mapping_error = None;
                        self.plan = None;
                        self.prepared = None;
                        self.step = RestoreStep::Preview;
                    }
                }
                _ => {}
            },
            RestoreStep::Preview => match code {
                KeyCode::Esc => self.step = RestoreStep::Mapping,
                KeyCode::Enter | KeyCode::Char('y') => {
                    let has_refusals = self.plan.as_ref().map(Plan::has_refusals).unwrap_or(false);
                    if !has_refusals {
                        self.confirmed = true;
                        self.step = RestoreStep::Done;
                    }
                }
                KeyCode::Char('q') => {
                    self.cancelled = true;
                    self.step = RestoreStep::Done;
                }
                KeyCode::Char('r') => {
                    self.flags.allow_running = !self.flags.allow_running;
                    self.invalidate_plan();
                }
                KeyCode::Char('x') => {
                    self.flags.allow_exec_config = !self.flags.allow_exec_config;
                    self.invalidate_plan();
                }
                KeyCode::Char('c') => {
                    self.flags.allow_credentials = !self.flags.allow_credentials;
                    self.invalidate_plan();
                }
                KeyCode::Char('o') => {
                    self.flags.overwrite_config = !self.flags.overwrite_config;
                    self.invalidate_plan();
                }
                _ => {}
            },
            RestoreStep::Done => {}
        }
    }

    fn invalidate_plan(&mut self) {
        self.plan = None;
        self.prepared = None;
    }

    pub fn preview_help(&self) -> String {
        let state = |on: bool| if on { "on" } else { "off" };
        let blocked = if self.plan.as_ref().is_some_and(Plan::has_refusals) {
            "Blocked by REFUSE entries, enter is disabled | "
        } else {
            "enter/y execute | "
        };
        format!(
            "{blocked}r allow-running:{} | x exec-config:{} | c credentials:{} | o overwrite:{} | esc back | q quit",
            state(self.flags.allow_running),
            state(self.flags.allow_exec_config),
            state(self.flags.allow_credentials),
            state(self.flags.overwrite_config),
        )
    }

    fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        loop {
            if self.step == RestoreStep::Preview && self.plan.is_none() {
                let req = self.build_restore_request();
                match engine::prepare_restore(req) {
                    Ok(prepared) => {
                        self.plan = Some(prepared.plan.clone());
                        self.prepared = Some(prepared);
                    }
                    Err(err) => self.plan = Some(Plan::failed(format!("{err:#}"))),
                }
            }
            terminal.draw(|f| self.draw(f))?;
            if self.step == RestoreStep::Done {
                return Ok(());
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            self.handle_key(key.code);
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        match self.step {
            RestoreStep::Select => self.draw_select(f),
            RestoreStep::Mapping => self.draw_mapping(f),
            RestoreStep::Preview => self.draw_preview(f),
            RestoreStep::Done => self.draw_done(f),
        }
    }

    fn draw_select(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[2, 0, 2]);
        let m = &self.manifest;
        let header = Paragraph::new(format!(
            "Restore - {}/{} selected | source: {} ({}){}",
            self.picker.selected_count(),
            self.picker.entries.len(),
            sanitize(&m.source_host),
            sanitize(&m.source_os),
            self.picker.filter_suffix()
        ))
        .style(bold());
        f.render_widget(header, chunks[0]);

        let list = List::new(self.picker.list_items())
            .block(Block::default().borders(Borders::ALL).title("Directories"))
            .highlight_style(highlight())
            .highlight_symbol("> ");
        f.render_stateful_widget(list, chunks[1], &mut self.picker.state);

        let hint = format!("{PICKER_HELP} | p proceed | q quit");
        f.render_widget(help(&hint), chunks[2]);
    }

    fn draw_mapping(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[3, 0, 2]);
        let info = Paragraph::new(format!(
            "Path Mapping  (source {} -> target {})\nEdit destination for each project root:",
            sanitize(&self.manifest.source_os),
            std::env::consts::OS
        ))
        .style(bold());
        f.render_widget(info, chunks[0]);

        let mut lines: Vec<Line> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let marker = if i == self.focus { "> " } else { "  " };
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "{}{}  ({} sessions)",
                    marker,
                    sanitize(&row.root),
                    row.count
                ),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )]));
            let cursor = if i == self.focus { "_" } else { "" };
            lines.push(Line::from(format!(
                "    -> {}{}",
                sanitize(&row.value),
                cursor
            )));
            lines.push(Line::from(""));
        }
        let body = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(body, chunks[1]);

        let hint = match &self.mapping_error {
            Some(err) => sanitize(err),
            None => "tab/down next | shift-tab/up prev | type to edit | enter confirm | esc back"
                .to_string(),
        };
        f.render_widget(help(&hint), chunks[2]);
    }

    fn draw_preview(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[2, 0, 2]);
        let title = Paragraph::new(if self.flags.dry_run {
            "Confirm Restore Plan (DRY RUN)"
        } else {
            "Confirm Restore Plan"
        })
        .style(bold());
        f.render_widget(title, chunks[0]);

        let lines = render_plan_lines(self.plan.as_ref());
        let body = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Plan"))
            .wrap(Wrap { trim: false });
        f.render_widget(body, chunks[1]);

        let hint = self.preview_help();
        f.render_widget(help(&hint), chunks[2]);
    }

    fn draw_done(&mut self, f: &mut Frame) {
        let chunks = vchunks(f.area(), &[2, 0, 2]);
        f.render_widget(Paragraph::new("Restore Complete").style(bold()), chunks[0]);

        let mut lines: Vec<Line> = Vec::new();
        if let Some(r) = &self.report {
            lines.push(Line::from(format!("Written: {}", r.written)));
            lines.push(Line::from(format!("Merged:  {}", r.merged)));
            lines.push(Line::from(format!("Skipped: {}", r.skipped)));
            lines.push(Line::from(format!("Failed:  {}", r.failed.len())));
            for err in &r.failed {
                lines.push(Line::from(Span::styled(
                    format!("x {}", sanitize(err)),
                    Style::default().fg(Color::Red),
                )));
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
    flags: super::RestoreFlags,
) -> Result<()> {
    let passphrase = crate::cli::resolve_decrypt_passphrase(backup_path)?;
    let staged = crate::archive::stage(backup_path, &passphrase)?;
    if !flags.no_verify_checksum && !staged.report.ok() {
        bail!("archive verification failed; pass --no-verify-checksum to override");
    }

    let mut app = RestoreApp::new(staged.manifest, backup_path, claude_dir, flags);
    app.passphrase = passphrase;
    app.provider_homes = crate::cli::default_provider_homes(claude_dir);
    with_terminal(|t| app.run(t))?;

    if app.cancelled || !app.confirmed {
        println!("Cancelled.");
        return Ok(());
    }

    let Some(prepared) = app.prepared.take() else {
        bail!("restore plan was not prepared");
    };
    if flags.dry_run {
        crate::ui::print_plan(&prepared.plan, false);
        return Ok(());
    }
    let report = engine::apply_restore(prepared)?;
    print!(
        "{}",
        crate::ui::format_restore_report(&report, crate::ui::color_stdout())
    );
    if !report.failed.is_empty() {
        bail!(
            "restore finished with {} failed operation(s)",
            report.failed.len()
        );
    }
    Ok(())
}
