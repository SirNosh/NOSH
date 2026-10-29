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

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::sync::LazyLock;

use regex::Regex;

use crate::js::{JsObject, JsValue, length, slice_to, string};

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
        todo!()
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
        todo!()
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
    todo!()
}

/// `fields`: up to `maximum` labelled scalar fields of an object, in JS property order
/// (viewmodel §2.8). Callers pass [`FIELDS_MAXIMUM`] where TS uses the default.
pub fn fields(value: Option<&JsValue>, maximum: usize) -> Vec<ViewField> {
    todo!()
}

/// `formatFields`: `label: value` lines joined with `\n` (viewmodel §2.10).
pub fn format_fields(items: &[ViewField]) -> String {
    todo!()
}

/// `modelName`: `provider/id` from `provider ?? modelProvider` and `id ?? modelId`, or `""`
/// (viewmodel §2.11).
pub fn model_name(value: Option<&JsValue>) -> String {
    todo!()
}

/// `entityRow`: the inspector card of one entity (viewmodel §2.12). `index` names id-less rows
/// `<section>-<index>`; callers pass 0 where TS uses the default.
pub fn entity_row(section: &str, value: Option<&JsValue>, index: usize) -> ViewRow {
    todo!()
}

/// `responseRows`: the rows of one daemon response for a section (viewmodel §2.13).
pub fn response_rows(section: &str, data: Option<&JsValue>) -> Vec<ViewRow> {
    todo!()
}

/// `boundRows`: at most `maximum` rows, 16 fields each, 48000 units of field values in total
/// (viewmodel §2.14). Callers pass [`BOUND_ROWS_MAXIMUM`] where TS uses the default.
pub fn bound_rows(rows: &[ViewRow], maximum: usize) -> Vec<ViewRow> {
    todo!()
}

/// `rawDetail`: the clipped `JSON.stringify(value, null, 2)` shown for staged reviews and raw
/// details (viewmodel §2.15).
pub fn raw_detail(value: Option<&JsValue>) -> String {
    todo!()
}

/// `collapseLines`: at most `maximum` lines and `characters` units (viewmodel §2.20). Callers pass
/// [`COLLAPSE_LINES`] and [`COLLAPSE_CHARACTERS`] where TS uses the defaults.
pub fn collapse_lines(value: &str, maximum: usize, characters: usize) -> String {
    todo!()
}

/// `eventEntries`: transcript entries from durable events (each a `NoshEvent` object), already
/// bounded by `boundEntries` (viewmodel §2.21).
pub fn event_entries(events: &[JsValue]) -> Vec<TranscriptEntry> {
    todo!()
}

/// `boundEntries`: the last 120 entries within 64000 units of text (viewmodel §2.22).
pub fn bound_entries(entries: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    todo!()
}

/// `mapTone`: the tone of a map state (viewmodel §2.23; `mapTone('idle')` is `Active`, an
/// upstream quirk kept per DECISIONS.md item 7).
pub fn map_tone(state: &str) -> MapTone {
    todo!()
}

/// `mapGlyph`: the glyph of a map node by kind and state (viewmodel §2.24).
pub fn map_glyph(kind: &str, state: &str) -> &'static str {
    todo!()
}

/// `progressBar`: `▰`/`▱` cells plus ` done/total` (viewmodel §2.25). Callers pass
/// [`PROGRESS_CELLS`] where TS uses the default.
pub fn progress_bar(done: f64, total: f64, cells: usize) -> String {
    todo!()
}

/// `climbLines`: the sidebar climb graph of a `/research-map` response (viewmodel §2.27).
/// Callers pass [`CLIMB_MAXIMUM`] where TS uses the default.
pub fn climb_lines(map: Option<&JsValue>, width: usize, maximum: usize) -> Vec<MapLine> {
    todo!()
}

/// `researchMapMermaid`: the `/map mermaid` chart text (viewmodel §2.28, §2.29).
pub fn research_map_mermaid(map: Option<&JsValue>) -> String {
    todo!()
}

/// `MAP_ROOTS[kind]` as `(glyph, label)`; `None` for every other kind, including
/// `Object.prototype` names (deviation 2 of viewmodel §7.6).
pub fn map_root(kind: &str) -> Option<(&'static str, &'static str)> {
    todo!()
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
