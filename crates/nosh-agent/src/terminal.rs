//! The terminal-output protocol (`packages/pi-adapter/src/terminal-output.ts`, specified in
//! `docs/protocols/terminal-output.md`): a scoped turn ends with one JSON envelope of typed records,
//! which the host validates; a rejected envelope gets at most one schema-only correction.

use crate::js_json::{self, SyntaxError};
use nosh_wire::js::strict_equal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub const TERMINAL_OUTPUT_SCHEMA: &str = "https://nosh.dev/schemas/terminal-output/v1";
pub const TERMINAL_OUTPUT_MAX_BYTES: usize = 131_072;

/// Terminal record schema URI → the submission tool that owns it, in declaration order.
pub static TERMINAL_SCHEMA_TOOLS: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| {
    [
        ("general-worker-completion", "nosh_response_submit"),
        ("librarian-completion", "nosh_response_submit"),
        ("task-failure", "nosh_response_submit"),
        ("mission-director-cycle", "nosh_response_submit"),
        ("research-director-cycle", "nosh_response_submit"),
        ("review-verdict", "nosh_review_submit"),
        ("episode-draft", "nosh_episode_submit"),
        ("experiment-proposal", "nosh_experiment_propose"),
    ]
    .into_iter()
    .map(|(schema, tool)| (format!("https://nosh.dev/schemas/{schema}/v1"), tool))
    .collect()
});

/// The tools whose records only a terminal envelope may carry in a scoped turn.
pub fn is_terminal_tool(name: &str) -> bool {
    TERMINAL_SCHEMA_TOOLS.iter().any(|(_, tool)| *tool == name)
}

fn terminal_tool(schema: &str) -> Option<&'static str> {
    TERMINAL_SCHEMA_TOOLS
        .iter()
        .find(|(uri, _)| uri == schema)
        .map(|(_, tool)| *tool)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalContext {
    pub project_id: String,
    pub agent_id: String,
    pub task_id: Option<String>,
    pub instruction_id: Option<String>,
    pub thread_id: Option<String>,
    pub expected_version: Option<f64>,
    pub turn_id: String,
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_episode_type: Option<String>,
}

/// A parse failure: a `JSON.parse` syntax error, or a protocol violation (a plain `Error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalError {
    Syntax(SyntaxError),
    Invalid(String),
}

impl TerminalError {
    pub fn message(&self) -> &str {
        match self {
            Self::Syntax(error) => &error.message,
            Self::Invalid(message) => message,
        }
    }
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for TerminalError {}

fn invalid<T>(message: &str) -> Result<T, TerminalError> {
    Err(TerminalError::Invalid(message.into()))
}

/// The parsed envelope (`{ $schema, schemaVersion, records, repaired? }`).
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalOutput {
    pub envelope: Map<String, Value>,
    pub repaired: Option<String>,
}

impl TerminalOutput {
    pub fn records(&self) -> Vec<Value> {
        self.envelope["records"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The envelope as TypeScript returns it, with `repaired` when a repair was made.
    pub fn to_value(&self) -> Value {
        let mut envelope = self.envelope.clone();
        if let Some(repaired) = &self.repaired {
            envelope.insert("repaired".into(), json!(repaired));
        }
        Value::Object(envelope)
    }
}

/// JavaScript's `String.prototype.trim` whitespace and line terminators.
pub(crate) fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

static TOKENS: LazyLock<regex::Regex> = LazyLock::new(|| {
    // JavaScript's `\s` and line-terminator-free `.`, spelled out.
    let space = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
    regex::Regex::new(&format!(
        r#""(?:[^"\\]|\\[^\n\r\x{{2028}}\x{{2029}}])*"|[{{}}\[\]:,]|[^{space}{{}}\[\]:,]+"#
    ))
    .expect("token pattern compiles")
});

/// Reject duplicate keys rather than accepting `JSON.parse`'s last-key-wins ambiguity.
fn assert_unique_keys(text: &str) -> Result<(), TerminalError> {
    let tokens: Vec<&str> = TOKENS.find_iter(text).map(|token| token.as_str()).collect();
    let mut index = 0;
    value(&tokens, &mut index)?;
    if index != tokens.len() {
        return invalid("Multiple JSON values are not allowed");
    }
    Ok(())
}

fn value(tokens: &[&str], index: &mut usize) -> Result<(), TerminalError> {
    let token = tokens.get(*index).copied();
    *index += 1;
    match token {
        Some("{") => {
            let mut keys = std::collections::HashSet::new();
            while tokens.get(*index).copied() != Some("}") {
                let key = js_json::parse(tokens.get(*index).copied().unwrap_or("undefined"))
                    .map_err(TerminalError::Syntax)?;
                *index += 1;
                if !keys.insert(key.as_str().unwrap_or_default().to_owned()) {
                    return invalid("Duplicate JSON object key");
                }
                let colon = tokens.get(*index).copied();
                *index += 1;
                if colon != Some(":") {
                    return invalid("Invalid JSON object");
                }
                value(tokens, index)?;
                if tokens.get(*index).copied() != Some(",") {
                    break;
                }
                *index += 1;
            }
            let close = tokens.get(*index).copied();
            *index += 1;
            if close != Some("}") {
                return invalid("Invalid JSON object");
            }
        }
        Some("[") => {
            while tokens.get(*index).copied() != Some("]") {
                value(tokens, index)?;
                if tokens.get(*index).copied() != Some(",") {
                    break;
                }
                *index += 1;
            }
            let close = tokens.get(*index).copied();
            *index += 1;
            if close != Some("]") {
                return invalid("Invalid JSON array");
            }
        }
        _ => {}
    }
    Ok(())
}

static CONTROL_TOKENS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</?\|[a-z_]+\|>").expect("pattern compiles"));

/// Models hand-copying long envelopes often append stray closing brackets after a complete
/// object. Accept the first complete top-level object only when everything after it is `]`, `}`,
/// or whitespace (or a leaked chat-template control token such as `<|im_end|>`); any other
/// trailing content (prose, a second object) still fails JSON parsing.
fn without_stray_closers(text: &[u16]) -> Vec<u16> {
    // `indexOf` -1 scans from the start: `text[-1]` is undefined and changes nothing.
    let start = text
        .iter()
        .position(|unit| *unit == u16::from(b'{'))
        .unwrap_or(0);
    let (mut depth, mut in_string, mut escaped) = (0i64, false, false);
    for index in start..text.len() {
        let unit = text[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if unit == u16::from(b'\\') {
                escaped = true;
            } else if unit == u16::from(b'"') {
                in_string = false;
            }
            continue;
        }
        if unit == u16::from(b'"') {
            in_string = true;
        } else if unit == u16::from(b'{') || unit == u16::from(b'[') {
            depth += 1;
        } else if unit == u16::from(b'}') || unit == u16::from(b']') {
            depth -= 1;
            if depth == 0 {
                let rest = String::from_utf16_lossy(&text[index + 1..]);
                let rest = CONTROL_TOKENS.replace_all(&rest, "");
                return if rest
                    .chars()
                    .all(|c| is_js_whitespace(c) || c == ']' || c == '}')
                {
                    text[..=index].to_vec()
                } else {
                    text.to_vec()
                };
            }
        }
    }
    text.to_vec()
}

struct Parsed {
    value: Value,
    text: String,
    repaired: Option<String>,
}

/// Models sometimes write one stray closing bracket mid-envelope and repeat it on correction,
/// because the parse error lands after it. Deleting one of the two nearest closers before the
/// error is a syntax-only repair: the result still passes every envelope check and host validation.
fn parse_repairing_stray_closer(text: &str) -> Result<Parsed, SyntaxError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    // Trailing-closer trimming runs per candidate: a mid-envelope stray closer makes the raw text
    // look complete one bracket early.
    let trimmed = String::from_utf16_lossy(&without_stray_closers(&units));
    let error = match js_json::parse(&trimmed) {
        Ok(value) => {
            return Ok(Parsed {
                value,
                text: trimmed,
                repaired: None,
            });
        }
        Err(error) => error,
    };
    let Some(at) = error.position() else {
        return Err(error);
    };
    let mut closers = Vec::new();
    let mut in_string = false;
    let mut index = 0;
    while index < at {
        let unit = units.get(index).copied();
        if in_string {
            if unit == Some(u16::from(b'\\')) {
                index += 1;
            } else if unit == Some(u16::from(b'"')) {
                in_string = false;
            }
        } else if unit == Some(u16::from(b'"')) {
            in_string = true;
        } else if unit == Some(u16::from(b'}')) || unit == Some(u16::from(b']')) {
            closers.push(index);
        }
        index += 1;
    }
    for &index in closers.iter().rev().take(2) {
        let removed: Vec<u16> = units[..index]
            .iter()
            .chain(&units[index + 1..])
            .copied()
            .collect();
        let candidate = String::from_utf16_lossy(&without_stray_closers(&removed));
        if let Ok(value) = js_json::parse(&candidate) {
            return Ok(Parsed {
                value,
                text: candidate,
                repaired: Some(format!(
                    "removed '{}' at offset {index}",
                    char::from(units[index] as u8)
                )),
            });
        }
    }
    Err(error)
}

pub fn parse_terminal_output(
    text: &str,
    context: &TerminalContext,
) -> Result<TerminalOutput, TerminalError> {
    if text.len() > TERMINAL_OUTPUT_MAX_BYTES {
        return invalid("Terminal output exceeds 131072 UTF-8 bytes");
    }
    if !text.trim_matches(is_js_whitespace).starts_with('{') {
        return invalid("Return one unfenced JSON object, without prose");
    }
    let parsed = parse_repairing_stray_closer(text).map_err(TerminalError::Syntax)?;
    assert_unique_keys(&parsed.text)?;
    let Value::Object(object) = parsed.value else {
        return invalid("Terminal envelope must be an object");
    };
    let records = object.get("records").and_then(Value::as_array);
    let envelope_keys = object.len() == 3
        && ["$schema", "records", "schemaVersion"]
            .iter()
            .all(|key| object.contains_key(*key));
    if !envelope_keys
        || object["$schema"] != TERMINAL_OUTPUT_SCHEMA
        || !strict_equal(object.get("schemaVersion"), Some(&json!(1)))
        || !records.is_some_and(|records| (1..=2).contains(&records.len()))
    {
        return invalid("Invalid terminal envelope schema or record count (1..2)");
    }
    let mut seen: Vec<&'static str> = Vec::new();
    for record in records.into_iter().flatten() {
        let Some(record) = record.as_object() else {
            return invalid("Terminal record must be an object");
        };
        let schema = nosh_wire::js::string(record.get("$schema"));
        let Some(tool) = terminal_tool(&schema)
            .filter(|tool| context.allowed_tools.iter().any(|allowed| allowed == tool))
        else {
            return invalid("Terminal record schema is not permitted for this turn");
        };
        if seen.contains(&tool)
            || tool != "nosh_episode_submit"
                && seen.iter().any(|name| *name != "nosh_episode_submit")
        {
            return invalid("Only one terminal outcome and one episode draft are allowed");
        }
        seen.push(tool);
        let expected = context
            .expected_episode_type
            .as_ref()
            .map(|kind| json!(kind));
        if tool == "nosh_episode_submit"
            && !strict_equal(expected.as_ref(), record.get("episodeType"))
        {
            return invalid("Episode type does not match the host instruction");
        }
    }
    if context.task_id.as_deref().is_some_and(|id| !id.is_empty())
        && !seen.iter().any(|tool| *tool != "nosh_episode_submit")
    {
        return invalid("Task turn requires one terminal outcome");
    }
    if context
        .expected_episode_type
        .as_deref()
        .is_some_and(|kind| !kind.is_empty())
        && !seen.contains(&"nosh_episode_submit")
    {
        return invalid("Runtime turn requires one episode draft");
    }
    Ok(TerminalOutput {
        envelope: object,
        repaired: parsed.repaired,
    })
}

pub fn terminal_prompt(context: &TerminalContext) -> String {
    let schemas: Vec<&str> = TERMINAL_SCHEMA_TOOLS
        .iter()
        .filter(|(_, tool)| context.allowed_tools.iter().any(|allowed| allowed == tool))
        .map(|(schema, _)| schema.as_str())
        .collect();
    let task = if context.task_id.as_deref().is_some_and(|id| !id.is_empty()) {
        " Include exactly one task terminal outcome."
    } else {
        ""
    };
    let episode = match context.expected_episode_type.as_deref() {
        Some(kind) if !kind.is_empty() => {
            format!(" Include exactly one episode-draft of type {kind}.")
        }
        _ => String::new(),
    };
    format!(
        "TERMINAL OUTPUT CONTRACT (overrides older completion-tool instructions): Keep using execution tools and immediate acknowledgement/progress/effect commands. Do not call response, review, or episode submission tools. Your last assistant message must be exactly one unfenced JSON object, no prose: {{\"$schema\":\"{TERMINAL_OUTPUT_SCHEMA}\",\"schemaVersion\":1,\"records\":[COMPLETE_TYPED_RECORDS]}}. Maximum {TERMINAL_OUTPUT_MAX_BYTES} UTF-8 bytes. Allowed schemas: {}. At most one terminal outcome plus one episode draft.{task}{episode} The host binds all routing and validates records; output is a proposal, not proof of acceptance or successful effects.",
        schemas.join(", ")
    )
}

/// A bare character offset is useless to a model; show where the JSON broke so the correction can
/// fix it.
pub fn with_json_excerpt(error: &TerminalError, text: &str) -> String {
    let TerminalError::Syntax(syntax) = error else {
        return error.message().to_owned();
    };
    let Some(at) = syntax.position() else {
        return syntax.message.clone();
    };
    let message = &syntax.message;
    let units: Vec<u16> = text.encode_utf16().collect();
    let slice = |start: usize, end: usize| {
        let (start, end) = (start.min(units.len()), end.min(units.len()));
        String::from_utf16_lossy(&units[start..end.max(start)])
    };
    format!(
        "{message}. JSON near the error: …{}⟪HERE⟫{}…",
        slice(at.saturating_sub(80), at),
        slice(at, at + 40)
    )
}
