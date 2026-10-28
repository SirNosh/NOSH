//! `JSON.parse` as V8 (Node 22) reports it. The terminal-output repair reads the `position N` of
//! V8's syntax error, and rejection reasons reach models and receipts, so syntax errors here carry
//! V8's exact message text and UTF-16 positions.
//!
//! Known divergences: a lone surrogate inside an error message (an unexpected token or a context
//! window that splits a surrogate pair) becomes U+FFFD, and text that V8 accepts but serde_json
//! cannot hold (a lone `\uD800` escape, a number beyond f64 range) is reported as a syntax error.

use serde_json::Value;

/// A `SyntaxError` thrown by `JSON.parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
}

impl SyntaxError {
    /// `Number(/position (\d+)/.exec(message)?.[1])`: the first `position N` in the message, which
    /// is V8's UTF-16 offset (a context window quoting such text is read the same way).
    pub fn position(&self) -> Option<usize> {
        static POSITION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"position (\d+)").expect("pattern compiles")
        });
        POSITION.captures(&self.message)?[1].parse().ok()
    }
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SyntaxError {}

/// `JSON.parse(text)`, with JavaScript number semantics.
pub fn parse(text: &str) -> Result<Value, SyntaxError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    Parser {
        units: &units,
        cursor: 0,
    }
    .document()?;
    let mut value: Value = serde_json::from_str(text).map_err(|error| SyntaxError {
        message: error.to_string(),
    })?;
    nosh_wire::normalize_numbers(&mut value);
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    Eos,
    String,
    Number,
    Other,
}

/// V8's message templates that take a position (the text before ` at position N`).
#[derive(Clone, Copy)]
enum Expected {
    PropertyNameOrRightBrace,
    DoubleQuotedPropertyName,
    ColonAfterPropertyName,
    CommaOrRightBrace,
    CommaOrRightBracket,
    NonWhitespaceAfterJson,
    NoNumberAfterMinusSign,
    UnterminatedFractionalNumber,
    ExponentPartMissingNumber,
    UnterminatedString,
    BadControlCharacter,
    BadEscapedCharacter,
    BadUnicodeEscape,
}

impl Expected {
    fn text(self) -> &'static str {
        match self {
            Self::PropertyNameOrRightBrace => "Expected property name or '}' in JSON",
            Self::DoubleQuotedPropertyName => "Expected double-quoted property name in JSON",
            Self::ColonAfterPropertyName => "Expected ':' after property name in JSON",
            Self::CommaOrRightBrace => "Expected ',' or '}' after property value in JSON",
            Self::CommaOrRightBracket => "Expected ',' or ']' after array element in JSON",
            Self::NonWhitespaceAfterJson => "Unexpected non-whitespace character after JSON",
            Self::NoNumberAfterMinusSign => "No number after minus sign in JSON",
            Self::UnterminatedFractionalNumber => "Unterminated fractional number in JSON",
            Self::ExponentPartMissingNumber => "Exponent part is missing a number in JSON",
            Self::UnterminatedString => "Unterminated string in JSON",
            Self::BadControlCharacter => "Bad control character in string literal in JSON",
            Self::BadEscapedCharacter => "Bad escaped character in JSON",
            Self::BadUnicodeEscape => "Bad Unicode escape in JSON",
        }
    }
}

const MAX_CONTEXT_CHARACTERS: usize = 10;
const MIN_LENGTH_FOR_CONTEXT: usize = MAX_CONTEXT_CHARACTERS * 2 + 1;

struct Parser<'a> {
    units: &'a [u16],
    cursor: usize,
}

type Step = Result<(), SyntaxError>;

impl Parser<'_> {
    fn document(&mut self) -> Step {
        self.skip_whitespace();
        self.value()?;
        self.skip_whitespace();
        if self.cursor < self.units.len() {
            return self.fail_with(Expected::NonWhitespaceAfterJson);
        }
        Ok(())
    }

    /// V8 parses iteratively, so nesting depth is unbounded; this recursion only checks syntax and
    /// is bounded by the 128 KiB terminal-output limit.
    fn value(&mut self) -> Step {
        match self.peek() {
            Some(0x22) => self.string(),
            Some(0x2D | 0x30..=0x39) => self.number(),
            Some(0x74) => self.literal("true"),
            Some(0x66) => self.literal("false"),
            Some(0x6E) => self.literal("null"),
            Some(0x7B) => self.object(),
            Some(0x5B) => self.array(),
            _ => self.fail_token(),
        }
    }

    fn object(&mut self) -> Step {
        self.cursor += 1;
        self.skip_whitespace();
        if self.peek() == Some(0x7D) {
            self.cursor += 1;
            return Ok(());
        }
        let mut expected = Expected::PropertyNameOrRightBrace;
        loop {
            self.skip_whitespace();
            if self.peek() != Some(0x22) {
                return self.fail_with(expected);
            }
            self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(0x3A) {
                return self.fail_with(Expected::ColonAfterPropertyName);
            }
            self.cursor += 1;
            self.skip_whitespace();
            self.value()?;
            self.skip_whitespace();
            match self.peek() {
                Some(0x2C) => {
                    self.cursor += 1;
                    expected = Expected::DoubleQuotedPropertyName;
                }
                Some(0x7D) => {
                    self.cursor += 1;
                    return Ok(());
                }
                _ => return self.fail_with(Expected::CommaOrRightBrace),
            }
        }
    }

    fn array(&mut self) -> Step {
        self.cursor += 1;
        self.skip_whitespace();
        if self.peek() == Some(0x5D) {
            self.cursor += 1;
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            self.value()?;
            self.skip_whitespace();
            match self.peek() {
                Some(0x2C) => self.cursor += 1,
                Some(0x5D) => {
                    self.cursor += 1;
                    return Ok(());
                }
                _ => return self.fail_with(Expected::CommaOrRightBracket),
            }
        }
    }

    /// `ScanLiteral`: the first mismatching character is the unexpected token.
    fn literal(&mut self, word: &str) -> Step {
        for expected in word.encode_utf16() {
            if self.peek() != Some(expected) {
                return self.fail_token();
            }
            self.cursor += 1;
        }
        Ok(())
    }

    fn number(&mut self) -> Step {
        if self.peek() == Some(0x2D) {
            self.cursor += 1;
        }
        if self.peek() == Some(0x30) {
            self.cursor += 1;
            // A leading zero may only be followed by a fraction or an exponent.
            if self.peek().is_some_and(is_digit) {
                return self.fail(Token::Number, None);
            }
        } else {
            if !self.peek().is_some_and(is_digit) {
                return self.fail_with(Expected::NoNumberAfterMinusSign);
            }
            self.skip_digits();
        }
        if self.peek() == Some(0x2E) {
            self.cursor += 1;
            if !self.peek().is_some_and(is_digit) {
                return self.fail_with(Expected::UnterminatedFractionalNumber);
            }
            self.skip_digits();
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            self.cursor += 1;
            if matches!(self.peek(), Some(0x2B | 0x2D)) {
                self.cursor += 1;
            }
            if !self.peek().is_some_and(is_digit) {
                return self.fail_with(Expected::ExponentPartMissingNumber);
            }
            self.skip_digits();
        }
        Ok(())
    }

    fn string(&mut self) -> Step {
        self.cursor += 1;
        loop {
            match self.peek() {
                None => return self.fail_with(Expected::UnterminatedString),
                Some(0x22) => {
                    self.cursor += 1;
                    return Ok(());
                }
                Some(0x5C) => {
                    self.cursor += 1;
                    match self.peek() {
                        None => return self.fail(Token::Eos, None),
                        Some(0x22 | 0x5C | 0x2F | 0x62 | 0x66 | 0x6E | 0x72 | 0x74) => {
                            self.cursor += 1;
                        }
                        Some(0x75) => {
                            for _ in 0..4 {
                                self.cursor += 1;
                                if !self.peek().is_some_and(is_hex) {
                                    return self.fail_with(Expected::BadUnicodeEscape);
                                }
                            }
                            self.cursor += 1;
                        }
                        // `ReportUnexpectedCharacter`: beyond Latin-1, the generic token message.
                        Some(unit) if unit > 0xFF => return self.fail_token(),
                        Some(_) => return self.fail_with(Expected::BadEscapedCharacter),
                    }
                }
                Some(unit) if unit < 0x20 => return self.fail_with(Expected::BadControlCharacter),
                Some(_) => self.cursor += 1,
            }
        }
    }

    fn peek(&self) -> Option<u16> {
        self.units.get(self.cursor).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(0x20 | 0x09 | 0x0A | 0x0D)) {
            self.cursor += 1;
        }
    }

    fn skip_digits(&mut self) {
        while self.peek().is_some_and(is_digit) {
            self.cursor += 1;
        }
    }

    /// The unexpected token at the cursor, classified as V8's one-character token table does.
    fn fail_token(&self) -> Step {
        let token = match self.peek() {
            None => Token::Eos,
            Some(0x22) => Token::String,
            Some(0x2D | 0x30..=0x39) => Token::Number,
            Some(_) => Token::Other,
        };
        self.fail(token, None)
    }

    fn fail_with(&self, expected: Expected) -> Step {
        self.fail(Token::Other, Some(expected))
    }

    fn fail(&self, token: Token, expected: Option<Expected>) -> Step {
        let position = self.cursor;
        let located = |text: &str| SyntaxError {
            message: format!("{text} at position {position} {}", self.location()),
        };
        Err(match (expected, token) {
            (Some(expected), _) => located(expected.text()),
            (None, Token::Eos) => SyntaxError {
                message: "Unexpected end of JSON input".into(),
            },
            (None, Token::Number) => located("Unexpected number in JSON"),
            (None, Token::String) => located("Unexpected string in JSON"),
            (None, Token::Other) => SyntaxError {
                message: self.unexpected_token_message(position),
            },
        })
    }

    fn unexpected_token_message(&self, position: usize) -> String {
        let source = String::from_utf16_lossy(self.units);
        if ["NaN", "Infinity", "undefined", "[object Object]"].contains(&source.as_str()) {
            return format!("\"{source}\" is not valid JSON");
        }
        let token = String::from_utf16_lossy(&self.units[position..=position]);
        let length = self.units.len();
        if length < MIN_LENGTH_FOR_CONTEXT {
            return format!("Unexpected token '{token}', \"{source}\" is not valid JSON");
        }
        let slice = |start: usize, end: usize| {
            String::from_utf16_lossy(&self.units[start..end.min(length)])
        };
        if position < MAX_CONTEXT_CHARACTERS {
            format!(
                "Unexpected token '{token}', \"{}\"... is not valid JSON",
                slice(0, position + MAX_CONTEXT_CHARACTERS)
            )
        } else if position < length - MAX_CONTEXT_CHARACTERS {
            format!(
                "Unexpected token '{token}', ...\"{}\"... is not valid JSON",
                slice(
                    position - MAX_CONTEXT_CHARACTERS,
                    position + MAX_CONTEXT_CHARACTERS
                )
            )
        } else {
            format!(
                "Unexpected token '{token}', ...\"{}\" is not valid JSON",
                slice(position - MAX_CONTEXT_CHARACTERS, length)
            )
        }
    }

    /// `(line L column C)`: `\r\n`, `\r` and `\n` each end a line.
    fn location(&self) -> String {
        let (mut line, mut line_start, mut index) = (1, 0, 0);
        while index < self.cursor {
            if self.units[index] == 0x0D && index + 1 < self.cursor && self.units[index + 1] == 0x0A
            {
                index += 1;
            }
            if matches!(self.units[index], 0x0D | 0x0A) {
                line += 1;
                line_start = index + 1;
            }
            index += 1;
        }
        format!("(line {line} column {})", 1 + self.cursor - line_start)
    }
}

fn is_digit(unit: u16) -> bool {
    (0x30..=0x39).contains(&unit)
}

fn is_hex(unit: u16) -> bool {
    is_digit(unit) || (0x41..=0x46).contains(&unit) || (0x61..=0x66).contains(&unit)
}
