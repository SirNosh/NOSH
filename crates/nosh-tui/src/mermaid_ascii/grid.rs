//! Grid placement: `converter.ts`, `grid.ts` and `shapes/rectangle.ts` of beautiful-mermaid
//! (tui-viewmodel.md A.3 box dimensions, A.4 TD placement with the growing level tracker, A.5 column
//! widths and row heights, A.8 drawing coordinates). Coordinates are `i64` (A.13).

use std::collections::HashMap;

use super::parse::Flowchart;

/// The placed grid: each node's top-left grid cell, and the column widths and row heights by index.
pub(super) struct Grid {
    pub(super) positions: Vec<(i64, i64)>,
    pub(super) column_width: HashMap<i64, i64>,
    pub(super) row_height: HashMap<i64, i64>,
}

/// Places every node of a TD flowchart (A.4, A.5).
pub(super) fn place(chart: &Flowchart) -> Grid {
    todo!()
}
