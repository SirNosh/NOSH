//! Drawing helpers of the workspace: the ratatui side of `render()` in `apps/tui/src/ui.ts` and of
//! OpenTUI's renderer (background fill, split bar, scrollbar), plus the plain-text frame capture the
//! smoke test compares against (`captureCharFrame`).
//!
//! Specified by tui-workspace.md §6.4 (element tree), §7.5 (scrollbar glyphs), §9.3 (`frame_text`)
//! and §10.6 (drawing notes: fill every cell with `#0a0a0a`, the `┃` bar on `#1e1e1e`). The overlay dim
//! pass lives in `crate::dialog`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::theme::SPLIT_BAR;

/// The scrollbar track.
const TRACK: Color = Color::Rgb(0x25, 0x25, 0x27);
/// The scrollbar thumb.
const THUMB: Color = Color::Rgb(0x9a, 0x9e, 0xa3);

/// The screen as plain characters, rows joined with `\n` (the smoke test's `frame`). A wide
/// character's covered cells are skipped, as a terminal shows them.
pub fn frame_text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut text = String::new();
    for y in area.top()..area.bottom() {
        if y > area.top() {
            text.push('\n');
        }
        let mut covered = 0;
        for x in area.left()..area.right() {
            if covered > 0 {
                covered -= 1;
                continue;
            }
            let symbol = buffer[(x, y)].symbol();
            text.push_str(symbol);
            covered = symbol.width().saturating_sub(1);
        }
    }
    text
}

/// Fills `area` with spaces on `background`.
pub fn fill(buf: &mut Buffer, area: Rect, background: Color) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].reset();
            buf[(x, y)].set_bg(background);
        }
    }
}

/// Draws `theme::SPLIT_BAR` in `color` on every row of `area`'s first column, over `background`.
pub fn split_bar(buf: &mut Buffer, area: Rect, color: Color, background: Color) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    for y in area.top()..area.bottom() {
        buf[(area.x, y)]
            .set_char(SPLIT_BAR)
            .set_fg(color)
            .set_bg(background);
    }
}

/// Draws the 1-column scrollbar (track `#252527`, thumb `#9a9ea3`, half-cell precision) for
/// `content` rows scrolled to `offset` in a viewport of `area.height` rows (workspace §7.5).
pub fn scrollbar(buf: &mut Buffer, area: Rect, content: usize, offset: usize) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let viewport = usize::from(area.height);
    let halves = 2 * viewport;
    let range = content.saturating_sub(viewport);
    let (start, size) = if range == 0 {
        (0, halves)
    } else {
        let size = (halves * viewport / (range + viewport)).clamp(1, halves);
        let start = (offset.min(range) as f64 / range as f64 * (halves - size) as f64).round();
        (start as usize, size)
    };
    let thumb = start..start + size;
    for row in 0..viewport {
        let (top, bottom) = (thumb.contains(&(2 * row)), thumb.contains(&(2 * row + 1)));
        let glyph = match (top, bottom) {
            (true, true) => '█',
            (true, false) => '▀',
            (false, true) => '▄',
            (false, false) => ' ',
        };
        buf[(area.x, area.y + row as u16)]
            .set_char(glyph)
            .set_fg(THUMB)
            .set_bg(TRACK);
    }
}
