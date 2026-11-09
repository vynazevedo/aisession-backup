use std::sync::OnceLock;

use ratatui::crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::plan::{Action, Plan, Risk};
use crate::ui::{human_size, sanitize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconSet {
    Text,
    Unicode,
    Nerd,
}

impl IconSet {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ascii" | "text" | "plain" => Some(IconSet::Text),
            "unicode" | "default" => Some(IconSet::Unicode),
            "nerd" | "nerdfont" | "nerd-font" => Some(IconSet::Nerd),
            _ => None,
        }
    }
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

const TEXT_SPINNER: [&str; 4] = ["|", "/", "-", "\\"];
const BRAILLE_SPINNER: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

pub fn current() -> &'static Theme {
    CURRENT.get_or_init(Theme::from_env)
}

impl Theme {
    pub fn new(color: bool, icons: IconSet) -> Self {
        Self { color, icons }
    }

    pub fn from_env() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        let dumb = std::env::var("TERM").is_ok_and(|term| term == "dumb");
        let icons = crate::archive::env_setting("ICONS")
            .and_then(|value| value.into_string().ok())
            .and_then(|value| IconSet::parse(&value))
            .unwrap_or(if dumb {
                IconSet::Text
            } else {
                IconSet::Unicode
            });
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
            (IconSet::Unicode, Icon::Folder) => "\u{25b8}",
            (IconSet::Unicode, Icon::FolderOpen) => "\u{25be}",
            (IconSet::Unicode, Icon::Check) => "\u{25cf}",
            (IconSet::Unicode, Icon::Partial) => "\u{25d0}",
            (IconSet::Unicode, Icon::Empty) => "\u{25cb}",
            (IconSet::Unicode, Icon::Warning) => "\u{25b2}",
            (IconSet::Unicode, Icon::Lock) => "\u{25a0}",
            (IconSet::Unicode, Icon::Archive) => "\u{25c6}",
            (IconSet::Unicode, Icon::Arrow) => "\u{2192}",
            (IconSet::Unicode, Icon::Search) => "\u{25ce}",
            (IconSet::Unicode, Icon::Clock) => "\u{25f7}",
            (IconSet::Unicode, Icon::Bolt) => "\u{25b6}",
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

    pub fn spinner_len(&self) -> usize {
        match self.icons {
            IconSet::Text => TEXT_SPINNER.len(),
            IconSet::Unicode | IconSet::Nerd => BRAILLE_SPINNER.len(),
        }
    }

    pub fn spinner(&self, frame: usize) -> &'static str {
        match self.icons {
            IconSet::Text => TEXT_SPINNER[frame % TEXT_SPINNER.len()],
            IconSet::Unicode | IconSet::Nerd => BRAILLE_SPINNER[frame % BRAILLE_SPINNER.len()],
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
        self.fg(Color::Indexed(245))
    }

    pub fn selected_row(&self) -> Style {
        if self.color {
            Style::default()
                .bg(Color::Indexed(238))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
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
            "agy" | "antigravity" => Color::Indexed(141),
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

pub fn plural(count: usize, word: &str) -> String {
    crate::ui::plural(count, word)
}

pub fn truncate_left(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let tail: String = text.chars().skip(count - (width - 1)).collect();
    format!("\u{2026}{tail}")
}

pub fn home_relative(path: &std::path::Path) -> String {
    let text = sanitize(&path.display().to_string());
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        if !home.is_empty() && home != "/" {
            if let Some(rest) = text.strip_prefix(&home) {
                if rest.is_empty() || rest.starts_with(['/', '\\']) {
                    return format!("~{rest}");
                }
            }
        }
    }
    text
}

pub fn chip_spans(theme: &Theme, key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!(" {key} "), theme.key()),
        Span::styled(format!(" {label}"), theme.muted()),
    ]
}

fn chip_width(key: &str, label: &str) -> usize {
    key.chars().count() + label.chars().count() + 3
}

pub fn chip_lines(theme: &Theme, keys: &[(&str, &str)], width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (key, label) in keys {
        let needed = chip_width(key, label);
        let gap = if spans.is_empty() { 0 } else { 2 };
        if !spans.is_empty() && used + gap + needed > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        } else if gap > 0 {
            spans.push(Span::raw("  "));
            used += gap;
        }
        spans.extend(chip_spans(theme, key, label));
        used += needed;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

pub fn render_footer(f: &mut Frame, area: Rect, theme: &Theme, keys: &[(&str, &str)]) {
    f.render_widget(Paragraph::new(chip_lines(theme, keys, area.width)), area);
}

pub fn footer<'a>(theme: &Theme, keys: &[(&str, &str)]) -> Paragraph<'a> {
    let mut spans = Vec::new();
    for (index, (key, label)) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(chip_spans(theme, key, label));
    }
    Paragraph::new(Line::from(spans))
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
        .spacing(1)
        .split(area);
    (chunks[0], Some(chunks[1]))
}

pub fn dim_frame(f: &mut Frame, _theme: &Theme) {
    let area = f.area();
    let buffer = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.set_symbol(" ");
            cell.set_style(Style::reset());
        }
    }
}

pub fn render_help(f: &mut Frame, theme: &Theme, title: &str, keys: &[(&str, &str)]) {
    dim_frame(f, theme);
    let area = f.area();
    let width = area.width.saturating_sub(4).min(68);
    let height = (keys.len() as u16 + 5).min(area.height.saturating_sub(2));
    if width < 10 || height < 4 {
        return;
    }
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let mut lines = vec![
        Line::from(chip_spans(theme, "esc", "close this help")),
        Line::from(""),
    ];
    lines.extend(keys.iter().map(|(key, label)| {
        Line::from(vec![
            Span::styled(format!(" {key:>10} "), theme.accent()),
            Span::raw(format!("  {label}")),
        ])
    }));
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(theme.panel(title))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

pub fn plan_lines(plan: Option<&Plan>, theme: &Theme) -> Vec<Line<'static>> {
    plan_lines_width(plan, theme, 120)
}

pub fn plan_lines_width(plan: Option<&Plan>, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let Some(plan) = plan else {
        return vec![Line::from(Span::styled(
            "Calculating the plan...",
            theme.muted(),
        ))];
    };
    let summary = plan.summary();
    let mut counts: Vec<Span<'static>> = Vec::new();
    for (label, count, style) in [
        ("create", summary.create, theme.success()),
        ("overwrite", summary.overwrite, theme.warning()),
        ("merge", summary.merge, theme.info()),
        ("skip", summary.skip, theme.muted()),
        ("refuse", summary.refuse, theme.danger()),
    ] {
        if count > 0 {
            counts.push(Span::styled(format!("{label} {count}"), style));
            counts.push(Span::raw("   "));
        }
    }
    if counts.is_empty() {
        counts.push(Span::styled("nothing to do", theme.muted()));
        counts.push(Span::raw("   "));
    }
    counts.push(Span::styled(human_size(summary.bytes), theme.strong()));
    let mut lines = vec![Line::from(counts)];
    let mut risks: Vec<Span<'static>> = Vec::new();
    if summary.executes_code > 0 {
        risks.push(Span::styled(
            format!("{} executes code", plural(summary.executes_code, "item")),
            theme.danger(),
        ));
    }
    if summary.sensitive > 0 {
        if !risks.is_empty() {
            risks.push(Span::raw("   "));
        }
        risks.push(Span::styled(
            format!("{} sensitive (SENS)", plural(summary.sensitive, "item")),
            theme.warning(),
        ));
    }
    if !risks.is_empty() {
        risks.insert(0, Span::styled("risk  ", theme.muted()));
        lines.push(Line::from(risks));
    }
    if summary.refuse > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} Blocked: {} refused. Nothing will be written until {} resolved.",
                theme.icon(Icon::Warning),
                plural(summary.refuse, "operation"),
                if summary.refuse == 1 {
                    "it is"
                } else {
                    "they are"
                }
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
        let fixed = 9 + 6 + 10 + 10;
        let dest = truncate_left(
            &home_relative(&op.dest),
            width.saturating_sub(fixed).max(12),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<9}"), style),
            risk,
            Span::styled(
                format!(" {:<9}", sanitize(&op.provider)),
                theme.provider(&op.provider),
            ),
            Span::styled(format!("{:>9} ", human_size(op.size)), theme.muted()),
            Span::raw(dest),
        ]));
        let reason = match &op.action {
            Action::Skip(reason) | Action::Refuse(reason) => sanitize(reason),
            _ => String::new(),
        };
        let note = sanitize(&op.note);
        let extra: Vec<String> = [reason, note]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        if !extra.is_empty() {
            let reason_style = if matches!(op.action, Action::Refuse(_)) {
                theme.danger()
            } else {
                theme.muted()
            };
            lines.push(Line::from(Span::styled(
                format!("{}{}", " ".repeat(fixed), extra.join("  ")),
                reason_style,
            )));
        }
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
    let lines = plan_lines_width(plan, theme, usize::from(area.width.saturating_sub(2)));
    scroll.clamp(lines.len(), area.height);
    let title = format!(
        "Plan  {}",
        plural(plan.map(|p| p.ops.len()).unwrap_or(0), "operation")
    );
    f.render_widget(
        Paragraph::new(lines)
            .block(theme.panel(&title))
            .wrap(Wrap { trim: false })
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
    fn icon_sets_parse_from_their_names() {
        assert_eq!(IconSet::parse("ASCII"), Some(IconSet::Text));
        assert_eq!(IconSet::parse(" nerd "), Some(IconSet::Nerd));
        assert_eq!(IconSet::parse("unicode"), Some(IconSet::Unicode));
        assert_eq!(IconSet::parse("emoji"), None);
    }

    #[test]
    fn text_icons_are_ascii_and_nerd_icons_are_single_glyphs() {
        let text = Theme::new(true, IconSet::Text);
        let nerd = Theme::new(true, IconSet::Nerd);
        let unicode = Theme::new(true, IconSet::Unicode);
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
            assert_eq!(unicode.icon(icon).chars().count(), 1);
            assert!(unicode
                .icon(icon)
                .chars()
                .all(|c| ('\u{2190}'..='\u{25ff}').contains(&c)));
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
        assert!(rendered.contains("Blocked: 1 operation refused"));
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
    fn chips_wrap_only_between_whole_chips() {
        let theme = Theme::new(false, IconSet::Text);
        let keys = [
            ("space", "select"),
            ("enter", "open/close"),
            ("q", "quit"),
            ("?", "help"),
        ];
        let lines = chip_lines(&theme, &keys, 30);
        assert!(lines.len() > 1);
        for line in &lines {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.to_string())
                .collect();
            assert!(text.chars().count() <= 30, "{text}");
            for (key, label) in keys {
                assert_eq!(
                    text.contains(&format!(" {key} ")),
                    text.contains(&format!(" {label}"))
                );
            }
        }
        let single = chip_lines(&theme, &[("verylongkey", "an even longer label")], 5);
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn helpers_truncate_from_the_left_and_pluralize() {
        assert_eq!(truncate_left("/a/b/c/.claude", 8), "\u{2026}.claude");
        assert_eq!(truncate_left("short", 10), "short");
        assert_eq!(truncate_left("abcdef", 4), "\u{2026}def");
        assert_eq!(truncate_left("abcdef", 0), "");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(3, "file"), "3 files");
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
