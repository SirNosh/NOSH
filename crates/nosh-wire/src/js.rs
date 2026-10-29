//! JavaScript value semantics the TypeScript implementation relies on, for JSON values.

use serde_json::Value;

/// `String(value)`; `None` is `undefined`.
pub fn string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => crate::canonical_json(&Value::Number(n.clone())),
        Some(Value::String(s)) => s.clone(),
        // Array.prototype.join: null and undefined elements become "".
        Some(Value::Array(items)) => items
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
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `Boolean(value)`; `None` is `undefined`.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `a === b` for JSON primitives (numbers compare by value, so `1` equals `1.0`); objects and
/// arrays are never strictly equal to anything parsed separately.
pub fn strict_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::Array(_) | Value::Object(_)), _)
        | (_, Some(Value::Array(_) | Value::Object(_))) => false,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// `value.length`: UTF-16 code units.
pub fn length(value: &str) -> usize {
    value.encode_utf16().count()
}

/// `value.slice(0, end)` by UTF-16 code units. A surrogate pair split by `end` leaves a lone
/// surrogate in JavaScript, which Rust strings cannot hold; it becomes U+FFFD, as JavaScript's own
/// `toWellFormed()` would make it.
pub fn slice_to(value: &str, end: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().take(end).collect();
    String::from_utf16_lossy(&units)
}

/// `Math.max(a, b)`: NaN if either is NaN (Rust's `f64::max` would return the other).
pub fn max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// `Math.min(a, b)`: NaN if either is NaN.
pub fn min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// `array.sort((a, b) => compare(a, b))` as V8 performs it, including for comparators that return
/// NaN (treated as "not less"). Below 64 elements V8's TimSort is one run-detection pass plus a
/// binary insertion sort, reproduced here; longer arrays use a stable sort, which agrees with V8
/// whenever the comparator is consistent.
pub fn sort_by_number<T>(items: &mut [T], compare: impl Fn(&T, &T) -> f64) {
    let less = |a: &T, b: &T| compare(a, b) < 0.0;
    let n = items.len();
    if n < 2 {
        return;
    }
    if n >= 64 {
        items.sort_by(|a, b| {
            compare(a, b)
                .partial_cmp(&0.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        return;
    }
    // CountAndMakeRun: the leading ascending or strictly descending run.
    let descending = less(&items[1], &items[0]);
    let mut run = 2;
    while run < n {
        let ends = if descending {
            !less(&items[run], &items[run - 1])
        } else {
            less(&items[run], &items[run - 1])
        };
        if ends {
            break;
        }
        run += 1;
    }
    if descending {
        items[..run].reverse();
    }
    // BinaryInsertionSort of the rest into the run.
    for start in run..n {
        let (mut left, mut right) = (0, start);
        while left < right {
            let mid = left + (right - left) / 2;
            if less(&items[start], &items[mid]) {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        items[left..=start].rotate_right(1);
    }
}

/// `String(number)`.
pub fn number_string(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        string(Some(&Value::from(value)))
    }
}

/// `Number(text)` for the decimal forms a regex such as `-?[\d.e-]+` admits: NaN when JavaScript
/// would not parse it either.
pub fn parse_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

/// JavaScript's `String.prototype.trim` whitespace and line terminators.
pub fn is_js_whitespace(character: char) -> bool {
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

/// `Number(text)` for a string: JavaScript's `StringToNumber`.
pub fn js_number(text: &str) -> f64 {
    let text = text.trim_matches(is_js_whitespace);
    if text.is_empty() {
        return 0.0;
    }
    let radix = match text.get(..2) {
        Some("0x" | "0X") => 16,
        Some("0o" | "0O") => 8,
        Some("0b" | "0B") => 2,
        _ => 10,
    };
    if radix != 10 {
        let digits = &text[2..];
        if digits.is_empty() {
            return f64::NAN;
        }
        return digits
            .chars()
            .try_fold(0.0, |value: f64, digit| {
                digit
                    .to_digit(radix)
                    .map(|digit| value * f64::from(radix) + f64::from(digit))
            })
            .unwrap_or(f64::NAN);
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    static DECIMAL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$").expect("pattern compiles")
    });
    if DECIMAL.is_match(text) {
        text.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `value.toFixed(digits)` (digits ≤ 20): the exact decimal expansion rounded half away from
/// zero, as ECMAScript picks "the larger n" on a tie.
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return number_string(value);
    }
    // 1100 fractional digits hold any f64's exact expansion.
    let exact = format!("{:.1100}", value.abs());
    let (whole, fraction) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut kept: Vec<u8> = whole.bytes().chain(fraction.bytes().take(digits)).collect();
    if fraction
        .as_bytes()
        .get(digits)
        .is_some_and(|digit| *digit >= b'5')
    {
        let mut index = kept.len();
        loop {
            if index == 0 {
                kept.insert(0, b'1');
                break;
            }
            index -= 1;
            if kept[index] == b'9' {
                kept[index] = b'0';
            } else {
                kept[index] += 1;
                break;
            }
        }
    }
    let split = kept.len() - digits;
    let mut text = String::from_utf8(kept[..split].to_vec()).expect("digits");
    if digits > 0 {
        text.push('.');
        text.push_str(std::str::from_utf8(&kept[split..]).expect("digits"));
    }
    // (-0.00001).toFixed(2) is "-0.00": the sign is kept for any negative x.
    if value < 0.0 {
        text.insert(0, '-');
    }
    text
}

/// `Number(value.toPrecision(precision))`: `value` rounded to `precision` significant digits.
pub fn to_precision_number(value: f64, precision: usize) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let exponent = value.abs().log10().floor() as i32;
    let digits = precision as i32 - 1 - exponent;
    if digits >= 0 {
        parse_number(&to_fixed(value, digits as usize))
    } else {
        let scale = 10f64.powi(-digits);
        parse_number(&to_fixed(value / scale, 0)) * scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Outputs of Node for `toFixed`, `Number(toPrecision)`, and `String`.
    #[test]
    fn number_formatting_matches_javascript() {
        assert_eq!(to_fixed(0.03125, 4), "0.0313");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(-0.12345, 4), "-0.1235");
        assert_eq!(to_fixed(-0.00001, 2), "-0.00");
        assert_eq!(to_fixed(9.99995, 4), "10.0000");
        assert_eq!(to_fixed(99.5, 0), "100");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_precision_number(0.123456, 4), 0.1235);
        assert_eq!(to_precision_number(123456.0, 4), 123500.0);
        assert_eq!(number_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(number_string(f64::NAN), "NaN");
        assert!(parse_number("1.2.3").is_nan());
        assert_eq!(parse_number("-0.5"), -0.5);
    }

    #[test]
    fn string_and_truthiness_match_javascript() {
        let s = |v: Value| string(Some(&v));
        assert_eq!(s(json!([1, null, "a", [2, [3]], true])), "1,,a,2,3,true");
        assert_eq!(s(json!(1.5e21)), "1.5e+21");
        assert_eq!(s(json!({ "a": 1 })), "[object Object]");
        assert_eq!(string(None), "undefined");
        for falsy in [
            None,
            Some(json!(null)),
            Some(json!(false)),
            Some(json!(0)),
            Some(json!(0.0)),
            Some(json!("")),
        ] {
            assert!(!truthy(falsy.as_ref()), "{falsy:?}");
        }
        for value in [
            json!(true),
            json!(1),
            json!(-0.5),
            json!("x"),
            json!([]),
            json!({}),
        ] {
            assert!(truthy(Some(&value)), "{value}");
        }
    }

    #[test]
    fn numbers_read_as_javascript_reads_them() {
        for (text, expected) in [
            ("", 0.0),
            ("  12  ", 12.0),
            ("0x10", 16.0),
            ("0b101", 5.0),
            ("0o17", 15.0),
            ("1e3", 1000.0),
            ("-2.5", -2.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("+7", 7.0),
            ("-Infinity", f64::NEG_INFINITY),
        ] {
            assert_eq!(js_number(text), expected, "{text:?}");
        }
        for text in [
            "abc", "1x", "-0x10", "0x", "inf", "NaN", "1e", "1_000", "infinity",
        ] {
            assert!(js_number(text).is_nan(), "{text:?}");
        }
    }

    /// Orders produced by Node (V8) for `.sort((a, b) => b - a)` on arrays containing NaN.
    #[test]
    fn sorts_like_v8_even_with_nan() {
        let cases: [(&[f64], &[usize]); 5] = [
            (&[3.0, f64::NAN, 1.0, 2.0], &[0, 1, 3, 2]),
            (&[f64::NAN, 5.0, 1.0, f64::NAN, 2.0], &[0, 1, 2, 3, 4]),
            (&[1.0, 2.0, 3.0, f64::NAN], &[2, 1, 0, 3]),
            (
                &[5.0, 4.0, 3.0, 2.0, 1.0, f64::NAN, 0.0],
                &[0, 1, 2, 3, 4, 5, 6],
            ),
            (&[2.0, 2.0, 1.0, f64::NAN, 3.0, 1.0], &[0, 1, 2, 3, 4, 5]),
        ];
        for (values, expected) in cases {
            let mut indices: Vec<usize> = (0..values.len()).collect();
            sort_by_number(&mut indices, |a, b| values[*b] - values[*a]);
            assert_eq!(indices, expected, "{values:?}");
        }
    }

    #[test]
    fn strict_equality_and_utf16_lengths() {
        assert!(strict_equal(Some(&json!(1)), Some(&json!(1.0))));
        assert!(!strict_equal(Some(&json!(1)), Some(&json!("1"))));
        assert!(!strict_equal(Some(&json!([1])), Some(&json!([1]))));
        assert!(strict_equal(None, None) && !strict_equal(None, Some(&json!(null))));
        assert_eq!(length("a😀"), 3);
        assert_eq!(slice_to("ab😀c", 3), "ab\u{fffd}");
        assert_eq!(slice_to("ab😀c", 4), "ab😀");
        assert_eq!(slice_to("abc", 10), "abc");
    }
}
