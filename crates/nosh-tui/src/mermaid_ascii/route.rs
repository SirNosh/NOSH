//! Single-edge routing: `edge-routing.ts` and `pathfinder.ts` of beautiful-mermaid (tui-viewmodel.md
//! A.7: `determinePath`, the A* search over grid cells with its `MinHeap`, and `mergePath`).

use super::grid::Grid;

/// The grid path of one edge from node `from` to node `to`, after `mergePath` (A.7).
pub(super) fn determine_path(grid: &Grid, from: usize, to: usize) -> Vec<(i64, i64)> {
    todo!()
}
