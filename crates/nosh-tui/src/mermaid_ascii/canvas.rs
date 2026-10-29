//! The character canvas: `canvas.ts` of beautiful-mermaid (tui-viewmodel.md A.11: layers, merging
//! with junction characters, and the final rows).

/// A character canvas indexed `[x][y]`, as the TS layers are; blank cells are spaces.
pub(super) struct Canvas {
    pub(super) cells: Vec<Vec<char>>,
}

impl Canvas {
    /// Overlays `layer` in drawing order, merging box-drawing junctions (A.11).
    pub(super) fn merge(&mut self, layer: &Canvas) {
        todo!()
    }

    /// The output rows, all the same length (A.11).
    pub(super) fn rows(&self) -> Vec<String> {
        todo!()
    }
}
