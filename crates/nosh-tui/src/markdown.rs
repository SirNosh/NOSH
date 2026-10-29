//! Markdown rendering of assistant messages: the behaviour of OpenTUI's `MarkdownRenderable` as the
//! transcript configures it (`conceal: true`, coalesced blocks, `columns` tables without borders),
//! reproduced with pulldown-cmark source offsets.
//!
//! Specified by tui-dialogs-visual.md §6 (6.1 block segmentation, 6.2 source-preserving highlight
//! with conceal, 6.3 code/blockquote/rule/table blocks, 6.4 streaming), §10.4 items 3-5 (plain code
//! blocks, no hyperlinks, synchronous rendering) and the style table of tui-workspace.md §2.3
//! (`crate::theme::markdown_style`). Options: tables, strikethrough, task lists.
//!
//! Prose keeps its source text: each top-level run of prose blocks is parsed again on its own and
//! every event paints a style over, or conceals, a byte range of that source. Ranges are painted
//! outer before inner, so the innermost scope wins. Any bracketed prose is a shortcut link (the
//! broken-link callback accepts every reference), as tree-sitter's inline grammar has it.
//! Deviations, all where CommonMark and tree-sitter-markdown disagree or the native side is not
//! visible: text blocks drop their trailing blank lines and blocks are always one blank row apart
//! (the same picture as §6.1's 0-or-1 rule); `~~` delimiters are hidden; the link `]` replacement
//! does not swallow a following space; code blocks are word-wrapped plain text.

use std::ops::Range;

use pulldown_cmark::{
    BrokenLink, CodeBlockKind, CowStr, Event, LinkType, Options, Parser, Tag, TagEnd,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::layout::{TAB_WIDTH, wrap_ranges};
use crate::theme::markdown_style;

/// The pulldown-cmark options of §6.
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// A parser over `source` that resolves every broken reference, with source offsets.
fn parse(source: &str) -> impl Iterator<Item = (Event<'_>, Range<usize>)> {
    let accept = |_: BrokenLink| Some((CowStr::Borrowed(""), CowStr::Borrowed("")));
    Parser::new_with_broken_link_callback(source, options(), Some(accept)).into_offset_iter()
}

/// One top-level block of §6.1.
enum Block {
    /// A run of prose blocks, as its source.
    Text(Range<usize>),
    Code(String),
    Quote(Range<usize>),
    Rule,
    Table(Range<usize>),
}

/// The rows of one assistant message at `width` cells: prose shown as its raw source with styled and
/// concealed ranges, separate blocks laid out per §6.3, blank rows per §6.1. The `streaming` flag of
/// OpenTUI changes nothing visible and is not a parameter (§6.4).
pub fn render_markdown(source: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, block) in blocks(source).into_iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        match block {
            Block::Text(range) => {
                lines.extend(prose(
                    source[range].trim_end_matches(['\n', '\r', ' ']),
                    markdown_style("default"),
                    width,
                ));
            }
            Block::Code(code) => {
                let code = code.strip_suffix('\n').unwrap_or(&code);
                lines.extend(plain(code, markdown_style("default"), width));
            }
            Block::Quote(range) => {
                let inner: Vec<&str> = source[range]
                    .trim_end_matches(['\n', '\r'])
                    .lines()
                    .map(|line| {
                        let line = line.trim_start_matches(' ');
                        let line = line.strip_prefix('>').unwrap_or(line);
                        line.strip_prefix(' ').unwrap_or(line)
                    })
                    .collect();
                let bar = Span::styled("│ ", markdown_style("conceal"));
                for line in prose(
                    &inner.join("\n"),
                    markdown_style("markup.quote"),
                    width.saturating_sub(2),
                ) {
                    let mut spans = vec![bar.clone()];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
            }
            Block::Rule => {
                lines.push(Line::styled(
                    "─".repeat(usize::from(width)),
                    markdown_style("conceal"),
                ));
            }
            Block::Table(range) => lines.extend(table(&source[range], width)),
        }
    }
    lines
}

/// §6.1: code, tables, blockquotes and rules stand alone; every other run of top-level blocks is one
/// text block spanning the source up to the next separate block.
fn blocks(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut run: Option<usize> = None;
    let mut depth = 0usize;
    let mut code: Option<String> = None;
    let close_run = |blocks: &mut Vec<Block>, run: &mut Option<usize>, end: usize| {
        if let Some(start) = run.take() {
            blocks.push(Block::Text(start..end));
        }
    };
    for (event, range) in parse(source) {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    match tag {
                        Tag::CodeBlock(_) => {
                            close_run(&mut blocks, &mut run, range.start);
                            code = Some(String::new());
                        }
                        Tag::BlockQuote(_) => {
                            close_run(&mut blocks, &mut run, range.start);
                            blocks.push(Block::Quote(range.clone()));
                        }
                        Tag::Table(_) => {
                            close_run(&mut blocks, &mut run, range.start);
                            blocks.push(Block::Table(range.clone()));
                        }
                        _ => {
                            run.get_or_insert(range.start);
                        }
                    }
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth -= 1;
                if depth == 0 && tag == TagEnd::CodeBlock {
                    blocks.push(Block::Code(code.take().unwrap_or_default()));
                }
            }
            Event::Text(text) if depth == 1 && code.is_some() => {
                code.as_mut().expect("in a code block").push_str(&text);
            }
            Event::Rule if depth == 0 => {
                close_run(&mut blocks, &mut run, range.start);
                blocks.push(Block::Rule);
            }
            _ if depth == 0 => {
                run.get_or_insert(range.start);
            }
            _ => {}
        }
    }
    close_run(&mut blocks, &mut run, source.len());
    blocks
}

/// The displayed text of a prose source and the style of each of its bytes.
struct Styled {
    text: String,
    styles: Vec<Style>,
}

/// Paints and conceals over one prose source (§6.2).
struct Painter<'a> {
    source: &'a str,
    styles: Vec<Style>,
    /// Hidden ranges and their replacement text, disjoint.
    hidden: Vec<(Range<usize>, &'static str)>,
}

impl Painter<'_> {
    fn paint(&mut self, range: Range<usize>, scope: &str) {
        let style = markdown_style(scope);
        for byte in &mut self.styles[range] {
            *byte = byte.patch(style);
        }
    }

    fn hide(&mut self, range: Range<usize>, replacement: &'static str) {
        if !range.is_empty() || !replacement.is_empty() {
            self.hidden.push((range, replacement));
        }
    }

    /// Hides `count` delimiter bytes at both ends of `range`.
    fn delimiters(&mut self, range: &Range<usize>, count: usize) {
        if range.len() >= 2 * count {
            self.hide(range.start..range.start + count, "");
            self.hide(range.end - count..range.end, "");
        }
    }

    fn finish(mut self) -> Styled {
        self.hidden.sort_by_key(|(range, _)| range.start);
        let mut text = String::with_capacity(self.source.len());
        let mut styles = Vec::with_capacity(self.source.len());
        let mut position = 0;
        let mut hidden = self.hidden.iter().peekable();
        for (index, char) in self.source.char_indices() {
            if index < position {
                continue;
            }
            while hidden
                .next_if(|(range, _)| range.end <= index && range.start < index)
                .is_some()
            {}
            if let Some((range, replacement)) = hidden.next_if(|(range, _)| range.start == index) {
                text.push_str(replacement);
                styles.extend(std::iter::repeat_n(self.styles[index], replacement.len()));
                if !range.is_empty() {
                    position = range.end;
                    continue;
                }
            }
            text.push(char);
            styles.extend(std::iter::repeat_n(self.styles[index], char.len_utf8()));
            position = index + char.len_utf8();
        }
        Styled { text, styles }
    }
}

/// An open link or image while its children are read.
struct Open {
    range: Range<usize>,
    kind: LinkType,
    destination: String,
    image: bool,
    children: Option<Range<usize>>,
}

/// §6.2 over one prose source, every byte starting from `base`.
fn style_prose(source: &str, base: Style) -> Styled {
    let mut painter = Painter {
        source,
        styles: vec![base; source.len()],
        hidden: Vec::new(),
    };
    let mut links: Vec<Open> = Vec::new();
    let mut items = 0usize;
    for (event, range) in parse(source) {
        if let Some(open) = links.last_mut()
            && !matches!(event, Event::End(TagEnd::Link | TagEnd::Image))
        {
            let children = open.children.get_or_insert(range.clone());
            children.start = children.start.min(range.start);
            children.end = children.end.max(range.end);
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let text = &source[range.clone()];
                let indent = text.len() - text.trim_start_matches(' ').len();
                let content_end = range.start + text.trim_end_matches(['\n', '\r']).len();
                if text[indent..].starts_with('#') {
                    let hashes = text[indent..]
                        .bytes()
                        .take_while(|&byte| byte == b'#')
                        .count();
                    let mut end = range.start + indent + hashes;
                    if source[end..].starts_with(' ') {
                        end += 1;
                    }
                    painter.hide(range.start + indent..end, "");
                    painter.paint(
                        end..content_end,
                        &format!("markup.heading.{}", level as usize),
                    );
                } else {
                    painter.paint(range.start..content_end, "markup.heading");
                }
            }
            Event::Start(Tag::Strong) => {
                painter.paint(range.clone(), "markup.strong");
                painter.delimiters(&range, 2);
            }
            Event::Start(Tag::Emphasis) => {
                painter.paint(range.clone(), "markup.italic");
                painter.delimiters(&range, 1);
            }
            Event::Start(Tag::Strikethrough) => {
                let count = source[range.clone()]
                    .bytes()
                    .take_while(|&byte| byte == b'~')
                    .count();
                painter.delimiters(&range, count.max(1));
            }
            Event::Code(_) => {
                let count = source[range.clone()]
                    .bytes()
                    .take_while(|&byte| byte == b'`')
                    .count();
                painter.paint(range.clone(), "markup.raw");
                painter.delimiters(&range, count);
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => links.push(Open {
                range,
                kind: link_type,
                destination: dest_url.into_string(),
                image: false,
                children: None,
            }),
            Event::Start(Tag::Image { dest_url, .. }) => links.push(Open {
                range,
                kind: LinkType::Inline,
                destination: dest_url.into_string(),
                image: true,
                children: None,
            }),
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some(open) = links.pop() {
                    link(&mut painter, open);
                }
            }
            Event::Start(Tag::Item) => {
                let marker = source[range.start..]
                    .find([' ', '\n'])
                    .unwrap_or(range.len());
                painter.paint(range.start..range.start + marker, "markup.list");
                items += 1;
            }
            Event::End(TagEnd::Item) => items -= 1,
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) if items > 0 => {
                let text = &source[range.clone()];
                let open = text.find('\n').map_or(text.len(), |index| index + 1);
                painter.hide(range.start..range.start + open, "");
                let body = text.trim_end_matches(['\n', '\r']);
                if let Some(close) = body.rfind('\n')
                    && body[close + 1..].trim_start().starts_with(['`', '~'])
                    && range.start + close + 1 > range.start + open
                {
                    painter.hide(range.start + close..range.start + body.len(), "");
                }
            }
            Event::Text(text) => {
                let raw = &source[range.clone()];
                if raw.starts_with('&') && raw.ends_with(';') && raw != &*text {
                    let replacement = match raw {
                        "&nbsp;" => "",
                        "&ensp;" | "&emsp;" => " ",
                        "&lt;" => "<",
                        "&gt;" => ">",
                        "&amp;" => "&",
                        "&quot;" => "\"",
                        _ => continue,
                    };
                    painter.hide(range, replacement);
                } else if range.start > 0
                    && source.as_bytes()[range.start - 1] == b'\\'
                    && raw.len() == 1
                    && raw.as_bytes()[0].is_ascii_punctuation()
                {
                    painter.paint(range.start - 1..range.end, "string");
                }
            }
            _ => {}
        }
    }
    painter.finish()
}

/// The conceal and colors of one link or image (§6.2).
fn link(painter: &mut Painter, open: Open) {
    let range = open.range;
    let label = open.children.unwrap_or(range.start + 1..range.start + 1);
    let label = label.start.max(range.start)..label.end.min(range.end);
    if open.image {
        painter.hide(range.start..label.start, "");
        painter.hide(label.end..range.end, "");
        painter.paint(label, "markup.link.label");
        return;
    }
    match open.kind {
        LinkType::Autolink | LinkType::Email => {
            painter.paint(range.clone(), "markup.link.url");
            painter.delimiters(&range, 1);
        }
        LinkType::Inline => {
            painter.hide(range.start..label.start, "");
            if painter.source[label.clone()] == open.destination {
                painter.hide(label.end..range.end, "");
            } else if label.end < range.end {
                painter.hide(label.end..label.end + 1, " ");
                painter.paint(label.end + 1..range.end, "markup.link.url");
            }
            painter.paint(label, "markup.link.label");
        }
        _ => {
            painter.hide(range.start..label.start, "");
            painter.hide(label.end..range.end, "");
            painter.paint(label, "markup.link.label");
        }
    }
}

/// Word-wraps styled text to `width`, one span per run of equal style.
fn wrap_styled(styled: &Styled, width: u16) -> Vec<Line<'static>> {
    let tab = " ".repeat(TAB_WIDTH);
    let mut lines = Vec::new();
    let mut base = 0;
    for hard in styled.text.split('\n') {
        for row in wrap_ranges(hard, width) {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut start = row.start;
            for (index, _) in hard[row.clone()].char_indices() {
                let at = row.start + index;
                if at > start && styled.styles[base + at] != styled.styles[base + start] {
                    spans.push(Span::styled(
                        hard[start..at].replace('\t', &tab),
                        styled.styles[base + start],
                    ));
                    start = at;
                }
            }
            if start < row.end {
                spans.push(Span::styled(
                    hard[start..row.end].replace('\t', &tab),
                    styled.styles[base + start],
                ));
            }
            lines.push(Line::from(spans));
        }
        base += hard.len() + 1;
    }
    lines
}

/// A prose (markdown-text) block: its source, styled and concealed, word-wrapped.
fn prose(source: &str, base: Style, width: u16) -> Vec<Line<'static>> {
    wrap_styled(&style_prose(source, base), width)
}

/// Unstyled text, word-wrapped.
fn plain(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let styled = Styled {
        text: text.to_owned(),
        styles: vec![style; text.len()],
    };
    wrap_styled(&styled, width)
}

/// A `columns` table without borders (§6.3): content-fitted columns, proportionally shrunk to the
/// width, two cells apart, cells word-wrapped; header cells in the heading style.
fn table(source: &str, width: u16) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Styled>> = Vec::new();
    let mut head = false;
    for (event, range) in parse(source) {
        match event {
            Event::Start(Tag::TableHead) => {
                head = true;
                rows.push(Vec::new());
            }
            Event::End(TagEnd::TableHead) => head = false,
            Event::Start(Tag::TableRow) => rows.push(Vec::new()),
            Event::Start(Tag::TableCell) => {
                let cell = source[range].trim();
                let cell = cell
                    .strip_prefix('|')
                    .unwrap_or(cell)
                    .strip_suffix('|')
                    .unwrap_or(cell)
                    .trim();
                let mut styled = style_prose(
                    if cell.is_empty() { " " } else { cell },
                    markdown_style("default"),
                );
                if head {
                    let heading = markdown_style("markup.heading");
                    styled
                        .styles
                        .iter_mut()
                        .for_each(|style| *style = style.patch(heading));
                }
                if let Some(row) = rows.last_mut() {
                    row.push(styled);
                }
            }
            _ => {}
        }
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 || rows.len() < 2 {
        return prose(source.trim_end(), markdown_style("default"), width);
    }
    let natural: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.text.width())
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    let room = usize::from(width)
        .saturating_sub(2 * (columns - 1))
        .max(columns);
    let total: usize = natural.iter().sum();
    let widths: Vec<usize> = if total <= room {
        natural
    } else {
        natural
            .iter()
            .map(|&cells| (cells * room / total).max(1))
            .collect()
    };
    let mut lines = Vec::new();
    for row in &rows {
        let cells: Vec<Vec<Line<'static>>> = widths
            .iter()
            .enumerate()
            .map(|(column, &cells)| match row.get(column) {
                Some(cell) => wrap_styled(cell, cells as u16),
                None => vec![Line::default()],
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..height {
            let mut spans = Vec::new();
            for (column, cell) in cells.iter().enumerate() {
                let content = cell.get(line).cloned().unwrap_or_default();
                let used = content.width();
                spans.extend(content.spans);
                if column + 1 < columns {
                    spans.push(Span::styled(
                        " ".repeat(widths[column].saturating_sub(used) + 2),
                        markdown_style("default"),
                    ));
                }
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn style_of(lines: &[Line], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains(needle))
            .map(|span| span.style)
            .expect(needle)
    }

    /// The conceal rules of §6.2 on one prose block, and the separate blocks of §6.3.
    #[test]
    fn prose_keeps_source_and_conceals_syntax() {
        let source = "# Title\nSome **bold** and *it* `code`, [label](https://x) and [shortcut].\n- item &lt;\n\n```\nlet x;\n```\n> quoted\n\n---\n\n| a | b |\n|---|---|\n| 1 | **2** |";
        let lines = render_markdown(source, 80);
        assert_eq!(
            text(&lines),
            [
                "Title",
                "Some bold and it code, label (https://x) and shortcut.",
                "- item <",
                "",
                "let x;",
                "",
                "│ quoted",
                "",
                "─".repeat(80).as_str(),
                "",
                "a  b",
                "1  2",
            ]
        );
        let heading = markdown_style("markup.heading.1");
        assert_eq!(style_of(&lines, "Title"), heading);
        assert_eq!(style_of(&lines, "bold"), markdown_style("markup.strong"));
        assert!(
            style_of(&lines, "label")
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(style_of(&lines, "-").fg, markdown_style("markup.list").fg);
        assert_eq!(
            style_of(&lines, "quoted").fg,
            markdown_style("markup.quote").fg
        );
    }
}
