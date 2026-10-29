//! Delegation: a worker's request for a child session, authorized against its durable Task
//! Packet, budgets, and capacity, then run as a runtime thread, as the delegation methods of
//! `apps/noshd/src/daemon.ts`. Authority, reservations, and outcomes are durable projections and
//! operation intents, so a restart resumes or settles every delegation exactly once.

use super::{NoshDaemon, error_text, event_draft};
use crate::{Error, Result, rejected};
use nosh_store::ProjectionInput;
use nosh_wire::js::string;
use nosh_wire::{now_utc, sha256};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::rc::Rc;

fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

/// `{ requestId, parentTaskId, modelTokens, wallClockSeconds, missionId }` of a reservation intent.
struct Reservation {
    request_id: String,
    parent_task_id: String,
    model_tokens: f64,
    wall_clock_seconds: f64,
    mission_id: Option<String>,
}

fn positive_integer(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|number| number.fract() == 0.0 && *number > 0.0)
}

fn reservation(value: &Value) -> Option<Reservation> {
    let scope = value.get("scope").filter(|scope| scope.is_object())?;
    Some(Reservation {
        request_id: value.get("requestId")?.as_str()?.to_owned(),
        parent_task_id: value.get("parentTaskId")?.as_str()?.to_owned(),
        model_tokens: positive_integer(value.get("modelTokens")?)?,
        wall_clock_seconds: positive_integer(value.get("wallClockSeconds")?)?,
        mission_id: nullable_id(scope.get("missionId")),
    })
}

fn deterministic_id(prefix: &str, project_id: &str, request_id: &str) -> String {
    let digest =
        sha256(&json!({ "prefix": prefix, "projectId": project_id, "requestId": request_id }));
    format!("{prefix}_{}", &digest[7..39])
}

fn nullable_id(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn object(value: &Value, message: &str) -> Result<Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| Error::Rejected(message.to_owned()))
}

fn event_scope(project_id: &str, scope: &Value, agent_id: &str) -> Result<Value> {
    if scope.get("projectId").and_then(Value::as_str) != Some(project_id) {
        return rejected("Delegation parent scope is outside the Project");
    }
    let id = |field: &str| json!(nullable_id(scope.get(field)));
    Ok(
        json!({ "projectId": project_id, "missionId": id("missionId"), "directionId": id("directionId"), "autoresearchId": id("autoresearchId"), "experimentId": id("experimentId"), "runId": id("runId"), "jobId": id("jobId"), "agentId": agent_id }),
    )
}

/// A durable runtime scope, checked.
fn durable_scope(value: &Value) -> Result<Value> {
    let fields = [
        "missionId",
        "directionId",
        "autoresearchId",
        "experimentId",
        "runId",
        "jobId",
        "agentId",
    ];
    let valid = value.get("projectId").is_some_and(Value::is_string)
        && fields
            .iter()
            .all(|field| matches!(value.get(*field), Some(Value::Null | Value::String(_))));
    if !value.is_object() || !valid {
        return rejected("Delegation authority has an invalid durable runtime scope");
    }
    Ok(value.clone())
}

fn rejection_scope(project_id: &str, parent_scope: Option<&Value>, agent_id: &str) -> Value {
    // The Project scope when the parent scope is itself invalid.
    parent_scope
        .and_then(|scope| event_scope(project_id, scope, agent_id).ok())
        .unwrap_or_else(|| {
            let mut scope = super::project_scope(project_id);
            scope["agentId"] = json!(agent_id);
            scope
        })
}

fn terminal_hash(
    value: &Map<String, Value>,
    state: &str,
    outcome: &Value,
    episode: &Value,
    error: Option<&str>,
) -> Result<String> {
    let scope = durable_scope(value.get("runtimeScope").unwrap_or(&Value::Null))?;
    Ok(sha256(
        &json!({ "requestHash": value.get("requestHash"), "state": state, "runtimeScope": scope, "runtimeOutcome": outcome, "episode": episode, "error": error }),
    ))
}

fn asserted_terminal(value: Map<String, Value>, state: &str) -> Result<Value> {
    let error = value.get("error").and_then(Value::as_str);
    let expected = terminal_hash(
        &value,
        state,
        value.get("runtimeOutcome").unwrap_or(&Value::Null),
        value.get("episode").unwrap_or(&Value::Null),
        error,
    )?;
    if !value.get("requestHash").is_some_and(Value::is_string)
        || value.get("terminalHash").and_then(Value::as_str) != Some(expected.as_str())
    {
        return rejected("Delegation terminal authority hash is invalid");
    }
    Ok(Value::Object(value))
}

fn asserted_rejection(value: Map<String, Value>, request_hash: &str) -> Result<Value> {
    let scope = durable_scope(value.get("runtimeScope").unwrap_or(&Value::Null))?;
    let error = value.get("error").and_then(Value::as_str);
    let valid = value.get("requestHash").and_then(Value::as_str) == Some(request_hash)
        && value.get("state").and_then(Value::as_str) == Some("rejected")
        && error.is_some()
        && value.get("terminalHash").and_then(Value::as_str)
            == Some(sha256(&json!({ "requestHash": request_hash, "state": "rejected", "runtimeScope": scope, "error": error })).as_str());
    if !valid {
        return rejected("Delegation rejection authority hash is invalid");
    }
    Ok(Value::Object(value))
}

fn runtime_request(
    project_id: &str,
    record: &Value,
    scope: &Value,
    child_task_id: &str,
    child_agent_id: &str,
    child_thread_id: &str,
) -> Value {
    let request_id = string(record.get("requestId"));
    let budget = &record["budgetEstimate"];
    let issued_at = &record["submittedAt"];
    let id = |field: &str| json!(nullable_id(scope.get(field)));
    let permissions = json!({ "network": "disabled", "subprocess": "disabled", "gitCommit": false, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": ["tool_nosh.episode.submit"] });
    let open = json!({ "$schema": schema_uri("runtime-instruction"), "schemaVersion": 1, "instructionId": deterministic_id("ins", project_id, &format!("{request_id}:open")), "projectId": project_id, "idempotencyKey": format!("delegation:{request_id}:open"), "proposedByAgentId": null, "issuedAt": issued_at, "operation": "THREAD_OPEN", "threadId": child_thread_id, "taskId": child_task_id, "initialAgentId": child_agent_id, "ownerScope": { "missionId": id("missionId"), "directionId": id("directionId"), "autoresearchId": id("autoresearchId"), "experimentId": id("experimentId"), "graphNodeId": id("graphNodeId") }, "role": record["requestedRole"], "purpose": record["proposedObjective"], "executionMode": "background", "parentThreadId": null, "inputRefs": [], "skillIds": [], "capabilities": [], "taskPermissions": permissions, "taskWorkspace": null, "budget": { "maximumToolCalls": 50, "maximumModelTokens": budget["modelTokens"], "maximumWallClockSeconds": budget["wallClockSeconds"] } });
    let step = json!({ "$schema": schema_uri("runtime-instruction"), "schemaVersion": 1, "instructionId": deterministic_id("ins", project_id, &format!("{request_id}:step")), "projectId": project_id, "idempotencyKey": format!("delegation:{request_id}:step"), "proposedByAgentId": null, "issuedAt": issued_at, "operation": "THREAD_STEP", "threadId": child_thread_id, "objective": record["proposedObjective"], "expectedEpisodeType": "episode_delegation", "inputRefs": [], "skillIds": [] });
    json!({ "open": open, "step": step })
}

fn run_key(project_id: &str, request_id: &str) -> String {
    format!("{project_id}:{request_id}")
}

impl NoshDaemon {
    /// Authorizes (or replays the durable decision on) a delegation request. A refusal is recorded
    /// durably and returned as an error.
    pub(super) fn authorize_delegation(
        self: &Rc<Self>,
        project_id: &str,
        record: &Value,
    ) -> Result<Value> {
        self.assert_project_writable(project_id)?;
        let text_field = |field: &str| record.get(field).and_then(Value::as_str);
        let well_formed = record.get("$schema").and_then(Value::as_str)
            == Some(schema_uri("delegation-request").as_str())
            && text_field("requestId").is_some()
            && text_field("requestingTaskId").is_some()
            && text_field("requestingAgentId").is_some()
            && text_field("requestedRole").is_some()
            && text_field("proposedObjective")
                .is_some_and(|objective| !super::js_trim(objective).is_empty())
            && text_field("submittedAt").is_some()
            && record.get("budgetEstimate").is_some_and(Value::is_object);
        if !well_formed {
            return rejected("Delegation request is malformed");
        }
        let budget = &record["budgetEstimate"];
        let integer = |value: &Value| value.as_f64().filter(|number| number.fract() == 0.0);
        let tokens = integer(&budget["modelTokens"]);
        let seconds = integer(&budget["wallClockSeconds"]);
        let role_ok = ["librarian_researcher", "general_worker", "reviewer"]
            .contains(&text_field("requestedRole").unwrap_or_default());
        let (Some(tokens), Some(seconds)) =
            (tokens.filter(|n| *n > 0.0), seconds.filter(|n| *n > 0.0))
        else {
            return rejected("Delegation request budget or role is invalid");
        };
        if !role_ok {
            return rejected("Delegation request budget or role is invalid");
        }
        let store = self.store_for(project_id)?;
        let request_id = string(record.get("requestId"));
        let request_hash = sha256(record);
        if let Some(existing) = store.projection(project_id, "delegation", &request_id)? {
            let value = object(&existing["value"], "Delegation projection is corrupt")?;
            if value.get("requestHash").and_then(Value::as_str) != Some(request_hash.as_str()) {
                return rejected("Delegation request ID already has different durable authority");
            }
            let state = string(existing.get("state"));
            return match state.as_str() {
                "rejected" => asserted_rejection(value, &request_hash),
                "completed" | "failed" => asserted_terminal(value, &state),
                "authorized" => {
                    self.resume_delegation(project_id, &request_id)?;
                    Ok(Value::Object(value))
                }
                other => rejected(format!(
                    "Delegation request has unexpected {other} durable authority"
                )),
            };
        }
        let requesting_task = string(record.get("requestingTaskId"));
        let requesting_agent = string(record.get("requestingAgentId"));
        let packets: Vec<Value> = self
            .research
            .submitted(project_id, Some(&format!("task:{requesting_task}")))?
            .into_iter()
            .map(|entry| entry["payload"].clone())
            .filter(|entry| {
                entry.get("$schema").and_then(Value::as_str)
                    == Some(schema_uri("task-packet").as_str())
                    && entry.get("taskId").and_then(Value::as_str) == Some(requesting_task.as_str())
            })
            .collect();
        if packets.len() != 1 {
            return self.reject_delegation(
                project_id,
                record,
                &request_hash,
                "Delegation request does not bind exactly one durable parent Task Packet",
                None,
            );
        }
        let parent = &packets[0];
        let (permissions, parent_budget, scope) =
            (&parent["permissions"], &parent["budget"], &parent["scope"]);
        let number = |value: &Value| value.as_f64().unwrap_or(f64::NAN);
        let maximum_tokens = number(&parent_budget["maximumModelTokens"]);
        let maximum_seconds = number(&parent_budget["maximumWallClockSeconds"]);
        let within = permissions.is_object()
            && permissions["delegation"] == "request_only"
            && parent_budget.is_object()
            && scope.is_object()
            && parent.get("assignedAgentId").and_then(Value::as_str)
                == Some(requesting_agent.as_str())
            && tokens <= maximum_tokens
            && seconds <= maximum_seconds;
        if !within {
            return self.reject_delegation(
                project_id,
                record,
                &request_hash,
                "Delegation request exceeds durable parent Task Packet authority",
                None,
            );
        }
        let checked = event_scope(project_id, scope, &requesting_agent).and_then(|_| {
            self.assert_delegation_parent_lease(project_id, parent, scope, &requesting_agent)
        });
        if let Err(error) = checked {
            return self.reject_delegation(
                project_id,
                record,
                &request_hash,
                &error_text(&error, "Delegation parent authority is invalid"),
                Some(scope),
            );
        }
        let mission_id = nullable_id(scope.get("missionId"));
        let siblings: Vec<Reservation> = store
            .operation_intents(project_id, None)?
            .iter()
            .filter(|intent| {
                intent["operationType"] == "delegation.reservation"
                    && (intent["state"] == "pending" || intent["state"] == "completed")
            })
            .filter_map(|intent| reservation(&intent["request"]))
            .filter(|reservation| {
                reservation.parent_task_id == requesting_task
                    && reservation.request_id != request_id
            })
            .collect();
        let reserved_tokens: f64 = siblings
            .iter()
            .map(|reservation| reservation.model_tokens)
            .sum();
        let reserved_seconds: f64 = siblings
            .iter()
            .map(|reservation| reservation.wall_clock_seconds)
            .sum();
        if reserved_tokens + tokens > maximum_tokens || reserved_seconds + seconds > maximum_seconds
        {
            return self.reject_delegation(
                project_id,
                record,
                &request_hash,
                "Delegation siblings exceed the durable parent Task Packet budget",
                Some(scope),
            );
        }
        if let Some(mission_id) = &mission_id {
            let mission = self.research.mission(project_id, mission_id)?;
            let usage = self.research.mission_budget_use(project_id, mission_id)?;
            let pending: f64 = store
                .operation_intents(project_id, Some("pending"))?
                .iter()
                .filter(|intent| intent["operationType"] == "delegation.reservation")
                .filter_map(|intent| reservation(&intent["request"]))
                .filter(|reservation| {
                    reservation.mission_id.as_deref() == Some(mission_id.as_str())
                })
                .map(|reservation| reservation.model_tokens)
                .sum();
            let budgets = &mission["value"]["budgets"];
            if number(&usage["modelTokens"]) + pending + tokens
                > number(&budgets["maximumModelTokens"])
            {
                return self.reject_delegation(
                    project_id,
                    record,
                    &request_hash,
                    "Delegation siblings exceed the Mission model-token budget",
                    Some(scope),
                );
            }
            if seconds
                > number(&budgets["maximumWallClockSeconds"]) - number(&usage["wallClockSeconds"])
            {
                return self.reject_delegation(
                    project_id,
                    record,
                    &request_hash,
                    "Delegation request exceeds the remaining Mission wall-clock deadline",
                    Some(scope),
                );
            }
        }
        let host_maximum = self.options.maximum_active_pi_sessions.unwrap_or(8.0);
        if host_maximum.fract() != 0.0
            || host_maximum < 1.0
            || self.active_authorized_sessions(project_id, None)?.len() as f64 >= host_maximum
        {
            return self.reject_delegation(
                project_id,
                record,
                &request_hash,
                "Host active Pi-session capacity is exhausted",
                Some(scope),
            );
        }
        if let Some(mission_id) = &mission_id {
            let concurrent = number(
                &self.research.mission(project_id, mission_id)?["value"]["budgets"]["maximumConcurrentAgents"],
            );
            if self
                .active_authorized_sessions(project_id, Some(mission_id))?
                .len() as f64
                >= concurrent
            {
                return self.reject_delegation(
                    project_id,
                    record,
                    &request_hash,
                    "Mission maximum concurrent-agent budget is exhausted",
                    Some(scope),
                );
            }
        }
        let reservation = store.begin_operation(
            project_id,
            "delegation.reservation",
            &format!("delegation-reservation:{request_id}"),
            &json!({ "requestId": request_id, "parentTaskId": requesting_task, "modelTokens": budget["modelTokens"], "wallClockSeconds": budget["wallClockSeconds"], "scope": scope }),
        )?;
        if reservation["state"] != "pending" {
            return self.reject_delegation(
                project_id,
                record,
                &request_hash,
                "Delegation reservation is already terminal without an authorization",
                Some(scope),
            );
        }
        let reservation_id = string(reservation.get("intentId"));
        let authorized = (|| -> Result<Value> {
            let child_task = deterministic_id("tsk", project_id, &request_id);
            let child_agent = deterministic_id("agt", project_id, &request_id);
            let child_thread = deterministic_id("thr", project_id, &request_id);
            let runtime_scope = event_scope(project_id, scope, &child_agent)?;
            let request = runtime_request(
                project_id,
                record,
                scope,
                &child_task,
                &child_agent,
                &child_thread,
            );
            let mut event_scope_value = runtime_scope.clone();
            event_scope_value["agentId"] = json!(requesting_agent);
            let mutation = store.mutate_projection(
                &format!("delegation-authorize:{request_id}"),
                Some(0),
                &event_draft("delegation.authorized", "noshd", event_scope_value, json!(request_id), Value::Null, json!({ "requestId": request_id, "childTaskId": child_task, "childAgentId": child_agent, "childThreadId": child_thread, "reservationIntentId": reservation_id })),
                &ProjectionInput {
                    entity_type: "delegation".into(),
                    entity_id: request_id.clone(),
                    state: "authorized".into(),
                    value: json!({ "requestHash": request_hash, "parentTaskId": requesting_task, "childTaskId": child_task, "childAgentId": child_agent, "childThreadId": child_thread, "runtimeScope": runtime_scope, "runtimeRequest": request, "reservationIntentId": reservation_id, "authorizedAt": now_utc() }),
                    graph: None,
                    records: Vec::new(),
                    intent: None,
                },
            )?;
            if !mutation.replayed {
                self.core.publish(&mutation.event);
            }
            self.resume_delegation(project_id, &request_id)?;
            Ok(mutation.projection["value"].clone())
        })();
        if let Err(error) = &authorized {
            self.fail_pending_operation(
                project_id,
                &reservation_id,
                &error_text(error, "Delegation operation failed"),
            )?;
        }
        authorized
    }

    fn resume_delegation(self: &Rc<Self>, project_id: &str, request_id: &str) -> Result<()> {
        let store = self.store_for(project_id)?;
        let Some(delegation) = store.projection(project_id, "delegation", request_id)? else {
            return Ok(());
        };
        if delegation["state"] != "authorized"
            || delegation["value"].get("runtimeRequest").is_none()
        {
            return Ok(());
        }
        let operation = store.begin_operation(
            project_id,
            "delegation.runtime",
            &format!("delegation-runtime:{request_id}"),
            &delegation["value"]["runtimeRequest"],
        )?;
        let key = run_key(project_id, request_id);
        if operation["state"] == "pending" && self.delegation_runs.borrow_mut().insert(key.clone())
        {
            let daemon = Rc::clone(self);
            let (project, request) = (project_id.to_owned(), request_id.to_owned());
            super::spawn_eager(async move {
                daemon
                    .run_delegation(
                        &project,
                        &request,
                        &string(operation.get("intentId")),
                        &operation["request"],
                    )
                    .await;
                daemon.delegation_runs.borrow_mut().remove(&key);
            });
        }
        Ok(())
    }

    async fn run_delegation(
        self: &Rc<Self>,
        project_id: &str,
        request_id: &str,
        intent_id: &str,
        request: &Value,
    ) {
        let outcome: Result<()> = async {
            let store = self.store_for(project_id)?;
            if !request.get("open").is_some_and(Value::is_object) || !request.get("step").is_some_and(Value::is_object) {
                return rejected("Delegation runtime intent is corrupt");
            }
            self.runtime.execute(&request["open"]).await?;
            let runtime_outcome = self.runtime.execute(&request["step"]).await?;
            let episode = runtime_outcome.get("episode").filter(|episode| episode.is_object() && episode["status"].is_string()).cloned();
            let Some(episode) = episode.filter(|_| runtime_outcome.is_object()) else {
                return rejected("Delegation runtime returned no Episode outcome");
            };
            let completed = episode["status"] == "completed";
            let current = store.projection(project_id, "delegation", request_id)?;
            let Some(current) = current.filter(|current| current["state"] == "authorized") else {
                return rejected("Delegation authority is no longer active");
            };
            let mut value = object(&current["value"], "Delegation projection is corrupt")?;
            let runtime_scope = durable_scope(value.get("runtimeScope").unwrap_or(&Value::Null))?;
            let terminal_state = if completed { "completed" } else { "failed" };
            let hash = terminal_hash(&value, terminal_state, &runtime_outcome, &episode, None)?;
            let child_task = value.get("childTaskId").cloned().unwrap_or(Value::Null);
            value.insert("runtimeOutcome".into(), runtime_outcome.clone());
            value.insert("episode".into(), episode.clone());
            value.insert("completedAt".into(), json!(now_utc()));
            value.insert("terminalHash".into(), json!(hash));
            let reservation = value.get("reservationIntentId").and_then(Value::as_str).map(str::to_owned);
            let outcome = store.mutate_projection(
                &format!("delegation-outcome:{request_id}"),
                current["version"].as_i64(),
                &event_draft(if completed { "delegation.completed" } else { "delegation.failed" }, "noshd", runtime_scope, json!(request_id), Value::Null, json!({ "requestId": request_id, "childTaskId": child_task, "runtimeOutcome": runtime_outcome, "episode": episode, "terminalHash": hash })),
                &ProjectionInput { entity_type: "delegation".into(), entity_id: request_id.to_owned(), state: terminal_state.into(), value: Value::Object(value), graph: None, records: Vec::new(), intent: None },
            )?;
            if let Some(reservation) = reservation {
                self.complete_pending_operation(project_id, &reservation, &json!({ "requestId": request_id, "terminalState": terminal_state }))?;
            }
            self.complete_pending_operation(project_id, intent_id, &runtime_outcome)?;
            if !outcome.replayed {
                self.core.publish(&outcome.event);
            }
            Ok(())
        }
        .await;
        let Err(error) = outcome else { return };
        let message = error_text(&error, "Delegated child failed");
        let _ = (|| -> Result<()> {
            let store = self.store_for(project_id)?;
            if let Some(current) = store.projection(project_id, "delegation", request_id)?
                && current["state"] == "authorized"
            {
                let mut value = object(&current["value"], "Delegation projection is corrupt")?;
                let runtime_scope =
                    durable_scope(value.get("runtimeScope").unwrap_or(&Value::Null))?;
                let hash =
                    terminal_hash(&value, "failed", &Value::Null, &Value::Null, Some(&message))?;
                value.insert("error".into(), json!(message));
                value.insert("completedAt".into(), json!(now_utc()));
                value.insert("terminalHash".into(), json!(hash));
                let reservation = value
                    .get("reservationIntentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let failed = store.mutate_projection(
                    &format!("delegation-outcome:{request_id}"),
                    current["version"].as_i64(),
                    &event_draft(
                        "delegation.failed",
                        "noshd",
                        runtime_scope,
                        json!(request_id),
                        Value::Null,
                        json!({ "requestId": request_id, "error": message, "terminalHash": hash }),
                    ),
                    &ProjectionInput {
                        entity_type: "delegation".into(),
                        entity_id: request_id.to_owned(),
                        state: "failed".into(),
                        value: Value::Object(value),
                        graph: None,
                        records: Vec::new(),
                        intent: None,
                    },
                )?;
                if let Some(reservation) = reservation {
                    self.complete_pending_operation(
                        project_id,
                        &reservation,
                        &json!({ "requestId": request_id, "terminalState": "failed" }),
                    )?;
                }
                if !failed.replayed {
                    self.core.publish(&failed.event);
                }
            }
            self.fail_pending_operation(project_id, intent_id, &message)
        })();
    }

    fn assert_delegation_parent_lease(
        &self,
        project_id: &str,
        parent: &Value,
        scope: &Value,
        agent_id: &str,
    ) -> Result<()> {
        let lease = &parent["lease"];
        let expires = lease
            .get("expiresAt")
            .and_then(Value::as_str)
            .map(crate::runtime::parse_ms);
        let live = lease.is_object()
            && lease["leaseId"].is_string()
            && expires.is_some_and(|expires| expires > crate::runtime::now_ms())
            && parent["observedVersions"].is_object()
            && scope["graphNodeId"].is_string();
        if !live {
            return rejected("Delegation parent Task Packet lacks a live lease authority");
        }
        let id = |field: &str| nullable_id(scope.get(field));
        let parent_task = parent.get("taskId").and_then(Value::as_str);
        let sessions = self
            .agents
            .inspect()
            .into_iter()
            .filter(|session| {
                session.project_id == project_id
                    && session.agent_id == agent_id
                    && session.task_id.as_deref() == parent_task
                    && session.mission_id == id("missionId")
                    && session.direction_id == id("directionId")
                    && session.autoresearch_id == id("autoresearchId")
                    && session.experiment_id == id("experimentId")
            })
            .count();
        if sessions != 1 {
            return rejected(
                "Delegation parent Pi session is not uniquely live in the Task Packet scope",
            );
        }
        let graph_node = string(scope.get("graphNodeId"));
        let lease_id = string(lease.get("leaseId"));
        let exact = |entity: &Value, version_key: &str| {
            let lease_version = &parent["observedVersions"][version_key];
            let node = entity["value"]["nodes"]
                .as_array()
                .and_then(|nodes| nodes.iter().find(|node| node["id"] == graph_node.as_str()));
            lease_version
                .as_f64()
                .is_some_and(|version| version.fract() == 0.0)
                && entity["version"].as_f64() == lease_version.as_f64()
                && node.is_some_and(|node| {
                    node["lease"].is_object()
                        && node["lease"]["leaseId"] == lease_id.as_str()
                        && node["lease"]["ownerId"] == agent_id
                        && ["leased", "working"].contains(&string(node.get("state")).as_str())
                })
        };
        if let Some(mission_id) = scope.get("missionId").and_then(Value::as_str) {
            if !exact(
                &self.research.mission(project_id, mission_id)?,
                "leaseVersion",
            ) {
                return rejected("Delegation parent Mission lease is no longer exact and live");
            }
            return Ok(());
        }
        if let Some(direction_id) = scope.get("directionId").and_then(Value::as_str) {
            if !exact(
                &self.research.direction(project_id, direction_id)?,
                "directionProjectionVersion",
            ) {
                return rejected("Delegation parent Direction lease is no longer exact and live");
            }
            return Ok(());
        }
        rejected("Delegation parent Task Packet has no Mission or Direction graph scope")
    }

    fn active_authorized_sessions(
        &self,
        project_id: &str,
        mission_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let store = self.store_for(project_id)?;
        let delegated: HashSet<String> = store
            .projections(project_id, "delegation")?
            .iter()
            .filter(|delegation| delegation["state"] == "authorized")
            .filter_map(|delegation| {
                delegation["value"]
                    .get("childAgentId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
            })
            .collect();
        let mut active: Vec<String> = Vec::new();
        for agent in self.agents.inspect() {
            if agent.project_id == project_id
                && mission_id.is_none_or(|mission| agent.mission_id.as_deref() == Some(mission))
                && !delegated.contains(&agent.agent_id)
            {
                active.push(format!("agent:{}", agent.agent_id));
            }
        }
        for intent in store.operation_intents(project_id, Some("pending"))? {
            if intent["operationType"] != "delegation.reservation" {
                continue;
            }
            if let Some(reservation) = reservation(&intent["request"])
                && mission_id
                    .is_none_or(|mission| reservation.mission_id.as_deref() == Some(mission))
            {
                active.push(format!("reservation:{}", reservation.request_id));
            }
        }
        let mut seen = HashSet::new();
        active.retain(|entry| seen.insert(entry.clone()));
        Ok(active)
    }

    fn complete_pending_operation(
        &self,
        project_id: &str,
        intent_id: &str,
        result: &Value,
    ) -> Result<()> {
        let store = self.store_for(project_id)?;
        let pending = store
            .operation_intents(project_id, None)?
            .iter()
            .any(|intent| intent["intentId"] == intent_id && intent["state"] == "pending");
        if pending {
            store.complete_operation(project_id, intent_id, result)?;
        }
        Ok(())
    }

    fn fail_pending_operation(&self, project_id: &str, intent_id: &str, error: &str) -> Result<()> {
        let store = self.store_for(project_id)?;
        let pending = store
            .operation_intents(project_id, None)?
            .iter()
            .any(|intent| intent["intentId"] == intent_id && intent["state"] == "pending");
        if pending {
            store.fail_operation(project_id, intent_id, error)?;
        }
        Ok(())
    }

    pub(super) fn reconcile_delegation_reservations(&self, project_id: &str) -> Result<()> {
        let store = self.store_for(project_id)?;
        for intent in store.operation_intents(project_id, Some("pending"))? {
            if intent["operationType"] != "delegation.reservation" {
                continue;
            }
            let intent_id = string(intent.get("intentId"));
            let Some(reservation) = reservation(&intent["request"]) else {
                self.fail_pending_operation(
                    project_id,
                    &intent_id,
                    "Delegation reservation is corrupt",
                )?;
                continue;
            };
            let Some(delegation) =
                store.projection(project_id, "delegation", &reservation.request_id)?
            else {
                self.fail_pending_operation(
                    project_id,
                    &intent_id,
                    "Delegation reservation has no durable authorization",
                )?;
                continue;
            };
            let state = string(delegation.get("state"));
            match state.as_str() {
                "authorized" => {}
                "completed" | "failed" => self.complete_pending_operation(
                    project_id,
                    &intent_id,
                    &json!({ "requestId": reservation.request_id, "terminalState": state }),
                )?,
                other => self.fail_pending_operation(
                    project_id,
                    &intent_id,
                    &format!("Delegation reservation has unexpected {other} authority"),
                )?,
            }
        }
        Ok(())
    }

    /// Records a durable refusal and returns it as an error.
    fn reject_delegation(
        &self,
        project_id: &str,
        record: &Value,
        request_hash: &str,
        message: &str,
        parent_scope: Option<&Value>,
    ) -> Result<Value> {
        let request_id = string(record.get("requestId"));
        let store = self.store_for(project_id)?;
        let runtime_scope = rejection_scope(
            project_id,
            parent_scope,
            &string(record.get("requestingAgentId")),
        );
        let terminal = json!({ "requestHash": request_hash, "state": "rejected", "runtimeScope": runtime_scope, "error": message });
        let hash = sha256(&terminal);
        if let Some(existing) = store.projection(project_id, "delegation", &request_id)? {
            if existing["state"] == "rejected" {
                let value = asserted_rejection(
                    object(&existing["value"], "Delegation projection is corrupt")?,
                    request_hash,
                )?;
                return rejected(string(value.get("error")));
            }
            return rejected("Delegation request ID already has a non-rejected durable authority");
        }
        let key = format!("delegation-reservation:{request_id}");
        if let Some(intent) = store
            .operation_intents(project_id, None)?
            .into_iter()
            .find(|intent| intent["idempotencyKey"] == key.as_str())
        {
            self.fail_pending_operation(project_id, &string(intent.get("intentId")), message)?;
        }
        let mut value = terminal.clone();
        value["terminalHash"] = json!(hash);
        let refused = store.mutate_projection(
            &format!("delegation-reject:{request_id}"),
            Some(0),
            &event_draft("delegation.rejected", "noshd", runtime_scope, json!(request_id), Value::Null, json!({ "requestId": request_id, "requestHash": request_hash, "terminalHash": hash, "error": message })),
            &ProjectionInput { entity_type: "delegation".into(), entity_id: request_id.clone(), state: "rejected".into(), value, graph: None, records: Vec::new(), intent: None },
        )?;
        if !refused.replayed {
            self.core.publish(&refused.event);
        }
        rejected(message)
    }

    /// Re-authorizes every delegation request on record: `(authorized, rejected)` counts.
    pub(super) fn recover_delegations(self: &Rc<Self>, project_id: &str) -> Result<(usize, usize)> {
        let store = self.store_for(project_id)?;
        let (mut recovered, mut refused) = (0, 0);
        for entry in self.research.submitted(project_id, None)? {
            let record = &entry["payload"];
            if record.get("$schema").and_then(Value::as_str)
                != Some(schema_uri("delegation-request").as_str())
            {
                continue;
            }
            let request_id = string(record.get("requestId"));
            match self.authorize_delegation(project_id, record) {
                Ok(_) => match store
                    .projection(project_id, "delegation", &request_id)?
                    .map(|projection| string(projection.get("state")))
                    .as_deref()
                {
                    Some("authorized") => recovered += 1,
                    Some("rejected") => refused += 1,
                    _ => {}
                },
                Err(error) => {
                    let state = store
                        .projection(project_id, "delegation", &request_id)?
                        .map(|projection| string(projection.get("state")));
                    if state.as_deref() != Some("rejected") {
                        let message = error_text(&error, "Delegation recovery failed");
                        self.append_draft(&event_draft(
                            "recovery.delegation_failed",
                            "noshd",
                            entry["scope"].clone(),
                            json!(request_id),
                            entry["eventId"].clone(),
                            json!({ "message": message }),
                        ))?;
                    }
                }
            }
        }
        Ok((recovered, refused))
    }
}
