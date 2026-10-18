//! Domain-effect records (progress, blockers, graph proposals, delegation requests, handoffs and
//! teach-backs): their validation against Task authority, their effects, and recovery of accepted
//! records whose effect was not applied.

use super::*;
use crate::agent_runtime::validate_teachback;
use crate::graph::parse_ms;
use nosh_wire::{now_utc, sha256};
use serde::{Deserialize, Serialize};

/// The Pi session a domain record arrives through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainSession {
    pub agent_id: String,
    /// `nosh`, `user`, `mission_director`, `research_director`, `librarian_researcher`,
    /// `general_worker`, or `reviewer`.
    pub role: String,
    pub task_id: Option<String>,
    pub mission_id: Option<String>,
    pub direction_id: Option<String>,
    pub autoresearch_id: Option<String>,
    pub experiment_id: Option<String>,
    pub run_id: Option<String>,
    pub job_id: Option<String>,
}

/// What a durable Task Packet authorises its agent to do.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskAuthority {
    graph_node_id: String,
    mission_id: Option<String>,
    direction_id: Option<String>,
    lease_id: String,
    lease_projection_version: f64,
    agent_id: String,
    maximum_model_tokens: f64,
    maximum_wall_clock_seconds: f64,
    delegation: String,
}

const ROLES: &[&str] = &[
    "nosh",
    "user",
    "mission_director",
    "research_director",
    "librarian_researcher",
    "general_worker",
    "reviewer",
];

/// A string field as an ID (`None` for null or anything else).
fn id(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// `value === id`, where `id` is `string | null`.
fn same_id(value: Option<&Value>, id: &Option<String>) -> bool {
    match id {
        Some(id) => is(value, id),
        None => value == Some(&Value::Null),
    }
}

fn truthy_id(id: &Option<String>) -> Option<&str> {
    id.as_deref().filter(|id| !id.is_empty())
}

impl<H: Host> ResearchControl<H> {
    pub fn validate_domain_record(
        &self,
        project_id: &str,
        session: &DomainSession,
        record: &Value,
    ) -> Result<()> {
        if !record.is_object() {
            return Ok(());
        }
        // Daemon-owned identity in a proposal must match the execution exactly; reject with the
        // expected value so the single correction turn can fix a mis-copied hash instead of
        // persisting an unusable proposal.
        if is_schema(record, "experiment-proposal") {
            let execution =
                self.autoresearch_execution(project_id, &string(record.get("autoresearchId")))?;
            let hash = &execution["value"]["evaluationContractHash"];
            if !same(record.get("evaluationContractHash"), Some(hash)) {
                return rejected(format!(
                    "evaluationContractHash must be exactly {}",
                    string(Some(hash))
                ));
            }
            if let Some(experiment) = truthy_id(&session.experiment_id)
                && !is(record.get("experimentId"), experiment)
            {
                return rejected(format!("experimentId must be exactly {experiment}"));
            }
        }
        if is_schema(record, "progress-update") {
            let packet = self.task_authority(project_id, session, &string(record.get("taskId")))?;
            if !is(record.get("agentId"), &session.agent_id) {
                return rejected("Progress record actor does not own the task");
            }
            return self.assert_working_lease(project_id, &packet, session);
        }
        if is_schema(record, "blocker") {
            let packet = self.task_authority(project_id, session, &string(record.get("taskId")))?;
            if !is(record.get("scopeType"), "graph_node")
                || !is(record.get("scopeId"), &packet.graph_node_id)
            {
                return rejected("Blocker must be scoped to its assigned graph node");
            }
            return self.assert_live_lease(project_id, &packet, session);
        }
        if is_schema(record, "delegation-request") {
            let authority =
                self.task_authority(project_id, session, &string(record.get("requestingTaskId")))?;
            self.assert_live_lease(project_id, &authority, session)?;
            let estimate = &record["budgetEstimate"];
            let within = match (
                number(estimate.get("modelTokens")),
                number(estimate.get("wallClockSeconds")),
            ) {
                (Some(tokens), Some(seconds)) => {
                    estimate.is_object()
                        && tokens <= authority.maximum_model_tokens
                        && seconds <= authority.maximum_wall_clock_seconds
                }
                _ => false,
            };
            if authority.delegation != "request_only"
                || !is(record.get("requestingAgentId"), &session.agent_id)
                || !within
            {
                return rejected(
                    "Delegation request exceeds the parent task budget or actor authority",
                );
            }
            return Ok(());
        }
        if is_schema(record, "graph-change-proposal") {
            if !is(record.get("proposerRole"), &session.role)
                || !["mission_director", "research_director", "user"]
                    .contains(&session.role.as_str())
            {
                return rejected("Graph proposal role does not match the submitting authority");
            }
            if is(record.get("contractImpact"), "material")
                && record.get("approvalRequired") != Some(&Value::Bool(true))
            {
                return rejected("Material graph proposals require explicit user approval");
            }
            let nodes = match (
                record["scopeType"].as_str(),
                truthy_id(&session.mission_id),
                truthy_id(&session.direction_id),
            ) {
                (Some("mission"), Some(mission_id), _) if is(record.get("scopeId"), mission_id) => {
                    let mission = self.mission(project_id, mission_id)?;
                    if !same(
                        mission["value"].get("graphVersion"),
                        record.get("baseGraphVersion"),
                    ) {
                        return rejected("Graph proposal uses a stale Mission graph version");
                    }
                    if mission["value"].get("approvedGraphVersion") != Some(&Value::Null)
                        && record.get("approvalRequired") != Some(&Value::Bool(true))
                    {
                        return rejected(
                            "Mission graph changes after approval require explicit user approval",
                        );
                    }
                    mission["value"]["nodes"].clone()
                }
                (Some("direction"), _, Some(direction_id))
                    if is(record.get("scopeId"), direction_id) =>
                {
                    let direction = self.direction(project_id, direction_id)?;
                    if !same(
                        direction["value"].get("graphVersion"),
                        record.get("baseGraphVersion"),
                    ) {
                        return rejected("Graph proposal uses a stale Direction graph version");
                    }
                    direction["value"]["nodes"].clone()
                }
                _ => return rejected("Graph proposal is outside the submitting session scope"),
            };
            entities::graph_operations(
                record.get("operations"),
                nodes.as_array().map_or(&[][..], Vec::as_slice),
            )?;
            return Ok(());
        }
        if is_schema(record, "handoff") {
            let packet = self.task_authority(
                project_id,
                session,
                session.task_id.as_deref().unwrap_or_default(),
            )?;
            self.assert_live_lease(project_id, &packet, session)?;
            let goal_stack = &record["goalStack"];
            if !is(record.get("fromAgentId"), &session.agent_id)
                || !is_str(record.get("toAgentId"))
                || is(record.get("toAgentId"), &session.agent_id)
                || !is(record.get("oldLeaseReleaseId"), &packet.lease_id)
                || !record["scope"].is_object()
                || !same_scope(&record["scope"], session, true)
                || !goal_stack.is_object()
                || !is(goal_stack.get("currentGraphNodeId"), &packet.graph_node_id)
            {
                return rejected(
                    "Handoff must name a distinct recipient and preserve its exact live source Task Packet scope, lease, and graph node",
                );
            }
            return Ok(());
        }
        if is_schema(record, "handoff-teachback") {
            let handoff = self.pending_handoff(
                project_id,
                &string(record.get("handoffId")),
                &event_scope(project_id, session),
                &string(record.get("toAgentId")),
            )?;
            if !is(record.get("toAgentId"), &session.agent_id)
                || !same(record.get("logicalOwnerId"), handoff.get("logicalOwnerId"))
            {
                return rejected("Teach-back is not addressed to the durable handoff recipient");
            }
            let checked = validate_teachback(
                &handoff_state(&handoff["record"])?,
                &teachback(record, string(record.get("decision")))?,
            );
            if checked["ok"] != true {
                return rejected(format!(
                    "Teach-back does not exactly match the handoff: {}",
                    conflicts(&checked)
                ));
            }
        }
        Ok(())
    }

    pub fn apply_domain_effect(
        &self,
        project_id: &str,
        session: &DomainSession,
        record: &Value,
    ) -> Result<()> {
        if record.is_object() {
            // Binds the record to its accepted canonical payload.
            self.canonical_domain_record(project_id, record)?;
        }
        if is_schema(record, "handoff")
            && let Some(handoff_id) = record.get("handoffId").and_then(Value::as_str)
            && let Some(existing) = self
                .store(project_id)?
                .projection(project_id, "handoff", handoff_id)?
        {
            return assert_pending_handoff_record(&existing["value"], record);
        }
        if is_schema(record, "handoff-teachback")
            && let Some(handoff_id) = record.get("handoffId").and_then(Value::as_str)
            && let Some(terminal) =
                self.store(project_id)?
                    .projection(project_id, "handoff_decision", handoff_id)?
        {
            return assert_handoff_decision_record(&terminal["value"], record);
        }
        if let Some(effect_key) = domain_effect_key(record)
            && self
                .store(project_id)?
                .command_receipt(project_id, &effect_key)?
                .is_some()
        {
            return Ok(());
        }
        self.validate_domain_record(project_id, session, record)?;
        self.apply_validated_domain_effect(project_id, session, record, None)
    }

    /// The first accepted event carrying this domain record's effect key; an accepted record with
    /// that key but a different payload is a conflict.
    pub fn canonical_domain_record(
        &self,
        project_id: &str,
        record: &Value,
    ) -> Result<Option<Value>> {
        if !record.is_object() {
            return Ok(None);
        }
        let Some(key) = domain_effect_key(record) else {
            return Ok(None);
        };
        let payload_hash = sha256(record);
        let mut canonical = None;
        for accepted in self.submitted(project_id, None)? {
            if domain_effect_key(&accepted["payload"]).as_deref() != Some(key.as_str()) {
                continue;
            }
            if sha256(&accepted["payload"]) != payload_hash {
                return rejected(format!(
                    "Domain record {key} conflicts with its accepted canonical payload"
                ));
            }
            canonical.get_or_insert(accepted);
        }
        Ok(canonical)
    }

    /// Applies the effect of every accepted domain record that lacks one (after a crash between
    /// accepting a record and applying it). Returns `{ recovered, failed }`.
    pub fn reconcile_accepted_domain_effects(&self, project_id: &str) -> Result<Value> {
        let (mut recovered, mut failed) = (0, 0);
        for accepted in self.submitted(project_id, None)? {
            let record = &accepted["payload"];
            if !record.is_object() {
                continue;
            }
            let Some(effect_key) = domain_effect_key(record) else {
                continue;
            };
            if self
                .store(project_id)?
                .command_receipt(project_id, &effect_key)?
                .is_some()
            {
                continue;
            }
            let applied = self
                .recover_domain_authority(project_id, &accepted, record)
                .and_then(|(session, task)| {
                    self.apply_validated_domain_effect(project_id, &session, record, task.as_ref())
                });
            match applied {
                Ok(()) => recovered += 1,
                Err(error) => {
                    failed += 1;
                    let event_id = &accepted["eventId"];
                    let draft = json!({
                        "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "recovery.domain_effect_failed", "source": "noshd",
                        "scope": accepted["scope"], "correlationId": event_id, "causationId": event_id,
                        "payload": { "recordEventId": event_id, "schema": record.get("$schema").filter(|schema| schema.is_string()).cloned().unwrap_or(Value::Null), "effectKey": effect_key, "message": error.to_string() },
                    });
                    let stored = self.store(project_id)?.append_idempotent(
                        &format!("recovery:domain-effect:{}", string(Some(event_id))),
                        &draft,
                    )?;
                    if !stored.replayed {
                        self.host.publish(&stored.receipt["event"]);
                    }
                }
            }
        }
        Ok(json!({ "recovered": recovered, "failed": failed }))
    }

    fn recover_domain_authority(
        &self,
        project_id: &str,
        event: &Value,
        record: &Value,
    ) -> Result<(DomainSession, Option<TaskAuthority>)> {
        let scope = &event["scope"];
        let Some(agent_id) = scope
            .get("agentId")
            .and_then(Value::as_str)
            .filter(|agent| !agent.is_empty())
        else {
            return rejected("Accepted domain record has no durable submitting agent");
        };
        let scoped = |agent_id: String, role: String, task_id: Option<String>, scope: &Value| {
            DomainSession {
                agent_id,
                role,
                task_id,
                mission_id: id(scope.get("missionId")),
                direction_id: id(scope.get("directionId")),
                autoresearch_id: id(scope.get("autoresearchId")),
                experiment_id: id(scope.get("experimentId")),
                run_id: id(scope.get("runId")),
                job_id: id(scope.get("jobId")),
            }
        };
        if is_schema(record, "handoff-teachback") {
            let (Some(to_agent), Some(handoff_id)) = (
                record
                    .get("toAgentId")
                    .and_then(Value::as_str)
                    .filter(|to| *to == agent_id),
                record.get("handoffId").and_then(Value::as_str),
            ) else {
                return rejected("Teach-back recipient does not match the durable event scope");
            };
            let handoff = self.pending_handoff(project_id, handoff_id, scope, to_agent)?;
            return Ok((
                scoped(
                    string(handoff.get("recipientAgentId")),
                    string(handoff["source"].get("role")),
                    None,
                    &handoff["scope"],
                ),
                None,
            ));
        }
        if is_schema(record, "graph-change-proposal") {
            let role = string(record.get("proposerRole"));
            if (is(record.get("scopeType"), "mission")
                && !same(record.get("scopeId"), scope.get("missionId")))
                || (is(record.get("scopeType"), "direction")
                    && !same(record.get("scopeId"), scope.get("directionId")))
                || !["mission_director", "research_director", "user"].contains(&role.as_str())
                || !is_str(record.get("proposerRole"))
            {
                return rejected("Graph proposal does not match its durable accepted scope");
            }
            return Ok((scoped(agent_id.to_owned(), role, None, scope), None));
        }
        let record_agent = if is_schema(record, "progress-update") {
            record.get("agentId")
        } else if is_schema(record, "handoff") {
            record.get("fromAgentId")
        } else {
            scope.get("agentId")
        };
        if !is(record_agent, agent_id) {
            return rejected("Accepted domain record actor does not match durable event scope");
        }
        let task_id = id(record.get("taskId"));
        let graph_node_id = record["goalStack"]
            .get("currentGraphNodeId")
            .and_then(Value::as_str)
            .filter(|_| record["goalStack"].is_object())
            .or_else(|| {
                record["understoodGoalStack"]
                    .get("currentGraphNodeId")
                    .and_then(Value::as_str)
                    .filter(|_| record["understoodGoalStack"].is_object())
            });
        let packets: Vec<Value> = self
            .submitted_records(project_id, None)?
            .into_iter()
            .filter(|packet| is_schema(packet, "task-packet"))
            .filter(|packet| {
                let packet_scope = &packet["scope"];
                is(packet.get("assignedAgentId"), agent_id)
                    && packet_scope.is_object()
                    && is(packet_scope.get("projectId"), project_id)
                    && ["missionId", "directionId", "autoresearchId", "experimentId"]
                        .iter()
                        .all(|field| same(packet_scope.get(*field), scope.get(*field)))
            })
            .filter(|packet| match (&task_id, graph_node_id) {
                (Some(task_id), _) if !task_id.is_empty() => is(packet.get("taskId"), task_id),
                (_, Some(node)) if !node.is_empty() => is(packet["scope"].get("graphNodeId"), node),
                _ => false,
            })
            .collect();
        let [packet] = packets.as_slice() else {
            return rejected(
                "Accepted domain record cannot be bound to exactly one durable Task Packet",
            );
        };
        let (budget, permissions, observed) = (
            &packet["budget"],
            &packet["permissions"],
            &packet["observedVersions"],
        );
        if !is_str(packet.get("taskId"))
            || !is_str(packet.get("assignedRole"))
            || !packet["scope"].is_object()
            || !packet["lease"].is_object()
            || !is_str(packet["lease"].get("leaseId"))
            || !observed.is_object()
            || !budget.is_object()
            || number(budget.get("maximumModelTokens")).is_none()
            || number(budget.get("maximumWallClockSeconds")).is_none()
            || !permissions.is_object()
            || !is_str(permissions.get("delegation"))
            || !is_str(packet["scope"].get("graphNodeId"))
        {
            return rejected("Durable Task Packet lacks recoverable authority");
        }
        let lease_version = lease_projection_version(
            observed,
            truthy_id(&id(scope.get("missionId"))).is_some(),
            truthy_id(&id(scope.get("directionId"))).is_some(),
        );
        let Some(lease_version) = lease_version else {
            return rejected("Durable Task Packet lacks its exact lease projection version");
        };
        let session = scoped(
            agent_id.to_owned(),
            string(packet.get("assignedRole")),
            id(packet.get("taskId")),
            scope,
        );
        let task = TaskAuthority {
            graph_node_id: string(packet["scope"].get("graphNodeId")),
            mission_id: id(scope.get("missionId")),
            direction_id: id(scope.get("directionId")),
            lease_id: string(packet["lease"].get("leaseId")),
            lease_projection_version: lease_version,
            agent_id: agent_id.to_owned(),
            maximum_model_tokens: number(budget.get("maximumModelTokens")).expect("checked"),
            maximum_wall_clock_seconds: number(budget.get("maximumWallClockSeconds"))
                .expect("checked"),
            delegation: if is(permissions.get("delegation"), "request_only") {
                "request_only"
            } else {
                "disabled"
            }
            .into(),
        };
        Ok((session, Some(task)))
    }

    fn apply_validated_domain_effect(
        &self,
        project_id: &str,
        session: &DomainSession,
        record: &Value,
        recovered_task: Option<&TaskAuthority>,
    ) -> Result<()> {
        self.host.assert_writable(project_id)?;
        if !record.is_object() {
            return Ok(());
        }
        if let Some(key) = domain_effect_key(record)
            && self
                .store(project_id)?
                .command_receipt(project_id, &key)?
                .is_some()
        {
            return Ok(());
        }
        if is_schema(record, "progress-update") {
            return self.domain_event(
                project_id,
                session,
                "task.progress_recorded",
                &format!("domain-progress:{}", string(record.get("progressId"))),
                record,
            );
        }
        if is_schema(record, "blocker") {
            let packet = match recovered_task {
                Some(task) => task.clone(),
                None => self.task_authority(project_id, session, &string(record.get("taskId")))?,
            };
            let key = format!("domain-blocker:{}", string(record.get("blockerId")));
            if let Some(mission_id) = truthy_id(&packet.mission_id) {
                let version = self.mission(project_id, mission_id)?["version"]
                    .as_i64()
                    .unwrap_or_default();
                self.transition_mission_node(
                    project_id,
                    mission_id,
                    version,
                    &packet.graph_node_id,
                    "blocked",
                    &key,
                    None,
                    false,
                )?;
            } else if let Some(direction_id) = truthy_id(&packet.direction_id) {
                let version = self.direction(project_id, direction_id)?["version"]
                    .as_i64()
                    .unwrap_or_default();
                self.transition_direction_node(
                    project_id,
                    direction_id,
                    version,
                    &packet.graph_node_id,
                    "blocked",
                    &key,
                    None,
                    false,
                )?;
            } else {
                self.domain_event(project_id, session, "task.blocked", &key, record)?;
            }
            return Ok(());
        }
        if is_schema(record, "graph-change-proposal") {
            let scope_id = string(record.get("scopeId"));
            let mission = is(record.get("scopeType"), "mission");
            let current = if mission {
                self.mission(project_id, &scope_id)?
            } else {
                self.direction(project_id, &scope_id)?
            };
            let operations = Value::Array(entities::graph_operations(
                record.get("operations"),
                current["value"]["nodes"]
                    .as_array()
                    .map_or(&[][..], Vec::as_slice),
            )?);
            let proposal_id = string(record.get("proposalId"));
            let key = format!("domain-graph:{proposal_id}");
            if record.get("approvalRequired") != Some(&Value::Bool(true)) {
                let base = js_number(record.get("baseGraphVersion")) as i64;
                let (rationale, evidence_ids) = (
                    string(record.get("rationale")),
                    string_values(record.get("evidenceIds"))?,
                );
                if mission {
                    let version = self.mission(project_id, &scope_id)?["version"]
                        .as_i64()
                        .unwrap_or_default();
                    self.mutate_mission_graph(
                        project_id,
                        &scope_id,
                        version,
                        base,
                        &operations,
                        &rationale,
                        &evidence_ids,
                        &key,
                        false,
                    )?;
                } else {
                    let version = self.direction(project_id, &scope_id)?["version"]
                        .as_i64()
                        .unwrap_or_default();
                    self.mutate_direction_graph(
                        project_id,
                        &scope_id,
                        version,
                        base,
                        &operations,
                        &rationale,
                        &evidence_ids,
                        &key,
                    )?;
                }
            } else {
                let scope = event_scope(project_id, session);
                let mut payload = json!({ "proposalId": proposal_id });
                if let Some(impact) = record.get("contractImpact") {
                    payload["contractImpact"] = impact.clone();
                }
                let draft = json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "graph.proposal_pending", "source": "noshd", "scope": scope, "correlationId": proposal_id, "causationId": null, "payload": payload });
                let pending = self.store(project_id)?.mutate_projection(&key, Some(0), &draft, &ProjectionInput { entity_type: "graph_proposal".into(), entity_id: proposal_id, state: "pending".into(), value: json!({ "record": record, "scope": scope, "submittedAt": now_utc() }), graph: None, records: Vec::new(), intent: None })?;
                if !pending.replayed {
                    self.host.publish(&pending.event);
                }
            }
            return Ok(());
        }
        if is_schema(record, "delegation-request") {
            return self.domain_event(
                project_id,
                session,
                "delegation.requested",
                &format!("domain-delegation:{}", string(record.get("requestId"))),
                record,
            );
        }
        if is_schema(record, "handoff") {
            let source = match recovered_task {
                Some(task) => task.clone(),
                None => self.task_authority(
                    project_id,
                    session,
                    session.task_id.as_deref().unwrap_or_default(),
                )?,
            };
            self.persist_pending_handoff(project_id, session, record, &source)?;
            return Ok(());
        }
        if is_schema(record, "handoff-teachback") {
            let handoff = self.pending_handoff(
                project_id,
                &string(record.get("handoffId")),
                &event_scope(project_id, session),
                &string(record.get("toAgentId")),
            )?;
            return self.apply_handoff_decision(project_id, &handoff, record);
        }
        Ok(())
    }

    fn task_authority(
        &self,
        project_id: &str,
        session: &DomainSession,
        task_id: &str,
    ) -> Result<TaskAuthority> {
        if truthy_id(&session.task_id) != Some(task_id) || task_id.is_empty() {
            return rejected("Domain record task does not match the active Pi session");
        }
        let packets: Vec<Value> = self
            .submitted_records(project_id, None)?
            .into_iter()
            .filter(|entry| is_schema(entry, "task-packet") && is(entry.get("taskId"), task_id))
            .collect();
        let [packet] = packets.as_slice() else {
            return rejected("Task authority requires exactly one matching durable Task Packet");
        };
        let (scope, lease, observed, budget, permissions) = (
            &packet["scope"],
            &packet["lease"],
            &packet["observedVersions"],
            &packet["budget"],
            &packet["permissions"],
        );
        if !is(packet.get("assignedAgentId"), &session.agent_id)
            || !is(packet.get("assignedRole"), &session.role)
            || !scope.is_object()
            || !is(scope.get("projectId"), project_id)
            || !same_task_scope(scope, session)
            || !lease.is_object()
            || !is_str(lease.get("leaseId"))
            || !is_str(lease.get("expiresAt"))
            || !observed.is_object()
            || !is_str(scope.get("graphNodeId"))
            || !budget.is_object()
            || number(budget.get("maximumModelTokens")).is_none()
            || number(budget.get("maximumWallClockSeconds")).is_none()
            || !permissions.is_object()
            || !(is(permissions.get("delegation"), "disabled")
                || is(permissions.get("delegation"), "request_only"))
        {
            return rejected("No authoritative Task Packet matches the active Pi session");
        }
        let Some(lease_version) = lease_projection_version(
            observed,
            truthy_id(&session.mission_id).is_some(),
            truthy_id(&session.direction_id).is_some(),
        ) else {
            return rejected("Task Packet lacks the exact lease projection version");
        };
        // An unparseable expiry is NaN, which compares false, as in JavaScript.
        if parse_ms(lease["expiresAt"].as_str().expect("checked")) <= entities::now_ms() {
            return rejected("Task lease has expired");
        }
        if let Some(mission_id) = truthy_id(&session.mission_id)
            && !same(
                observed.get("missionGraphVersion"),
                self.mission(project_id, mission_id)?["value"].get("graphVersion"),
            )
        {
            return rejected("Task packet observes a stale Mission graph version");
        }
        if let Some(direction_id) = truthy_id(&session.direction_id)
            && !same(
                observed.get("directionGraphVersion"),
                self.direction(project_id, direction_id)?["value"].get("graphVersion"),
            )
        {
            return rejected("Task packet observes a stale Direction graph version");
        }
        Ok(TaskAuthority {
            graph_node_id: string(scope.get("graphNodeId")),
            mission_id: session.mission_id.clone(),
            direction_id: session.direction_id.clone(),
            lease_id: string(lease.get("leaseId")),
            lease_projection_version: lease_version,
            agent_id: session.agent_id.clone(),
            maximum_model_tokens: number(budget.get("maximumModelTokens")).expect("checked"),
            maximum_wall_clock_seconds: number(budget.get("maximumWallClockSeconds"))
                .expect("checked"),
            delegation: string(permissions.get("delegation")),
        })
    }

    /// The nodes of the task's Mission or Direction graph (none for other tasks).
    fn authority_nodes(&self, project_id: &str, packet: &TaskAuthority) -> Result<Vec<Value>> {
        let value = if let Some(mission_id) = truthy_id(&packet.mission_id) {
            self.mission(project_id, mission_id)?
        } else if let Some(direction_id) = truthy_id(&packet.direction_id) {
            self.direction(project_id, direction_id)?
        } else {
            return Ok(Vec::new());
        };
        Ok(value["value"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    fn holds_lease(
        &self,
        project_id: &str,
        packet: &TaskAuthority,
        session: &DomainSession,
        states: &[&str],
    ) -> Result<bool> {
        let nodes = self.authority_nodes(project_id, packet)?;
        Ok(nodes
            .iter()
            .find(|node| is(node.get("id"), &packet.graph_node_id))
            .is_some_and(|node| {
                node["lease"].is_object()
                    && is(node["lease"].get("leaseId"), &packet.lease_id)
                    && is(node["lease"].get("ownerId"), &session.agent_id)
                    && node["state"]
                        .as_str()
                        .is_some_and(|state| states.contains(&state))
            }))
    }

    fn assert_live_lease(
        &self,
        project_id: &str,
        packet: &TaskAuthority,
        session: &DomainSession,
    ) -> Result<()> {
        if !self.holds_lease(project_id, packet, session, &["leased", "working"])? {
            return rejected("Blocker requires the caller's live graph lease");
        }
        Ok(())
    }

    fn assert_working_lease(
        &self,
        project_id: &str,
        packet: &TaskAuthority,
        session: &DomainSession,
    ) -> Result<()> {
        if !self.holds_lease(project_id, packet, session, &["working"])? {
            return rejected("Progress requires the caller's current working graph lease");
        }
        Ok(())
    }

    fn persist_pending_handoff(
        &self,
        project_id: &str,
        session: &DomainSession,
        record: &Value,
        source: &TaskAuthority,
    ) -> Result<Value> {
        let handoff_id = string(record.get("handoffId"));
        let store = self.store(project_id)?;
        if let Some(existing) = store.projection(project_id, "handoff", &handoff_id)? {
            assert_pending_handoff_record(&existing["value"], record)?;
            return pending_handoff_value(&existing["value"]);
        }
        let current = if let Some(mission_id) = truthy_id(&source.mission_id) {
            Some(self.mission(project_id, mission_id)?)
        } else if let Some(direction_id) = truthy_id(&source.direction_id) {
            Some(self.direction(project_id, direction_id)?)
        } else {
            None
        };
        let live = current.as_ref().is_some_and(|current| {
            let node = current["value"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|node| is(node.get("id"), &source.graph_node_id));
            number(current.get("version")) == Some(source.lease_projection_version)
                && node.is_some_and(|node| {
                    node["lease"].is_object()
                        && is(node["lease"].get("leaseId"), &source.lease_id)
                        && is(node["lease"].get("ownerId"), &source.agent_id)
                        && matches!(node["state"].as_str(), Some("leased" | "working"))
                })
        });
        let (Some(current), true, Some(task_id)) = (current, live, truthy_id(&session.task_id))
        else {
            return rejected(
                "Handoff source Task Packet is no longer the exact live lease authority",
            );
        };
        let mut source_value = to_json(source)?;
        source_value["taskId"] = json!(task_id);
        source_value["role"] = json!(session.role);
        source_value["projectionVersion"] = current["version"].clone();
        let accepted_hash = sha256(record);
        let scope = event_scope(project_id, session);
        let recipient = string(record.get("toAgentId"));
        let value = json!({ "acceptedHandoffHash": accepted_hash, "handoffId": handoff_id, "logicalOwnerId": string(record.get("logicalOwnerId")), "recipientAgentId": recipient, "record": record, "scope": scope, "source": source_value });
        let draft = json!({
            "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "handoff.pending", "source": "noshd", "scope": scope, "correlationId": handoff_id, "causationId": null,
            "payload": { "handoffId": handoff_id, "acceptedHandoffHash": accepted_hash, "sourceTaskId": task_id, "oldLeaseReleaseId": source.lease_id, "graphNodeId": source.graph_node_id, "recipientAgentId": recipient },
        });
        let stored = store.mutate_projection(&format!("domain-handoff:{handoff_id}"), Some(0), &draft, &ProjectionInput { entity_type: "handoff".into(), entity_id: handoff_id.clone(), state: "pending".into(), value, graph: None, records: Vec::new(), intent: Some(json!({ "command": "handoff.pending", "handoffId": handoff_id, "acceptedHandoffHash": accepted_hash })) })?;
        if !stored.replayed {
            self.host.publish(&stored.event);
        }
        pending_handoff_value(&stored.projection["value"])
    }

    fn pending_handoff(
        &self,
        project_id: &str,
        handoff_id: &str,
        scope: &Value,
        recipient_agent_id: &str,
    ) -> Result<Value> {
        let stored = self
            .store(project_id)?
            .projection(project_id, "handoff", handoff_id)?;
        let Some(stored) = stored.filter(|stored| stored["state"] == "pending") else {
            return rejected("No pending accepted Handoff exists");
        };
        let handoff = pending_handoff_value(&stored["value"])?;
        if !is(handoff.get("handoffId"), handoff_id)
            || !is(handoff.get("recipientAgentId"), recipient_agent_id)
            || !same_event_scope(&handoff["scope"], scope, false)
        {
            return rejected(
                "Handoff lookup is missing, duplicate, or outside the durable recipient scope",
            );
        }
        Ok(handoff)
    }

    fn apply_handoff_decision(
        &self,
        project_id: &str,
        handoff: &Value,
        record: &Value,
    ) -> Result<()> {
        let decision = string(record.get("decision"));
        let checked = validate_teachback(
            &handoff_state(&handoff["record"])?,
            &teachback(record, decision.clone())?,
        );
        if checked["ok"] != true {
            return rejected(format!(
                "Teach-back does not exactly match the Handoff: {}",
                conflicts(&checked)
            ));
        }
        let handoff_id = string(handoff.get("handoffId"));
        let store = self.store(project_id)?;
        if let Some(terminal) = store.projection(project_id, "handoff_decision", &handoff_id)? {
            return assert_handoff_decision_record(&terminal["value"], record);
        }
        if decision == "accepted" {
            self.assert_handoff_ownership_transfer(project_id, handoff)?;
            self.release_handoff_source_lease(project_id, handoff)?;
            self.transfer_handoff_ownership(project_id, handoff)?;
        }
        let decision_hash = sha256(record);
        let draft = json!({
            "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": if decision == "accepted" { "handoff.accepted" } else { "handoff.decision_recorded" }, "source": "noshd", "scope": handoff["scope"], "correlationId": handoff_id, "causationId": null,
            "payload": { "handoffId": handoff_id, "logicalOwnerId": handoff["logicalOwnerId"], "recipientAgentId": handoff["recipientAgentId"], "decision": decision, "decisionHash": decision_hash },
        });
        let value = json!({ "handoffId": handoff_id, "decisionHash": decision_hash, "decision": decision, "recipientAgentId": handoff["recipientAgentId"], "decidedAt": now_utc() });
        let stored = store.mutate_projection(&format!("domain-handoff-decision:{handoff_id}"), Some(0), &draft, &ProjectionInput { entity_type: "handoff_decision".into(), entity_id: handoff_id.clone(), state: decision, value, graph: None, records: Vec::new(), intent: Some(json!({ "command": "handoff.decision", "handoffId": handoff_id, "decisionHash": decision_hash })) })?;
        if !stored.replayed {
            self.host.publish(&stored.event);
        }
        Ok(())
    }

    fn assert_handoff_ownership_transfer(&self, project_id: &str, handoff: &Value) -> Result<()> {
        let ownership = self.store(project_id)?.projection(
            project_id,
            "logical_ownership",
            &string(handoff.get("logicalOwnerId")),
        )?;
        if let Some(ownership) = ownership {
            let value = &ownership["value"];
            if !value.is_object()
                || !same(value.get("handoffId"), handoff.get("handoffId"))
                || !same(value.get("agentId"), handoff.get("recipientAgentId"))
            {
                return rejected(
                    "Logical ownership was already transferred by a different Handoff",
                );
            }
        }
        Ok(())
    }

    fn release_handoff_source_lease(&self, project_id: &str, handoff: &Value) -> Result<()> {
        let source = &handoff["source"];
        let handoff_id = string(handoff.get("handoffId"));
        let graph_node_id = string(source.get("graphNodeId"));
        let (scope_name, current) = if let Some(mission_id) = source
            .get("missionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            (
                "Mission",
                (mission_id.to_owned(), self.mission(project_id, mission_id)?),
            )
        } else if let Some(direction_id) = source
            .get("directionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            (
                "Direction",
                (
                    direction_id.to_owned(),
                    self.direction(project_id, direction_id)?,
                ),
            )
        } else {
            return rejected("Handoff source has no graph authority");
        };
        let (entity_id, current) = current;
        let node = current["value"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|node| is(node.get("id"), &graph_node_id))
            .cloned();
        let version = number(current.get("version")).unwrap_or(f64::NAN);
        let projection_version = number(source.get("projectionVersion")).unwrap_or(f64::NAN);
        if version == projection_version {
            let held = node.as_ref().is_some_and(|node| {
                node["lease"].is_object()
                    && same(node["lease"].get("leaseId"), source.get("leaseId"))
                    && same(node["lease"].get("ownerId"), source.get("agentId"))
                    && matches!(node["state"].as_str(), Some("leased" | "working"))
            });
            if !held {
                return rejected(format!(
                    "Handoff source {scope_name} lease changed before release"
                ));
            }
            let key = format!("handoff-release:{handoff_id}");
            if scope_name == "Mission" {
                self.transition_mission_node(
                    project_id,
                    &entity_id,
                    version as i64,
                    &graph_node_id,
                    "ready",
                    &key,
                    None,
                    false,
                )?;
            } else {
                self.transition_direction_node(
                    project_id,
                    &entity_id,
                    version as i64,
                    &graph_node_id,
                    "ready",
                    &key,
                    None,
                    false,
                )?;
            }
        } else if !(version == projection_version + 1.0
            && node.as_ref().is_some_and(|node| {
                node["state"] == "ready" && node.get("lease") == Some(&Value::Null)
            }))
        {
            return rejected(format!(
                "Handoff source {scope_name} projection changed before release"
            ));
        }
        Ok(())
    }

    fn transfer_handoff_ownership(&self, project_id: &str, handoff: &Value) -> Result<()> {
        let store = self.store(project_id)?;
        let logical_owner_id = string(handoff.get("logicalOwnerId"));
        if store
            .projection(project_id, "logical_ownership", &logical_owner_id)?
            .is_some()
        {
            return self.assert_handoff_ownership_transfer(project_id, handoff);
        }
        let handoff_id = string(handoff.get("handoffId"));
        let draft = json!({
            "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "handoff.ownership_transferred", "source": "noshd", "scope": handoff["scope"], "correlationId": handoff_id, "causationId": null,
            "payload": { "handoffId": handoff_id, "logicalOwnerId": logical_owner_id, "recipientAgentId": handoff["recipientAgentId"] },
        });
        let value = json!({ "handoffId": handoff_id, "logicalOwnerId": logical_owner_id, "agentId": handoff["recipientAgentId"], "acceptedHandoffHash": handoff["acceptedHandoffHash"] });
        let intent = json!({ "command": "handoff.ownership_transfer", "handoffId": handoff_id, "acceptedHandoffHash": handoff["acceptedHandoffHash"] });
        let stored = store.mutate_projection(
            &format!("handoff-ownership:{handoff_id}"),
            Some(0),
            &draft,
            &ProjectionInput {
                entity_type: "logical_ownership".into(),
                entity_id: logical_owner_id,
                state: "accepted".into(),
                value,
                graph: None,
                records: Vec::new(),
                intent: Some(intent),
            },
        )?;
        if !stored.replayed {
            self.host.publish(&stored.event);
        }
        Ok(())
    }

    fn domain_event(
        &self,
        project_id: &str,
        session: &DomainSession,
        event_type: &str,
        idempotency_key: &str,
        payload: &Value,
    ) -> Result<()> {
        self.host.assert_writable(project_id)?;
        let draft = json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": event_type, "source": "noshd", "scope": event_scope(project_id, session), "correlationId": session.task_id, "causationId": null, "payload": payload });
        let stored = self
            .store(project_id)?
            .append_idempotent(idempotency_key, &draft)?;
        if !stored.replayed {
            self.host.publish(&stored.receipt["event"]);
        }
        Ok(())
    }
}

/// The lease projection version a Task Packet observed: `leaseVersion` in a Mission,
/// `directionProjectionVersion` in a Direction; a positive integer.
fn lease_projection_version(observed: &Value, mission: bool, direction: bool) -> Option<f64> {
    let version = if mission {
        observed.get("leaseVersion")
    } else if direction {
        observed.get("directionProjectionVersion")
    } else {
        None
    };
    number(version).filter(|n| is_integer(version) && *n >= 1.0)
}

pub(super) fn domain_effect_key(record: &Value) -> Option<String> {
    let field = |schema: &str, key: &str, prefix: &str| {
        (is_schema(record, schema) && is_str(record.get(key)))
            .then(|| format!("{prefix}:{}", string(record.get(key))))
    };
    field("progress-update", "progressId", "domain-progress")
        .or_else(|| field("blocker", "blockerId", "domain-blocker"))
        .or_else(|| field("graph-change-proposal", "proposalId", "domain-graph"))
        .or_else(|| field("delegation-request", "requestId", "domain-delegation"))
        .or_else(|| field("handoff", "handoffId", "domain-handoff"))
        .or_else(|| field("handoff-teachback", "handoffId", "domain-handoff-decision"))
}

fn event_scope(project_id: &str, session: &DomainSession) -> Value {
    json!({ "projectId": project_id, "missionId": session.mission_id, "directionId": session.direction_id, "autoresearchId": session.autoresearch_id, "experimentId": session.experiment_id, "runId": session.run_id, "jobId": session.job_id, "agentId": session.agent_id })
}

fn same_scope(value: &Value, session: &DomainSession, require_agent: bool) -> bool {
    same_id(value.get("missionId"), &session.mission_id)
        && same_id(value.get("directionId"), &session.direction_id)
        && same_id(value.get("autoresearchId"), &session.autoresearch_id)
        && same_id(value.get("experimentId"), &session.experiment_id)
        && same_id(value.get("runId"), &session.run_id)
        && same_id(value.get("jobId"), &session.job_id)
        && (!require_agent || is(value.get("agentId"), &session.agent_id))
}

fn same_task_scope(value: &Value, session: &DomainSession) -> bool {
    let or_null = |key: &str| Some(value.get(key).cloned().unwrap_or(Value::Null));
    same_id(value.get("missionId"), &session.mission_id)
        && same_id(value.get("directionId"), &session.direction_id)
        && same_id(value.get("autoresearchId"), &session.autoresearch_id)
        && same_id(value.get("experimentId"), &session.experiment_id)
        && same_id(or_null("runId").as_ref(), &session.run_id)
        && same_id(or_null("jobId").as_ref(), &session.job_id)
}

fn same_event_scope(left: &Value, right: &Value, require_agent: bool) -> bool {
    [
        "projectId",
        "missionId",
        "directionId",
        "autoresearchId",
        "experimentId",
        "runId",
        "jobId",
    ]
    .iter()
    .all(|field| same(left.get(*field), right.get(*field)))
        && (!require_agent || same(left.get("agentId"), right.get("agentId")))
}

fn object_versions(value: Option<&Value>) -> Result<Value> {
    match value.and_then(Value::as_object) {
        Some(versions)
            if versions.values().all(|entry| {
                entry.is_string()
                    || (is_integer(Some(entry)) && number(Some(entry)).is_some_and(|n| n >= 0.0))
            }) =>
        {
            Ok(value.cloned().expect("checked"))
        }
        _ => rejected("Expected observed versions"),
    }
}

fn handoff_goal_stack(value: Option<&Value>) -> Result<Value> {
    let valid = value.is_some_and(|stack| {
        stack.is_object()
            && is_str(stack.get("projectGoalId"))
            && stack["missionCriterionIds"]
                .as_array()
                .is_some_and(|ids| ids.iter().all(Value::is_string))
            && (stack.get("directionQuestionId") == Some(&Value::Null)
                || is_str(stack.get("directionQuestionId")))
            && (stack.get("currentGraphNodeId") == Some(&Value::Null)
                || is_str(stack.get("currentGraphNodeId")))
    });
    let Some(stack) = value.filter(|_| valid) else {
        return rejected("Handoff goal stack is invalid");
    };
    Ok(
        json!({ "projectGoalId": stack["projectGoalId"], "missionCriterionIds": stack["missionCriterionIds"], "directionQuestionId": stack["directionQuestionId"], "currentGraphNodeId": stack["currentGraphNodeId"] }),
    )
}

/// `handoffState`: what the recipient's teach-back must reproduce.
fn handoff_state(handoff: &Value) -> Result<Value> {
    let defects: Vec<Value> = handoff["openDefects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|defect| {
            if defect.is_string() {
                Some(defect.clone())
            } else {
                defect.get("defectId").filter(|id| id.is_string()).cloned()
            }
        })
        .collect();
    let Some(head) = handoff["branch"]
        .get("head")
        .and_then(Value::as_str)
        .filter(|_| handoff["branch"].is_object())
    else {
        return rejected("Handoff branch state is invalid");
    };
    Ok(json!({
        "handoffId": string(handoff.get("handoffId")), "logicalOwnerId": string(handoff.get("logicalOwnerId")), "goalStack": handoff_goal_stack(handoff.get("goalStack"))?, "observedVersions": object_versions(handoff.get("observedVersions"))?,
        "branchHead": head, "defectIds": defects, "blockerIds": string_values(handoff.get("openBlockerIds"))?, "readyNodeIds": string_values(handoff.get("readyNodeIds"))?,
    }))
}

/// The teach-back as `validateTeachback` reads it.
fn teachback(record: &Value, decision: String) -> Result<Value> {
    Ok(json!({
        "handoffId": string(record.get("handoffId")), "logicalOwnerId": string(record.get("logicalOwnerId")), "understoodGoalStack": handoff_goal_stack(record.get("understoodGoalStack"))?, "decision": decision,
        "observedVersions": object_versions(record.get("observedVersions"))?, "observedBranchHead": string(record.get("observedBranchHead")),
        "acknowledgedDefectIds": string_values(record.get("acknowledgedDefectIds"))?, "acknowledgedBlockerIds": string_values(record.get("acknowledgedBlockerIds"))?,
        "selectedNextNodeId": record.get("selectedNextNodeId").filter(|id| id.is_string()).cloned().unwrap_or(Value::Null), "conflicts": string_values(record.get("conflicts"))?,
    }))
}

fn conflicts(checked: &Value) -> String {
    checked["conflicts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|conflict| string(Some(conflict)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pending_handoff_value(value: &Value) -> Result<Value> {
    if !value.is_object()
        || !is_str(value.get("acceptedHandoffHash"))
        || !is_str(value.get("handoffId"))
        || !is_str(value.get("logicalOwnerId"))
        || !is_str(value.get("recipientAgentId"))
        || !value["record"].is_object()
        || !value["scope"].is_object()
        || !value["source"].is_object()
    {
        return rejected("Pending Handoff projection is malformed");
    }
    let (scope, source) = (&value["scope"], &value["source"]);
    let nullable = |item: Option<&Value>| item == Some(&Value::Null) || is_str(item);
    let valid = is_str(scope.get("projectId"))
        && [
            "missionId",
            "directionId",
            "autoresearchId",
            "experimentId",
            "runId",
            "jobId",
            "agentId",
        ]
        .iter()
        .all(|field| nullable(scope.get(*field)))
        && is_str(source.get("taskId"))
        && source["role"]
            .as_str()
            .is_some_and(|role| ROLES.contains(&role))
        && nullable(source.get("missionId"))
        && nullable(source.get("directionId"))
        && is_str(source.get("graphNodeId"))
        && is_str(source.get("leaseId"))
        && is_str(source.get("agentId"))
        && is_integer(source.get("leaseProjectionVersion"))
        && is_integer(source.get("projectionVersion"))
        && number(source.get("maximumModelTokens")).is_some()
        && number(source.get("maximumWallClockSeconds")).is_some()
        && (is(source.get("delegation"), "disabled")
            || is(source.get("delegation"), "request_only"));
    if !valid {
        return rejected("Pending Handoff authority is malformed");
    }
    Ok(
        json!({ "acceptedHandoffHash": value["acceptedHandoffHash"], "handoffId": value["handoffId"], "logicalOwnerId": value["logicalOwnerId"], "recipientAgentId": value["recipientAgentId"], "record": value["record"], "scope": scope, "source": source }),
    )
}

fn assert_pending_handoff_record(value: &Value, record: &Value) -> Result<()> {
    if !is(
        pending_handoff_value(value)?.get("acceptedHandoffHash"),
        &sha256(record),
    ) {
        return rejected("Handoff ID was reused for a different authoritative Handoff");
    }
    Ok(())
}

fn assert_handoff_decision_record(value: &Value, record: &Value) -> Result<()> {
    if !value.is_object() || !is(value.get("decisionHash"), &sha256(record)) {
        return rejected("Handoff already has a different terminal decision");
    }
    Ok(())
}
