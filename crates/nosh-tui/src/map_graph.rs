//! The research map as a laid-out graph: a port of `apps/tui/src/map-graph.ts`.
//!
//! Specified by tui-viewmodel.md §3 (types, functions, goldens §3.10/§3.11, callers §3.12), §5.1
//! (the seven `map-graph.test.ts` cases), §6.1-§6.4 (byte-exact goldens) and §7.2, §7.5. One root
//! becomes a Mermaid flowchart that the private `mermaid_ascii` engine (Appendix A, Option A per
//! DECISIONS.md item 2) lays out as Unicode box drawing; each box is found again on the grid by its
//! `#k` tag.
//!
//! Deliberate fixes (DECISIONS.md item 2, logged as deviations): the level tracker grows, so graphs
//! 25 or more levels deep do not hang; a layout-cache hit refreshes `node` and `owner` from the
//! current map, so no stale `owner.version` reaches `/retry` or `/transition`. `shrink_graph` supports
//! only the even factors the map view uses (2, 4, 8). A box's `owner` is copied without its
//! `children`: only its `kind`, `id` and `version` are read (by `suggested_command`), and a whole
//! copy per box costs boxes × owner subtree on every call, cache hits included, while §3.12 needs a
//! hit to stay cheap (it runs on every repaint poll). Titles keep TS's Mermaid quirks (DECISIONS.md
//! item 7): `*`, `**` and `~~` become literal tags, a backslash-`n` breaks the line, and the first
//! `]` ends the label; text after that `]` is ignored rather than parsed as more Mermaid (see
//! `mermaid_ascii::parse`).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;

use crate::js::{
    JsObject, JsValue, js_collapse_ws, js_number, js_trim, last_index_of_char, length,
    number_string, slice_range, slice_to, to_number, truthy,
};
use crate::mermaid_ascii;
use crate::view_model::{
    MapTone, TEXT_LIMIT, map_glyph, map_root, map_tone, progress_bar, record, safe_text_str, text,
};

/// `GraphDirection`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphDirection {
    Up,
    Down,
    Left,
    Right,
}

/// `GraphDetail`: normal boxes hold a title on two lines; full boxes (zoomed in) hold whole titles
/// and the node's detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphDetail {
    Normal,
    Full,
}

/// `GraphBox`: one drawn node and its inclusive character-cell bounds.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphBox {
    /// The map node (or a synthetic baseline node), from the current map.
    pub node: JsValue,
    /// The nearest enclosing mission or direction node, without its `children`.
    pub owner: Option<JsValue>,
    /// The node number `k` of the `#k` tag, not the position in `boxes`.
    pub index: usize,
    /// The node number of the node this one hangs from.
    pub parent: Option<usize>,
    /// A nested Autoresearch summary box that opens its own graph.
    pub opens: bool,
    pub tone: MapTone,
    pub top: usize,
    pub left: usize,
    pub bottom: usize,
    pub right: usize,
}

/// `GraphLayout`: equal-length picture rows and the boxes sorted by `index`. Positions in `boxes`
/// are what [`move_selection`], [`box_at`] and [`ShrunkGraph::owners`] use.
#[derive(Debug)]
pub struct GraphLayout {
    pub lines: Vec<String>,
    pub boxes: Vec<GraphBox>,
    /// The cached render this layout was built from, which also caches its shrinks.
    geometry: Rc<Geometry>,
}

impl PartialEq for GraphLayout {
    fn eq(&self, other: &Self) -> bool {
        self.lines == other.lines && self.boxes == other.boxes
    }
}

/// `ShrunkGraph`: one Braille glyph per factor-by-factor block, and the box position (or -1) each
/// glyph belongs to.
#[derive(Debug, PartialEq)]
pub struct ShrunkGraph {
    pub lines: Vec<String>,
    pub owners: Vec<Vec<i32>>,
}

/// What a Mermaid source renders to: the rows and each found box, sorted by node number. Node data
/// is not cached, so a hit is re-attached to the current map (§7.5).
#[derive(Debug, Default)]
struct Geometry {
    lines: Vec<String>,
    rects: Vec<Rect>,
    /// `shrinkGraph`'s cache by factor: it depends only on `lines` and `rects`.
    shrunk: RefCell<HashMap<usize, Rc<ShrunkGraph>>>,
}

#[derive(Debug)]
struct Rect {
    index: usize,
    top: usize,
    left: usize,
    bottom: usize,
    right: usize,
}

/// One numbered node of the DFS (§3.5 step 2).
struct Entry<'a> {
    node: &'a JsObject,
    owner: Option<&'a JsObject>,
    parent: Option<usize>,
    opens: bool,
}

/// `LABELS.normal.width`: `cell`'s default limit.
const LABEL: usize = 26;
/// `layouts`'s size; the oldest source is evicted first and hits do not refresh it.
const CACHED_LAYOUTS: usize = 16;
/// Braille dot bits by `[row][column]` within a glyph's 2×4 dot grid.
const DOTS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

thread_local! {
    static LAYOUTS: RefCell<VecDeque<(String, Rc<Geometry>)>> = const { RefCell::new(VecDeque::new()) };
}

/// The node an Autoresearch's first experiments hang from.
static BASELINE: LazyLock<JsObject> = LazyLock::new(|| {
    [
        ("kind", "baseline"),
        ("title", "baseline"),
        ("state", ""),
        ("detail", ""),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), JsValue::from(value)))
    .collect()
});

/// Characters that are not one terminal cell (§3.2). JS `\d` is ASCII, hence `[0-9]` below.
static WIDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\x{10000}-\x{10FFFF}\x{300}-\x{36F}\x{1100}-\x{115F}\x{200B}-\x{200F}\x{2E80}-\x{A4CF}\x{AC00}-\x{D7A3}\x{F900}-\x{FAFF}\x{FE00}-\x{FE0F}\x{FE30}-\x{FE4F}\x{FF00}-\x{FF60}\x{FFE0}-\x{FFE6}]")
        .expect("valid wide pattern")
});
static SCORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:score|best) (-?[0-9.e-]+)").expect("valid score pattern"));
static ATTEMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"attempt [0-9]+/[0-9]+").expect("valid attempt pattern"));
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v[0-9]+$").expect("valid version pattern"));
static BASELINE_SCORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"baseline (-?[0-9.e-]+)").expect("valid baseline pattern"));
static TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#([0-9]+)").expect("valid tag pattern"));

/// `layoutGraph`: lays out one map root (viewmodel §3.5). A root without a truthy `kind` gives an
/// empty layout. Repeated sources share cached geometry (16 entries, FIFO) with node data refreshed.
pub fn layout_graph(root: Option<&JsValue>, detail: GraphDetail) -> Rc<GraphLayout> {
    let root = record(root);
    if !truthy(root.get("kind")) {
        return Rc::new(GraphLayout {
            lines: Vec::new(),
            boxes: Vec::new(),
            geometry: Rc::default(),
        });
    }
    let (entries, key) = number(root, detail);
    let geometry = LAYOUTS.with_borrow_mut(|layouts| {
        if let Some((_, geometry)) = layouts.iter().find(|(source, _)| *source == key) {
            return Rc::clone(geometry);
        }
        let geometry = Rc::new(render(&key, entries.len()));
        if layouts.len() >= CACHED_LAYOUTS {
            layouts.pop_front();
        }
        layouts.push_back((key, Rc::clone(&geometry)));
        geometry
    });
    let boxes = geometry
        .rects
        .iter()
        .map(|rect| {
            let entry = &entries[rect.index];
            let baseline =
                matches!(entry.node.get("kind"), Some(JsValue::String(kind)) if kind == "baseline");
            GraphBox {
                node: JsValue::Object(entry.node.clone()),
                owner: entry.owner.map(|owner| {
                    let fields = owner.iter().filter(|&(key, _)| key != "children");
                    JsValue::Object(
                        fields
                            .map(|(key, value)| (key.to_owned(), value.clone()))
                            .collect(),
                    )
                }),
                index: rect.index,
                parent: entry.parent,
                opens: entry.opens,
                tone: if baseline {
                    MapTone::Idle
                } else {
                    map_tone(&text(entry.node.get("state"), 40))
                },
                top: rect.top,
                left: rect.left,
                bottom: rect.bottom,
                right: rect.right,
            }
        })
        .collect();
    Rc::new(GraphLayout {
        lines: geometry.lines.clone(),
        boxes,
        geometry,
    })
}

/// The Mermaid flowchart [`layout_graph`] renders for a root with a `kind`, which is also its
/// cache key (viewmodel §3.5 step 2, goldens §6.1).
pub fn mermaid_source(root: Option<&JsValue>, detail: GraphDetail) -> String {
    number(record(root), detail).1
}

/// §3.5 step 2: the DFS-numbered nodes and the Mermaid source.
fn number(root: &JsObject, detail: GraphDetail) -> (Vec<Entry<'_>>, String) {
    let mut builder = Builder {
        entries: Vec::new(),
        source: vec!["flowchart TD".to_owned()],
        detail,
    };
    builder.visit(root, None, None);
    (builder.entries, builder.source.join("\n"))
}

struct Builder<'a> {
    entries: Vec<Entry<'a>>,
    source: Vec<String>,
    detail: GraphDetail,
}

impl<'a> Builder<'a> {
    fn visit(&mut self, node: &'a JsObject, parent: Option<usize>, owner: Option<&'a JsObject>) {
        let (width, lines) = match self.detail {
            GraphDetail::Normal => (LABEL, 2),
            GraphDetail::Full => (40, 3),
        };
        let index = self.entries.len();
        let kind = text(node.get("kind"), TEXT_LIMIT);
        let state = text(node.get("state"), 40);
        let all = children(node);
        let experiment = |child: &JsObject| text(child.get("kind"), TEXT_LIMIT) == "experiment";
        let experiments = all.iter().filter(|child| experiment(child)).count();
        let opens = kind == "autoresearch" && index > 0 && experiments > 0;
        self.entries.push(Entry {
            node,
            owner,
            parent,
            opens,
        });
        let name = map_root(&kind).map_or(
            match kind.as_str() {
                "job" => "JOB",
                "worker" => "WORKER",
                _ => "",
            },
            |(_, label)| label,
        );
        let tag = if name.is_empty() {
            format!("#{index}")
        } else {
            format!("#{index} {name}")
        };
        let parts = [
            format!("{} {state}", map_glyph(&kind, &state)),
            fact(node),
            if opens {
                format!("{experiments} exp ▸")
            } else {
                String::new()
            },
        ];
        let parts: Vec<String> = parts.into_iter().filter(|part| !part.is_empty()).collect();
        // Never empty: the glyph survives `cell`.
        let status = cell_str(&parts.join(" · "), if opens { width + 8 } else { width });
        let mut title = title(&cell(node.get("title"), 400), width, lines);
        if title.is_empty() {
            title = cell(node.get("id"), LABEL);
        }
        // Zoomed in, the node's whole detail line (role, attempt, score) follows its state.
        let more = match self.detail {
            GraphDetail::Full => cell(node.get("detail"), width),
            GraphDetail::Normal => String::new(),
        };
        let more = if more.is_empty() {
            more
        } else {
            format!("<br>{more}")
        };
        self.source.push(format!(
            "  n{index}[\"{tag}<br>{title}<br>{status}{more}\"]"
        ));
        if let Some(parent) = parent {
            self.source.push(format!("  n{parent} --> n{index}"));
        }
        let next_owner = if kind == "mission" || kind == "direction" {
            Some(node)
        } else {
            owner
        };
        let children = if opens { Vec::new() } else { all };
        // An Autoresearch's first experiments start from its baseline, as in the Mermaid export.
        let mut base = index;
        if kind == "autoresearch" && children.iter().any(|child| experiment(child)) {
            base = self.entries.len();
            self.entries.push(Entry {
                node: &BASELINE,
                owner: next_owner,
                parent: Some(index),
                opens: false,
            });
            let score = BASELINE_SCORE
                .captures(&text(node.get("detail"), 200))
                .map_or(String::new(), |captures| {
                    let score = nosh_wire::js::to_precision_number(js_number(&captures[1]), 4);
                    format!("<br>{}", cell_str(&number_string(score), LABEL))
                });
            self.source
                .push(format!("  n{base}[\"#{base}<br>baseline{score}\"]"));
            self.source.push(format!("  n{index} --> n{base}"));
        }
        for child in children {
            let parent = if experiment(child) { base } else { index };
            self.visit(child, Some(parent), next_owner);
        }
    }
}

/// `Array.isArray(node.children) ? node.children.map(record) : []`.
fn children(node: &JsObject) -> Vec<&JsObject> {
    node.get("children")
        .and_then(JsValue::as_array)
        .map_or_else(Vec::new, |all| {
            all.iter().map(|child| record(Some(child))).collect()
        })
}

/// `cell`: one terminal cell per character, with nothing Mermaid parses inside a label (§3.2).
fn cell(value: Option<&JsValue>, limit: usize) -> String {
    clean(&text(value, 400), limit)
}

/// `cell` of a string.
fn cell_str(value: &str, limit: usize) -> String {
    clean(&slice_to(&safe_text_str(value), 400), limit)
}

fn clean(text: &str, limit: usize) -> String {
    let plain = WIDE
        .replace_all(text, "")
        .replace(['"', '`'], "'")
        .replace('<', "‹")
        .replace('>', "›");
    let plain = js_collapse_ws(&plain);
    let plain = js_trim(&plain);
    if length(plain) > limit {
        format!("{}…", slice_to(plain, limit - 1))
    } else {
        plain.to_owned()
    }
}

/// `title(value, width, lines)` for `whole = cell(value, 400)`: up to `lines` box lines joined by
/// `<br>`, each broken at a word where one fits; the last ends in … when cut (§3.3).
fn title(whole: &str, width: usize, lines: usize) -> String {
    if length(whole) <= width || lines <= 1 {
        return cell_str(whole, width);
    }
    let cut = match last_index_of_char(whole, ' ', width) {
        Some(space) if 2 * space > width => space,
        _ => width,
    };
    let rest = cell_str(js_trim(&slice_range(whole, cut, usize::MAX)), 400);
    format!(
        "{}<br>{}",
        js_trim(&slice_to(whole, cut)),
        title(&rest, width, lines - 1)
    )
}

/// `fact`: progress, a score, an attempt or a version, short enough for a box (§3.4).
fn fact(node: &JsObject) -> String {
    let progress = record(node.get("progress"));
    if let Some(JsValue::Number(total)) = progress.get("total")
        && *total > 0.0
    {
        return progress_bar(to_number(progress.get("done")), *total, 6);
    }
    let detail = text(node.get("detail"), 200);
    if let Some(score) = SCORE
        .captures(&detail)
        .map(|captures| js_number(&captures[1]))
        && score.is_finite()
    {
        return number_string(nosh_wire::js::to_precision_number(score, 4));
    }
    ATTEMPT
        .find(&detail)
        .or_else(|| VERSION.find(&detail))
        .map_or(String::new(), |found| found.as_str().to_owned())
}

/// §3.5 steps 4-5: renders a source and finds every box whose `#k` names one of its `count` nodes.
fn render(source: &str, count: usize) -> Geometry {
    // The engine pads every row to the canvas width, so TS's `padEnd` changes nothing.
    let lines = mermaid_ascii::render(source);
    let grid: Vec<Vec<char>> = lines.iter().map(|line| line.chars().collect()).collect();
    let at = |row: usize, column: usize| grid.get(row).and_then(|line| line.get(column)).copied();
    let mut rects = Vec::new();
    for (top, line) in grid.iter().enumerate() {
        for (left, &corner) in line.iter().enumerate() {
            if corner != '┌' {
                continue;
            }
            let mut right = left + 1;
            while matches!(at(top, right), Some('─' | '┬' | '┴' | '┼')) {
                right += 1;
            }
            let mut bottom = top + 1;
            while matches!(at(bottom, left), Some('│' | '├' | '┤' | '┼')) {
                bottom += 1;
            }
            if at(top, right) != Some('┐')
                || at(bottom, left) != Some('└')
                || at(bottom, right) != Some('┘')
            {
                continue;
            }
            let inside: Vec<String> = grid[top + 1..bottom]
                .iter()
                .map(|row| row[left + 1..right].iter().collect())
                .collect();
            if let Some(index) = TAG
                .captures(&inside.join(" "))
                .and_then(|captures| captures[1].parse::<usize>().ok())
                .filter(|&index| index < count)
            {
                rects.push(Rect {
                    index,
                    top,
                    left,
                    bottom,
                    right,
                });
            }
        }
    }
    rects.sort_by_key(|rect| rect.index);
    Geometry {
        lines,
        rects,
        shrunk: RefCell::default(),
    }
}

/// `moveSelection`: the nearest box in a direction, or `current` at an edge or when out of range
/// (viewmodel §3.6). Cells are about twice as tall as wide.
pub fn move_selection(boxes: &[GraphBox], current: usize, direction: GraphDirection) -> usize {
    let Some(from) = boxes.get(current) else {
        return current;
    };
    let centre = |graph_box: &GraphBox| {
        (
            (graph_box.top + graph_box.bottom) as f64 / 2.0 * 2.0,
            (graph_box.left + graph_box.right) as f64 / 2.0,
        )
    };
    let (origin_y, origin_x) = centre(from);
    let mut best = current;
    let mut best_score = f64::INFINITY;
    for (index, graph_box) in boxes.iter().enumerate() {
        if index == current {
            continue;
        }
        let (y, x) = centre(graph_box);
        let (dy, dx) = (y - origin_y, x - origin_x);
        let (along, across) = match direction {
            GraphDirection::Down => (dy, dx),
            GraphDirection::Up => (-dy, dx),
            GraphDirection::Right => (dx, dy),
            GraphDirection::Left => (-dx, dy),
        };
        if along <= 0.0 {
            continue;
        }
        let score = along + 2.0 * across.abs();
        if score < best_score {
            best_score = score;
            best = index;
        }
    }
    best
}

/// `suggestedCommand`: the staging command a node needs (a retry, a resume, a job's details), or
/// `""` (viewmodel §3.7).
pub fn suggested_command(graph_box: &GraphBox) -> String {
    let node = record(Some(&graph_box.node));
    let kind = text(node.get("kind"), TEXT_LIMIT);
    let state = text(node.get("state"), 40);
    let id = text(node.get("id"), 80);
    let stuck = |state: &str| state == "failed" || state == "blocked";
    let owner = record(graph_box.owner.as_ref());
    if kind == "node"
        && stuck(&state)
        && let Some(JsValue::Number(version)) = owner.get("version")
    {
        return format!(
            "/retry {}s {} {} {id}",
            text(owner.get("kind"), TEXT_LIMIT),
            text(owner.get("id"), 80),
            number_string(*version)
        );
    }
    if (kind == "mission" || kind == "direction")
        && state == "blocked"
        && let Some(JsValue::Number(version)) = node.get("version")
    {
        let version = number_string(*version);
        // Resuming while a node is still failed re-blocks at once (no ready frontier), so retry
        // that node first.
        return match children(node).into_iter().find(|child| {
            text(child.get("kind"), TEXT_LIMIT) == "node" && stuck(&text(child.get("state"), 40))
        }) {
            Some(child) => format!(
                "/retry {kind}s {id} {version} {}",
                text(child.get("id"), 80)
            ),
            None => {
                let resumed = if kind == "mission" {
                    "running"
                } else {
                    "active"
                };
                format!("/transition {kind}s {id} {version} {resumed}")
            }
        };
    }
    if kind == "job" && !id.is_empty() {
        return format!("/job {id}");
    }
    String::new()
}

/// `boxAt`: the position of the first box drawn at a layout cell, or -1. Coordinates may be
/// negative (viewmodel §3.8).
pub fn box_at(boxes: &[GraphBox], x: i64, y: i64) -> i32 {
    let within = |value: i64, low: usize, high: usize| (low as i64..=high as i64).contains(&value);
    boxes
        .iter()
        .position(|graph_box| {
            within(x, graph_box.left, graph_box.right) && within(y, graph_box.top, graph_box.bottom)
        })
        .map_or(-1, |position| position as i32)
}

/// `shrinkGraph`: the layout zoomed out by an even `factor`; the same layout and factor return the
/// same `Rc` (viewmodel §3.9, test 5 checks identity).
pub fn shrink_graph(layout: &Rc<GraphLayout>, factor: usize) -> Rc<ShrunkGraph> {
    // TS samples fractional columns for odd factors; the map view zooms by 2, 4 and 8 only.
    debug_assert!(factor >= 2 && factor.is_multiple_of(2), "even factors only");
    let geometry = &layout.geometry;
    let mut shrunk = geometry.shrunk.borrow_mut();
    Rc::clone(
        shrunk
            .entry(factor)
            .or_insert_with(|| Rc::new(shrink(geometry, factor))),
    )
}

/// Each output cell covers a factor-by-factor block of layout cells. A dot is set where its part of
/// the block holds any character, the glyph belongs to the box owning most of its inked cells, and
/// each box keeps its `#k` tag where it fits.
fn shrink(geometry: &Geometry, factor: usize) -> ShrunkGraph {
    let grid: Vec<Vec<char>> = geometry
        .lines
        .iter()
        .map(|line| line.chars().collect())
        .collect();
    let height = grid.len();
    let width = grid.first().map_or(0, Vec::len);
    let mut owner_of = vec![vec![-1; width]; height];
    for (position, rect) in geometry.rects.iter().enumerate() {
        for row in &mut owner_of[rect.top..=rect.bottom] {
            row[rect.left..=rect.right].fill(position as i32);
        }
    }
    let (rows, columns) = (height.div_ceil(factor), width.div_ceil(factor));
    let mut glyphs = vec![vec![' '; columns]; rows];
    let mut owners = vec![vec![-1; columns]; rows];
    for y in 0..rows {
        for x in 0..columns {
            let mut bits = 0;
            let mut votes: HashMap<i32, usize> = HashMap::new();
            for (dy, dots) in DOTS.iter().enumerate() {
                for (dx, dot) in dots.iter().enumerate() {
                    // `floor(y·f + dy·f/4)` and `floor(y·f + (dy+1)·f/4)` in integers.
                    let top = (4 * y * factor + dy * factor) / 4;
                    let bottom =
                        height.min((top + 1).max((4 * y * factor + (dy + 1) * factor) / 4));
                    let left = x * factor + dx * factor / 2;
                    let right = width.min(left + factor / 2);
                    for row in top..bottom {
                        for column in left..right {
                            if grid[row][column] != ' ' {
                                bits |= dot;
                                *votes.entry(owner_of[row][column]).or_default() += 1;
                            }
                        }
                    }
                }
            }
            if bits != 0 {
                glyphs[y][x] = char::from_u32(0x2800 + bits).expect("a Braille pattern");
            }
            // Most votes, ties to the larger owner (so a box beats -1).
            if let Some((owner, _)) = votes
                .into_iter()
                .max_by_key(|&(owner, count)| (count, owner))
            {
                owners[y][x] = owner;
            }
        }
    }
    for (position, rect) in geometry.rects.iter().enumerate() {
        let tag: Vec<char> = format!("#{}", rect.index).chars().collect();
        let left = rect.left.div_ceil(factor) + 1;
        // `right - left + 1 < tag.length`, with `right = floor(box.right / factor) - 1`.
        if rect.right / factor < left + tag.len() {
            continue;
        }
        let right = rect.right / factor - 1;
        let row = (rect.top + rect.bottom) / (2 * factor);
        let start = left.max((left + right + 1 - tag.len()) / 2);
        for (offset, &c) in tag.iter().enumerate() {
            glyphs[row][start + offset] = c;
            owners[row][start + offset] = position as i32;
        }
    }
    ShrunkGraph {
        lines: glyphs.into_iter().map(String::from_iter).collect(),
        owners,
    }
}
