//! The conversation transcript: `createTranscript` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.3 (reconciliation of at most 100 blocks, tints, icons,
//! titles, accent blocks, move loop), §6 (assistant bodies through `crate::markdown`), §8.1 tests
//! 1-4 (the serial/`UpdateStats` harness) and §10.3 (blocks rebuilt only when their entry or the
//! width changed; a `Vec<String>` order plus a `HashMap`, no `indexmap`, DV-2).

use std::collections::{HashMap, HashSet};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::markdown::render_markdown;
use crate::theme::{SPLIT_BAR, THEME};
use crate::view_model::{EntryKind, TranscriptEntry, safe_text_str};
use crate::visual::text_rows;

/// What one [`Transcript::update`] did; TS `add` calls = `created + moved`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateStats {
    pub created: usize,
    pub moved: usize,
    pub removed: usize,
}

/// One message block; its kind, tint and body type are fixed when it is created.
#[derive(Debug)]
struct Block {
    serial: u64,
    body_serial: u64,
    kind: EntryKind,
    tint: Color,
    entry: TranscriptEntry,
    title: String,
    body: String,
    streaming: bool,
    cache: Option<(u16, Vec<Line<'static>>)>,
}

/// The retained transcript model. Every block and body gets a serial from one monotonic counter
/// (the analogue of object identity in the TS tests).
#[derive(Debug, Default)]
pub struct Transcript {
    next_serial: u64,
    blocks: HashMap<String, Block>,
    order: Vec<String>,
    live: HashSet<u64>,
}

fn tint(kind: EntryKind) -> Color {
    match kind {
        EntryKind::User => THEME.accent,
        EntryKind::Failure => THEME.error,
        EntryKind::Approval => THEME.warning,
        EntryKind::Tool => THEME.cyan,
        EntryKind::Receipt => THEME.success,
        EntryKind::Assistant => THEME.secondary,
    }
}

fn icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::User => "▪",
        EntryKind::Assistant => "◆",
        EntryKind::Tool => "↳",
        EntryKind::Failure => "!",
        EntryKind::Receipt => "✓",
        EntryKind::Approval => "?",
    }
}

impl Transcript {
    /// An empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    fn serial(&mut self) -> u64 {
        self.next_serial += 1;
        self.live.insert(self.next_serial);
        self.next_serial
    }

    /// `update(entries)`: reconciles the last 100 entries (dialogs-visual §5.3 steps 1-4).
    pub fn update(&mut self, entries: &[TranscriptEntry]) -> UpdateStats {
        let mut stats = UpdateStats::default();
        let shown = &entries[entries.len().saturating_sub(100)..];
        let ids: HashSet<&str> = shown.iter().map(|entry| entry.id.as_str()).collect();
        let gone: Vec<String> = self
            .order
            .iter()
            .filter(|id| !ids.contains(id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            if let Some(block) = self.blocks.remove(&id) {
                self.live.remove(&block.serial);
                self.live.remove(&block.body_serial);
            }
            self.order.retain(|other| *other != id);
            stats.removed += 1;
        }
        for entry in shown {
            if self
                .blocks
                .get(&entry.id)
                .is_some_and(|block| block.entry == *entry)
            {
                continue;
            }
            if !self.blocks.contains_key(&entry.id) {
                let (serial, body_serial) = (self.serial(), self.serial());
                self.blocks.insert(
                    entry.id.clone(),
                    Block {
                        serial,
                        body_serial,
                        kind: entry.kind,
                        tint: tint(entry.kind),
                        entry: entry.clone(),
                        title: String::new(),
                        body: String::new(),
                        streaming: false,
                        cache: None,
                    },
                );
                self.order.push(entry.id.clone());
                stats.created += 1;
            }
            let streaming = entry.status.as_deref() == Some("streaming");
            let meta: Vec<&str> = [
                entry.model.as_deref(),
                entry.thinking_level.as_deref(),
                streaming.then_some("writing…"),
            ]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect();
            let meta = if meta.is_empty() {
                String::new()
            } else {
                format!("  ·  {}", meta.join(" · "))
            };
            let block = self.blocks.get_mut(&entry.id).expect("created above");
            block.title = safe_text_str(&format!("{} {}{meta}", icon(entry.kind), entry.title));
            block.body = safe_text_str(&entry.text);
            block.streaming = streaming;
            block.entry = entry.clone();
            block.cache = None;
        }
        // `add(box, index)` moves a misplaced box from a later position; earlier ones are settled.
        for (index, entry) in shown.iter().enumerate() {
            if self.order[index] != entry.id {
                let from = self
                    .order
                    .iter()
                    .position(|id| *id == entry.id)
                    .expect("a block per entry");
                let id = self.order.remove(from);
                self.order.insert(index, id);
                stats.moved += 1;
            }
        }
        stats
    }

    /// The block ids in display order: `message-<id>`.
    pub fn ids(&self) -> Vec<String> {
        self.order
            .iter()
            .map(|id| format!("message-{id}"))
            .collect()
    }

    /// The serial of the block for entry `id`.
    pub fn block_serial(&self, id: &str) -> Option<u64> {
        self.blocks.get(id).map(|block| block.serial)
    }

    /// The serial of the body widget of entry `id`.
    pub fn body_serial(&self, id: &str) -> Option<u64> {
        self.blocks.get(id).map(|block| block.body_serial)
    }

    /// The body text of entry `id` (`safeText(entry.text)`).
    pub fn body_content(&self, id: &str) -> Option<String> {
        self.blocks.get(id).map(|block| block.body.clone())
    }

    /// The Markdown body's `streaming` flag of entry `id`.
    pub fn body_streaming(&self, id: &str) -> Option<bool> {
        self.blocks.get(id).map(|block| block.streaming)
    }

    /// False once the block or body with this serial was released.
    pub fn is_live(&self, serial: u64) -> bool {
        self.live.contains(&serial)
    }

    /// The rendered rows at `width` cells (accent bar, padding, backgrounds, blank row after each
    /// block), from per-block caches.
    pub fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for id in &self.order {
            let block = self.blocks.get_mut(id).expect("ordered blocks exist");
            if block
                .cache
                .as_ref()
                .is_none_or(|(cached, _)| *cached != width)
            {
                block.cache = Some((width, block_lines(block, width)));
            }
            lines.extend(
                block
                    .cache
                    .as_ref()
                    .expect("filled above")
                    .1
                    .iter()
                    .cloned(),
            );
        }
        lines
    }
}

/// One block's rows: `paddingX 2`, the user block's `paddingY 1`, a left `┃` and panel fill for
/// accent kinds, and the `marginBottom 1` row.
fn block_lines(block: &Block, width: u16) -> Vec<Line<'static>> {
    let accent = matches!(
        block.kind,
        EntryKind::User | EntryKind::Failure | EntryKind::Approval
    );
    let content = width.saturating_sub(if accent { 5 } else { 4 });
    let mut rows = text_rows(
        &block.title,
        content,
        Style::new().fg(block.tint).add_modifier(Modifier::BOLD),
    );
    let body_color = match block.kind {
        EntryKind::Tool | EntryKind::Receipt => THEME.muted,
        _ => THEME.text,
    };
    if block.kind == EntryKind::Assistant {
        rows.extend(render_markdown(&block.body, content));
    } else {
        rows.extend(text_rows(&block.body, content, Style::new().fg(body_color)));
    }
    let background = if accent {
        THEME.panel
    } else {
        THEME.background
    };
    let pad = usize::from(block.kind == EntryKind::User);
    let blank = || Line::default();
    let mut lines = Vec::new();
    for row in std::iter::repeat_n(blank(), pad)
        .chain(rows)
        .chain(std::iter::repeat_n(blank(), pad))
    {
        let mut spans = Vec::with_capacity(row.spans.len() + 1);
        if accent {
            spans.push(Span::styled(
                SPLIT_BAR.to_string(),
                Style::new().fg(block.tint),
            ));
        }
        spans.push(Span::raw("  "));
        spans.extend(row.spans);
        lines.push(Line::from(spans).style(Style::new().bg(background)));
    }
    lines.push(Line::default().style(Style::new().bg(THEME.background)));
    lines
}
