//! Interactive terminal UI for selecting sessions and remapping paths,
//! built on ratatui + crossterm.

mod backup_ui;
mod restore_ui;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;

pub use backup_ui::run_backup;
pub use restore_ui::run_restore;

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Tui> {
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

/// Runs `f` with a configured terminal, always restoring it afterwards.
fn with_terminal<T>(f: impl FnOnce(&mut Tui) -> Result<T>) -> Result<T> {
    let mut terminal = setup_terminal()?;
    let result = f(&mut terminal);
    let _ = restore_terminal(&mut terminal);
    result
}

/// Current OS user, for path suggestions.
pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
}
