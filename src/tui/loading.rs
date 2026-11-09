use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::provider::{self, Item, LocateEnv, ProviderId, ScanOptions};
use crate::ui::{human_size, sanitize};

use super::theme::{self, Icon, Theme};
use super::{ProviderEntry, Tui};

const TICK: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    Scanning,
    Working(String),
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
                (ProviderId::Claude, Some((done, total))) if done < total => {
                    format!("reading sessions {done}/{total}")
                }
                (ProviderId::Claude, Some(_)) => "finishing".to_string(),
                _ => "scanning".to_string(),
            };
            (
                Span::styled(theme.spinner(frame).to_string(), theme.accent()),
                Span::styled(detail, theme.info()),
            )
        }
        LoadState::Working(detail) => (
            Span::styled(theme.spinner(frame).to_string(), theme.accent()),
            Span::styled(sanitize(detail), theme.info()),
        ),
        LoadState::Done { items: 0, .. } => (
            Span::styled(theme.icon(Icon::Partial).to_string(), theme.warning()),
            Span::styled("empty".to_string(), theme.warning()),
        ),
        LoadState::Done { items, bytes } => (
            Span::styled(theme.icon(Icon::Check).to_string(), theme.success()),
            Span::styled(
                format!("{}  {}", theme::plural(*items, "item"), human_size(*bytes)),
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

pub struct Meter {
    pub done: usize,
    pub total: usize,
    pub unit: &'static str,
}

fn meter_line(theme: &Theme, meter: &Meter, width: u16) -> Line<'static> {
    let label = format!(" {}/{} {} ", meter.done, meter.total, meter.unit);
    let bar_width = usize::from(width)
        .saturating_sub(label.chars().count() + 1)
        .max(1);
    let filled = (meter.done * bar_width)
        .checked_div(meter.total)
        .unwrap_or(bar_width)
        .min(bar_width);
    let (full, empty) = if theme.color {
        ("\u{2588}", "\u{2591}")
    } else {
        ("#", ".")
    };
    Line::from(vec![
        Span::styled(full.repeat(filled), theme.accent()),
        Span::styled(empty.repeat(bar_width - filled), theme.border()),
        Span::raw(" "),
        Span::styled(label, theme.key()),
    ])
}

#[allow(clippy::too_many_arguments)]
pub fn draw_loading(
    f: &mut Frame,
    theme: &Theme,
    title: &str,
    subtitle: &str,
    rows: &[LoadRow],
    meter: Option<&Meter>,
    elapsed: Duration,
    frame: usize,
) {
    let [top, body, bottom] = theme::frame_layout(f.area());
    f.render_widget(
        theme::header(theme, "Loading", &format!("{:.1}s", elapsed.as_secs_f64())),
        top,
    );
    let block = theme.panel(title);
    let inner = block.inner(body);
    f.render_widget(block, body);
    let inner = Rect {
        x: inner.x.saturating_add(1),
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            subtitle.to_string(),
            theme.muted(),
        ))),
        chunks[0],
    );
    if let Some(meter) = meter {
        f.render_widget(
            Paragraph::new(meter_line(theme, meter, chunks[2].width)),
            chunks[2],
        );
    }
    let lines: Vec<Line> = rows.iter().map(|row| row_line(row, theme, frame)).collect();
    let list_height = (lines.len() as u16).min(chunks[4].height);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(list_height),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(chunks[4]);
    f.render_widget(Paragraph::new(lines), parts[0]);
    super::art::render_centered(f, parts[2], super::art::logo(theme, frame / 3));
    theme::render_footer(f, bottom, theme, &[("q", "cancel")]);
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
        let meter = Meter {
            done: rows
                .iter()
                .filter(|row| !matches!(row.state, LoadState::Scanning))
                .count(),
            total: rows.len(),
            unit: "providers",
        };
        terminal.draw(|f| {
            draw_loading(
                f,
                theme,
                "Scanning providers",
                "Looking for sessions and settings. Nothing is written while this runs.",
                &rows,
                Some(&meter),
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
    subtitle: &str,
    row: (ProviderId, &str, &str),
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<Option<T>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(task());
    });
    let theme = theme::current();
    let tracks_sessions = row.0 == ProviderId::Claude && row.2.is_empty();
    let rows = vec![LoadRow {
        id: row.0,
        name: row.1.to_string(),
        state: if tracks_sessions {
            LoadState::Scanning
        } else {
            LoadState::Working(row.2.to_string())
        },
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
        let meter = if tracks_sessions {
            session_progress().map(|(done, total)| Meter {
                done,
                total,
                unit: "sessions",
            })
        } else {
            None
        };
        terminal.draw(|f| {
            draw_loading(
                f,
                theme,
                title,
                subtitle,
                &rows,
                meter.as_ref(),
                start.elapsed(),
                frame,
            )
        })?;
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
        for icons in [IconSet::Text, IconSet::Unicode, IconSet::Nerd] {
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
                                "Looking for sessions.",
                                &rows(),
                                Some(&Meter {
                                    done: 2,
                                    total: 4,
                                    unit: "providers",
                                }),
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
        assert!(text[1].contains("3 items"));
        assert!(text[2].contains("not found"));
        assert!(text[3].contains("scan failed"));
        assert!(text.iter().all(|line| !line.chars().any(char::is_control)));
    }

    #[test]
    fn meter_fits_the_width_and_keeps_its_label() {
        for color in [true, false] {
            let theme = Theme::new(color, IconSet::Unicode);
            let meter = Meter {
                done: 3,
                total: 7,
                unit: "providers",
            };
            let line = meter_line(&theme, &meter, 40);
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert_eq!(text.chars().count(), 40, "{text}");
            assert!(text.ends_with(" 3/7 providers "));
        }
    }

    #[test]
    fn spinner_changes_between_frames() {
        for icons in [IconSet::Text, IconSet::Unicode, IconSet::Nerd] {
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
