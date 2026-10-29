//! Pure derivations from daemon JSON to display rows: a port of `apps/tui/src/view-model.ts`.
//!
//! Specified by tui-viewmodel.md §1, §2 (every function in source order), §5.2, §5.3, §6.1 and §7
//! with addenda VA-1 to VA-4; the controller spec §12 restates the same functions as consumed
//! interfaces. Every length and slice counts UTF-16 code units (DECISIONS.md item 5).
//!
//! `safe_text`, `safe_text_str` and `record` are implemented by the scaffold because every track
//! consumes them (transport `requestError`, controller, dialogs, visuals); the rest is Track B's.
//!
//! Deviations to document in code (viewmodel §7.6): lone surrogates become U+FFFD; only the three
//! `MAP_ROOTS` keys are roots (no `Object.prototype` names); `localeCompare` is a UTF-16 code-unit
//! compare (DECISIONS.md item 7); `progress.done` goes through full `ToNumber`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::js::{
    JsObject, JsValue, is_safe_integer, js_collapse_ws, js_json_parse, js_lowercase, js_number,
    js_trim, json_stringify_pretty, length, number_string, pad_end, slice_from_end, slice_to,
    string, truthy,
};

/// `text()`'s default `limit`.
pub const TEXT_LIMIT: usize = 512;
/// `fields()`'s default `maximum`.
pub const FIELDS_MAXIMUM: usize = 16;
/// `boundRows()`'s default `maximum`.
pub const BOUND_ROWS_MAXIMUM: usize = 80;
/// `collapseLines()`'s default `maximum` (lines).
pub const COLLAPSE_LINES: usize = 6;
/// `collapseLines()`'s default `characters`.
pub const COLLAPSE_CHARACTERS: usize = 360;
/// `climbLines()`'s default `maximum`.
pub const CLIMB_MAXIMUM: usize = 24;
/// `progressBar()`'s default `cells`.
pub const PROGRESS_CELLS: usize = 10;

/// `MAP_ROOTS` as `(kind, glyph, label)`, in source order (viewmodel §2.3).
pub const MAP_ROOTS: [(&str, &str, &str); 3] = [
    ("mission", "◆", "MISSION"),
    ("direction", "◇", "DIRECTION"),
    ("autoresearch", "⟳", "AUTORESEARCH"),
];

/// `ViewField`: one labelled value of a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewField {
    pub label: String,
    pub value: String,
}

/// `ViewRow`: one inspector card or sidebar line. Optional fields are absent (`None`), never
/// defaulted: `version` is absent for event-derived rows (viewmodel §2.2, controller §2.2).
#[derive(Clone, Debug, PartialEq)]
pub struct ViewRow {
    pub id: String,
    pub section: String,
    pub title: String,
    pub state: Option<String>,
    /// A JS number; print it with `number_string`.
    pub version: Option<f64>,
    pub subtitle: Option<String>,
    pub fields: Vec<ViewField>,
}

/// `TranscriptEntry['kind']`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    User,
    Assistant,
    Tool,
    Failure,
    Receipt,
    Approval,
}

impl EntryKind {
    /// The TS string: `user`, `assistant`, `tool`, `failure`, `receipt` or `approval`.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::User => "user",
            EntryKind::Assistant => "assistant",
            EntryKind::Tool => "tool",
            EntryKind::Failure => "failure",
            EntryKind::Receipt => "receipt",
            EntryKind::Approval => "approval",
        }
    }
}

/// `TranscriptEntry`: one conversation message (durable or a live draft).
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptEntry {
    pub id: String,
    pub kind: EntryKind,
    pub title: String,
    pub text: String,
    /// The JS `number | null`: `None` for ephemeral entries and drafts.
    pub sequence: Option<f64>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub status: Option<String>,
}

/// `ActivityState`: what the composer line shows, produced by `Controller::activity` (controller
/// §7.14). `role` is always present (possibly `""`); `agent_id` only when non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityState {
    pub working: bool,
    pub label: String,
    pub model: String,
    pub thinking_level: String,
    pub agent_id: Option<String>,
    pub role: String,
}

/// `PendingAction`: the staged action as `Controller::pending_action` presents it (controller
/// §7.10). `body` keeps JS insertion order; `palette::action_fingerprint` serializes it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingAction {
    pub path: String,
    pub body: JsObject,
    pub project_id: String,
    pub title: String,
    pub fields: Vec<ViewField>,
}

/// `MapTone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MapTone {
    Success,
    Active,
    Waiting,
    Error,
    Idle,
}

impl MapTone {
    /// The TS string: `success`, `active`, `waiting`, `error` or `idle`.
    pub fn as_str(self) -> &'static str {
        match self {
            MapTone::Success => "success",
            MapTone::Active => "active",
            MapTone::Waiting => "waiting",
            MapTone::Error => "error",
            MapTone::Idle => "idle",
        }
    }
}

/// `MapLine`: one row of the sidebar climb graph (`climbLines`).
#[derive(Clone, Debug, PartialEq)]
pub struct MapLine {
    pub prefix: String,
    pub glyph: String,
    pub tone: MapTone,
    pub label: String,
    pub title: String,
    pub state: String,
    pub detail: String,
    /// The optional TS `heading?: boolean`: `Some(true)` on heading lines, absent otherwise.
    pub heading: Option<bool>,
}

/// Node's `stripVTControlCharacters` pattern, translated for the `regex` crate (viewmodel §2.4).
static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"[\x1B\x{9B}][\[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?(?:\x07|\x1B\\|\x{9C}))|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]))",
    )
    .expect("valid ANSI pattern")
});

/// C0 controls except TAB and LF, DEL, C1 controls, bidi embeddings/overrides and isolates.
static CONTROLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\x00-\x08\x0B-\x1F\x7F-\x{9F}\x{202A}-\x{202E}\x{2066}-\x{2069}]")
        .expect("valid control pattern")
});

/// The shared `{}` that [`record`] returns for non-objects.
static EMPTY_OBJECT: JsObject = JsObject::new();

/// `safeText`: `String(value)`, cut to 256000 UTF-16 units, ANSI sequences stripped, the remaining
/// controls (all but TAB and LF) and bidi controls removed, cut to 16000 units (viewmodel §2.4).
pub fn safe_text(value: Option<&JsValue>) -> String {
    safe_text_str(&string(value))
}

/// `safeText` for a string argument (the common case: `String(value)` is the identity).
pub fn safe_text_str(value: &str) -> String {
    let head = if length(value) > 256_000 {
        slice_to(value, 256_000)
    } else {
        value.to_owned()
    };
    let stripped = ANSI.replace_all(&head, "");
    let cleaned = CONTROLS.replace_all(&stripped, "");
    if length(&cleaned) > 16_000 {
        slice_to(&cleaned, 16_000)
    } else {
        cleaned.into_owned()
    }
}

/// `record`: the object itself for a non-null, non-array object; a shared empty object otherwise
/// (viewmodel §2.5). It is not a boolean test.
pub fn record(value: Option<&JsValue>) -> &JsObject {
    match value {
        Some(JsValue::Object(object)) => object,
        _ => &EMPTY_OBJECT,
    }
}

/// `text`: `safeText(value).slice(0, limit)` for strings, numbers and booleans; `""` otherwise
/// (viewmodel §2.6). Callers pass [`TEXT_LIMIT`] where TS uses the default.
pub fn text(value: Option<&JsValue>, limit: usize) -> String {
    match value {
        Some(JsValue::String(_) | JsValue::Number(_) | JsValue::Bool(_)) => {
            slice_to(&safe_text(value), limit)
        }
        _ => String::new(),
    }
}

/// `text` of a value known to be a string.
fn text_str(value: &str, limit: usize) -> String {
    slice_to(&safe_text_str(value), limit)
}

/// `a ?? b ?? …`: the first value that is neither absent nor `null`. When every value is nullish
/// the result is `None`; every caller then reads it through `text`, `truthy` or `record`, for which
/// `null` and `undefined` agree.
fn first<const N: usize>(values: [Option<&JsValue>; N]) -> Option<&JsValue> {
    values
        .into_iter()
        .find(|value| value.is_some_and(|value| !value.is_null()))
        .flatten()
}

/// `value || fallback` for a string.
fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// `Array.isArray(value) ? value.map(record) : []`.
fn records(value: Option<&JsValue>) -> Vec<&JsObject> {
    value
        .and_then(JsValue::as_array)
        .map_or_else(Vec::new, |items| {
            items.iter().map(|item| record(Some(item))).collect()
        })
}

/// `array.join(separator)`: `null` elements become empty, everything else is `String(element)`.
fn join(items: &[JsValue], separator: &str) -> String {
    items
        .iter()
        .map(|item| {
            if item.is_null() {
                String::new()
            } else {
                string(Some(item))
            }
        })
        .collect::<Vec<_>>()
        .join(separator)
}

static CAMEL_CASE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("([a-z])([A-Z])").expect("valid camel-case pattern"));

/// `label`: a field key as words, first letter uppercased (viewmodel §2.7). `charAt(0)` is one
/// UTF-16 unit, so an astral first character stays as it is.
fn label(key: &str) -> String {
    let spaced = CAMEL_CASE
        .replace_all(key, "${1} ${2}")
        .replace(['_', '-'], " ");
    let words = slice_to(&js_lowercase(&safe_text_str(&spaced)), 80);
    let mut characters = words.chars();
    match characters.next() {
        Some(head) if u32::from(head) < 0x1_0000 => head.to_uppercase().chain(characters).collect(),
        _ => words,
    }
}

/// Keys `fields` never shows: provenance, bulk payloads, and ids that are always the current
/// project or Pi-internal.
const HIDDEN_FIELDS: [&str; 10] = [
    "$schema",
    "schemaVersion",
    "idempotencyKey",
    "payload",
    "records",
    "content",
    "thinking",
    "reasoning",
    "projectId",
    "piSessionId",
];

/// `fields`: up to `maximum` labelled scalar fields of an object, in JS property order
/// (viewmodel §2.8). Callers pass [`FIELDS_MAXIMUM`] where TS uses the default.
pub fn fields(value: Option<&JsValue>, maximum: usize) -> Vec<ViewField> {
    let mut result = Vec::new();
    visit_fields(record(value), "", 0, maximum, &mut result);
    result
}

fn visit_fields(
    data: &JsObject,
    prefix: &str,
    depth: u8,
    maximum: usize,
    result: &mut Vec<ViewField>,
) {
    for (key, item) in data.iter().take(40) {
        if result.len() >= maximum {
            break;
        }
        if HIDDEN_FIELDS.contains(&key) {
            continue;
        }
        let name = if prefix.is_empty() {
            label(key)
        } else {
            format!("{prefix} / {}", label(key))
        };
        match item {
            JsValue::Null => {}
            JsValue::Array(items) => {
                let scalars: Vec<String> = items
                    .iter()
                    .take(8)
                    .map(|item| text(Some(item), 120))
                    .filter(|scalar| !scalar.is_empty())
                    .collect();
                let value = if scalars.is_empty() {
                    format!("{} items", items.len())
                } else {
                    scalars.join(" · ") + if items.len() > 8 { " …" } else { "" }
                };
                result.push(ViewField { label: name, value });
            }
            JsValue::Object(object) => {
                if depth < 1 {
                    visit_fields(object, &name, depth + 1, maximum, result);
                }
            }
            scalar => result.push(ViewField {
                label: name,
                value: short_digest(text(Some(scalar), TEXT_LIMIT)),
            }),
        }
    }
}

static DIGEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^(sha256:)?([0-9a-f]{40,64})$").expect("valid digest pattern"));

/// `shortDigest`: a full digest as its first 12 and last 4 hex digits (viewmodel §2.9).
fn short_digest(value: String) -> String {
    match DIGEST.captures(&value) {
        Some(digest) => {
            let hex = &digest[2];
            format!(
                "{}{}…{}",
                digest.get(1).map_or("", |prefix| prefix.as_str()),
                &hex[..12],
                &hex[hex.len() - 4..]
            )
        }
        None => value,
    }
}

/// `formatFields`: `label: value` lines joined with `\n` (viewmodel §2.10).
pub fn format_fields(items: &[ViewField]) -> String {
    items
        .iter()
        .map(|field| format!("{}: {}", field.label, field.value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `modelName`: `provider/id` from `provider ?? modelProvider` and `id ?? modelId`, or `""`
/// (viewmodel §2.11). Pi's `unknown` placeholder is not a model.
pub fn model_name(value: Option<&JsValue>) -> String {
    let model = record(value);
    [
        text(
            first([model.get("provider"), model.get("modelProvider")]),
            TEXT_LIMIT,
        ),
        text(first([model.get("id"), model.get("modelId")]), TEXT_LIMIT),
    ]
    .into_iter()
    .filter(|part| !part.is_empty() && part != "unknown")
    .collect::<Vec<_>>()
    .join("/")
}

/// `entityRow`: the inspector card of one entity (viewmodel §2.12). `index` names id-less rows
/// `<section>-<index>`; callers pass 0 where TS uses the default.
pub fn entity_row(section: &str, value: Option<&JsValue>, index: usize) -> ViewRow {
    let stored = record(value);
    let inner = record(stored.get("value"));
    let v = if inner.is_empty() { stored } else { inner };
    let proposal = record(v.get("record"));
    let resources = record(stored.get("resources"));
    let id = non_empty(text(
        first([
            stored.get("entityId"),
            v.get("jobId"),
            v.get("agentId"),
            v.get("proposalId"),
            v.get("id"),
            proposal.get("proposalId"),
        ]),
        TEXT_LIMIT,
    ))
    .unwrap_or_else(|| format!("{section}-{index}"));
    let mut title = text(
        first([
            v.get("title"),
            v.get("question"),
            v.get("decisionQuestion"),
            proposal.get("summary"),
            proposal.get("rationale"),
            v.get("role"),
            v.get("workingTitle"),
        ]),
        TEXT_LIMIT,
    );
    if title.is_empty() && section == "jobs" {
        title = match v.get("command") {
            Some(JsValue::Array(command)) => {
                text_str(&join(&command[..command.len().min(8)], " "), TEXT_LIMIT)
            }
            command => text(command, TEXT_LIMIT),
        };
    }
    if title.is_empty() {
        title.clone_from(&id);
    }
    let state = text(
        first([stored.get("state"), v.get("state"), v.get("status")]),
        TEXT_LIMIT,
    );
    let version = match stored.get("version") {
        Some(JsValue::Number(version)) if is_safe_integer(*version) => Some(*version),
        _ => None,
    };
    let subtitle = text(
        first([
            v.get("objective"),
            v.get("decisionUse"),
            v.get("failureReason"),
            proposal.get("contractImpact"),
            v.get("currentTool"),
            v.get("workingDirectory"),
        ]),
        TEXT_LIMIT,
    );
    // A job's title is already its command line. Elsewhere `{command, ...detail}` puts `command`
    // first even when it is undefined; `null` stands in for that, as `fields` skips both.
    let mut detail = JsObject::new();
    if section != "jobs" {
        detail.set(
            "command",
            v.get("command").cloned().unwrap_or(JsValue::Null),
        );
    }
    for (key, item) in v.iter().filter(|(key, _)| *key != "command") {
        detail.set(key, item.clone());
    }
    detail.spread(resources);
    ViewRow {
        id,
        section: section.to_owned(),
        title,
        state: non_empty(state),
        version,
        subtitle: non_empty(subtitle),
        fields: fields(Some(&JsValue::Object(detail)), FIELDS_MAXIMUM),
    }
}

/// A row with no state, version or subtitle.
fn plain_row(id: &str, section: &str, title: &str, fields: Vec<ViewField>) -> ViewRow {
    ViewRow {
        id: id.to_owned(),
        section: section.to_owned(),
        title: title.to_owned(),
        state: None,
        version: None,
        subtitle: None,
        fields,
    }
}

/// `responseRows`: the rows of one daemon response for a section (viewmodel §2.13).
pub fn response_rows(section: &str, data: Option<&JsValue>) -> Vec<ViewRow> {
    let body = record(data);
    let list_key = match section {
        "autoresearch" => "executions",
        "approvals" => "proposals",
        other => other,
    };
    if let Some(JsValue::Array(list)) = body.get(list_key) {
        return list
            .iter()
            .take(80)
            .enumerate()
            .map(|(index, item)| entity_row(section, Some(item), index))
            .collect();
    }
    if truthy(body.get("job")) {
        let mut job = record(body.get("job")).clone();
        job.set(
            "resources",
            body.get("resources").cloned().unwrap_or(JsValue::Null),
        );
        return vec![entity_row("jobs", Some(&JsValue::Object(job)), 0)];
    }
    if let Some(JsValue::String(output)) = body.get("text") {
        let value = safe_text_str(&slice_from_end(output, 16_000));
        return vec![plain_row(
            "output",
            "output",
            "Bounded job output",
            vec![ViewField {
                label: "output".into(),
                value,
            }],
        )];
    }
    if body.contains_key("contract") {
        let contract = body.get("contract");
        if !truthy(contract) {
            let mut row = plain_row(
                "contract",
                "contract",
                "Project contract",
                vec![ViewField {
                    label: "next step".into(),
                    value: "Continue intake in the conversation. Contract approval is explicit."
                        .into(),
                }],
            );
            row.state = Some("not approved".into());
            return vec![row];
        }
        // The daemon returns the active contract even while it is an unapproved draft.
        let approved = truthy(record(contract).get("approvedAt"));
        let mut row = entity_row("contract", contract, 0);
        row.id = "contract".into();
        row.title = non_empty(text(record(contract).get("workingTitle"), TEXT_LIMIT))
            .unwrap_or_else(|| "Project contract".into());
        row.state = Some(if approved { "approved" } else { "draft" }.into());
        if !approved {
            row.subtitle = Some(
                "Not approved. Approval happens explicitly in the intake conversation.".into(),
            );
        }
        return vec![row];
    }
    if body.is_empty() {
        return Vec::new();
    }
    let nested = ["mission", "direction", "execution", "proposal"]
        .into_iter()
        .find(|key| truthy(body.get(key)));
    vec![entity_row(
        section,
        nested.map_or(data, |key| body.get(key)),
        0,
    )]
}

/// `boundRows`: at most `maximum` rows, 16 fields each, 48000 units of field values in total
/// (viewmodel §2.14). Callers pass [`BOUND_ROWS_MAXIMUM`] where TS uses the default. As in TS, the
/// first empty field value ends a row's fields, and once the budget is spent later rows get none.
pub fn bound_rows(rows: &[ViewRow], maximum: usize) -> Vec<ViewRow> {
    let mut left = 48_000;
    rows.iter()
        .take(maximum)
        .map(|row| {
            let mut result = ViewRow {
                id: safe_text_str(&row.id),
                section: text_str(&row.section, 80),
                title: text_str(&row.title, TEXT_LIMIT),
                state: row.state.clone(),
                version: row.version,
                subtitle: row.subtitle.clone(),
                fields: Vec::new(),
            };
            if let Some(subtitle) = result.subtitle.as_mut().filter(|value| !value.is_empty()) {
                *subtitle = text_str(subtitle, TEXT_LIMIT);
            }
            if let Some(state) = result.state.as_mut().filter(|value| !value.is_empty()) {
                *state = text_str(state, 80);
            }
            for field in row.fields.iter().take(16) {
                let value = slice_to(&safe_text_str(&field.value), left);
                if value.is_empty() {
                    break;
                }
                left -= length(&value);
                result.fields.push(ViewField {
                    label: text_str(&field.label, 80),
                    value,
                });
            }
            result
        })
        .collect()
}

/// `rawDetail`: the clipped `JSON.stringify(value, null, 2)` shown for staged reviews and raw
/// details (viewmodel §2.15).
pub fn raw_detail(value: Option<&JsValue>) -> String {
    let mut left = 24_000;
    match value {
        // `JSON.stringify(undefined)` is undefined, and `safeText` makes it "undefined".
        None => "undefined".into(),
        Some(value) => safe_text_str(&json_stringify_pretty(&clip(value, 0, &mut left))),
    }
}

fn clip(value: &JsValue, depth: u8, left: &mut usize) -> JsValue {
    if *left == 0 {
        return "[clipped]".into();
    }
    match value {
        JsValue::String(text) => {
            let out = slice_to(&safe_text_str(text), (*left).min(4096));
            *left -= length(&out);
            JsValue::String(out)
        }
        JsValue::Array(_) | JsValue::Object(_) if depth > 4 => "[focused detail omitted]".into(),
        JsValue::Array(items) => JsValue::Array(
            items
                .iter()
                .take(30)
                .map(|item| clip(item, depth + 1, left))
                .collect(),
        ),
        JsValue::Object(object) => JsValue::Object(
            object
                .iter()
                .take(40)
                .map(|(key, item)| (text_str(key, 100), clip(item, depth + 1, left)))
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}

/// `contentText`: the readable text of a tool result (viewmodel §2.16).
fn content_text(value: Option<&JsValue>) -> String {
    if let Some(JsValue::String(text)) = value {
        return safe_text_str(text);
    }
    let v = record(value);
    if let Some(JsValue::Array(content)) = v.get("content") {
        let parts: Vec<String> = content
            .iter()
            .take(20)
            .map(|item| record(Some(item)))
            .filter(|item| item.get("type").and_then(JsValue::as_str) == Some("text"))
            .map(|item| text(item.get("text"), 4000))
            .collect();
        return safe_text_str(&parts.join("\n"));
    }
    non_empty(text(
        first([
            v.get("message"),
            v.get("text"),
            v.get("summary"),
            v.get("error"),
        ]),
        4000,
    ))
    .unwrap_or_else(|| format_fields(&fields(value, 8)))
}

const TERMINAL_OUTPUT_SCHEMA: &str = "https://nosh.dev/schemas/terminal-output/v1";

/// `assistantText`: a completed turn's prose, or a summary of a terminal envelope, which is
/// protocol output rather than conversation (viewmodel §2.17).
fn assistant_text(value: Option<&JsValue>) -> String {
    let plain = text(value, 16_000);
    if let Some(JsValue::String(message)) = value
        && length(message) <= 131_072
        && js_trim(message).starts_with('{')
        && let Some(envelope) = parse_leading_object(message)
    {
        let envelope = record(Some(&envelope));
        if envelope.get("$schema").and_then(JsValue::as_str) == Some(TERMINAL_OUTPUT_SCHEMA)
            && let Some(JsValue::Array(items)) = envelope.get("records")
        {
            let lines: Vec<String> = items
                .iter()
                .take(8)
                .map(|item| {
                    let item = record(Some(item));
                    non_empty(text(
                        first([
                            item.get("summary"),
                            item.get("message"),
                            item.get("outcome"),
                        ]),
                        2000,
                    ))
                    .unwrap_or_else(|| "Structured result submitted".into())
                })
                .collect();
            return lines.join("\n") + "\nAwait the host receipt for validation.";
        }
    }
    plain
}

/// JavaScript's `\s` as a regex class body (viewmodel §1.6).
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// What the daemon's terminal parser strips after the envelope: stray closers and chat-template
/// tokens such as `</|im_end|>` (`/^[\s\]}]*(<\/?\|[a-z_]+\|>\s*)*$/i`).
static ENVELOPE_TRAILER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^[{JS_SPACE}\]}}]*(?:</?\|[a-zA-Z_]+\|>[{JS_SPACE}]*)*$"
    ))
    .expect("valid trailer pattern")
});

/// `parseLeadingObject`: the first complete JSON object of `value`, or `None` where TS throws
/// (viewmodel §2.18).
fn parse_leading_object(value: &str) -> Option<JsValue> {
    let source = js_trim(value);
    let (mut depth, mut in_string, mut escaped) = (0i64, false, false);
    let mut end = None;
    for (index, character) in source.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(index + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    if !ENVELOPE_TRAILER.is_match(&source[end..]) {
        return None;
    }
    js_json_parse(&source[..end]).ok()
}

/// `toolSummary`: the one-line target of a tool call for its transcript title (viewmodel §2.19).
fn tool_summary(args: Option<&JsValue>) -> String {
    let a = record(args);
    let value = text(
        first([
            a.get("command"),
            a.get("path"),
            a.get("file_path"),
            a.get("pattern"),
            a.get("query"),
            a.get("url"),
        ]),
        200,
    );
    let value = js_trim(&js_collapse_ws(&value)).to_owned();
    if length(&value) > 72 {
        slice_to(&value, 71) + "…"
    } else {
        value
    }
}

/// `collapseLines`: at most `maximum` lines and `characters` units (viewmodel §2.20). Callers pass
/// [`COLLAPSE_LINES`] and [`COLLAPSE_CHARACTERS`] where TS uses the defaults.
pub fn collapse_lines(value: &str, maximum: usize, characters: usize) -> String {
    let lines: Vec<&str> = value.trim_end_matches('\n').split('\n').collect();
    let mut head = lines[..lines.len().min(maximum)].join("\n");
    // A single long line (minified JSON) also floods the transcript.
    if length(&head) > characters {
        head = slice_to(&head, characters.saturating_sub(1)) + "…";
    }
    match lines.len().checked_sub(maximum) {
        Some(more) if more > 0 => format!("{head}\n… {more} more lines"),
        _ => head,
    }
}

impl TranscriptEntry {
    /// `Object.assign(previous, entry)` for a tool entry: keys the newer entry lacks (agent, model
    /// and thinking level when absent) keep their earlier values; `sequence` is always assigned.
    pub(crate) fn assign(&mut self, entry: TranscriptEntry) {
        self.id = entry.id;
        self.kind = entry.kind;
        self.title = entry.title;
        self.text = entry.text;
        self.sequence = entry.sequence;
        self.status = entry.status;
        if entry.agent_id.is_some() {
            self.agent_id = entry.agent_id;
        }
        if entry.model.is_some() {
            self.model = entry.model;
        }
        if entry.thinking_level.is_some() {
            self.thinking_level = entry.thinking_level;
        }
    }
}

/// `eventEntries`: transcript entries from durable events (each a `NoshEvent` object), already
/// bounded by `boundEntries` (viewmodel §2.21).
pub fn event_entries(events: &[JsValue]) -> Vec<TranscriptEntry> {
    let mut result: Vec<TranscriptEntry> = Vec::new();
    let mut tools: HashMap<String, usize> = HashMap::new();
    let mut selections: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for event in &events[events.len().saturating_sub(400)..] {
        let e = record(Some(event));
        let p = record(e.get("payload"));
        let scope = record(e.get("scope"));
        let kind = e.get("type").and_then(JsValue::as_str).unwrap_or("");
        let agent_id = text(first([scope.get("agentId"), p.get("agentId")]), TEXT_LIMIT);
        if kind == "chat.user_message" && !agent_id.is_empty() {
            let selection = record(p.get("selection"));
            selections.insert(
                agent_id.clone(),
                (
                    non_empty(model_name(selection.get("model"))),
                    non_empty(text(selection.get("thinkingLevel"), TEXT_LIMIT)),
                ),
            );
        }
        let event_id = string(e.get("eventId"));
        let (model, thinking_level) = selections.get(&agent_id).cloned().unwrap_or_default();
        let entry = |kind: EntryKind, title: &str, text: String, status: &str| TranscriptEntry {
            id: event_id.clone(),
            kind,
            title: title.to_owned(),
            text,
            sequence: e.get("sequence").and_then(JsValue::as_f64),
            agent_id: non_empty(agent_id.clone()),
            model: model.clone(),
            thinking_level: thinking_level.clone(),
            status: non_empty(status.to_owned()),
        };
        match kind {
            "chat.user_message" => {
                result.push(entry(
                    EntryKind::User,
                    "YOU",
                    text(p.get("message"), 16_000),
                    "",
                ));
            }
            "agent.completed" => {
                let message = assistant_text(first([p.get("message"), p.get("text")]));
                if !message.is_empty() {
                    result.push(entry(EntryKind::Assistant, "NOSH", message, "completed"));
                }
            }
            "agent.failed" | "agent.retrying" => {
                let retrying = kind == "agent.retrying";
                let message = non_empty(text(
                    first([p.get("message"), p.get("error"), p.get("reason")]),
                    4000,
                ))
                .unwrap_or_else(|| "The agent turn failed.".into());
                result.push(entry(
                    EntryKind::Failure,
                    if retrying { "RETRYING" } else { "TURN FAILED" },
                    message,
                    if retrying { "retrying" } else { "failed" },
                ));
            }
            "agent.tool_started" | "agent.tool_completed" => {
                let tool_key = if truthy(p.get("toolCallId")) {
                    format!("{agent_id}:{}", text(p.get("toolCallId"), TEXT_LIMIT))
                } else {
                    event_id.clone()
                };
                let complete = kind == "agent.tool_completed";
                let previous = tools.get(&tool_key).copied();
                // Completion events carry no args; keep the started title (tool + target).
                let summary = tool_summary(p.get("args"));
                let title = previous.map_or_else(
                    || {
                        let tool = non_empty(text(p.get("toolName"), TEXT_LIMIT))
                            .unwrap_or_else(|| "Tool".into());
                        if summary.is_empty() {
                            tool
                        } else {
                            format!("{tool} {summary}")
                        }
                    },
                    |index| result[index].title.clone(),
                );
                let body = if complete {
                    collapse_lines(
                        &content_text(p.get("result")),
                        COLLAPSE_LINES,
                        COLLAPSE_CHARACTERS,
                    )
                } else if summary.is_empty() {
                    format_fields(&fields(p.get("args"), 8))
                } else {
                    String::new()
                };
                let status = match (complete, truthy(p.get("isError"))) {
                    (false, _) => "running",
                    (true, true) => "failed",
                    (true, false) => "completed",
                };
                let mut tool = entry(EntryKind::Tool, &title, body, status);
                tool.id = previous.map_or_else(
                    || format!("tool:{}:{tool_key}", string(scope.get("projectId"))),
                    |index| result[index].id.clone(),
                );
                match previous {
                    Some(index) => result[index].assign(tool),
                    None => {
                        tools.insert(tool_key, result.len());
                        result.push(tool);
                    }
                }
            }
            "agent.terminal_receipt" => {
                let accepted = p.get("accepted").and_then(JsValue::as_bool) == Some(true);
                let failed = record(p.get("effect"))
                    .get("state")
                    .and_then(JsValue::as_str)
                    == Some("failed");
                let status = non_empty(text(p.get("status"), TEXT_LIMIT)).unwrap_or_else(|| {
                    if accepted {
                        "accepted"
                    } else if p.get("retryAllowed").and_then(JsValue::as_bool) == Some(true) {
                        "correction requested"
                    } else {
                        "rejected"
                    }
                    .into()
                });
                let mut summary = JsObject::new();
                summary.set("status", status.as_str().into());
                for key in ["accepted", "retryAllowed", "effect"] {
                    summary.set(key, p.get(key).cloned().unwrap_or(JsValue::Null));
                }
                let detail = [
                    text(first([p.get("error"), p.get("message")]), 2000),
                    format_fields(&fields(Some(&JsValue::Object(summary)), 6)),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
                result.push(entry(
                    EntryKind::Receipt,
                    "HOST RECEIPT",
                    detail,
                    if failed { "failed" } else { &status },
                ));
            }
            "record.submitted"
                if p.get("requiredAuthority").and_then(JsValue::as_str) == Some("user") =>
            {
                let detail = [
                    text(p.get("summary"), 2000),
                    text(p.get("requestedAction"), 2000),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
                result.push(entry(
                    EntryKind::Approval,
                    "APPROVAL NEEDED",
                    detail,
                    "pending",
                ));
            }
            _ => {}
        }
    }
    bound_entries(&result)
}

/// `boundEntries`: the last 120 entries within 64000 units of text, newest first in the budget
/// (viewmodel §2.22).
pub fn bound_entries(entries: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    let mut left = 64_000;
    let mut result = Vec::new();
    for entry in entries[entries.len().saturating_sub(120)..].iter().rev() {
        if left == 0 {
            break;
        }
        let body = slice_to(&safe_text_str(&entry.text), left);
        left -= length(&body);
        result.push(TranscriptEntry {
            id: safe_text_str(&entry.id),
            title: text_str(&entry.title, TEXT_LIMIT),
            text: body,
            ..entry.clone()
        });
    }
    result.reverse();
    result
}

/// `mapTone`: the tone of a map state (viewmodel §2.23; `mapTone('idle')` is `Active`, an
/// upstream quirk kept per DECISIONS.md item 7). The TS patterns are ASCII with the non-unicode
/// `i` flag, which folds ASCII letters only.
pub fn map_tone(state: &str) -> MapTone {
    let state = state.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|word| state.contains(word));
    if any(&[
        "fail", "error", "reject", "cancel", "stop", "blocked", "lost",
    ]) {
        MapTone::Error
    } else if any(&["accept", "complete", "promoted", "passed", "closed"]) {
        MapTone::Success
    } else if any(&[
        "review",
        "postflight",
        "approval",
        "pending",
        "paus",
        "held",
        "awaiting",
        "planning",
        "draft",
    ]) {
        MapTone::Waiting
    } else if any(&[
        "running",
        "working",
        "active",
        "evaluating",
        "implementing",
        "leased",
        "starting",
        "idle",
    ]) {
        MapTone::Active
    } else {
        MapTone::Idle
    }
}

/// `mapGlyph`: the glyph of a map node by kind and state (viewmodel §2.24).
pub fn map_glyph(kind: &str, state: &str) -> &'static str {
    if let Some((glyph, _)) = map_root(kind) {
        return glyph;
    }
    match (kind, state) {
        ("worker", _) => "⚙",
        ("job", _) => "▸",
        (_, "promoted") => "★",
        _ => match map_tone(state) {
            MapTone::Success => "✓",
            MapTone::Active => "●",
            MapTone::Waiting => "◐",
            MapTone::Error => "✗",
            MapTone::Idle => "○",
        },
    }
}

/// `progressBar`: `▰`/`▱` cells plus ` done/total` (viewmodel §2.25). Callers pass
/// [`PROGRESS_CELLS`] where TS uses the default.
pub fn progress_bar(done: f64, total: f64, cells: usize) -> String {
    if total == 0.0 || total.is_nan() {
        return String::new();
    }
    // `Math.round` of a ratio in [0, 1]; a NaN ratio repeats nothing, as `'▰'.repeat(NaN)` does.
    let filled = (nosh_wire::js::min(1.0, nosh_wire::js::max(0.0, done / total)) * cells as f64
        + 0.5)
        .floor();
    let filled = if filled.is_nan() {
        None
    } else {
        Some(filled as usize)
    };
    format!(
        "{}{} {}/{}",
        "▰".repeat(filled.unwrap_or(0)),
        "▱".repeat(filled.map_or(0, |filled| cells - filled)),
        number_string(done),
        number_string(total)
    )
}

static SCORES: LazyLock<[(&str, Regex); 3]> = LazyLock::new(|| {
    ["score", "best", "baseline"].map(|key| {
        (
            key,
            Regex::new(&format!("{key} (-?[0-9.e-]+)")).expect("valid score pattern"),
        )
    })
});

/// `detailScore`: the number after `key ` in a map detail, to 4 significant figures, or `""`
/// (viewmodel §2.26).
fn detail_score(detail: &str, key: &str) -> String {
    let pattern = &SCORES
        .iter()
        .find(|(name, _)| *name == key)
        .expect("a known score key")
        .1;
    let value = pattern
        .captures(detail)
        .map_or(f64::NAN, |captures| js_number(&captures[1]));
    if value.is_finite() {
        number_string(nosh_wire::js::to_precision_number(value, 4))
    } else {
        String::new()
    }
}

static EXPERIMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("([0-9]+/[^{JS_SPACE}]+) experiments")).expect("valid experiments pattern")
});

/// The `climbLines` builder: the lines so far and the width they are cut to.
struct Climb {
    width: i64,
    lines: Vec<MapLine>,
}

impl Climb {
    fn push(&mut self, prefix: &str, glyph: &str, tone: MapTone, title: &str, full: &str) {
        let used = length(prefix) as i64 + if glyph.is_empty() { 0 } else { 2 };
        // A long live state ("implementing") yields to the title's minimum of 4 characters.
        let keep = (self.width - used - 5).max(1);
        let value = if length(full) as i64 > keep {
            slice_to(full, keep as usize - 1) + "…"
        } else {
            full.to_owned()
        };
        let room = (self.width - used - length(&value) as i64 - 1).max(4);
        let clipped = if length(title) as i64 > room {
            slice_to(title, room as usize - 1) + "…"
        } else {
            title.to_owned()
        };
        let pad = (length(&clipped) as i64 + 1).max(self.width - used - length(&value) as i64);
        self.lines.push(MapLine {
            prefix: prefix.to_owned(),
            glyph: glyph.to_owned(),
            tone,
            label: String::new(),
            title: pad_end(&clipped, pad as usize),
            state: value,
            detail: String::new(),
            heading: None,
        });
    }

    /// The spine continues with the last kept child; the others hang under the step they tried to
    /// beat.
    fn chain(&mut self, nodes: &[&JsObject], prefix: &str) {
        let next = nodes
            .iter()
            .rposition(|node| text(node.get("state"), TEXT_LIMIT) == "promoted");
        let hanging = format!("{prefix}{}", if next.is_some() { "│ " } else { "  " });
        for (index, node) in nodes.iter().enumerate() {
            if Some(index) != next {
                self.step(node, &hanging);
            }
        }
        if let Some(index) = next {
            self.step(nodes[index], prefix);
        }
    }

    fn step(&mut self, node: &JsObject, prefix: &str) {
        let state = text(node.get("state"), 40);
        let score = detail_score(&text(node.get("detail"), 200), "score");
        let shown = if !score.is_empty() {
            score
        } else if state == "failed" {
            "crash".into()
        } else {
            state.clone()
        };
        self.push(
            prefix,
            map_glyph("experiment", &state),
            map_tone(&state),
            &text(node.get("title"), 200),
            &shown,
        );
        self.chain(&experiments(node), prefix);
    }
}

/// An experiment node's experiment children.
fn experiments(node: &JsObject) -> Vec<&JsObject> {
    records(node.get("children"))
        .into_iter()
        .filter(|child| text(child.get("kind"), TEXT_LIMIT) == "experiment")
        .collect()
}

/// The `MapLine` with only a title.
fn title_line(tone: MapTone, label: &str, title: String, heading: Option<bool>) -> MapLine {
    MapLine {
        prefix: String::new(),
        glyph: String::new(),
        tone,
        label: label.to_owned(),
        title,
        state: String::new(),
        detail: String::new(),
        heading,
    }
}

/// `climbLines`: the sidebar climb graph of a `/research-map` response (viewmodel §2.27): the most
/// relevant Autoresearch (a live one, else the most recently updated) as a narrow chain. Callers
/// pass [`CLIMB_MAXIMUM`] where TS uses the default. `localeCompare` on `updatedAt` is a UTF-16
/// code-unit compare (DECISIONS.md item 7).
pub fn climb_lines(map: Option<&JsValue>, width: usize, maximum: usize) -> Vec<MapLine> {
    fn find<'a>(node: &'a JsObject, runs: &mut Vec<&'a JsObject>) {
        if text(node.get("kind"), TEXT_LIMIT) == "autoresearch" {
            runs.push(node);
        }
        for child in records(node.get("children")) {
            find(child, runs);
        }
    }
    let mut runs = Vec::new();
    for root in records(record(map).get("roots")) {
        find(root, &mut runs);
    }
    let live = runs
        .iter()
        .find(|node| map_tone(&text(node.get("state"), 40)) == MapTone::Active);
    let run = match live {
        Some(run) => *run,
        None => {
            let mut sorted = runs.clone();
            sorted.sort_by(|left, right| {
                let updated = |node: &JsObject| text(node.get("updatedAt"), TEXT_LIMIT);
                updated(right)
                    .encode_utf16()
                    .cmp(updated(left).encode_utf16())
            });
            match sorted.first() {
                Some(run) => *run,
                None => return Vec::new(),
            }
        }
    };
    let detail = text(run.get("detail"), 200);
    let state = text(run.get("state"), 40);
    let best = detail_score(&detail, "best");
    let summary = [
        non_empty(best).map(|best| format!("best {best}")),
        EXPERIMENTS
            .captures(&detail)
            .map(|captures| format!("{} exp", &captures[1])),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let mut climb = Climb {
        width: width as i64,
        lines: vec![
            title_line(map_tone(&state), "CLIMB", state, Some(true)),
            title_line(MapTone::Idle, "", summary, None),
        ],
    };
    climb.push(
        "",
        "◎",
        MapTone::Idle,
        "baseline",
        &detail_score(&detail, "baseline"),
    );
    climb.chain(&experiments(run), "");
    let mut lines = climb.lines;
    // Header lines clip to the width as well (chain lines are already exact).
    for line in &mut lines[..2] {
        let used = if line.label.is_empty() {
            0
        } else {
            length(&line.label) + 1
        };
        let room = width.saturating_sub(used).max(4);
        if length(&line.title) > room {
            line.title = slice_to(&line.title, room - 1) + "…";
        }
    }
    if lines.len() > maximum {
        let more = lines.len() - maximum + 1;
        lines.truncate(maximum.saturating_sub(1));
        lines.push(title_line(MapTone::Idle, "", format!("+{more} more"), None));
    }
    lines
}

const MERMAID_CLASSES: [&str; 7] = [
    "keep fill:#c9f2c9,stroke:#2e7d32,color:#000",
    "discard fill:#fbd5d5,stroke:#c62828,color:#000",
    "crash fill:#e0e0e0,stroke:#616161,stroke-dasharray:4 3,color:#000",
    "live fill:#fff2b3,stroke:#f9a825,color:#000",
    "blocked fill:#ffd8a8,stroke:#e65100,color:#000",
    "done fill:#d6e6ff,stroke:#1565c0,color:#000",
    "idle fill:#f5f5f5,stroke:#9e9e9e,color:#000",
];

/// `mermaidClass`: the class of a node by state (viewmodel §2.28).
fn mermaid_class(state: &str) -> &'static str {
    if state == "promoted" {
        "keep"
    } else if state.contains("rejected") {
        "discard"
    } else if state.contains("fail") {
        "crash"
    } else if state.contains("blocked") || state.contains("held") {
        "blocked"
    } else {
        match map_tone(state) {
            MapTone::Success => "done",
            MapTone::Active => "live",
            MapTone::Waiting => "blocked",
            MapTone::Error => "discard",
            MapTone::Idle => "idle",
        }
    }
}

static LONG_DECIMALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([0-9]\.[0-9]{5})[0-9]+").expect("valid decimals pattern"));

fn mermaid_escape(value: &str) -> String {
    value
        .replace('"', "#quot;")
        .replace('<', "#lt;")
        .replace('>', "#gt;")
}

fn mermaid_visit(
    node: &JsObject,
    parent: Option<&str>,
    lines: &mut Vec<String>,
    count: &mut usize,
) {
    let id = format!("n{count}");
    *count += 1;
    let kind = text(node.get("kind"), TEXT_LIMIT);
    let state = text(node.get("state"), 40);
    let root = match map_root(&kind) {
        Some((_, label)) => label.to_owned(),
        None if kind == "worker" || kind == "job" => kind.to_uppercase(),
        None => String::new(),
    };
    let head = [root, text(node.get("title"), 60)]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    let detail = LONG_DECIMALS
        .replace_all(&text(node.get("detail"), 200), "${1}")
        .into_owned();
    let small = [state.clone(), detail]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    lines.push(format!(
        "  {id}[\"{}<br/><small>{}</small>\"]:::{}",
        mermaid_escape(&head),
        mermaid_escape(&small),
        mermaid_class(&state)
    ));
    if let Some(parent) = parent {
        lines.push(format!("  {parent} --> {id}"));
    }
    // An Autoresearch's top-level experiments start from its baseline.
    let base = if kind == "autoresearch" {
        let base = format!("{id}base");
        lines.push(format!("  {base}([\"baseline\"]):::idle"));
        lines.push(format!("  {id} --> {base}"));
        base
    } else {
        id.clone()
    };
    for child in records(node.get("children")) {
        let target = if text(child.get("kind"), TEXT_LIMIT) == "experiment" {
            &base
        } else {
            &id
        };
        mermaid_visit(child, Some(target), lines, count);
    }
}

/// `researchMapMermaid`: the `/map mermaid` chart text, one node per map node and an edge per
/// parent → child, experiments by lineage (viewmodel §2.28, §2.29).
pub fn research_map_mermaid(map: Option<&JsValue>) -> String {
    let mut lines = vec!["flowchart LR".to_owned()];
    let mut count = 0;
    for root in records(record(map).get("roots")) {
        mermaid_visit(root, None, &mut lines, &mut count);
    }
    lines.extend(
        MERMAID_CLASSES
            .iter()
            .map(|class| format!("  classDef {class}")),
    );
    lines.join("\n")
}

/// `MAP_ROOTS[kind]` as `(glyph, label)`; `None` for every other kind, including
/// `Object.prototype` names (deviation 2 of viewmodel §7.6).
pub fn map_root(kind: &str) -> Option<(&'static str, &'static str)> {
    MAP_ROOTS
        .iter()
        .find(|(name, _, _)| *name == kind)
        .map(|(_, glyph, label)| (*glyph, *label))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probes from tui-viewmodel.md §2.4, tui-transport.md §4.1 and the controller tests.
    #[test]
    fn safe_text_matches_the_node_probes() {
        let probes = [
            ("hello\x1b]52;secret\x07", "hello"),
            (
                "before\x1b]52;c;secret\x07\x1b[31mred\x1b[0m\rafter\u{202e}",
                "beforeredafter",
            ),
            ("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\", "link"),
            ("\x1b]0;title with spaces\x07x", "itle with spacesx"),
            ("a\x1b]52;c;se cret\x07b", "a;se cretb"),
            ("\x1b[38:2:1:2:3mx", ":2:1:2:3mx"),
            ("\x1b[?25lx", "x"),
            ("\u{9b}31mx", "x"),
            ("\x1b(Bx", "x"),
            ("\x1bPx", "x"),
            ("x\x1b[1;2;3Hy", "xy"),
            ("x\x1b7y", "x"),
            ("\x1b[12345mZ", "mZ"),
            (
                "a\tb\nc\0d\x7fe\u{85}f\u{2066}g\u{200e}",
                "a\tb\ncdefg\u{200e}",
            ),
        ];
        for (input, expected) in probes {
            assert_eq!(safe_text_str(input), expected, "{input:?}");
        }
        assert_eq!(safe_text(None), "undefined");
        assert_eq!(safe_text(Some(&JsValue::Null)), "null");
        assert_eq!(safe_text(Some(&JsValue::Number(1e21))), "1e+21");
        assert_eq!(safe_text_str(&"x".repeat(20_000)).len(), 16_000);
        assert_eq!(length(&safe_text_str(&"é😀".repeat(9_000))), 16_000);
    }

    #[test]
    fn record_returns_objects_or_empty() {
        let object = crate::js::js_json_parse(r#"{"a":1}"#).expect("json");
        assert_eq!(record(Some(&object)).len(), 1);
        for other in ["null", "[1]", "\"s\"", "3", "true"] {
            let value = crate::js::js_json_parse(other).expect("json");
            assert!(record(Some(&value)).is_empty(), "{other}");
        }
        assert!(record(None).is_empty());
    }
}
