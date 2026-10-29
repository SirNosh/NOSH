//! Fan-out bundles: `edge-bundling.ts` of beautiful-mermaid (tui-viewmodel.md A.6).

use super::grid::Grid;
use super::parse::Flowchart;

/// The edges, as indices into `chart.edges`, that leave one source together as a TD fan-out bundle.
pub(super) fn bundles(chart: &Flowchart, grid: &Grid) -> Vec<Vec<usize>> {
    todo!()
}
