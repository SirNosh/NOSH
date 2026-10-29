//! Markdown rendering of assistant messages: the behaviour of OpenTUI's `MarkdownRenderable` as the
//! transcript configures it (`conceal: true`, coalesced blocks, `columns` tables without borders),
//! reproduced with pulldown-cmark source offsets.
//!
//! Specified by tui-dialogs-visual.md §6 (6.1 block segmentation, 6.2 source-preserving highlight
//! with conceal, 6.3 code/blockquote/rule/table blocks, 6.4 streaming), §10.4 items 3-5 (plain code
//! blocks, no hyperlinks, synchronous rendering) and the style table of tui-workspace.md §2.3
//! (`crate::theme::markdown_style`). Options: tables, strikethrough, task lists.

// Remove when implemented: the todo!() body below leaves its parameters unused.
#![allow(unused_variables)]

use ratatui::text::Line;

/// The rows of one assistant message at `width` cells: prose shown as its raw source with styled and
/// concealed ranges, separate blocks laid out per §6.3, blank rows per §6.1. The `streaming` flag of
/// OpenTUI changes nothing visible and is not a parameter (§6.4).
pub fn render_markdown(source: &str, width: u16) -> Vec<Line<'static>> {
    todo!()
}
