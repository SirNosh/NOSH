//! The durable terminal-turn journal, as `admitTerminal`, `rejectTerminal`, and `submitTerminal`
//! in `apps/noshd/src/daemon.ts`. A turn admits at most two provider generations and one
//! correction; its validated records are frozen before any effect, and an accepted prefix resumes
//! after a crash.
//!
//! The journal is generic over [`JournalHost`] so the TypeScript tests' stubbed daemons port as
//! test hosts. Turns of one `projectId:turnId` run one at a time, in arrival order.

use super::{NoshDaemon, error_text};
use crate::{Error, Result, rejected};
use futures_util::future::LocalBoxFuture;
use nosh_agent::terminal::{TERMINAL_SCHEMA_TOOLS, TerminalContext};
use nosh_store::EventStore;
use nosh_wire::canonical_json;
use nosh_wire::js::string;
use regex::Regex;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::LazyLock;

/// The per-turn serialization (TypeScript chains each turn's promises).
#[derive(Default)]
pub struct TurnLocks(RefCell<HashMap<String, Rc<tokio::sync::Mutex<()>>>>);

impl TurnLocks {
    async fn with_turn<T>(&self, context: &TerminalContext, action: impl Future<Output = T>) -> T {
        let key = format!("{}:{}", context.project_id, context.turn_id);
        let lock = Rc::clone(self.0.borrow_mut().entry(key.clone()).or_default());
        let guard = lock.lock().await;
        let result = action.await;
        drop(guard);
        // The map and this call hold the last references: no later turn is waiting.
        if Rc::strong_count(&lock) == 2 {
            self.0.borrow_mut().remove(&key);
        }
        result
    }
}

/// What the journal needs from the daemon.
pub trait JournalHost {
    fn turn_locks(&self) -> &TurnLocks;
    /// The Project's store, refusing a Project in backup maintenance.
    fn journal_store(&self, project_id: &str) -> Result<Rc<EventStore>>;
    /// Whether the turn's session may still record its outcome.
    fn turn_active(&self, context: &TerminalContext) -> bool;
    /// The whole envelope's policy check, before any record is accepted.
    fn validate_terminal(&self, context: &TerminalContext, records: &[Value]) -> Result<()>;
    /// Daemon-owned facts filled into the records before validation.
    fn with_daemon_facts(
        &self,
        context: &TerminalContext,
        records: Vec<Value>,
    ) -> Result<Vec<Value>>;
    /// Accepts one record through its submission tool.
    fn submit_tool<'a>(
        &'a self,
        tool: &'a str,
        project_id: &'a str,
        attempt_key: &'a str,
        record: &'a Value,
        agent_id: &'a str,
    ) -> LocalBoxFuture<'a, Result<Value>>;
    /// The recorded Artifact (`art`) or Evidence (`evd`) IDs of the Project.
    fn recorded_ids(&self, project_id: &str, kind: &str) -> Result<Vec<String>>;
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

fn has_records(journal: &Value) -> bool {
    journal
        .get("records")
        .is_some_and(|records| !records.is_null())
}

fn closed(journal: &Value) -> bool {
    journal["closed"] == true
}

/// The receipt of an earlier attempt when the turn is already decided, else `None` (a new
/// generation is admitted).
pub async fn admit_terminal<H: JournalHost + ?Sized>(
    host: &H,
    context: &TerminalContext,
) -> Result<Option<Value>> {
    let mut frozen: Option<Vec<Value>> = None;
    let result = host
        .turn_locks()
        .with_turn(context, async {
            let store = host.journal_store(&context.project_id)?;
            let mut journal = store.terminal_turn(&context.project_id, &context.turn_id, &json!(context))?;
            if has_records(&journal) {
                frozen = Some(journal["records"].as_array().cloned().unwrap_or_default());
                return Ok(json!({ "accepted": false, "retryAllowed": false }));
            }
            // A generation without a durable rejection may have executed tools before a crash.
            // Do not start a replacement generation for that uncertain turn.
            let generations = number(&journal["generations"]);
            if closed(&journal) || generations >= 2.0 || generations > number(&journal["failures"]) {
                journal["closed"] = json!(true);
                journal["status"] = json!("rejected");
                store.save_terminal_turn(&context.project_id, &context.turn_id, &journal)?;
                return Ok(json!({ "accepted": false, "retryAllowed": false, "status": "rejected", "error": "Terminal generation budget exhausted" }));
            }
            journal["generations"] = json!(generations + 1.0);
            store.save_terminal_turn(&context.project_id, &context.turn_id, &journal)?;
            Ok::<_, Error>(json!({ "accepted": false, "retryAllowed": true, "admitted": true }))
        })
        .await?;
    if let Some(records) = frozen {
        return submit_terminal(host, context, records, true)
            .await
            .map(Some);
    }
    Ok(if result["admitted"] == true {
        None
    } else {
        Some(result)
    })
}

pub async fn reject_terminal<H: JournalHost + ?Sized>(
    host: &H,
    context: &TerminalContext,
    reason: &str,
) -> Result<Value> {
    host.turn_locks()
        .with_turn(context, async { reject_attempt(host, context, reason) })
        .await
}

fn reject_attempt<H: JournalHost + ?Sized>(
    host: &H,
    context: &TerminalContext,
    reason: &str,
) -> Result<Value> {
    let store = host.journal_store(&context.project_id)?;
    let mut receipt =
        store.terminal_turn(&context.project_id, &context.turn_id, &json!(context))?;
    if !closed(&receipt) && !has_records(&receipt) {
        let failures = number(&receipt["failures"]) + 1.0;
        let close = failures >= 2.0
            || reason.starts_with("Non-successful provider stop:")
            || reason.starts_with("Terminal policy:");
        receipt["failures"] = json!(failures);
        receipt["closed"] = json!(close);
        receipt["status"] = json!(if close { "rejected" } else { "open" });
        store.save_terminal_turn(&context.project_id, &context.turn_id, &receipt)?;
    }
    let retry = !closed(&receipt) && !has_records(&receipt);
    Ok(
        json!({ "accepted": false, "retryAllowed": retry, "status": receipt["status"], "error": reason }),
    )
}

static UNKNOWN_RECORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Episode references unknown record ((art|evd)_[0-9a-f]+)$").expect("regex")
});

/// Models mis-copy long hex IDs: an unknown-record slip names the closest recorded ID of its kind
/// (at most 3 characters off), so the one correction can fix it.
fn with_record_suggestion<H: JournalHost + ?Sized>(
    host: &H,
    project_id: &str,
    message: &str,
) -> String {
    let Some(missing) = UNKNOWN_RECORD.captures(message) else {
        return message.to_owned();
    };
    let id: Vec<u16> = missing[1].encode_utf16().collect();
    let known = host
        .recorded_ids(project_id, &missing[2])
        .unwrap_or_default();
    let distance = |other: &[u16]| {
        other
            .iter()
            .enumerate()
            .filter(|(index, unit)| id.get(*index) != Some(unit))
            .count()
    };
    let mut candidates: Vec<(usize, &String)> = known
        .iter()
        .map(|other| (other.encode_utf16().collect::<Vec<u16>>(), other))
        .filter(|(other, _)| other.len() == id.len())
        .map(|(other, text)| (distance(&other), text))
        .collect();
    candidates.sort_by_key(|(distance, _)| *distance);
    match candidates.first() {
        Some((distance, nearest)) if *distance <= 3 => {
            format!("{message} (did you mean {nearest}?)")
        }
        _ => message.to_owned(),
    }
}

/// A frozen journal's receipt.
fn receipt(journal: &Value) -> Value {
    let results = journal["results"].as_array().cloned().unwrap_or_default();
    let failed = results
        .iter()
        .any(|item| item.pointer("/effect/state").and_then(Value::as_str) == Some("failed"));
    let completed = journal["status"] == "completed";
    let state = if failed {
        "failed"
    } else if completed {
        "completed"
    } else {
        "pending"
    };
    json!({ "accepted": completed, "retryAllowed": false, "status": journal["status"], "results": results, "effect": { "state": state } })
}

/// Validates, freezes, and accepts a turn's records; `frozen` records were validated before.
pub async fn submit_terminal<H: JournalHost + ?Sized>(
    host: &H,
    context: &TerminalContext,
    submitted: Vec<Value>,
    frozen: bool,
) -> Result<Value> {
    let records = if frozen {
        submitted
    } else {
        match host.with_daemon_facts(context, submitted) {
            Ok(records) => records,
            Err(error) => {
                let reason = error_text(&error, "Terminal policy: daemon facts failed");
                return reject_terminal(host, context, &reason).await;
            }
        }
    };
    host.turn_locks()
        .with_turn(context, async {
            let project_id = &context.project_id;
            let store = host.journal_store(project_id)?;
            let mut journal = store.terminal_turn(project_id, &context.turn_id, &json!(context))?;
            if has_records(&journal) && canonical_json(&journal["records"]) != canonical_json(&json!(records)) {
                return Ok(json!({ "accepted": false, "retryAllowed": false, "status": journal["status"], "error": "Terminal envelope replay conflict" }));
            }
            if closed(&journal) && !has_records(&journal) {
                return Ok(json!({ "accepted": false, "retryAllowed": false, "status": journal["status"], "error": "Terminal correction budget exhausted" }));
            }
            if closed(&journal) {
                return Ok(receipt(&journal));
            }
            if !has_records(&journal) {
                if let Err(error) = host.validate_terminal(context, &records) {
                    let message = error_text(&error, "Terminal validation failed");
                    // A mis-copied ID of the session's own scope is a correctable model slip; the
                    // corrected record is checked by the same policy.
                    let correctable = [
                        "Invalid terminal record:",
                        "Unexpected terminal episode type",
                        "Episode references",
                        "Changed file escapes",
                        "Terminal output requires",
                        "Scope field mismatch:",
                    ]
                    .iter()
                    .any(|prefix| message.starts_with(prefix));
                    let reason = if correctable {
                        with_record_suggestion(host, project_id, &message)
                    } else {
                        format!("Terminal policy: {message}")
                    };
                    return reject_attempt(host, context, &reason);
                }
                // Freeze the complete validated intent before ANY record acceptance or effect.
                journal["records"] = json!(records);
                journal["status"] = json!("pending");
                store.save_terminal_turn(project_id, &context.turn_id, &journal)?;
            }
            let frozen_records = journal["records"].as_array().cloned().unwrap_or_default();
            let start = journal["results"].as_array().map_or(0, Vec::len);
            for index in start..records.len() {
                let record = frozen_records.get(index).cloned().unwrap_or(Value::Null);
                let schema = string(record.get("$schema"));
                let tool = TERMINAL_SCHEMA_TOOLS
                    .iter()
                    .find(|(uri, _)| *uri == schema)
                    .map_or("undefined", |(_, tool)| *tool);
                // Task terminal authority and supervisor lookup require the exact task correlation.
                let attempt = match (&context.instruction_id, &context.task_id) {
                    (Some(instruction), _) if tool == "nosh_episode_submit" && !instruction.is_empty() => format!("instruction:{instruction}"),
                    (_, Some(task)) if !task.is_empty() => format!("task:{task}"),
                    _ => format!("terminal:{}:{index}", context.turn_id),
                };
                let outcome: Result<Value> = async {
                    // These terminal schemas have no submitTool domain/runtime effects. Recover the
                    // append/journal crash window before reauthorizing a now-stale session.
                    let accepted = store.replay(project_id, 0)?.into_iter().find(|event| {
                        event["type"] == "record.submitted"
                            && event.pointer("/scope/projectId").and_then(Value::as_str) == Some(project_id.as_str())
                            && event.pointer("/scope/agentId").and_then(Value::as_str) == Some(context.agent_id.as_str())
                            && event.get("correlationId").and_then(Value::as_str) == Some(attempt.as_str())
                            && canonical_json(&event["payload"]) == canonical_json(&record)
                    });
                    Ok(if let Some(event) = accepted {
                        json!({ "accepted": true, "event": event })
                    } else if !host.turn_active(context) {
                        json!({ "accepted": false, "retryAllowed": false, "error": "Terminal session is cancelled or inactive" })
                    } else {
                        host.submit_tool(tool, project_id, &attempt, &record, &context.agent_id).await?
                    })
                }
                .await;
                let result = match outcome {
                    Ok(result) => result,
                    Err(error) => {
                        // The accepted prefix is resumable; no rewritten correction is safe now.
                        let any = journal["results"].as_array().is_some_and(|results| !results.is_empty());
                        journal["status"] = json!(if any { "partial" } else { "pending" });
                        store.save_terminal_turn(project_id, &context.turn_id, &journal)?;
                        let mut answer = receipt(&journal);
                        answer["error"] = json!(error_text(&error, "Terminal effect interrupted"));
                        return Ok(answer);
                    }
                };
                let accepted = result.is_object() && result["accepted"] == true;
                journal["results"].as_array_mut().expect("journal results").push(result);
                if !accepted {
                    let any = journal["results"]
                        .as_array()
                        .is_some_and(|results| results.iter().any(|item| item["accepted"] == true));
                    journal["status"] = json!(if any { "partial" } else { "rejected" });
                    journal["closed"] = json!(true);
                    store.save_terminal_turn(project_id, &context.turn_id, &journal)?;
                    return Ok(receipt(&journal));
                }
                let complete = index + 1 == records.len();
                journal["status"] = json!(if complete { "completed" } else { "partial" });
                journal["closed"] = json!(complete);
                store.save_terminal_turn(project_id, &context.turn_id, &journal)?;
            }
            Ok(receipt(&journal))
        })
        .await
}

const ROLES: [(&str, &str); 5] = [
    ("general-worker-completion", "general_worker"),
    ("librarian-completion", "librarian_researcher"),
    ("review-verdict", "reviewer"),
    ("mission-director-cycle", "mission_director"),
    ("research-director-cycle", "research_director"),
];

const SCOPE_FIELDS: [&str; 6] = [
    "missionId",
    "directionId",
    "autoresearchId",
    "experimentId",
    "runId",
    "jobId",
];

fn issues_json(issues: &[nosh_wire::Issue]) -> String {
    let items: Vec<String> = issues
        .iter()
        .map(|issue| {
            format!(
                "{{\"pointer\":{},\"code\":{},\"message\":{}}}",
                json!(issue.pointer),
                json!(issue.code),
                json!(issue.message)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

impl JournalHost for Rc<NoshDaemon> {
    fn turn_locks(&self) -> &TurnLocks {
        &self.terminal_turns
    }

    fn journal_store(&self, project_id: &str) -> Result<Rc<EventStore>> {
        self.assert_project_writable(project_id)?;
        self.store_for(project_id)
    }

    fn turn_active(&self, context: &TerminalContext) -> bool {
        self.agents.terminal_turn_active(context)
    }

    fn validate_terminal(&self, context: &TerminalContext, records: &[Value]) -> Result<()> {
        let agent = self.agents.inspect().into_iter().find(|agent| {
            agent.agent_id == context.agent_id && agent.project_id == context.project_id
        });
        let Some(agent) = agent.filter(|agent| agent.task_id == context.task_id) else {
            return rejected("Terminal output requires its active daemon-bound session");
        };
        let store = self.store_for(&context.project_id)?;
        if let Some(thread_id) = &context.thread_id {
            let thread = store.projection(&context.project_id, "execution_thread", thread_id)?;
            let stale = thread.as_ref().is_none_or(|thread| {
                thread["version"].as_f64() != context.expected_version
                    || thread
                        .pointer("/value/activeInstructionId")
                        .and_then(Value::as_str)
                        != context.instruction_id.as_deref()
                    || thread
                        .pointer("/value/currentAgentId")
                        .and_then(Value::as_str)
                        != Some(context.agent_id.as_str())
                    || thread.pointer("/value/state").and_then(Value::as_str) != Some("running")
            });
            if stale {
                return rejected("Stale terminal instruction or thread version");
            }
        } else if context.instruction_id.is_some() || context.expected_version.is_some() {
            return rejected("Incomplete terminal version scope");
        }
        if records.is_empty() || records.len() > 2 {
            return rejected("Terminal output requires one or two records");
        }
        let session = serde_json::to_value(&agent).unwrap_or(Value::Null);
        let (mut terminals, mut episodes) = (0, 0);
        for record in records {
            let schema = string(record.get("$schema"));
            let tool = TERMINAL_SCHEMA_TOOLS
                .iter()
                .find(|(uri, _)| *uri == schema)
                .map(|(_, tool)| *tool);
            if !tool.is_some_and(|tool| context.allowed_tools.iter().any(|allowed| allowed == tool))
            {
                return rejected("Terminal schema is outside the host tool contract");
            }
            let parsed = nosh_wire::validate_record(&schema, record).map_err(|issues| {
                Error::Rejected(format!("Invalid terminal record: {}", issues_json(&issues)))
            })?;
            if let Some((_, message)) = super::submit::submission_scope_error(
                record,
                &context.project_id,
                Some(&context.agent_id),
            ) {
                return rejected(message);
            }
            if let Some((_, role)) = ROLES
                .iter()
                .find(|(name, _)| schema == format!("https://nosh.dev/schemas/{name}/v1"))
                && *role != agent.role.as_str()
            {
                return rejected("Terminal record role does not match its session");
            }
            if let Some(task) = record.get("taskId")
                && task.as_str() != context.task_id.as_deref()
            {
                return rejected("Terminal task scope mismatch");
            }
            for field in SCOPE_FIELDS {
                let expected = &session[field];
                if let Some(value) = record.get(field)
                    && value != expected
                {
                    return rejected(format!(
                        "Scope field mismatch: {field} must be exactly {}",
                        string(Some(expected))
                    ));
                }
                if let Some(value) = record
                    .get("scope")
                    .filter(|scope| scope.is_object())
                    .and_then(|scope| scope.get(field))
                    && value != expected
                {
                    return rejected(format!(
                        "Scope field mismatch: scope.{field} must be exactly {}",
                        string(Some(expected))
                    ));
                }
            }
            if schema == "https://nosh.dev/schemas/episode-draft/v1" {
                episodes += 1;
                if context.expected_episode_type.is_none()
                    || record.get("episodeType").and_then(Value::as_str)
                        != context.expected_episode_type.as_deref()
                {
                    return rejected("Unexpected terminal episode type");
                }
                self.runtime.validate_draft(
                    &context.project_id,
                    &parsed,
                    Some(&context.agent_id),
                )?;
            } else {
                terminals += 1;
                let Some(task_id) = context.task_id.as_deref().filter(|task| !task.is_empty())
                else {
                    return rejected("Task terminal output requires a daemon-bound task");
                };
                if let Some(prior) =
                    self.research
                        .terminal_record(&context.project_id, task_id, None)?
                    && canonical_json(&prior) != canonical_json(record)
                {
                    return rejected("Conflicting task terminal replay");
                }
            }
            let domain = super::submit::domain_session(&agent);
            self.research
                .validate_domain_record(&context.project_id, &domain, record)?;
            self.research
                .canonical_domain_record(&context.project_id, record)?;
        }
        let expects_episode = context
            .expected_episode_type
            .as_deref()
            .is_some_and(|kind| !kind.is_empty());
        let task_bound = context
            .task_id
            .as_deref()
            .is_some_and(|task| !task.is_empty())
            && context.instruction_id.as_deref().is_none_or(str::is_empty);
        if terminals > 1
            || episodes > 1
            || (expects_episode && episodes != 1)
            || (task_bound && terminals != 1)
        {
            return rejected("Terminal output requires unique task result and expected episode");
        }
        Ok(())
    }

    fn with_daemon_facts(
        &self,
        context: &TerminalContext,
        records: Vec<Value>,
    ) -> Result<Vec<Value>> {
        NoshDaemon::with_daemon_facts(self, context, records)
    }

    fn submit_tool<'a>(
        &'a self,
        tool: &'a str,
        project_id: &'a str,
        attempt_key: &'a str,
        record: &'a Value,
        agent_id: &'a str,
    ) -> LocalBoxFuture<'a, Result<Value>> {
        Box::pin(NoshDaemon::submit_tool(
            self,
            tool,
            project_id,
            attempt_key,
            record,
            Some(agent_id),
        ))
    }

    fn recorded_ids(&self, project_id: &str, kind: &str) -> Result<Vec<String>> {
        let (schema, key) = if kind == "art" {
            ("artifact", "artifactId")
        } else {
            ("evidence", "evidenceId")
        };
        Ok(self
            .research
            .records(project_id, Some(schema))?
            .iter()
            .map(|entry| string(entry.get(key)))
            .collect())
    }
}

impl NoshDaemon {
    pub async fn admit_terminal(
        self: &Rc<Self>,
        context: &TerminalContext,
    ) -> Result<Option<Value>> {
        admit_terminal(self, context).await
    }

    pub async fn reject_terminal(
        self: &Rc<Self>,
        context: &TerminalContext,
        reason: &str,
    ) -> Result<Value> {
        reject_terminal(self, context, reason).await
    }

    pub async fn submit_terminal(
        self: &Rc<Self>,
        context: &TerminalContext,
        records: Vec<Value>,
        frozen: bool,
    ) -> Result<Value> {
        submit_terminal(self, context, records, frozen).await
    }
}
