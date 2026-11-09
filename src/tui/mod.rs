pub mod art;
pub mod backup_ui;
pub mod dashboard;
pub mod groups;
pub mod loading;
pub mod restore_ui;
pub mod theme;

use std::io::{self, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use anyhow::{bail, Result};
use ratatui::crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::engine::{self, BackupRequest, ClaudeSelection, Passphrase, ProviderSelection};
use crate::plan::Plan;
use crate::provider::{self, Item, LocateEnv, ProviderId, ScanOptions};
use crate::ui::{human_size, sanitize};

pub use backup_ui::{run_backup, BackupApp, BackupStep};
pub use groups::{GroupPicker, Mark, PickRow};
pub use restore_ui::{run_restore, MapRow, RestoreApp, RestoreStep};
pub use theme::{IconSet, Theme};

#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreFlags {
    pub dry_run: bool,
    pub rewrite_tool_paths: bool,
    pub restore_history: bool,
    pub no_verify_checksum: bool,
    pub apply_config: bool,
    pub overwrite_config: bool,
    pub allow_exec_config: bool,
    pub allow_credentials: bool,
    pub allow_running: bool,
}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK_ONCE: Once = Once::new();

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn restore_terminal_raw() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

pub fn install_panic_hook() {
    PANIC_HOOK_ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal_raw();
            prev(info);
        }));
    });
}

pub fn check_tty(stdin_tty: bool, stdout_tty: bool) -> Result<()> {
    if !stdin_tty || !stdout_tty {
        bail!("interactive TUI requires a TTY");
    }
    Ok(())
}

pub fn ensure_tty() -> Result<()> {
    check_tty(io::stdin().is_terminal(), io::stdout().is_terminal())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_raw();
    }
}

fn setup_terminal() -> Result<Tui> {
    ensure_tty()?;
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn with_terminal<T>(f: impl FnOnce(&mut Tui) -> Result<T>) -> Result<T> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let result = f(&mut terminal);
    let _ = restore_terminal(&mut terminal);
    result
}

pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
}

#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub id: ProviderId,
    pub display_name: String,
    pub home: Option<PathBuf>,
    pub items: Vec<Item>,
    pub selected: Vec<bool>,
    pub scan_error: Option<String>,
}

impl ProviderEntry {
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&b| b).count()
    }

    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(Item::total_size).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeScreen {
    Providers,
    ItemPicker,
    ConfirmBackup,
    RestoreInput,
    ConvertPlan,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeOutcome {
    BackupConfirmed,
    RestoreArchive(PathBuf),
    Cancelled,
}

pub struct HomeState {
    pub providers: Vec<ProviderEntry>,
    pub screen: HomeScreen,
    pub provider_cursor: usize,
    pub provider_list_state: TableState,
    pub show_help: bool,
    pub frame: usize,
    plan_scroll: theme::Scroll,
    pub picker: Option<GroupPicker>,
    picker_refs: Vec<(usize, usize)>,
    picker_all_providers: bool,
    pub restore_input: String,
    pub plan: Option<Plan>,
    pub outcome: Option<HomeOutcome>,
    pub status: Option<(String, bool)>,
    pub conversion: Option<Conversion>,
    plan_return: HomeScreen,
}

pub struct Conversion {
    pub label: String,
    pub request: crate::convert::ConvertRequest,
    pub prepared: Option<crate::convert::PreparedConversion>,
    pub error: Option<String>,
}

impl Conversion {
    fn refresh(&mut self) {
        match crate::convert::prepare(&self.request) {
            Ok(prepared) => {
                self.prepared = Some(prepared);
                self.error = None;
            }
            Err(err) => {
                self.prepared = None;
                self.error = Some(format!("{err:#}"));
            }
        }
    }
}

impl HomeState {
    pub fn new(providers: Vec<ProviderEntry>) -> Self {
        let mut provider_list_state = TableState::default();
        if !providers.is_empty() {
            provider_list_state.select(Some(0));
        }
        Self {
            providers,
            screen: HomeScreen::Providers,
            provider_cursor: 0,
            provider_list_state,
            show_help: false,
            frame: 0,
            plan_scroll: theme::Scroll::default(),
            picker: None,
            picker_refs: Vec::new(),
            picker_all_providers: false,
            restore_input: String::new(),
            plan: None,
            outcome: None,
            status: None,
            conversion: None,
            plan_return: HomeScreen::Providers,
        }
    }

    pub fn from_env(env: &LocateEnv, opts: &ScanOptions) -> Self {
        let mut entries = Vec::new();
        for prov in provider::registry() {
            let home = prov.locate(env);
            let (items, scan_error) = match &home {
                Some(h) if h.exists() => match prov.scan(h, opts) {
                    Ok(inv) => (inv.items, None),
                    Err(err) => (Vec::new(), Some(format!("{err:#}"))),
                },
                _ => (Vec::new(), None),
            };
            let selected = vec![false; items.len()];
            entries.push(ProviderEntry {
                id: prov.id(),
                display_name: prov.display_name().to_string(),
                home,
                items,
                selected,
                scan_error,
            });
        }
        Self::new(entries)
    }

    pub fn total_selected_items(&self) -> usize {
        self.providers
            .iter()
            .map(ProviderEntry::selected_count)
            .sum()
    }

    pub fn open_picker(&mut self, only_provider: Option<usize>) {
        let mut entries = Vec::new();
        let mut selected = Vec::new();
        let mut refs = Vec::new();
        for (provider_index, provider) in self.providers.iter().enumerate() {
            if only_provider.is_some_and(|wanted| wanted != provider_index) {
                continue;
            }
            for (item_index, item) in provider.items.iter().enumerate() {
                entries.push(crate::grouping::Entry {
                    id: item.id.clone(),
                    label: item.label.clone(),
                    cwd: item.cwd.clone(),
                    provider: provider.id.as_str().to_string(),
                    detail: String::new(),
                    size: item.total_size(),
                    updated_at: item.updated_at,
                });
                selected.push(provider.selected.get(item_index).copied().unwrap_or(false));
                refs.push((provider_index, item_index));
            }
        }
        self.picker = Some(GroupPicker::new(entries, selected));
        self.picker_refs = refs;
        self.picker_all_providers = only_provider.is_none();
        self.screen = HomeScreen::ItemPicker;
    }

    pub fn convertible_current(&self) -> Option<(PathBuf, ProviderId, ProviderId)> {
        let picker = self.picker.as_ref()?;
        let Some(PickRow::Entry { entry, .. }) = picker.current() else {
            return None;
        };
        let &(provider_index, item_index) = self.picker_refs.get(entry)?;
        let provider = self.providers.get(provider_index)?;
        let item = provider.items.get(item_index)?;
        if item.kind != provider::ItemKind::Session {
            return None;
        }
        let to = match provider.id {
            ProviderId::Claude => ProviderId::Codex,
            ProviderId::Codex => ProviderId::Claude,
            _ => return None,
        };
        let home = provider.home.as_ref()?;
        let file = item.files.iter().find(|file| {
            file.rel.ends_with(".jsonl")
                && (provider.id == ProviderId::Codex
                    || file.rel.ends_with(&format!("{}.jsonl", item.id)))
        })?;
        Some((home.join(&file.rel), provider.id, to))
    }

    fn sync_picker_selection(&mut self) {
        let Some(picker) = &self.picker else {
            return;
        };
        for (position, &(provider, item)) in self.picker_refs.iter().enumerate() {
            let value = picker.selected.get(position).copied().unwrap_or(false);
            if let Some(flag) = self
                .providers
                .get_mut(provider)
                .and_then(|entry| entry.selected.get_mut(item))
            {
                *flag = value;
            }
        }
    }

    pub fn set_plan(&mut self, plan: Plan) {
        self.plan = Some(plan);
    }

    pub fn build_backup_request(
        &self,
        output: PathBuf,
        default_claude_dir: &Path,
    ) -> BackupRequest {
        let mut claude = None;
        let mut providers = Vec::new();
        for entry in &self.providers {
            let chosen: Vec<String> = entry
                .items
                .iter()
                .enumerate()
                .filter(|(i, _)| entry.selected.get(*i).copied().unwrap_or(false))
                .map(|(_, item)| item.id.clone())
                .collect();
            if chosen.is_empty() {
                continue;
            }
            if entry.id == ProviderId::Claude {
                claude = Some(ClaudeSelection {
                    claude_dir: entry
                        .home
                        .clone()
                        .unwrap_or_else(|| default_claude_dir.to_path_buf()),
                    session_ids: Some(chosen),
                    match_path: None,
                    include_extras: false,
                    include_config: false,
                    include_credentials: false,
                });
            } else if let Some(home) = &entry.home {
                providers.push(ProviderSelection {
                    provider: entry.id,
                    home: home.clone(),
                    item_ids: Some(chosen),
                    include_state: false,
                    include_credentials: false,
                });
            }
        }
        BackupRequest {
            output,
            overwrite: false,
            claude,
            providers,
            passphrase: Passphrase::None,
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        if self.show_help {
            self.show_help = false;
            return;
        }
        let typing = match self.screen {
            HomeScreen::ItemPicker => self.picker.as_ref().is_some_and(|p| p.filtering),
            HomeScreen::RestoreInput | HomeScreen::Done => true,
            HomeScreen::ConvertPlan => false,
            _ => false,
        };
        if code == KeyCode::Char('?') && !typing {
            self.show_help = true;
            return;
        }
        match self.screen {
            HomeScreen::Providers => self.handle_providers_key(code),
            HomeScreen::ItemPicker => self.handle_item_picker_key(code),
            HomeScreen::ConfirmBackup => self.handle_confirm_key(code),
            HomeScreen::RestoreInput => self.handle_restore_input_key(code),
            HomeScreen::ConvertPlan => self.handle_convert_key(code),
            HomeScreen::Done => {}
        }
    }

    fn handle_providers_key(&mut self, code: KeyCode) {
        let len = self.providers.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.outcome = Some(HomeOutcome::Cancelled);
                self.screen = HomeScreen::Done;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if len > 0 {
                    self.provider_cursor = (self.provider_cursor + 1) % len;
                    self.provider_list_state.select(Some(self.provider_cursor));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if len > 0 {
                    self.provider_cursor = (self.provider_cursor + len - 1) % len;
                    self.provider_list_state.select(Some(self.provider_cursor));
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self.provider_cursor < len {
                    self.open_picker(Some(self.provider_cursor));
                }
            }
            KeyCode::Char('d') => self.open_picker(None),
            KeyCode::Char(' ') => {
                if let Some(entry) = self.providers.get_mut(self.provider_cursor) {
                    let all_selected =
                        !entry.selected.is_empty() && entry.selected.iter().all(|&b| b);
                    entry.selected.fill(!all_selected);
                }
            }
            KeyCode::Char('a') => {
                for entry in &mut self.providers {
                    entry.selected.fill(true);
                }
            }
            KeyCode::Char('n') => {
                for entry in &mut self.providers {
                    entry.selected.fill(false);
                }
            }
            KeyCode::Char('b') if self.total_selected_items() > 0 => {
                self.plan_return = HomeScreen::Providers;
                self.screen = HomeScreen::ConfirmBackup;
            }
            KeyCode::Char('r') => {
                self.screen = HomeScreen::RestoreInput;
            }
            _ => {}
        }
    }

    fn handle_item_picker_key(&mut self, code: KeyCode) {
        let Some(picker) = self.picker.as_mut() else {
            self.screen = HomeScreen::Providers;
            return;
        };
        if picker.filtering {
            picker.handle_key(code);
            self.sync_picker_selection();
            return;
        }
        match code {
            KeyCode::Esc if !picker.filter.is_empty() => {
                picker.handle_key(code);
            }
            KeyCode::Esc => self.screen = HomeScreen::Providers,
            KeyCode::Char('q') => {
                self.outcome = Some(HomeOutcome::Cancelled);
                self.screen = HomeScreen::Done;
            }
            KeyCode::Char('b') => {
                if self.total_selected_items() > 0 {
                    self.plan_return = HomeScreen::ItemPicker;
                    self.screen = HomeScreen::ConfirmBackup;
                }
            }
            KeyCode::Char('t') => self.start_conversion(),
            _ => {
                picker.handle_key(code);
                self.sync_picker_selection();
            }
        }
    }

    fn start_conversion(&mut self) {
        let Some((source, from, to)) = self.convertible_current() else {
            self.status = Some((
                "Move the cursor to a Claude Code or Codex session to convert it.".into(),
                false,
            ));
            return;
        };
        let Some(target_home) = self
            .providers
            .iter()
            .find(|p| p.id == to)
            .and_then(|p| p.home.clone())
        else {
            self.status = Some((
                format!("{} was not found on this machine.", to.as_str()),
                true,
            ));
            return;
        };
        let label = self
            .picker
            .as_ref()
            .and_then(|picker| match picker.current() {
                Some(PickRow::Entry { entry, .. }) => Some(picker.entry_label(entry)),
                _ => None,
            })
            .unwrap_or_default();
        let mut conversion = Conversion {
            label,
            request: crate::convert::ConvertRequest {
                source,
                from: Some(from),
                to,
                target_home,
                mappings: std::collections::BTreeMap::new(),
                allow_running: false,
            },
            prepared: None,
            error: None,
        };
        conversion.refresh();
        self.conversion = Some(conversion);
        self.plan_scroll = theme::Scroll::default();
        self.status = None;
        self.screen = HomeScreen::ConvertPlan;
    }

    fn handle_convert_key(&mut self, code: KeyCode) {
        let Some(conversion) = self.conversion.as_mut() else {
            self.screen = HomeScreen::ItemPicker;
            return;
        };
        match code {
            KeyCode::Esc => {
                self.conversion = None;
                self.screen = HomeScreen::ItemPicker;
            }
            KeyCode::Char('q') => {
                self.outcome = Some(HomeOutcome::Cancelled);
                self.screen = HomeScreen::Done;
            }
            KeyCode::Char('r') => {
                conversion.request.allow_running = !conversion.request.allow_running;
                conversion.refresh();
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let ready = conversion
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| !prepared.plan.has_refusals());
                if !ready {
                    return;
                }
                let Some(prepared) = conversion.prepared.take() else {
                    return;
                };
                let label = conversion.label.clone();
                self.status = Some(match crate::convert::apply(prepared) {
                    Ok(report) if report.written == 0 => (
                        format!(
                            "\"{label}\" was already converted: {}",
                            report.summary.resume
                        ),
                        false,
                    ),
                    Ok(report) => (
                        format!(
                            "Converted \"{label}\". Resume: {}{}",
                            report.summary.resume,
                            report
                                .journal_id
                                .map(|id| format!("   Undo: aisession-backup rollback {id}"))
                                .unwrap_or_default()
                        ),
                        false,
                    ),
                    Err(err) => (format!("Conversion failed: {err:#}"), true),
                });
                self.conversion = None;
                self.screen = HomeScreen::ItemPicker;
            }
            _ => {
                self.plan_scroll.handle_key(code);
            }
        }
    }

    fn handle_confirm_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.screen = self.plan_return;
            }
            KeyCode::Char('q') => {
                self.outcome = Some(HomeOutcome::Cancelled);
                self.screen = HomeScreen::Done;
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let has_refusals = self.plan.as_ref().map(Plan::has_refusals).unwrap_or(false);
                if !has_refusals {
                    self.outcome = Some(HomeOutcome::BackupConfirmed);
                    self.screen = HomeScreen::Done;
                }
            }
            _ => {
                self.plan_scroll.handle_key(code);
            }
        }
    }

    fn handle_restore_input_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.screen = HomeScreen::Providers;
            }
            KeyCode::Backspace => {
                self.restore_input.pop();
                self.status = None;
            }
            KeyCode::Char(c) => {
                self.restore_input.push(c);
                self.status = None;
            }
            KeyCode::Enter => {
                let trimmed = self.restore_input.trim();
                if trimmed.is_empty() {
                    return;
                }
                let path = crate::cli::expand_path(trimmed);
                if path.is_file() {
                    self.status = None;
                    self.outcome = Some(HomeOutcome::RestoreArchive(path));
                    self.screen = HomeScreen::Done;
                } else {
                    let reason = if path.exists() {
                        "is not a file"
                    } else {
                        "was not found"
                    };
                    self.status = Some((format!("{} {reason}", sanitize(trimmed)), true));
                }
            }
            _ => {}
        }
    }

    fn help_keys(&self) -> Vec<(&'static str, &'static str)> {
        match self.screen {
            HomeScreen::Providers => vec![
                ("j/k", "move between providers"),
                ("enter", "open the items of the provider"),
                ("d", "open every provider grouped by directory"),
                ("space", "select or clear the whole provider"),
                ("a / n", "select or clear everything"),
                ("b", "review the backup plan"),
                ("r", "restore an archive"),
                ("q / esc", "quit"),
            ],
            HomeScreen::ItemPicker => {
                let mut keys = groups::PICKER_HELP_KEYS.to_vec();
                keys.push(("b", "review the backup plan"));
                keys.push(("t", "convert the session to the other tool"));
                keys
            }
            HomeScreen::ConvertPlan => vec![
                ("enter / y", "convert, when nothing is refused"),
                ("r", "allow writing while the target tool runs"),
                ("j/k", "scroll the plan"),
                ("esc", "back to the list"),
                ("q", "quit without writing"),
            ],
            _ => vec![
                ("j/k", "scroll the plan"),
                ("g / G", "top or end of the plan"),
                ("enter / y", "write, when nothing is refused"),
                ("esc", "go back without writing"),
                ("q", "quit without writing"),
            ],
        }
    }

    fn provider_rows(&self, theme: &Theme, location_width: usize) -> Vec<Row<'static>> {
        self.providers
            .iter()
            .map(|p| {
                let detected = p.home.as_ref().is_some_and(|h| h.exists());
                let status = if p.scan_error.is_some() {
                    Span::styled("scan failed", theme.danger())
                } else if detected && p.items.is_empty() {
                    Span::styled("empty", theme.warning())
                } else if detected {
                    Span::styled("detected", theme.success())
                } else {
                    Span::styled("not found", theme.muted())
                };
                let location = p
                    .home
                    .as_ref()
                    .filter(|_| detected)
                    .map(|h| theme::truncate_left(&theme::home_relative(h), location_width))
                    .unwrap_or_default();
                let count_style = if p.selected_count() > 0 {
                    theme.accent()
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(Span::styled(
                        sanitize(&p.display_name),
                        theme.provider(p.id.as_str()),
                    )),
                    Cell::from(status),
                    Cell::from(Span::styled(
                        format!("{}/{}", p.selected_count(), p.items.len()),
                        count_style,
                    )),
                    Cell::from(Span::styled(
                        format!("{:>9}", human_size(p.total_bytes())),
                        theme.muted(),
                    )),
                    Cell::from(Span::styled(location, theme.muted())),
                ])
            })
            .collect()
    }

    fn status_area(&self, body: Rect) -> (Rect, Option<Rect>) {
        if self.status.is_none() || body.height < 4 {
            return (body, None);
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(body);
        (chunks[0], Some(chunks[1]))
    }

    fn render_status(&self, f: &mut Frame, area: Option<Rect>, theme: &Theme) {
        if let (Some(area), Some((message, danger))) = (area, &self.status) {
            let style = if *danger {
                theme.danger()
            } else {
                theme.success()
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(sanitize(message), style)))
                    .wrap(ratatui::widgets::Wrap { trim: true }),
                area,
            );
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        let selected = self.total_selected_items();
        let (body, status_area) = self.status_area(body);
        match self.screen {
            HomeScreen::Providers => {
                f.render_widget(
                    theme::header(
                        theme,
                        "Home",
                        &format!("{} selected", theme::plural(selected, "item")),
                    ),
                    top,
                );
                let (table_area, overview_area) = if body.width >= 110 {
                    let columns = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Min(60), Constraint::Length(46)])
                        .spacing(1)
                        .split(body);
                    (columns[0], Some(columns[1]))
                } else {
                    (body, None)
                };
                let location_width = usize::from(table_area.width)
                    .saturating_sub(17 + 12 + 10 + 10 + 8)
                    .max(8);
                let table = Table::new(
                    self.provider_rows(theme, location_width),
                    [
                        Constraint::Length(17),
                        Constraint::Length(12),
                        Constraint::Length(10),
                        Constraint::Length(10),
                        Constraint::Min(8),
                    ],
                )
                .header(
                    Row::new(["Provider", "Status", "Selected", "     Size", "Location"])
                        .style(theme.muted().add_modifier(Modifier::BOLD)),
                )
                .block(theme.panel("Providers"))
                .row_highlight_style(theme.selected_row())
                .highlight_symbol("> ");
                f.render_stateful_widget(table, table_area, &mut self.provider_list_state);
                if let Some(area) = overview_area {
                    dashboard::render_overview(
                        f,
                        area,
                        theme,
                        &self.providers,
                        chrono::Utc::now(),
                        self.frame,
                    );
                }
                let mut keys = vec![
                    ("enter", "items"),
                    ("d", "by directory"),
                    ("space", "select"),
                    ("r", "restore"),
                ];
                if selected > 0 {
                    keys.insert(0, ("b", "backup"));
                }
                keys.extend([("?", "help"), ("q", "quit")]);
                theme::render_footer(f, bottom, theme, &keys);
            }
            HomeScreen::ItemPicker => {
                let scope = if self.picker_all_providers {
                    "all providers".to_string()
                } else {
                    self.providers
                        .get(self.provider_cursor)
                        .map(|p| sanitize(&p.display_name))
                        .unwrap_or_default()
                };
                f.render_widget(
                    theme::header(
                        theme,
                        "Select",
                        &format!("{scope}  |  {} selected", theme::plural(selected, "item")),
                    ),
                    top,
                );
                if let Some(picker) = self.picker.as_mut() {
                    picker.render(f, body, theme, "Directories");
                }
                let mut keys = groups::PICKER_KEYS.to_vec();
                if selected > 0 {
                    keys.insert(0, ("b", "backup"));
                }
                keys.push((
                    "esc",
                    self.picker
                        .as_ref()
                        .map(GroupPicker::esc_label)
                        .unwrap_or("back"),
                ));
                match self.convertible_current() {
                    Some((_, _, ProviderId::Codex)) => keys.insert(0, ("t", "convert to codex")),
                    Some((_, _, _)) => keys.insert(0, ("t", "convert to claude")),
                    None => {}
                }
                keys.push(("q", "quit"));
                theme::render_footer(f, bottom, theme, &keys);
            }
            HomeScreen::ConfirmBackup => {
                f.render_widget(
                    theme::header(theme, "Backup plan", &theme::plural(selected, "item")),
                    top,
                );
                theme::render_plan(f, body, theme, self.plan.as_ref(), &mut self.plan_scroll);
                let blocked = self.plan.as_ref().is_some_and(Plan::has_refusals);
                let mut keys = theme::PLAN_KEYS.to_vec();
                if !blocked {
                    keys.insert(0, ("enter", "write backup"));
                }
                keys.extend([("esc", "back"), ("?", "help"), ("q", "quit")]);
                theme::render_footer(f, bottom, theme, &keys);
            }
            HomeScreen::ConvertPlan => {
                let (title, context, allow) = match &self.conversion {
                    Some(c) => (
                        format!("Convert to {}", c.request.to.as_str()),
                        format!("\"{}\"", c.label),
                        c.request.allow_running,
                    ),
                    None => (String::from("Convert"), String::new(), false),
                };
                f.render_widget(theme::header(theme, &title, &context), top);
                let plan = self
                    .conversion
                    .as_ref()
                    .and_then(|c| c.prepared.as_ref())
                    .map(|p| p.plan.clone());
                let error = self.conversion.as_ref().and_then(|c| c.error.clone());
                if let Some(error) = error {
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(sanitize(&error), theme.danger())))
                            .wrap(ratatui::widgets::Wrap { trim: true })
                            .block(theme.panel("Cannot convert")),
                        body,
                    );
                } else {
                    theme::render_plan(f, body, theme, plan.as_ref(), &mut self.plan_scroll);
                }
                let blocked = plan.as_ref().is_none_or(Plan::has_refusals);
                let running = if allow {
                    "allow running ON"
                } else {
                    "allow running off"
                };
                let mut keys = vec![("r", running)];
                if !blocked {
                    keys.insert(0, ("enter", "convert"));
                }
                keys.extend([("esc", "back"), ("?", "help"), ("q", "quit")]);
                theme::render_footer(f, bottom, theme, &keys);
            }
            HomeScreen::RestoreInput => {
                f.render_widget(theme::header(theme, "Restore", "choose an archive"), top);
                let field_width = usize::from(body.width).saturating_sub(8).max(4);
                let shown = theme::truncate_left(&sanitize(&self.restore_input), field_width);
                let lines = vec![
                    Line::from(Span::styled(
                        "Path to a backup archive (.tar.gz or .tar.gz.age). ~ is expanded.",
                        theme.muted(),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", theme.icon(theme::Icon::Archive)),
                            theme.accent(),
                        ),
                        Span::styled(shown, theme.strong()),
                        Span::styled("_", theme.accent()),
                    ]),
                ];
                f.render_widget(Paragraph::new(lines).block(theme.panel("Archive")), body);
                theme::render_footer(f, bottom, theme, &[("enter", "open"), ("esc", "back")]);
            }
            HomeScreen::Done => {}
        }
        self.render_status(f, status_area, theme);
        if self.show_help {
            theme::render_help(f, theme, "Keys", &self.help_keys());
        }
    }
}

pub fn run_home(claude_dir: &Path) -> Result<()> {
    let mut env = LocateEnv::from_system();
    if claude_dir.exists() {
        env.overrides
            .insert(ProviderId::Claude, claude_dir.to_path_buf());
    }
    let output = PathBuf::from(format!(
        "aisession-backup-{}.tar.gz",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let mut kept: Option<HomeState> = None;
    loop {
        let finished = with_terminal(|terminal| {
            let mut state = match kept.take() {
                Some(state) => state,
                None => {
                    let Some(entries) =
                        loading::scan_providers(terminal, &env, &ScanOptions::default())?
                    else {
                        return Ok(None);
                    };
                    HomeState::new(entries)
                }
            };
            loop {
                if state.screen == HomeScreen::ConfirmBackup && state.plan.is_none() {
                    let req = state.build_backup_request(output.clone(), claude_dir);
                    state.plan_scroll = theme::Scroll::default();
                    match engine::plan_backup(&req) {
                        Ok(plan) => state.set_plan(plan),
                        Err(err) => state.set_plan(Plan::failed(format!("{err:#}"))),
                    }
                } else if state.screen != HomeScreen::ConfirmBackup {
                    state.plan = None;
                }
                terminal.draw(|f| state.draw(f))?;
                if state.screen == HomeScreen::Done {
                    return Ok(Some(state));
                }
                if !event::poll(std::time::Duration::from_millis(120))? {
                    state.frame = state.frame.wrapping_add(1);
                    continue;
                }
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        state.handle_key(key.code);
                    }
                }
            }
        })?;
        let Some(mut state) = finished else {
            println!("Cancelled.");
            return Ok(());
        };
        match state.outcome.take() {
            Some(HomeOutcome::BackupConfirmed) => {
                let req = state.build_backup_request(output, claude_dir);
                let report = engine::run_backup(&req)?;
                print!(
                    "{}",
                    crate::ui::format_backup_report(&report, crate::ui::color_stdout())
                );
                return Ok(());
            }
            Some(HomeOutcome::RestoreArchive(path)) => {
                if restore_ui::run_restore_from_home(&path, claude_dir, RestoreFlags::default())? {
                    state.restore_input = path.display().to_string();
                    state.screen = HomeScreen::RestoreInput;
                    kept = Some(state);
                    continue;
                }
                return Ok(());
            }
            _ => {
                println!("Cancelled.");
                return Ok(());
            }
        }
    }
}
