//! The subset of `beautiful-mermaid@1.1.3` that `layoutGraph` uses (`renderMermaidASCII` with
//! `colorMode: 'none'`, `paddingX: 3`, `paddingY: 3`, `boxBorderPadding: 0`): TD flowcharts of
//! rectangles joined by solid unlabelled `-->` edges, drawn with Unicode box characters.
//!
//! Specified by tui-viewmodel.md Appendix A (A.1-A.13) and §7.1; DECISIONS.md item 2 (Option A,
//! byte-identical to the §6 goldens; the level tracker grows instead of hanging past 24 levels).
//! Private to the crate and owned by Track B: the submodule items below are a suggested shape that
//! Track B may change freely, as long as [`render`] keeps its signature for `map_graph`.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

mod bundle;
mod canvas;
mod draw;
mod grid;
mod parse;
mod route;

/// `renderMermaidASCII(source, …).split('\n')`: the picture rows, all the same length (A.1). Input
/// the engine would throw on cannot come from generated sources; it yields no rows instead of a
/// panic (A.13).
pub(crate) fn render(source: &str) -> Vec<String> {
    todo!()
}
