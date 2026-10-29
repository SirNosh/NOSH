//! The research map view: `createResearchMap` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.10-§5.13 (`MAP_TONES`, `EMPTY_MAP`, `clipped`, `ZOOMS`,
//! `update` steps 1-14, `select`, `zoomTo`, mouse zoom/drag/click, the returned API) on top of
//! `crate::map_graph`; tui-workspace.md §9.1 steps 14-17 and `checkMapMouse` (smoke).

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use crossterm::event::MouseEventKind;
use ratatui::text::Line;

use crate::js::JsValue;
use crate::map_graph::{GraphDetail, GraphDirection};

/// `EMPTY_MAP`: the text shown when the map has no roots (dialogs-visual §5.10).
pub const EMPTY_MAP: &str = "Nothing is running yet.\nMissions, research directions, autoresearch experiments and workers appear here, live, as they start.\nAsk NOSH in the conversation to propose research, or /create one.";

/// `ZOOMS` as `(label, detail, factor)`; index 1 (`100%`) is the initial zoom (dialogs-visual §5.12).
pub const ZOOMS: [(&str, GraphDetail, usize); 5] = [
    ("150%", GraphDetail::Full, 1),
    ("100%", GraphDetail::Normal, 1),
    ("50%", GraphDetail::Normal, 2),
    ("25%", GraphDetail::Normal, 4),
    ("12%", GraphDetail::Normal, 8),
];

/// The retained map view state (dialogs-visual §5.13 private state).
#[derive(Debug)]
pub struct ResearchMap {
    root_index: i64,
    selected_id: String,
    zoom: usize,
    follow: bool,
}

impl Default for ResearchMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchMap {
    /// The initial state: first root, 100%, following the selection.
    pub fn new() -> Self {
        todo!()
    }

    /// `update(map, width, height)`: lays out the current root and rebuilds the rows when the
    /// signature changed (steps 1-14).
    pub fn update(&mut self, map: Option<&JsValue>, width: u16, height: u16) {
        todo!()
    }

    /// `move(direction)`: selects the nearest box that way and follows it.
    pub fn move_selection(&mut self, direction: GraphDirection) {
        todo!()
    }

    /// `cycle(step)`: the next or previous root, reset to the top.
    pub fn cycle(&mut self, step: i64) {
        todo!()
    }

    /// `pan(rows)`: scrolls by `rows × factor` layout rows and stops following.
    pub fn pan(&mut self, rows: i64) {
        todo!()
    }

    /// `open()`: drills into an opening box (returns `""`), else returns the selected box's
    /// suggested command (`""` without a selection).
    pub fn open(&mut self) -> String {
        todo!()
    }

    /// `back()`: leaves the innermost opened Autoresearch; false when none is open.
    pub fn back(&mut self) -> bool {
        todo!()
    }

    /// A mouse event at canvas cell `(x, y)` (relative to the canvas origin; may be outside it
    /// during a drag): wheel zooms, left drag pans, a click selects. Returns whether the view changed
    /// and must be redrawn. The workspace never lets these events reach the conversation.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, x: i64, y: i64) -> bool {
        todo!()
    }

    /// The rendered rows: header, workers, canvas, then the selected-box panel.
    pub fn lines(&self) -> Vec<Line<'static>> {
        todo!()
    }

    /// The index in [`ResearchMap::lines`] of the first canvas row (to place the canvas origin).
    pub fn canvas_row(&self) -> usize {
        todo!()
    }
}
