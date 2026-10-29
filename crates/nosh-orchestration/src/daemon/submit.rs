//! Typed submissions, as `submitRecord`, `submitTool`, and `executeRuntimeEffect` in
//! `apps/noshd/src/daemon.ts`.

use super::{NoshDaemon, error_text};
use crate::{Error, Result, rejected};
use nosh_agent::pi::Inspection;
use nosh_research::project::approve_project_contract;
use nosh_research::research_control::DomainSession;
use nosh_wire::js::string;
use nosh_wire::{EVENT_SCHEMA, is_task_terminal_record, sha256};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::rc::Rc;

fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

/// The research control's view of a Pi session.
pub(crate) fn domain_session(agent: &Inspection) -> DomainSession {
    DomainSession {
        agent_id: agent.agent_id.clone(),
        role: agent.role.as_str().to_owned(),
        task_id: agent.task_id.clone(),
        mission_id: agent.mission_id.clone(),
        direction_id: agent.direction_id.clone(),
        autoresearch_id: agent.autoresearch_id.clone(),
        experiment_id: agent.experiment_id.clone(),
        run_id: agent.run_id.clone(),
        job_id: agent.job_id.clone(),
    }
}

/// A record whose Project or actor does not match its daemon-bound session: `(pointer, message)`.
pub(crate) fn submission_scope_error(
    record: &Value,
    project_id: &str,
    agent_id: Option<&str>,
) -> Option<(String, String)> {
    let value = record.as_object()?;
    if let Some(Value::String(project)) = value.get("projectId")
        && project != project_id
    {
        return Some((
            "/projectId".into(),
            "Record Project does not match the daemon-bound session".into(),
        ));
    }
    if let Some(Value::String(project)) =
        value.get("scope").and_then(|scope| scope.get("projectId"))
        && project != project_id
    {
        return Some((
            "/scope/projectId".into(),
            "Record scope does not match the daemon-bound session".into(),
        ));
    }
    let agent_id = agent_id.filter(|id| !id.is_empty())?;
    let schema = value
        .get("$schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let field = [
        ("response-envelope", "agentId"),
        ("task-acknowledgement", "agentId"),
        ("progress-update", "agentId"),
        ("delegation-request", "requestingAgentId"),
        ("review-verdict", "reviewerAgentId"),
        ("experiment-proposal", "proposedByAgentId"),
        ("evidence", "createdByAgentId"),
        ("handoff", "fromAgentId"),
        ("handoff-teachback", "toAgentId"),
        ("mission-director-cycle", "directorAgentId"),
        ("research-director-cycle", "directorAgentId"),
        ("runtime-instruction", "proposedByAgentId"),
    ]
    .iter()
    .find(|(name, _)| schema == schema_uri(name))
    .map(|(_, field)| *field)?;
    if value.get(field).and_then(Value::as_str) != Some(agent_id) {
        return Some((
            format!("/{field}"),
            "Record actor does not match the daemon-bound Pi session".into(),
        ));
    }
    None
}

/// The task whose single terminal record this submission is, when it is one.
fn terminal_outcome_task_id(
    tool: &str,
    schema: &str,
    attempt_key: &str,
    record: &Value,
) -> Option<String> {
    let verdict = schema_uri("review-verdict");
    let owned = (tool == "nosh_response_submit" && schema != verdict)
        || (tool == "nosh_review_submit" && schema == verdict)
        || (tool == "nosh_experiment_propose" && schema == schema_uri("experiment-proposal"));
    if !is_task_terminal_record(record) || !owned {
        return None;
    }
    let is_task = |id: &str| {
        id.len() == 36
            && id.starts_with("tsk_")
            && id[4..]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    if let Some(task) = attempt_key
        .strip_prefix("task:")
        .filter(|task| is_task(task))
    {
        return Some(task.to_owned());
    }
    record
        .get("taskId")
        .and_then(Value::as_str)
        .filter(|task| is_task(task))
        .map(str::to_owned)
}

fn refusal(retry: bool, pointer: &str, code: &str, message: impl Into<String>) -> Value {
    json!({ "accepted": false, "retryAllowed": retry, "errors": [{ "pointer": pointer, "code": code, "message": message.into() }] })
}

impl NoshDaemon {
    /// Accepts one typed record: `{ accepted: true, event, effect? }` or
    /// `{ accepted: false, retryAllowed, errors }`.
    pub fn submit_record(
        &self,
        tool: &str,
        project_id: &str,
        attempt_key: &str,
        record: &Value,
        agent_id: Option<&str>,
    ) -> Result<Value> {
        self.assert_project_writable(project_id)?;
        if let Some((pointer, message)) = submission_scope_error(record, project_id, agent_id) {
            return Ok(refusal(false, &pointer, "scope_policy", message));
        }
        let result = self
            .gate
            .borrow_mut()
            .submit(tool, &format!("{attempt_key}:{tool}"), record);
        if result["ok"] != true {
            return Ok(
                json!({ "accepted": false, "retryAllowed": result["retryAllowed"], "errors": result["errors"] }),
            );
        }
        let record = &result["record"];
        let agent = match agent_id.filter(|id| !id.is_empty()) {
            Some(agent_id) => {
                let found = self
                    .agents
                    .inspect()
                    .into_iter()
                    .find(|agent| agent.agent_id == agent_id && agent.project_id == project_id);
                if found.is_none() {
                    return Ok(refusal(
                        false,
                        "/",
                        "scope_policy",
                        "Submitting Pi session is no longer active in this Project",
                    ));
                }
                found
            }
            None => None,
        };
        if tool == "nosh_project_contract_submit" {
            if agent
                .as_ref()
                .is_none_or(|agent| agent.role.as_str() != "nosh")
            {
                return Ok(refusal(
                    false,
                    "/",
                    "scope_policy",
                    "Only the Project Nosh session may submit the approved Project contract",
                ));
            }
            if record.to_string().contains("\"EDIT:") {
                return Ok(refusal(
                    true,
                    "/",
                    "template_placeholder",
                    "Replace every \"EDIT: …\" placeholder of the successor template with the agreed content before submitting",
                ));
            }
            let repository_root =
                PathBuf::from(string(self.core.project(project_id)?.get("repositoryRoot")));
            let approved = approve_project_contract(&repository_root, record)
                .map_err(Error::from)
                .and_then(|_| {
                    self.record_contract_commit(
                        project_id,
                        &repository_root,
                        &format!("{attempt_key}:contract-commit"),
                    )
                });
            if let Err(error) = approved {
                return Ok(refusal(
                    false,
                    "/",
                    "contract_policy",
                    error_text(&error, "Project contract approval failed"),
                ));
            }
        }
        if self.research.is_domain_effect_record(record) && agent.is_none() {
            return Ok(refusal(
                false,
                "/",
                "domain_policy",
                "Domain-effect records require an active matching Pi agent",
            ));
        }
        let session = agent.as_ref().map(domain_session);
        if let Some(session) = &session
            && let Err(error) = self
                .research
                .validate_domain_record(project_id, session, record)
        {
            return Ok(refusal(
                false,
                "/",
                "domain_policy",
                error_text(&Error::from(error), "Domain record violates task authority"),
            ));
        }
        let canonical = match self.research.canonical_domain_record(project_id, record) {
            Ok(canonical) => canonical,
            Err(error) => {
                return Ok(refusal(
                    false,
                    "/",
                    "domain_policy",
                    error_text(
                        &Error::from(error),
                        "Domain record conflicts with an accepted canonical payload",
                    ),
                ));
            }
        };
        let id = |field: fn(&Inspection) -> &Option<String>| {
            agent.as_ref().and_then(|agent| field(agent).clone())
        };
        let scope = json!({ "projectId": project_id, "missionId": id(|a| &a.mission_id), "directionId": id(|a| &a.direction_id), "autoresearchId": id(|a| &a.autoresearch_id), "experimentId": id(|a| &a.experiment_id), "runId": id(|a| &a.run_id), "jobId": id(|a| &a.job_id), "agentId": agent.as_ref().map(|agent| agent.agent_id.clone()) });
        let draft = json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "pi", "scope": scope, "correlationId": attempt_key, "causationId": null, "payload": record });
        // String(record.$schema ?? "")
        let schema = match record.get("$schema") {
            None | Some(Value::Null) => String::new(),
            schema => string(schema),
        };
        let stored = match &canonical {
            Some(canonical) => Ok((canonical["event"].clone(), true)),
            None => {
                let store = self.store_for(project_id)?;
                let appended = match terminal_outcome_task_id(tool, &schema, attempt_key, record) {
                    Some(task_id) => {
                        store.append_terminal_submission(&task_id, tool, &schema, record, &draft)
                    }
                    None => store.append_idempotent(
                        &format!("record:{attempt_key}:{tool}:{}", sha256(record)),
                        &draft,
                    ),
                };
                appended.map(|appended| (appended.receipt["event"].clone(), appended.replayed))
            }
        };
        let (event, replayed) = match stored {
            Ok(stored) => stored,
            Err(error) => {
                return Ok(refusal(
                    false,
                    "/",
                    "terminal_conflict",
                    error_text(
                        &Error::from(error),
                        "Terminal submission conflicts with an accepted record",
                    ),
                ));
            }
        };
        if !replayed {
            self.core.publish(&event);
        }
        if let Some(session) = &session
            && let Err(error) = self
                .research
                .apply_domain_effect(project_id, session, record)
        {
            let message = error_text(
                &Error::from(error),
                "Domain effect failed after record acceptance",
            );
            return Ok(
                json!({ "accepted": true, "event": event, "effect": { "state": "failed", "error": message } }),
            );
        }
        Ok(json!({ "accepted": true, "event": event }))
    }

    /// A session tool's daemon side: `nosh_run`, progress notes, Artifact reads, and typed
    /// submissions with their delegation and runtime effects.
    pub async fn submit_tool(
        self: &Rc<Self>,
        tool: &str,
        project_id: &str,
        attempt_key: &str,
        record: &Value,
        agent_id: Option<&str>,
    ) -> Result<Value> {
        match tool {
            "nosh_run" => return Ok(self.run_task_command(project_id, agent_id, record).await),
            "nosh_progress_note" => {
                return self.record_progress_note(project_id, attempt_key, agent_id, record);
            }
            "nosh_artifact_read" => {
                return Ok(self.read_task_artifact(project_id, agent_id, record));
            }
            _ => {}
        }
        let mut submitted = self.submit_record(tool, project_id, attempt_key, record, agent_id)?;
        if submitted["accepted"] != true {
            return Ok(submitted);
        }
        if tool == "nosh_delegation_request" && record.is_object() {
            submitted["delegation"] = match self.authorize_delegation(project_id, record) {
                Ok(delegation) => delegation,
                Err(error) => {
                    json!({ "state": "failed", "error": error_text(&error, "Delegation authorization failed") })
                }
            };
            return Ok(submitted);
        }
        if tool != "nosh_runtime_instruct" {
            return Ok(submitted);
        }
        let request = json!({ "instruction": record });
        let store = self.store_for(project_id)?;
        let operation = store.begin_operation(
            project_id,
            "runtime.instruction",
            &format!("tool-effect:{attempt_key}:{tool}"),
            &request,
        )?;
        let state = string(operation.get("state"));
        if state == "completed" || state == "failed" {
            submitted["effect"] = if operation["result"].is_null() {
                json!({ "state": state, "error": operation["error"] })
            } else {
                json!({ "state": state, "result": operation["result"] })
            };
            return Ok(submitted);
        }
        let intent_id = string(operation.get("intentId"));
        match self
            .execute_runtime_effect(&string(operation.get("operationType")), &request)
            .await
        {
            Ok(result) => {
                self.store_for(project_id)?
                    .complete_operation(project_id, &intent_id, &result)?;
                submitted["effect"] = json!({ "state": "completed", "result": result });
            }
            Err(error) => {
                let message = error_text(&error, "Runtime effect failed");
                self.store_for(project_id)?
                    .fail_operation(project_id, &intent_id, &message)?;
                submitted["effect"] = json!({ "state": "failed", "error": message });
            }
        }
        Ok(submitted)
    }

    pub(super) fn execute_runtime_effect(
        self: &Rc<Self>,
        operation_type: &str,
        request: &Value,
    ) -> futures_util::future::LocalBoxFuture<'static, Result<Value>> {
        if !request.is_object() {
            return Box::pin(async { rejected("Runtime effect request is invalid") });
        }
        if operation_type != "runtime.instruction" {
            let message = format!("Unsupported runtime effect {operation_type}");
            return Box::pin(async move { rejected(message) });
        }
        let instruction = request.get("instruction").cloned().unwrap_or(Value::Null);
        Box::pin(self.runtime.execute(&instruction))
    }
}
