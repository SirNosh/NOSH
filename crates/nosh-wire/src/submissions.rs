//! Submission helpers from `packages/wire/src/submissions.ts`.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

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
        && TERMINAL_RESPONSE_STATUSES.contains(&crate::js::string(map.get("status")).as_str())
}

fn uri(name: &str) -> String {
    schema_uri(name)
}

/// `schemaUri` (common.ts): the `$schema` URI of the named record.
pub fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

/// `schemaDocumentPath` (submission-tools.ts): the named schema's JSON Schema file. TypeScript
/// resolves it inside its wire package; the Rust daemon's vendored copies live in `schema_dir`.
pub fn schema_document_path(schema_dir: &Path, name: &str) -> PathBuf {
    schema_dir.join(format!("{name}.v1.schema.json"))
}

/// `episodeTypeForRole` (submission-tools.ts): the Episode type a role's THREAD_STEP must
/// produce. Roles contain underscores, but a ref allows exactly one.
pub fn episode_type_for_role(role: &str) -> String {
    format!("episode_{}", role.replace('_', "-"))
}

/// The record schemas (names) each typed submission tool accepts (`submissionToolSchemas`),
/// vendored from the TypeScript wire package by `scripts/sync-schemas.mjs`.
pub fn submission_tool_schemas() -> &'static BTreeMap<String, Vec<String>> {
    static TOOLS: LazyLock<BTreeMap<String, Vec<String>>> = LazyLock::new(|| {
        serde_json::from_str(include_str!(
            "../../../conformance/schemas/submission-tools.json"
        ))
        .expect("submission-tools.json")
    });
    &TOOLS
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
}
