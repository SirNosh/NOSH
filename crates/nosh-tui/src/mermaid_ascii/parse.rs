//! Flowchart parsing: `parser.ts` and `multiline-utils.ts` of beautiful-mermaid, as reached by
//! generated sources (tui-viewmodel.md A.2: header, `parseEdgeLine`, `consumeNode`, the first-`]`
//! rule and `normalizeBrTags`).

/// A parsed flowchart: nodes in first-registration order (their position is their index) and edges
/// as `(from, to)` node indices in emission order.
pub(super) struct Flowchart {
    pub(super) nodes: Vec<Node>,
    pub(super) edges: Vec<(usize, usize)>,
}

/// One registered node: the first definition of an id wins.
pub(super) struct Node {
    pub(super) id: String,
    /// The label after `normalizeBrTags`; `\n` separates box lines.
    pub(super) label: String,
}

/// Parses a `flowchart TD` source; `None` when the header does not match (A.2).
pub(super) fn parse(source: &str) -> Option<Flowchart> {
    todo!()
}
