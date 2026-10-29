//! The subset of `beautiful-mermaid@1.1.3` that `layoutGraph` uses (`renderMermaidASCII` with
//! `colorMode: 'none'`, `paddingX: 3`, `paddingY: 3`, `boxBorderPadding: 0`): TD flowcharts of
//! rectangles joined by solid unlabelled `-->` edges, drawn with Unicode box characters.
//!
//! Specified by tui-viewmodel.md Appendix A (A.1-A.13) and §7.1; DECISIONS.md item 2 (Option A,
//! byte-identical to the §6 goldens; the level tracker grows instead of hanging past 24 levels).
//! Generated sources are trees rooted at `n0`, which is all the routing and drawing handle.

mod canvas;
mod draw;
mod grid;
mod parse;
mod route;

/// A grid cell or a drawing (character) cell, `(x, y)`.
type Cell = (i64, i64);

/// `renderMermaidASCII(source, …).split('\n')`: the picture rows, all the same length (A.1). Input
/// the engine would throw on cannot come from generated sources; it yields no rows instead of a
/// panic (A.13).
pub(crate) fn render(source: &str) -> Vec<String> {
    let Some(chart) = parse::parse(source) else {
        return Vec::new();
    };
    let Some(mut grid) = grid::place(&chart) else {
        return Vec::new();
    };
    let (routes, bundles) = route::route(&chart, &mut grid);
    let mut canvas = canvas::Canvas::new(grid.canvas_size());
    canvas.merge(&draw::boxes(&chart, &grid));
    for layer in draw::edges(&grid, &routes, &bundles) {
        canvas.merge(&layer);
    }
    canvas.rows()
}
