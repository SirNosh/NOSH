//! The research map view: `createResearchMap` of `apps/tui/src/visual.ts`.
//!
//! Specified by tui-dialogs-visual.md §5.10-§5.13 (`MAP_TONES`, `EMPTY_MAP`, `clipped`, `ZOOMS`,
//! `update` steps 1-14, `select`, `zoomTo`, mouse zoom/drag/click, the returned API) on top of
//! `crate::map_graph`; tui-workspace.md §9.1 steps 14-17 and `checkMapMouse` (smoke).
//!
//! Layout rows are indexed by character: the graph pictures are grids of one-cell characters.

use std::rc::Rc;
use std::sync::LazyLock;

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;

use crate::js::{JsValue, length};
use crate::map_graph::{
    GraphBox, GraphDetail, GraphDirection, GraphLayout, box_at, layout_graph, move_selection,
    shrink_graph, suggested_command,
};
use crate::theme::THEME;
use crate::view_model::{MapTone, TEXT_LIMIT, map_root, record, text};
use crate::visual::{clipped, map_tone_color, text_rows};

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

/// `/(\d\.\d{5})\d+/g` of the facts line.
static DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([0-9]\.[0-9]{5})[0-9]+").expect("valid digits pattern"));

/// A mouse drag in progress, in canvas cells, with the view origin it started from.
#[derive(Clone, Copy, Debug)]
struct Drag {
    x: i64,
    y: i64,
    pan_x: i64,
    pan_y: i64,
    moved: bool,
}

/// What the rows were last built from (the TS `JSON.stringify` signature).
#[derive(Debug, PartialEq)]
struct Signature {
    lines: Vec<String>,
    tones: Vec<MapTone>,
    selected: usize,
    x0: i64,
    y0: i64,
    zoom: usize,
    width: u16,
    height: u16,
    root_index: i64,
    opened: Vec<String>,
    roots: usize,
    workers: Vec<String>,
}

/// The retained map view state (dialogs-visual §5.13 private state).
#[derive(Debug)]
pub struct ResearchMap {
    root_index: i64,
    selected_id: String,
    zoom: usize,
    follow: bool,
    pan_x: i64,
    pan_y: i64,
    origin_x: i64,
    origin_y: i64,
    fits: bool,
    graph: Option<JsValue>,
    drag: Option<Drag>,
    /// Autoresearch boxes opened from a Mission or Direction graph, innermost last.
    opened: Vec<String>,
    boxes: Vec<GraphBox>,
    selected: usize,
    previous: Option<Signature>,
    header: Vec<Line<'static>>,
    canvas: Vec<Line<'static>>,
    panel: Vec<Line<'static>>,
}

impl Default for ResearchMap {
    fn default() -> Self {
        Self::new()
    }
}

/// `plain(value, 80)` of a node's id.
fn id_of(node: &JsValue) -> String {
    text(node.get("id"), 80)
}

/// `find(node, id)`: depth-first through `children` for `node.id === id`.
fn find<'a>(node: &'a JsValue, id: &str) -> Option<&'a JsValue> {
    if node.get("id").and_then(JsValue::as_str) == Some(id) {
        return Some(node);
    }
    node.get("children")
        .and_then(JsValue::as_array)?
        .iter()
        .filter(|child| child.is_object())
        .find_map(|child| find(child, id))
}

/// `line(id, styledText)`: one row of chunks.
fn row(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(spans)
}

impl ResearchMap {
    /// The initial state: first root, 100%, following the selection.
    pub fn new() -> Self {
        ResearchMap {
            root_index: 0,
            selected_id: String::new(),
            zoom: 1,
            follow: true,
            pan_x: 0,
            pan_y: 0,
            origin_x: 0,
            origin_y: 0,
            fits: true,
            graph: None,
            drag: None,
            opened: Vec::new(),
            boxes: Vec::new(),
            selected: 0,
            previous: None,
            header: Vec::new(),
            canvas: Vec::new(),
            panel: Vec::new(),
        }
    }

    /// `update(map, width, height)`: lays out the current root and rebuilds the rows when the
    /// signature changed (steps 1-14).
    pub fn update(&mut self, map: Option<&JsValue>, width: u16, height: u16) {
        let map_record = record(map);
        let roots: Vec<&JsValue> = match map_record.get("roots") {
            Some(JsValue::Array(roots)) => roots.iter().collect(),
            _ => Vec::new(),
        };
        let count = roots.len() as i64;
        self.root_index = if count > 0 {
            (self.root_index % count + count) % count
        } else {
            0
        };
        let empty = JsValue::Object(Default::default());
        let top = roots
            .get(self.root_index as usize)
            .map(|root| if root.is_object() { *root } else { &empty });
        while let (Some(top), Some(last)) = (top, self.opened.last()) {
            if find(top, last).is_some() {
                break;
            }
            self.opened.pop();
        }
        let drilled = top
            .and(self.opened.last())
            .and_then(|last| find(top?, last));
        self.graph = drilled.or(top).cloned();
        let (label, detail, factor) = ZOOMS[self.zoom];
        let factor = factor as i64;
        let layout = layout_graph(self.graph.as_ref(), detail);
        let boxes = layout.boxes.clone();
        let found = (!self.selected_id.is_empty())
            .then(|| {
                boxes
                    .iter()
                    .position(|entry| id_of(&entry.node) == self.selected_id)
            })
            .flatten();
        let selected = found.unwrap_or(0);
        self.selected_id = boxes
            .get(selected)
            .map_or(String::new(), |entry| id_of(&entry.node));
        self.boxes = boxes;
        self.selected = selected;
        let shrunk = (factor > 1).then(|| shrink_graph(&layout, factor as usize));
        let view_lines: &[String] = shrunk
            .as_ref()
            .map_or(&layout.lines, |shrunk| &shrunk.lines);
        let workers: Vec<String> = match map_record.get("workers") {
            Some(JsValue::Array(workers)) => workers
                .iter()
                .map(|worker| {
                    let worker = record(Some(worker));
                    let line = format!(
                        "⚙ {} {}",
                        text(worker.get("title"), 40),
                        text(worker.get("state"), 40)
                    );
                    crate::js::js_trim(&line).to_owned()
                })
                .collect(),
            _ => Vec::new(),
        };
        let rows = (i64::from(height) - 5 - i64::from(!workers.is_empty())).max(4);
        let cols = i64::from(width).max(12);
        let (span_x, span_y) = (cols * factor, rows * factor);
        let canvas_width = layout
            .lines
            .first()
            .map_or(0, |line| line.chars().count() as i64);
        let canvas_height = layout.lines.len() as i64;
        if let Some(entry) = self.boxes.get(selected).filter(|_| self.follow) {
            let (left, right, top_row, bottom) = (
                entry.left as i64,
                entry.right as i64,
                entry.top as i64,
                entry.bottom as i64,
            );
            if left < self.pan_x {
                self.pan_x = (left - 2).max(0);
            } else if right >= self.pan_x + span_x {
                self.pan_x = (left - 2).min(right - span_x + 3).max(0);
            }
            if top_row < self.pan_y {
                self.pan_y = (top_row - 1).max(0);
            } else if bottom >= self.pan_y + span_y {
                self.pan_y = (top_row - 1).min(bottom - span_y + 2).max(0);
            }
        }
        self.pan_x = self.pan_x.min(canvas_width - span_x).max(0);
        self.pan_y = self.pan_y.min(canvas_height - span_y).max(0);
        self.fits = canvas_width <= span_x && canvas_height <= span_y;
        let (x0, y0) = (self.pan_x / factor, self.pan_y / factor);
        (self.origin_x, self.origin_y) = (x0 * factor, y0 * factor);
        let signature = Signature {
            lines: view_lines.to_vec(),
            tones: self.boxes.iter().map(|entry| entry.tone).collect(),
            selected,
            x0,
            y0,
            zoom: self.zoom,
            width,
            height,
            root_index: self.root_index,
            opened: self.opened.clone(),
            roots: roots.len(),
            workers: workers.clone(),
        };
        if self.previous.as_ref() == Some(&signature) {
            return;
        }
        self.previous = Some(signature);
        let Some(top) = top else {
            self.header = text_rows(EMPTY_MAP, width, Style::new().fg(THEME.muted));
            self.header.push(Line::default());
            self.canvas.clear();
            self.panel.clear();
            return;
        };
        let kind = text(top.get("kind"), TEXT_LIMIT);
        let mut path = vec![
            map_root(&kind).map_or(kind.to_uppercase(), |(_, label)| label.to_owned()),
            format!("{}/{}", self.root_index + 1, roots.len()),
            text(top.get("state"), TEXT_LIMIT),
            text(top.get("title"), 200),
        ];
        if let Some(drilled) = drilled {
            path.push(format!("› {}", text(drilled.get("title"), 80)));
        }
        let path = path.join("  ");
        let hint = format!(
            "{label}{}{}",
            if roots.len() > 1 { " · tab next" } else { "" },
            if self.opened.is_empty() {
                ""
            } else {
                " · backspace back"
            }
        );
        let budget = (cols - length(&hint) as i64 - 2).max(8) as usize;
        let accent = Style::new().fg(THEME.accent).add_modifier(Modifier::BOLD);
        self.header = vec![row(vec![
            Span::styled(clipped(&path, budget), accent),
            Span::styled(format!("  {hint}"), Style::new().fg(THEME.muted)),
        ])];
        if !workers.is_empty() {
            let line = clipped(
                &format!("workers  {}", workers.join("  ·  ")),
                cols as usize,
            );
            self.header.push(row(vec![Span::styled(
                line,
                Style::new().fg(THEME.secondary),
            )]));
        }
        let visible = rows.min(view_lines.len() as i64 - y0).max(0);
        self.canvas = (0..visible)
            .map(|index| {
                self.canvas_line(
                    view_lines,
                    shrunk.as_ref().map(|shrunk| &shrunk.owners),
                    y0 + index,
                    x0,
                    cols,
                )
            })
            .collect();
        let Some(entry) = self.boxes.get(selected) else {
            self.panel.clear();
            return;
        };
        let command = suggested_command(entry);
        let parent = self
            .boxes
            .iter()
            .find(|other| Some(other.index) == entry.parent);
        let node = &entry.node;
        let kind = text(node.get("kind"), TEXT_LIMIT);
        let under = parent.map_or(String::new(), |parent| {
            format!(
                "under #{} {}",
                parent.index,
                clipped(&text(parent.node.get("title"), 200), 40)
            )
        });
        let facts: Vec<String> = [
            if kind == "node" {
                "graph node".to_owned()
            } else {
                kind
            },
            text(node.get("state"), 40),
            DIGITS
                .replace_all(&text(node.get("detail"), 200), "$1")
                .into_owned(),
            text(node.get("id"), 80),
            under,
        ]
        .into_iter()
        .filter(|fact| !fact.is_empty())
        .collect();
        let action = if entry.opens {
            "enter  open its experiments".to_owned()
        } else if !command.is_empty() {
            format!("enter  {command}   (fills the composer; nothing changes until you confirm)")
        } else {
            String::new()
        };
        let title = text(node.get("title"), 400);
        let name = if title.is_empty() {
            text(node.get("id"), 80)
        } else {
            title
        };
        let bold = Style::new().fg(THEME.text).add_modifier(Modifier::BOLD);
        self.panel = vec![
            Line::default(),
            row(vec![Span::styled(clipped(&name, cols as usize), bold)]),
            row(vec![Span::styled(
                clipped(&facts.join(" · "), cols as usize),
                Style::new().fg(map_tone_color(entry.tone)),
            )]),
        ];
        if !action.is_empty() {
            self.panel.push(row(vec![Span::styled(
                clipped(&action, cols as usize),
                Style::new().fg(THEME.accent),
            )]));
        }
    }

    /// One canvas row: each run of cells in its box's color (the selected box accent and bold),
    /// edges muted.
    fn canvas_line(
        &self,
        lines: &[String],
        owners: Option<&Vec<Vec<i32>>>,
        row_index: i64,
        x0: i64,
        cols: i64,
    ) -> Line<'static> {
        let cells: Vec<char> = lines[row_index as usize]
            .chars()
            .skip(x0 as usize)
            .take(cols as usize)
            .collect();
        let owner_at = |column: usize| -> i32 {
            match owners {
                Some(owners) => owners[row_index as usize]
                    .get(column + x0 as usize)
                    .copied()
                    .unwrap_or(-1),
                None => box_at(&self.boxes, column as i64 + x0, row_index),
            }
        };
        let mut spans = Vec::new();
        let (mut start, mut owner) = (0, owner_at(0));
        for column in 1..=cells.len() {
            let next = if column < cells.len() {
                owner_at(column)
            } else {
                -2
            };
            if next == owner {
                continue;
            }
            let piece: String = cells[start..column].iter().collect();
            let style = if owner == -1 {
                Style::new().fg(THEME.muted)
            } else if owner as usize == self.selected {
                Style::new().fg(THEME.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(map_tone_color(self.boxes[owner as usize].tone))
            };
            spans.push(Span::styled(piece, style));
            (start, owner) = (column, next);
        }
        Line::from(spans)
    }

    /// `select(index)`.
    fn select(&mut self, index: usize) {
        self.selected_id = self
            .boxes
            .get(index)
            .map_or(String::new(), |entry| id_of(&entry.node));
    }

    /// `move(direction)`: selects the nearest box that way and follows it.
    pub fn move_selection(&mut self, direction: GraphDirection) {
        self.select(move_selection(&self.boxes, self.selected, direction));
        self.follow = true;
    }

    /// `cycle(step)`: the next or previous root, reset to the top.
    pub fn cycle(&mut self, step: i64) {
        self.root_index += step;
        self.opened.clear();
        self.selected_id.clear();
        (self.pan_x, self.pan_y) = (0, 0);
        self.follow = true;
    }

    /// `pan(rows)`: scrolls by `rows × factor` layout rows and stops following.
    pub fn pan(&mut self, rows: i64) {
        self.pan_y = (self.pan_y + rows * ZOOMS[self.zoom].2 as i64).max(0);
        self.follow = false;
    }

    /// `open()`: drills into an opening box (returns `""`), else returns the selected box's
    /// suggested command (`""` without a selection).
    pub fn open(&mut self) -> String {
        let Some(entry) = self.boxes.get(self.selected) else {
            return String::new();
        };
        if entry.opens {
            self.opened.push(id_of(&entry.node));
            self.selected_id.clear();
            (self.pan_x, self.pan_y) = (0, 0);
            self.follow = true;
            return String::new();
        }
        suggested_command(entry)
    }

    /// `back()`: leaves the innermost opened Autoresearch; false when none is open.
    pub fn back(&mut self) -> bool {
        let Some(last) = self.opened.pop() else {
            return false;
        };
        self.selected_id = last;
        self.follow = true;
        true
    }

    /// `zoomTo(next, cell)`: keeps the point under the pointer in place; across layouts, the box
    /// under it.
    fn zoom_to(&mut self, next: usize, (x, y): (i64, i64)) {
        let (_, from_detail, from) = ZOOMS[self.zoom];
        let (_, to_detail, to) = ZOOMS[next];
        let point = (
            self.origin_x + x * from as i64,
            self.origin_y + y * from as i64,
        );
        let (mut pan_x, mut pan_y) = (self.pan_x as f64, self.pan_y as f64);
        if from_detail == to_detail {
            pan_x = (point.0 - x * to as i64) as f64;
            pan_y = (point.1 - y * to as i64) as f64;
        } else {
            let before: Rc<GraphLayout> = layout_graph(self.graph.as_ref(), from_detail);
            let after = layout_graph(self.graph.as_ref(), to_detail);
            let centre = |entry: &GraphBox| {
                ((entry.left + entry.right) as f64 / 2.0 - point.0 as f64).abs()
                    + 2.0 * ((entry.top + entry.bottom) as f64 / 2.0 - point.1 as f64).abs()
            };
            let hit = box_at(&before.boxes, point.0, point.1);
            let anchor = if hit >= 0 {
                before.boxes.get(hit as usize)
            } else {
                // A stable sort keeps the first of equally near boxes, as `Array.sort` does.
                let mut sorted: Vec<&GraphBox> = before.boxes.iter().collect();
                sorted.sort_by(|a, b| centre(a).total_cmp(&centre(b)));
                sorted.first().copied()
            };
            let target = anchor
                .and_then(|anchor| after.boxes.iter().find(|entry| entry.index == anchor.index));
            if let (Some(anchor), Some(target)) = (anchor, target) {
                let span = |a: usize, b: usize| (b as f64 - a as f64).max(1.0);
                pan_x = target.left as f64
                    + (point.0 as f64 - anchor.left as f64)
                        * (target.right as f64 - target.left as f64)
                        / span(anchor.left, anchor.right)
                    - x as f64;
                pan_y = target.top as f64
                    + (point.1 as f64 - anchor.top as f64)
                        * (target.bottom as f64 - target.top as f64)
                        / span(anchor.top, anchor.bottom)
                    - y as f64;
            }
        }
        // `Math.round` rounds halves up.
        self.pan_x = ((pan_x + 0.5).floor() as i64).max(0);
        self.pan_y = ((pan_y + 0.5).floor() as i64).max(0);
        self.zoom = next;
        self.follow = false;
    }

    /// A mouse event at canvas cell `(x, y)` (relative to the canvas origin; may be outside it
    /// during a drag): wheel zooms, left drag pans, a click selects. Returns whether the view changed
    /// and must be redrawn. The workspace never lets these events reach the conversation.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, x: i64, y: i64) -> bool {
        let factor = ZOOMS[self.zoom].2 as i64;
        match kind {
            MouseEventKind::ScrollUp if self.zoom > 0 => {
                self.zoom_to(self.zoom - 1, (x, y));
                true
            }
            MouseEventKind::ScrollDown if self.zoom < ZOOMS.len() - 1 && !self.fits => {
                self.zoom_to(self.zoom + 1, (x, y));
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = Some(Drag {
                    x,
                    y,
                    pan_x: self.origin_x,
                    pan_y: self.origin_y,
                    moved: false,
                });
                false
            }
            MouseEventKind::Drag(_) => {
                let Some(drag) = self.drag.as_mut() else {
                    return false;
                };
                drag.moved |= x != drag.x || y != drag.y;
                self.pan_x = (drag.pan_x - (x - drag.x) * factor).max(0);
                self.pan_y = (drag.pan_y - (y - drag.y) * factor).max(0);
                self.follow = false;
                true
            }
            MouseEventKind::Up(_) => {
                let Some(drag) = self.drag.take() else {
                    return false;
                };
                if drag.moved {
                    return false;
                }
                let index = box_at(
                    &self.boxes,
                    self.origin_x + x * factor,
                    self.origin_y + y * factor,
                );
                if index < 0 {
                    return false;
                }
                self.select(index as usize);
                self.follow = false;
                true
            }
            _ => false,
        }
    }

    /// Whether a drag started on the canvas is in progress (it captures later mouse events).
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The rendered rows: header, workers, canvas, then the selected-box panel.
    pub fn lines(&self) -> Vec<Line<'static>> {
        self.header
            .iter()
            .chain(&self.canvas)
            .chain(&self.panel)
            .cloned()
            .collect()
    }

    /// The index in [`ResearchMap::lines`] of the first canvas row (to place the canvas origin).
    pub fn canvas_row(&self) -> usize {
        self.header.len()
    }

    /// The number of canvas rows.
    pub fn canvas_rows(&self) -> usize {
        self.canvas.len()
    }
}
