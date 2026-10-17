//! Typed submissions and handoff teachbacks, as in `packages/agent-runtime`.

use nosh_wire::js::{strict_equal, string};
use nosh_wire::{submission_tool_schemas, validate_record};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Validates records submitted through typed tools; the first invalid attempt per key may retry.
#[derive(Default)]
pub struct StructuredSubmissionGate {
    invalid_attempts: HashMap<String, u32>,
}

impl StructuredSubmissionGate {
    /// `{ ok: true, record }` or `{ ok: false, retryAllowed, errors }`.
    pub fn submit(&mut self, tool: &str, attempt_key: &str, record: &Value) -> Value {
        let uri = match record {
            Value::Object(map) if map.contains_key("$schema") => string(map.get("$schema")),
            _ => String::new(),
        };
        let allowed = submission_tool_schemas().get(tool).is_some_and(|names| {
            names
                .iter()
                .any(|name| uri == format!("https://nosh.dev/schemas/{name}/v1"))
        });
        let result = if allowed {
            validate_record(&uri, record).map_err(|issues| serde_json::to_value(issues.iter().map(|i| json!({ "pointer": i.pointer, "code": i.code, "message": i.message })).collect::<Vec<_>>()).expect("issues serialize"))
        } else {
            let shown = if uri.is_empty() {
                "<missing>"
            } else {
                uri.as_str()
            };
            Err(
                json!([{ "pointer": "/$schema", "code": "wrong_tool", "message": format!("Schema {shown} is not accepted by {tool}") }]),
            )
        };
        match result {
            Ok(value) => {
                self.invalid_attempts.remove(attempt_key);
                json!({ "ok": true, "record": value })
            }
            Err(errors) => {
                let count = self
                    .invalid_attempts
                    .entry(attempt_key.to_owned())
                    .or_insert(0);
                *count += 1;
                json!({ "ok": false, "retryAllowed": *count == 1, "errors": errors })
            }
        }
    }

    pub fn supported_schemas(&self) -> Vec<String> {
        nosh_wire::schema_uris()
    }
}

/// Checks a receiving agent's teachback against the handoff it took over: `{ ok: true }` or
/// `{ ok: false, conflicts }` naming every mismatched field.
pub fn validate_teachback(state: &Value, teachback: &Value) -> Value {
    let mut conflicts: Vec<&str> = Vec::new();
    let differ = |a: &str, b: &str| !strict_equal(teachback.get(a), state.get(b));
    if differ("handoffId", "handoffId") {
        conflicts.push("handoffId");
    }
    if differ("logicalOwnerId", "logicalOwnerId") {
        conflicts.push("logicalOwnerId");
    }
    if !same_goal_stack(&teachback["understoodGoalStack"], &state["goalStack"]) {
        conflicts.push("understoodGoalStack");
    }
    if !same_versions(&teachback["observedVersions"], &state["observedVersions"]) {
        conflicts.push("observedVersions");
    }
    if differ("observedBranchHead", "branchHead") {
        conflicts.push("observedBranchHead");
    }
    if !same_set(&teachback["acknowledgedDefectIds"], &state["defectIds"]) {
        conflicts.push("acknowledgedDefectIds");
    }
    if !same_set(&teachback["acknowledgedBlockerIds"], &state["blockerIds"]) {
        conflicts.push("acknowledgedBlockerIds");
    }
    let selected = teachback.get("selectedNextNodeId");
    if !selected.is_some_and(Value::is_null)
        && !state["readyNodeIds"]
            .as_array()
            .is_some_and(|ready| ready.iter().any(|id| strict_equal(Some(id), selected)))
    {
        conflicts.push("selectedNextNodeId");
    }
    let decision = teachback.get("decision");
    let decided = |name: &str| strict_equal(decision, Some(&json!(name)));
    let has_conflicts = teachback["conflicts"]
        .as_array()
        .is_some_and(|c| !c.is_empty());
    if decided("accepted") && has_conflicts {
        conflicts.push("decision");
    }
    if (decided("rejected_conflict") || decided("clarification_required")) && !has_conflicts {
        conflicts.push("conflicts");
    }
    if !decided("accepted") && !decided("rejected_conflict") && !decided("clarification_required") {
        conflicts.push("decision");
    }
    if conflicts.is_empty() {
        return json!({ "ok": true });
    }
    let mut unique: Vec<&str> = Vec::new();
    for conflict in conflicts {
        if !unique.contains(&conflict) {
            unique.push(conflict);
        }
    }
    json!({ "ok": false, "conflicts": unique })
}

/// Same length and the same elements after JavaScript's default sort.
fn same_set(left: &Value, right: &Value) -> bool {
    let sorted = |value: &Value| {
        let mut items: Vec<Value> = value.as_array().cloned().unwrap_or_default();
        items.sort_by(|a, b| {
            string(Some(a))
                .encode_utf16()
                .cmp(string(Some(b)).encode_utf16())
        });
        items
    };
    let (a, b) = (sorted(left), sorted(right));
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(x, y)| strict_equal(Some(x), Some(y)))
}

fn same_goal_stack(left: &Value, right: &Value) -> bool {
    strict_equal(left.get("projectGoalId"), right.get("projectGoalId"))
        && same_set(&left["missionCriterionIds"], &right["missionCriterionIds"])
        && strict_equal(
            left.get("directionQuestionId"),
            right.get("directionQuestionId"),
        )
        && strict_equal(
            left.get("currentGraphNodeId"),
            right.get("currentGraphNodeId"),
        )
}

fn same_versions(left: &Value, right: &Value) -> bool {
    let keys = |value: &Value| {
        let mut keys: Vec<String> = value
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        keys
    };
    let (a, b) = (keys(left), keys(right));
    a == b
        && a.iter()
            .all(|key| strict_equal(left.get(key), right.get(key)))
}
