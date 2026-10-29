//! Drawing helpers of the workspace: the ratatui side of `render()` in `apps/tui/src/ui.ts` and of
//! OpenTUI's renderer (background fill, split bar, scrollbar), plus the plain-text frame capture the
//! smoke test compares against (`captureCharFrame`).
//!
//! Specified by tui-workspace.md §6.4 (element tree), §7.5 (scrollbar glyphs), §9.3 (`frame_text`)
//! and §10.6 (drawing notes: fill every cell with `#0a0a0a`, the `┃` bar on `#1e1e1e`). The overlay dim
//! pass lives in `crate::dialog`. Track F owns this file and may refine its API.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// The screen as plain characters, rows joined with `\n` (the smoke test's `frame`).
pub fn frame_text(buffer: &Buffer) -> String {
    todo!()
}

/// Fills `area` with spaces on `background`.
pub fn fill(buf: &mut Buffer, area: Rect, background: Color) {
    todo!()
}

/// Draws `theme::SPLIT_BAR` in `color` on every row of `area`'s first column, over `background`.
pub fn split_bar(buf: &mut Buffer, area: Rect, color: Color, background: Color) {
    todo!()
}

/// Draws the 1-column scrollbar (track `#252527`, thumb `#9a9ea3`, half-cell precision) for
/// `content` rows scrolled to `offset` in a viewport of `area.height` rows (workspace §7.5).
pub fn scrollbar(buf: &mut Buffer, area: Rect, content: usize, offset: usize) {
    todo!()
}
