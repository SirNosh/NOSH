//! Event envelopes and drafts, as in `packages/wire/src/events.ts`.

use crate::registry::{Issue, validate_record};
use serde_json::{Value, json};

pub const EVENT_SCHEMA: &str = "https://nosh.dev/schemas/event/v1";

/// Fields the store assigns; a draft is an envelope without them.
const ASSIGNED: [&str; 3] = ["eventId", "sequence", "timestamp"];

/// Validates a full event envelope.
pub fn validate_event(value: &Value) -> Result<Value, Vec<Issue>> {
    validate_record(EVENT_SCHEMA, value)
}

/// Validates an event draft (`eventDraftSchema`). The draft schema is the envelope schema without
/// the assigned fields and without the envelope's refinement, so the draft is checked as an
/// envelope with placeholder assigned fields that always satisfy that refinement.
pub fn validate_event_draft(draft: &Value) -> Result<Value, Vec<Issue>> {
    let Value::Object(map) = draft else {
        return validate_event(draft);
    };
    let assigned: Vec<Issue> = ASSIGNED
        .iter()
        .filter(|key| map.contains_key(**key))
        .map(|key| Issue {
            pointer: format!("/{key}"),
            code: "unrecognized_keys".into(),
            message: format!("Unrecognized key: {key}"),
        })
        .collect();
    if !assigned.is_empty() {
        return Err(assigned);
    }
    let mut envelope = map.clone();
    let persistent = map.get("retention") == Some(&json!("persistent"));
    envelope.insert(
        "eventId".into(),
        json!("evt_00000000000000000000000000000000"),
    );
    envelope.insert(
        "sequence".into(),
        if persistent { json!(1) } else { Value::Null },
    );
    envelope.insert("timestamp".into(), json!("1970-01-01T00:00:00.000Z"));
    let Value::Object(mut parsed) = validate_event(&Value::Object(envelope))? else {
        unreachable!("an object validates to an object");
    };
    for key in ASSIGNED {
        parsed.remove(key);
    }
    Ok(Value::Object(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Value {
        let mut value: Value = serde_json::from_str(include_str!(
            "../../../conformance/schemas/fixtures/event/full.valid.json"
        ))
        .unwrap();
        for key in ASSIGNED {
            value.as_object_mut().unwrap().remove(key);
        }
        value
    }

    #[test]
    fn drafts_validate_without_assigned_fields() {
        let persistent = draft();
        assert_eq!(validate_event_draft(&persistent).unwrap(), persistent);
        let mut ephemeral = draft();
        ephemeral["retention"] = json!("ephemeral");
        assert_eq!(validate_event_draft(&ephemeral).unwrap(), ephemeral);
    }

    #[test]
    fn drafts_reject_assigned_fields_and_schema_errors() {
        let mut with_id = draft();
        with_id["sequence"] = json!(3);
        let issues = validate_event_draft(&with_id).unwrap_err();
        assert_eq!(issues[0].pointer, "/sequence");
        assert_eq!(issues[0].code, "unrecognized_keys");

        let mut bad_type = draft();
        bad_type["type"] = json!("NoDots");
        assert_eq!(
            validate_event_draft(&bad_type).unwrap_err()[0].pointer,
            "/type"
        );
        assert!(validate_event_draft(&json!("not an object")).is_err());
    }
}
