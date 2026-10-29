//! The visual components of the workspace: a port of `apps/tui/src/visual.ts` (transcript,
//! inspector, research sidebar, research map) as retained models that produce styled rows.
//!
//! Specified by tui-dialogs-visual.md §5 (5.1-5.13), §8.1 (`visual.test.ts`), §10.3 (retained
//! models rebuilt only when their signature changes) and DV-2 (`visual/` gets a `mod.rs`). This file
//! holds the helpers shared by the components: `stateColor`, `MAP_TONES`, `sectionTitle`, `clipped`,
//! and the `text()` widget as word-wrapped rows.

use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::js::{self, js_lowercase};
use crate::layout::wrap_words;
use crate::theme::THEME;
use crate::view_model::{MapTone, safe_text_str};

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
    let state = js_lowercase(state);
    let any = |words: &[&str]| words.iter().any(|word| state.contains(word));
    if any(&["fail", "error", "reject", "cancel", "stop"]) {
        THEME.error
    } else if any(&["approved", "accept"]) {
        THEME.success
    } else if any(&["wait", "approv", "pending", "pause", "review", "draft"]) {
        THEME.warning
    } else if any(&["complete", "success", "ready", "accept", "connected"]) {
        THEME.success
    } else if any(&["running", "working", "active", "stream"]) {
        THEME.secondary
    } else {
        THEME.muted
    }
}

/// `MAP_TONES[tone]`: success `#7fd88f`, active `#5c9cf5`, waiting `#f5a742`, error `#e06c75`, idle
/// `#909090` (dialogs-visual §5.10).
pub fn map_tone_color(tone: MapTone) -> Color {
    match tone {
        MapTone::Success => THEME.success,
        MapTone::Active => THEME.secondary,
        MapTone::Waiting => THEME.warning,
        MapTone::Error => THEME.error,
        MapTone::Idle => THEME.muted,
    }
}

/// `sectionTitle`: the `SECTION_TITLES` name of a section, else its first character uppercased
/// (dialogs-visual §5.4).
pub fn section_title(section: &str) -> String {
    let known = match section {
        "missions" => "Missions",
        "directions" => "Directions",
        "autoresearch" => "Autoresearch",
        "agents" => "Agents",
        "jobs" => "Jobs",
        "approvals" => "Approvals",
        "contract" => "Project contract",
        "projects" => "Projects",
        "models" => "Models",
        "action" => "Staged action",
        "result" => "Result",
        "output" => "Job output",
        _ => {
            let mut chars = section.chars();
            return chars.next().map_or(String::new(), |first| {
                first.to_uppercase().chain(chars).collect()
            });
        }
    };
    known.into()
}

/// `clipped`: `value` cut to `max(1, width − 1)` UTF-16 units plus `…` when longer than `width`
/// (dialogs-visual §5.11).
pub fn clipped(value: &str, width: usize) -> String {
    if js::length(value) > width {
        format!("{}…", js::slice_to(value, width.saturating_sub(1).max(1)))
    } else {
        value.to_owned()
    }
}

/// `text(renderer, id, content, color)` as rows: `safeText(content)` word-wrapped to `width`.
pub(crate) fn text_rows(content: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    wrap_words(&safe_text_str(content), width)
        .into_iter()
        .map(|row| Line::styled(row, style))
        .collect()
}
