//! Colors, the split border, the Markdown style table, the gradient wordmark and key hints: a port of
//! `apps/tui/src/theme.ts`, plus the dialog theme the workspace passes to `dialog.ts`.
//!
//! Specified by tui-workspace.md §2 (2.1 tokens, 2.2 split border, 2.3 Markdown styles, 2.4 logo,
//! 2.5-2.10 helpers with the per-column gradient test vectors) and §5.1 / §6.8 (effective dialog
//! theme); tui-dialogs-visual.md §1.1.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use ratatui::style::{Color, Style};
use ratatui::text::Line;

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
    todo!()
}

/// `mix`: per channel `round(a + (b - a) * amount)` of two RGB colors (workspace §2.6).
pub fn mix(from: Color, to: Color, amount: f64) -> Color {
    todo!()
}

/// `gradientText`: each line colored left to right from `logo_start` to `logo_end`; spaces in the
/// background color, outline strokes mixed 55% toward it; equal-colored runs merged (workspace §2.7).
pub fn gradient_lines(lines: &[&str]) -> Vec<Line<'static>> {
    todo!()
}

/// `logoText`: `gradientText(NOSH_LOGO_LINES)`, 6 rows × 35 columns.
pub fn logo() -> Vec<Line<'static>> {
    todo!()
}

/// `compactLogoText`: `gradientText(['N O S H'])`, 1 row × 7 columns.
pub fn compact_logo() -> Line<'static> {
    todo!()
}

/// `keyHints`: each key in `accent`, then ` label` in `muted`, three spaces between pairs
/// (workspace §2.10).
pub fn key_hints(pairs: &[(&str, &str)]) -> Line<'static> {
    todo!()
}

/// `createMarkdownStyle` as a lookup: the style (fg and attributes) the workspace §2.3 table gives a
/// syntax scope; scopes that resolve to no style give the `default` style (dialogs-visual §6.2).
pub fn markdown_style(scope: &str) -> Style {
    todo!()
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
}
