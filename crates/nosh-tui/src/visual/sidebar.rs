//! The research sidebar list: `createResearchSidebar` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.8-§5.9 (attention block, climb rows, sections with the
//! first 8 items, empty-state pairs, element order) and §8.1 test 5 (the sidebar as `(id, Line)`
//! items with chunk colors). The blank row above each heading (`marginTop 1`) is an item with an
//! empty id; a wrapped text gives one item per row, all with its id.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::js::js_lowercase;
use crate::theme::THEME;
use crate::view_model::{MapLine, ViewRow, safe_text_str};
use crate::visual::{map_tone_color, section_title, state_color, text_rows};

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
        if self
            .previous
            .as_ref()
            .is_some_and(|(previous, items, lines)| {
                previous == rows && items == attention && lines == climb
            })
        {
            return false;
        }
        self.previous = Some((rows.to_vec(), attention.to_vec(), climb.to_vec()));
        true
    }

    /// The elements in order as `(id, row)` (`side-attention`, `side-attention-0`, …,
    /// `side-climb-<i>`, `side-section-<section>`, `side-<section>-<i>`, …) at `width` cells; ids
    /// follow dialogs-visual §5.9.
    pub fn items(&self, width: u16) -> Vec<(String, Line<'static>)> {
        let Some((rows, attention, climb)) = &self.previous else {
            return Vec::new();
        };
        let mut items = Items {
            width,
            items: Vec::new(),
        };
        let muted = Style::new().fg(THEME.muted);
        if !attention.is_empty() {
            items.heading(
                "side-attention",
                "Needs you",
                Style::new().fg(THEME.warning),
            );
            for (i, item) in attention.iter().take(6).enumerate() {
                items.text(
                    &format!("side-attention-{i}"),
                    &format!("! {}", item.title),
                    Style::new().fg(THEME.warning),
                );
                items.text(
                    &format!("side-attention-hint-{i}"),
                    &format!("  {}", item.hint),
                    muted,
                );
            }
        }
        for (i, line) in climb.iter().enumerate() {
            let tone = Style::new().fg(map_tone_color(line.tone));
            let mut spans = vec![Span::styled(line.prefix.clone(), muted)];
            spans.push(Span::styled(
                if line.glyph.is_empty() {
                    String::new()
                } else {
                    format!("{} ", line.glyph)
                },
                tone,
            ));
            if !line.label.is_empty() {
                let label = Style::new().fg(THEME.accent).add_modifier(Modifier::BOLD);
                spans.push(Span::styled(format!("{} ", line.label), label));
            }
            let heading = line.heading == Some(true);
            let title = if heading {
                tone
            } else if line.glyph.is_empty() {
                muted
            } else {
                Style::new().fg(THEME.text)
            };
            spans.push(Span::styled(line.title.clone(), title));
            spans.push(Span::styled(line.state.clone(), tone));
            if heading {
                items.blank();
            }
            items
                .items
                .push((format!("side-climb-{i}"), Line::from(spans)));
        }
        let mut sections: Vec<&str> = Vec::new();
        for row in rows {
            if !sections.contains(&row.section.as_str()) {
                sections.push(&row.section);
            }
        }
        for section in sections {
            let members: Vec<&ViewRow> = rows.iter().filter(|row| row.section == section).collect();
            let running = if section == "jobs" {
                members
                    .iter()
                    .filter(|row| {
                        let state = js_lowercase(row.state.as_deref().unwrap_or(""));
                        state.contains("running") || state.contains("active")
                    })
                    .count()
            } else {
                0
            };
            let running = if running > 0 {
                format!(" · {running} running")
            } else {
                String::new()
            };
            let title = format!("{}  {}{running}", section_title(section), members.len());
            items.heading(
                &format!("side-section-{section}"),
                &title,
                Style::new().fg(THEME.text),
            );
            for (i, row) in members.iter().take(8).enumerate() {
                items.line(
                    &format!("side-{section}-{i}"),
                    &row.title,
                    row.state.as_deref().unwrap_or(""),
                );
            }
            if members.len() > 8 {
                items.text(
                    &format!("side-{section}-more"),
                    &format!("  +{} more", members.len() - 8),
                    muted,
                );
            }
        }
        if rows.is_empty() && attention.is_empty() {
            let pairs = [
                ("Missions & directions", "None yet · /status"),
                ("Jobs", "None yet · /jobs"),
                ("Approvals", "None · /approvals"),
            ];
            for (i, (title, hint)) in pairs.into_iter().enumerate() {
                items.heading(
                    &format!("side-empty-{i}"),
                    title,
                    Style::new().fg(THEME.text),
                );
                items.text(&format!("side-hint-{i}"), hint, muted);
            }
        }
        items.items
    }
}

/// Builds the item list.
struct Items {
    width: u16,
    items: Vec<(String, Line<'static>)>,
}

impl Items {
    fn blank(&mut self) {
        self.items.push((String::new(), Line::default()));
    }

    fn text(&mut self, id: &str, content: &str, style: Style) {
        for row in text_rows(content, self.width, style) {
            self.items.push((id.to_owned(), row));
        }
    }

    /// `heading(id, content, color)`: bold, one blank row above.
    fn heading(&mut self, id: &str, content: &str, style: Style) {
        self.blank();
        self.text(id, content, style.add_modifier(Modifier::BOLD));
    }

    /// `line(id, title, state)`: `● title` in the state color (or `· title` muted), clipped, and the
    /// state right-aligned in muted.
    fn line(&mut self, id: &str, title: &str, state: &str) {
        let width = usize::from(self.width);
        let tag = safe_text_str(state);
        let marker = if state.is_empty() { '·' } else { '●' };
        let label = safe_text_str(&format!("{marker} {title}"));
        let room = if tag.is_empty() {
            width
        } else {
            width.saturating_sub(tag.width() + 1)
        };
        let mut clipped = String::new();
        for char in label.chars() {
            if (clipped.as_str().width()
                + unicode_width::UnicodeWidthChar::width(char).unwrap_or(0))
                > room
            {
                break;
            }
            clipped.push(char);
        }
        let color = if state.is_empty() {
            THEME.muted
        } else {
            state_color(state)
        };
        let mut spans = vec![Span::styled(clipped.clone(), Style::new().fg(color))];
        if !tag.is_empty() {
            let gap = width
                .saturating_sub(clipped.as_str().width() + tag.width())
                .max(1);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(tag, Style::new().fg(THEME.muted)));
        }
        self.items.push((id.to_owned(), Line::from(spans)));
    }
}
