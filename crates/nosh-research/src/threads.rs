//! Execution threads, as in `packages/threads`.

use crate::episodes::render_episode;
use crate::{Result, rejected};
use nosh_wire::js::{length, string};
use nosh_wire::validate_record;
use serde_json::{Value, json};
use std::collections::HashSet;

const EXECUTION_THREAD: &str = "https://nosh.dev/schemas/execution-thread/v1";

/// A new thread for a `THREAD_OPEN` or `THREAD_FORK` instruction.
pub fn create_thread(instruction: &Value) -> Result<Value> {
    let now = nosh_wire::now_utc();
    let field = |key: &str| instruction.get(key).cloned().unwrap_or(Value::Null);
    let foreground = instruction["executionMode"] == "foreground_fork";
    let parent = if instruction["operation"] == "THREAD_FORK" {
        field("controllingThreadId")
    } else {
        field("parentThreadId")
    };
    let thread = json!({
        "$schema": EXECUTION_THREAD, "schemaVersion": 1, "threadId": field("threadId"), "projectId": field("projectId"), "taskId": field("taskId"), "ownerScope": field("ownerScope"),
        "role": field("role"), "purpose": field("purpose"), "executionMode": field("executionMode"), "state": if foreground { "awaiting_user" } else { "open" },
        "parentThreadId": parent, "childThreadIds": [], "currentAgentId": null, "currentPiSessionId": null, "activeInstructionId": null, "sessionHistory": [],
        "episodeIds": [], "inputRefs": field("inputRefs"), "activeSkillIds": field("skillIds"), "capabilities": field("capabilities"), "taskPermissions": field("taskPermissions"), "taskWorkspace": field("taskWorkspace"),
        "budget": field("budget"), "usage": { "toolCalls": 0, "modelTokens": 0, "wallClockSeconds": 0 }, "nextStepNumber": 1, "createdAt": now, "updatedAt": now,
    });
    Ok(validate_record(EXECUTION_THREAD, &thread)?)
}

/// Refuses a step when the thread is not steppable or has spent a budget.
pub fn ensure_can_step(thread: &Value) -> Result<()> {
    let id = string(thread.get("threadId"));
    let state = thread["state"].as_str().unwrap_or_default();
    if !matches!(state, "open" | "awaiting" | "awaiting_user" | "paused") {
        return rejected(format!(
            "Thread {id} cannot step from {}",
            string(thread.get("state"))
        ));
    }
    if nosh_wire::js::truthy(thread.get("activeInstructionId")) {
        return rejected(format!("Thread {id} still has an active instruction"));
    }
    for (usage, limit, label) in [
        ("toolCalls", "maximumToolCalls", "tool-call"),
        ("modelTokens", "maximumModelTokens", "model-token"),
        ("wallClockSeconds", "maximumWallClockSeconds", "wall-clock"),
    ] {
        if number(&thread["usage"][usage]) >= number(&thread["budget"][limit]) {
            return rejected(format!("Thread {id} exhausted its {label} budget"));
        }
    }
    Ok(())
}

/// The context a thread step starts from: active skills, the newest selected Episodes that fit in
/// 16,000 UTF-16 units, the rest by ID, other references, and the remaining budget.
pub fn render_thread_context(
    thread: &Value,
    episodes: &[Value],
    extra_refs: &[String],
    skill_prompts: &[String],
) -> String {
    let mut seen = HashSet::new();
    let refs: Vec<String> = thread["inputRefs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| string(Some(r)))
        .chain(extra_refs.iter().cloned())
        .filter(|r| seen.insert(r.clone()))
        .collect();
    let mut candidates: Vec<&Value> = refs
        .iter()
        .filter_map(|r| {
            episodes
                .iter()
                .find(|e| e["episodeId"].as_str() == Some(r.as_str()))
        })
        .collect();
    candidates = candidates.split_off(candidates.len().saturating_sub(20));
    let (mut rendered, mut included, mut remaining) = (Vec::new(), HashSet::new(), 16_000_i64);
    for episode in candidates.iter().rev() {
        let value = render_episode(episode);
        let size = i64::try_from(length(&value)).unwrap_or(i64::MAX);
        if size > remaining {
            continue;
        }
        rendered.insert(0, value);
        included.insert(string(episode.get("episodeId")));
        remaining -= size + 2;
    }
    let omitted: Vec<String> = candidates
        .iter()
        .map(|e| string(e.get("episodeId")))
        .filter(|id| !included.contains(id))
        .collect();
    let episode_ids: HashSet<String> = candidates
        .iter()
        .map(|e| string(e.get("episodeId")))
        .collect();
    let others: Vec<&String> = refs.iter().filter(|r| !episode_ids.contains(*r)).collect();
    let others = &others[others.len().saturating_sub(50)..];
    let left = |usage: &str, limit: &str| {
        string(Some(&json!(nosh_wire::js::max(
            0.0,
            number(&thread["budget"][limit]) - number(&thread["usage"][usage])
        ))))
    };
    let sections = [
        if skill_prompts.is_empty() {
            String::new()
        } else {
            format!("Active skills:\n{}", skill_prompts.join("\n\n"))
        },
        if rendered.is_empty() {
            String::new()
        } else {
            format!("Selected prior episodes:\n\n{}", rendered.join("\n\n"))
        },
        if omitted.is_empty() {
            String::new()
        } else {
            format!(
                "Older selected episodes (fetch by ID if needed): {}",
                omitted.join(", ")
            )
        },
        if others.is_empty() {
            String::new()
        } else {
            format!(
                "Selected authoritative references: {}",
                others
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        format!(
            "Thread budget remaining: {} tool calls, {} model tokens, {} seconds.",
            left("toolCalls", "maximumToolCalls"),
            left("modelTokens", "maximumModelTokens"),
            left("wallClockSeconds", "maximumWallClockSeconds")
        ),
    ];
    sections
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}
