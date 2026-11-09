use std::sync::OnceLock;

use ratatui::crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::plan::{Action, Plan, Risk};
use crate::ui::{human_size, sanitize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconSet {
    Text,
    Nerd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Folder,
    FolderOpen,
    Check,
    Partial,
    Empty,
    Warning,
    Lock,
    Archive,
    Arrow,
    Search,
    Clock,
    Bolt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub icons: IconSet,
}

static CURRENT: OnceLock<Theme> = OnceLock::new();

pub fn current() -> &'static Theme {
    CURRENT.get_or_init(Theme::from_env)
}

impl Theme {
    pub fn new(color: bool, icons: IconSet) -> Self {
        Self { color, icons }
    }

    pub fn from_env() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        let icons = match crate::archive::env_setting("ICONS")
            .and_then(|value| value.into_string().ok())
            .as_deref()
            .map(str::trim)
        {
            Some(value) if value.eq_ignore_ascii_case("nerd") => IconSet::Nerd,
            _ => IconSet::Text,
        };
        Self::new(color, icons)
    }

    pub fn icon(&self, icon: Icon) -> &'static str {
        match (self.icons, icon) {
            (IconSet::Text, Icon::Folder) => ">",
            (IconSet::Text, Icon::FolderOpen) => "v",
            (IconSet::Text, Icon::Check) => "[x]",
            (IconSet::Text, Icon::Partial) => "[-]",
            (IconSet::Text, Icon::Empty) => "[ ]",
            (IconSet::Text, Icon::Warning) => "!",
            (IconSet::Text, Icon::Lock) => "*",
            (IconSet::Text, Icon::Archive) => "#",
            (IconSet::Text, Icon::Arrow) => "->",
            (IconSet::Text, Icon::Search) => "/",
            (IconSet::Text, Icon::Clock) => "@",
            (IconSet::Text, Icon::Bolt) => "+",
            (IconSet::Nerd, Icon::Folder) => "\u{f07b}",
            (IconSet::Nerd, Icon::FolderOpen) => "\u{f07c}",
            (IconSet::Nerd, Icon::Check) => "\u{f14a}",
            (IconSet::Nerd, Icon::Partial) => "\u{f146}",
            (IconSet::Nerd, Icon::Empty) => "\u{f096}",
            (IconSet::Nerd, Icon::Warning) => "\u{f071}",
            (IconSet::Nerd, Icon::Lock) => "\u{f023}",
            (IconSet::Nerd, Icon::Archive) => "\u{f187}",
            (IconSet::Nerd, Icon::Arrow) => "\u{f061}",
            (IconSet::Nerd, Icon::Search) => "\u{f002}",
            (IconSet::Nerd, Icon::Clock) => "\u{f017}",
            (IconSet::Nerd, Icon::Bolt) => "\u{f0e7}",
        }
    }

    fn fg(&self, color: Color) -> Style {
        if self.color {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    pub fn accent(&self) -> Style {
        self.fg(Color::Indexed(45)).add_modifier(Modifier::BOLD)
    }

    pub fn muted(&self) -> Style {
        if self.color {
            Style::default().fg(Color::Indexed(244))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn success(&self) -> Style {
        self.fg(Color::Indexed(78))
    }

    pub fn warning(&self) -> Style {
        self.fg(Color::Indexed(214))
    }

    pub fn danger(&self) -> Style {
        self.fg(Color::Indexed(203)).add_modifier(Modifier::BOLD)
    }

    pub fn info(&self) -> Style {
        self.fg(Color::Indexed(117))
    }

    pub fn strong(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    pub fn border(&self) -> Style {
        self.fg(Color::Indexed(239))
    }

    pub fn selected_row(&self) -> Style {
        if self.color {
            Style::default()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn key(&self) -> Style {
        if self.color {
            Style::default()
                .fg(Color::Indexed(16))
                .bg(Color::Indexed(45))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn provider(&self, id: &str) -> Style {
        let color = match id {
            "claude" => Color::Indexed(173),
            "codex" => Color::Indexed(42),
            "grok" => Color::Indexed(252),
            "gemini" => Color::Indexed(75),
            "agy" => Color::Indexed(141),
            "maestri" => Color::Indexed(220),
            "opencode" => Color::Indexed(51),
            _ => Color::Indexed(250),
        };
        self.fg(color).add_modifier(Modifier::BOLD)
    }

    pub fn panel<'a>(&self, title: &str) -> Block<'a> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.border())
            .title(Line::from(Span::styled(
                format!(" {title} "),
                self.accent(),
            )))
    }
}

pub fn header<'a>(theme: &Theme, screen: &str, context: &str) -> Paragraph<'a> {
    let mut spans = vec![
        Span::styled(
            format!(" {} aisession-backup ", theme.icon(Icon::Archive)),
            theme.key(),
        ),
        Span::styled(format!(" v{} ", env!("CARGO_PKG_VERSION")), theme.muted()),
        Span::styled(format!(" {screen}"), theme.strong()),
    ];
    if !context.is_empty() {
        spans.push(Span::styled(format!("  {context}"), theme.muted()));
    }
    Paragraph::new(Line::from(spans))
}

pub fn footer<'a>(theme: &Theme, keys: &[(&str, &str)]) -> Paragraph<'a> {
    let mut spans = Vec::new();
    for (index, (key, label)) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!(" {key} "), theme.key()));
        spans.push(Span::styled(format!(" {label}"), theme.muted()));
    }
    Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true })
}

pub fn status_line<'a>(theme: &Theme, message: &str, danger: bool) -> Paragraph<'a> {
    let style = if danger {
        theme.danger()
    } else {
        theme.warning()
    };
    Paragraph::new(Line::from(Span::styled(
        format!(" {} {}", theme.icon(Icon::Warning), sanitize(message)),
        style,
    )))
    .wrap(Wrap { trim: true })
}

pub fn frame_layout(area: Rect) -> [Rect; 3] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2]]
}

pub fn split_with_details(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < 100 {
        return (area, None);
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);
    (chunks[0], Some(chunks[1]))
}

pub fn render_help(f: &mut Frame, theme: &Theme, title: &str, keys: &[(&str, &str)]) {
    let area = f.area();
    let width = area.width.saturating_sub(4).min(64);
    let height = (keys.len() as u16 + 4).min(area.height.saturating_sub(2));
    if width < 10 || height < 3 {
        return;
    }
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, label)| {
            Line::from(vec![
                Span::styled(format!(" {key:>9} "), theme.accent()),
                Span::raw(format!("  {label}")),
            ])
        })
        .collect();
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(theme.panel(title))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn count_span(theme: &Theme, label: &str, count: usize, style: Style) -> Span<'static> {
    let style = if count == 0 { theme.muted() } else { style };
    Span::styled(format!("{label} {count}"), style)
}

pub fn plan_lines(plan: Option<&Plan>, theme: &Theme) -> Vec<Line<'static>> {
    let Some(plan) = plan else {
        return vec![Line::from(Span::styled(
            "Calculating the plan...",
            theme.muted(),
        ))];
    };
    let summary = plan.summary();
    let gap = || Span::raw("   ");
    let mut lines = vec![
        Line::from(vec![
            count_span(theme, "create", summary.create, theme.success()),
            gap(),
            count_span(theme, "overwrite", summary.overwrite, theme.warning()),
            gap(),
            count_span(theme, "merge", summary.merge, theme.info()),
            gap(),
            count_span(theme, "skip", summary.skip, theme.muted()),
            gap(),
            count_span(theme, "refuse", summary.refuse, theme.danger()),
            gap(),
            Span::styled(human_size(summary.bytes), theme.strong()),
        ]),
        Line::from(vec![
            Span::styled("risk ", theme.muted()),
            count_span(
                theme,
                "executes code",
                summary.executes_code,
                theme.danger(),
            ),
            gap(),
            count_span(theme, "sensitive", summary.sensitive, theme.warning()),
        ]),
    ];
    if summary.refuse > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} Blocked: {} operation(s) refused. Nothing will be written until they are resolved.",
                theme.icon(Icon::Warning),
                summary.refuse
            ),
            theme.danger(),
        )));
    }
    for warning in &plan.warnings {
        lines.push(Line::from(Span::styled(
            format!("{} {}", theme.icon(Icon::Warning), sanitize(warning)),
            theme.warning(),
        )));
    }
    lines.push(Line::from(""));
    for op in &plan.ops {
        let (label, style) = match &op.action {
            Action::Create => ("CREATE".to_string(), theme.success()),
            Action::Overwrite => ("OVERWRITE".to_string(), theme.warning()),
            Action::Merge => ("MERGE".to_string(), theme.info()),
            Action::Skip(_) => ("SKIP".to_string(), theme.muted()),
            Action::Refuse(_) => ("REFUSE".to_string(), theme.danger()),
        };
        let risk = match op.risk {
            Risk::Normal => Span::raw("      "),
            Risk::Sensitive => Span::styled(" SENS ", theme.warning()),
            Risk::ExecutesCode => Span::styled(" EXEC ", theme.danger()),
        };
        let mut spans = vec![
            Span::styled(format!("{label:<9}"), style),
            risk,
            Span::styled(
                format!(" {:<9}", sanitize(&op.provider)),
                theme.provider(&op.provider),
            ),
            Span::styled(format!("{:>9} ", human_size(op.size)), theme.muted()),
            Span::raw(sanitize(&op.dest.display().to_string())),
        ];
        let reason = match &op.action {
            Action::Skip(reason) | Action::Refuse(reason) => sanitize(reason),
            _ => String::new(),
        };
        let note = sanitize(&op.note);
        for extra in [reason, note] {
            if !extra.is_empty() {
                spans.push(Span::styled(format!("  {extra}"), theme.muted()));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Scroll {
    pub offset: u16,
}

impl Scroll {
    pub fn handle_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Down | KeyCode::Char('j') => self.offset = self.offset.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.offset = self.offset.saturating_sub(1),
            KeyCode::PageDown => self.offset = self.offset.saturating_add(15),
            KeyCode::PageUp => self.offset = self.offset.saturating_sub(15),
            KeyCode::Home | KeyCode::Char('g') => self.offset = 0,
            KeyCode::End | KeyCode::Char('G') => self.offset = u16::MAX,
            _ => return false,
        }
        true
    }

    pub fn clamp(&mut self, lines: usize, height: u16) {
        let max = lines.saturating_sub(height.saturating_sub(2) as usize);
        self.offset = self.offset.min(max.min(u16::MAX as usize) as u16);
    }
}

pub fn render_plan(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    plan: Option<&Plan>,
    scroll: &mut Scroll,
) {
    let lines = plan_lines(plan, theme);
    scroll.clamp(lines.len(), area.height);
    let title = format!(
        "Plan  {} {} operation(s)",
        theme.icon(Icon::Bolt),
        plan.map(|p| p.ops.len()).unwrap_or(0)
    );
    f.render_widget(
        Paragraph::new(lines)
            .block(theme.panel(&title))
            .scroll((scroll.offset, 0)),
        area,
    );
}

pub const PLAN_KEYS: [(&str, &str); 2] = [("j/k", "scroll"), ("g/G", "top/end")];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlannedOp;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn plan() -> Plan {
        let mut plan = Plan::default();
        for (action, risk) in [
            (Action::Create, Risk::Normal),
            (Action::Overwrite, Risk::Sensitive),
            (
                Action::Refuse("provider\u{1b}[31m may be running".into()),
                Risk::ExecutesCode,
            ),
        ] {
            plan.ops.push(PlannedOp {
                provider: "claude".into(),
                action,
                risk,
                dest: PathBuf::from("/dest/file\u{7}"),
                size: 10,
                rewrites: 0,
                note: "note\r".into(),
            });
        }
        plan
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn text_icons_are_ascii_and_nerd_icons_are_single_glyphs() {
        let text = Theme::new(true, IconSet::Text);
        let nerd = Theme::new(true, IconSet::Nerd);
        for icon in [
            Icon::Folder,
            Icon::FolderOpen,
            Icon::Check,
            Icon::Partial,
            Icon::Empty,
            Icon::Warning,
            Icon::Lock,
            Icon::Archive,
            Icon::Arrow,
            Icon::Search,
            Icon::Clock,
            Icon::Bolt,
        ] {
            assert!(text.icon(icon).is_ascii());
            assert_eq!(nerd.icon(icon).chars().count(), 1);
        }
    }

    #[test]
    fn no_color_theme_sets_no_colors() {
        let theme = Theme::new(false, IconSet::Text);
        for style in [
            theme.accent(),
            theme.muted(),
            theme.success(),
            theme.warning(),
            theme.danger(),
            theme.key(),
            theme.selected_row(),
            theme.provider("claude"),
        ] {
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);
        }
    }

    #[test]
    fn plan_lines_show_blocking_banner_and_strip_controls() {
        let theme = Theme::new(true, IconSet::Text);
        let lines = plan_lines(Some(&plan()), &theme);
        let rendered = text(&lines);
        assert!(rendered.contains("Blocked: 1 operation(s) refused"));
        assert!(rendered.contains("REFUSE"));
        assert!(rendered.contains("EXEC"));
        assert!(!rendered.chars().any(|c| c.is_control() && c != '\n'));
    }

    #[test]
    fn plan_and_help_render_on_tiny_terminals() {
        let theme = Theme::new(true, IconSet::Nerd);
        for (width, height) in [(1, 1), (12, 4), (40, 10), (160, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut scroll = Scroll { offset: u16::MAX };
            terminal
                .draw(|f| {
                    let [top, body, bottom] = frame_layout(f.area());
                    f.render_widget(header(&theme, "Plan", "context"), top);
                    render_plan(f, body, &theme, Some(&plan()), &mut scroll);
                    f.render_widget(footer(&theme, &PLAN_KEYS), bottom);
                    render_help(f, &theme, "Keys", &PLAN_KEYS);
                })
                .unwrap();
        }
    }

    #[test]
    fn scroll_is_clamped_to_the_content() {
        let mut scroll = Scroll::default();
        scroll.handle_key(KeyCode::End);
        scroll.clamp(30, 12);
        assert_eq!(scroll.offset, 20);
        scroll.handle_key(KeyCode::Home);
        assert_eq!(scroll.offset, 0);
        scroll.handle_key(KeyCode::Up);
        assert_eq!(scroll.offset, 0);
    }
}
