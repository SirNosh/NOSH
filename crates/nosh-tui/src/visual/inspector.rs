//! The inspector cards of non-chat views: `createInspector` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.4-§5.7 (section headings, `commandId`, `MAX_CARD_FIELDS`,
//! card layout, `+N more fields`, the empty fallback) and §10.3 (rebuilt only when `(rows, fallback)`
//! changes).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::js::{length, number_string, slice_to};
use crate::theme::{SPLIT_BAR, THEME};
use crate::view_model::{ViewField, ViewRow, safe_text_str};
use crate::visual::{section_title, state_color, text_rows};

/// `MAX_CARD_FIELDS`.
pub const MAX_CARD_FIELDS: usize = 8;

/// The retained inspector model.
#[derive(Debug, Default)]
pub struct Inspector {
    previous: Option<(Vec<ViewRow>, String)>,
    cache: Option<(u16, Vec<Line<'static>>)>,
}

/// `commandId`: the row's entity id, or `""` for the `<section>-<index>` placeholders.
fn command_id(row: &ViewRow) -> &str {
    let numbered = row
        .id
        .strip_prefix(&format!("{}-", row.section))
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()));
    let placeholder = row.section == "action" || row.id == row.section || numbered;
    if row.id != row.title && !placeholder {
        &row.id
    } else {
        ""
    }
}

impl Inspector {
    /// An empty inspector.
    pub fn new() -> Self {
        Self::default()
    }

    /// `update(rows, fallback)`: returns false when the signature is unchanged (dialogs-visual §5.7
    /// step 1).
    pub fn update(&mut self, rows: &[ViewRow], fallback: &str) -> bool {
        if self
            .previous
            .as_ref()
            .is_some_and(|(previous, text)| previous == rows && text == fallback)
        {
            return false;
        }
        self.previous = Some((rows.to_vec(), fallback.to_owned()));
        self.cache = None;
        true
    }

    /// The rendered rows at `width` cells.
    pub fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        if self
            .cache
            .as_ref()
            .is_none_or(|(cached, _)| *cached != width)
        {
            let (rows, fallback) = self.previous.clone().unwrap_or_default();
            self.cache = Some((width, cards(&rows, &fallback, width)));
        }
        self.cache.as_ref().expect("filled above").1.clone()
    }
}

fn cards(rows: &[ViewRow], fallback: &str, width: u16) -> Vec<Line<'static>> {
    let muted = Style::new().fg(THEME.muted);
    let text = Style::new().fg(THEME.text);
    if rows.is_empty() {
        return text_rows(fallback, width, muted);
    }
    let mut lines = Vec::new();
    let mut section = "";
    // A card: `┃` in column 0, padding 1 around content from column 2.
    let inner = width.saturating_sub(3);
    for (index, row) in rows.iter().take(120).enumerate() {
        if row.section != section {
            let count = rows
                .iter()
                .filter(|other| other.section == row.section)
                .count();
            let counted = if count > 1 {
                format!("  ({count})")
            } else {
                String::new()
            };
            if index > 0 {
                lines.push(Line::default());
            }
            let heading = Style::new().fg(THEME.accent).add_modifier(Modifier::BOLD);
            lines.extend(text_rows(
                &format!("{}{counted}", section_title(&row.section)),
                width,
                heading,
            ));
            lines.push(Line::default());
            section = &row.section;
        }
        let output = row.section == "output";
        let state = row.state.as_deref().unwrap_or("");
        let color = state_color(state);
        let mut content: Vec<Line<'static>> = Vec::new();
        if !output {
            content.extend(text_rows(
                &row.title,
                inner,
                text.add_modifier(Modifier::BOLD),
            ));
        }
        let version = row
            .version
            .filter(|version| *version != 0.0 && !version.is_nan());
        if !state.is_empty() || version.is_some() {
            let parts: Vec<String> = [
                state.to_owned(),
                version.map_or(String::new(), |v| format!("v{}", number_string(v))),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
            content.extend(text_rows(&parts.join(" · "), inner, Style::new().fg(color)));
        }
        let id = command_id(row);
        if !id.is_empty() {
            content.extend(text_rows(&format!("id {id}"), inner, muted));
        }
        let subtitle = row.subtitle.as_deref().unwrap_or("");
        if !subtitle.is_empty() {
            content.extend(text_rows(subtitle, inner, muted));
        }
        let fields: Vec<&ViewField> = row
            .fields
            .iter()
            .filter(|field| {
                output || (field.value != row.title && field.value != subtitle && field.value != id)
            })
            .collect();
        let shown = &fields[..fields.len().min(if output { 1 } else { MAX_CARD_FIELDS })];
        let label_width = shown
            .iter()
            .map(|field| length(&field.label) + 1)
            .max()
            .unwrap_or(0)
            .clamp(10, 26);
        for field in shown {
            if output {
                content.extend(text_rows(&field.value, inner, text));
                continue;
            }
            let value_width = inner.saturating_sub(label_width as u16 + 2);
            let label = slice_to(&safe_text_str(&field.label), label_width);
            let label = format!(
                "{label}{}",
                " ".repeat(label_width.saturating_sub(length(&label)) + 2)
            );
            for (n, value) in text_rows(&field.value, value_width, text)
                .into_iter()
                .enumerate()
            {
                let lead = if n == 0 {
                    label.clone()
                } else {
                    " ".repeat(label_width + 2)
                };
                let mut spans = vec![Span::styled(lead, muted)];
                spans.extend(value.spans);
                content.push(Line::from(spans));
            }
        }
        if fields.len() > shown.len() {
            let job = if row.section == "jobs" && !id.is_empty() {
                format!(" · /job {id}")
            } else {
                String::new()
            };
            content.extend(text_rows(
                &format!("+{} more fields{job}", fields.len() - shown.len()),
                inner,
                muted,
            ));
        }
        let card = Style::new().bg(THEME.panel);
        for line in std::iter::once(Line::default())
            .chain(content)
            .chain(std::iter::once(Line::default()))
        {
            let mut spans = vec![
                Span::styled(SPLIT_BAR.to_string(), Style::new().fg(color)),
                Span::raw(" "),
            ];
            spans.extend(line.spans);
            lines.push(Line::from(spans).style(card));
        }
        lines.push(Line::default());
    }
    lines
}
