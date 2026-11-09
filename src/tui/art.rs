use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::theme::{IconSet, Theme};

const UPPER_HALF: &str = "\u{2580}";
const LOWER_HALF: &str = "\u{2584}";
const FULL_BLOCK: &str = "\u{2588}";
const LIGHT_SHADE: &str = "\u{2591}";

pub const LOGO_WIDTH: u16 = 24;
pub const LOGO_HEIGHT: u16 = 7;
pub const BADGE_WIDTH: u16 = 8;
pub const BADGE_HEIGHT: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtKind {
    Scanning,
    Success,
    Warning,
    Error,
    Empty,
}

const LOGO_0: [&str; 14] = [
    "....bbbbbbbbbbbbbbbb....",
    "...bhhhhhhhhhhhhhhhhb...",
    "..bhttttttttttttttttdb..",
    "..btsbbbbbbbbbbbbbbtdb..",
    "..btbddddddddddddddbdb..",
    "..btbdppppppppcaaadbdb..",
    "..btbdpaaaaaaccaakdbdb..",
    "..bbbbsbbbbbcckckbbbbbb.",
    "..btbdpaaaaaaccaakdbdb..",
    "..btbdppppppppcaaadbdb..",
    "..btbddddddddddddddbdb..",
    "..btbbbbbbbbbbbbbbbbdb..",
    "...bddddddddddddddddb...",
    "....bbbbbbbbbbbbbbbb....",
];

const LOGO_1: [&str; 14] = [
    "....bbbbbbbbbbbbbbbb....",
    "...bhhhhhhhhhhhhhhhhb...",
    "..bhshhhhhhhhhhhhhhtdb..",
    "..bthbbbbbbbbbbbbbbtdb..",
    "..btbddddddddddddddbdb..",
    "..btbdppppppppaccadbdb..",
    "..btbdcccccccaakckdbdb..",
    "..bbbbhbbbbbcckwkcbbbbb.",
    "..btbdcccccccaakckdbdb..",
    "..btbdppppppppaccadbdb..",
    "..btbddddddddddddddbdb..",
    "..btbbbbbbbbbbbbbbbbdb..",
    "...bddddddddddddddddb...",
    "....bbbbbbbbbbbbbbbb....",
];

const LOGO_2: [&str; 14] = [
    "....bbbbbbbbbbbbbbbb....",
    "...bhhhhhhhhhhhhhhhhb...",
    "..bhtttttshhhhhhhhhtdb..",
    "..btbbbbbbbbbbbbbbbtdb..",
    "..btbddddddddddddddbdb..",
    "..btbdppppppppcckcdbdb..",
    "..btbdaaaaaaacckwkdbdb..",
    "..bbbbtbbbbbcakwwwabbbb.",
    "..btbdaaaaaaacckwkdbdb..",
    "..btbdppppppppcckcdbdb..",
    "..btbddddddddddddddbdb..",
    "..btbbbbbbbbbbbbbbbbdb..",
    "...bddddddddddddddddb...",
    "....bbbbbbbbbbbbbbbb....",
];

const LOGO_3: [&str; 14] = [
    "....bbbbbbbbbbbbbbbb....",
    "...bhhhhhhhhhhhhhhhhb...",
    "..bhttttttttttshhhhtdb..",
    "..btbbbbbbbbbbbbbbbtdb..",
    "..btbddddddddddddddbdb..",
    "..btbdppppppppaccadbdb..",
    "..btbdcccccccaakckdbdb..",
    "..bbbbtbbbbbcckwkcbbbbb.",
    "..btbdcccccccaakckdbdb..",
    "..btbdppppppppaccadbdb..",
    "..btbddddddddddddddbdb..",
    "..btbbbbbbbbbbbbbbbbdb..",
    "...bddddddddddddddddb...",
    "....bbbbbbbbbbbbbbbb....",
];

const LOGO_FRAMES: [[&str; 14]; 4] = [LOGO_0, LOGO_1, LOGO_2, LOGO_3];

const SCANNING_0: [&str; 8] = [
    "..bbbb..", ".bccccb.", "bckwwwcb", "bcwwwwcb", "bccccccb", ".bccccb.", "..bbbbbb", ".....bbb",
];

const SCANNING_1: [&str; 8] = [
    "..bbbb..", ".bccccb.", "bcccckwb", "bccccwwb", "bccccccb", ".bccccb.", "..bbbbbb", ".....bbb",
];

const SCANNING_2: [&str; 8] = [
    "..bbbb..", ".bccccb.", "bccccccb", "bccccwwb", "bcccckwb", ".bccccb.", "..bbbbbb", ".....bbb",
];

const SCANNING_3: [&str; 8] = [
    "..bbbb..", ".bccccb.", "bccccccb", "bcwwwwcb", "bckwwwcb", ".bccccb.", "..bbbbbb", ".....bbb",
];

const SCANNING_FRAMES: [[&str; 8]; 4] = [SCANNING_0, SCANNING_1, SCANNING_2, SCANNING_3];

const SUCCESS_0: [&str; 8] = [
    "..gggg..", ".gggggg.", "ggggggwg", "gggggwwg", "gwgggwg.", "gwwgwg..", ".gwwg...", "..gg....",
];

const SUCCESS_1: [&str; 8] = [
    "..gggg..", ".gggggg.", "ggggggsg", "gggggssg", "gsgggsg.", "gssgsg..", ".gssg...", "..gg....",
];

const SUCCESS_FRAMES: [[&str; 8]; 2] = [SUCCESS_0, SUCCESS_1];

const WARNING_0: [&str; 8] = [
    "...yy...", "..yyyy..", "..ywwy..", ".yywwyy.", ".yywwyy.", "yyyyyyyy", "yyywwyyy", "yyyyyyyy",
];

const WARNING_1: [&str; 8] = [
    "...yy...", "..yyyy..", "..yssy..", ".yyssyy.", ".yyssyy.", "yyyyyyyy", "yyyssyyy", "yyyyyyyy",
];

const WARNING_FRAMES: [[&str; 8]; 2] = [WARNING_0, WARNING_1];

const ERROR_0: [&str; 8] = [
    "rr....rr", "rrr..rrr", ".rrwwrr.", "..rwwr..", "..rwwr..", ".rrwwrr.", "rrr..rrr", "rr....rr",
];

const ERROR_1: [&str; 8] = [
    "rr....rr", "rrr..rrr", ".rrssrr.", "..rssr..", "..rssr..", ".rrssrr.", "rrr..rrr", "rr....rr",
];

const ERROR_FRAMES: [[&str; 8]; 2] = [ERROR_0, ERROR_1];

const EMPTY_0: [&str; 8] = [
    ".bbbbbb.", "bttttttb", "bddddddB", "bddddddB", "bddddddB", "bddddddB", "bttttttb", ".bbbbbb.",
];

const EMPTY_1: [&str; 8] = [
    ".bbbbbb.", "bhhhhhhb", "bddddddB", "bddddddB", "bddddddB", "bddddddB", "bttttttb", ".bbbbbb.",
];

const EMPTY_FRAMES: [[&str; 8]; 2] = [EMPTY_0, EMPTY_1];

fn pixel_color(ch: char) -> Option<Color> {
    match ch {
        '.' => None,
        'd' => Some(Color::Indexed(236)),
        'p' => Some(Color::Indexed(238)),
        'b' => Some(Color::Indexed(240)),
        'B' => Some(Color::Indexed(242)),
        't' => Some(Color::Indexed(245)),
        'h' => Some(Color::Indexed(250)),
        's' => Some(Color::Indexed(255)),
        'c' => Some(Color::Indexed(45)),
        'a' => Some(Color::Indexed(117)),
        'k' => Some(Color::Indexed(220)),
        'w' => Some(Color::Indexed(231)),
        'g' => Some(Color::Indexed(78)),
        'y' => Some(Color::Indexed(214)),
        'r' => Some(Color::Indexed(203)),
        _ => None,
    }
}

fn is_bright_pixel(ch: char) -> bool {
    matches!(
        ch,
        'b' | 'B' | 't' | 'h' | 's' | 'c' | 'a' | 'k' | 'w' | 'g' | 'y' | 'r'
    )
}

fn render_sprite(rows: &[&str], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(rows.len().div_ceil(2));
    let mut index = 0;
    while index < rows.len() {
        let top_row = rows[index];
        let bottom_row = rows.get(index + 1).copied().unwrap_or("");
        let width = top_row.len().max(bottom_row.len());
        let mut spans = Vec::with_capacity(width);
        let mut top_chars = top_row.chars();
        let mut bottom_chars = bottom_row.chars();
        for _ in 0..width {
            let top = top_chars.next().unwrap_or('.');
            let bottom = bottom_chars.next().unwrap_or('.');
            if theme.color {
                match (pixel_color(top), pixel_color(bottom)) {
                    (None, None) => spans.push(Span::raw(" ")),
                    (Some(fg), None) => {
                        spans.push(Span::styled(UPPER_HALF, Style::default().fg(fg)))
                    }
                    (None, Some(fg)) => {
                        spans.push(Span::styled(LOWER_HALF, Style::default().fg(fg)))
                    }
                    (Some(fg), Some(bg)) => {
                        spans.push(Span::styled(UPPER_HALF, Style::default().fg(fg).bg(bg)))
                    }
                }
            } else {
                let top_on = is_bright_pixel(top);
                let bottom_on = is_bright_pixel(bottom);
                let glyph = match (top_on, bottom_on) {
                    (false, false) => {
                        if (top != '.' || bottom != '.') && theme.icons == IconSet::Nerd {
                            LIGHT_SHADE
                        } else {
                            " "
                        }
                    }
                    (true, false) => UPPER_HALF,
                    (false, true) => LOWER_HALF,
                    (true, true) => FULL_BLOCK,
                };
                spans.push(Span::raw(glyph));
            }
        }
        lines.push(Line::from(spans));
        index += 2;
    }
    lines
}

pub fn logo(theme: &Theme, frame: usize) -> Vec<Line<'static>> {
    let sprite = &LOGO_FRAMES[frame % LOGO_FRAMES.len()];
    render_sprite(sprite, theme)
}

pub fn badge(kind: ArtKind, theme: &Theme, frame: usize) -> Vec<Line<'static>> {
    let sprite: &[&str] = match kind {
        ArtKind::Scanning => &SCANNING_FRAMES[frame % SCANNING_FRAMES.len()],
        ArtKind::Success => &SUCCESS_FRAMES[frame % SUCCESS_FRAMES.len()],
        ArtKind::Warning => &WARNING_FRAMES[frame % WARNING_FRAMES.len()],
        ArtKind::Error => &ERROR_FRAMES[frame % ERROR_FRAMES.len()],
        ArtKind::Empty => &EMPTY_FRAMES[frame % EMPTY_FRAMES.len()],
    };
    render_sprite(sprite, theme)
}

pub fn size(lines: &[Line<'_>]) -> (u16, u16) {
    let height = lines.len().min(u16::MAX as usize) as u16;
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .min(u16::MAX as usize) as u16;
    (width, height)
}

pub fn logo_size() -> (u16, u16) {
    (LOGO_WIDTH, LOGO_HEIGHT)
}

pub fn badge_size() -> (u16, u16) {
    (BADGE_WIDTH, BADGE_HEIGHT)
}

pub fn fits(area: Rect, lines: &[Line<'_>]) -> bool {
    let (width, height) = size(lines);
    area.width >= width && area.height >= height
}

pub fn center_rect(area: Rect, width: u16, height: u16) -> Option<Rect> {
    if area.width < width || area.height < height {
        return None;
    }
    Some(Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    })
}

pub fn render_centered(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> bool {
    let (width, height) = size(&lines);
    let Some(target) = center_rect(area, width, height) else {
        return false;
    };
    f.render_widget(Paragraph::new(lines), target);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    const ALL_KINDS: [ArtKind; 5] = [
        ArtKind::Scanning,
        ArtKind::Success,
        ArtKind::Warning,
        ArtKind::Error,
        ArtKind::Empty,
    ];

    fn assert_uniform_sprite(rows: &[&str], expected_width: usize, expected_height: usize) {
        assert_eq!(rows.len(), expected_height);
        for row in rows {
            assert_eq!(row.len(), expected_width);
            assert!(row.chars().all(|c| c == '.' || pixel_color(c).is_some()));
        }
    }

    #[test]
    fn every_sprite_row_has_the_same_width() {
        for sprite in &LOGO_FRAMES {
            assert_uniform_sprite(sprite, 24, 14);
        }
        for sprite in &SCANNING_FRAMES {
            assert_uniform_sprite(sprite, 8, 8);
        }
        for sprite in &SUCCESS_FRAMES {
            assert_uniform_sprite(sprite, 8, 8);
        }
        for sprite in &WARNING_FRAMES {
            assert_uniform_sprite(sprite, 8, 8);
        }
        for sprite in &ERROR_FRAMES {
            assert_uniform_sprite(sprite, 8, 8);
        }
        for sprite in &EMPTY_FRAMES {
            assert_uniform_sprite(sprite, 8, 8);
        }
    }

    #[test]
    fn sizes_match_rendered_dimensions() {
        let theme = Theme::new(true, IconSet::Nerd);
        let logo_lines = logo(&theme, 0);
        assert_eq!(size(&logo_lines), logo_size());
        assert_eq!(logo_size(), (24, 7));
        for kind in ALL_KINDS {
            let badge_lines = badge(kind, &theme, 0);
            assert_eq!(size(&badge_lines), badge_size());
            assert_eq!(badge_size(), (8, 4));
        }
    }

    #[test]
    fn animations_change_across_frames() {
        let theme = Theme::new(true, IconSet::Nerd);
        assert_ne!(logo(&theme, 0), logo(&theme, 1));
        for kind in ALL_KINDS {
            assert_ne!(badge(kind, &theme, 0), badge(kind, &theme, 1));
        }
    }

    #[test]
    fn renders_at_tiny_and_large_terminal_sizes_without_panicking() {
        for color in [true, false] {
            for icons in [IconSet::Text, IconSet::Nerd] {
                let theme = Theme::new(color, icons);
                for (width, height) in [(1, 1), (20, 6), (120, 40)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    for frame in 0..4 {
                        terminal
                            .draw(|f| {
                                let area = f.area();
                                let logo_lines = logo(&theme, frame);
                                f.render_widget(Paragraph::new(logo_lines.clone()), area);
                                let _ = render_centered(f, area, logo_lines);
                                for kind in ALL_KINDS {
                                    let badge_lines = badge(kind, &theme, frame);
                                    f.render_widget(Paragraph::new(badge_lines.clone()), area);
                                    let _ = render_centered(f, area, badge_lines);
                                }
                            })
                            .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn no_color_output_contains_no_colors() {
        for icons in [IconSet::Text, IconSet::Nerd] {
            let theme = Theme::new(false, icons);
            for frame in 0..4 {
                for line in logo(&theme, frame) {
                    assert_eq!(line.style.fg, None);
                    assert_eq!(line.style.bg, None);
                    for span in line.spans {
                        assert_eq!(span.style.fg, None);
                        assert_eq!(span.style.bg, None);
                    }
                }
                for kind in ALL_KINDS {
                    for line in badge(kind, &theme, frame) {
                        assert_eq!(line.style.fg, None);
                        assert_eq!(line.style.bg, None);
                        for span in line.spans {
                            assert_eq!(span.style.fg, None);
                            assert_eq!(span.style.bg, None);
                        }
                    }
                }
            }
            let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
            terminal
                .draw(|f| {
                    f.render_widget(Paragraph::new(logo(&theme, 0)), f.area());
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for cell in buffer.content() {
                assert_eq!(cell.fg, Color::Reset);
                assert_eq!(cell.bg, Color::Reset);
            }
        }
    }
}
