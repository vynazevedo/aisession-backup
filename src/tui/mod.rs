pub mod backup_ui;
pub mod restore_ui;

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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::engine::{self, BackupRequest, ClaudeSelection, Passphrase, ProviderSelection};
use crate::plan::Plan;
use crate::provider::{self, Item, LocateEnv, ProviderId, ScanOptions};
use crate::ui::{human_size, sanitize};

pub use backup_ui::{run_backup, BackupApp, BackupStep};
pub use restore_ui::{run_restore, MapRow, RestoreApp, RestoreStep};

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
    pub provider_list_state: ListState,
    pub item_cursor: usize,
    pub item_list_state: ListState,
    pub visible_items: Vec<usize>,
    pub filter: String,
    pub filtering: bool,
    pub restore_input: String,
    pub plan: Option<Plan>,
    pub outcome: Option<HomeOutcome>,
}

impl HomeState {
    pub fn new(providers: Vec<ProviderEntry>) -> Self {
        let mut provider_list_state = ListState::default();
        if !providers.is_empty() {
            provider_list_state.select(Some(0));
        }
        Self {
            providers,
            screen: HomeScreen::Providers,
            provider_cursor: 0,
            provider_list_state,
            item_cursor: 0,
            item_list_state: ListState::default(),
            visible_items: Vec::new(),
            filter: String::new(),
            filtering: false,
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

    pub fn recompute_visible_items(&mut self) {
        let Some(entry) = self.providers.get(self.provider_cursor) else {
            self.visible_items.clear();
            self.item_list_state.select(None);
            return;
        };
        let needle = self.filter.to_lowercase();
        self.visible_items = entry
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if needle.is_empty() {
                    return true;
                }
                let label_match = item.label.to_lowercase().contains(&needle);
                let cwd_match = item
                    .cwd
                    .as_ref()
                    .map(|c| c.to_lowercase().contains(&needle))
                    .unwrap_or(false);
                let id_match = item.id.to_lowercase().contains(&needle);
                label_match || cwd_match || id_match
            })
            .map(|(i, _)| i)
            .collect();
        if self.visible_items.is_empty() {
            self.item_cursor = 0;
            self.item_list_state.select(None);
        } else {
            if self.item_cursor >= self.visible_items.len() {
                self.item_cursor = 0;
            }
            self.item_list_state.select(Some(self.item_cursor));
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
                    self.filter.clear();
                    self.filtering = false;
                    self.item_cursor = 0;
                    self.recompute_visible_items();
                    self.screen = HomeScreen::ItemPicker;
                }
            }
            KeyCode::Char(' ') => {
                if let Some(entry) = self.providers.get_mut(self.provider_cursor) {
                    let all_selected =
                        !entry.selected.is_empty() && entry.selected.iter().all(|&b| b);
                    for b in &mut entry.selected {
                        *b = !all_selected;
                    }
                }
            }
            KeyCode::Char('a') => {
                for entry in &mut self.providers {
                    for b in &mut entry.selected {
                        *b = true;
                    }
                }
            }
            KeyCode::Char('n') => {
                for entry in &mut self.providers {
                    for b in &mut entry.selected {
                        *b = false;
                    }
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
        if self.filtering {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.recompute_visible_items();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.recompute_visible_items();
                }
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                self.screen = HomeScreen::Providers;
            }
            KeyCode::Char('q') => {
                self.outcome = Some(HomeOutcome::Cancelled);
                self.screen = HomeScreen::Done;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.visible_items.is_empty() {
                    self.item_cursor = (self.item_cursor + 1) % self.visible_items.len();
                    self.item_list_state.select(Some(self.item_cursor));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.visible_items.is_empty() {
                    let n = self.visible_items.len();
                    self.item_cursor = (self.item_cursor + n - 1) % n;
                    self.item_list_state.select(Some(self.item_cursor));
                }
            }
            KeyCode::Char(' ') => {
                if let Some(&real_idx) = self.visible_items.get(self.item_cursor) {
                    if let Some(entry) = self.providers.get_mut(self.provider_cursor) {
                        if let Some(flag) = entry.selected.get_mut(real_idx) {
                            *flag = !*flag;
                        }
                    }
                }
            }
            KeyCode::Char('a') => {
                if let Some(entry) = self.providers.get_mut(self.provider_cursor) {
                    for &real_idx in &self.visible_items {
                        if let Some(flag) = entry.selected.get_mut(real_idx) {
                            *flag = true;
                        }
                    }
                }
            }
            KeyCode::Char('n') => {
                if let Some(entry) = self.providers.get_mut(self.provider_cursor) {
                    for &real_idx in &self.visible_items {
                        if let Some(flag) = entry.selected.get_mut(real_idx) {
                            *flag = false;
                        }
                    }
                }
            }
            KeyCode::Char('/') => {
                self.filtering = true;
            }
            KeyCode::Char('b') | KeyCode::Enter if self.total_selected_items() > 0 => {
                self.screen = HomeScreen::ConfirmBackup;
            }
            _ => {}
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
            _ => {}
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

    pub fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(f.area());

        match self.screen {
            HomeScreen::Providers => {
                let header = Paragraph::new(format!(
                    "claude-migrate Home | {} item(s) selected across providers",
                    self.total_selected_items()
                ))
                .style(Style::default().add_modifier(Modifier::BOLD));
                f.render_widget(header, chunks[0]);

                let items: Vec<ListItem> = self
                    .providers
                    .iter()
                    .map(|p| {
                        let status = match &p.home {
                            Some(h) if h.exists() => sanitize(&h.display().to_string()),
                            _ => "not detected".to_string(),
                        };
                        let line = format!(
                            "{:<16} [{}/{} selected] ({}) - {}",
                            sanitize(&p.display_name),
                            p.selected_count(),
                            p.items.len(),
                            human_size(p.total_bytes()),
                            status
                        );
                        ListItem::new(line)
                    })
                    .collect();
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title("Providers"))
                    .highlight_style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                f.render_stateful_widget(list, chunks[1], &mut self.provider_list_state);

                let help = Paragraph::new(
                    "enter/l items | space toggle provider | a all | n none | b backup | r restore | q quit",
                )
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
                f.render_widget(help, chunks[2]);
            }
            HomeScreen::ItemPicker => {
                let prov_name = self
                    .providers
                    .get(self.provider_cursor)
                    .map(|p| sanitize(&p.display_name))
                    .unwrap_or_default();
                let filter_str = if self.filtering {
                    format!(" | filter: {}_", sanitize(&self.filter))
                } else if !self.filter.is_empty() {
                    format!(" | filter: {}", sanitize(&self.filter))
                } else {
                    String::new()
                };
                let header = Paragraph::new(format!(
                    "Items - {prov_name} ({} total selected){filter_str}",
                    self.total_selected_items()
                ))
                .style(Style::default().add_modifier(Modifier::BOLD));
                f.render_widget(header, chunks[0]);

                let items: Vec<ListItem> =
                    if let Some(entry) = self.providers.get(self.provider_cursor) {
                        self.visible_items
                            .iter()
                            .filter_map(|&idx| {
                                let item = entry.items.get(idx)?;
                                let check = if entry.selected.get(idx).copied().unwrap_or(false) {
                                    "[x]"
                                } else {
                                    "[ ]"
                                };
                                let cwd = item
                                    .cwd
                                    .as_deref()
                                    .map(sanitize)
                                    .unwrap_or_else(|| "-".to_string());
                                let label = sanitize(&item.label);
                                Some(ListItem::new(format!(
                                    "{check} {label} ({cwd}, {})",
                                    human_size(item.total_size())
                                )))
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Sessions & Items"),
                    )
                    .highlight_style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                f.render_stateful_widget(list, chunks[1], &mut self.item_list_state);

                let help = Paragraph::new(
                    "space toggle | a all | n none | / filter | b/enter backup | esc/h back | q quit",
                )
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
                f.render_widget(help, chunks[2]);
            }
            HomeScreen::ConfirmBackup => {
                let header = Paragraph::new("Confirm Backup Plan")
                    .style(Style::default().add_modifier(Modifier::BOLD));
                f.render_widget(header, chunks[0]);

                let lines = render_plan_lines(self.plan.as_ref());
                let body = Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title("Plan"))
                    .wrap(Wrap { trim: false });
                f.render_widget(body, chunks[1]);

                let help = Paragraph::new("enter/y confirm | esc back | q quit")
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: true });
                f.render_widget(help, chunks[2]);
            }
            HomeScreen::RestoreInput => {
                let header = Paragraph::new("Restore Wizard - Enter Archive Path")
                    .style(Style::default().add_modifier(Modifier::BOLD));
                f.render_widget(header, chunks[0]);

                let body =
                    Paragraph::new(format!("Archive file: {}_", sanitize(&self.restore_input)))
                        .block(Block::default().borders(Borders::ALL).title("Archive"));
                f.render_widget(body, chunks[1]);

                let help = Paragraph::new("type path | enter open restore wizard | esc back")
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: true });
                f.render_widget(help, chunks[2]);
            }
            HomeScreen::Done => {}
        }
    }
}

pub(crate) fn render_plan_lines(plan: Option<&Plan>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(plan) = plan else {
        lines.push(Line::from("No plan available."));
        return lines;
    };
    let summary = plan.summary();
    lines.push(Line::from(Span::styled(
        format!(
            "Summary: create={}, overwrite={}, merge={}, skip={}, refuse={} | size={}",
            summary.create,
            summary.overwrite,
            summary.merge,
            summary.skip,
            summary.refuse,
            human_size(summary.bytes)
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::raw("Risks: "),
        Span::styled(
            format!("executes_code={}", summary.executes_code),
            if summary.executes_code > 0 {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::raw(", "),
        Span::styled(
            format!("sensitive={}", summary.sensitive),
            if summary.sensitive > 0 {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
    ]));
    lines.push(Line::from(""));

    for warn in &plan.warnings {
        lines.push(Line::from(Span::styled(
            format!("! warning: {}", sanitize(warn)),
            Style::default().fg(Color::Yellow),
        )));
    }

    for op in &plan.ops {
        let act = crate::ui::action_label(&op.action);
        let risk = crate::ui::risk_label(op.risk);
        let dest = sanitize(&op.dest.display().to_string());
        let prov = sanitize(&op.provider);
        let note = sanitize(&op.note);
        let style = match op.action {
            crate::plan::Action::Refuse(_) => Style::default().fg(Color::Red),
            crate::plan::Action::Overwrite => Style::default().fg(Color::Yellow),
            crate::plan::Action::Create => Style::default().fg(Color::Green),
            crate::plan::Action::Merge => Style::default().fg(Color::Cyan),
            crate::plan::Action::Skip(_) => Style::default().fg(Color::DarkGray),
        };
        lines.push(Line::from(Span::styled(
            format!(
                "[{prov}] {act} ({risk}, {}) -> {dest} {note}",
                human_size(op.size)
            ),
            style,
        )));
    }
    lines
}

pub fn run_home(claude_dir: &Path) -> Result<()> {
    let mut env = LocateEnv::from_system();
    if claude_dir.exists() {
        env.overrides
            .insert(ProviderId::Claude, claude_dir.to_path_buf());
    }
    let mut state = HomeState::from_env(&env, &ScanOptions::default());
    let output = PathBuf::from(format!(
        "claude-backup-{}.tar.gz",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));

    with_terminal(|terminal| loop {
        if state.screen == HomeScreen::ConfirmBackup && state.plan.is_none() {
            let req = state.build_backup_request(output.clone(), claude_dir);
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
