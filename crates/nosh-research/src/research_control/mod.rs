//! The daemon's research control, as `ResearchControl` in `apps/noshd/src/research-control.ts`:
//! Missions, Directions, Autoresearch, graph proposals, domain-effect records and handoffs, user
//! Evidence and Claims, Mission completion, and the paper.
//!
//! Projections and records stay `serde_json::Value`s shaped like the TypeScript objects. They are
//! stored as canonical JSON that both implementations read, and spreading one
//! (`{ ...current.value, state }`) must keep fields a newer NOSH added.

mod completion;
mod domain;
mod entities;
mod paper_ops;
mod records;

use crate::{Error, Result, rejected};
use nosh_store::{EventStore, GraphInput, ProjectionInput};
use nosh_wire::js::{slice_to, strict_equal, string};
use nosh_wire::{canonical_json, normalize_numbers, validate_record};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

pub use completion::{
    BasisCompletion, BasisCriterion, CompletionBasis, downtime_intervals, running_seconds,
};
pub use domain::DomainSession;
pub use entities::{
    AutoresearchInput, BaselineInput, DirectionInput, MissionInput, SPAWN_NODE_TYPES,
    SpawnedAutoresearch, SpawnedChild, SpawnedDirection,
};
pub use records::ReviewAuthority;

/// What the daemon provides: per-Project stores, the Project registry, and event delivery (the
/// TypeScript constructor's `store`, `project`, `publish`, and `assertWritable` callbacks).
pub trait Host {
    /// The Project's event store; an unknown Project is an error.
    fn store(&self, project_id: &str) -> Result<&EventStore>;
    /// The registered Project: `{ projectId, repositoryRoot, databasePath, registeredAt }`.
    fn project(&self, project_id: &str) -> Result<Value>;
    /// Delivers a newly stored event to subscribers.
    fn publish(&self, event: &Value);
    /// Refuses writes while a Project cannot accept them (during a restore).
    fn assert_writable(&self, _project_id: &str) -> Result<()> {
        Ok(())
    }
}

pub struct ResearchControl<H: Host> {
    host: H,
    /// Evidence frozen for an open Review but not yet recorded (it is published with the Review's
    /// disposition).
    candidates: Mutex<HashMap<String, Value>>,
}

impl<H: Host> ResearchControl<H> {
    pub fn new(host: H) -> Self {
        ResearchControl {
            host,
            candidates: Mutex::new(HashMap::new()),
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    fn store(&self, project_id: &str) -> Result<&EventStore> {
        self.host.store(project_id)
    }

    fn repository_root(&self, project_id: &str) -> Result<PathBuf> {
        let project = self.host.project(project_id)?;
        Ok(PathBuf::from(string(project.get("repositoryRoot"))))
    }

    /// Durable Project events after a cursor; director briefs build their digest from this.
    pub fn events_after(&self, project_id: &str, after_sequence: i64) -> Result<Vec<Value>> {
        Ok(self.store(project_id)?.replay(project_id, after_sequence)?)
    }

    pub fn offer_candidate_evidence(&self, project_id: &str, evidence: Value) {
        let key = format!("{project_id}:{}", string(evidence.get("evidenceId")));
        self.candidates
            .lock()
            .expect("candidates lock")
            .insert(key, evidence);
    }

    pub fn withdraw_candidate_evidence(&self, project_id: &str, evidence_id: &str) {
        self.candidates
            .lock()
            .expect("candidates lock")
            .remove(&format!("{project_id}:{evidence_id}"));
    }

    pub fn candidate_evidence(&self, project_id: &str, evidence_id: &str) -> Option<Value> {
        self.candidates
            .lock()
            .expect("candidates lock")
            .get(&format!("{project_id}:{evidence_id}"))
            .cloned()
    }

    /// Writes a Mission, Direction, or Autoresearch projection with its event, graph version, and
    /// companion records (the canonical contract; Mission node records) in one transaction.
    #[allow(clippy::too_many_arguments)]
    fn mutate(
        &self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
        expected_version: i64,
        event_type: &str,
        idempotency_key: &str,
        value: Value,
        graph: Option<GraphInput>,
        details: Option<Map<String, Value>>,
        persist_canonical_contract: bool,
        intent: Option<Value>,
    ) -> Result<Value> {
        self.host.assert_writable(project_id)?;
        let scope = json!({
            "projectId": project_id,
            "missionId": if entity_type == "mission" { json!(entity_id) } else { or_null(value.get("missionId")) },
            "directionId": if entity_type == "direction" { json!(entity_id) } else { or_null(value.get("directionId")) },
            "autoresearchId": if entity_type == "autoresearch" { json!(entity_id) } else { Value::Null },
            "experimentId": null, "runId": null, "jobId": null, "agentId": null,
        });
        let graph_version = value.get("graphVersion").cloned().unwrap_or(Value::Null);
        let mut canonical_value = value.clone();
        if persist_canonical_contract {
            canonical_value["canonicalContract"] =
                entities::canonical_contract(entity_type, &value)?;
        }
        let mut companion_records = Vec::new();
        if persist_canonical_contract {
            let contract = &canonical_value["canonicalContract"];
            let parsed =
                validate_record(&string(contract.get("$schema")), contract).map_err(|issues| {
                    Error::Rejected(format!("Invalid canonical contract: {}", messages(&issues)))
                })?;
            companion_records.push(record_submitted(&scope, entity_id, parsed));
        }
        let detail_node = details
            .as_ref()
            .and_then(|details| details.get("nodeId"))
            .and_then(Value::as_str);
        if entity_type == "mission" && (graph.is_some() || detail_node.is_some()) {
            let rationale = graph
                .as_ref()
                .map_or("node state transition", |graph| graph.rationale.as_str());
            for node in canonical_value["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|node| {
                    graph.is_some() || node.get("id").and_then(Value::as_str) == detail_node
                })
            {
                let record = entities::mission_node_record(&canonical_value, node, rationale);
                let parsed =
                    validate_record(&schema_uri("mission-node"), &record).map_err(|issues| {
                        Error::Rejected(format!("Invalid mission node: {}", messages(&issues)))
                    })?;
                companion_records.push(record_submitted(&scope, entity_id, parsed));
            }
        }
        let mut payload = Map::new();
        payload.insert("entityId".into(), json!(entity_id));
        payload.insert("state".into(), value["state"].clone());
        payload.insert("graphVersion".into(), graph_version);
        // The human title rides along so clients can label the entity from the event stream alone.
        if let Some(title) = entity_title(&value) {
            payload.insert("title".into(), json!(title));
        }
        payload.extend(details.unwrap_or_default());
        let draft = json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": event_type, "source": "noshd", "scope": scope, "correlationId": entity_id, "causationId": null, "payload": payload });
        let result = self.store(project_id)?.mutate_projection(
            idempotency_key,
            Some(expected_version),
            &draft,
            &ProjectionInput {
                entity_type: entity_type.into(),
                entity_id: entity_id.into(),
                state: string(value.get("state")),
                value: canonical_value,
                graph,
                records: companion_records,
                intent,
            },
        )?;
        if !result.replayed {
            self.host.publish(&result.event);
            for record in &result.records {
                self.host.publish(record);
            }
        }
        Ok(result.projection)
    }
}

// ---- Free functions exported by research-control.ts ----

/// Librarian network reads need the active Project contract to opt in explicitly; the default
/// policy (`network_user.approved`) grants none, fail closed.
pub fn librarian_network_allowed(project: &Value) -> bool {
    crate::project::read_project_contract(&PathBuf::from(string(project.get("repositoryRoot"))))
        .is_ok_and(|contract| contract["policies"]["network"] == "network_research.allowlisted")
}

/// Commands the approved Project contract lets general workers run through `nosh_run`; none
/// unless the contract opts in.
pub fn declared_task_commands(project: &Value) -> Vec<Value> {
    crate::project::read_project_contract(&PathBuf::from(string(project.get("repositoryRoot"))))
        .ok()
        .and_then(|contract| {
            contract
                .get("execution")
                .and_then(|execution| execution.get("commands"))
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
}

/// Task-packet instructions listing the runnable commands, or nothing when none are declared.
/// Mission and Direction tasks (whose postflight accepts deferred criteria) also learn how to
/// evidence a comparison across configurations.
pub fn task_command_instructions(commands: &[Value], deferred_comparisons: bool) -> Vec<String> {
    if commands.is_empty() {
        return Vec::new();
    }
    let listed: Vec<String> = commands
        .iter()
        .map(|command| {
            let argv: Vec<String> = command["argv"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|arg| string(Some(arg)))
                .collect();
            format!(
                "{} = {} ({})",
                string(command.get("commandId")),
                argv.join(" "),
                string(command.get("description"))
            )
        })
        .collect();
    let mut instructions = vec![format!(
        "You may run these Project-contract commands with nosh_run (by commandId) in your worktree: {}. The daemon commits your edits before each run; to cite a run as a criterion validator, run it after your last edit and put its jobId in validatorRunIds.",
        listed.join("; ")
    )];
    if deferred_comparisons {
        instructions.push("A comparison across configurations (one run per configuration, each on its own commit) cannot all run after your last edit: claim that criterion deferred_to_review and put every compared run's jobId in its validatorRunIds; the independent Review judges them.".into());
    }
    instructions
}

/// A retried node's worker learns what ended its last three attempts (newest first) from the
/// supervisors' durable failure events and Review verdicts. A reviewer's own error is not the
/// work's defect, so only its occurrence is named.
pub fn prior_attempt_instructions(events: &[Value], node_id: &str) -> Vec<String> {
    // `array.join(separator)`: null and undefined elements become "".
    let join = |items: Option<&Value>, separator: &str| -> String {
        items
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    string(Some(item))
                }
            })
            .collect::<Vec<_>>()
            .join(separator)
    };
    let mut failures: Vec<String> = events
        .iter()
        .filter_map(|entry| {
            let payload = &entry["payload"];
            let kind = entry["type"].as_str().unwrap_or_default();
            if kind == "record.submitted"
                && is_schema(payload, "review-verdict")
                && is(payload.get("target").and_then(|target| target.get("targetId")), node_id)
                && !is(payload.get("verdict"), "PASS")
            {
                let unmet: Vec<String> = payload["criteria"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|criterion| !is(criterion.get("status"), "PASS"))
                    .map(|criterion| {
                        format!(
                            "{} {}: {}",
                            string(criterion.get("criterionId")),
                            string(criterion.get("status")),
                            string(criterion.get("finding"))
                        )
                    })
                    .collect();
                return Some(format!(
                    "its independent Review returned {}: {} Unmet: {}",
                    string(payload.get("verdict")),
                    string(payload.get("summary")),
                    unmet.join("; ")
                ));
            }
            if !is(payload.get("nodeId"), node_id) {
                return None;
            }
            match kind {
                "task.postflight_failed" | "direction.postflight_failed" => Some(format!(
                    "postflight rejected it: {}",
                    join(payload.get("issues"), "; ")
                )),
                "task.execution_failed" | "direction.task_failed" => Some(format!(
                    "it ended with an error: {}",
                    string(payload.get("message"))
                )),
                "task.review_failed" => Some("it passed postflight, but its independent Review did not complete (a reviewer error, not a judgement of the work)".into()),
                _ => None,
            }
        })
        .collect();
    failures.drain(..failures.len().saturating_sub(3));
    if failures.is_empty() {
        return Vec::new();
    }
    let listed: Vec<String> = failures
        .iter()
        .rev()
        .enumerate()
        .map(|(index, failure)| format!("({}) {}", index + 1, slice_to(failure, 290)))
        .collect();
    vec![slice_to(
        &format!(
            "Earlier attempts at this node failed (most recent first); address these before claiming the work complete: {}",
            listed.join(" ")
        ),
        1_000,
    )]
}

/// The earlier worker attempt a retry should build on: the latest one at this node that passed
/// postflight, unless its Review returned REDESIGN (a rejected approach, so start over). REVISE,
/// BLOCKED and a reviewer's own error keep it. Returns `{ taskId }` or null.
pub fn resumable_attempt(events: &[Value], node_id: &str) -> Value {
    let mut passed: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for entry in events {
        let payload = &entry["payload"];
        let kind = entry["type"].as_str().unwrap_or_default();
        if kind == "record.submitted"
            && is_schema(payload, "review-verdict")
            && is(
                payload
                    .get("target")
                    .and_then(|target| target.get("targetId")),
                node_id,
            )
        {
            if is(payload.get("verdict"), "REDESIGN")
                && current.as_deref().is_some_and(|lease| !lease.is_empty())
                && passed.last() == current.as_ref()
            {
                passed.pop();
            }
            continue;
        }
        if !is(payload.get("nodeId"), node_id)
            || !matches!(
                kind,
                "mission.node_state_changed" | "direction.node_state_changed"
            )
        {
            continue;
        }
        let lease = payload.get("leaseId").and_then(Value::as_str);
        if is(payload.get("nodeState"), "leased") {
            current = lease.map(str::to_owned);
        }
        if is(payload.get("nodeState"), "postflight")
            && let Some(lease) = lease.filter(|lease| !lease.is_empty())
        {
            current = Some(lease.to_owned());
            passed.push(lease.to_owned());
        }
    }
    passed
        .last()
        .map_or(Value::Null, |task_id| json!({ "taskId": task_id }))
}

/// What a task reviewer must see besides the worker's claims: the daemon's own verified facts for
/// that task.
pub fn daemon_task_facts<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    task_id: &str,
) -> Result<String> {
    let runs: Vec<String> = task_command_runs(research, project_id)?
        .iter()
        .filter(|run| run.get("taskId").and_then(Value::as_str) == Some(task_id))
        .map(|run| {
            literal(&[
                ("jobId", run.get("jobId")),
                ("commandId", run.get("commandId")),
                ("description", run.get("description")),
                ("argv", run.get("argv")),
                ("commit", run.get("commit")),
                (
                    "cleanWorktree",
                    Some(&json!(run.get("dirty") == Some(&Value::Bool(false)))),
                ),
                ("state", run.get("state")),
                ("exitCode", run.get("exitCode")),
                ("stdoutTail", run.get("stdoutTail")),
            ])
        })
        .collect();
    Ok(format!(
        "Daemon-verified facts (authoritative; the worker cannot forge them): this completion passed the deterministic postflight (Git scope and branch, no protected or out-of-scope changes, every cited job_ validator is this task's own passing run on its clean ending commit). nosh_run Jobs of this task: [{}].",
        runs.join(",")
    ))
}

/// What a Mission reviewer node must see: the accepted work mapped to each success criterion, with
/// reviews and the daemon's run facts. Returns `{ artifactIds, evidenceIds, digest }`.
pub fn mission_outcomes<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    mission_id: &str,
    maximum_characters: usize,
) -> Result<Value> {
    let basis = research.mission_completion_basis(project_id, mission_id)?;
    let runs = task_command_runs(research, project_id)?;
    let criteria: Vec<String> = basis
        .criteria
        .iter()
        .map(|criterion| {
            literal(&[
                ("criterionId", Some(&json!(criterion.criterion_id))),
                ("statement", Some(&json!(criterion.statement))),
                ("taskId", Some(&json!(criterion.task_id))),
                ("reviewId", Some(&json!(criterion.review_id))),
                ("validatorRunIds", Some(&json!(criterion.validator_run_ids))),
            ])
        })
        .collect();
    let accepted: Vec<String> = basis
        .completions
        .iter()
        .map(|entry| {
            let review = literal(&[
                ("reviewId", entry.review.get("reviewId")),
                ("verdict", entry.review.get("verdict")),
                ("summary", entry.review.get("summary")),
            ]);
            let task_runs: Vec<String> = runs
                .iter()
                .filter(|run| {
                    run.get("taskId").and_then(Value::as_str) == Some(entry.task_id.as_str())
                })
                .map(|run| {
                    literal(&[
                        ("jobId", run.get("jobId")),
                        ("commandId", run.get("commandId")),
                        ("description", run.get("description")),
                        ("commit", run.get("commit")),
                        (
                            "cleanWorktree",
                            Some(&json!(run.get("dirty") == Some(&Value::Bool(false)))),
                        ),
                        ("exitCode", run.get("exitCode")),
                        ("stdoutTail", run.get("stdoutTail")),
                    ])
                })
                .collect();
            literal_text(&[
                ("nodeId", Some(canonical_json(&json!(entry.node_id)))),
                ("taskId", Some(canonical_json(&json!(entry.task_id)))),
                (
                    "taskOutcome",
                    entry.completion.get("taskOutcome").map(canonical_json),
                ),
                (
                    "workerCriteria",
                    entry.completion.get("criteria").map(canonical_json),
                ),
                ("review", Some(review)),
                ("nosh_runs", Some(format!("[{}]", task_runs.join(",")))),
            ])
        })
        .collect();
    let issues: Vec<String> = basis
        .issues
        .iter()
        .take(10)
        .map(|issue| canonical_json(&json!(issue)))
        .collect();
    let digest = literal_text(&[
        ("criteria", Some(format!("[{}]", criteria.join(",")))),
        ("acceptedWork", Some(format!("[{}]", accepted.join(",")))),
        ("openIssues", Some(format!("[{}]", issues.join(",")))),
    ]);
    Ok(
        json!({ "artifactIds": basis.artifact_ids, "evidenceIds": basis.evidence_ids, "digest": nosh_wire::js::slice_to(&digest, maximum_characters) }),
    )
}

/// Every `nosh_run` result recorded for the Project (payload of `task.command_run`).
pub fn task_command_runs<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
) -> Result<Vec<Value>> {
    Ok(research
        .events_after(project_id, 0)?
        .into_iter()
        .filter(|event| event["type"] == "task.command_run")
        .map(|event| event["payload"].clone())
        .collect())
}

pub const LIBRARIAN_RESEARCH_HOSTS: &[&str] = &[
    "api.crossref.org",
    "api.openalex.org",
    "api.semanticscholar.org",
    "arxiv.org",
    "export.arxiv.org",
    "eutils.ncbi.nlm.nih.gov",
    "pubmed.ncbi.nlm.nih.gov",
];

// ---- JavaScript and record helpers ----

pub(crate) fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

/// `isSchema(value, name)`: an object whose `$schema` is that schema's URI.
fn is_schema(value: &Value, name: &str) -> bool {
    value.get("$schema").and_then(Value::as_str) == Some(schema_uri(name).as_str())
}

/// `value === "text"`.
fn is(value: Option<&Value>, text: &str) -> bool {
    value.and_then(Value::as_str) == Some(text)
}

fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    strict_equal(a, b)
}

/// `value ?? null`.
fn or_null(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or(Value::Null)
}

fn is_str(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_string)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

/// `Number.isInteger(value)`.
fn is_integer(value: Option<&Value>) -> bool {
    number(value).is_some_and(|n| n.is_finite() && n.trunc() == n)
}

/// `Number(value)` for JSON values (`None` is `undefined`).
fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        // Decimal text only: Rust's parser would also take "inf" and "nan", JavaScript's would not.
        Some(Value::String(s)) => match crate::episodes::js_trim(s) {
            "" => 0.0,
            "Infinity" | "+Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            text if text
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) =>
            {
                text.parse().unwrap_or(f64::NAN)
            }
            _ => f64::NAN,
        },
        Some(Value::Array(_) | Value::Object(_)) => f64::NAN,
    }
}

/// `isArray(value) ? value.filter(isObject) : []`.
fn objects(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.is_object())
}

/// `array.includes(text)` for a JSON array.
fn includes(array: Option<&Value>, text: &str) -> bool {
    array
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(text)))
}

/// `stringValues`: the strings of an all-string array, else an error.
fn string_values(value: Option<&Value>) -> Result<Vec<String>> {
    match value.and_then(Value::as_array) {
        Some(items) if items.iter().all(Value::is_string) => Ok(items
            .iter()
            .map(|item| item.as_str().expect("checked").to_owned())
            .collect()),
        _ => rejected("Expected an array of string references"),
    }
}

/// `sameStringSet`: both duplicate-free with the same members.
fn same_string_set(value: Option<&Value>, expected: &[String]) -> bool {
    let Some(items) = value.and_then(Value::as_array) else {
        return false;
    };
    if !items.iter().all(Value::is_string) {
        return false;
    }
    let values: Vec<&str> = items
        .iter()
        .map(|item| item.as_str().expect("checked"))
        .collect();
    values.iter().collect::<HashSet<_>>().len() == values.len()
        && expected.iter().collect::<HashSet<_>>().len() == expected.len()
        && values.len() == expected.len()
        && values.iter().all(|item| expected.iter().any(|e| e == item))
}

/// `[...new Set(items)]`: first occurrences, in order.
fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

/// `strings.sort()`: UTF-16 code-unit order.
fn js_sort(strings: &mut [String]) {
    strings.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

fn messages(issues: &[nosh_wire::Issue]) -> String {
    issues
        .iter()
        .map(|issue| issue.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// `errors.map((error) => `${error.pointer} ${error.message}`).join("; ")`.
fn pointer_messages(issues: &[nosh_wire::Issue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{} {}", issue.pointer, issue.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn valid(name: &str, record: &Value) -> bool {
    validate_record(&schema_uri(name), record).is_ok()
}

/// `serde_json::to_value` with integral floats as integers, as JavaScript holds them.
fn to_json<T: serde::Serialize>(value: &T) -> Result<Value> {
    let mut value = serde_json::to_value(value)?;
    normalize_numbers(&mut value);
    Ok(value)
}

fn record_submitted(scope: &Value, correlation_id: &str, record: Value) -> Value {
    json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "noshd", "scope": scope, "correlationId": correlation_id, "causationId": null, "payload": record })
}

fn project_scope(project_id: &str) -> Value {
    json!({ "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null })
}

/// `entityTitle`: `title ?? question ?? decisionQuestion`, when a non-empty string, to 200 units.
fn entity_title(value: &Value) -> Option<String> {
    let title = [
        value.get("title"),
        value.get("question"),
        value.get("decisionQuestion"),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.is_null())?;
    title
        .as_str()
        .filter(|title| !title.is_empty())
        .map(|title| nosh_wire::js::slice_to(title, 200))
}

/// `JSON.stringify` of an object literal built in code: keys in the order written, `undefined`
/// (`None`) omitted. Values come from the store (parsed from canonical JSON, so their own keys are
/// already in canonical order) or from code, so their canonical text is their `JSON.stringify`.
fn literal(entries: &[(&str, Option<&Value>)]) -> String {
    literal_text(
        &entries
            .iter()
            .map(|(key, value)| (*key, value.map(canonical_json)))
            .collect::<Vec<_>>(),
    )
}

fn literal_text(entries: &[(&str, Option<String>)]) -> String {
    let fields: Vec<String> = entries
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_ref()
                .map(|value| format!("{}:{value}", canonical_json(&json!(key))))
        })
        .collect();
    format!("{{{}}}", fields.join(","))
}
