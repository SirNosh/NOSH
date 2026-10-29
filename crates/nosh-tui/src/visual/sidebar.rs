//! The research sidebar list: `createResearchSidebar` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.8-§5.9 (attention block, climb rows, sections with the
//! first 8 items, empty-state pairs, element order) and §8.1 test 5 (the sidebar as `(id, Line)`
//! items with chunk colors).

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use ratatui::text::Line;

use crate::view_model::{MapLine, ViewRow};

/// `AttentionItem`: a "Needs you" entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionItem {
    pub title: String,
    pub hint: String,
}

/// The retained sidebar model.
#[derive(Debug, Default)]
pub struct ResearchSidebar {
    previous: Option<(Vec<ViewRow>, Vec<AttentionItem>, Vec<MapLine>)>,
}

impl ResearchSidebar {
    /// An empty sidebar.
    pub fn new() -> Self {
        Self::default()
    }

    /// `update(rows, attention, climb)`: returns false when the signature is unchanged.
    pub fn update(
        &mut self,
        rows: &[ViewRow],
        attention: &[AttentionItem],
        climb: &[MapLine],
    ) -> bool {
        todo!()
    }

    /// The elements in order as `(id, row)` (`side-attention`, `side-attention-0`, …,
    /// `side-climb-<i>`, `side-section-<section>`, `side-<section>-<i>`, …) at `width` cells; ids
    /// follow dialogs-visual §5.9.
    pub fn items(&self, width: u16) -> Vec<(String, Line<'static>)> {
        todo!()
    }
}
