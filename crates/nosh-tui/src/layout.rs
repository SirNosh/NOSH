//! Pure region arithmetic of the workspace (the flex/Yoga layout of `apps/tui/src/ui.ts`), and the
//! text measuring and word wrapping OpenTUI's text widgets apply.
//!
//! Specified by tui-workspace.md §6.4 (6.4.1 element tree, 6.4.2 region formulas and the Yoga
//! centering rule, 6.4.3 worked examples A-C as tests), §6.10 steps 2-11 (`compact`, `sbVisible`,
//! `sbW`, `maxHeight`, `mainWidth`), §7.6 (word wrapping) and §9.2 item 4 (thresholds);
//! tui-dialogs-visual.md §4.5-§4.6 (text widgets, cell widths). Explicit arithmetic, not ratatui
//! `Layout` constraints, so the flex rounding is reproduced. No drawing happens here.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use ratatui::layout::Rect;

/// `sbW`: 38 when `W ≥ 140`, 32 when `W ≥ 100`, else 28.
pub fn sidebar_width(width: u16) -> u16 {
    todo!()
}

/// `sbVisible = !home && W ≥ 80 && showSidebar`.
pub fn sidebar_visible(home: bool, width: u16, show_sidebar: bool) -> bool {
    todo!()
}

/// `mainWidth = max(12, W − (sbVisible ? sbW : 0) − 4)`: the clipping budget of the main column.
pub fn main_width(width: u16, sidebar_visible: bool) -> u16 {
    todo!()
}

/// The composer `maxHeight`: 3 when `H < 24`, else `max(3, min(6, floor(H / 4)))`.
pub fn composer_max_height(height: u16) -> u16 {
    todo!()
}

/// What the region formulas depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutInput {
    pub width: u16,
    pub height: u16,
    /// `view === 'chat'` with no entries and no activity.
    pub home: bool,
    /// The Ctrl+B toggle (`showSidebar`).
    pub show_sidebar: bool,
    /// The composer editor's height, `1..=maxHeight`.
    pub input_rows: u16,
    /// Wrapped status rows, `0..=3`.
    pub status_rows: u16,
}

/// The sidebar box and its content rows (workspace §6.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidebarRegions {
    /// The whole sidebar, filled `#141414`.
    pub area: Rect,
    pub title: Rect,
    pub subtitle: Rect,
    pub project: Rect,
    /// The scroll area of the research list.
    pub list: Rect,
    pub footer: Rect,
}

/// Every positioned element of one frame; `None` for elements hidden in this state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regions {
    /// Session header row 1 (bold heading).
    pub heading: Option<Rect>,
    /// Session header row 2 (context line).
    pub context: Option<Rect>,
    /// Home: the 6×35 logo (`H ≥ 20`).
    pub logo: Option<Rect>,
    /// Home: the 1×7 wordmark (`H < 20`).
    pub wordmark: Option<Rect>,
    /// Home: the tagline under the logo (`H ≥ 20`).
    pub tagline: Option<Rect>,
    /// Session: the conversation scroll area (banner, transcript, inspector or map).
    pub conversation: Option<Rect>,
    /// The prompt host (blank row, composer, hints, status).
    pub prompt: Rect,
    /// The composer box, filled `#1e1e1e`, `┃` in its first column.
    pub composer: Rect,
    /// The editor inside the composer.
    pub input: Rect,
    /// The role · model · thinking row.
    pub model_line: Rect,
    pub hints: Rect,
    pub status: Option<Rect>,
    /// Home and not compact: the tip row.
    pub home_tip: Option<Rect>,
    pub sidebar: Option<SidebarRegions>,
    /// The footer row `H − 1`, inside its 2-column padding.
    pub footer: Rect,
}

/// The regions of workspace §6.4.2 for one frame (worked examples §6.4.3).
pub fn compute(input: &LayoutInput) -> Regions {
    todo!()
}

/// The Yoga centering rule: the unrounded left of a child of `child_width` centered in a parent at
/// unrounded `parent_left` with `parent_width`; nested centering passes the unrounded value on.
pub fn centered_left(parent_left: f64, parent_width: f64, child_width: f64) -> f64 {
    todo!()
}

/// Rounds a non-negative unrounded coordinate half up, as Yoga draws it.
pub fn round_half_up(value: f64) -> u16 {
    todo!()
}

/// The footer's three parts (workspace §6.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FooterLayout {
    pub cwd: Rect,
    pub connection: Rect,
    pub version: Rect,
}

/// `version` right-aligned ending at column `W − 3`, `connection` ending 2 columns before it, `cwd`
/// filling the rest from column 2.
pub fn footer_layout(footer: Rect, connection_width: u16, version_width: u16) -> FooterLayout {
    todo!()
}

/// The model line's three parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelLineLayout {
    pub role: Rect,
    pub model: Rect,
    pub thinking: Rect,
}

/// `role` at natural width, a 1-column gap, `model` (shrinks first, minimum 0), a gap, `thinking`
/// at natural width (0 when empty).
pub fn model_line_layout(
    line: Rect,
    role_width: u16,
    model_width: u16,
    thinking_width: u16,
) -> ModelLineLayout {
    todo!()
}

/// Terminal cell width of a string (unicode-width; dialogs-visual §4.6).
pub fn cell_width(text: &str) -> usize {
    todo!()
}

/// OpenTUI word wrapping: breaks at spaces to fit `width` cells, splits a word longer than the width
/// at the width, keeps `\n` as a hard break (workspace §7.6).
pub fn wrap_words(text: &str, width: u16) -> Vec<String> {
    todo!()
}
