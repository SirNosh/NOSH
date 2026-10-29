//! Flowchart parsing: `parser.ts` and `multiline-utils.ts` of beautiful-mermaid, as reached by
//! generated sources (tui-viewmodel.md A.2: header, `parseEdgeLine`, `consumeNode`, the first-`]`
//! rule and `normalizeBrTags`).
//!
//! `layoutGraph` emits only two line forms after the header: `nK["label"]` and `nA --> nB`. Every
//! label went through `cell`, so it holds no line terminator and no `>`; the only `-->` in a source
//! is a generated edge. Deliberate deviation (reported, not in the spec): text after the first `]`
//! of a node line is ignored. TS parses it as more Mermaid, so a title like `Compare [A] & B` adds a
//! tagless phantom box `B`, and `x [y] & n3` registers `n3` early as a second root whose top edge
//! no path can reach, which makes TS's A* search run forever. The `]` truncation itself (the label
//! ends at the first `]`, keeps its opening `"` and loses its status line) is reproduced.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::js::{js_split_ws, js_trim};

/// A parsed flowchart: node labels in first-registration order (the position is the node index)
/// and edges as `(from, to)` node indices in emission order.
pub(super) struct Flowchart {
    /// Labels after `normalizeBrTags`; `\n` separates box lines.
    pub(super) labels: Vec<String>,
    pub(super) edges: Vec<(usize, usize)>,
}

/// JavaScript's `\s` (`is_js_whitespace`) as a regex class body.
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

// `normalizeBrTags` patterns. JS `i` without `u` folds ASCII letters only, hence `(?i-u:…)`; JS `.`
// stops at the four line terminators.
static BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("<(?i-u:br)[{JS_SPACE}]*/?>")).expect("valid br pattern"));
static HTML: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("</?(?i-u:sub|sup|small|mark)[{JS_SPACE}]*>")).expect("valid tag pattern")
});
static BOLD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\*\*([^\n\r\x{2028}\x{2029}]+?)\*\*").expect("valid bold pattern")
});
static STRIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"~~([^\n\r\x{2028}\x{2029}]+?)~~").expect("valid strike pattern"));

/// Parses a `flowchart TD` source; `None` unless the header is a TD (or TB) flowchart header.
pub(super) fn parse(source: &str) -> Option<Flowchart> {
    let mut lines = source
        .split('\n')
        .map(js_trim)
        .filter(|line| !line.is_empty() && !line.starts_with("%%"));
    let header = js_split_ws(lines.next()?);
    let [kind, direction] = header.as_slice() else {
        return None;
    };
    let flowchart = kind.eq_ignore_ascii_case("flowchart") || kind.eq_ignore_ascii_case("graph");
    if !flowchart || !(direction.eq_ignore_ascii_case("TD") || direction.eq_ignore_ascii_case("TB"))
    {
        return None;
    }
    let mut chart = Flowchart {
        labels: Vec::new(),
        edges: Vec::new(),
    };
    let mut ids = HashMap::new();
    for line in lines {
        let Some((id, rest)) = split_id(line) else {
            continue;
        };
        // `^([\w-]+)\[(.+?)\]`: the label runs to the first `]` after at least one character.
        if let Some(label) = rest.strip_prefix('[').and_then(|label| {
            let end = label.char_indices().skip(1).find(|&(_, c)| c == ']')?.0;
            Some(&label[..end])
        }) {
            register(&mut chart, &mut ids, id, normalize_br_tags(label));
            continue;
        }
        let from = register(&mut chart, &mut ids, id, id.to_owned());
        let Some((target, _)) = js_trim(rest)
            .strip_prefix("-->")
            .and_then(|rest| split_id(js_trim(rest)))
        else {
            continue;
        };
        let to = register(&mut chart, &mut ids, target, target.to_owned());
        chart.edges.push((from, to));
    }
    Some(chart)
}

/// `^([\w-]+)` with JS's ASCII `\w`: the id and the rest of the line.
fn split_id(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// `registerNode`: the first definition of an id wins; a bare reference registers its id as label.
fn register<'a>(
    chart: &mut Flowchart,
    ids: &mut HashMap<&'a str, usize>,
    id: &'a str,
    label: String,
) -> usize {
    *ids.entry(id).or_insert_with(|| {
        chart.labels.push(label);
        chart.labels.len() - 1
    })
}

/// `normalizeBrTags`: unquote, `<br>` and literal `\n` to line breaks, drop `sub`/`sup`/`small`/
/// `mark` tags, and markdown `**b**`, `*i*`, `~~s~~` to literal `<b>`, `<i>`, `<s>` text (A.2).
fn normalize_br_tags(label: &str) -> String {
    let unquoted = match label.strip_prefix('"') {
        Some(rest) if label.ends_with('"') => rest.strip_suffix('"').unwrap_or(rest),
        _ => label,
    };
    let text = BREAK.replace_all(unquoted, "\n").replace("\\n", "\n");
    let text = HTML.replace_all(&text, "");
    let text = BOLD.replace_all(&text, "<b>${1}</b>");
    STRIKE
        .replace_all(&italic(&text), "<s>${1}</s>")
        .into_owned()
}

/// `/(?<!\*)\*([^\s*](?:[^*]*[^\s*])?)\*(?!\*)/g` → `<i>$1</i>`, which needs look-around. The group
/// holds no `*`, so the closing star is the first one after the opening star; the look-behind reads
/// the input, not the replaced output (A.2).
fn italic(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let star = |index: usize| chars.get(index) == Some(&'*');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let opens = star(i)
            && (i == 0 || !star(i - 1))
            && chars
                .get(i + 1)
                .is_some_and(|&c| c != '*' && !crate::js::is_js_whitespace(c));
        if opens
            && let Some(close) = (i + 2..chars.len()).find(|&j| star(j))
            && !crate::js::is_js_whitespace(chars[close - 1])
            && !star(close + 1)
        {
            out.push_str("<i>");
            out.extend(&chars[i + 1..close]);
            out.push_str("</i>");
            i = close + 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}
