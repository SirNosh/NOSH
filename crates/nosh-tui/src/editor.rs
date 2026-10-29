//! The multi-line composer and the single-line inputs: a port of `apps/tui/src/composer.ts`
//! (`ComposerRenderable` over OpenTUI's textarea) and of OpenTUI's `InputRenderable` as the dialogs
//! use it.
//!
//! Specified by tui-workspace.md §4.1-§4.3 (composer: limit, bindings with CA-2's correction,
//! word-wrapped geometry, scrolling, cursor, placeholder, paste) and §4.4 = tui-dialogs-visual.md
//! §4.2 (single-line input); DECISIONS.md item 4 (Keypad Enter inserts a newline when the terminal
//! reports `KeyEventState::KEYPAD`, otherwise Enter submits). INDEX.md §2 item 9: there is no
//! separate `input.rs`. Keys arrive as crossterm events; the workspace and dialogs consume their own
//! shortcuts first.
//!
//! Both widgets share one editing core ([`Text`]): the buffer, a byte-offset cursor on a grapheme
//! boundary, the selection anchor and an undo history of whole-buffer snapshots, one per edit.
//! Deviations: Ctrl+Alt with a printable character inserts it, because Windows reports AltGr that
//! way; Up and Down keep no goal column; the undo history keeps the last 256 edits.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::js;
use crate::layout::{TAB_WIDTH, char_cells, wrap_ranges};
use crate::theme::THEME;

/// `ComposerRenderable.maxLength`, in UTF-16 units.
pub const COMPOSER_MAX_LENGTH: usize = 16_000;
/// The dialog search box limit.
pub const SEARCH_MAX_LENGTH: usize = 200;
/// The project-form path limit.
pub const PATH_MAX_LENGTH: usize = 4096;
/// The project-form title limit.
pub const TITLE_MAX_LENGTH: usize = 240;

/// OpenTUI's default input placeholder color; the dialogs do not override it.
const INPUT_PLACEHOLDER: Color = Color::Rgb(0x66, 0x66, 0x66);
const UNDO_LIMIT: usize = 256;

/// What a key or paste did to an editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    /// The submit binding (Enter, Alt+Keypad Enter); the text is unchanged.
    Submit,
    /// The text changed (the composer's content-change handler runs). For [`Input`] this is the
    /// `input` event, which OpenTUI also emits after a deletion that changed nothing.
    Changed,
    /// Consumed without changing the text (cursor movement, selection).
    Handled,
    /// Not bound: the key is neither inserted nor consumed (Tab, unbound Ctrl/Alt combinations).
    Ignored,
}

/// What the shared bindings did; each widget maps it to an [`EditOutcome`].
enum Applied {
    /// An editing action ran; `true` when the buffer changed.
    Edited(bool),
    Moved,
    Newline,
    Ignored,
}

/// The editing core of both widgets.
#[derive(Debug)]
struct Text {
    value: String,
    cursor: usize,
    anchor: Option<usize>,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    max_length: usize,
}

impl Text {
    fn new(max_length: usize) -> Self {
        Text {
            value: String::new(),
            cursor: 0,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            max_length,
        }
    }

    /// Replaces the buffer programmatically: no history, cursor at the end.
    fn set(&mut self, value: String) {
        self.cursor = value.len();
        self.value = value;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
    }

    fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor.filter(|&anchor| anchor != self.cursor)?;
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    fn move_to(&mut self, target: usize, select: bool) -> Applied {
        if select {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = target;
        Applied::Moved
    }

    fn replace(&mut self, range: Range<usize>, text: &str) -> bool {
        if range.is_empty() && text.is_empty() {
            self.anchor = None;
            return false;
        }
        if self.undo.len() == UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push((self.value.clone(), self.cursor));
        self.redo.clear();
        self.value.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        self.anchor = None;
        true
    }

    /// `insertText`: the room is measured before a selection is replaced (composer §4.1).
    fn insert(&mut self, text: &str) -> bool {
        let room = self.max_length.saturating_sub(js::length(&self.value));
        let text = js::slice_to(text, room);
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.replace(range, &text)
    }

    /// Deletes the selection, or `from..to` (in either order) when there is none.
    fn delete(&mut self, target: usize) -> bool {
        let range = self
            .selection()
            .unwrap_or(target.min(self.cursor)..target.max(self.cursor));
        self.replace(range, "")
    }

    fn restore(&mut self, redo: bool) -> bool {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some((value, cursor)) = from.pop() else {
            return false;
        };
        to.push((std::mem::replace(&mut self.value, value), self.cursor));
        self.cursor = cursor;
        self.anchor = None;
        true
    }

    fn previous(&self, position: usize) -> usize {
        self.value[..position]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next(&self, position: usize) -> usize {
        self.value[position..]
            .graphemes(true)
            .next()
            .map_or(position, |grapheme| position + grapheme.len())
    }

    fn word_back(&self, position: usize) -> usize {
        let before = &self.value[..position];
        let mut chars = before.char_indices().rev().peekable();
        let mut start = position;
        while let Some(&(index, char)) = chars.peek() {
            if is_word(char) {
                break;
            }
            start = index;
            chars.next();
        }
        while let Some(&(index, char)) = chars.peek() {
            if !is_word(char) {
                break;
            }
            start = index;
            chars.next();
        }
        start
    }

    fn word_forward(&self, position: usize) -> usize {
        let after = &self.value[position..];
        let skipped = after.find(is_word).unwrap_or(after.len());
        let word = after[skipped..]
            .find(|char| !is_word(char))
            .unwrap_or(after.len() - skipped);
        position + skipped + word
    }

    fn line_start(&self, position: usize) -> usize {
        self.value[..position]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self, position: usize) -> usize {
        self.value[position..]
            .find('\n')
            .map_or(self.value.len(), |index| position + index)
    }

    /// The bindings of composer §4.2 that both widgets share; `rows` are the visual rows as byte
    /// ranges of the buffer (one row for an input).
    fn apply(&mut self, key: &KeyEvent, rows: &[Range<usize>]) -> Applied {
        let modifiers = key.modifiers;
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        let control = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);
        let command = modifiers.contains(KeyModifiers::SUPER);
        let cursor = self.cursor;
        let (row, column) = locate(&self.value, rows, cursor);
        let at_column = |row: usize, column: usize| offset_at(&self.value, &rows[row], column);
        match key.code {
            KeyCode::Char(char) if !command && (!(control || alt) || (control && alt)) => {
                if char < ' ' || char == '\u{7f}' {
                    return Applied::Ignored;
                }
                Applied::Edited(self.insert(char.encode_utf8(&mut [0; 4])))
            }
            KeyCode::Char(char) => {
                let char = char.to_ascii_lowercase();
                match (char, control, alt, command) {
                    ('a', true, false, false) => {
                        let start = self.line_start(cursor);
                        let target = if !shift && start == cursor && cursor > 0 {
                            cursor - 1
                        } else {
                            start
                        };
                        self.move_to(target, shift)
                    }
                    ('e', true, false, false) => {
                        let end = self.line_end(cursor);
                        let target = if !shift && end == cursor && cursor < self.value.len() {
                            cursor + 1
                        } else {
                            end
                        };
                        self.move_to(target, shift)
                    }
                    ('a', false, true, false) => self.move_to(rows[row].start, shift),
                    ('e', false, true, false) => self.move_to(rows[row].end, shift),
                    ('f', true, false, false) => self.move_to(self.next(cursor), false),
                    ('b', true, false, false) => self.move_to(self.previous(cursor), false),
                    ('f', false, true, false) => self.move_to(self.word_forward(cursor), shift),
                    ('b', false, true, false) => self.move_to(self.word_back(cursor), shift),
                    ('w', true, false, false) => {
                        Applied::Edited(self.delete(self.word_back(cursor)))
                    }
                    ('d', false, true, false) => {
                        Applied::Edited(self.delete(self.word_forward(cursor)))
                    }
                    ('d', true, false, false) if shift => {
                        let start = self.line_start(cursor);
                        let end = self.line_end(cursor);
                        let range = if end < self.value.len() {
                            start..end + 1
                        } else {
                            start.saturating_sub(1)..end
                        };
                        Applied::Edited(self.replace(range, ""))
                    }
                    ('d', true, false, false) => Applied::Edited(self.delete(self.next(cursor))),
                    ('k', true, false, false) => {
                        Applied::Edited(self.delete(self.line_end(cursor)))
                    }
                    ('u', true, false, false) => {
                        let start = self.line_start(cursor);
                        let target = if start == cursor {
                            cursor.saturating_sub(1)
                        } else {
                            start
                        };
                        Applied::Edited(self.delete(target))
                    }
                    ('j', true, false, false) => Applied::Newline,
                    ('-', true, false, false) => Applied::Edited(self.restore(false)),
                    ('.', true, false, false) => Applied::Edited(self.restore(true)),
                    ('z', false, false, true) => Applied::Edited(self.restore(shift)),
                    ('a', false, false, true) => {
                        self.anchor = Some(0);
                        self.cursor = self.value.len();
                        Applied::Moved
                    }
                    _ => Applied::Ignored,
                }
            }
            KeyCode::Left if command => self.move_to(rows[row].start, shift),
            KeyCode::Right if command => self.move_to(rows[row].end, shift),
            KeyCode::Up if command => self.move_to(0, shift),
            KeyCode::Down if command => self.move_to(self.value.len(), shift),
            KeyCode::Left if control || alt => self.move_to(self.word_back(cursor), shift),
            KeyCode::Right if control || alt => self.move_to(self.word_forward(cursor), shift),
            KeyCode::Left => match self.selection() {
                Some(range) if !shift => self.move_to(range.start, false),
                _ => self.move_to(self.previous(cursor), shift),
            },
            KeyCode::Right => match self.selection() {
                Some(range) if !shift => self.move_to(range.end, false),
                _ => self.move_to(self.next(cursor), shift),
            },
            KeyCode::Up if row > 0 => self.move_to(at_column(row - 1, column), shift),
            KeyCode::Down if row + 1 < rows.len() => {
                self.move_to(at_column(row + 1, column), shift)
            }
            KeyCode::Up | KeyCode::Down => Applied::Moved,
            KeyCode::Home => self.move_to(0, shift),
            KeyCode::End => self.move_to(self.value.len(), shift),
            KeyCode::Backspace if control || alt => {
                Applied::Edited(self.delete(self.word_back(cursor)))
            }
            KeyCode::Delete if control || alt => {
                Applied::Edited(self.delete(self.word_forward(cursor)))
            }
            KeyCode::Backspace => Applied::Edited(self.delete(self.previous(cursor))),
            KeyCode::Delete => Applied::Edited(self.delete(self.next(cursor))),
            _ => Applied::Ignored,
        }
    }
}

/// The composer outcome of an editing action.
fn outcome(changed: bool) -> EditOutcome {
    if changed {
        EditOutcome::Changed
    } else {
        EditOutcome::Handled
    }
}

/// A word character for word movement and deletion.
fn is_word(char: char) -> bool {
    char.is_alphanumeric() || char == '_'
}

/// The visual row holding `cursor` (the last row starting at or before it) and the cursor's column.
fn locate(value: &str, rows: &[Range<usize>], cursor: usize) -> (usize, usize) {
    let row = rows
        .iter()
        .rposition(|row| row.start <= cursor)
        .unwrap_or(0);
    let end = cursor.min(rows[row].end).max(rows[row].start);
    (
        row,
        value[rows[row].start..end].chars().map(char_cells).sum(),
    )
}

/// The offset in `row` of the last grapheme boundary at or before `column`.
fn offset_at(value: &str, row: &Range<usize>, column: usize) -> usize {
    let mut used = 0;
    for (index, grapheme) in value[row.clone()].grapheme_indices(true) {
        let cells: usize = grapheme.chars().map(char_cells).sum();
        if used + cells > column {
            return row.start + index;
        }
        used += cells;
    }
    row.end
}

/// The pasted text without ANSI sequences, CR LF and lone CR as LF.
fn clean_paste(text: &str) -> String {
    js::strip_vt_controls(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Draws `value[range]` at `(x, y)`, tabs expanded, the part inside `selection` reversed; returns
/// the next column.
fn draw_run(
    buf: &mut Buffer,
    (x, y): (u16, u16),
    limit: u16,
    value: &str,
    range: Range<usize>,
    selection: Option<&Range<usize>>,
    style: Style,
) {
    let tab = " ".repeat(TAB_WIDTH);
    let mut cuts = vec![range.start, range.end];
    if let Some(selection) = selection {
        cuts.extend([selection.start, selection.end].map(|cut| cut.clamp(range.start, range.end)));
    }
    cuts.sort_unstable();
    let mut column = x;
    for part in cuts.windows(2) {
        let (start, end) = (part[0], part[1]);
        if start == end || column >= limit {
            continue;
        }
        let selected =
            selection.is_some_and(|selection| selection.start <= start && end <= selection.end);
        let style = if selected {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        };
        let text = value[start..end].replace('\t', &tab);
        let (next, _) = buf.set_stringn(column, y, &text, usize::from(limit - column), style);
        column = next;
    }
}

/// `ComposerRenderable`: the word-wrapped multi-line editor with OpenTUI textarea semantics
/// (workspace §4.1-§4.3). Private fields are Track C's choice.
#[derive(Debug)]
pub struct Editor {
    text: Text,
    width: u16,
    max_height: u16,
    scroll: usize,
    /// Keep the cursor in view on the next draw; cleared by wheel scrolling.
    follow: bool,
}

impl Editor {
    /// An empty composer (`maxHeight` 6 until the workspace sets it).
    pub fn new() -> Self {
        Editor {
            text: Text::new(COMPOSER_MAX_LENGTH),
            width: 1,
            max_height: 6,
            scroll: 0,
            follow: true,
        }
    }

    /// `value`: the buffer, lines joined with `\n`.
    pub fn value(&self) -> String {
        self.text.value.clone()
    }

    /// `set value`: replaces the buffer with the first 16000 units and moves the cursor to the end.
    /// Programmatic: the caller does not treat it as a user edit.
    pub fn set_value(&mut self, value: &str) {
        self.text.set(js::slice_to(value, COMPOSER_MAX_LENGTH));
        self.follow = true;
    }

    /// `insertText`: inserts at the cursor (replacing a selection), limited to the room left before
    /// the selection is removed.
    pub fn insert_text(&mut self, text: &str) {
        self.text.insert(text);
        self.follow = true;
    }

    /// The visual rows of the buffer as byte ranges, at the current width.
    fn rows(&self) -> Vec<Range<usize>> {
        let mut rows = Vec::new();
        let mut base = 0;
        for line in self.text.value.split('\n') {
            let wrapped = wrap_ranges(line, self.width);
            rows.extend(
                wrapped
                    .into_iter()
                    .map(|row| base + row.start..base + row.end),
            );
            base += line.len() + 1;
        }
        rows
    }

    /// Applies one key with the effective composer bindings of workspace §4.2.
    pub fn handle_key(&mut self, key: &KeyEvent) -> EditOutcome {
        self.follow = true;
        if key.code == KeyCode::Enter {
            let keypad = key.state.contains(KeyEventState::KEYPAD);
            let modified = key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if keypad && alt || !keypad && !modified {
                return EditOutcome::Submit;
            }
            return outcome(self.text.insert("\n"));
        }
        let rows = self.rows();
        match self.text.apply(key, &rows) {
            Applied::Edited(changed) => outcome(changed),
            Applied::Moved => EditOutcome::Handled,
            Applied::Newline => outcome(self.text.insert("\n")),
            Applied::Ignored => EditOutcome::Ignored,
        }
    }

    /// Bracketed paste: strip ANSI sequences, normalize CR LF and lone CR to LF, then `insertText`.
    pub fn paste(&mut self, text: &str) -> EditOutcome {
        self.follow = true;
        let changed = self.text.insert(&clean_paste(text));
        outcome(changed)
    }

    /// The logical (`\n`-separated) row of the cursor; history recall tests it.
    pub fn logical_cursor_row(&self) -> usize {
        self.text.value[..self.text.cursor].matches('\n').count()
    }

    /// The number of logical lines.
    pub fn logical_line_count(&self) -> usize {
        self.text.value.matches('\n').count() + 1
    }

    /// Sets the wrap width (the composer content width of the last layout).
    pub fn set_width(&mut self, width: u16) {
        self.width = width.max(1);
    }

    /// Sets `maxHeight` (the workspace sets it on every render).
    pub fn set_max_height(&mut self, rows: u16) {
        self.max_height = rows.max(1);
    }

    /// The number of visual (wrapped) lines at the current width.
    pub fn visual_line_count(&self) -> usize {
        self.rows().len()
    }

    /// The editor's height: the visual line count clamped to `1..=maxHeight`.
    pub fn height(&self) -> u16 {
        let lines = u16::try_from(self.visual_line_count()).unwrap_or(u16::MAX);
        lines.clamp(1, self.max_height)
    }

    /// Scrolls the view by `rows` (mouse wheel, 1 row per notch) when the content is taller than
    /// the height.
    pub fn scroll(&mut self, rows: i32) {
        let room = self
            .visual_line_count()
            .saturating_sub(usize::from(self.height()));
        if room == 0 {
            return;
        }
        self.scroll = self.scroll.saturating_add_signed(rows as isize).min(room);
        self.follow = false;
    }

    /// Draws the visible rows (text `#eeeeee` on `#1e1e1e`, or the placeholder in `#909090` when
    /// empty) and returns the cursor cell, keeping the cursor visible with a 0.2 scroll margin.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, placeholder: &str) -> Position {
        let style = Style::new().fg(THEME.text).bg(THEME.element);
        buf.set_style(area, style);
        if area.is_empty() {
            return area.as_position();
        }
        if self.text.value.is_empty() {
            let placeholder = placeholder.replace('\n', " ");
            buf.set_stringn(
                area.x,
                area.y,
                placeholder,
                usize::from(area.width),
                style.fg(THEME.muted),
            );
            self.scroll = 0;
            return area.as_position();
        }
        let rows = self.rows();
        let height = usize::from(area.height);
        let (row, column) = locate(&self.text.value, &rows, self.text.cursor);
        if self.follow {
            let margin = height / 5;
            let top = (row + margin + 1).saturating_sub(height);
            let bottom = row.saturating_sub(margin);
            self.scroll = self.scroll.clamp(top, bottom.max(top));
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));
        let selection = self.text.selection();
        for (line, range) in rows.iter().skip(self.scroll).take(height).enumerate() {
            let y = area.y + line as u16;
            draw_run(
                buf,
                (area.x, y),
                area.right(),
                &self.text.value,
                range.clone(),
                selection.as_ref(),
                style,
            );
        }
        let line = row.clamp(self.scroll, self.scroll + height - 1) - self.scroll;
        let x = area
            .x
            .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
        Position::new(x.min(area.right()), area.y + line as u16)
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// OpenTUI's `InputRenderable`: one row, no wrapping, horizontal scrolling, newlines stripped
/// (workspace §4.4).
#[derive(Debug)]
pub struct Input {
    text: Text,
    /// The first visible column.
    offset: usize,
}

impl Input {
    /// An empty input limited to `max_length` UTF-16 units.
    pub fn new(max_length: usize) -> Self {
        Input {
            text: Text::new(max_length),
            offset: 0,
        }
    }

    /// The current text.
    pub fn value(&self) -> &str {
        &self.text.value
    }

    /// Programmatic set: strip newlines, cut to the limit, cursor to the end; returns whether the
    /// value changed (the `input` event fires only then).
    pub fn set_value(&mut self, value: &str) -> bool {
        let value = js::slice_to(&value.replace(['\r', '\n'], ""), self.text.max_length);
        let changed = value != self.text.value;
        self.text.set(value);
        changed
    }

    /// Applies one key with the single-line subset of workspace §4.2 (Enter and LF give `Submit`).
    pub fn handle_key(&mut self, key: &KeyEvent) -> EditOutcome {
        if key.code == KeyCode::Enter {
            return EditOutcome::Submit;
        }
        let row = 0..self.text.value.len();
        match self.text.apply(key, std::slice::from_ref(&row)) {
            Applied::Edited(_) => EditOutcome::Changed,
            Applied::Moved => EditOutcome::Handled,
            Applied::Newline => EditOutcome::Submit,
            Applied::Ignored => EditOutcome::Ignored,
        }
    }

    /// Paste: strip ANSI sequences and newlines, insert within the limit.
    pub fn paste(&mut self, text: &str) -> EditOutcome {
        self.text.insert(&clean_paste(text).replace('\n', ""));
        EditOutcome::Changed
    }

    /// Draws the row (text `#eeeeee`, placeholder `#666666`) on `background` and returns the cursor
    /// cell.
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        placeholder: &str,
        background: Color,
    ) -> Position {
        let style = Style::new().fg(THEME.text).bg(background);
        buf.set_style(area, style);
        if area.is_empty() {
            return area.as_position();
        }
        let width = usize::from(area.width);
        if self.text.value.is_empty() {
            self.offset = 0;
            buf.set_stringn(
                area.x,
                area.y,
                placeholder,
                width,
                style.fg(INPUT_PLACEHOLDER),
            );
            return area.as_position();
        }
        let value = &self.text.value;
        let row = 0..value.len();
        let (_, column) = locate(value, std::slice::from_ref(&row), self.text.cursor);
        self.offset = self
            .offset
            .clamp((column + 1).saturating_sub(width), column);
        let start = offset_at(value, &row, self.offset);
        let selection = self.text.selection();
        draw_run(
            buf,
            (area.x, area.y),
            area.right(),
            value,
            start..value.len(),
            selection.as_ref(),
            style,
        );
        Position::new(area.x + (column - self.offset) as u16, area.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for char in text.chars() {
            editor.handle_key(&key(KeyCode::Char(char), KeyModifiers::NONE));
        }
    }

    /// Enter submits; Shift/Alt+Enter, Keypad Enter and Ctrl+J insert newlines; Alt+Keypad Enter
    /// submits (composer §4.2, DECISIONS.md item 4).
    #[test]
    fn enter_bindings() {
        let mut editor = Editor::new();
        typed(&mut editor, "a");
        assert_eq!(
            editor.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
            EditOutcome::Submit
        );
        assert_eq!(
            editor.handle_key(&key(KeyCode::Enter, KeyModifiers::SHIFT)),
            EditOutcome::Changed
        );
        assert_eq!(
            editor.handle_key(&key(KeyCode::Enter, KeyModifiers::ALT)),
            EditOutcome::Changed
        );
        let mut keypad = key(KeyCode::Enter, KeyModifiers::NONE);
        keypad.state = KeyEventState::KEYPAD;
        assert_eq!(editor.handle_key(&keypad), EditOutcome::Changed);
        keypad.modifiers = KeyModifiers::ALT;
        assert_eq!(editor.handle_key(&keypad), EditOutcome::Submit);
        assert_eq!(
            editor.handle_key(&key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            EditOutcome::Changed
        );
        assert_eq!(editor.value(), "a\n\n\n\n");
        assert_eq!(
            (editor.logical_cursor_row(), editor.logical_line_count()),
            (4, 5)
        );
        assert_eq!(
            editor.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE)),
            EditOutcome::Ignored
        );
    }

    /// Word wrapping, visual Up/Down, the height clamp and the 16000-unit limit (composer §4.1, §4.3).
    #[test]
    fn wrapping_and_limits() {
        let mut editor = Editor::new();
        editor.set_width(10);
        editor.set_max_height(3);
        typed(&mut editor, "hello world again and more");
        assert_eq!(editor.visual_line_count(), 4);
        assert_eq!(editor.height(), 3);
        editor.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
        editor.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
        editor.handle_key(&key(KeyCode::Char('X'), KeyModifiers::SHIFT));
        assert_eq!(editor.value(), "hello worlXd again and more");
        assert_eq!(editor.logical_cursor_row(), 0);

        editor.set_value(&"x".repeat(20_000));
        assert_eq!(editor.value().len(), COMPOSER_MAX_LENGTH);
        assert_eq!(editor.paste("more"), EditOutcome::Handled);
        editor.set_value("");
        assert_eq!(editor.paste("a\r\nb\rc\u{1b}[31m"), EditOutcome::Changed);
        assert_eq!(editor.value(), "a\nb\nc");
    }

    /// Deletion, word movement, selection replacement and undo/redo.
    #[test]
    fn editing_keys() {
        let mut editor = Editor::new();
        editor.set_width(80);
        typed(&mut editor, "one two three");
        editor.handle_key(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(editor.value(), "one two ");
        editor.handle_key(&key(KeyCode::Left, KeyModifiers::ALT));
        editor.handle_key(&key(
            KeyCode::Right,
            KeyModifiers::SHIFT | KeyModifiers::ALT,
        ));
        typed(&mut editor, "2");
        assert_eq!(editor.value(), "one 2 ");
        editor.handle_key(&key(KeyCode::Char('-'), KeyModifiers::CONTROL));
        assert_eq!(editor.value(), "one two ");
        editor.handle_key(&key(KeyCode::Char('.'), KeyModifiers::CONTROL));
        assert_eq!(editor.value(), "one 2 ");
        editor.handle_key(&key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.value(), " ");
    }

    /// The input strips newlines, reports every edit, and scrolls to keep the cursor visible.
    #[test]
    fn input_behaviour() {
        let mut input = Input::new(5);
        assert!(input.set_value("a\nbcdefg"));
        assert_eq!(input.value(), "abcde");
        assert!(!input.set_value("abcde"));
        assert_eq!(
            input.handle_key(&key(KeyCode::Char('z'), KeyModifiers::NONE)),
            EditOutcome::Changed
        );
        assert_eq!(input.value(), "abcde");
        assert_eq!(
            input.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
            EditOutcome::Submit
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        let cursor = input.render(Rect::new(0, 0, 3, 1), &mut buf, "", THEME.panel);
        assert_eq!(cursor, Position::new(2, 0));
        assert_eq!(
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>(),
            "de "
        );
    }
}
