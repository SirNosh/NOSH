//! Edge routing: `edge-bundling.ts`, `edge-routing.ts` and `pathfinder.ts` of beautiful-mermaid
//! (tui-viewmodel.md A.6 fan-out bundles, A.7 `determinePath`, the A* search with its `MinHeap`,
//! and `mergePath`).

use std::collections::HashMap;

use super::Cell;
use super::grid::Grid;
use super::parse::Flowchart;

/// A direction between two cells, which also names a cell of a node's 3×3 block (`types.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dir {
    Up,
    Down,
    Left,
    Right,
    UpperRight,
    UpperLeft,
    LowerRight,
    LowerLeft,
}

impl Dir {
    /// The block cell this direction names, relative to the block's top-left cell.
    fn offset(self) -> Cell {
        match self {
            Dir::Up => (1, 0),
            Dir::Down => (1, 2),
            Dir::Left => (0, 1),
            Dir::Right => (2, 1),
            Dir::UpperRight => (2, 0),
            Dir::UpperLeft => (0, 0),
            Dir::LowerRight => (2, 2),
            Dir::LowerLeft => (0, 2),
        }
    }

    /// `getOpposite`.
    pub(super) fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
            Dir::UpperRight => Dir::LowerLeft,
            Dir::UpperLeft => Dir::LowerRight,
            Dir::LowerRight => Dir::UpperLeft,
            Dir::LowerLeft => Dir::UpperRight,
        }
    }
}

/// `determineDirection`: the 8-way direction from one cell to another (`Up` for equal cells).
pub(super) fn direction((fx, fy): Cell, (tx, ty): Cell) -> Dir {
    match (tx.cmp(&fx), ty.cmp(&fy)) {
        (std::cmp::Ordering::Equal, y) => {
            if y.is_gt() {
                Dir::Down
            } else {
                Dir::Up
            }
        }
        (x, std::cmp::Ordering::Equal) => {
            if x.is_gt() {
                Dir::Right
            } else {
                Dir::Left
            }
        }
        (x, y) => match (x.is_gt(), y.is_gt()) {
            (true, true) => Dir::LowerRight,
            (true, false) => Dir::UpperRight,
            (false, true) => Dir::LowerLeft,
            (false, false) => Dir::UpperLeft,
        },
    }
}

/// How one edge is drawn.
pub(super) enum Route {
    /// A non-bundled edge: its whole grid path.
    Single(Vec<Cell>),
    /// An edge of `bundles[bundle]`: its grid path from the junction to the target's top centre.
    Fan { bundle: usize, path: Vec<Cell> },
}

/// A fan-out bundle (A.6): the edges leaving one source together through the junction under it.
pub(super) struct Bundle {
    pub(super) junction: Cell,
    /// The grid path from the source's bottom centre to the junction.
    pub(super) shared: Vec<Cell>,
}

/// Routes every edge (A.6, A.7) and widens the grid for the cells the paths use (A.5).
///
/// Edges are grouped by source in order of each source's first edge; a source with two or more
/// edges forms a fan-out bundle. Generated sources are trees, so no node has two incoming edges (no
/// fan-in bundle) and none points at itself, and every edge is solid and unlabelled (`canBundle`).
pub(super) fn route(chart: &Flowchart, grid: &mut Grid) -> (Vec<Route>, Vec<Bundle>) {
    let mut by_source: Vec<(usize, Vec<usize>)> = Vec::new();
    for (edge, &(from, _)) in chart.edges.iter().enumerate() {
        match by_source.iter_mut().find(|(source, _)| *source == from) {
            Some((_, edges)) => edges.push(edge),
            None => by_source.push((from, vec![edge])),
        }
    }
    let mut fans: Vec<Option<Route>> = chart.edges.iter().map(|_| None).collect();
    let mut bundles = Vec::new();
    for (source, edges) in by_source {
        if edges.len() < 2 {
            continue;
        }
        let (x, y) = grid.positions[source];
        let junction = (x + 1, y + 3);
        let shared = path_or_line(grid, (x + 1, y + 2), junction);
        for edge in edges {
            let (tx, ty) = grid.positions[chart.edges[edge].1];
            fans[edge] = Some(Route::Fan {
                bundle: bundles.len(),
                path: path_or_line(grid, junction, (tx + 1, ty)),
            });
        }
        bundles.push(Bundle { junction, shared });
    }
    let routes: Vec<Route> = fans
        .into_iter()
        .zip(&chart.edges)
        .map(|(fan, &edge)| fan.unwrap_or_else(|| Route::Single(determine_path(grid, edge))))
        .collect();
    // TS widens per edge right after routing it; routing reads only `occupied`, so the order of
    // these idempotent insertions is immaterial.
    for route in &routes {
        match route {
            Route::Single(path) => grid.grow_for_path(path),
            Route::Fan { bundle, path } => {
                grid.grow_for_path(&bundles[*bundle].shared);
                grid.grow_for_path(path);
            }
        }
    }
    (routes, bundles)
}

/// `mergePath(getPath(from, to))`, or the straight `[from, to]` when no path exists.
fn path_or_line(grid: &Grid, from: Cell, to: Cell) -> Vec<Cell> {
    get_path(&grid.occupied, from, to).map_or_else(|| vec![from, to], merge_path)
}

/// `determinePath` for TD (A.7): the preferred and the alternative start/end block cells, keeping
/// the shorter merged path (the preferred one on a tie).
fn determine_path(grid: &Grid, (from, to): (usize, usize)) -> Vec<Cell> {
    let (source, target) = (grid.positions[from], grid.positions[to]);
    let d = direction(source, target);
    // `determineStartAndEndDir` for TD. Trees place children 4 rows below, so only `Down`,
    // `LowerLeft` and `LowerRight` occur; self-loops (handled separately in TS) cannot.
    let (preferred, preferred_end, alternative, alternative_end) = match d {
        Dir::LowerRight => (Dir::Right, Dir::Up, Dir::Down, Dir::Left),
        Dir::UpperRight => (Dir::Right, Dir::Down, Dir::Up, Dir::Left),
        Dir::LowerLeft => (Dir::Left, Dir::Up, Dir::Down, Dir::Right),
        Dir::UpperLeft => (Dir::Right, Dir::Right, Dir::Up, Dir::Right),
        Dir::Up => (Dir::Right, Dir::Right, Dir::Up, Dir::Down),
        Dir::Down | Dir::Left | Dir::Right => (d, d.opposite(), d, d.opposite()),
    };
    let at = |(x, y): Cell, dir: Dir| {
        let (dx, dy) = dir.offset();
        (x + dx, y + dy)
    };
    let (start, end) = (at(source, preferred), at(target, preferred_end));
    match (
        get_path(&grid.occupied, start, end),
        get_path(
            &grid.occupied,
            at(source, alternative),
            at(target, alternative_end),
        ),
    ) {
        (Some(preferred), Some(alternative)) => {
            let (preferred, alternative) = (merge_path(preferred), merge_path(alternative));
            if preferred.len() <= alternative.len() {
                preferred
            } else {
                alternative
            }
        }
        (Some(path), None) | (None, Some(path)) => merge_path(path),
        (None, None) => vec![start, end],
    }
}

/// The A* open set: `pathfinder.ts`'s array-backed binary min-heap. Its exact sift order decides
/// ties between equally short routes, so `std::collections::BinaryHeap` would draw other paths.
#[derive(Default)]
struct MinHeap {
    items: Vec<(i64, Cell)>,
}

impl MinHeap {
    fn push(&mut self, cell: Cell, priority: i64) {
        self.items.push((priority, cell));
        let mut i = self.items.len() - 1;
        while i > 0 {
            let parent = (i - 1) >> 1;
            if self.items[i].0 >= self.items[parent].0 {
                break;
            }
            self.items.swap(i, parent);
            i = parent;
        }
    }

    fn pop(&mut self) -> Option<Cell> {
        if self.items.is_empty() {
            return None;
        }
        // The last item takes the root's place, then sinks.
        let (_, top) = self.items.swap_remove(0);
        let mut i = 0;
        loop {
            let mut smallest = i;
            for child in [2 * i + 1, 2 * i + 2] {
                if child < self.items.len() && self.items[child].0 < self.items[smallest].0 {
                    smallest = child;
                }
            }
            if smallest == i {
                return Some(top);
            }
            self.items.swap(i, smallest);
            i = smallest;
        }
    }
}

/// `getPath`: A* over grid cells with 4-way moves. Node cells and negative coordinates are
/// blocked except the destination; there is no closed set. It terminates for generated sources:
/// every destination is a block-border cell beside a free padding row or column (a child's top
/// centre sits at row 4 or lower), and all free cells are connected.
fn get_path(occupied: &HashMap<Cell, usize>, from: Cell, to: Cell) -> Option<Vec<Cell>> {
    let mut open = MinHeap::default();
    open.push(from, 0);
    let mut cost = HashMap::from([(from, 0)]);
    let mut came_from: HashMap<Cell, Cell> = HashMap::new();
    while let Some(current) = open.pop() {
        if current == to {
            let mut path = vec![current];
            while let Some(&previous) = came_from.get(path.last().unwrap_or(&current)) {
                path.push(previous);
            }
            path.reverse();
            return Some(path);
        }
        let next_cost = cost[&current] + 1;
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let next = (current.0 + dx, current.1 + dy);
            let blocked = next.0 < 0 || next.1 < 0 || occupied.contains_key(&next);
            if blocked && next != to {
                continue;
            }
            if cost.get(&next).is_none_or(|&known| next_cost < known) {
                cost.insert(next, next_cost);
                open.push(next, next_cost + heuristic(next, to));
                came_from.insert(next, current);
            }
        }
    }
    None
}

/// Manhattan distance plus one when both axes differ, which favours straight lines.
fn heuristic((ax, ay): Cell, (bx, by): Cell) -> i64 {
    let (dx, dy) = ((ax - bx).abs(), (ay - by).abs());
    if dx == 0 || dy == 0 {
        dx + dy
    } else {
        dx + dy + 1
    }
}

/// `mergePath`: drops every interior point whose incoming and outgoing steps are equal.
fn merge_path(path: Vec<Cell>) -> Vec<Cell> {
    if path.len() <= 2 {
        return path;
    }
    let step = |a: Cell, b: Cell| (b.0 - a.0, b.1 - a.1);
    let mut merged = vec![path[0]];
    merged.extend(
        path.windows(3)
            .filter(|w| step(w[0], w[1]) != step(w[1], w[2]))
            .map(|w| w[1]),
    );
    merged.push(path[path.len() - 1]);
    merged
}
