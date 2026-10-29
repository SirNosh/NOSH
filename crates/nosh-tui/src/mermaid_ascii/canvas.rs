//! The character canvas: `canvas.ts` of beautiful-mermaid (tui-viewmodel.md A.11: merging with
//! junction characters, and the final rows).

use super::Cell;

/// `JUNCTION_CHARS`: a write of one onto another is resolved by [`JUNCTION_MAP`].
const JUNCTIONS: &str = "─│┌┐└┘├┤┬┴┼╴╵╶╷";
/// The junctions [`JUNCTION_MAP`] has rows and columns for; any other pair keeps the cell.
const MAPPED: &str = "─│┌┐└┘├┤┬┴";
/// `JUNCTION_MAP[cur][c]`: row `cur`, column `c`, both in [`MAPPED`] order.
const JUNCTION_MAP: [&str; 10] = [
    "─┼┬┬┴┴┼┼┬┴",
    "┼│├┤├┤├┤┼┼",
    "┬├┌┬├┼├┼┬┼",
    "┬┤┬┐┼┤┼┤┬┼",
    "┴├├┼└┴├┼┼┴",
    "┴┤┼┤┴┘┼┤┼┴",
    "┼├├┼├┼├┼┼┼",
    "┼┤┼┤┼┤┼┤┼┼",
    "┬┼┬┬┼┼┼┼┬┼",
    "┴┼┼┼┴┴┼┼┼┴",
];

/// A blank character canvas, row-major (TS's is column-major, which changes nothing visible).
pub(super) struct Canvas {
    rows: Vec<Vec<char>>,
}

impl Canvas {
    pub(super) fn new((width, height): Cell) -> Canvas {
        Canvas {
            rows: vec![vec![' '; width as usize]; height as usize],
        }
    }

    /// `mergeCanvases`: overlays writes in order. Spaces are transparent; writes outside the
    /// canvas, which TS would grow it for, cannot come from generated sources and are dropped.
    pub(super) fn merge(&mut self, cells: &[(Cell, char)]) {
        for &((x, y), c) in cells {
            let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
                continue;
            };
            if let Some(cur) = self.rows.get_mut(y).and_then(|row| row.get_mut(x))
                && c != ' '
            {
                *cur = merged(*cur, c);
            }
        }
    }

    /// The rows, all the canvas width.
    pub(super) fn rows(self) -> Vec<String> {
        self.rows.into_iter().map(String::from_iter).collect()
    }
}

fn merged(cur: char, c: char) -> char {
    if JUNCTIONS.contains(cur) && JUNCTIONS.contains(c) {
        let index = |junction| MAPPED.chars().position(|mapped| mapped == junction);
        index(cur)
            .zip(index(c))
            .and_then(|(row, column)| JUNCTION_MAP[row].chars().nth(column))
            .unwrap_or(cur)
    } else if cur.is_ascii_alphanumeric() && c.is_ascii_alphanumeric() {
        cur
    } else {
        c
    }
}
