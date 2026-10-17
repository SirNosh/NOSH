//! Episodes, as in `packages/episodes`.

use crate::Result;
use nosh_wire::js::{length, slice_to};
use nosh_wire::{IdPrefix, create_id, sha256, validate_record};
use serde_json::{Map, Value, json};

const EPISODE: &str = "https://nosh.dev/schemas/episode/v1";

/// A validated, hashed Episode. The hash covers the record as given, before validation applies
/// defaults; TypeScript does the same, so an input fact without `confidence` yields an Episode
/// whose integrity check then fails (reproduced deliberately).
pub fn create_episode(input: &Map<String, Value>) -> Result<Value> {
    let mut unhashed = Map::new();
    unhashed.insert("$schema".into(), json!(EPISODE));
    unhashed.insert("schemaVersion".into(), json!(1));
    unhashed.extend(input.clone());
    let id = input
        .get("episodeId")
        .filter(|id| !id.is_null())
        .cloned()
        .unwrap_or_else(|| json!(create_id(IdPrefix::Epi)));
    unhashed.insert("episodeId".into(), id);
    let mut hashed = unhashed.clone();
    hashed.insert(
        "episodeHash".into(),
        json!(sha256(&Value::Object(unhashed))),
    );
    Ok(validate_record(EPISODE, &Value::Object(hashed))?)
}

pub fn fallback_episode_draft(episode_type: &str, summary: &str) -> Value {
    let trimmed = js_trim(summary);
    let summary = if trimmed.is_empty() {
        "The execution step completed without a semantic summary."
    } else {
        trimmed
    };
    json!({ "$schema": "https://nosh.dev/schemas/episode-draft/v1", "schemaVersion": 1, "episodeType": episode_type, "summary": summary, "facts": [], "decisions": [], "artifactIds": [], "evidenceIds": [], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [] })
}

/// The compact text form of an Episode given to agents.
pub fn render_episode(episode: &Value) -> String {
    let text = |value: &Value| nosh_wire::js::string(Some(value));
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    let ids = |value: &Value, n: usize| {
        list(value)
            .iter()
            .take(n)
            .map(text)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let facts: Vec<String> = list(&episode["facts"])
        .iter()
        .take(4)
        .map(|item| {
            let refs = list(&item["evidenceRefs"]);
            let suffix = if refs.is_empty() {
                String::new()
            } else {
                format!(" ({})", ids(&item["evidenceRefs"], 8))
            };
            format!(
                "- {} [{}]{suffix}",
                clip(&text(&item["statement"]), 240),
                text(&item["confidence"])
            )
        })
        .collect();
    let decisions: Vec<String> = list(&episode["decisions"])
        .iter()
        .take(3)
        .map(|item| {
            format!(
                "- {}: {}",
                clip(&text(&item["statement"]), 180),
                clip(&text(&item["rationale"]), 240)
            )
        })
        .collect();
    let questions: Vec<String> = list(&episode["unresolvedQuestions"])
        .iter()
        .take(4)
        .map(|q| clip(&text(q), 180))
        .collect();
    let lines = [
        format!(
            "Episode {} ({}, {})",
            text(&episode["episodeId"]),
            text(&episode["episodeType"]),
            text(&episode["status"])
        ),
        format!("Objective: {}", clip(&text(&episode["objective"]), 400)),
        format!("Summary: {}", clip(&text(&episode["summary"]), 500)),
        if facts.is_empty() {
            String::new()
        } else {
            format!("Facts:\n{}", facts.join("\n"))
        },
        if decisions.is_empty() {
            String::new()
        } else {
            format!("Decisions:\n{}", decisions.join("\n"))
        },
        if list(&episode["artifactIds"]).is_empty() {
            String::new()
        } else {
            format!("Artifacts: {}", ids(&episode["artifactIds"], 12))
        },
        if list(&episode["evidenceIds"]).is_empty() {
            String::new()
        } else {
            format!("Evidence: {}", ids(&episode["evidenceIds"], 12))
        },
        if questions.is_empty() {
            String::new()
        } else {
            format!("Open questions: {}", questions.join(" | "))
        },
    ];
    lines
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the Episode's hash matches its content.
pub fn episode_integrity(episode: &Value) -> bool {
    let mut unhashed = episode.as_object().cloned().unwrap_or_default();
    let hash = unhashed.remove("episodeHash");
    hash.as_ref().and_then(Value::as_str) == Some(sha256(&Value::Object(unhashed)).as_str())
}

/// Clips to `maximum` UTF-16 units, ending in `…`.
pub(crate) fn clip(value: &str, maximum: usize) -> String {
    if length(value) > maximum {
        format!("{}…", slice_to(value, maximum.saturating_sub(1)))
    } else {
        value.to_owned()
    }
}

/// `String.prototype.trim`: JavaScript whitespace and line terminators (Unicode White_Space
/// without U+0085, plus U+FEFF).
pub(crate) fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}
