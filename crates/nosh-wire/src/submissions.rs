//! Submission helpers from `packages/wire/src/submissions.ts`.

use serde_json::Value;

const TERMINAL_SCHEMAS: [&str; 7] = [
    "general-worker-completion",
    "librarian-completion",
    "task-failure",
    "mission-director-cycle",
    "research-director-cycle",
    "review-verdict",
    "experiment-proposal",
];

const TERMINAL_RESPONSE_STATUSES: [&str; 4] = ["completed", "failed", "cancelled", "superseded"];

/// Whether `record` ends a task (`isTaskTerminalRecord`): one of the completion records, or a
/// response envelope whose status is terminal.
pub fn is_task_terminal_record(record: &Value) -> bool {
    let Value::Object(map) = record else {
        return false;
    };
    let Some(schema) = map.get("$schema").and_then(Value::as_str) else {
        return false;
    };
    if TERMINAL_SCHEMAS.iter().any(|name| schema == uri(name)) {
        return true;
    }
    // TypeScript compares `String(value.status)`, so e.g. ["completed"] also counts.
    schema == uri("response-envelope")
        && TERMINAL_RESPONSE_STATUSES.contains(&js_string(map.get("status")).as_str())
}

fn uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

/// JavaScript's `String(value)`.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => crate::json::canonical_json(&Value::Number(n.clone())),
        Some(Value::String(s)) => s.clone(),
        // Array.prototype.join: null and undefined elements become "".
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_string(Some(item))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agrees_with_typescript() {
        let cases = crate::registry::vectors()["taskTerminal"]
            .as_array()
            .expect("taskTerminal vectors");
        assert!(!cases.is_empty());
        for case in cases {
            assert_eq!(
                is_task_terminal_record(&case["input"]),
                case["terminal"].as_bool().unwrap(),
                "{}",
                case["input"]
            );
        }
    }

    #[test]
    fn js_string_matches_javascript() {
        let s = |text: &str| js_string(Some(&serde_json::from_str(text).unwrap()));
        assert_eq!(s(r#"[1, null, "a", [2, [3]], true]"#), "1,,a,2,3,true");
        assert_eq!(s("1.5e21"), "1.5e+21");
        assert_eq!(s(r#"{"a":1}"#), "[object Object]");
        assert_eq!(js_string(None), "undefined");
    }
}
