//! Pure region arithmetic of the workspace (the flex/Yoga layout of `apps/tui/src/ui.ts`), and the
//! text measuring and word wrapping OpenTUI's text widgets apply.
//!
//! Specified by tui-workspace.md §6.4 (6.4.1 element tree, 6.4.2 region formulas and the Yoga
//! centering rule, 6.4.3 worked examples A-C as tests), §6.10 steps 2-11 (`compact`, `sbVisible`,
//! `sbW`, `maxHeight`, `mainWidth`), §7.6 (word wrapping) and §9.2 item 4 (thresholds);
//! tui-dialogs-visual.md §4.5-§4.6 (text widgets, cell widths). Explicit arithmetic, not ratatui
//! `Layout` constraints, so the flex rounding is reproduced. No drawing happens here.

use std::ops::Range;

use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The tab stop text widgets expand `\t` to (OpenTUI's text buffers expand tabs; the width is our
/// choice, used by every wrapped text and the editor alike).
pub const TAB_WIDTH: usize = 4;

/// `sbW`: 38 when `W ≥ 140`, 32 when `W ≥ 100`, else 28.
pub fn sidebar_width(width: u16) -> u16 {
    match width {
        140.. => 38,
        100.. => 32,
        _ => 28,
    }
}

/// `sbVisible = !home && W ≥ 80 && showSidebar`.
pub fn sidebar_visible(home: bool, width: u16, show_sidebar: bool) -> bool {
    !home && width >= 80 && show_sidebar
}

/// `mainWidth = max(12, W − (sbVisible ? sbW : 0) − 4)`: the clipping budget of the main column.
pub fn main_width(width: u16, sidebar_visible: bool) -> u16 {
    let sidebar = if sidebar_visible {
        sidebar_width(width)
    } else {
        0
    };
    width.saturating_sub(sidebar + 4).max(12)
}

/// The composer `maxHeight`: 3 when `H < 24`, else `max(3, min(6, floor(H / 4)))`.
pub fn composer_max_height(height: u16) -> u16 {
    if height < 24 {
        3
    } else {
        (height / 4).clamp(3, 6)
    }
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
///
/// Rectangles are clipped to their part of the screen (the body above the footer row, or the
/// footer row), so content that overflows a tiny terminal is cut instead of drawn out of bounds.
pub fn compute(input: &LayoutInput) -> Regions {
    let (width, height) = (i32::from(input.width), i32::from(input.height));
    let body = height - 1;
    let clip = |x: i32, y: i32, w: i32, h: i32| -> Rect {
        let (left, top) = (x.clamp(0, width), y.clamp(0, body.max(0)));
        let right = (x + w.max(0)).clamp(left, width);
        let bottom = (y + h.max(0)).clamp(top, body.max(0));
        Rect::new(
            left as u16,
            top as u16,
            (right - left) as u16,
            (bottom - top) as u16,
        )
    };
    let compact = input.height < 24;
    let home = input.home;
    let sidebar = sidebar_visible(home, input.width, input.show_sidebar);
    let sidebar_cols = if sidebar {
        i32::from(sidebar_width(input.width))
    } else {
        0
    };
    let inner = (width - sidebar_cols - 4).max(0);
    let pad = if compact && !home { 0 } else { 1 };
    let input_rows = i32::from(input.input_rows);
    let status_rows = i32::from(input.status_rows);
    let composer_rows = pad + input_rows + 2 + pad;
    let prompt_rows = 1 + composer_rows + 2 + status_rows;

    let mut regions = Regions {
        heading: None,
        context: None,
        logo: None,
        wordmark: None,
        tagline: None,
        conversation: None,
        prompt: Rect::default(),
        composer: Rect::default(),
        input: Rect::default(),
        model_line: Rect::default(),
        hints: Rect::default(),
        status: None,
        home_tip: None,
        sidebar: None,
        footer: Rect::new(
            2,
            input.height.saturating_sub(1),
            input.width.saturating_sub(4),
            1,
        )
        .intersection(Rect::new(0, 0, input.width, input.height)),
    };
    let (x, prompt_width, prompt_top) = if home {
        let hero_rows = if height >= 20 { 8 } else { 1 };
        let margin = if compact { 1 } else { 2 };
        let tip_rows = if compact { 0 } else { 2 };
        let free = body - (hero_rows + margin + prompt_rows + tip_rows);
        // Yoga gives the odd row of the two growing spacers to the top one.
        let top = if free > 0 { (free + 1) / 2 } else { 0 };
        let host_width = inner.min(75);
        let host_left = centered_left(2.0, f64::from(inner), f64::from(host_width));
        let centered = |child: i32| {
            round(centered_left(
                host_left,
                f64::from(host_width),
                f64::from(child),
            ))
        };
        if height >= 20 {
            regions.logo = Some(clip(centered(35), top, 35, 6));
            regions.tagline = Some(clip(centered(34), top + 7, 34, 1));
        } else {
            regions.wordmark = Some(clip(centered(7), top, 7, 1));
        }
        let x = round(host_left);
        let prompt_top = top + hero_rows + margin;
        if !compact {
            regions.home_tip = Some(clip(x, prompt_top + prompt_rows + 1, host_width, 1));
        }
        (x, host_width, prompt_top)
    } else {
        let prompt_top = body - prompt_rows;
        regions.heading = Some(clip(2, 1, inner, 1));
        regions.context = Some(clip(2, 2, inner, 1));
        regions.conversation = Some(clip(2, 3, inner, prompt_top - 3));
        (2, inner, prompt_top)
    };
    let composer_top = prompt_top + 1;
    regions.prompt = clip(x, prompt_top, prompt_width, prompt_rows);
    regions.composer = clip(x, composer_top, prompt_width, composer_rows);
    regions.input = clip(x + 3, composer_top + pad, prompt_width - 5, input_rows);
    regions.model_line = clip(
        x + 3,
        composer_top + pad + input_rows + 1,
        prompt_width - 5,
        1,
    );
    regions.hints = clip(x, composer_top + composer_rows + 1, prompt_width, 1);
    if status_rows > 0 {
        regions.status = Some(clip(
            x,
            composer_top + composer_rows + 2,
            prompt_width,
            status_rows,
        ));
    }
    if sidebar {
        let left = width - sidebar_cols;
        let (content, content_width) = (left + 2, sidebar_cols - 4);
        regions.sidebar = Some(SidebarRegions {
            area: clip(left, 0, sidebar_cols, body),
            title: clip(content, 1, content_width, 1),
            subtitle: clip(content, 2, content_width, 1),
            project: clip(content, 3, content_width, 1),
            list: clip(content, 4, content_width, body - 6),
            footer: clip(content, body - 2, content_width, 1),
        });
    }
    regions
}

/// [`round_half_up`] for coordinates that may be negative (overflowing children).
fn round(value: f64) -> i32 {
    (value + 0.5).floor() as i32
}

/// The Yoga centering rule: the unrounded left of a child of `child_width` centered in a parent at
/// unrounded `parent_left` with `parent_width`; nested centering passes the unrounded value on.
pub fn centered_left(parent_left: f64, parent_width: f64, child_width: f64) -> f64 {
    parent_left + (parent_width - child_width) / 2.0
}

/// Rounds a non-negative unrounded coordinate half up, as Yoga draws it.
pub fn round_half_up(value: f64) -> u16 {
    round(value).max(0) as u16
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
    let version_left = footer.right().saturating_sub(version_width).max(footer.x);
    let connection_left = version_left
        .saturating_sub(connection_width + 2)
        .max(footer.x);
    let row = |left: u16, width: u16| Rect::new(left, footer.y, width, footer.height);
    FooterLayout {
        cwd: row(footer.x, connection_left - footer.x),
        connection: row(
            connection_left,
            connection_width.min(version_left - connection_left),
        ),
        version: row(
            version_left,
            version_width.min(footer.right() - version_left),
        ),
    }
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
    let fixed = role_width.saturating_add(thinking_width).saturating_add(2);
    let model_width = model_width.min(line.width.saturating_sub(fixed));
    let model_left = line.x.saturating_add(role_width).saturating_add(1);
    let thinking_left = model_left.saturating_add(model_width).saturating_add(1);
    // Natural widths past the line's end are cut off.
    let part =
        |left: u16, width: u16| Rect::new(left, line.y, width, line.height).intersection(line);
    ModelLineLayout {
        role: part(line.x, role_width),
        model: part(model_left, model_width),
        thinking: part(thinking_left, thinking_width),
    }
}

/// Terminal cell width of a string (unicode-width; dialogs-visual §4.6).
pub fn cell_width(text: &str) -> usize {
    text.width()
}

/// OpenTUI word wrapping: breaks at spaces to fit `width` cells, splits a word longer than the width
/// at the width, keeps `\n` as a hard break (workspace §7.6).
///
/// A break consumes the one space it happens at; tabs expand to [`TAB_WIDTH`] spaces. Every hard
/// line gives at least one row, so `""` wraps to `[""]`.
pub fn wrap_words(text: &str, width: u16) -> Vec<String> {
    let tab = " ".repeat(TAB_WIDTH);
    text.split('\n')
        .flat_map(|line| {
            wrap_ranges(line, width)
                .into_iter()
                .map(|range| line[range].replace('\t', &tab))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The cells a character takes in wrapped text: [`TAB_WIDTH`] for a tab, else its unicode width.
pub fn char_cells(char: char) -> usize {
    if char == '\t' {
        TAB_WIDTH
    } else {
        char.width().unwrap_or(0)
    }
}

/// The rows [`wrap_words`] makes of one hard line (no `\n`), as byte ranges of `line`. The space a
/// break consumes belongs to neither row; the editor maps its cursor through these ranges.
pub fn wrap_ranges(line: &str, width: u16) -> Vec<Range<usize>> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let (mut start, mut used, mut position) = (0, 0, 0);
    for (index, word) in line.split(' ').enumerate() {
        let word_start = position;
        position += word.len() + 1;
        let cells: usize = word.chars().map(char_cells).sum();
        if index > 0 {
            if used + 1 + cells <= width {
                used += 1 + cells;
                continue;
            }
            rows.push(start..word_start - 1);
            (start, used) = (word_start, 0);
        }
        for (offset, char) in word.char_indices() {
            let char_width = char_cells(char);
            if used + char_width > width && used > 0 {
                rows.push(start..word_start + offset);
                (start, used) = (word_start + offset, 0);
            }
            used += char_width;
        }
    }
    rows.push(start..line.len());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(width: u16, height: u16, home: bool) -> LayoutInput {
        LayoutInput {
            width,
            height,
            home,
            show_sidebar: true,
            input_rows: 1,
            status_rows: 0,
        }
    }

    /// Worked example A of workspace §6.4.3: home, 100×30.
    #[test]
    fn home_100_by_30() {
        let regions = compute(&input(100, 30, true));
        assert_eq!(regions.logo, Some(Rect::new(33, 5, 35, 6)));
        assert_eq!(regions.tagline, Some(Rect::new(33, 12, 34, 1)));
        assert_eq!(regions.prompt.y, 15);
        assert_eq!(regions.composer, Rect::new(13, 16, 75, 5));
        assert_eq!(regions.input, Rect::new(16, 17, 70, 1));
        assert_eq!(regions.model_line, Rect::new(16, 19, 70, 1));
        assert_eq!(regions.hints, Rect::new(13, 22, 75, 1));
        assert_eq!(regions.home_tip, Some(Rect::new(13, 24, 75, 1)));
        assert_eq!(
            (regions.heading, regions.conversation, regions.sidebar),
            (None, None, None)
        );
        assert_eq!(regions.footer, Rect::new(2, 29, 96, 1));
        let footer = footer_layout(regions.footer, 12, 11);
        assert_eq!((footer.connection.x, footer.connection.right()), (73, 85));
        assert_eq!((footer.version.x, footer.version.right()), (87, 98));
    }

    /// Worked example B: a session with the sidebar, 100×30.
    #[test]
    fn session_with_sidebar_100_by_30() {
        let regions = compute(&input(100, 30, false));
        assert_eq!(main_width(100, true), 64);
        assert_eq!(regions.heading, Some(Rect::new(2, 1, 64, 1)));
        assert_eq!(regions.conversation, Some(Rect::new(2, 3, 64, 18)));
        assert_eq!(regions.prompt, Rect::new(2, 21, 64, 8));
        assert_eq!(regions.composer, Rect::new(2, 22, 64, 5));
        assert_eq!(regions.input, Rect::new(5, 23, 59, 1));
        let sidebar = regions.sidebar.unwrap();
        assert_eq!(sidebar.area, Rect::new(68, 0, 32, 29));
        assert_eq!(sidebar.title, Rect::new(70, 1, 28, 1));
        assert_eq!(sidebar.list, Rect::new(70, 4, 28, 23));
        assert_eq!(sidebar.footer, Rect::new(70, 27, 28, 1));
    }

    /// Worked example C: the map view, 60×20 (compact, no sidebar).
    #[test]
    fn compact_map_60_by_20() {
        let regions = compute(&input(60, 20, false));
        assert_eq!(regions.sidebar, None);
        assert_eq!(regions.conversation, Some(Rect::new(2, 3, 56, 10)));
        assert_eq!(regions.prompt, Rect::new(2, 13, 56, 6));
        assert_eq!(regions.composer, Rect::new(2, 14, 56, 3));
        assert_eq!(regions.input.y, 14);
        assert_eq!(regions.model_line.y, 16);
        assert_eq!(regions.hints.y, 18);
    }
}
