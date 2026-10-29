//! Box and edge drawing: `draw.ts` of beautiful-mermaid (tui-viewmodel.md A.9 box drawing, A.10
//! edge lines, corners and arrowheads).

use super::canvas::Canvas;
use super::grid::Grid;
use super::parse::Flowchart;

/// Draws every node box of the chart onto a layer (A.9).
pub(super) fn draw_boxes(chart: &Flowchart, grid: &Grid) -> Canvas {
    todo!()
}

/// Draws one routed edge path onto a layer (A.10).
pub(super) fn draw_edge(grid: &Grid, path: &[(i64, i64)]) -> Canvas {
    todo!()
}
