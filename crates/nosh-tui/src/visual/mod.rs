//! The visual components of the workspace: a port of `apps/tui/src/visual.ts` (transcript,
//! inspector, research sidebar, research map) as retained models that produce styled rows.
//!
//! Specified by tui-dialogs-visual.md §5 (5.1-5.13), §8.1 (`visual.test.ts`), §10.3 (retained
//! models rebuilt only when their signature changes) and DV-2 (`visual/` gets a `mod.rs`). This file
//! holds the helpers shared by the components: `stateColor`, `MAP_TONES`, `sectionTitle`, `clipped`.
//! Track F owns the whole `visual` tree and may refine its API (only Track F consumes it).

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use ratatui::style::Color;

use crate::view_model::MapTone;

pub mod inspector;
pub mod map;
pub mod sidebar;
pub mod transcript;

pub use inspector::Inspector;
pub use map::ResearchMap;
pub use sidebar::{AttentionItem, ResearchSidebar};
pub use transcript::{Transcript, UpdateStats};

/// `stateColor`: error, success, warning, success, secondary or muted by the case-insensitive rules
/// of dialogs-visual §5.2, tested in that order (`not approved` is green, `blocked` muted).
pub fn state_color(state: &str) -> Color {
    todo!()
}

/// `MAP_TONES[tone]`: success `#7fd88f`, active `#5c9cf5`, waiting `#f5a742`, error `#e06c75`, idle
/// `#909090` (dialogs-visual §5.10).
pub fn map_tone_color(tone: MapTone) -> Color {
    todo!()
}

/// `sectionTitle`: the `SECTION_TITLES` name of a section, else its first character uppercased
/// (dialogs-visual §5.4).
pub fn section_title(section: &str) -> String {
    todo!()
}

/// `clipped`: `value` cut to `max(1, width − 1)` UTF-16 units plus `…` when longer than `width`
/// (dialogs-visual §5.11).
pub fn clipped(value: &str, width: usize) -> String {
    todo!()
}
