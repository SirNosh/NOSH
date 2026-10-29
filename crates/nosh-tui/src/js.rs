//! JavaScript semantics for the terminal UI: the ordered JSON value [`JsValue`] / [`JsObject`],
//! `JSON.parse` with V8's error texts, `JSON.stringify` (compact and `(value, null, 2)`), and the
//! string helpers that `controller.ts`, `view-model.ts`, `client.ts` and `ui.ts` rely on.
//!
//! Specified by INDEX.md §2 item 2; tui-controller.md §1, §16.5, §16.6 and addenda CC-2, CC-3;
//! tui-viewmodel.md §1, §7.3 and addenda VA-1, VA-3; tui-transport.md addendum TA-2.
//!
//! Key order is JavaScript property order: canonical array-index keys (`"0"` to `"4294967294"`)
//! come first in ascending numeric order, every other key keeps its insertion order, and setting an
//! existing key keeps its position. `serde_json`'s `preserve_order` feature is never enabled: the
//! [`Deserialize`] impl keeps document order without it.
//!
//! Known divergences: Rust strings cannot hold lone surrogates, so a UTF-16 slice that splits a
//! surrogate pair yields U+FFFD, and JSON text holding a lone `\uD800` escape or a number beyond the
//! f64 range is a parse error here while V8 accepts it. Nesting depth is unbounded, as in V8
//! (transport §11.9 item 5): the daemon's research map nests two levels per experiment generation,
//! so serde_json's recursion limit is off and parsing and stringifying grow the stack on demand.

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

pub use nosh_wire::js::{is_js_whitespace, js_number, length, number_string, slice_to};

/// The largest safe integer, `Number.MAX_SAFE_INTEGER` (2^53 - 1).
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A JSON value as JavaScript holds it after `JSON.parse`: numbers are f64, objects keep JavaScript
/// property order. `undefined` has no variant; an absent value is `None` in an `Option<&JsValue>`.
///
/// `PartialEq` is structural and order-sensitive (two objects with the same entries in a different
/// order differ); it is not `===`, which [`strict_equal`] reproduces.
#[derive(Clone, Debug, PartialEq)]
pub enum JsValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsValue>),
    Object(JsObject),
}

/// A plain JavaScript object: entries in JavaScript property order (see the module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JsObject {
    entries: Vec<(String, JsValue)>,
}

/// `Some(index)` when `key` is a canonical array index (`"0"`, or a decimal without leading zeros
/// up to 4294967294), which V8 enumerates before every other key.
fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[0] == b'0' && bytes.len() > 1 {
        return None;
    }
    let value: u64 = key.parse().ok()?;
    u32::try_from(value)
        .ok()
        .filter(|index| *index <= 4_294_967_294)
}

impl JsObject {
    /// `{}`.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The number of own properties.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True for `{}`.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `object[key]` for an own property.
    pub fn get(&self, key: &str) -> Option<&JsValue> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// Mutable access to an own property.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut JsValue> {
        self.entries
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// `Object.hasOwn(object, key)`.
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// `object[key] = value`: an existing key keeps its position and takes the new value; a new
    /// array-index key goes among the index keys in ascending order; any other new key goes last.
    /// Object spread, `Object.fromEntries` and `JSON.parse` duplicates follow the same rule.
    pub fn set(&mut self, key: impl Into<String>, value: JsValue) {
        let key = key.into();
        if let Some(slot) = self.get_mut(&key) {
            *slot = value;
            return;
        }
        match array_index(&key) {
            Some(index) => {
                let at = self
                    .entries
                    .iter()
                    .position(|(name, _)| array_index(name).is_none_or(|other| other > index))
                    .unwrap_or(self.entries.len());
                self.entries.insert(at, (key, value));
            }
            None => self.entries.push((key, value)),
        }
    }

    /// `delete object[key]`: removes the property and returns its value.
    pub fn remove(&mut self, key: &str) -> Option<JsValue> {
        let at = self.entries.iter().position(|(name, _)| name == key)?;
        Some(self.entries.remove(at).1)
    }

    /// `object = { ...object, ...other }`: every entry of `other`, in its order, through [`set`].
    ///
    /// [`set`]: JsObject::set
    pub fn spread(&mut self, other: &JsObject) {
        for (key, value) in other.iter() {
            self.set(key, value.clone());
        }
    }

    /// `Object.entries(object)`, in JavaScript property order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &JsValue)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// `Object.keys(object)`.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    /// `Object.values(object)`.
    pub fn values(&self) -> impl Iterator<Item = &JsValue> {
        self.entries.iter().map(|(_, value)| value)
    }
}

impl FromIterator<(String, JsValue)> for JsObject {
    /// `Object.fromEntries(entries)`.
    fn from_iter<I: IntoIterator<Item = (String, JsValue)>>(entries: I) -> Self {
        let mut object = JsObject::new();
        for (key, value) in entries {
            object.set(key, value);
        }
        object
    }
}

impl IntoIterator for JsObject {
    type Item = (String, JsValue);
    type IntoIter = std::vec::IntoIter<(String, JsValue)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl JsValue {
    /// `value[key]` when `value` is a plain object; `None` for every other value (including
    /// arrays, whose index properties are read with [`JsValue::as_array`]).
    pub fn get(&self, key: &str) -> Option<&JsValue> {
        self.as_object()?.get(key)
    }

    /// The string, when `typeof value === 'string'`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsValue::String(text) => Some(text),
            _ => None,
        }
    }

    /// The number, when `typeof value === 'number'`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsValue::Number(number) => Some(*number),
            _ => None,
        }
    }

    /// The boolean, when `typeof value === 'boolean'`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsValue::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    /// The elements, when `Array.isArray(value)`.
    pub fn as_array(&self) -> Option<&[JsValue]> {
        match self {
            JsValue::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The object, when `value` is a non-null, non-array object.
    pub fn as_object(&self) -> Option<&JsObject> {
        match self {
            JsValue::Object(object) => Some(object),
            _ => None,
        }
    }

    /// `value === null`.
    pub fn is_null(&self) -> bool {
        matches!(self, JsValue::Null)
    }

    /// `value !== null && typeof value === 'object' && !Array.isArray(value)`.
    pub fn is_object(&self) -> bool {
        matches!(self, JsValue::Object(_))
    }
}

impl From<&str> for JsValue {
    fn from(text: &str) -> Self {
        JsValue::String(text.to_owned())
    }
}

impl From<String> for JsValue {
    fn from(text: String) -> Self {
        JsValue::String(text)
    }
}

impl From<f64> for JsValue {
    fn from(number: f64) -> Self {
        JsValue::Number(number)
    }
}

impl From<bool> for JsValue {
    fn from(flag: bool) -> Self {
        JsValue::Bool(flag)
    }
}

impl From<Vec<JsValue>> for JsValue {
    fn from(items: Vec<JsValue>) -> Self {
        JsValue::Array(items)
    }
}

impl From<JsObject> for JsValue {
    fn from(object: JsObject) -> Self {
        JsValue::Object(object)
    }
}

impl From<serde_json::Value> for JsValue {
    /// A `serde_json::Value` (whose objects iterate in sorted order) as JavaScript would hold it:
    /// numbers become f64 and keys are re-inserted through [`JsObject::set`].
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => JsValue::Null,
            serde_json::Value::Bool(flag) => JsValue::Bool(flag),
            serde_json::Value::Number(number) => JsValue::Number(number.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(text) => JsValue::String(text),
            serde_json::Value::Array(items) => {
                JsValue::Array(items.into_iter().map(JsValue::from).collect())
            }
            serde_json::Value::Object(map) => JsValue::Object(
                map.into_iter()
                    .map(|(key, item)| (key, JsValue::from(item)))
                    .collect(),
            ),
        }
    }
}

impl From<JsValue> for serde_json::Value {
    /// The value as `serde_json` holds it, for decoding with serde derives. Integral numbers within
    /// the safe range become JSON integers (so `u64`/`i64` fields decode), non-finite numbers become
    /// `null` as in `JSON.stringify`, and object key order is lost (serde_json maps are sorted).
    fn from(value: JsValue) -> Self {
        match value {
            JsValue::Null => serde_json::Value::Null,
            JsValue::Bool(flag) => serde_json::Value::Bool(flag),
            JsValue::Number(number) => number_to_serde(number),
            JsValue::String(text) => serde_json::Value::String(text),
            JsValue::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(serde_json::Value::from).collect())
            }
            JsValue::Object(object) => serde_json::Value::Object(
                object
                    .into_iter()
                    .map(|(key, item)| (key, serde_json::Value::from(item)))
                    .collect(),
            ),
        }
    }
}

fn is_integral(number: f64) -> bool {
    number.is_finite() && number.trunc() == number && number.abs() <= MAX_SAFE_INTEGER
}

fn number_to_serde(number: f64) -> serde_json::Value {
    if !number.is_finite() {
        serde_json::Value::Null
    } else if is_integral(number) {
        if number >= 0.0 {
            serde_json::Value::from(number as u64)
        } else {
            serde_json::Value::from(number as i64)
        }
    } else {
        serde_json::Number::from_f64(number).map_or(serde_json::Value::Null, Into::into)
    }
}

impl Serialize for JsValue {
    /// Serializes in JavaScript property order. Integral numbers within the safe range are written
    /// as integers and non-finite numbers as `null`; for byte-exact `JSON.stringify` output (number
    /// spelling such as `1e+21`) use [`json_stringify`] instead.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            JsValue::Null => serializer.serialize_unit(),
            JsValue::Bool(flag) => serializer.serialize_bool(*flag),
            JsValue::Number(number) => {
                if !number.is_finite() {
                    serializer.serialize_unit()
                } else if is_integral(*number) {
                    serializer.serialize_i64(*number as i64)
                } else {
                    serializer.serialize_f64(*number)
                }
            }
            JsValue::String(text) => serializer.serialize_str(text),
            JsValue::Array(items) => {
                let mut sequence = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    sequence.serialize_element(item)?;
                }
                sequence.end()
            }
            JsValue::Object(object) => object.serialize(serializer),
        }
    }
}

impl Serialize for JsObject {
    /// Serializes the entries in JavaScript property order.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

struct JsValueVisitor;

impl<'de> Visitor<'de> for JsValueVisitor {
    type Value = JsValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, flag: bool) -> Result<JsValue, E> {
        Ok(JsValue::Bool(flag))
    }

    fn visit_i64<E>(self, number: i64) -> Result<JsValue, E> {
        Ok(JsValue::Number(number as f64))
    }

    fn visit_u64<E>(self, number: u64) -> Result<JsValue, E> {
        Ok(JsValue::Number(number as f64))
    }

    fn visit_f64<E>(self, number: f64) -> Result<JsValue, E> {
        Ok(JsValue::Number(number))
    }

    fn visit_str<E>(self, text: &str) -> Result<JsValue, E> {
        Ok(JsValue::String(text.to_owned()))
    }

    fn visit_string<E>(self, text: String) -> Result<JsValue, E> {
        Ok(JsValue::String(text))
    }

    fn visit_unit<E>(self) -> Result<JsValue, E> {
        Ok(JsValue::Null)
    }

    fn visit_none<E>(self) -> Result<JsValue, E> {
        Ok(JsValue::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<JsValue, D::Error> {
        JsValue::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<JsValue, A::Error> {
        let mut items = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(item) = sequence.next_element()? {
            items.push(item);
        }
        Ok(JsValue::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsValue, A::Error> {
        let mut object = JsObject::new();
        while let Some((key, value)) = map.next_entry::<String, JsValue>()? {
            object.set(key, value);
        }
        Ok(JsValue::Object(object))
    }
}

impl<'de> Deserialize<'de> for JsValue {
    /// Keeps document order (serde visits map entries in order) under the [`JsObject::set`] rule;
    /// integers convert with `as f64`, which rounds as `JSON.parse` does.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        grow_stack(|| deserializer.deserialize_any(JsValueVisitor))
    }
}

/// `String(value)`; `None` is `undefined`. Numbers use ECMAScript `Number#toString`, arrays join
/// their elements with `,` (null elements become empty), objects give `[object Object]`.
pub fn string(value: Option<&JsValue>) -> String {
    match value {
        None => "undefined".into(),
        Some(JsValue::Null) => "null".into(),
        Some(JsValue::Bool(flag)) => flag.to_string(),
        Some(JsValue::Number(number)) => number_string(*number),
        Some(JsValue::String(text)) => text.clone(),
        Some(JsValue::Array(items)) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    string(Some(item))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(JsValue::Object(_)) => "[object Object]".into(),
    }
}

/// `Boolean(value)`; `None` is `undefined`. False for `undefined`, `null`, `false`, `0`, `-0`,
/// `NaN` and `""`; true for everything else, including `[]` and `{}`.
pub fn truthy(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Null) => false,
        Some(JsValue::Bool(flag)) => *flag,
        Some(JsValue::Number(number)) => *number != 0.0 && !number.is_nan(),
        Some(JsValue::String(text)) => !text.is_empty(),
        Some(JsValue::Array(_) | JsValue::Object(_)) => true,
    }
}

/// `a === b`: primitives compare by value (`NaN !== NaN`), arrays and objects are never strictly
/// equal to a separately held value, and `undefined` (`None`) equals only `undefined`.
pub fn strict_equal(a: Option<&JsValue>, b: Option<&JsValue>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(JsValue::Null), Some(JsValue::Null)) => true,
        (Some(JsValue::Bool(x)), Some(JsValue::Bool(y))) => x == y,
        (Some(JsValue::Number(x)), Some(JsValue::Number(y))) => x == y,
        (Some(JsValue::String(x)), Some(JsValue::String(y))) => x == y,
        _ => false,
    }
}

/// `Number(value)` (ECMAScript `ToNumber`): `undefined` is NaN, `null` 0, booleans 1/0, strings
/// through `StringToNumber` ([`js_number`]), arrays through `String(value)`, objects NaN.
pub fn to_number(value: Option<&JsValue>) -> f64 {
    match value {
        None => f64::NAN,
        Some(JsValue::Null) => 0.0,
        Some(JsValue::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(JsValue::Number(number)) => *number,
        Some(JsValue::String(text)) => js_number(text),
        Some(array @ JsValue::Array(_)) => js_number(&string(Some(array))),
        Some(JsValue::Object(_)) => f64::NAN,
    }
}

/// `Number.isSafeInteger(number)`: finite, integral, and `|number| <= 2^53 - 1`.
pub fn is_safe_integer(number: f64) -> bool {
    is_integral(number)
}

/// `JSON.parse(text)` keeping document order (addendum CC-3). On failure the error is V8's
/// `SyntaxError` message from `nosh_agent::js_json`, e.g. `Unexpected end of JSON input`.
pub fn js_json_parse(text: &str) -> Result<JsValue, String> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    JsValue::deserialize(&mut deserializer)
        .and_then(|value| deserializer.end().map(|()| value))
        .map_err(|serde_error| match nosh_agent::js_json::parse(text) {
            Err(syntax) => syntax.message,
            Ok(_) => serde_error.to_string(),
        })
}

/// `JSON.stringify(value)`: compact, JavaScript property order, numbers through
/// `Number#toString` (`1e+21`, `-0` as `0`), non-finite numbers as `null`.
pub fn json_stringify(value: &JsValue) -> String {
    let mut out = String::new();
    write_json(&mut out, value, false, 0);
    out
}

/// `JSON.stringify(value, null, 2)`: two-space indentation, `"key": value`, `{}` and `[]` for
/// empty containers.
pub fn json_stringify_pretty(value: &JsValue) -> String {
    let mut out = String::new();
    write_json(&mut out, value, true, 0);
    out
}

fn write_indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Runs one level of a recursion over nested JSON on a fresh stack segment when less than 64 KiB
/// of the current one is left, so depth is bounded by memory rather than the 1 MiB main thread.
fn grow_stack<R>(level: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, level)
}

fn write_json(out: &mut String, value: &JsValue, pretty: bool, depth: usize) {
    grow_stack(|| write_json_level(out, value, pretty, depth));
}

fn write_json_level(out: &mut String, value: &JsValue, pretty: bool, depth: usize) {
    match value {
        JsValue::Null => out.push_str("null"),
        JsValue::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        JsValue::Number(number) => {
            if number.is_finite() {
                out.push_str(&number_string(*number));
            } else {
                out.push_str("null");
            }
        }
        JsValue::String(text) => write_json_string(out, text),
        JsValue::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if pretty {
                    write_indent(out, depth + 1);
                }
                write_json(out, item, pretty, depth + 1);
            }
            if pretty {
                write_indent(out, depth);
            }
            out.push(']');
        }
        JsValue::Object(object) => {
            if object.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in object.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if pretty {
                    write_indent(out, depth + 1);
                }
                write_json_string(out, key);
                out.push(':');
                if pretty {
                    out.push(' ');
                }
                write_json(out, item, pretty, depth + 1);
            }
            if pretty {
                write_indent(out, depth);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(text)` for a well-formed string: serde_json escapes exactly the same characters
/// (`"`, `\`, `\b \f \n \r \t`, other C0 controls as lowercase `\u00XX`).
fn write_json_string(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).expect("a string always serializes"));
}

/// `text.trim()` with JavaScript's whitespace set (which adds U+FEFF and lacks U+0085 compared
/// with Rust's `str::trim`).
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// `text.split(/\s+/)` with JavaScript's `\s`: `" a"` gives `["", "a"]`, `"a "` gives
/// `["a", ""]`, `""` gives `[""]`.
pub fn js_split_ws(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if !is_js_whitespace(character) {
            continue;
        }
        parts.push(&text[start..index]);
        let mut end = index + character.len_utf8();
        while let Some(&(next, following)) = characters.peek() {
            if !is_js_whitespace(following) {
                break;
            }
            end = next + following.len_utf8();
            characters.next();
        }
        start = end;
    }
    parts.push(&text[start..]);
    parts
}

/// `text.replace(/\s+/g, ' ')` with JavaScript's `\s`: every whitespace run (newlines included)
/// becomes one space; leading and trailing runs stay as one space each.
pub fn js_collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for character in text.chars() {
        if is_js_whitespace(character) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(character);
            in_run = false;
        }
    }
    out
}

/// The characters `encodeURIComponent` leaves unescaped: ASCII alphanumerics and `-_.!~*'()`.
const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// `encodeURIComponent(text)`: every other character as its UTF-8 bytes in uppercase `%XX`.
pub fn encode_uri_component(text: &str) -> String {
    utf8_percent_encode(text, URI_COMPONENT).to_string()
}

/// Node's `stripVTControlCharacters` pattern, translated for the `regex` crate (viewmodel §2.4).
static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"[\x1B\x{9B}][\[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?(?:\x07|\x1B\\|\x{9C}))|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]))",
    )
    .expect("valid ANSI pattern")
});

/// `stripVTControlCharacters(text)`: ANSI escape sequences removed (`safeText`, and pasted text).
pub fn strip_vt_controls(text: &str) -> Cow<'_, str> {
    ANSI.replace_all(text, "")
}

/// `text.toLowerCase()`: the full Unicode default case mapping (final sigma included), which
/// `str::to_lowercase` implements identically.
pub fn js_lowercase(text: &str) -> String {
    text.to_lowercase()
}

/// `text.slice(-count)`: the last `count` UTF-16 units. As in JavaScript, `slice(-0)` is the whole
/// string, so `count == 0` returns `text` unchanged.
pub fn slice_from_end(text: &str, count: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if count == 0 || count >= units.len() {
        return text.to_owned();
    }
    String::from_utf16_lossy(&units[units.len() - count..])
}

/// `text.slice(start, end)` for non-negative indices in UTF-16 units: both are clamped to the
/// length, and `start >= end` gives `""`.
pub fn slice_range(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = end.min(units.len());
    if start >= end {
        return String::new();
    }
    String::from_utf16_lossy(&units[start..end])
}

/// `text.indexOf(needle, from)` in UTF-16 units; `None` for JavaScript's `-1`. `from` is clamped
/// to the length, so an empty needle is found at `min(from, length)`.
pub fn index_of(text: &str, needle: &str, from: usize) -> Option<usize> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let wanted: Vec<u16> = needle.encode_utf16().collect();
    let from = from.min(units.len());
    if wanted.len() > units.len() {
        return None;
    }
    (from..=units.len() - wanted.len()).find(|&index| units[index..index + wanted.len()] == wanted)
}

/// `text.lastIndexOf(needle, from)` for a one-character needle, in UTF-16 units; `None` for `-1`.
/// The search starts at `from` (clamped to the last possible position) and goes backwards.
pub fn last_index_of_char(text: &str, needle: char, from: usize) -> Option<usize> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut buffer = [0u16; 2];
    let wanted = needle.encode_utf16(&mut buffer);
    if wanted.len() > units.len() {
        return None;
    }
    let start = from.min(units.len() - wanted.len());
    (0..=start)
        .rev()
        .find(|&index| units[index..index + wanted.len()] == *wanted)
}

/// `text.padEnd(width)`: spaces appended until the UTF-16 length is `width` (no-op when longer).
pub fn pad_end(text: &str, width: usize) -> String {
    let mut out = text.to_owned();
    for _ in length(text)..width {
        out.push(' ');
    }
    out
}

/// `Date.now()`: milliseconds since the Unix epoch, integral as in JavaScript.
pub fn date_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// `path.resolve(path)`: absolute against the current directory, with `.` and `..` collapsed
/// lexically and duplicate separators removed; `path_resolve("")` is the current directory.
pub fn path_resolve(path: &str) -> PathBuf {
    let absolute = if path.is_empty() {
        std::env::current_dir()
    } else {
        std::path::absolute(path)
    }
    .unwrap_or_else(|_| PathBuf::from(path));
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.parent().is_some() {
                    resolved.pop();
                }
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// `readFile(path, 'utf8')`: the bytes decoded as UTF-8 with U+FFFD for invalid sequences, keeping
/// a leading BOM as Node does.
pub fn read_text_lossy(path: &Path) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(path)?).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> JsValue {
        js_json_parse(text).expect("valid JSON")
    }

    fn keys(value: &JsValue) -> Vec<&str> {
        value.as_object().expect("an object").keys().collect()
    }

    #[test]
    fn objects_keep_javascript_property_order() {
        let value = parse(
            r#"{"b":1,"2":0,"a":2,"1":3,"b":4,"01":5,"-1":6,"4294967295":7,"4294967294":8,"10":9}"#,
        );
        assert_eq!(
            keys(&value),
            [
                "1",
                "2",
                "10",
                "4294967294",
                "b",
                "a",
                "01",
                "-1",
                "4294967295"
            ]
        );
        // A duplicate keeps its first position and takes the last value.
        assert_eq!(value.get("b"), Some(&JsValue::Number(4.0)));

        let mut object = JsObject::new();
        object.set("z", JsValue::Null);
        object.set("5", JsValue::Null);
        object.set("a", JsValue::Null);
        object.set("0", JsValue::Null);
        object.set("z", JsValue::Bool(true));
        assert_eq!(object.keys().collect::<Vec<_>>(), ["0", "5", "z", "a"]);
        assert_eq!(object.get("z"), Some(&JsValue::Bool(true)));
        assert_eq!(object.remove("5"), Some(JsValue::Null));
        assert_eq!(object.len(), 3);

        let mut spread = parse(r#"{"command":"x","k":1}"#)
            .as_object()
            .cloned()
            .expect("object");
        spread.spread(parse(r#"{"k":2,"new":3}"#).as_object().expect("object"));
        assert_eq!(
            json_stringify(&JsValue::Object(spread)),
            r#"{"command":"x","k":2,"new":3}"#
        );
    }

    #[test]
    fn parse_errors_carry_v8_texts() {
        assert_eq!(
            js_json_parse("").unwrap_err(),
            "Unexpected end of JSON input"
        );
        assert_eq!(
            js_json_parse("<html>x</html>").unwrap_err(),
            r#"Unexpected token '<', "<html>x</html>" is not valid JSON"#
        );
        assert_eq!(
            js_json_parse("\u{feff}{\"a\":1}").unwrap_err(),
            "Unexpected token '\u{feff}', \"\u{feff}{\"a\":1}\" is not valid JSON"
        );
    }

    #[test]
    fn numbers_parse_and_print_as_javascript() {
        assert_eq!(
            parse("9007199254740993"),
            JsValue::Number(9_007_199_254_740_992.0)
        );
        assert_eq!(parse("1.0"), JsValue::Number(1.0));
        let value = parse(r#"[1e21,-0,100.0,1.5e-7,0.1,-12,1e-7,123456789012345680000]"#);
        assert_eq!(
            json_stringify(&value),
            "[1e+21,0,100,1.5e-7,0.1,-12,1e-7,123456789012345680000]"
        );
        assert_eq!(json_stringify(&JsValue::Number(-0.0)), "0");
        assert_eq!(json_stringify(&JsValue::Number(f64::NAN)), "null");
        assert_eq!(json_stringify(&JsValue::Number(f64::INFINITY)), "null");
        assert_eq!(
            json_stringify_pretty(&parse(r#"{"n":1e21,"z":-0}"#)),
            "{\n  \"n\": 1e+21,\n  \"z\": 0\n}"
        );
    }

    /// Goldens from tui-controller.md §15.0 and tui-workspace.md §3.11.
    #[test]
    fn stringify_matches_the_spec_goldens() {
        let body = parse(
            r#"{"expectedVersion":4,"action":"pause","mode":"safe","projectId":"p","idempotencyKey":"tui-51684924-fccb-4bc8-b3de-1f2444e2ccfd"}"#,
        );
        assert_eq!(
            json_stringify_pretty(&body),
            "{\n  \"expectedVersion\": 4,\n  \"action\": \"pause\",\n  \"mode\": \"safe\",\n  \"projectId\": \"p\",\n  \"idempotencyKey\": \"tui-51684924-fccb-4bc8-b3de-1f2444e2ccfd\"\n}"
        );
        let projects = parse(r#"{"projects":[{"projectId":"p","repositoryRoot":"/p"}]}"#);
        assert_eq!(
            json_stringify_pretty(&projects),
            "{\n  \"projects\": [\n    {\n      \"projectId\": \"p\",\n      \"repositoryRoot\": \"/p\"\n    }\n  ]\n}"
        );
        let fingerprint = JsValue::Array(vec![
            "p".into(),
            "/jobs/j1/cancel".into(),
            parse(r#"{"projectId":"p","idempotencyKey":"tui-1","z":1,"a":[1,"x"]}"#),
        ]);
        assert_eq!(
            json_stringify(&fingerprint),
            r#"["p","/jobs/j1/cancel",{"projectId":"p","idempotencyKey":"tui-1","z":1,"a":[1,"x"]}]"#
        );
        let form: JsObject = [
            ("path".to_owned(), JsValue::from(r#"C:\research\x "q""#)),
            ("workingTitle".to_owned(), JsValue::from("Study é")),
            ("createRepository".to_owned(), JsValue::Bool(true)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            json_stringify(&JsValue::Object(form)),
            r#"{"path":"C:\\research\\x \"q\"","workingTitle":"Study é","createRepository":true}"#
        );
        assert_eq!(json_stringify_pretty(&parse("{}")), "{}");
        assert_eq!(json_stringify_pretty(&parse("[]")), "[]");
        assert_eq!(
            json_stringify_pretty(&parse(r#"{"a":[],"b":{}}"#)),
            "{\n  \"a\": [],\n  \"b\": {}\n}"
        );
        assert_eq!(
            json_stringify(&JsValue::from("\u{1}\u{7f}\"\\\n\t/é\u{2028}")),
            "\"\\u0001\u{7f}\\\"\\\\\\n\\t/é\u{2028}\""
        );
    }

    #[test]
    fn serde_round_trips_keep_order() {
        let value = parse(r#"{"b":[1,2.5,null,true],"a":{"y":"x","1":-3}}"#);
        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"b":[1,2.5,null,true],"a":{"1":-3,"y":"x"}}"#
        );
        let decoded: serde_json::Value = value.clone().into();
        assert_eq!(decoded["b"][0].as_u64(), Some(1));
        assert_eq!(decoded["a"]["1"].as_i64(), Some(-3));
        let back = JsValue::from(serde_json::json!({"b": 1, "10": 2, "2": 3}));
        assert_eq!(keys(&back), ["2", "10", "b"]);
    }

    #[test]
    fn coercions_match_javascript() {
        let s = |text: &str| string(Some(&parse(text)));
        assert_eq!(s(r#"[1,null,"a",[2,[3]],true]"#), "1,,a,2,3,true");
        assert_eq!(s("1e21"), "1e+21");
        assert_eq!(s("-0"), "0");
        assert_eq!(s("0.30000000000000004"), "0.30000000000000004");
        assert_eq!(s(r#"{"a":1}"#), "[object Object]");
        assert_eq!(s("null"), "null");
        assert_eq!(string(None), "undefined");

        for falsy in ["null", "false", "0", "-0", r#""""#] {
            assert!(!truthy(Some(&parse(falsy))), "{falsy}");
        }
        assert!(!truthy(None) && !truthy(Some(&JsValue::Number(f64::NAN))));
        for value in ["true", "1", "-0.5", r#""x""#, "[]", "{}"] {
            assert!(truthy(Some(&parse(value))), "{value}");
        }

        let n = |text: &str| to_number(Some(&parse(text)));
        assert!(to_number(None).is_nan());
        assert_eq!(n("null"), 0.0);
        assert_eq!(n("true"), 1.0);
        assert_eq!(n(r#""""#), 0.0);
        assert_eq!(n(r#"" 12 ""#), 12.0);
        assert_eq!(n(r#""0x10""#), 16.0);
        assert_eq!(n("[]"), 0.0);
        assert_eq!(n("[5]"), 5.0);
        assert!(n("[1,2]").is_nan());
        assert!(n("{}").is_nan());

        assert!(strict_equal(Some(&parse("1")), Some(&parse("1.0"))));
        assert!(!strict_equal(Some(&parse("1")), Some(&parse(r#""1""#))));
        assert!(!strict_equal(Some(&parse("[1]")), Some(&parse("[1]"))));
        assert!(strict_equal(None, None) && !strict_equal(None, Some(&JsValue::Null)));
        assert!(!strict_equal(
            Some(&JsValue::Number(f64::NAN)),
            Some(&JsValue::Number(f64::NAN))
        ));
    }

    #[test]
    fn safe_integers() {
        assert!(is_safe_integer(MAX_SAFE_INTEGER));
        assert!(is_safe_integer(-MAX_SAFE_INTEGER));
        assert!(is_safe_integer(0.0) && is_safe_integer(13.0));
        assert!(!is_safe_integer(9_007_199_254_740_992.0));
        assert!(!is_safe_integer(1.5));
        assert!(!is_safe_integer(f64::NAN) && !is_safe_integer(f64::INFINITY));
        assert_eq!(js_number("0x10"), 16.0);
        assert!(js_number("-0x1").is_nan());
    }

    /// Probes from tui-controller.md §1.3 and §1.4.
    #[test]
    fn whitespace_and_uri_helpers() {
        let probe = "  a \u{a0}b\u{85}c\u{feff}d";
        assert_eq!(js_split_ws(js_trim(probe)), ["a", "b\u{85}c", "d"]);
        assert_eq!(js_split_ws(" a"), ["", "a"]);
        assert_eq!(js_split_ws("a "), ["a", ""]);
        assert_eq!(js_split_ws(""), [""]);
        assert_eq!(js_split_ws("  "), ["", ""]);
        assert_eq!(js_split_ws("a \t\n b"), ["a", "b"]);
        assert_eq!(js_trim("\u{feff} x \u{3000}"), "x");
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
        assert_eq!(js_collapse_ws("a  b\n c"), "a b c");
        assert_eq!(js_collapse_ws("  lead\t"), " lead ");
        assert_eq!(
            encode_uri_component("a b/c?d#e&f=g'h(i)j*k!l~m_n.o-pé"),
            "a%20b%2Fc%3Fd%23e%26f%3Dg'h(i)j*k!l~m_n.o-p%C3%A9"
        );
        assert_eq!(js_lowercase("ΣΑΣ"), "σας");
    }

    #[test]
    fn utf16_helpers() {
        assert_eq!(slice_from_end("abcdef", 2), "ef");
        assert_eq!(slice_from_end("abc", 0), "abc");
        assert_eq!(slice_from_end("abc", 10), "abc");
        assert_eq!(slice_from_end("a😀b", 2), "\u{fffd}b");
        assert_eq!(slice_range("abcdef", 1, 3), "bc");
        assert_eq!(slice_range("abc", 2, 1), "");
        assert_eq!(slice_range("abc", 1, 10), "bc");
        assert_eq!(index_of("hello", "l", 0), Some(2));
        assert_eq!(index_of("hello", "l", 3), Some(3));
        assert_eq!(index_of("hello", "z", 0), None);
        assert_eq!(index_of("😀ab", "a", 0), Some(2));
        assert_eq!(index_of("abc", "", 5), Some(3));
        assert_eq!(last_index_of_char("a b c d", ' ', 4), Some(3));
        assert_eq!(last_index_of_char("a b c d", ' ', 100), Some(5));
        assert_eq!(last_index_of_char("abc", ' ', 2), None);
        assert_eq!(last_index_of_char("", ' ', 0), None);
        assert_eq!(pad_end("ab", 4), "ab  ");
        assert_eq!(pad_end("abcdef", 4), "abcdef");
        assert_eq!(length("a😀"), 3);
    }

    /// V8 texts probed with Node 22.23.1 (`JSON.parse(text)` → `error.message`).
    #[test]
    fn parse_errors_match_node_22() {
        let probes = [
            ("  ", "Unexpected end of JSON input"),
            ("tru", "Unexpected end of JSON input"),
            (
                "{",
                "Expected property name or '}' in JSON at position 1 (line 1 column 2)",
            ),
            (
                "{\"a\":1,}",
                "Expected double-quoted property name in JSON at position 7 (line 1 column 8)",
            ),
            (
                "{\"a\" 1}",
                "Expected ':' after property name in JSON at position 5 (line 1 column 6)",
            ),
            ("[1,]", "Unexpected token ']', \"[1,]\" is not valid JSON"),
            (
                "[1",
                "Expected ',' or ']' after array element in JSON at position 2 (line 1 column 3)",
            ),
            (
                "1 2",
                "Unexpected non-whitespace character after JSON at position 2 (line 1 column 3)",
            ),
            (
                "01",
                "Unexpected number in JSON at position 1 (line 1 column 2)",
            ),
            (
                "-",
                "No number after minus sign in JSON at position 1 (line 1 column 2)",
            ),
            (
                "1.",
                "Unterminated fractional number in JSON at position 2 (line 1 column 3)",
            ),
            (
                "\"abc",
                "Unterminated string in JSON at position 4 (line 1 column 5)",
            ),
            (
                "\"a\nb\"",
                "Bad control character in string literal in JSON at position 2 (line 1 column 3)",
            ),
            (
                "\"\\x\"",
                "Bad escaped character in JSON at position 2 (line 1 column 3)",
            ),
            ("NaN", "\"NaN\" is not valid JSON"),
            (
                &"x".repeat(40),
                "Unexpected token 'x', \"xxxxxxxxxx\"... is not valid JSON",
            ),
        ];
        for (text, message) in probes {
            assert_eq!(js_json_parse(text).unwrap_err(), message, "{text:?}");
        }
    }

    /// `JSON.parse` rounds decimal literals correctly (serde_json's `float_roundtrip`); goldens are
    /// `String(JSON.parse(text))` from Node 22.23.1.
    #[test]
    fn number_literals_round_like_v8() {
        let probes = [
            ("18446744073709551615", "18446744073709552000"),
            ("2.2250738585072011e-308", "2.225073858507201e-308"),
            ("8.98846567431158e307", "8.98846567431158e+307"),
            ("4.9406564584124654e-324", "5e-324"),
            ("0.1e1", "1"),
            ("123.456e-789", "0"),
            (
                "1.00000000000000011102230246251565404236316680908203125",
                "1",
            ),
            ("9007199254740993.0000000001", "9007199254740994"),
        ];
        for (text, expected) in probes {
            assert_eq!(string(Some(&parse(text))), expected, "{text}");
        }
    }

    /// A research-map lineage of 1000 generations nests 2000 levels, past serde_json's default
    /// limit of 128; `JSON.parse` takes it.
    #[test]
    fn parses_and_stringifies_deep_nesting() {
        let depth = 2000;
        let text = format!(
            "{}{}",
            "[{\"c\":".repeat(depth / 2),
            "0".to_owned() + &"}]".repeat(depth / 2)
        );
        let value = js_json_parse(&text).expect("deep JSON parses");
        assert_eq!(json_stringify(&value), text);
        assert_eq!(
            js_json_parse(&text[..text.len() - 1]).unwrap_err(),
            "Expected ',' or ']' after array element in JSON at position 8000 (line 1 column 8001)"
        );
    }

    /// `JSON.stringify` goldens from Node 22.23.1.
    #[test]
    fn stringify_numbers_escapes_and_nesting_like_v8() {
        let numbers = [
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1e20, "100000000000000000000"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (0.1 + 0.2, "0.30000000000000004"),
            (-1e-7, "-1e-7"),
            (123e-20, "1.23e-18"),
            (9_007_199_254_740_992.0, "9007199254740992"),
        ];
        for (number, expected) in numbers {
            assert_eq!(json_stringify(&JsValue::Number(number)), expected);
        }
        assert_eq!(
            json_stringify(&JsValue::from("\u{0}\u{8}\u{b}\u{c}\u{1f}\u{80}\"\\/")),
            "\"\\u0000\\b\\u000b\\f\\u001f\u{80}\\\"\\\\/\""
        );
        assert_eq!(
            json_stringify_pretty(&parse(r#"[[1,[2]],{"a":{}},[],{"b":[{}]}]"#)),
            "[\n  [\n    1,\n    [\n      2\n    ]\n  ],\n  {\n    \"a\": {}\n  },\n  [],\n  {\n    \"b\": [\n      {}\n    ]\n  }\n]"
        );
        // A duplicate key keeps its first position and takes the whole last value.
        assert_eq!(
            json_stringify(&parse(r#"{"a":{"x":1,"y":2},"b":0,"a":{"y":3,"x":4}}"#)),
            r#"{"a":{"y":3,"x":4},"b":0}"#
        );
        // The serde path writes non-finite numbers as null too.
        let odd = JsValue::Array(vec![
            JsValue::Number(f64::NAN),
            JsValue::Number(f64::NEG_INFINITY),
        ]);
        assert_eq!(
            serde_json::to_string(&odd).expect("serializes"),
            "[null,null]"
        );
    }

    /// `Object.keys(JSON.parse(text))` from Node 22.23.1: numeric-looking keys that are not
    /// canonical array indices keep insertion order.
    #[test]
    fn only_canonical_indices_move_first() {
        let value = parse(
            r#"{"1e3":0,"+1":0," 1":0,"1.0":0,"b":0,"2":0,"00":0,"0":0,"4294967295":0,"4294967294":0}"#,
        );
        assert_eq!(
            keys(&value),
            [
                "0",
                "2",
                "4294967294",
                "1e3",
                "+1",
                " 1",
                "1.0",
                "b",
                "00",
                "4294967295"
            ]
        );
    }

    /// `String(Number(text))` goldens from Node 22.23.1.
    #[test]
    fn to_number_of_strings_matches_v8() {
        let probes = [
            ("  0x1A  ", "26"),
            (".5", "0.5"),
            ("5.", "5"),
            ("+.5", "0.5"),
            ("1.e5", "100000"),
            ("1e3", "1000"),
            ("Infinity", "Infinity"),
            ("-Infinity", "-Infinity"),
            ("infinity", "NaN"),
            ("1_000", "NaN"),
            ("1e1000", "Infinity"),
            ("0b101", "5"),
            ("0o17", "15"),
            ("\u{feff}5\n", "5"),
            ("-0x1", "NaN"),
            ("0x", "NaN"),
            ("00012", "12"),
            ("1e", "NaN"),
            (" ", "0"),
            ("\u{a0}12\u{3000}", "12"),
            ("\u{85}12", "NaN"),
        ];
        for (text, expected) in probes {
            let value = JsValue::from(text);
            assert_eq!(number_string(to_number(Some(&value))), expected, "{text:?}");
        }
        assert_eq!(
            string(Some(&parse(r#"[1,[2,[null,[]]],{},true]"#))),
            "1,2,,,[object Object],true"
        );
        assert_eq!(string(Some(&parse("[[null]]"))), "");
        assert_eq!(
            string(Some(&parse("123456789.123456789"))),
            "123456789.12345679"
        );
    }

    #[test]
    fn paths_and_clock() {
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(path_resolve(""), cwd);
        assert_eq!(path_resolve("a/../b/./c"), cwd.join("b").join("c"));
        assert!(date_now() > 1.7e12 && date_now().fract() == 0.0);
    }
}
