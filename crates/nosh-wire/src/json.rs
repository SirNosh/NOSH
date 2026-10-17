//! RFC 8785 (JCS) canonical JSON and its SHA-256 digest, matching `packages/wire/src/json.ts`
//! (`json-canonicalize` plus `@noble/hashes`).

use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Canonical JSON: object keys sorted by UTF-16 code units, numbers formatted as ECMAScript
/// `Number.prototype.toString`, strings escaped as `JSON.stringify` does.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

/// `sha256:<64 hex>` over the UTF-8 bytes of [`canonical_json`].
pub fn sha256(value: &Value) -> String {
    let digest = Sha256::digest(canonical_json(value).as_bytes());
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&ecmascript_number(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

/// Gives numbers JavaScript's `JSON.parse` semantics: every number is an f64, so `1.0` is the
/// integer `1` and integers beyond 2^53 round. Integral values are stored as integers so
/// `as_u64`/`as_i64` work as callers expect.
pub fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(n) => {
            let f = n
                .as_f64()
                .expect("serde_json numbers convert to f64 without arbitrary_precision");
            // Below 2^63 every integral f64 converts to i64 exactly.
            if f.fract() == 0.0 && f.abs() < 9_223_372_036_854_775_808.0 {
                #[allow(clippy::cast_possible_truncation)]
                let integer = f as i64;
                *n = integer.into();
            } else if let Some(float) = Number::from_f64(f) {
                *n = float;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        Value::Object(map) => map.values_mut().for_each(normalize_numbers),
        _ => {}
    }
}

/// JavaScript holds every JSON number as an f64, so integers are rounded the same way first.
fn ecmascript_number(n: &Number) -> String {
    let value = n
        .as_f64()
        .expect("serde_json numbers convert to f64 without arbitrary_precision");
    ryu_js::Buffer::new().format(value).to_owned()
}

/// serde_json escapes exactly the characters `JSON.stringify` does for well-formed strings:
/// `"`, `\`, and U+0000..U+001F (short forms for \b \f \n \r \t, lowercase `\u00xx` otherwise).
fn write_string(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).expect("strings always serialize"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::vectors;

    #[test]
    fn matches_typescript_canonicalizer() {
        let canonical = vectors()["canonical"]
            .as_array()
            .expect("canonical vectors");
        assert!(!canonical.is_empty());
        for case in canonical {
            let input = case["input"].as_str().unwrap();
            let value: Value = serde_json::from_str(input).unwrap();
            assert_eq!(
                canonical_json(&value),
                case["canonical"].as_str().unwrap(),
                "{input}"
            );
            assert_eq!(sha256(&value), case["sha256"].as_str().unwrap(), "{input}");
        }
    }

    #[test]
    fn numbers_follow_json_parse() {
        let mut value: Value =
            serde_json::from_str("[1.0, -0.0, 2.5, 9007199254740993, 18446744073709551615, -3]")
                .unwrap();
        normalize_numbers(&mut value);
        assert_eq!(value[0].as_u64(), Some(1));
        assert_eq!(value[1].as_i64(), Some(0));
        assert_eq!(value[2].as_f64(), Some(2.5));
        assert_eq!(value[3].as_u64(), Some(9_007_199_254_740_992));
        // 2^64 is beyond i64, so it stays a float, as large as JavaScript's value.
        assert_eq!(value[4].as_f64(), Some(18_446_744_073_709_551_616.0));
        assert_eq!(value[5].as_i64(), Some(-3));
        assert_eq!(
            canonical_json(&value),
            "[1,0,2.5,9007199254740992,18446744073709552000,-3]"
        );
    }

    #[test]
    fn sorts_keys_by_utf16_not_utf8() {
        // U+1F600 is a surrogate pair (0xD83D...), so in UTF-16 it sorts before U+FFFD; UTF-8 orders them the other way.
        let value: Value = serde_json::from_str(r#"{"�":1,"😀":2}"#).unwrap();
        assert_eq!(canonical_json(&value), r#"{"😀":2,"�":1}"#);
    }
}
