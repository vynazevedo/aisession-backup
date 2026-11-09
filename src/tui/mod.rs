pub mod backup_ui;
pub mod groups;
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
    plan_scroll: theme::Scroll,
    pub picker: Option<GroupPicker>,
    picker_refs: Vec<(usize, usize)>,
    picker_all_providers: bool,
    pub restore_input: String,
    pub plan: Option<Plan>,
    pub outcome: Option<HomeOutcome>,
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
            plan_scroll: theme::Scroll::default(),
            picker: None,
            picker_refs: Vec::new(),
            picker_all_providers: false,
            restore_input: String::new(),
            plan: None,
            outcome: None,
        }
    }

    pub fn from_env(env: &LocateEnv, opts: &ScanOptions) -> Self {
        let mut entries = Vec::new();
        for prov in provider::registry() {
            let home = prov.locate(env);
            let items = match &home {
                Some(h) if h.exists() => {
                    prov.scan(h, opts).map(|inv| inv.items).unwrap_or_default()
                }
                _ => Vec::new(),
            };
            let selected = vec![false; items.len()];
            entries.push(ProviderEntry {
                id: prov.id(),
                display_name: prov.display_name().to_string(),
                home,
                items,
                selected,
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
            KeyCode::Esc => self.screen = HomeScreen::Providers,
            KeyCode::Char('q') => {
                self.outcome = Some(HomeOutcome::Cancelled);
                self.screen = HomeScreen::Done;
            }
            KeyCode::Char('b') => {
                if self.total_selected_items() > 0 {
                    self.screen = HomeScreen::ConfirmBackup;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if !picker.handle_key(code) {
                    self.screen = HomeScreen::Providers;
                }
            }
            _ => {
                picker.handle_key(code);
                self.sync_picker_selection();
            }
        }
    }

    fn handle_confirm_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.screen = HomeScreen::Providers;
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
            }
            KeyCode::Char(c) => {
                self.restore_input.push(c);
            }
            KeyCode::Enter => {
                let trimmed = self.restore_input.trim();
                if !trimmed.is_empty() {
                    self.outcome = Some(HomeOutcome::RestoreArchive(PathBuf::from(trimmed)));
                    self.screen = HomeScreen::Done;
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
                ("q", "quit"),
            ],
            HomeScreen::ItemPicker => {
                let mut keys = groups::PICKER_HELP_KEYS.to_vec();
                keys.push(("b", "review the backup plan"));
                keys
            }
            _ => vec![
                ("j/k", "scroll the plan"),
                ("g / G", "top or end of the plan"),
                ("enter / y", "write, when nothing is refused"),
                ("esc", "go back without writing"),
                ("q", "quit without writing"),
            ],
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let theme = theme::current();
        let [top, body, bottom] = theme::frame_layout(f.area());
        let selected = self.total_selected_items();
        match self.screen {
            HomeScreen::Providers => {
                f.render_widget(
                    theme::header(theme, "Home", &format!("{selected} item(s) selected")),
                    top,
                );
                let rows: Vec<Row> = self
                    .providers
                    .iter()
                    .map(|p| {
                        let detected = p.home.as_ref().is_some_and(|h| h.exists());
                        let status = if detected {
                            Span::styled("detected", theme.success())
                        } else {
                            Span::styled("not found", theme.muted())
                        };
                        let location = p
                            .home
                            .as_ref()
                            .filter(|_| detected)
                            .map(|h| sanitize(&h.display().to_string()))
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
                            Cell::from(Span::styled(human_size(p.total_bytes()), theme.muted())),
                            Cell::from(Span::styled(location, theme.muted())),
                        ])
                    })
                    .collect();
                let table = Table::new(
                    rows,
                    [
                        Constraint::Length(17),
                        Constraint::Length(10),
                        Constraint::Length(10),
                        Constraint::Length(10),
                        Constraint::Min(10),
                    ],
                )
                .header(
                    Row::new(["Provider", "Status", "Selected", "Size", "Location"])
                        .style(theme.muted().add_modifier(Modifier::BOLD)),
                )
                .block(theme.panel("Providers"))
                .row_highlight_style(theme.selected_row())
                .highlight_symbol("> ");
                f.render_stateful_widget(table, body, &mut self.provider_list_state);
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
                f.render_widget(theme::footer(theme, &keys), bottom);
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
                        &format!("{scope}  |  {selected} item(s) selected"),
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
                keys.push(("esc", "back"));
                f.render_widget(theme::footer(theme, &keys), bottom);
            }
            HomeScreen::ConfirmBackup => {
                f.render_widget(
                    theme::header(theme, "Backup plan", &format!("{selected} item(s)")),
                    top,
                );
                theme::render_plan(f, body, theme, self.plan.as_ref(), &mut self.plan_scroll);
                let blocked = self.plan.as_ref().is_some_and(Plan::has_refusals);
                let mut keys = theme::PLAN_KEYS.to_vec();
                if !blocked {
                    keys.insert(0, ("enter", "write backup"));
                }
                keys.extend([("esc", "back"), ("q", "quit")]);
                f.render_widget(theme::footer(theme, &keys), bottom);
            }
            HomeScreen::RestoreInput => {
                f.render_widget(theme::header(theme, "Restore", "choose an archive"), top);
                let lines = vec![
                    Line::from(Span::styled(
                        "Path to a backup archive (.tar.gz or .tar.gz.age)",
                        theme.muted(),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", theme.icon(theme::Icon::Archive)),
                            theme.accent(),
                        ),
                        Span::styled(sanitize(&self.restore_input), theme.strong()),
                        Span::styled("_", theme.accent()),
                    ]),
                ];
                f.render_widget(Paragraph::new(lines).block(theme.panel("Archive")), body);
                f.render_widget(
                    theme::footer(theme, &[("enter", "open"), ("esc", "back")]),
                    bottom,
                );
            }
            HomeScreen::Done => {}
        }
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
    let mut state = HomeState::from_env(&env, &ScanOptions::default());
    let output = PathBuf::from(format!(
        "aisession-backup-{}.tar.gz",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));

    with_terminal(|terminal| loop {
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
            break Ok(());
        }
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                state.handle_key(key.code);
            }
        }
    })?;

    match state.outcome {
        Some(HomeOutcome::BackupConfirmed) => {
            let req = state.build_backup_request(output, claude_dir);
            let report = engine::run_backup(&req)?;
            print!(
                "{}",
                crate::ui::format_backup_report(&report, crate::ui::color_stdout())
            );
            Ok(())
        }
        Some(HomeOutcome::RestoreArchive(path)) => {
            run_restore(&path, claude_dir, RestoreFlags::default())
        }
        _ => {
            println!("Cancelled.");
            Ok(())
        }
    }
}
