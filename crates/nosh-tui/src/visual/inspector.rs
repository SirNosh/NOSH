//! The inspector cards of non-chat views: `createInspector` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.4-§5.7 (section headings, `commandId`, `MAX_CARD_FIELDS`,
//! card layout, `+N more fields`, the empty fallback) and §10.3 (rebuilt only when `(rows, fallback)`
//! changes).

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use ratatui::text::Line;

use crate::view_model::ViewRow;

/// `MAX_CARD_FIELDS`.
pub const MAX_CARD_FIELDS: usize = 8;

/// The retained inspector model.
#[derive(Debug, Default)]
pub struct Inspector {
    previous: Option<(Vec<ViewRow>, String)>,
}

impl Inspector {
    /// An empty inspector.
    pub fn new() -> Self {
        Self::default()
    }

    /// `update(rows, fallback)`: returns false when the signature is unchanged (dialogs-visual §5.7
    /// step 1).
    pub fn update(&mut self, rows: &[ViewRow], fallback: &str) -> bool {
        todo!()
    }

    /// The rendered rows at `width` cells.
    pub fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        todo!()
    }
}
