//! Grid placement: `converter.ts`, `grid.ts` and `shapes/rectangle.ts` of beautiful-mermaid
//! (tui-viewmodel.md A.3 box dimensions, A.4 TD placement with the growing level tracker, A.5 column
//! widths and row heights, A.8 drawing coordinates). Coordinates are `i64` (A.13).

use std::collections::HashMap;

use super::Cell;
use super::parse::Flowchart;

/// The placed grid: each node's top-left grid cell, the cells node blocks occupy, and the column
/// widths and row heights by index.
pub(super) struct Grid {
    pub(super) positions: Vec<Cell>,
    pub(super) occupied: HashMap<Cell, usize>,
    pub(super) column_width: HashMap<i64, i64>,
    pub(super) row_height: HashMap<i64, i64>,
}

/// Places every node of a TD flowchart in a 3×3 block of grid cells and sizes the columns and rows
/// (A.4, A.5). `None` if a node stays unplaced, which TS would crash on; it cannot happen for
/// generated sources, where every node descends from `n0`.
pub(super) fn place(chart: &Flowchart) -> Option<Grid> {
    let count = chart.labels.len();
    let mut children = vec![Vec::new(); count];
    for &(from, to) in &chart.edges {
        children[from].push(to);
    }
    let mut found = vec![false; count];
    let mut roots = Vec::new();
    for node in 0..count {
        if !found[node] {
            roots.push(node);
        }
        found[node] = true;
        for &child in &children[node] {
            found[child] = true;
        }
    }
    let mut positions = vec![None; count];
    let mut occupied = HashMap::new();
    // `reserveSpotInGrid`: only the top-left cell is checked; a taken spot moves 4 cells right.
    let mut reserve = |positions: &mut [Option<Cell>], node: usize, (mut x, y): Cell| {
        while occupied.contains_key(&(x, y)) {
            x += 4;
        }
        for dx in 0..3 {
            for dy in 0..3 {
                occupied.insert((x + dx, y + dy), node);
            }
        }
        positions[node] = Some((x, y));
    };
    // `highestPositionPerLevel`: TS allocates 100 levels, so a node 25 levels deep reads
    // `undefined`, gets NaN coordinates and A* never ends. This map grows instead (the documented
    // fix, DECISIONS.md item 2).
    let mut highest: HashMap<i64, i64> = HashMap::new();
    for &root in &roots {
        let x = highest.get(&0).copied().unwrap_or(0);
        reserve(&mut positions, root, (x, 0));
        highest.insert(0, x + 4);
    }
    let mut placed = roots.len();
    while placed < count {
        let before = placed;
        for node in 0..count {
            let Some((_, y)) = positions[node] else {
                continue;
            };
            for &child in &children[node] {
                if positions[child].is_some() {
                    continue;
                }
                let level = y + 4;
                let x = highest.get(&level).copied().unwrap_or(0);
                reserve(&mut positions, child, (x, level));
                highest.insert(level, x + 4);
                placed += 1;
            }
        }
        if placed == before {
            break;
        }
    }
    let mut grid = Grid {
        positions: positions.into_iter().collect::<Option<_>>()?,
        occupied,
        column_width: HashMap::new(),
        row_height: HashMap::new(),
    };
    for (label, &(x, y)) in chart.labels.iter().zip(&grid.positions) {
        // A.3 with `boxBorderPadding` 0; the content row count is made odd. Every label is BMP-only
        // (`cell` removed astral characters), so chars are UTF-16 units.
        let lines = label.split('\n').map(|line| line.chars().count() as i64);
        let (width, count) =
            lines.fold((0, 0), |(width, count), line| (width.max(line), count + 1));
        let height = if count % 2 == 0 { count + 1 } else { count };
        for (offset, (column, row)) in [(1, 1), (width, height), (1, 1)].into_iter().enumerate() {
            widen(&mut grid.column_width, x + offset as i64, column);
            widen(&mut grid.row_height, y + offset as i64, row);
        }
        // `paddingX` and `paddingY` before the block.
        if x > 0 {
            widen(&mut grid.column_width, x - 1, 3);
        }
        if y > 0 {
            widen(&mut grid.row_height, y - 1, 3);
        }
    }
    Some(grid)
}

fn widen(sizes: &mut HashMap<i64, i64>, key: i64, size: i64) {
    let entry = sizes.entry(key).or_insert(0);
    *entry = (*entry).max(size);
}

impl Grid {
    /// `increaseGridSizeForPath`: a path cell outside every block gets a column or row of
    /// `floor(padding / 2)` = 1.
    pub(super) fn grow_for_path(&mut self, path: &[Cell]) {
        for &(x, y) in path {
            self.column_width.entry(x).or_insert(1);
            self.row_height.entry(y).or_insert(1);
        }
    }

    /// `gridToDrawingCoord`: the character cell at the centre of a grid cell (A.8).
    pub(super) fn drawing(&self, (x, y): Cell) -> Cell {
        let start = |sizes: &HashMap<i64, i64>, index: i64| {
            let before: i64 = (0..index)
                .map(|i| sizes.get(&i).copied().unwrap_or(0))
                .sum();
            before + sizes.get(&index).copied().unwrap_or(0) / 2
        };
        (start(&self.column_width, x), start(&self.row_height, y))
    }

    /// A node box's `(w, h)`: its border and content columns and rows, so the box canvas is
    /// `(w + 1) × (h + 1)` (A.9).
    pub(super) fn box_size(&self, node: usize) -> (i64, i64) {
        let (x, y) = self.positions[node];
        let size = |sizes: &HashMap<i64, i64>, index: i64| {
            sizes.get(&index).copied().unwrap_or(0) + sizes.get(&(index + 1)).copied().unwrap_or(0)
        };
        (size(&self.column_width, x), size(&self.row_height, y))
    }

    /// The canvas size: the sum of every column width and row height (`setCanvasSizeToGrid`).
    pub(super) fn canvas_size(&self) -> (i64, i64) {
        (
            self.column_width.values().sum::<i64>().max(1),
            self.row_height.values().sum::<i64>().max(1),
        )
    }
}
