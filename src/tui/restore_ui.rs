use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

use crate::engine::{self, Passphrase, PreparedRestore, RestoreReport, RestoreRequest};
use crate::model::{Manifest, Session};
use crate::plan::Plan;
use crate::provider::ProviderId;
use crate::remap;
use crate::ui::sanitize;

use super::groups::{GroupPicker, PICKER_HELP_KEYS, PICKER_KEYS};
use super::theme::{self, Icon, Scroll, Theme};
use super::{current_user, with_terminal, Tui};

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
    pub show_help: bool,
    plan_scroll: Scroll,
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
            show_help: false,
            plan_scroll: Scroll::default(),
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
        if self.show_help {
            self.show_help = false;
            return;
        }
        let help_allowed = match self.step {
            RestoreStep::Select => !self.picker.filtering,
            RestoreStep::Preview => true,
            _ => false,
        };
        if code == KeyCode::Char('?') && help_allowed {
            self.show_help = true;
            return;
        }
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
                _ => {
                    self.plan_scroll.handle_key(code);
                }
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
                self.plan_scroll = Scroll::default();
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
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        let m = &self.manifest;
        let context = format!(
            "{}/{} selected  |  from {} ({})",
            self.picker.selected_count(),
            self.picker.entries.len(),
            sanitize(&m.source_host),
            sanitize(&m.source_os)
        );
        f.render_widget(theme::header(theme, "Restore", &context), top);
        self.picker
            .render(f, body, theme, "Directories in the archive");
        let mut keys = PICKER_KEYS.to_vec();
        if self.selected_count() > 0 {
            keys.insert(0, ("p", "proceed"));
        }
        keys.push(("q", "quit"));
        f.render_widget(theme::footer(theme, &keys), bottom);
        if self.show_help {
            let mut keys = PICKER_HELP_KEYS.to_vec();
            keys.push(("p", "map paths and review the plan"));
            theme::render_help(f, theme, "Keys", &keys);
        }
    }

    fn draw_mapping(&mut self, f: &mut Frame) {
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        let context = format!(
            "{} {} {}",
            sanitize(&self.manifest.source_os),
            theme.icon(Icon::Arrow),
            std::env::consts::OS
        );
        f.render_widget(theme::header(theme, "Path mapping", &context), top);

        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "Type the destination of each directory. Leave it unchanged to keep the path.",
                theme.muted(),
            )),
            Line::from(""),
        ];
        for (i, row) in self.rows.iter().enumerate() {
            let focused = i == self.focus;
            lines.push(Line::from(vec![
                Span::styled(if focused { "> " } else { "  " }, theme.accent()),
                Span::styled(format!("{} ", theme.icon(Icon::Folder)), theme.accent()),
                Span::styled(sanitize(&row.root), theme.strong()),
                Span::styled(format!("  {} item(s)", row.count), theme.muted()),
            ]));
            let value_style = if focused {
                theme.selected_row()
            } else if row.value == row.root {
                theme.muted()
            } else {
                theme.success()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("    {} ", theme.icon(Icon::Arrow)), theme.accent()),
                Span::styled(sanitize(&row.value), value_style),
                Span::styled(if focused { "_" } else { "" }, theme.accent()),
            ]));
            lines.push(Line::from(""));
        }
        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "No directory to map in the selection.",
                theme.muted(),
            )));
        }
        f.render_widget(
            Paragraph::new(lines)
                .block(theme.panel("Destinations"))
                .wrap(Wrap { trim: false }),
            body,
        );
        match &self.mapping_error {
            Some(err) => f.render_widget(theme::status_line(theme, err, true), bottom),
            None => f.render_widget(
                theme::footer(
                    theme,
                    &[
                        ("enter", "review plan"),
                        ("tab", "next"),
                        ("shift-tab", "previous"),
                        ("type", "edit"),
                        ("esc", "back"),
                    ],
                ),
                bottom,
            ),
        }
    }

    fn toggle_spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (key, label, on) in [
            ("r", "allow-running", self.flags.allow_running),
            ("x", "exec-config", self.flags.allow_exec_config),
            ("c", "credentials", self.flags.allow_credentials),
            ("o", "overwrite", self.flags.overwrite_config),
        ] {
            spans.push(Span::styled(format!(" {key} "), theme.key()));
            spans.push(Span::styled(format!(" {label} "), theme.muted()));
            spans.push(if on {
                Span::styled("ON  ", theme.warning())
            } else {
                Span::styled("off  ", theme.muted())
            });
        }
        spans
    }

    fn draw_preview(&mut self, f: &mut Frame) {
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        let title = if self.flags.dry_run {
            "Restore plan (dry run)"
        } else {
            "Restore plan"
        };
        f.render_widget(
            theme::header(
                theme,
                title,
                &format!("{} item(s) selected", self.selected_count()),
            ),
            top,
        );
        theme::render_plan(f, body, theme, self.plan.as_ref(), &mut self.plan_scroll);
        let blocked = self.plan.as_ref().is_some_and(Plan::has_refusals);
        let mut keys = theme::PLAN_KEYS.to_vec();
        keys.insert(
            0,
            if blocked {
                ("blocked", "resolve REFUSE first")
            } else {
                ("enter", "restore")
            },
        );
        keys.extend([("esc", "back"), ("?", "help"), ("q", "quit")]);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(bottom);
        f.render_widget(theme::footer(theme, &keys), rows[0]);
        f.render_widget(
            Paragraph::new(Line::from(self.toggle_spans(theme))),
            rows[1],
        );
        if self.show_help {
            theme::render_help(
                f,
                theme,
                "Keys",
                &[
                    ("j/k", "scroll the plan"),
                    ("g / G", "top or end of the plan"),
                    ("r", "allow restoring while the provider runs"),
                    ("x", "allow config that executes code"),
                    ("c", "allow login credentials"),
                    ("o", "allow overwriting existing files"),
                    ("enter / y", "restore, when nothing is refused"),
                    ("esc", "back to the mapping"),
                    ("q", "quit without writing"),
                ],
            );
        }
    }

    fn draw_done(&mut self, f: &mut Frame) {
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        f.render_widget(theme::header(theme, "Restore complete", ""), top);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(r) = &self.report {
            let failed_style = if r.failed.is_empty() {
                theme.muted()
            } else {
                theme.danger()
            };
            lines.push(Line::from(Span::styled(
                format!("written  {}", r.written),
                theme.success(),
            )));
            lines.push(Line::from(Span::styled(
                format!("merged   {}", r.merged),
                theme.info(),
            )));
            lines.push(Line::from(Span::styled(
                format!("skipped  {}", r.skipped),
                theme.muted(),
            )));
            lines.push(Line::from(Span::styled(
                format!("failed   {}", r.failed.len()),
                failed_style,
            )));
            for err in &r.failed {
                lines.push(Line::from(Span::styled(
                    format!("{} {}", theme.icon(Icon::Warning), sanitize(err)),
                    theme.danger(),
                )));
            }
        }
        f.render_widget(
            Paragraph::new(lines)
                .block(theme.panel("Result"))
                .wrap(Wrap { trim: false }),
            body,
        );
        f.render_widget(theme::footer(theme, &[("enter", "exit")]), bottom);
    }
}

pub fn run_restore(
    backup_path: &Path,
    claude_dir: &Path,
    flags: super::RestoreFlags,
) -> Result<()> {
    let passphrase = crate::cli::resolve_decrypt_passphrase(backup_path)?;
    let archive = backup_path.to_path_buf();
    let stage_passphrase = passphrase.clone();
    let finished = with_terminal(|t| {
        let Some(staged) = super::loading::run_with_loading(
            t,
            "Verifying and extracting the archive",
            (ProviderId::Claude, "Archive"),
            move || crate::archive::stage(&archive, &stage_passphrase),
        )?
        else {
            return Ok(None);
        };
        let staged = staged?;
        if !flags.no_verify_checksum && !staged.report.ok() {
            bail!("archive verification failed; pass --no-verify-checksum to override");
        }
        let mut app = RestoreApp::new(staged.manifest, backup_path, claude_dir, flags);
        app.passphrase = passphrase;
        app.provider_homes = crate::cli::default_provider_homes(claude_dir);
        app.run(t)?;
        Ok(Some(app))
    })?;
    let Some(mut app) = finished else {
        println!("Cancelled.");
        return Ok(());
    };

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
