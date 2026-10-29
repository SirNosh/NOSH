//! Colors, the split border, the Markdown style table, the gradient wordmark and key hints: a port of
//! `apps/tui/src/theme.ts`, plus the dialog theme the workspace passes to `dialog.ts`.
//!
//! Specified by tui-workspace.md §2 (2.1 tokens, 2.2 split border, 2.3 Markdown styles, 2.4 logo,
//! 2.5-2.10 helpers with the per-column gradient test vectors) and §5.1 / §6.8 (effective dialog
//! theme); tui-dialogs-visual.md §1.1.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// `theme`: the workspace color tokens (workspace §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub panel: Color,
    pub element: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub secondary: Color,
    pub purple: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub cyan: Color,
    pub logo_start: Color,
    pub logo_end: Color,
}

/// `theme` (workspace §2.1).
pub const THEME: Theme = Theme {
    background: Color::Rgb(0x0a, 0x0a, 0x0a),
    panel: Color::Rgb(0x14, 0x14, 0x14),
    element: Color::Rgb(0x1e, 0x1e, 0x1e),
    border: Color::Rgb(0x3c, 0x3c, 0x3c),
    text: Color::Rgb(0xee, 0xee, 0xee),
    muted: Color::Rgb(0x90, 0x90, 0x90),
    accent: Color::Rgb(0x5f, 0xd4, 0xc4),
    secondary: Color::Rgb(0x5c, 0x9c, 0xf5),
    purple: Color::Rgb(0x9d, 0x7c, 0xd8),
    success: Color::Rgb(0x7f, 0xd8, 0x8f),
    warning: Color::Rgb(0xf5, 0xa7, 0x42),
    error: Color::Rgb(0xe0, 0x6c, 0x75),
    cyan: Color::Rgb(0x56, 0xb6, 0xc2),
    logo_start: Color::Rgb(0x8e, 0xe8, 0xd8),
    logo_end: Color::Rgb(0x1f, 0x9e, 0x8f),
};

/// `splitBorder`: the left-only border character, drawn on every row of the box (workspace §2.2).
pub const SPLIT_BAR: char = '┃';

/// `NOSH_LOGO_LINES`: the wordmark, 6 rows of exactly 35 columns (workspace §2.4).
pub const NOSH_LOGO_LINES: [&str; 6] = [
    "███╗   ██╗ ██████╗ ███████╗██╗  ██╗",
    "████╗  ██║██╔═══██╗██╔════╝██║  ██║",
    "██╔██╗ ██║██║   ██║███████╗███████║",
    "██║╚██╗██║██║   ██║╚════██║██╔══██║",
    "██║ ╚████║╚██████╔╝███████║██║  ██║",
    "╚═╝  ╚═══╝ ╚═════╝ ╚══════╝╚═╝  ╚═╝",
];

/// The dialog colors (`DialogTheme`); the bg token is unused by the dialogs and omitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DialogTheme {
    pub panel: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    pub selected: Color,
    pub error: Color,
}

/// The effective dialog theme in NOSH: the full override the workspace passes (workspace §5.1,
/// §6.8); `dialog.ts`'s `defaultTheme` is never visible.
pub const DIALOG_THEME: DialogTheme = DialogTheme {
    panel: Color::Rgb(0x14, 0x14, 0x14),
    text: Color::Rgb(0xee, 0xee, 0xee),
    muted: Color::Rgb(0x90, 0x90, 0x90),
    accent: Color::Rgb(0x5f, 0xd4, 0xc4),
    border: Color::Rgb(0x3c, 0x3c, 0x3c),
    selected: Color::Rgb(0x1e, 0x1e, 0x1e),
    error: Color::Rgb(0xe0, 0x6c, 0x75),
};

/// `hex`: `#rrggbb` as an RGB color (workspace §2.5).
pub fn hex(value: &str) -> Color {
    let channel = |index: usize| {
        value
            .get(index..index + 2)
            .and_then(|digits| u8::from_str_radix(digits, 16).ok())
            .unwrap_or(0)
    };
    Color::Rgb(channel(1), channel(3), channel(5))
}

fn channels(color: Color) -> [u8; 3] {
    match color {
        Color::Rgb(r, g, b) => [r, g, b],
        _ => [0, 0, 0],
    }
}

/// `mix`: per channel `round(a + (b - a) * amount)` of two RGB colors (workspace §2.6).
pub fn mix(from: Color, to: Color, amount: f64) -> Color {
    let (a, b) = (channels(from), channels(to));
    let channel = |index: usize| {
        let a = f64::from(a[index]);
        // All operands are non-negative, so `round` (half away from zero) is `Math.round`.
        (a + (f64::from(b[index]) - a) * amount).round() as u8
    };
    Color::Rgb(channel(0), channel(1), channel(2))
}

/// `gradientText`: each line colored left to right from `logo_start` to `logo_end`; spaces in the
/// background color, outline strokes mixed 55% toward it; equal-colored runs merged (workspace §2.7).
///
/// Columns are chars: the only inputs are the BMP wordmarks, where chars are UTF-16 units.
pub fn gradient_lines(lines: &[&str]) -> Vec<Line<'static>> {
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let span = (width.max(2) - 1) as f64;
    lines
        .iter()
        .map(|line| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut run_color = THEME.background;
            for (column, char) in line.chars().enumerate() {
                let base = mix(THEME.logo_start, THEME.logo_end, column as f64 / span);
                let color = if char == ' ' {
                    THEME.background
                } else if char == '█' || char.is_ascii_uppercase() {
                    base
                } else {
                    mix(base, THEME.background, 0.55)
                };
                if color != run_color && !run.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut run),
                        Style::new().fg(run_color),
                    ));
                }
                run_color = color;
                run.push(char);
            }
            if !run.is_empty() {
                spans.push(Span::styled(run, Style::new().fg(run_color)));
            }
            Line::from(spans)
        })
        .collect()
}

/// `logoText`: `gradientText(NOSH_LOGO_LINES)`, 6 rows × 35 columns.
pub fn logo() -> Vec<Line<'static>> {
    gradient_lines(&NOSH_LOGO_LINES)
}

/// `compactLogoText`: `gradientText(['N O S H'])`, 1 row × 7 columns.
pub fn compact_logo() -> Line<'static> {
    gradient_lines(&["N O S H"]).remove(0)
}

/// `keyHints`: each key in `accent`, then ` label` in `muted`, three spaces between pairs
/// (workspace §2.10).
pub fn key_hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let last = pairs.len().saturating_sub(1);
    let spans = pairs.iter().enumerate().flat_map(|(index, (key, label))| {
        let gap = if index < last { "   " } else { "" };
        [
            Span::styled(key.to_string(), Style::new().fg(THEME.accent)),
            Span::styled(format!(" {label}{gap}"), Style::new().fg(THEME.muted)),
        ]
    });
    Line::from(spans.collect::<Vec<_>>())
}

/// `createMarkdownStyle` as a lookup: the style (fg and attributes) the workspace §2.3 table gives a
/// syntax scope; scopes that resolve to no style give the `default` style (dialogs-visual §6.2).
pub fn markdown_style(scope: &str) -> Style {
    const YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
    let fg = |color| Style::new().fg(color);
    let bold = |color| fg(color).add_modifier(Modifier::BOLD);
    let italic = |color| fg(color).add_modifier(Modifier::ITALIC);
    let underline = |color| fg(color).add_modifier(Modifier::UNDERLINED);
    match scope {
        "markup.heading" | "markup.heading.1" | "markup.heading.2" | "markup.heading.3" => {
            bold(THEME.purple)
        }
        "markup.strong" | "markup.bold" => bold(THEME.warning),
        "markup.italic" | "markup.quote" => italic(YELLOW),
        "markup.list" | "function" => fg(THEME.accent),
        "markup.raw" | "string" => fg(THEME.success),
        "markup.raw.inline" => fg(THEME.success).bg(THEME.element),
        "markup.link" | "markup.link.url" => underline(THEME.accent),
        "markup.link.label" => underline(THEME.cyan),
        "label" | "operator" => fg(THEME.cyan),
        "conceal" => fg(THEME.muted),
        "comment" => italic(THEME.muted),
        "keyword" => fg(THEME.purple),
        "number" => fg(THEME.warning),
        "type" => fg(YELLOW),
        _ => fg(THEME.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn logo_lines_are_35_columns() {
        for line in NOSH_LOGO_LINES {
            assert_eq!(line.chars().count(), 35, "{line}");
            assert_eq!(line.width(), 35, "{line}");
        }
        assert!(NOSH_LOGO_LINES[0].starts_with("███╗   ██╗"));
    }

    /// The per-column vectors of workspace §2.7 (columns 0, 12, 24, 34; base and stroke).
    #[test]
    fn gradient_matches_the_ts_colors() {
        for (column, base, stroke) in [
            (0, "#8ee8d8", "#456e67"),
            (12, "#67cebe", "#34625b"),
            (24, "#40b4a4", "#22564f"),
            (34, "#1f9e8f", "#134d46"),
        ] {
            let color = mix(THEME.logo_start, THEME.logo_end, f64::from(column) / 34.0);
            assert_eq!(color, hex(base));
            assert_eq!(mix(color, THEME.background, 0.55), hex(stroke));
        }
        let compact = compact_logo();
        let colors: Vec<_> = compact.spans.iter().map(|span| span.style.fg).collect();
        let expected = [
            "#8ee8d8", "#0a0a0a", "#69cfc0", "#0a0a0a", "#44b7a7", "#0a0a0a", "#1f9e8f",
        ];
        assert_eq!(colors, expected.map(|value| Some(hex(value))));
    }
}
