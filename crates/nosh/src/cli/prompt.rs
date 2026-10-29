//! The terminal side of the interactive prompts: Node `readline`'s `question` (cli.md §8.9) and the
//! raw key reading `secret` uses. History, yank, undo and Ctrl+L are left out: each question is a
//! fresh interface with no history in TypeScript too, and the rest is cosmetic.

use super::out;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nosh_agent::pi::AuthSignal;
use std::io::{IsTerminal, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

/// The terminal in raw mode until dropped.
pub(super) struct RawMode;

impl RawMode {
    pub(super) fn enable() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// The next key press, or `None` once `signal` is aborted (checked between 50 ms waits).
pub(super) fn next_key(signal: Option<&AuthSignal>) -> Result<Option<KeyEvent>, String> {
    let failed = |error: std::io::Error| error.to_string();
    loop {
        if signal.is_some_and(AuthSignal::aborted) {
            return Ok(None);
        }
        if !event::poll(Duration::from_millis(50)).map_err(failed)? {
            continue;
        }
        // Windows also reports key releases.
        if let Event::Key(key) = event::read().map_err(failed)?
            && key.kind == KeyEventKind::Press
        {
            return Ok(Some(key));
        }
    }
}

/// Drops input already waiting after the line break that ended an answer: the rest of a paste,
/// which Node discards with the rest of its chunk.
pub(super) fn discard_pending() {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
}

/// Lines of input, read on a thread of their own, one each time a question asks. A question that
/// stops waiting leaves its read pending, and the next question takes the line it gets: Node's
/// aborted interface pauses the shared stdin, and the next interface reads that line.
struct Lines {
    ask: Sender<()>,
    lines: Receiver<std::io::Result<String>>,
    pending: bool,
}

/// Standard input's lines.
static STDIN: LazyLock<Mutex<Lines>> =
    LazyLock::new(|| Mutex::new(Lines::new(|line| std::io::stdin().read_line(line))));

impl Lines {
    fn new(mut read: impl FnMut(&mut String) -> std::io::Result<usize> + Send + 'static) -> Self {
        let (ask, asked) = mpsc::channel();
        let (answer, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for () in asked {
                let mut line = String::new();
                if answer.send(read(&mut line).map(|_| line)).is_err() {
                    break;
                }
            }
        });
        Self {
            ask,
            lines,
            pending: false,
        }
    }

    /// The next line with its line break, empty at the end of input, or `None` once `signal` is
    /// aborted (checked between 50 ms waits).
    fn next(&mut self, signal: Option<&AuthSignal>) -> Result<Option<String>, String> {
        if !self.pending {
            let _ = self.ask.send(());
            self.pending = true;
        }
        loop {
            if signal.is_some_and(AuthSignal::aborted) {
                return Ok(None);
            }
            match self.lines.recv_timeout(Duration::from_millis(50)) {
                Ok(read) => {
                    self.pending = false;
                    return read.map(Some).map_err(|error| error.to_string());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    unreachable!("the reader thread lives as long as its `Lines`")
                }
            }
        }
    }
}

/// `rl.question(query, { signal })`: the answer untrimmed. On a terminal the line is echoed and
/// edited here; Ctrl+C, and Ctrl+D on an empty line, reject as Node's readline does. An aborted
/// `signal` ends the question with a newline (`ask`'s abort handler). Without a terminal on both
/// ends Node reads a plain line, and so does this.
pub(super) fn question(query: &str, signal: Option<&AuthSignal>) -> Result<String, String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        out(query);
        let line = STDIN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next(signal)?;
        let Some(line) = line else {
            out("\n");
            return Err("The operation was aborted".into());
        };
        if line.is_empty() {
            return Err("Aborted with Ctrl+D".into());
        }
        let line = line.strip_suffix('\n').unwrap_or(&line);
        return Ok(line.strip_suffix('\r').unwrap_or(line).to_owned());
    }
    let _raw = RawMode::enable()?;
    out(query);
    let mut line = Line::default();
    loop {
        let Some(key) = next_key(signal)? else {
            out("\n");
            return Err("The operation was aborted".into());
        };
        match line.edit(&key) {
            Edit::Submit => {
                out("\r\n");
                discard_pending();
                return Ok(line.chars.iter().collect());
            }
            Edit::Abort(message) => {
                // The error goes on the next line, where closing Node's readline leaves a Windows
                // console.
                out("\r\n");
                return Err(message.into());
            }
            Edit::Echo(text) => out(&text),
            Edit::Redraw => line.redraw(query),
            Edit::None => {}
        }
    }
}

/// What one key does to the line being edited.
#[derive(Debug, PartialEq)]
enum Edit {
    Submit,
    Abort(&'static str),
    /// Written as is: the line changed at its end.
    Echo(String),
    Redraw,
    None,
}

/// The line being edited and the cursor in it, in characters.
#[derive(Default)]
struct Line {
    chars: Vec<char>,
    cursor: usize,
}

impl Line {
    fn edit(&mut self, key: &KeyEvent) -> Edit {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Ctrl+J and Ctrl+M are LF and CR, which readline submits as it does Enter.
            KeyCode::Enter => Edit::Submit,
            KeyCode::Char('j' | 'm') if control && !alt => Edit::Submit,
            KeyCode::Char('c') if control && !alt => Edit::Abort("Aborted with Ctrl+C"),
            KeyCode::Char('d') if control && !alt && self.chars.is_empty() => {
                Edit::Abort("Aborted with Ctrl+D")
            }
            KeyCode::Char('d') if control && !alt => self.delete_right(),
            KeyCode::Backspace if control || alt => self.delete_word_left(),
            KeyCode::Char('w') if control && !alt => self.delete_word_left(),
            KeyCode::Backspace => self.delete_left(),
            KeyCode::Char('h') if control && !alt => self.delete_left(),
            KeyCode::Delete => self.delete_right(),
            KeyCode::Char('u') if control && !alt => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
                Edit::Redraw
            }
            KeyCode::Char('k') if control && !alt => {
                self.chars.truncate(self.cursor);
                Edit::Redraw
            }
            KeyCode::Left => self.move_to(self.cursor.saturating_sub(1)),
            KeyCode::Char('b') if control && !alt => self.move_to(self.cursor.saturating_sub(1)),
            KeyCode::Right => self.move_to(self.cursor + 1),
            KeyCode::Char('f') if control && !alt => self.move_to(self.cursor + 1),
            KeyCode::Home => self.move_to(0),
            KeyCode::Char('a') if control && !alt => self.move_to(0),
            KeyCode::End => self.move_to(self.chars.len()),
            KeyCode::Char('e') if control && !alt => self.move_to(self.chars.len()),
            KeyCode::Tab => self.insert('\t'),
            // AltGr arrives as Ctrl+Alt and types its character.
            KeyCode::Char(character) if control == alt => self.insert(character),
            _ => Edit::None,
        }
    }

    fn insert(&mut self, character: char) -> Edit {
        self.chars.insert(self.cursor, character);
        self.cursor += 1;
        if self.cursor == self.chars.len() {
            Edit::Echo(character.to_string())
        } else {
            Edit::Redraw
        }
    }

    fn delete_left(&mut self) -> Edit {
        if self.cursor == 0 {
            return Edit::None;
        }
        self.cursor -= 1;
        self.chars.remove(self.cursor);
        if self.cursor == self.chars.len() {
            Edit::Echo("\u{8} \u{8}".into())
        } else {
            Edit::Redraw
        }
    }

    fn delete_right(&mut self) -> Edit {
        if self.cursor == self.chars.len() {
            return Edit::None;
        }
        self.chars.remove(self.cursor);
        Edit::Redraw
    }

    /// Readline's `_deleteWordLeft`: trailing whitespace, then a run of word characters or of
    /// punctuation (`/^\s*(?:[^\w\s]+|\w+)?/` on the reversed text before the cursor).
    fn delete_word_left(&mut self) -> Edit {
        let class = |character: char| {
            if character.is_ascii_alphanumeric() || character == '_' {
                1
            } else if nosh_wire::js::is_js_whitespace(character) {
                0
            } else {
                2
            }
        };
        let before: Vec<u8> = self.chars[..self.cursor]
            .iter()
            .rev()
            .map(|character| class(*character))
            .collect();
        let spaces = before.iter().take_while(|class| **class == 0).count();
        let word = before.get(spaces).map_or(0, |first| {
            before[spaces..]
                .iter()
                .take_while(|class| *class == first)
                .count()
        });
        let start = self.cursor - spaces - word;
        if start == self.cursor {
            return Edit::None;
        }
        self.chars.drain(start..self.cursor);
        self.cursor = start;
        Edit::Redraw
    }

    fn move_to(&mut self, cursor: usize) -> Edit {
        self.cursor = cursor.min(self.chars.len());
        Edit::Redraw
    }

    /// Rewrites the prompt line and puts the cursor back in place.
    fn redraw(&self, query: &str) {
        let line: String = self.chars.iter().collect();
        let before: String = self.chars[..self.cursor].iter().collect();
        let mut stdout = std::io::stdout().lock();
        let _ = crossterm::queue!(
            stdout,
            crossterm::style::Print(format!("\r{query}{line}")),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::UntilNewLine),
            crossterm::style::Print(format!("\r{query}{before}"))
        );
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn typed(line: &mut Line, text: &str) {
        for character in text.chars() {
            line.edit(&key(KeyCode::Char(character), KeyModifiers::NONE));
        }
    }

    #[test]
    fn edits_like_node_readline() {
        let ctrl = |character| key(KeyCode::Char(character), KeyModifiers::CONTROL);
        let mut line = Line::default();
        assert_eq!(line.edit(&ctrl('d')), Edit::Abort("Aborted with Ctrl+D"));
        typed(&mut line, "abc def");
        assert_eq!(line.edit(&ctrl('w')), Edit::Redraw);
        assert_eq!(line.chars.iter().collect::<String>(), "abc ");
        line.edit(&ctrl('a'));
        assert_eq!(
            line.edit(&ctrl('d')),
            Edit::Redraw,
            "Ctrl+D deletes right on a non-empty line"
        );
        typed(&mut line, "x");
        assert_eq!(line.chars.iter().collect::<String>(), "xbc ");
        line.edit(&ctrl('k'));
        assert_eq!(line.chars.iter().collect::<String>(), "x");
        assert_eq!(
            line.edit(&key(KeyCode::Backspace, KeyModifiers::NONE)),
            Edit::Echo("\u{8} \u{8}".into())
        );
        assert_eq!(line.edit(&ctrl('c')), Edit::Abort("Aborted with Ctrl+C"));
        assert_eq!(
            line.edit(&key(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            Edit::Echo("@".into()),
            "AltGr types its character"
        );
        assert_eq!(
            line.edit(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Edit::None
        );
        assert_eq!(line.edit(&ctrl('j')), Edit::Submit, "LF submits");
    }

    #[test]
    fn an_abandoned_read_leaves_its_line_to_the_next_question() {
        use std::io::BufRead;
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut reader = std::io::BufReader::new(reader);
        let mut lines = Lines::new(move |line| reader.read_line(line));
        writer.write_all(b"1\r\n").unwrap();
        assert_eq!(lines.next(None).unwrap().as_deref(), Some("1\r\n"));
        // Pi abandons a prompt while it waits for input; the user answers the next one.
        let signal = AuthSignal::default();
        let aborting = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            aborting.abort();
            std::thread::sleep(Duration::from_millis(100));
            writer.write_all(b"2\n").unwrap();
        });
        assert_eq!(lines.next(Some(&signal)).unwrap(), None);
        assert_eq!(lines.next(None).unwrap().as_deref(), Some("2\n"));
        assert_eq!(
            lines.next(None).unwrap().as_deref(),
            Some(""),
            "end of input"
        );
    }
}
