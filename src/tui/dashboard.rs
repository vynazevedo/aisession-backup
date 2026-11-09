use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::provider::Item;
use crate::ui::human_size;

use super::theme::{plural, Icon, Theme};
use super::ProviderEntry;

pub const ACTIVITY_WEEKS: usize = 12;

pub fn activity_buckets<'a>(
    items: impl IntoIterator<Item = &'a Item>,
    now: DateTime<Utc>,
    weeks: usize,
) -> Vec<u64> {
    let mut buckets = vec![0u64; weeks];
    for item in items {
        let Some(updated) = item.updated_at else {
            continue;
        };
        let age = now.signed_duration_since(updated).num_days();
        if age < 0 {
            if let Some(last) = buckets.last_mut() {
                *last += 1;
            }
            continue;
        }
        let week = (age / 7) as usize;
        if week < weeks {
            buckets[weeks - 1 - week] += 1;
        }
    }
    buckets
}

fn stat(theme: &Theme, label: &str, value: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme.muted()),
        Span::styled(value, style),
    ])
}

pub fn bar_lines(theme: &Theme, providers: &[&ProviderEntry], width: usize) -> Vec<Line<'static>> {
    let max = providers
        .iter()
        .map(|p| p.total_bytes())
        .max()
        .unwrap_or(0)
        .max(1);
    let label_width = providers
        .iter()
        .map(|p| p.id.as_str().chars().count() + 1)
        .max()
        .unwrap_or(9)
        .max(9);
    let value_width = 9;
    let bar_width = width.saturating_sub(label_width + value_width + 1).max(1);
    providers
        .iter()
        .map(|p| {
            let style = theme.provider(p.id.as_str());
            let bytes = p.total_bytes();
            let filled = if bytes == 0 {
                0
            } else {
                ((bytes as f64 / max as f64) * bar_width as f64).ceil() as usize
            }
            .min(bar_width);
            Line::from(vec![
                Span::styled(format!("{:<label_width$}", p.id.as_str()), style),
                Span::styled("\u{2588}".repeat(filled), style),
                Span::styled("\u{2591}".repeat(bar_width - filled), theme.border()),
                Span::styled(
                    format!(" {:>value_width$}", human_size(bytes)),
                    theme.muted(),
                ),
            ])
        })
        .collect()
}

pub fn sparkline_text(buckets: &[u64]) -> String {
    const LEVELS: [&str; 8] = [
        "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}", "\u{2585}", "\u{2586}", "\u{2587}",
        "\u{2588}",
    ];
    let max = buckets.iter().copied().max().unwrap_or(0);
    buckets
        .iter()
        .map(|&value| {
            if value == 0 || max == 0 {
                " ".to_string()
            } else {
                let level = ((value as f64 / max as f64) * 7.0).round() as usize;
                LEVELS[level.min(7)].to_string()
            }
        })
        .collect()
}

fn overview_logo(theme: &Theme, frame: usize) -> Option<(Vec<Line<'static>>, (u16, u16))> {
    Some((super::art::logo(theme, frame / 4), super::art::logo_size()))
}

pub fn render_overview(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    providers: &[ProviderEntry],
    now: DateTime<Utc>,
    frame: usize,
) {
    let block = theme.panel("Overview");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = Rect {
        x: inner.x.saturating_add(1),
        width: inner.width.saturating_sub(2),
        ..inner
    };
    if inner.width < 20 || inner.height < 6 {
        return;
    }

    let detected: Vec<&ProviderEntry> = providers
        .iter()
        .filter(|p| p.home.as_ref().is_some_and(|h| h.exists()))
        .collect();
    let total_items: usize = providers.iter().map(|p| p.items.len()).sum();
    let total_bytes: u64 = providers.iter().map(ProviderEntry::total_bytes).sum();
    let selected: usize = providers.iter().map(ProviderEntry::selected_count).sum();
    let failed = providers.iter().filter(|p| p.scan_error.is_some()).count();

    let logo = overview_logo(theme, frame);
    let show_logo = logo
        .as_ref()
        .is_some_and(|(_, (w, h))| inner.height >= h + 14 && inner.width >= *w);
    let logo_rows = match (&logo, show_logo) {
        (Some((_, (_, h))), true) => h + 1,
        _ => 0,
    };
    let bar_rows = (detected.len() as u16).max(1);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(logo_rows),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(bar_rows),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    if let (Some((lines, _)), true) = (logo, show_logo) {
        f.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            chunks[0],
        );
    }

    let mut stats = vec![
        stat(
            theme,
            "providers",
            format!("{} of {} detected", detected.len(), providers.len()),
            theme.strong(),
        ),
        stat(theme, "items", total_items.to_string(), theme.strong()),
        stat(theme, "size", human_size(total_bytes), theme.strong()),
        stat(
            theme,
            "selected",
            selected.to_string(),
            if selected > 0 {
                theme.accent()
            } else {
                theme.muted()
            },
        ),
    ];
    if failed > 0 {
        stats.push(stat(
            theme,
            "failed",
            format!(
                "{} {}",
                theme.icon(Icon::Warning),
                plural(failed, "provider")
            ),
            theme.danger(),
        ));
    }
    f.render_widget(Paragraph::new(stats), chunks[1]);

    f.render_widget(
        Paragraph::new(Span::styled("Size by provider", theme.muted())),
        chunks[2],
    );
    if detected.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("No provider detected.", theme.muted())),
            chunks[3],
        );
    } else {
        f.render_widget(
            Paragraph::new(bar_lines(theme, &detected, usize::from(inner.width))),
            chunks[3],
        );
    }

    let activity = activity_buckets(
        providers.iter().flat_map(|p| p.items.iter()),
        now,
        ACTIVITY_WEEKS,
    );
    let recent: u64 = activity.iter().sum();
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("Activity by week: {}", plural(recent as usize, "item")),
            theme.muted(),
        )),
        chunks[5],
    );
    let spark = sparkline_text(&activity);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(spark, theme.accent())])),
        chunks[6],
    );
    let start = (now - chrono::Duration::weeks(ACTIVITY_WEEKS as i64)).format("%b %d");
    let start_text = start.to_string();
    let end_text = "now";
    let gap = ACTIVITY_WEEKS
        .saturating_sub(start_text.len() + end_text.len())
        .max(1);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("{start_text}{}{end_text}", " ".repeat(gap)),
            theme.muted(),
        )),
        chunks[7],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ItemKind, ProviderId};
    use crate::tui::theme::IconSet;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 0, 0, 0).unwrap()
    }

    fn item(days_ago: i64) -> Item {
        Item {
            id: format!("i{days_ago}"),
            kind: ItemKind::Session,
            label: "x".into(),
            cwd: Some("/w".into()),
            updated_at: Some(now() - chrono::Duration::days(days_ago)),
            files: Vec::new(),
        }
    }

    #[test]
    fn activity_is_bucketed_by_week_with_the_newest_last() {
        let items = vec![item(0), item(1), item(8), item(83), item(84), item(-2)];
        let buckets = activity_buckets(&items, now(), 12);
        assert_eq!(buckets.len(), 12);
        assert_eq!(buckets[11], 3);
        assert_eq!(buckets[10], 1);
        assert_eq!(buckets[0], 1);
        assert_eq!(buckets.iter().sum::<u64>(), 5);
    }

    #[test]
    fn sparkline_and_bars_share_a_baseline() {
        assert_eq!(sparkline_text(&[0, 1, 4, 8]).chars().count(), 4);
        assert!(sparkline_text(&[0, 0]).trim().is_empty());
        let dir = tempfile::tempdir().unwrap();
        let big = ProviderEntry {
            id: ProviderId::Claude,
            display_name: "Claude Code".into(),
            home: Some(dir.path().to_path_buf()),
            items: Vec::new(),
            selected: Vec::new(),
            scan_error: None,
        };
        let theme = Theme::new(false, IconSet::Unicode);
        let mut long = big.clone();
        long.id = ProviderId::Antigravity;
        let lines = bar_lines(&theme, &[&big, &long], 40);
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert_eq!(text.chars().count(), 40, "{text}");
        }
    }

    #[test]
    fn overview_renders_at_every_size_and_without_providers() {
        let dir = tempfile::tempdir().unwrap();
        let entry = ProviderEntry {
            id: ProviderId::Claude,
            display_name: "Claude Code".into(),
            home: Some(dir.path().to_path_buf()),
            items: vec![item(0), item(20)],
            selected: vec![true, false],
            scan_error: None,
        };
        let missing = ProviderEntry {
            id: ProviderId::Grok,
            display_name: "Grok CLI".into(),
            home: Some(PathBuf::from("/does/not/exist")),
            items: Vec::new(),
            selected: Vec::new(),
            scan_error: Some("boom".into()),
        };
        for icons in [IconSet::Text, IconSet::Unicode, IconSet::Nerd] {
            for color in [true, false] {
                let theme = Theme::new(color, icons);
                for (width, height) in [(1, 1), (22, 8), (50, 30), (60, 50)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    for providers in [vec![entry.clone(), missing.clone()], Vec::new()] {
                        terminal
                            .draw(|f| render_overview(f, f.area(), &theme, &providers, now(), 3))
                            .unwrap();
                    }
                }
            }
        }
    }
}
