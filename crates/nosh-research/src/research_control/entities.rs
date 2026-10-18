//! Missions, Directions, Autoresearch, and graph proposals: creation, `start`, transitions, graph
//! mutations, and canonical contracts.

use super::*;
use crate::episodes::js_trim;
use crate::graph::{
    GraphNode, Lease, VersionedDag, is_legal_direction_transition, is_legal_mission_transition,
    parse_ms,
};
use nosh_wire::{IdPrefix, create_id, now_utc, random_hex, sha256};
use serde::Deserialize;

/// Mission nodes whose work is a spawned research child rather than one task.
pub const SPAWN_NODE_TYPES: &[&str] = &["research_direction", "autoresearch"];

const MISSION_STATES: &[&str] = &[
    "created",
    "draft",
    "planning",
    "awaiting_approval",
    "running",
    "pausing",
    "paused",
    "reviewing",
    "blocked",
    "stopping",
    "completed",
    "stopped",
    "failed",
];
const ACTIVE_MISSION_STATES: &[&str] = &[
    "running",
    "pausing",
    "paused",
    "reviewing",
    "blocked",
    "stopping",
];

fn autoresearch_transitions(state: &str) -> &'static [&'static str] {
    match state {
        "draft" => &["running", "stopped"],
        "running" => &["paused", "blocked", "completed", "stopped", "failed"],
        "paused" => &["running", "stopped"],
        "blocked" => &["running", "stopped", "failed"],
        _ => &[],
    }
}

/// A JSON field present even when `null` (`Some(Null)`), unlike a plain `Option<Value>`.
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionInput {
    pub title: String,
    pub objective: String,
    pub deliverables: Vec<String>,
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub non_objectives: Option<Vec<String>>,
    #[serde(default)]
    pub starting_evidence: Option<Vec<String>>,
    #[serde(default)]
    pub research_plan: Option<Value>,
    #[serde(default)]
    pub budgets: Option<Map<String, Value>>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectionInput {
    pub question: String,
    pub decision_use: String,
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub evaluation_contract: Option<Value>,
    pub idempotency_key: String,
}

/// A Direction a Mission spawned: its origin, base commit, and budget share.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnedDirection {
    pub origin: Value,
    pub base_commit: String,
    pub budget: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoresearchInput {
    pub decision_question: String,
    #[serde(default)]
    pub direction_id: Option<String>,
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub family_tags: Option<Vec<String>>,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    pub evaluation_contract: Option<Value>,
    #[serde(default)]
    pub maximum_experiments: Option<f64>,
    #[serde(default)]
    pub maximum_rounds: Option<f64>,
    #[serde(default)]
    pub maximum_wall_clock_seconds: Option<f64>,
    #[serde(default)]
    pub maximum_model_tokens: Option<f64>,
    #[serde(default)]
    pub maximum_gpu_seconds: Option<f64>,
    #[serde(default)]
    pub maximum_disk_bytes: Option<f64>,
    pub idempotency_key: String,
}

/// Autoresearch a director spawned.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnedAutoresearch {
    pub invoked_by_role: String,
    pub origin: Value,
    pub base_commit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineInput {
    pub commit: String,
    pub review_id: String,
    pub evaluation_contract_hash: String,
    pub idempotency_key: String,
}

/// The latest Direction or Autoresearch spawned for a scope's node.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SpawnedChild {
    pub kind: &'static str,
    #[serde(rename = "entityId")]
    pub entity_id: String,
    pub state: String,
    pub version: i64,
    pub attempt: f64,
}

fn trimmed(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|item| js_trim(item).to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

fn nodes_of(value: &Value) -> Result<Vec<GraphNode>> {
    Ok(serde_json::from_value(
        value.get("nodes").cloned().unwrap_or(Value::Null),
    )?)
}

fn version_of(stored: &Value) -> i64 {
    stored["version"].as_i64().unwrap_or_default()
}

fn graph_input(
    scope_type: &str,
    version: &crate::graph::GraphVersion,
    rationale: &str,
) -> Result<GraphInput> {
    Ok(GraphInput {
        scope_type: scope_type.into(),
        version: version.version,
        value: to_json(version)?,
        rationale: rationale.into(),
    })
}

impl<H: Host> ResearchControl<H> {
    pub fn missions(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self.store(project_id)?.projections(project_id, "mission")?)
    }

    pub fn mission(&self, project_id: &str, mission_id: &str) -> Result<Value> {
        self.store(project_id)?
            .projection(project_id, "mission", mission_id)?
            .map_or_else(|| rejected("Unknown Mission"), Ok)
    }

    pub fn graph_proposals(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .store(project_id)?
            .projections(project_id, "graph_proposal")?)
    }

    pub fn directions(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .store(project_id)?
            .projections(project_id, "direction")?)
    }

    pub fn direction(&self, project_id: &str, direction_id: &str) -> Result<Value> {
        self.store(project_id)?
            .projection(project_id, "direction", direction_id)?
            .map_or_else(|| rejected("Unknown Direction"), Ok)
    }

    pub fn autoresearch(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .store(project_id)?
            .projections(project_id, "autoresearch")?)
    }

    pub fn autoresearch_execution(&self, project_id: &str, autoresearch_id: &str) -> Result<Value> {
        self.store(project_id)?
            .projection(project_id, "autoresearch", autoresearch_id)?
            .map_or_else(|| rejected("Unknown Autoresearch execution"), Ok)
    }

    pub fn create_mission(&self, project_id: &str, input: &MissionInput) -> Result<Value> {
        let research_plan = valid_research_plan(input.research_plan.as_ref())?;
        let budgets = mission_budgets(&research_plan, input.budgets.as_ref())?;
        let deliverables = trimmed(&input.deliverables);
        let success_criteria = trimmed(&input.success_criteria);
        let starting_evidence = trimmed(input.starting_evidence.as_deref().unwrap_or_default());
        let non_objectives = trimmed(input.non_objectives.as_deref().unwrap_or_default());
        let (title, objective) = (js_trim(&input.title), js_trim(&input.objective));
        if let Some(replay) = created_projection_replay(
            self.store(project_id)?,
            project_id,
            &input.idempotency_key,
            "mission",
            "mission.created",
        )? {
            let value = &replay["value"];
            if !is(value.get("title"), title)
                || !is(value.get("objective"), objective)
                || !same_string_set(value.get("deliverables"), &deliverables)
                || !same_string_set(value.get("successCriteria"), &success_criteria)
                || !same_string_set(value.get("nonObjectives"), &non_objectives)
                || !same_string_set(value.get("startingEvidence"), &starting_evidence)
                || canonical_json(&or_null(value.get("researchPlan")))
                    != canonical_json(&research_plan)
                || canonical_json(&value["budgets"])
                    != canonical_json(&Value::Object(budgets.clone()))
            {
                return rejected(
                    "Mission creation idempotency key was reused for a different request",
                );
            }
            return Ok(replay);
        }
        if title.is_empty()
            || objective.is_empty()
            || deliverables.is_empty()
            || success_criteria.is_empty()
        {
            return rejected(
                "Mission title, objective, deliverables, and success criteria are required",
            );
        }
        assert_existing_evidence_ids(
            &self
                .submitted_records(project_id, None)?
                .into_iter()
                .filter(|record| is_schema(record, "evidence"))
                .collect::<Vec<_>>(),
            &starting_evidence,
        )?;
        let mission_id = create_id(IdPrefix::Mis);
        let now = now_utc();
        let criterion_ids: Vec<String> = (1..=success_criteria.len())
            .map(|index| format!("criterion_{index}"))
            .collect();
        let nodes = default_mission_nodes(&now, &criterion_ids, &research_plan);
        let graph = VersionedDag::new(&mission_id, nodes, "initial plan", 1)?.current();
        let value = json!({
            "missionId": mission_id, "projectId": project_id, "title": title, "objective": objective,
            "nonObjectives": non_objectives, "deliverables": deliverables, "successCriteria": success_criteria, "startingEvidence": starting_evidence,
            "budgets": budgets, "researchPlan": research_plan,
            "approvalBoundaries": ["Protected-branch integration and budget increases require user approval"],
            "externalActionRestrictions": ["No publication, push, purchase, or third-party message without explicit user approval"],
            "pausePolicy": "safe",
            "finalReviewRubric": ["Every required node is accepted", "Claims resolve to reviewed evidence or explicit limitations", "Artifacts and evaluated commits resolve to stored hashes", "No unresolved blocking defect remains"],
            "approvedGraphVersion": null, "approvedAt": null, "canonicalContract": null,
            "state": "draft", "graphVersion": graph.version, "nodes": to_json(&graph.nodes)?, "createdAt": now, "updatedAt": now,
        });
        self.mutate(
            project_id,
            "mission",
            &mission_id,
            0,
            "mission.created",
            &input.idempotency_key,
            value,
            Some(graph_input("mission", &graph, "initial plan")?),
            None,
            false,
            None,
        )
    }

    /// User controls (steer, pause, resume, stop) act on the Mission state the user inspected; node
    /// churn after that version does not invalidate them.
    pub fn assert_inspected_mission_state(
        &self,
        project_id: &str,
        mission_id: &str,
        expected_version: i64,
    ) -> Result<Value> {
        let mission = self.mission(project_id, mission_id)?;
        let version = version_of(&mission);
        if expected_version == version {
            return Ok(mission);
        }
        let since = match mission["value"]
            .get("stateVersion")
            .filter(|value| !value.is_null())
        {
            Some(state_version) => js_number(Some(state_version)),
            // Missions written before stateVersion existed: each version is exactly one projection
            // event, so a transition's version is its ordinal.
            None => {
                let flags: Vec<bool> = self
                    .store(project_id)?
                    .replay(project_id, 0)?
                    .iter()
                    .filter(|event| {
                        event["type"]
                            .as_str()
                            .is_some_and(|kind| kind.starts_with("mission."))
                            && is(event["payload"].get("entityId"), mission_id)
                    })
                    .map(|event| {
                        MISSION_STATES.contains(
                            &&event["type"].as_str().unwrap_or_default()["mission.".len()..],
                        )
                    })
                    .collect();
                flags
                    .iter()
                    .rposition(|flag| *flag)
                    .map_or(0.0, |index| index as f64 + 1.0)
            }
        };
        if since < 1.0 || (expected_version as f64) < since || expected_version > version {
            return rejected(
                "Mission version conflict: its state changed after the inspected version",
            );
        }
        Ok(mission)
    }

    fn assert_single_active_mission(&self, project_id: &str, mission_id: &str) -> Result<()> {
        if self.missions(project_id)?.iter().any(|mission| {
            !is(mission.get("entityId"), mission_id)
                && mission["state"]
                    .as_str()
                    .is_some_and(|state| ACTIVE_MISSION_STATES.contains(&state))
        }) {
            return rejected("Version 1 allows one active Mission per Project");
        }
        Ok(())
    }

    /// One explicit user approval that walks a newly created entity to its working state (a
    /// Mission: draft → planning → awaiting_approval → running; a Direction: draft → proposed →
    /// active; Autoresearch: draft → running). Each step keeps its own idempotency key, so a retried
    /// start resumes where it stopped.
    pub fn start(
        &self,
        project_id: &str,
        family: &str,
        entity_id: &str,
        expected_version: i64,
        idempotency_key: &str,
    ) -> Result<Value> {
        let chain: &[&str] = match family {
            "missions" => &["draft", "planning", "awaiting_approval", "running"],
            "directions" => &["draft", "proposed", "active"],
            "autoresearch" => &["draft", "running"],
            _ => return rejected(format!("Unknown family {family}")),
        };
        let mut current = match family {
            "missions" => self.mission(project_id, entity_id)?,
            "directions" => self.direction(project_id, entity_id)?,
            _ => self.autoresearch_execution(project_id, entity_id)?,
        };
        if version_of(&current) != expected_version {
            return rejected(format!(
                "Stale {family} version {expected_version}; current is {}",
                version_of(&current)
            ));
        }
        let state = string(current.get("state"));
        let Some(from) = chain.iter().position(|step| *step == state) else {
            return rejected(format!("Cannot start from state {state}"));
        };
        // Check the last step's precondition first, so a refused start leaves the entity where it was.
        if family == "missions" {
            self.assert_single_active_mission(project_id, entity_id)?;
        }
        for next in &chain[from + 1..] {
            let key = format!("{idempotency_key}:{next}");
            let version = version_of(&current);
            current = match family {
                "missions" => {
                    self.transition_mission(project_id, entity_id, version, next, &key)?
                }
                "directions" => {
                    self.transition_direction(project_id, entity_id, version, next, &key)?
                }
                _ => self.transition_autoresearch(project_id, entity_id, version, next, &key)?,
            };
        }
        Ok(current)
    }

    pub fn transition_mission(
        &self,
        project_id: &str,
        mission_id: &str,
        expected_version: i64,
        next: &str,
        idempotency_key: &str,
    ) -> Result<Value> {
        let intent = json!({ "command": "mission.transition", "missionId": mission_id, "expectedVersion": expected_version, "next": next });
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            "mission",
            mission_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.mission(project_id, mission_id)?;
        let value = &current["value"];
        let state = string(value.get("state"));
        if !is_legal_mission_transition(&state, next) {
            return rejected(format!("Illegal Mission transition {state} -> {next}"));
        }
        if next == "running" {
            self.assert_single_active_mission(project_id, mission_id)?;
        }
        if next == "completed" {
            let packet = completion::current_mission_completion_packet(
                &self.records(project_id, None)?,
                mission_id,
                version_of(&current),
                &value["graphVersion"],
            );
            let basis = self.mission_completion_basis(project_id, mission_id)?;
            let complete = match &packet {
                None => false,
                Some(packet) => {
                    let pre_review = packet
                        .get("deterministicPreReview")
                        .filter(|value| value.is_object())
                        .and_then(|review| review.get("validatorRunIds"));
                    self.mission_completion_issues(project_id, mission_id)?
                        .is_empty()
                        && basis.issues.is_empty()
                        && completion::same_mission_criterion_mappings(
                            packet.get("criteria"),
                            &basis.criteria,
                        )
                        && same_string_set(
                            packet.get("deliverableArtifactIds"),
                            &basis.artifact_ids,
                        )
                        && same_string_set(pre_review, &basis.validator_run_ids)
                        && same_string_set(packet.get("openDefectIds"), &basis.open_defect_ids)
                }
            };
            if !complete {
                return rejected(
                    "Mission completion requires an exact current completion packet, each contract criterion's task, validator, independent Review, artifact, and defect basis",
                );
            }
        }
        let now = now_utc();
        let approving = state == "awaiting_approval" && next == "running";
        let mut updated = value.clone();
        if approving {
            updated["approvedGraphVersion"] = value["graphVersion"].clone();
            updated["approvedAt"] = json!(now);
        }
        updated["state"] = json!(next);
        updated["stateVersion"] = json!(expected_version + 1);
        updated["updatedAt"] = json!(now);
        self.mutate(
            project_id,
            "mission",
            mission_id,
            expected_version,
            &format!("mission.{next}"),
            idempotency_key,
            updated,
            None,
            None,
            approving,
            Some(intent),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mutate_mission_graph(
        &self,
        project_id: &str,
        mission_id: &str,
        expected_version: i64,
        base_graph_version: i64,
        operations: &Value,
        rationale: &str,
        evidence_ids: &[String],
        idempotency_key: &str,
        approved: bool,
    ) -> Result<Value> {
        let mut sorted = evidence_ids.to_vec();
        js_sort(&mut sorted);
        let intent = json!({ "command": "mission.graph_mutation", "missionId": mission_id, "expectedVersion": expected_version, "baseGraphVersion": base_graph_version, "operations": operations, "rationale": rationale, "evidenceIds": sorted, "approved": approved });
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            "mission",
            mission_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.mission(project_id, mission_id)?;
        let value = &current["value"];
        if !approved
            && !matches!(
                value["state"].as_str(),
                Some("draft" | "planning" | "awaiting_approval")
            )
        {
            return rejected(
                "Direct Mission graph mutation is allowed only before Mission approval",
            );
        }
        if approved && !nosh_wire::js::truthy(value.get("canonicalContract")) {
            return rejected(
                "Mission graph approval requires an existing canonical Mission contract",
            );
        }
        let graph_version = value["graphVersion"].as_i64().unwrap_or_default();
        if graph_version != base_graph_version {
            return rejected(format!(
                "Stale graph version {base_graph_version}; current version is {graph_version}"
            ));
        }
        assert_existing_evidence_ids(
            &self
                .submitted_records(project_id, None)?
                .into_iter()
                .filter(|record| is_schema(record, "evidence"))
                .collect::<Vec<_>>(),
            &string_values(value.get("startingEvidence"))?,
        )?;
        let mut graph = VersionedDag::new(mission_id, nodes_of(value)?, "hydrated", graph_version)?;
        let version = graph.apply(
            base_graph_version,
            &graph_operations(
                Some(operations),
                value["nodes"].as_array().map_or(&[][..], Vec::as_slice),
            )?,
            rationale,
            evidence_ids,
        )?;
        assert_mission_criterion_ownership(
            value["successCriteria"].as_array().map_or(0, Vec::len),
            &version.nodes,
        )?;
        let mut updated = value.clone();
        updated["graphVersion"] = json!(version.version);
        updated["nodes"] = to_json(&version.nodes)?;
        if approved {
            updated["approvedGraphVersion"] = json!(version.version);
        }
        updated["updatedAt"] = json!(now_utc());
        self.mutate(
            project_id,
            "mission",
            mission_id,
            expected_version,
            "mission.graph_changed",
            idempotency_key,
            updated,
            Some(graph_input("mission", &version, rationale)?),
            None,
            false,
            Some(intent),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transition_mission_node(
        &self,
        project_id: &str,
        mission_id: &str,
        expected_version: i64,
        node_id: &str,
        next: &str,
        idempotency_key: &str,
        lease: Option<&Value>,
        user_retry: bool,
    ) -> Result<Value> {
        self.transition_node(
            "mission",
            project_id,
            mission_id,
            expected_version,
            node_id,
            next,
            idempotency_key,
            lease,
            user_retry,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transition_direction_node(
        &self,
        project_id: &str,
        direction_id: &str,
        expected_version: i64,
        node_id: &str,
        next: &str,
        idempotency_key: &str,
        lease: Option<&Value>,
        user_retry: bool,
    ) -> Result<Value> {
        self.transition_node(
            "direction",
            project_id,
            direction_id,
            expected_version,
            node_id,
            next,
            idempotency_key,
            lease,
            user_retry,
        )
    }

    /// `transitionMissionNode` and `transitionDirectionNode`, which differ only in their scope's
    /// active states, messages, and the order of the event's detail fields.
    #[allow(clippy::too_many_arguments)]
    fn transition_node(
        &self,
        entity_type: &str,
        project_id: &str,
        entity_id: &str,
        expected_version: i64,
        node_id: &str,
        next: &str,
        idempotency_key: &str,
        lease: Option<&Value>,
        user_retry: bool,
    ) -> Result<Value> {
        let mission = entity_type == "mission";
        let id_key = if mission { "missionId" } else { "directionId" };
        let mut intent = json!({ "command": format!("{entity_type}.node_transition"), id_key: entity_id, "expectedVersion": expected_version, "nodeId": node_id, "next": next, "lease": or_null(lease) });
        if user_retry {
            intent["userRetry"] = json!(true);
        }
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            entity_type,
            entity_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = if mission {
            self.mission(project_id, entity_id)?
        } else {
            self.direction(project_id, entity_id)?
        };
        let value = &current["value"];
        let state = string(current.get("state"));
        let nodes = nodes_of(value)?;
        let active: &[&str] = if mission {
            &["running", "pausing", "reviewing"]
        } else {
            &["active", "reviewing"]
        };
        if !active.contains(&state.as_str()) && !repair_retry(&state, &nodes, node_id, next) {
            return rejected(if mission {
                "Mission nodes can change only while the Mission is active"
            } else {
                "Direction nodes can change only while the Direction is active"
            });
        }
        assert_lease_transition(next, lease, expected_version)?;
        let (nodes, granted) = grant_user_attempt(nodes, node_id, next, user_retry);
        let mut graph = VersionedDag::new(
            entity_id,
            nodes,
            "hydrated",
            value["graphVersion"].as_i64().unwrap_or_default(),
        )?;
        let node = match lease {
            Some(lease) => graph.lease(node_id, serde_json::from_value::<Lease>(lease.clone())?)?,
            None => graph.transition(node_id, next)?,
        };
        let mut updated = value.clone();
        updated["nodes"] = to_json(&graph.current().nodes)?;
        updated["updatedAt"] = json!(now_utc());
        let mut details = Map::new();
        details.insert("nodeId".into(), json!(node_id));
        details.insert("nodeState".into(), json!(node.state));
        details.insert("attempt".into(), to_json(&node.attempt)?);
        details.insert(
            "leaseId".into(),
            node.lease
                .as_ref()
                .map_or(Value::Null, |lease| json!(lease.lease_id)),
        );
        if granted {
            details.insert("userGrantedAttempt".into(), json!(true));
            details.insert("maximumAttempts".into(), to_json(&node.maximum_attempts)?);
        }
        self.mutate(
            project_id,
            entity_type,
            entity_id,
            expected_version,
            &format!("{entity_type}.node_state_changed"),
            idempotency_key,
            updated,
            None,
            Some(details),
            false,
            Some(intent),
        )
    }

    pub fn create_direction(
        &self,
        project_id: &str,
        input: &DirectionInput,
        spawned: Option<&SpawnedDirection>,
    ) -> Result<Value> {
        let question = js_trim(&input.question);
        let decision_use = js_trim(&input.decision_use);
        let evaluation_contract = input
            .evaluation_contract
            .clone()
            .filter(|value| !value.is_null())
            .unwrap_or_else(|| json!({ "metrics": [], "datasets": [], "seeds": [0] }));
        let mission_id = input
            .mission_id
            .as_deref()
            .map_or(Value::Null, |id| json!(id));
        let origin = spawned.map_or(Value::Null, |spawned| spawned.origin.clone());
        if let Some(replay) = created_projection_replay(
            self.store(project_id)?,
            project_id,
            &input.idempotency_key,
            "direction",
            "direction.created",
        )? {
            let value = &replay["value"];
            if !is(value.get("question"), question)
                || !is(value.get("decisionUse"), decision_use)
                || !same(value.get("missionId"), Some(&mission_id))
                || canonical_json(&value["evaluationContract"])
                    != canonical_json(&evaluation_contract)
                || canonical_json(&or_null(value.get("origin"))) != canonical_json(&origin)
            {
                return rejected(
                    "Direction creation idempotency key was reused for a different request",
                );
            }
            return Ok(replay);
        }
        if !evaluation_contract.is_object() {
            return rejected("Direction evaluation contract must be an object");
        }
        if question.is_empty() || decision_use.is_empty() {
            return rejected("Direction question and decision use are required");
        }
        if let Some(mission) = input.mission_id.as_deref().filter(|id| !id.is_empty()) {
            self.mission(project_id, mission)?;
        }
        let direction_id = create_id(IdPrefix::Dir);
        let now = now_utc();
        let graph = VersionedDag::new(
            &direction_id,
            default_direction_nodes(&now),
            "initial plan",
            1,
        )?
        .current();
        let evaluation_contract_hash = sha256(&evaluation_contract);
        let mut scope =
            json!({ "missionId": mission_id, "evaluationContractHash": evaluation_contract_hash });
        let mut budget = json!({ "evaluationContractHash": evaluation_contract_hash });
        if let Some(spawned) = spawned {
            scope["origin"] = spawned.origin.clone();
            scope["authorizedBy"] = json!("mission_approval");
            budget
                .as_object_mut()
                .expect("object")
                .extend(spawned.budget.clone());
        }
        let value = json!({
            "directionId": direction_id, "projectId": project_id, "missionId": mission_id, "questionId": format!("question_{}", random_hex()), "question": question, "decisionUse": decision_use,
            "falsifiability": { "supportingOutcome": format!("Evidence supports: {question}"), "refutingOutcome": format!("Evidence refutes: {question}"), "inconclusiveOutcome": format!("Evidence is inconclusive for: {question}") },
            "projectClaimIds": [], "scope": scope, "evaluationContractId": format!("evaluation_{}", random_hex()), "evaluationContract": evaluation_contract,
            "evaluationContractHash": evaluation_contract_hash, "plannedBaselineExperimentId": create_id(IdPrefix::Exp), "integrationBranch": format!("direction/{direction_id}"),
            "stoppingRules": [{ "ruleId": format!("stop_{}", random_hex()), "statement": "Stop when the evaluation contract is exhausted." }],
            "budget": budget, "closureRubric": [{ "rubricId": format!("rubric_{}", random_hex()), "statement": decision_use }],
            "createdBy": if spawned.is_some() { "mission_director" } else { "user" }, "approvedBy": null, "activatedAt": null, "canonicalContract": null,
            "acceptedBaseline": null, "state": "draft", "graphVersion": graph.version, "nodes": to_json(&graph.nodes)?, "origin": origin, "baseCommit": spawned.map_or(Value::Null, |spawned| json!(spawned.base_commit)), "createdAt": now, "updatedAt": now,
        });
        self.mutate(
            project_id,
            "direction",
            &direction_id,
            0,
            "direction.created",
            &input.idempotency_key,
            value,
            Some(graph_input("direction", &graph, "initial direction plan")?),
            None,
            false,
            None,
        )
    }

    pub fn transition_direction(
        &self,
        project_id: &str,
        direction_id: &str,
        expected_version: i64,
        next: &str,
        idempotency_key: &str,
    ) -> Result<Value> {
        let intent = json!({ "command": "direction.transition", "directionId": direction_id, "expectedVersion": expected_version, "next": next });
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            "direction",
            direction_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.direction(project_id, direction_id)?;
        let value = &current["value"];
        let state = string(value.get("state"));
        if !is_legal_direction_transition(&state, next) {
            return rejected(format!("Illegal Direction transition {state} -> {next}"));
        }
        if next == "closed" {
            let graph_complete = nodes_of(value)?.iter().all(|node| {
                !node.required
                    || matches!(node.state.as_str(), "accepted" | "waived" | "superseded")
            });
            let executions_complete = self
                .autoresearch(project_id)?
                .iter()
                .filter(|execution| is(execution["value"].get("directionId"), direction_id))
                .all(|execution| {
                    matches!(
                        execution["state"].as_str(),
                        Some("completed" | "stopped" | "failed")
                    )
                });
            let evidence = self.submitted(project_id, None)?.iter().any(|event| {
                is(event["scope"].get("directionId"), direction_id)
                    && is_schema(&event["payload"], "evidence")
            });
            if !graph_complete
                || !executions_complete
                || !evidence
                || !self.passing_review(
                    project_id,
                    direction_id,
                    "direction_closure",
                    Some(version_of(&current) as f64),
                    None,
                )?
            {
                return rejected(
                    "Direction closure requires complete graph, terminal Autoresearch, evidence, and independent Review",
                );
            }
        }
        let now = now_utc();
        let activating = state == "proposed" && next == "active";
        let mut updated = value.clone();
        updated["state"] = json!(next);
        if activating {
            updated["approvedBy"] = json!("user");
            updated["activatedAt"] = json!(now);
        }
        updated["updatedAt"] = json!(now);
        self.mutate(
            project_id,
            "direction",
            direction_id,
            expected_version,
            &format!("direction.{next}"),
            idempotency_key,
            updated,
            None,
            None,
            activating,
            Some(intent),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mutate_direction_graph(
        &self,
        project_id: &str,
        direction_id: &str,
        expected_version: i64,
        base_graph_version: i64,
        operations: &Value,
        rationale: &str,
        evidence_ids: &[String],
        idempotency_key: &str,
    ) -> Result<Value> {
        let mut sorted = evidence_ids.to_vec();
        js_sort(&mut sorted);
        let intent = json!({ "command": "direction.graph_mutation", "directionId": direction_id, "expectedVersion": expected_version, "baseGraphVersion": base_graph_version, "operations": operations, "rationale": rationale, "evidenceIds": sorted });
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            "direction",
            direction_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.direction(project_id, direction_id)?;
        let value = &current["value"];
        let graph_version = value["graphVersion"].as_i64().unwrap_or_default();
        if graph_version != base_graph_version {
            return rejected(format!(
                "Stale graph version {base_graph_version}; current version is {graph_version}"
            ));
        }
        let mut graph =
            VersionedDag::new(direction_id, nodes_of(value)?, "hydrated", graph_version)?;
        let version = graph.apply(
            base_graph_version,
            &graph_operations(
                Some(operations),
                value["nodes"].as_array().map_or(&[][..], Vec::as_slice),
            )?,
            rationale,
            evidence_ids,
        )?;
        let mut updated = value.clone();
        updated["graphVersion"] = json!(version.version);
        updated["nodes"] = to_json(&version.nodes)?;
        updated["updatedAt"] = json!(now_utc());
        self.mutate(
            project_id,
            "direction",
            direction_id,
            expected_version,
            "direction.graph_changed",
            idempotency_key,
            updated,
            Some(graph_input("direction", &version, rationale)?),
            None,
            false,
            Some(intent),
        )
    }

    pub fn approve_graph_proposal(
        &self,
        project_id: &str,
        proposal_id: &str,
        expected_proposal_version: i64,
        idempotency_key: &str,
    ) -> Result<Value> {
        self.host.assert_writable(project_id)?;
        let store = self.store(project_id)?;
        let intent = json!({ "command": "graph.proposal_approval", "proposalId": proposal_id, "expectedProposalVersion": expected_proposal_version, "approval": { "proposalId": proposal_id, "approverRole": "user" } });
        if let Some(replay) = store.command_receipt(project_id, idempotency_key)? {
            let approved = store.projection(project_id, "graph_proposal", proposal_id)?;
            match approved {
                Some(approved)
                    if is(replay.get("intentHash"), &sha256(&intent))
                        && is(replay["event"].get("type"), "graph.proposal_approved")
                        && is(replay["event"].get("correlationId"), proposal_id)
                        && approved["state"] == "approved" =>
                {
                    return Ok(approved);
                }
                _ => {
                    return rejected(
                        "Idempotency key was already used for a different graph-proposal approval",
                    );
                }
            }
        }
        let pending = store.projection(project_id, "graph_proposal", proposal_id)?;
        let pending = match pending {
            Some(pending)
                if pending["state"] == "pending"
                    && version_of(&pending) == expected_proposal_version
                    && pending["value"].is_object()
                    && pending["value"]["record"].is_object()
                    && pending["value"]["scope"].is_object() =>
            {
                pending
            }
            _ => {
                return rejected(
                    "Graph proposal is not an approvable pending proposal at the requested version",
                );
            }
        };
        let record = &pending["value"]["record"];
        if !valid("graph-change-proposal", record)
            || record.get("approvalRequired") != Some(&Value::Bool(true))
            || !is_str(record.get("scopeType"))
            || !is_str(record.get("scopeId"))
            || number(record.get("baseGraphVersion")).is_none()
        {
            return rejected("Pending graph proposal is corrupt");
        }
        if is(record.get("scopeType"), "mission") && is(record.get("contractImpact"), "material") {
            return rejected(
                "Material Mission contract changes are rejected; create a new Mission contract instead of mutating the approved canonical contract",
            );
        }
        let apply_key = format!("graph-proposal-apply:{proposal_id}");
        if store.command_receipt(project_id, &apply_key)?.is_none() {
            let scope_id = string(record.get("scopeId"));
            let base = js_number(record.get("baseGraphVersion"));
            let rationale = string(record.get("rationale"));
            match record["scopeType"].as_str() {
                Some("mission") => {
                    let current = self.mission(project_id, &scope_id)?;
                    if !same(
                        current["value"].get("graphVersion"),
                        record.get("baseGraphVersion"),
                    ) {
                        return rejected(
                            "Graph proposal approval uses a stale Mission graph version",
                        );
                    }
                    let operations = Value::Array(graph_operations(
                        record.get("operations"),
                        current["value"]["nodes"]
                            .as_array()
                            .map_or(&[][..], Vec::as_slice),
                    )?);
                    self.mutate_mission_graph(
                        project_id,
                        &scope_id,
                        version_of(&current),
                        base as i64,
                        &operations,
                        &rationale,
                        &string_values(record.get("evidenceIds"))?,
                        &apply_key,
                        true,
                    )?;
                }
                Some("direction") => {
                    let current = self.direction(project_id, &scope_id)?;
                    if !same(
                        current["value"].get("graphVersion"),
                        record.get("baseGraphVersion"),
                    ) {
                        return rejected(
                            "Graph proposal approval uses a stale Direction graph version",
                        );
                    }
                    let operations = Value::Array(graph_operations(
                        record.get("operations"),
                        current["value"]["nodes"]
                            .as_array()
                            .map_or(&[][..], Vec::as_slice),
                    )?);
                    self.mutate_direction_graph(
                        project_id,
                        &scope_id,
                        version_of(&current),
                        base as i64,
                        &operations,
                        &rationale,
                        &string_values(record.get("evidenceIds"))?,
                        &apply_key,
                    )?;
                }
                _ => return rejected("Pending graph proposal has an unknown scope"),
            }
        }
        let mut payload = json!({ "proposalId": proposal_id, "approverRole": "user" });
        if let Some(impact) = record.get("contractImpact") {
            payload["contractImpact"] = impact.clone();
        }
        let mut value = pending["value"].clone();
        value["approvedAt"] = json!(now_utc());
        value["approvedBy"] = json!("user");
        let draft = json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "graph.proposal_approved", "source": "noshd", "scope": pending["value"]["scope"], "correlationId": proposal_id, "causationId": null, "payload": payload });
        let approved = store.mutate_projection(
            idempotency_key,
            Some(expected_proposal_version),
            &draft,
            &ProjectionInput {
                entity_type: "graph_proposal".into(),
                entity_id: proposal_id.into(),
                state: "approved".into(),
                value,
                graph: None,
                records: Vec::new(),
                intent: Some(intent),
            },
        )?;
        if !approved.replayed {
            self.host.publish(&approved.event);
        }
        Ok(approved.projection)
    }

    pub fn accept_direction_baseline(
        &self,
        project_id: &str,
        direction_id: &str,
        expected_version: i64,
        input: &BaselineInput,
    ) -> Result<Value> {
        let intent = json!({ "command": "direction.baseline_accept", "directionId": direction_id, "expectedVersion": expected_version, "commit": input.commit, "reviewId": input.review_id, "evaluationContractHash": input.evaluation_contract_hash });
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            &input.idempotency_key,
            "direction",
            direction_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.direction(project_id, direction_id)?;
        let value = &current["value"];
        if current["state"] != "active" {
            return rejected("Direction must be active before baseline acceptance");
        }
        if !is(
            value.get("evaluationContractHash"),
            &input.evaluation_contract_hash,
        ) {
            return rejected(
                "Baseline evaluation contract does not match the frozen Direction contract",
            );
        }
        if !crate::regex!(r"(?i)^[0-9a-f]{7,64}$").is_match(&input.commit) {
            return rejected("Baseline commit must be an immutable Git commit hash");
        }
        let baseline = canonical_direction_baseline_node(value)?;
        if baseline.state != "accepted" || version_of(&current) < 2 {
            return rejected(
                "Baseline acceptance requires the canonical baseline node to be accepted",
            );
        }
        let reviewed_direction_version = version_of(&current) - 1;
        if !self.passing_review(
            project_id,
            &baseline.id,
            "task",
            Some(reviewed_direction_version as f64),
            Some(&input.review_id),
        )? {
            return rejected(
                "Baseline acceptance requires an exact canonical PASS Review for the baseline node at the pre-acceptance Direction projection version",
            );
        }
        let commit = git_commit(&self.repository_root(project_id)?, &input.commit)?;
        let mut updated = value.clone();
        updated["acceptedBaseline"] = json!({ "commit": commit, "reviewId": input.review_id });
        updated["updatedAt"] = json!(now_utc());
        self.mutate(
            project_id,
            "direction",
            direction_id,
            expected_version,
            "direction.baseline_accepted",
            &input.idempotency_key,
            updated,
            None,
            None,
            false,
            Some(intent),
        )
    }

    pub fn create_autoresearch(
        &self,
        project_id: &str,
        input: &AutoresearchInput,
        spawned: Option<&SpawnedAutoresearch>,
    ) -> Result<Value> {
        let decision_question = js_trim(&input.decision_question);
        let maximum_experiments = input.maximum_experiments.unwrap_or(12.0);
        let maximum_rounds = input.maximum_rounds.unwrap_or(8.0);
        let maximum_wall_clock_seconds = input.maximum_wall_clock_seconds.unwrap_or(86_400.0);
        let maximum_model_tokens = input.maximum_model_tokens.unwrap_or(500_000.0);
        let maximum_gpu_seconds = input.maximum_gpu_seconds.unwrap_or(86_400.0);
        let maximum_disk_bytes = input.maximum_disk_bytes.unwrap_or(10_000_000_000.0);
        let mut family_tags = unique(input.family_tags.clone().unwrap_or_default());
        js_sort(&mut family_tags);
        let mut scope = unique(input.scope.clone().unwrap_or_default());
        js_sort(&mut scope);
        let direction_id = input
            .direction_id
            .as_deref()
            .map_or(Value::Null, |id| json!(id));
        if let Some(replay) = created_projection_replay(
            self.store(project_id)?,
            project_id,
            &input.idempotency_key,
            "autoresearch",
            "autoresearch.created",
        )? {
            let value = &replay["value"];
            let differs_number =
                |key: &str, expected: f64| number(value.get(key)) != Some(expected);
            let contract_differs = input.evaluation_contract.as_ref().is_some_and(|contract| {
                !contract.is_object()
                    || canonical_json(&value["evaluationContract"]) != canonical_json(contract)
            });
            if !is(value.get("decisionQuestion"), decision_question)
                || !same(value.get("directionId"), Some(&direction_id))
                || input
                    .mission_id
                    .as_deref()
                    .is_some_and(|mission| !is(value.get("missionId"), mission))
                || !same_string_set(value.get("familyTags"), &family_tags)
                || !same_string_set(value.get("scope"), &scope)
                || contract_differs
                || differs_number("maximumExperiments", maximum_experiments)
                || differs_number("maximumRounds", maximum_rounds)
                || differs_number("maximumWallClockSeconds", maximum_wall_clock_seconds)
                || differs_number("maximumModelTokens", maximum_model_tokens)
                || differs_number("maximumGpuSeconds", maximum_gpu_seconds)
                || differs_number("maximumDiskBytes", maximum_disk_bytes)
            {
                return rejected("Autoresearch idempotency key was reused with different input");
            }
            return Ok(replay);
        }
        if decision_question.is_empty() {
            return rejected("Autoresearch decision question is required");
        }
        let integer = |n: f64| n.is_finite() && n.trunc() == n;
        if !integer(maximum_experiments)
            || !(1.0..=64.0).contains(&maximum_experiments)
            || !integer(maximum_rounds)
            || !(1.0..=64.0).contains(&maximum_rounds)
            || !integer(maximum_wall_clock_seconds)
            || maximum_wall_clock_seconds < 1.0
            || !integer(maximum_model_tokens)
            || maximum_model_tokens < 1.0
            || !integer(maximum_gpu_seconds)
            || maximum_gpu_seconds < 0.0
            || !integer(maximum_disk_bytes)
            || maximum_disk_bytes < 1.0
        {
            return rejected("Autoresearch budgets are invalid");
        }
        let direction = match input.direction_id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => Some(self.direction(project_id, id)?),
            None => None,
        };
        if let Some(direction) = &direction
            && (direction["state"] != "active"
                || !nosh_wire::js::truthy(direction["value"].get("acceptedBaseline")))
        {
            return rejected(
                "Direction Autoresearch requires an active Direction with a reviewed immutable baseline",
            );
        }
        if let Some(mission) = input.mission_id.as_deref().filter(|id| !id.is_empty()) {
            self.mission(project_id, mission)?;
        }
        let evaluation_contract = [
            input.evaluation_contract.clone(),
            direction
                .as_ref()
                .and_then(|direction| direction["value"].get("evaluationContract").cloned()),
        ]
        .into_iter()
        .flatten()
        .find(|value| !value.is_null())
        .unwrap_or_else(|| json!({ "metrics": [], "datasets": [], "seeds": [0] }));
        if !evaluation_contract.is_object() {
            return rejected("Autoresearch evaluation contract must be an object");
        }
        let evaluation_contract_hash = sha256(&evaluation_contract);
        if let Some(direction) = &direction
            && !is(
                direction["value"].get("evaluationContractHash"),
                &evaluation_contract_hash,
            )
        {
            return rejected("Autoresearch cannot change the frozen Direction evaluation contract");
        }
        let fingerprint = sha256(
            &json!({ "decisionQuestion": decision_question.to_lowercase(), "familyTags": family_tags, "scope": scope, "evaluationContractHash": evaluation_contract_hash }),
        );
        // Siblings share a Direction; without one, they share a Mission (or are both standalone).
        let mission_id = input
            .mission_id
            .as_deref()
            .map_or(Value::Null, |id| json!(id));
        let has_direction = input
            .direction_id
            .as_deref()
            .is_some_and(|id| !id.is_empty());
        let siblings: Vec<Value> = self
            .autoresearch(project_id)?
            .into_iter()
            .filter(|execution| {
                same(
                    Some(&or_null(execution["value"].get("directionId"))),
                    Some(&direction_id),
                ) && (has_direction
                    || same(
                        Some(&or_null(execution["value"].get("missionId"))),
                        Some(&mission_id),
                    ))
            })
            .collect();
        if siblings.len() >= 12 {
            return rejected("Direction Autoresearch execution budget exhausted");
        }
        if siblings
            .iter()
            .any(|execution| is(execution["value"].get("fingerprint"), &fingerprint))
        {
            return rejected("Autoresearch execution is not meaningfully separate");
        }
        let autoresearch_id = create_id(IdPrefix::Ar);
        let now = now_utc();
        let direction_value = direction.as_ref().map(|direction| &direction["value"]);
        let resolved_mission = [
            input.mission_id.as_deref().map(|id| json!(id)),
            direction_value.and_then(|value| value.get("missionId").cloned()),
        ]
        .into_iter()
        .flatten()
        .find(|value| !value.is_null())
        .unwrap_or(Value::Null);
        let mut value = json!({
            "autoresearchId": autoresearch_id, "projectId": project_id, "directionId": direction_id, "missionId": resolved_mission,
            "invokedByRole": spawned.map_or("user", |spawned| spawned.invoked_by_role.as_str()), "decisionQuestion": decision_question, "familyTags": family_tags, "scope": scope, "fingerprint": fingerprint, "state": "draft",
            "evaluationContractId": direction_value.and_then(|value| value.get("evaluationContractId")).filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(format!("evaluation_{}", random_hex()))),
            "evaluationContract": evaluation_contract, "evaluationContractHash": evaluation_contract_hash, "rootExperimentId": create_id(IdPrefix::Exp), "acceptedFrontierExperimentIds": [],
            "forbiddenChangeScopes": [".nosh/**"], "variantPolicy": { "maximumDepth": maximum_rounds, "maximumChildrenPerParent": maximum_experiments },
            "stopConditions": [{ "type": "maximum_rounds", "value": maximum_rounds }], "promotionRubricIds": [format!("rubric_{}", random_hex())],
            "currentRound": 0, "maximumExperiments": maximum_experiments, "maximumRounds": maximum_rounds, "maximumDepth": maximum_rounds, "maximumChildrenPerParent": maximum_experiments, "maximumWallClockSeconds": maximum_wall_clock_seconds, "maximumModelTokens": maximum_model_tokens,
            "maximumGpuSeconds": maximum_gpu_seconds, "maximumDiskBytes": maximum_disk_bytes, "canonicalContract": null, "origin": spawned.map_or(Value::Null, |spawned| spawned.origin.clone()), "baseCommit": spawned.and_then(|spawned| spawned.base_commit.clone()), "createdAt": now, "updatedAt": now,
        });
        normalize_numbers(&mut value);
        self.mutate(
            project_id,
            "autoresearch",
            &autoresearch_id,
            0,
            "autoresearch.created",
            &input.idempotency_key,
            value,
            None,
            None,
            true,
            None,
        )
    }

    pub fn transition_autoresearch(
        &self,
        project_id: &str,
        autoresearch_id: &str,
        expected_version: i64,
        next: &str,
        idempotency_key: &str,
    ) -> Result<Value> {
        let intent = json!({ "command": "autoresearch.transition", "autoresearchId": autoresearch_id, "expectedVersion": expected_version, "next": next });
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            "autoresearch",
            autoresearch_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.autoresearch_execution(project_id, autoresearch_id)?;
        let value = &current["value"];
        let state = string(value.get("state"));
        if !autoresearch_transitions(&state).contains(&next) {
            return rejected(format!("Illegal Autoresearch transition {state} -> {next}"));
        }
        if next == "completed" {
            let packets: Vec<Value> = self
                .submitted(project_id, None)?
                .into_iter()
                .filter(|event| {
                    is(event["scope"].get("autoresearchId"), autoresearch_id)
                        && is_schema(&event["payload"], "autoresearch-completion-packet")
                })
                .map(|event| event["payload"].clone())
                .filter(|packet| {
                    is(packet.get("autoresearchId"), autoresearch_id)
                        && same(packet.get("directionId"), value.get("directionId"))
                        && same(
                            packet.get("evaluationContractHash"),
                            value.get("evaluationContractHash"),
                        )
                        && valid("autoresearch-completion-packet", packet)
                })
                .collect();
            let passed = match packets.as_slice() {
                [packet] => match packet.get("closureReviewId").and_then(Value::as_str) {
                    Some(review_id) => self.passing_review(
                        project_id,
                        autoresearch_id,
                        "autoresearch_closure",
                        Some(version_of(&current) as f64),
                        Some(review_id),
                    )?,
                    None => false,
                },
                _ => false,
            };
            if !passed {
                return rejected(
                    "Autoresearch completion requires exactly one current canonical completion packet and its exact PASS closure Review at the current execution version",
                );
            }
        }
        let mut updated = value.clone();
        updated["state"] = json!(next);
        updated["updatedAt"] = json!(now_utc());
        self.mutate(
            project_id,
            "autoresearch",
            autoresearch_id,
            expected_version,
            &format!("autoresearch.{next}"),
            idempotency_key,
            updated,
            None,
            None,
            false,
            Some(intent),
        )
    }

    pub fn advance_autoresearch(
        &self,
        project_id: &str,
        autoresearch_id: &str,
        expected_version: i64,
        round: f64,
        frontier: &[String],
        idempotency_key: &str,
    ) -> Result<Value> {
        let mut canonical_frontier = unique(frontier.iter().cloned());
        js_sort(&mut canonical_frontier);
        let mut intent = json!({ "command": "autoresearch.advance", "autoresearchId": autoresearch_id, "expectedVersion": expected_version, "round": round, "frontier": canonical_frontier });
        normalize_numbers(&mut intent);
        if let Some(replay) = projection_intent_replay(
            self.store(project_id)?,
            project_id,
            idempotency_key,
            "autoresearch",
            autoresearch_id,
            &intent,
        )? {
            return Ok(replay);
        }
        let current = self.autoresearch_execution(project_id, autoresearch_id)?;
        let value = &current["value"];
        // A NaN bound fails both comparisons, as in JavaScript.
        let below =
            number(value.get("currentRound")).is_some_and(|current_round| round < current_round);
        let above = number(value.get("maximumRounds")).is_some_and(|maximum| round > maximum);
        if current["state"] != "running" || below || above {
            return rejected("Autoresearch round update violates its contract");
        }
        let mut updated = value.clone();
        updated["currentRound"] = to_json(&round)?;
        updated["acceptedFrontierExperimentIds"] = json!(canonical_frontier);
        updated["updatedAt"] = json!(now_utc());
        self.mutate(
            project_id,
            "autoresearch",
            autoresearch_id,
            expected_version,
            "autoresearch.round_completed",
            idempotency_key,
            updated,
            None,
            None,
            false,
            Some(intent),
        )
    }
}

// ---- Helpers ----

fn node(
    id: String,
    node_type: &str,
    title: &str,
    criterion_ids: &[String],
    dependencies: Vec<String>,
    created_at: &str,
) -> GraphNode {
    GraphNode {
        id,
        node_type: node_type.into(),
        title: title.into(),
        required: true,
        criterion_ids: criterion_ids.to_vec(),
        hard_dependencies: dependencies,
        soft_dependencies: Vec::new(),
        state: "pending".into(),
        attempt: 0.0,
        maximum_attempts: 3.0,
        priority: 5.0,
        critical_weight: 1.0,
        created_at: created_at.into(),
        lease: None,
        extra: Map::new(),
    }
}

fn valid_research_plan(plan: Option<&Value>) -> Result<Value> {
    let Some(plan) = plan.filter(|plan| !plan.is_null()) else {
        return Ok(Value::Null);
    };
    let directions = number(plan.get("directions"));
    let mission_autoresearch = plan.get("missionAutoresearch").and_then(Value::as_bool);
    let (Some(directions), Some(mission_autoresearch)) = (
        directions.filter(|n| {
            plan.is_object() && is_integer(plan.get("directions")) && (0.0..=4.0).contains(n)
        }),
        mission_autoresearch,
    ) else {
        return rejected(
            "Mission researchPlan needs directions (0-4) and missionAutoresearch (true or false)",
        );
    };
    Ok(if directions != 0.0 || mission_autoresearch {
        json!({ "directions": directions as i64, "missionAutoresearch": mission_autoresearch })
    } else {
        Value::Null
    })
}

/// A research plan runs Directions and Autoresearch under the Mission's own ceiling, so it starts
/// with a larger one.
fn mission_budgets(
    plan: &Value,
    requested: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>> {
    let planned = !plan.is_null();
    let mut budgets = json!({ "maximumModelTokens": if planned { 4_000_000 } else { 1_000_000 }, "maximumWallClockSeconds": if planned { 259_200 } else { 86_400 }, "maximumGpuSeconds": 0, "maximumDiskBytes": 10_000_000_000_u64, "maximumConcurrentAgents": 2 });
    let budgets = budgets.as_object_mut().expect("object");
    budgets.extend(requested.cloned().unwrap_or_default());
    let bounds: [(&str, u64, u64); 5] = [
        ("maximumModelTokens", 1, 50_000_000),
        ("maximumWallClockSeconds", 60, 2_592_000),
        ("maximumGpuSeconds", 0, 2_592_000),
        ("maximumDiskBytes", 1, 1_000_000_000_000),
        ("maximumConcurrentAgents", 1, 8),
    ];
    for (key, low, high) in bounds {
        let value = budgets.get(key);
        if !is_integer(value) || number(value).is_none_or(|n| n < low as f64 || n > high as f64) {
            return rejected(format!(
                "Mission budget {key} must be an integer from {low} to {high}"
            ));
        }
    }
    let mut budgets = Value::Object(budgets.clone());
    normalize_numbers(&mut budgets);
    Ok(budgets.as_object().cloned().expect("object"))
}

fn default_mission_nodes(
    created_at: &str,
    criterion_ids: &[String],
    plan: &Value,
) -> Vec<GraphNode> {
    let mut spawns = Vec::new();
    if !plan.is_null() {
        for index in 0..plan["directions"].as_u64().unwrap_or_default() {
            let mut spawn = node(
                format!("mnode_{}", random_hex()),
                "research_direction",
                &format!(
                    "Research Direction {}: answer one question the objective depends on",
                    index + 1
                ),
                &[],
                Vec::new(),
                created_at,
            );
            spawn.maximum_attempts = 2.0;
            spawns.push(spawn);
        }
        if plan["missionAutoresearch"] == true {
            let mut spawn = node(
                format!("mnode_{}", random_hex()),
                "autoresearch",
                "Mission Autoresearch: improve one measured result under a frozen evaluation",
                &[],
                Vec::new(),
                created_at,
            );
            spawn.maximum_attempts = 2.0;
            spawns.push(spawn);
        }
    }
    let base = default_task_nodes(created_at, criterion_ids);
    if spawns.is_empty() {
        return base;
    }
    // The deliverable builds on what the research children found; the final review sees everything.
    let [mut implementation, literature, mut review] =
        <[GraphNode; 3]>::try_from(base).expect("three task nodes");
    let spawn_ids: Vec<String> = spawns.iter().map(|spawn| spawn.id.clone()).collect();
    implementation.hard_dependencies = spawn_ids.clone();
    review.hard_dependencies = [implementation.id.clone(), literature.id.clone()]
        .into_iter()
        .chain(spawn_ids)
        .collect();
    spawns.extend([implementation, literature, review]);
    spawns
}

fn default_task_nodes(created_at: &str, criterion_ids: &[String]) -> Vec<GraphNode> {
    let plan = format!("mnode_{}", random_hex());
    let evidence = format!("mnode_{}", random_hex());
    let review = format!("mnode_{}", random_hex());
    vec![
        node(
            plan.clone(),
            "implementation",
            "Produce the bounded deliverable",
            criterion_ids,
            Vec::new(),
            created_at,
        ),
        node(
            evidence.clone(),
            "literature_review",
            "Collect and cite the sources the Mission relies on (repository files and, where relevant, literature); establishing implementation results is the implementation node's job",
            &[],
            Vec::new(),
            created_at,
        ),
        node(
            review,
            "final_review",
            "Independent final review",
            &[],
            vec![plan, evidence],
            created_at,
        ),
    ]
}

fn default_direction_nodes(created_at: &str) -> Vec<GraphNode> {
    let baseline = format!("dnode_{}", random_hex());
    let research = format!("dnode_{}", random_hex());
    let synthesis = format!("dnode_{}", random_hex());
    vec![
        node(
            baseline.clone(),
            "general_worker",
            "Reproduce accepted baseline",
            &[],
            Vec::new(),
            created_at,
        ),
        node(
            research.clone(),
            "general_worker",
            "Evaluate bounded hypothesis families",
            &[],
            vec![baseline],
            created_at,
        ),
        node(
            synthesis,
            "reviewer",
            "Synthesize and review results",
            &[],
            vec![research],
            created_at,
        ),
    ]
}

pub(super) fn mission_node_record(mission: &Value, graph_node: &Value, rationale: &str) -> Value {
    let node_type = graph_node["type"].as_str().unwrap_or_default();
    let assigned_role = match node_type {
        "literature_review" => "librarian_researcher",
        "claim_review" | "final_review" | "approval" => "reviewer",
        _ => "general_worker",
    };
    json!({
        "$schema": schema_uri("mission-node"), "schemaVersion": 1, "missionId": mission["missionId"], "graphVersion": mission["graphVersion"], "nodeId": graph_node["id"], "nodeType": graph_node["type"], "title": graph_node["title"], "rationale": rationale,
        "criterionIds": graph_node["criterionIds"], "hardDependencyNodeIds": graph_node["hardDependencies"], "softDependencyNodeIds": graph_node["softDependencies"], "inputArtifactIds": [], "expectedOutputKinds": [], "assignedRole": assigned_role,
        "preconditionValidatorIds": [], "postconditionValidatorIds": ["validator_task.postflight"], "reviewRubricIds": ["rubric_scientific.correctness"],
        "budget": { "maximumAttempts": graph_node["maximumAttempts"], "priority": graph_node["priority"], "criticalWeight": graph_node["criticalWeight"] },
        "workspacePolicy": {}, "state": graph_node["state"], "attempt": graph_node["attempt"], "lease": graph_node["lease"], "createdAt": graph_node["createdAt"],
    })
}

/// `{ ...contract, contractHash: sha256(contract) }`.
fn hashed(mut contract: Value) -> Value {
    let hash = sha256(&contract);
    contract["contractHash"] = json!(hash);
    contract
}

fn numbered(items: &Value, id_key: &str, prefix: &str) -> Value {
    Value::Array(items.as_array().into_iter().flatten().enumerate().map(|(index, statement)| json!({ id_key: format!("{prefix}_{}", index + 1), "statement": statement })).collect())
}

pub(super) fn canonical_contract(entity_type: &str, value: &Value) -> Result<Value> {
    match entity_type {
        "mission" => {
            let Some(approved_at) = value
                .get("approvedAt")
                .and_then(Value::as_str)
                .filter(|at| !at.is_empty())
            else {
                return rejected("Mission canonical contract requires an approval timestamp");
            };
            let budgets = &value["budgets"];
            let deadline = nosh_wire::iso_from_ms(
                parse_ms(approved_at) + js_number(budgets.get("maximumWallClockSeconds")) * 1000.0,
            )
            .map_or_else(|| rejected("Invalid time value"), Ok)?;
            Ok(hashed(json!({
                "$schema": schema_uri("mission-contract"), "schemaVersion": 1, "templateVersion": "1.0.0", "missionId": value["missionId"], "projectId": value["projectId"], "predecessorMissionId": null, "objectiveVersion": 1,
                "title": value["title"], "objective": value["objective"], "nonObjectives": value["nonObjectives"], "projectAlignment": { "projectGoalId": value["projectId"], "contribution": value["objective"] },
                "deliverables": numbered(&value["deliverables"], "deliverableId", "deliverable"), "successCriteria": numbered(&value["successCriteria"], "criterionId", "criterion"), "exitProofs": numbered(&value["successCriteria"], "proofId", "proof"),
                "startingEvidenceIds": value["startingEvidence"], "assumptions": [], "initialGraphVersion": 1,
                "budgets": { "deadline": deadline, "maximumWallClockSeconds": budgets["maximumWallClockSeconds"], "maximumModelTokens": budgets["maximumModelTokens"], "maximumGpuSeconds": budgets["maximumGpuSeconds"], "maximumDiskBytes": budgets["maximumDiskBytes"], "maximumExperiments": 0, "maximumConcurrentAgents": budgets["maximumConcurrentAgents"], "maximumConcurrentGpuJobs": 0 },
                "approvalBoundaries": { "values": value["approvalBoundaries"] }, "pausePolicy": { "mode": value["pausePolicy"] }, "finalReviewRubric": numbered(&value["finalReviewRubric"], "rubricId", "rubric"),
                "approvedBy": "user", "approvedAt": approved_at,
            })))
        }
        "direction" => {
            if !nosh_wire::js::truthy(value.get("activatedAt"))
                || !is(value.get("approvedBy"), "user")
            {
                return rejected(
                    "Direction canonical contract requires a user activation timestamp",
                );
            }
            Ok(hashed(json!({
                "$schema": schema_uri("direction-contract"), "schemaVersion": 1, "templateVersion": "1.0.0", "directionId": value["directionId"], "projectId": value["projectId"], "missionId": value["missionId"], "predecessorDirectionId": null,
                "questionId": value["questionId"], "primaryQuestion": value["question"], "decisionUse": value["decisionUse"], "falsifiability": value["falsifiability"], "projectClaimIds": value["projectClaimIds"], "scope": value["scope"],
                "evaluationContractId": value["evaluationContractId"], "evaluationContract": value["evaluationContract"], "evaluationContractHash": value["evaluationContractHash"], "baselineExperimentId": value["plannedBaselineExperimentId"], "integrationBranch": value["integrationBranch"],
                "initialGraphVersion": 1, "stoppingRules": value["stoppingRules"], "budget": value["budget"], "closureRubric": value["closureRubric"], "createdBy": value["createdBy"], "approvedBy": value["approvedBy"], "activatedAt": value["activatedAt"],
            })))
        }
        _ => {
            let family_tags = if value["familyTags"]
                .as_array()
                .is_some_and(|tags| !tags.is_empty())
            {
                value["familyTags"].clone()
            } else {
                json!([value["decisionQuestion"]])
            };
            Ok(hashed(json!({
                "$schema": schema_uri("autoresearch-contract"), "schemaVersion": 1, "templateVersion": "1.0.0", "autoresearchId": value["autoresearchId"], "projectId": value["projectId"], "missionId": value["missionId"], "directionId": value["directionId"],
                "invokedByRole": value["invokedByRole"], "decisionQuestion": value["decisionQuestion"], "distinctPurpose": value["decisionQuestion"], "hypothesisFamilyTags": family_tags, "duplicateCheck": { "fingerprint": value["fingerprint"] },
                "evaluationContractId": value["evaluationContractId"], "evaluationContractHash": value["evaluationContractHash"], "rootExperimentId": value["rootExperimentId"], "acceptedFrontierExperimentIds": value["acceptedFrontierExperimentIds"],
                "allowedChangeScopes": value["scope"], "forbiddenChangeScopes": value["forbiddenChangeScopes"], "variantPolicy": value["variantPolicy"],
                "budgets": { "maximumExperiments": value["maximumExperiments"], "maximumRounds": value["maximumRounds"], "maximumWallClockSeconds": value["maximumWallClockSeconds"], "maximumModelTokens": value["maximumModelTokens"], "maximumGpuSeconds": value["maximumGpuSeconds"], "maximumDiskBytes": value["maximumDiskBytes"] },
                "stopConditions": value["stopConditions"], "promotionRubricIds": value["promotionRubricIds"], "createdAt": value["createdAt"],
            })))
        }
    }
}

fn canonical_direction_baseline_node(direction: &Value) -> Result<GraphNode> {
    let nodes: Vec<GraphNode> = nodes_of(direction)?
        .into_iter()
        .filter(|node| {
            node.node_type == "general_worker"
                && node.title == "Reproduce accepted baseline"
                && node.hard_dependencies.is_empty()
                && node.soft_dependencies.is_empty()
        })
        .collect();
    match <[GraphNode; 1]>::try_from(nodes) {
        Ok([node]) => Ok(node),
        Err(_) => rejected("Direction has no unique canonical baseline node"),
    }
}

/// A user retry of an attempt-exhausted node grants exactly one more attempt, recorded on the
/// transition event; automatic retries never exceed the budget.
fn grant_user_attempt(
    mut nodes: Vec<GraphNode>,
    node_id: &str,
    next: &str,
    user_retry: bool,
) -> (Vec<GraphNode>, bool) {
    let Some(node) = nodes.iter_mut().find(|node| node.id == node_id) else {
        return (nodes, false);
    };
    if !user_retry
        || next != "ready"
        || !matches!(node.state.as_str(), "blocked" | "failed")
        || node.attempt < node.maximum_attempts
    {
        return (nodes, false);
    }
    node.maximum_attempts = node.attempt + 1.0;
    (nodes, true)
}

/// A blocked or paused scope can be repaired by returning its blocked or failed node to ready;
/// reactivating then resumes scheduling.
fn repair_retry(state: &str, nodes: &[GraphNode], node_id: &str, next: &str) -> bool {
    matches!(state, "blocked" | "paused")
        && next == "ready"
        && nodes
            .iter()
            .find(|node| node.id == node_id)
            .is_some_and(|node| matches!(node.state.as_str(), "blocked" | "failed"))
}

fn assert_lease_transition(next: &str, lease: Option<&Value>, expected_version: i64) -> Result<()> {
    if (next == "leased") != nosh_wire::js::truthy(lease) {
        return rejected(
            "Only a leased transition may carry a lease, and leased transitions require one",
        );
    }
    if let Some(lease) = lease.filter(|lease| nosh_wire::js::truthy(Some(lease))) {
        let expires = lease
            .get("expiresAt")
            .and_then(Value::as_str)
            .map_or(f64::NAN, parse_ms);
        if !nosh_wire::js::truthy(lease.get("leaseId"))
            || !nosh_wire::js::truthy(lease.get("ownerId"))
            || number(lease.get("version")) != Some(expected_version as f64)
            || !expires.is_finite()
            || expires <= now_ms()
        {
            return rejected(
                "Lease must name an owner, match the projection version, and expire in the future",
            );
        }
    }
    Ok(())
}

pub(super) fn now_ms() -> f64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as f64
}

fn git_commit(repository_root: &std::path::Path, commit: &str) -> Result<String> {
    let output = nosh_sys::process::hidden("git")
        .arg("-C")
        .arg(repository_root)
        .args(["rev-parse", &format!("{commit}^{{commit}}")])
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return rejected("Baseline commit does not resolve in the Project repository");
    }
    Ok(js_trim(&String::from_utf8_lossy(&output.stdout)).to_owned())
}

/// `graphOperations`: the operations normalised to their known fields, each checked against the
/// nodes it builds on.
pub(super) fn graph_operations(value: Option<&Value>, existing: &[Value]) -> Result<Vec<Value>> {
    let Some(items) = value
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    else {
        return rejected("Graph mutation requires operations");
    };
    let mut known: HashSet<String> = existing
        .iter()
        .filter_map(|node| node.get("id").map(|id| string(Some(id))))
        .collect();
    let mut operations = Vec::new();
    for operation in items {
        let Some(kind) = operation
            .is_object()
            .then(|| operation.get("type").and_then(Value::as_str))
            .flatten()
        else {
            return rejected("Invalid graph operation");
        };
        let text = |key: &str| operation.get(key).and_then(Value::as_str);
        if kind == "add_node" && is_graph_node(&operation["node"]) {
            let node = &operation["node"];
            let id = string(node.get("id"));
            let dependencies = node["hardDependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(node["softDependencies"].as_array().into_iter().flatten());
            if known.contains(&id)
                || dependencies
                    .map(|dependency| string(Some(dependency)))
                    .any(|dependency| !known.contains(&dependency))
            {
                return rejected("Added graph node has duplicate or unresolved dependencies");
            }
            known.insert(id);
            operations.push(json!({ "type": "add_node", "node": node }));
            continue;
        }
        if let ("add_hard_edge" | "remove_hard_edge", Some(from), Some(to)) =
            (kind, text("from"), text("to"))
            && from != to
            && known.contains(from)
            && known.contains(to)
        {
            operations.push(json!({ "type": kind, "from": from, "to": to }));
            continue;
        }
        if kind == "supersede_node"
            && let Some(node_id) = text("nodeId")
            && known.contains(node_id)
        {
            let replacement = operation.get("replacementId");
            let replacement_ok = matches!(replacement, Some(Value::Null))
                || replacement
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != node_id && known.contains(id));
            if replacement_ok {
                operations.push(json!({ "type": "supersede_node", "nodeId": node_id, "replacementId": replacement }));
                continue;
            }
        }
        return rejected("Invalid graph operation");
    }
    Ok(operations)
}

fn valid_graph_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !js_trim(text).is_empty() && nosh_wire::js::length(text) <= 256)
}

fn is_graph_node(value: &Value) -> bool {
    if !value.is_object()
        || !valid_graph_text(value.get("id"))
        || !valid_graph_text(value.get("type"))
        || !valid_graph_text(value.get("title"))
        || !value["required"].is_boolean()
    {
        return false;
    }
    let Some(criteria) = value["criterionIds"].as_array() else {
        return false;
    };
    if !criteria.iter().all(|item| {
        item.as_str()
            .is_some_and(|id| crate::regex!(r"^criterion_[1-9]\d*$").is_match(id))
    }) || criteria
        .iter()
        .map(canonical_json)
        .collect::<HashSet<_>>()
        .len()
        != criteria.len()
    {
        return false;
    }
    let (Some(hard), Some(soft)) = (
        value["hardDependencies"].as_array(),
        value["softDependencies"].as_array(),
    ) else {
        return false;
    };
    if !hard
        .iter()
        .chain(soft)
        .all(|item| valid_graph_text(Some(item)))
    {
        return false;
    }
    let dependencies: Vec<&str> = hard
        .iter()
        .chain(soft)
        .map(|item| item.as_str().expect("checked"))
        .collect();
    let id = value["id"].as_str().expect("checked");
    if dependencies.iter().collect::<HashSet<_>>().len() != dependencies.len()
        || dependencies.contains(&id)
    {
        return false;
    }
    let finite = |key: &str| number(value.get(key)).is_some_and(f64::is_finite);
    value["state"] == "pending"
        && number(value.get("attempt")) == Some(0.0)
        && is_integer(value.get("maximumAttempts"))
        && number(value.get("maximumAttempts")).is_some_and(|n| n >= 1.0)
        && finite("priority")
        && finite("criticalWeight")
        && value["createdAt"]
            .as_str()
            .is_some_and(|at| parse_ms(at).is_finite())
        && value.get("lease") == Some(&Value::Null)
}

fn assert_mission_criterion_ownership(criteria: usize, nodes: &[GraphNode]) -> Result<()> {
    const TYPES: &[&str] = &[
        "literature_review",
        "reproduction",
        "research_direction",
        "autoresearch",
        "implementation",
        "dataset",
        "analysis",
        "claim_review",
        "paper_section",
        "integration",
        "approval",
        "synthesis",
        "final_review",
    ];
    if nodes
        .iter()
        .any(|node| !TYPES.contains(&node.node_type.as_str()))
    {
        return rejected("Mission graph nodes must use canonical mission node types");
    }
    let expected: Vec<String> = (1..=criteria)
        .map(|index| format!("criterion_{index}"))
        .collect();
    let assigned: Vec<&String> = nodes
        .iter()
        .filter(|node| node.state != "superseded")
        .flat_map(|node| &node.criterion_ids)
        .collect();
    if assigned.iter().collect::<HashSet<_>>().len() != assigned.len()
        || expected
            .iter()
            .any(|criterion| assigned.iter().filter(|id| **id == criterion).count() != 1)
        || assigned
            .iter()
            .any(|criterion| !expected.contains(criterion))
    {
        return rejected("Each Mission contract criterion must have exactly one graph-node owner");
    }
    Ok(())
}

fn created_projection_replay(
    store: &EventStore,
    project_id: &str,
    idempotency_key: &str,
    entity_type: &str,
    event_type: &str,
) -> Result<Option<Value>> {
    let Some(receipt) = store.command_receipt(project_id, idempotency_key)? else {
        return Ok(None);
    };
    let projection = receipt
        .get("projection")
        .filter(|projection| projection.is_object());
    match projection {
        Some(projection)
            if is(receipt["event"].get("type"), event_type)
                && is(receipt["event"]["scope"].get("projectId"), project_id)
                && is(projection.get("projectId"), project_id)
                && is(projection.get("entityType"), entity_type) =>
        {
            Ok(Some(projection.clone()))
        }
        _ => rejected("Creation idempotency key was previously used for a different command"),
    }
}

fn projection_intent_replay(
    store: &EventStore,
    project_id: &str,
    idempotency_key: &str,
    entity_type: &str,
    entity_id: &str,
    intent: &Value,
) -> Result<Option<Value>> {
    let Some(receipt) = store.command_receipt(project_id, idempotency_key)? else {
        return Ok(None);
    };
    let projection = receipt
        .get("projection")
        .filter(|projection| projection.is_object());
    match projection {
        Some(projection)
            if is(projection.get("projectId"), project_id)
                && is(projection.get("entityType"), entity_type)
                && is(projection.get("entityId"), entity_id)
                && is(receipt.get("intentHash"), &sha256(intent)) =>
        {
            Ok(Some(projection.clone()))
        }
        _ => rejected("Idempotency key was previously used for a different projection intent"),
    }
}

fn assert_existing_evidence_ids(records: &[Value], evidence_ids: &[String]) -> Result<()> {
    let existing: HashSet<&str> = records
        .iter()
        .filter_map(|record| record.get("evidenceId").and_then(Value::as_str))
        .collect();
    if evidence_ids
        .iter()
        .any(|id| !existing.contains(id.as_str()))
    {
        return rejected("Mission references evidence that does not exist in the Project");
    }
    Ok(())
}
