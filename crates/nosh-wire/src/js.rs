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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
