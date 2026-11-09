use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Gauge, Paragraph};

use crate::provider::{self, Item, LocateEnv, ProviderId, ScanOptions};
use crate::ui::{human_size, sanitize};

use super::theme::{self, Icon, Theme};
use super::{ProviderEntry, Tui};

const TICK: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    Scanning,
    Done { items: usize, bytes: u64 },
    Missing,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct LoadRow {
    pub id: ProviderId,
    pub name: String,
    pub state: LoadState,
}

pub fn is_cancel(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

fn cancel_requested(timeout: Duration) -> Result<bool> {
    if event::poll(timeout)? {
        if let Event::Key(key) = event::read()? {
            return Ok(is_cancel(&key));
        }
    }
    Ok(false)
}

fn session_progress() -> Option<(usize, usize)> {
    let (done, total) = crate::discover::SESSION_PROGRESS.snapshot();
    (total > 0).then_some((done.min(total), total))
}

fn row_line(row: &LoadRow, theme: &Theme, frame: usize) -> Line<'static> {
    let (marker, status) = match &row.state {
        LoadState::Scanning => {
            let detail = match (row.id, session_progress()) {
                (ProviderId::Claude, Some((done, total))) => {
                    format!("reading sessions {done}/{total}")
                }
                _ => "scanning".to_string(),
            };
            (
                Span::styled(theme.spinner(frame).to_string(), theme.accent()),
                Span::styled(detail, theme.info()),
            )
        }
        LoadState::Done { items, bytes } => (
            Span::styled(theme.icon(Icon::Check).to_string(), theme.success()),
            Span::styled(
                format!("{items} item(s)  {}", human_size(*bytes)),
                theme.muted(),
            ),
        ),
        LoadState::Missing => (
            Span::styled(theme.icon(Icon::Empty).to_string(), theme.muted()),
            Span::styled("not found".to_string(), theme.muted()),
        ),
        LoadState::Failed(reason) => (
            Span::styled(theme.icon(Icon::Warning).to_string(), theme.danger()),
            Span::styled(format!("scan failed: {}", sanitize(reason)), theme.danger()),
        ),
    };
    Line::from(vec![
        Span::raw("  "),
        marker,
        Span::raw("  "),
        Span::styled(
            format!("{:<17}", sanitize(&row.name)),
            theme.provider(row.id.as_str()),
        ),
        status,
    ])
}

pub fn draw_loading(
    f: &mut Frame,
    theme: &Theme,
    title: &str,
    rows: &[LoadRow],
    elapsed: Duration,
    frame: usize,
) {
    let [top, body, bottom] = theme::frame_layout(f.area());
    let finished = rows
        .iter()
        .filter(|row| row.state != LoadState::Scanning)
        .count();
    f.render_widget(
        theme::header(theme, "Loading", &format!("{:.1}s", elapsed.as_secs_f64())),
        top,
    );
    let block = theme.panel(title);
    let inner = block.inner(body);
    f.render_widget(block, body);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    let claude_reading = rows
        .iter()
        .any(|row| row.id == ProviderId::Claude && row.state == LoadState::Scanning);
    let (ratio, label) = match session_progress() {
        Some((done, total)) if claude_reading => (
            done as f64 / total as f64,
            format!("{done}/{total} Claude Code sessions"),
        ),
        _ => (
            if rows.is_empty() {
                1.0
            } else {
                finished as f64 / rows.len() as f64
            },
            format!("{finished}/{} provider(s)", rows.len()),
        ),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Looking for sessions and settings. Nothing is written while this runs.".to_string(),
            theme.muted(),
        ))),
        chunks[0],
    );
    f.render_widget(
        Gauge::default()
            .gauge_style(theme.accent())
            .ratio(ratio.clamp(0.0, 1.0))
            .label(label),
        chunks[2],
    );
    let lines: Vec<Line> = rows.iter().map(|row| row_line(row, theme, frame)).collect();
    f.render_widget(Paragraph::new(lines), chunks[3]);
    f.render_widget(theme::footer(theme, &[("q", "cancel")]), bottom);
}

type ScanResult = (usize, Option<std::path::PathBuf>, Result<Vec<Item>, String>);

pub fn scan_providers(
    terminal: &mut Tui,
    env: &LocateEnv,
    opts: &ScanOptions,
) -> Result<Option<Vec<ProviderEntry>>> {
    let registry = provider::registry();
    let mut rows: Vec<LoadRow> = registry
        .iter()
        .map(|p| LoadRow {
            id: p.id(),
            name: p.display_name().to_string(),
            state: LoadState::Scanning,
        })
        .collect();
    let mut entries: Vec<Option<ProviderEntry>> = vec![None; rows.len()];
    let (tx, rx) = mpsc::channel::<ScanResult>();
    for (index, prov) in registry.into_iter().enumerate() {
        let tx = tx.clone();
        let env = env.clone();
        let opts = opts.clone();
        std::thread::spawn(move || {
            let home = prov.locate(&env);
            let items = match &home {
                Some(path) if path.exists() => prov
                    .scan(path, &opts)
                    .map(|inv| inv.items)
                    .map_err(|err| format!("{err:#}")),
                _ => Ok(Vec::new()),
            };
            let _ = tx.send((index, home, items));
        });
    }
    drop(tx);
    let theme = theme::current();
    let start = Instant::now();
    let mut frame = 0usize;
    while entries.iter().any(Option::is_none) {
        while let Ok((index, home, result)) = rx.try_recv() {
            let detected = home.as_ref().is_some_and(|path| path.exists());
            let (items, scan_error) = match result {
                Ok(items) => (items, None),
                Err(reason) => (Vec::new(), Some(reason)),
            };
            rows[index].state = match (&scan_error, detected) {
                (Some(reason), _) => LoadState::Failed(reason.clone()),
                (None, false) => LoadState::Missing,
                (None, true) => LoadState::Done {
                    items: items.len(),
                    bytes: items.iter().map(Item::total_size).sum(),
                },
            };
            entries[index] = Some(ProviderEntry {
                id: rows[index].id,
                display_name: rows[index].name.clone(),
                selected: vec![false; items.len()],
                home,
                items,
                scan_error,
            });
        }
        terminal.draw(|f| {
            draw_loading(
                f,
                theme,
                "Scanning providers",
                &rows,
                start.elapsed(),
                frame,
            )
        })?;
        if entries.iter().all(Option::is_some) {
            break;
        }
        if cancel_requested(TICK)? {
            return Ok(None);
        }
        frame = frame.wrapping_add(1);
    }
    Ok(Some(entries.into_iter().flatten().collect()))
}

pub fn run_with_loading<T: Send + 'static>(
    terminal: &mut Tui,
    title: &str,
    row: (ProviderId, &str),
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<Option<T>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(task());
    });
    let theme = theme::current();
    let rows = vec![LoadRow {
        id: row.0,
        name: row.1.to_string(),
        state: LoadState::Scanning,
    }];
    let start = Instant::now();
    let mut frame = 0usize;
    loop {
        match rx.try_recv() {
            Ok(value) => return Ok(Some(value)),
            Err(mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("background task stopped unexpectedly")
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        terminal.draw(|f| draw_loading(f, theme, title, &rows, start.elapsed(), frame))?;
        if cancel_requested(TICK)? {
            return Ok(None);
        }
        frame = frame.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::IconSet;
    use ratatui::backend::TestBackend;

    fn rows() -> Vec<LoadRow> {
        vec![
            LoadRow {
                id: ProviderId::Claude,
                name: "Claude Code".into(),
                state: LoadState::Scanning,
            },
            LoadRow {
                id: ProviderId::Codex,
                name: "Codex CLI".into(),
                state: LoadState::Done {
                    items: 3,
                    bytes: 2048,
                },
            },
            LoadRow {
                id: ProviderId::Grok,
                name: "Grok CLI".into(),
                state: LoadState::Missing,
            },
            LoadRow {
                id: ProviderId::Gemini,
                name: "Gemini\u{1b}[31m".into(),
                state: LoadState::Failed("bad\u{7} file".into()),
            },
        ]
    }

    #[test]
    fn loading_screen_renders_every_state_at_any_size() {
        for icons in [IconSet::Text, IconSet::Nerd] {
            let theme = Theme::new(true, icons);
            for (width, height) in [(1, 1), (20, 5), (100, 20)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                for frame in 0..12 {
                    terminal
                        .draw(|f| {
                            draw_loading(
                                f,
                                &theme,
                                "Scanning providers",
                                &rows(),
                                Duration::from_millis(1500),
                                frame,
                            )
                        })
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn rows_describe_each_state_without_control_characters() {
        let theme = Theme::new(false, IconSet::Text);
        let text: Vec<String> = rows()
            .iter()
            .map(|row| {
                row_line(row, &theme, 0)
                    .spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert!(text[1].contains("3 item(s)"));
        assert!(text[2].contains("not found"));
        assert!(text[3].contains("scan failed"));
        assert!(text.iter().all(|line| !line.chars().any(char::is_control)));
    }

    #[test]
    fn spinner_changes_between_frames() {
        for icons in [IconSet::Text, IconSet::Nerd] {
            let theme = Theme::new(true, icons);
            assert_ne!(theme.spinner(0), theme.spinner(1));
            assert_eq!(theme.spinner(0), theme.spinner(theme.spinner_len()));
        }
    }

    #[test]
    fn cancel_keys_are_recognized() {
        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(is_cancel(&press(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(is_cancel(&press(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(is_cancel(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!is_cancel(&press(KeyCode::Char('c'), KeyModifiers::NONE)));
    }
}
