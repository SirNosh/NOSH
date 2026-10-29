//! Box and edge drawing: `draw.ts` of beautiful-mermaid (tui-viewmodel.md A.9 box drawing, A.10
//! edge lines, corners, junctions and arrowheads, A.11 layer order).
//!
//! TS draws every item onto its own full-size blank layer and merges the layers one by one. No
//! layer writes a cell twice, so a list of `(cell, char)` writes merged in order is the same (A.13).
//!
//! Rectangles have 1-wide border columns and rows, so a node's `attach(Up)` and `attach(Down)`
//! points are the drawing coordinates of its top-centre and bottom-centre grid cells, which are the
//! ends of every bundled path; the paths are drawn from those directly.

use super::Cell;
use super::grid::Grid;
use super::parse::Flowchart;
use super::route::{Bundle, Dir, Route, direction};

/// Character writes in drawing order.
pub(super) type Layer = Vec<(Cell, char)>;

/// Every node box with its label (A.9), in node order.
pub(super) fn boxes(chart: &Flowchart, grid: &Grid) -> Layer {
    let mut cells = Vec::new();
    for (node, label) in chart.labels.iter().enumerate() {
        let (x, y) = grid.drawing(grid.positions[node]);
        let (w, h) = grid.box_size(node);
        for dx in 1..w {
            cells.extend([((x + dx, y), '─'), ((x + dx, y + h), '─')]);
        }
        for dy in 1..h {
            cells.extend([((x, y + dy), '│'), ((x + w, y + dy), '│')]);
        }
        cells.extend([
            ((x, y), '┌'),
            ((x + w, y), '┐'),
            ((x, y + h), '└'),
            ((x + w, y + h), '┘'),
        ]);
        // Boxes share their column's width and their level's height, so the widest line starts
        // at 1 and no line reaches the border.
        let lines: Vec<Vec<char>> = label
            .split('\n')
            .map(|line| line.chars().collect())
            .collect();
        let top = h / 2 - (lines.len() as i64 - 1) / 2;
        for (row, line) in (top..).zip(&lines) {
            let left = w / 2 - (line.len() as i64 + 1) / 2 + 1;
            cells.extend(
                (left..)
                    .zip(line)
                    .map(|(column, &c)| ((x + column, y + row), c)),
            );
        }
    }
    cells
}

/// The edge layers by kind, each in TS's push order: lines, corners, junctions, arrowheads and box
/// starts, which `drawGraph` merges in that order (A.11). Bundled edges get no box start; the
/// start-arrowhead and label layers are blank.
pub(super) fn edges(grid: &Grid, routes: &[Route], bundles: &[Bundle]) -> [Layer; 5] {
    let [
        mut lines,
        mut corners,
        mut junctions,
        mut arrows,
        mut starts,
    ] = <[Layer; 5]>::default();
    let mut drawn = vec![false; bundles.len()];
    for route in routes {
        match route {
            Route::Fan { bundle, path } => {
                for pair in path.windows(2) {
                    lines.extend(draw_line(grid.drawing(pair[0]), grid.drawing(pair[1])));
                }
                draw_corners(grid, path, &mut corners);
                if !std::mem::replace(&mut drawn[*bundle], true) {
                    let Bundle { junction, shared } = &bundles[*bundle];
                    for pair in shared.windows(2) {
                        lines.extend(draw_line(grid.drawing(pair[0]), grid.drawing(pair[1])));
                    }
                    draw_corners(grid, shared, &mut corners);
                    junctions.push((grid.drawing(*junction), tee(*bundle, routes, shared)));
                }
                let [.., before, last] = path.as_slice() else {
                    continue;
                };
                let arrow = match direction(*before, *last) {
                    d @ (Dir::Up | Dir::Down | Dir::Left | Dir::Right) => arrowhead(d),
                    _ => '▼',
                };
                let (x, y) = grid.drawing(*last);
                arrows.push(((x, y - 1), arrow));
            }
            Route::Single(path) => {
                // `drawPath`: each drawn segment's cells (the start cell if it drew none) and grid
                // direction.
                let mut segments = Vec::new();
                for pair in path.windows(2) {
                    let (from, to) = (grid.drawing(pair[0]), grid.drawing(pair[1]));
                    if from == to {
                        continue;
                    }
                    let segment = draw_line(from, to);
                    let cells: Vec<Cell> = segment.iter().map(|&(cell, _)| cell).collect();
                    lines.extend(segment);
                    segments.push((
                        if cells.is_empty() { vec![from] } else { cells },
                        direction(pair[0], pair[1]),
                    ));
                }
                if let (Some((first, _)), Some((last, last_dir))) =
                    (segments.first(), segments.last())
                {
                    let (x, y) = first[0];
                    match direction(path[0], path[1]) {
                        Dir::Up => starts.push(((x, y + 1), '┴')),
                        Dir::Down => starts.push(((x, y - 1), '┬')),
                        Dir::Left => starts.push(((x + 1, y), '┤')),
                        Dir::Right => starts.push(((x - 1, y), '├')),
                        _ => {}
                    }
                    let end = last[last.len() - 1];
                    let d = if last.len() == 1 {
                        *last_dir
                    } else {
                        direction(last[0], end)
                    };
                    arrows.push((end, arrowhead(d)));
                }
                draw_corners(grid, path, &mut corners);
            }
        }
    }
    [lines, corners, junctions, arrows, starts]
}

/// `drawLine` with offsets `(1, -1)`: the cells strictly between two drawing points, straight or
/// horizontal-then-vertical (A.10).
fn draw_line((fx, fy): Cell, (tx, ty): Cell) -> Layer {
    let row =
        |y: i64, from: i64, to: i64, step: i64| steps(from, to, step).map(move |x| ((x, y), '─'));
    let column =
        |x: i64, from: i64, to: i64, step: i64| steps(from, to, step).map(move |y| ((x, y), '│'));
    match direction((fx, fy), (tx, ty)) {
        Dir::Up => column(fx, fy - 1, ty + 1, -1).collect(),
        Dir::Down => column(fx, fy + 1, ty - 1, 1).collect(),
        Dir::Left => row(fy, fx - 1, tx + 1, -1).collect(),
        Dir::Right => row(fy, fx + 1, tx - 1, 1).collect(),
        Dir::UpperLeft => row(fy, fx - 1, tx, -1)
            .chain(column(tx, fy - 1, ty + 1, -1))
            .collect(),
        Dir::UpperRight => row(fy, fx + 1, tx, 1)
            .chain(column(tx, fy - 1, ty + 1, -1))
            .collect(),
        Dir::LowerLeft => row(fy, fx - 1, tx, -1)
            .chain(column(tx, fy + 1, ty - 1, 1))
            .collect(),
        Dir::LowerRight if tx - fx <= 1 => column(fx, fy + 1, ty - 1, 1).collect(),
        Dir::LowerRight => row(fy, fx + 1, tx, 1)
            .chain(column(tx, fy + 1, ty - 1, 1))
            .collect(),
    }
}

/// `from, from + step, …` up to and including `to`; empty when `from` is already past `to`.
fn steps(from: i64, to: i64, step: i64) -> impl Iterator<Item = i64> {
    (0..((to - from) * step + 1).max(0)).map(move |i| from + i * step)
}

/// A corner character at every interior point of a grid path, from the grid directions into and
/// out of it.
fn draw_corners(grid: &Grid, path: &[Cell], corners: &mut Layer) {
    corners.extend(path.windows(3).map(|w| {
        let corner = match (direction(w[0], w[1]), direction(w[1], w[2])) {
            (Dir::Right, Dir::Down) | (Dir::Up, Dir::Left) => '┐',
            (Dir::Right, Dir::Up) | (Dir::Down, Dir::Left) => '┘',
            (Dir::Left, Dir::Down) | (Dir::Up, Dir::Right) => '┌',
            (Dir::Left, Dir::Up) | (Dir::Down, Dir::Right) => '└',
            _ => '+',
        };
        (grid.drawing(w[1]), corner)
    }));
}

fn arrowhead(d: Dir) -> char {
    match d {
        Dir::Up => '▲',
        Dir::Down => '▼',
        Dir::Left => '◄',
        Dir::Right => '►',
        Dir::UpperRight => '◥',
        Dir::UpperLeft => '◤',
        Dir::LowerRight => '◢',
        Dir::LowerLeft => '◣',
    }
}

/// `drawJunctionCharacter`: the tee or corner joining a bundle's shared path to its edges.
fn tee(bundle: usize, routes: &[Route], shared: &[Cell]) -> char {
    let (mut up, mut down, mut left, mut right) = (false, false, false, false);
    let mut mark = |d: Dir| match d {
        Dir::Up => up = true,
        Dir::Down => down = true,
        Dir::Left => left = true,
        Dir::Right => right = true,
        _ => {}
    };
    // The shared path ends at the junction; each edge's path starts there.
    if let [.., before, last] = shared {
        mark(direction(*last, *before));
    }
    for route in routes {
        if let Route::Fan { bundle: b, path } = route
            && *b == bundle
            && let [first, next, ..] = path.as_slice()
        {
            mark(direction(*next, *first).opposite());
        }
    }
    match (up, down, left, right) {
        (true, true, true, true) => '┼',
        (false, true, true, true) => '┬',
        (true, false, true, true) => '┴',
        (true, true, false, true) => '├',
        (true, true, true, false) => '┤',
        (_, _, true, true) => '─',
        (true, true, _, _) => '│',
        (_, true, _, true) => '┌',
        (_, true, true, _) => '┐',
        (true, _, _, true) => '└',
        (true, _, true, _) => '┘',
        _ => '┼',
    }
}
