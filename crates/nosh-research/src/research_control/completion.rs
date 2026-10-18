//! Mission completion: the reviewed basis a Mission closes on, research children's outcomes, and
//! budget use (model tokens, running time, GPU time, disk).

use super::records::{exact_review_join, review_authority};
use super::*;
use crate::artifacts::ArtifactStore;
use crate::graph::parse_ms;
use nosh_wire::js::{max, slice_to};
use nosh_wire::{event_budget_tokens, now_utc, sha256};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BasisCriterion {
    pub criterion_id: String,
    pub statement: String,
    pub task_id: String,
    pub review_id: String,
    pub validator_run_ids: Vec<String>,
    pub artifact_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BasisCompletion {
    pub node_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub completion: Value,
    pub review: Value,
    pub artifact_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub validator_run_ids: Vec<String>,
}

/// What a Mission's completion rests on (`MissionCompletionBasis`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionBasis {
    pub completions: Vec<BasisCompletion>,
    pub criteria: Vec<BasisCriterion>,
    pub artifact_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub validator_run_ids: Vec<String>,
    pub claims: Vec<Value>,
    pub unresolved_claim_ids: Vec<String>,
    pub open_defect_ids: Vec<String>,
    pub issues: Vec<String>,
}

struct TaskResult {
    packet: Value,
    completion: Value,
    verdict: Value,
}

impl<H: Host> ResearchControl<H> {
    pub fn mission_completion_basis(
        &self,
        project_id: &str,
        mission_id: &str,
    ) -> Result<CompletionBasis> {
        let mission = self.mission(project_id, mission_id)?;
        let value = &mission["value"];
        let nodes: Vec<&Value> = value["nodes"].as_array().into_iter().flatten().collect();
        // Mission-own records only: a research child's reviews and claims are judged by its own
        // closure, which the spawn node requires.
        let records: Vec<Value> = self
            .submitted(project_id, None)?
            .into_iter()
            .filter(|event| {
                is(event["scope"].get("missionId"), mission_id)
                    && !nosh_wire::js::truthy(event["scope"].get("directionId"))
                    && !nosh_wire::js::truthy(event["scope"].get("autoresearchId"))
            })
            .collect();
        let record_values: Vec<Value> = records
            .iter()
            .map(|event| event["payload"].clone())
            .collect();
        let packets: Vec<&Value> = record_values
            .iter()
            .filter(|record| {
                is_schema(record, "task-packet")
                    && !is(record.get("assignedRole"), "reviewer")
                    && record["scope"].is_object()
                    && is(record["scope"].get("missionId"), mission_id)
            })
            .collect();
        let mut completions: Vec<BasisCompletion> = Vec::new();
        let mut criteria: Vec<BasisCriterion> = Vec::new();
        let mut issues: Vec<String> = Vec::new();
        // `Map<taskId, result>`: a repeated task keeps its first position and its last value.
        let mut task_results: Vec<(String, TaskResult)> = Vec::new();
        let excluded = [
            "reviewer",
            "claim_review",
            "final_review",
            "approval",
            "research_direction",
            "autoresearch",
        ];
        for node in nodes.iter().filter(|node| {
            node["required"] == true
                && node["state"] == "accepted"
                && !node["type"]
                    .as_str()
                    .is_some_and(|kind| excluded.contains(&kind))
        }) {
            let node_id = string(node.get("id"));
            let mut candidates: Vec<TaskResult> = Vec::new();
            for packet in packets
                .iter()
                .filter(|packet| is(packet["scope"].get("graphNodeId"), &node_id))
            {
                let (Some(task_id), Some(agent_id), Some(response_schema)) = (
                    packet.get("taskId").and_then(Value::as_str),
                    packet.get("assignedAgentId").and_then(Value::as_str),
                    packet.get("responseSchema").and_then(Value::as_str),
                ) else {
                    continue;
                };
                if !valid("task-packet", packet)
                    || !same(packet.get("attempt"), node.get("attempt"))
                {
                    continue;
                }
                let correlation = format!("task:{task_id}");
                let completion_records: Vec<&Value> = records
                    .iter()
                    .filter(|event| {
                        is(event.get("correlationId"), &correlation)
                            && is(event["payload"].get("$schema"), response_schema)
                    })
                    .map(|event| &event["payload"])
                    .collect();
                let [completion] = completion_records.as_slice() else {
                    continue;
                };
                if validate_record(response_schema, completion).is_err()
                    || !is(completion.get("taskOutcome"), "completed")
                    || completion.get("readyForReview") != Some(&Value::Bool(true))
                {
                    continue;
                }
                // The reviewer has its own Task ID. Bind its request to the worker packet by the
                // daemon-issued contract reference, not by the review session's correlation.
                let packet_hash = sha256(packet);
                let mut requests: Vec<&Value> = Vec::new();
                for entry in &record_values {
                    let target = &entry["target"];
                    if !(is_schema(entry, "review-request")
                        && is(entry.get("reviewType"), "task")
                        && target.is_object()
                        && is(target.get("targetType"), "graph_node")
                        && is(target.get("targetId"), &node_id)
                        && entry["producerAgentIds"].is_array())
                    {
                        continue;
                    }
                    if string_values(entry.get("producerAgentIds"))?
                        .iter()
                        .any(|id| id == agent_id)
                        && entry["contractRefs"].is_array()
                        && objects(entry.get("contractRefs")).any(|reference| {
                            is(reference.get("kind"), "contract_task.packet")
                                && is(reference.get("id"), task_id)
                                && is(reference.get("hash"), &packet_hash)
                        })
                    {
                        requests.push(entry);
                    }
                }
                let [request] = requests.as_slice() else {
                    continue;
                };
                let Some(review_id) = request.get("reviewId").and_then(Value::as_str) else {
                    continue;
                };
                // The joined request is necessarily `request`, the only review-request with its ID.
                let Some(joined) = exact_review_join(&record_values, review_id) else {
                    continue;
                };
                let target_version = js_number(joined.request["target"].get("targetVersion"));
                if !self.passing_review(
                    project_id,
                    &node_id,
                    "task",
                    Some(target_version),
                    Some(&string(joined.request.get("reviewId"))),
                )? {
                    continue;
                }
                candidates.push(TaskResult {
                    packet: (*packet).clone(),
                    completion: (*completion).clone(),
                    verdict: joined.verdict,
                });
            }
            if candidates.len() != 1 {
                issues.push(if candidates.is_empty() { format!("accepted node {node_id} lacks one completed independently reviewed authoritative Task Packet") } else { format!("accepted node {node_id} has ambiguous independently reviewed Task Packet mappings") });
                continue;
            }
            let task = candidates.pop().expect("one candidate");
            let task_id = string(task.packet.get("taskId"));
            let references = completion_references(&task.completion);
            completions.push(BasisCompletion {
                node_id,
                task_id: task_id.clone(),
                agent_id: string(task.packet.get("assignedAgentId")),
                completion: task.completion.clone(),
                review: task.verdict.clone(),
                artifact_ids: references.0,
                evidence_ids: references.1,
                validator_run_ids: Vec::new(),
            });
            match task_results.iter_mut().find(|(id, _)| *id == task_id) {
                Some(entry) => entry.1 = task,
                None => task_results.push((task_id, task)),
            }
        }
        for (index, statement) in value["successCriteria"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let criterion_id = format!("criterion_{}", index + 1);
            let candidates: Vec<&TaskResult> = task_results
                .iter()
                .map(|(_, task)| task)
                .filter(|task| {
                    objects(task.packet.get("acceptanceCriteria"))
                        .any(|criterion| is(criterion.get("criterionId"), &criterion_id))
                })
                .collect();
            let [task] = candidates.as_slice() else {
                issues.push(if candidates.is_empty() { format!("Mission contract criterion {criterion_id} is absent from an independently reviewed accepted-node Task Packet") } else { format!("Mission contract criterion {criterion_id} has ambiguous accepted-node Task Packet mappings") });
                continue;
            };
            let completion_criteria: Vec<&Value> = objects(task.completion.get("criteria"))
                .filter(|criterion| is(criterion.get("criterionId"), &criterion_id))
                .collect();
            let review_criteria: Vec<&Value> = objects(task.verdict.get("criteria"))
                .filter(|criterion| is(criterion.get("criterionId"), &criterion_id))
                .collect();
            let deferred = completion_criteria
                .first()
                .is_some_and(|criterion| is(criterion.get("workerClaim"), "deferred_to_review"));
            let satisfied = |criterion: &Value| {
                is(criterion.get("workerClaim"), "satisfied")
                    && criterion["validatorRunIds"]
                        .as_array()
                        .is_some_and(|ids| !ids.is_empty() && ids.iter().all(Value::is_string))
            };
            if completion_criteria.len() != 1
                || review_criteria.len() != 1
                || (!deferred && !satisfied(completion_criteria[0]))
                || !is(review_criteria[0].get("status"), "PASS")
            {
                issues.push(format!("Mission contract criterion {criterion_id} lacks one satisfied worker claim, validator set, and exact independent PASS Review"));
                continue;
            }
            // A deferred criterion's only validator is the independent Review that passed it.
            let validator_run_ids: Vec<String> = if deferred {
                vec![string(task.verdict.get("reviewId"))]
            } else {
                completion_criteria[0]["validatorRunIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|id| string(Some(id)))
                    .collect()
            };
            let references = completion_references(&task.completion);
            let task_id = string(task.packet.get("taskId"));
            criteria.push(BasisCriterion {
                criterion_id,
                statement: string(Some(statement)),
                task_id: task_id.clone(),
                review_id: string(task.verdict.get("reviewId")),
                validator_run_ids: validator_run_ids.clone(),
                artifact_ids: references.0,
                evidence_ids: references.1,
            });
            if let Some(completion) = completions
                .iter_mut()
                .find(|entry| entry.task_id == task_id)
            {
                completion.validator_run_ids.extend(validator_run_ids);
            }
        }
        let mut child_evidence_ids: Vec<String> = Vec::new();
        for node in nodes.iter().filter(|node| {
            node["required"] == true
                && node["state"] == "accepted"
                && node["type"]
                    .as_str()
                    .is_some_and(|kind| SPAWN_NODE_TYPES.contains(&kind))
        }) {
            let node_id = string(node.get("id"));
            match self.spawned_child(project_id, mission_id, &node_id)? {
                Some(child)
                    if child.state
                        == if child.kind == "direction" {
                            "closed"
                        } else {
                            "completed"
                        } =>
                {
                    child_evidence_ids
                        .extend(self.child_evidence_ids(project_id, &child.entity_id)?)
                }
                _ => issues.push(format!(
                    "accepted node {node_id} lacks a reviewed research child outcome"
                )),
            }
        }
        let artifact_root = self
            .repository_root(project_id)?
            .join(".nosh")
            .join("artifacts");
        let requested_artifact_ids = unique(
            completions
                .iter()
                .flat_map(|completion| completion.artifact_ids.clone()),
        );
        let artifact_ids: Vec<String> = requested_artifact_ids
            .iter()
            .filter(|id| artifact_exists(&artifact_root, id))
            .cloned()
            .collect();
        issues.extend(
            requested_artifact_ids
                .iter()
                .filter(|id| !artifact_ids.contains(id))
                .map(|id| format!("Mission completion references missing Artifact {id}")),
        );
        let evidence_records = self.records(project_id, Some("evidence"))?;
        let project_evidence: Vec<&Value> = evidence_records
            .iter()
            .filter(|record| {
                record.is_object()
                    && is(record.get("projectId"), project_id)
                    && valid("evidence", record)
            })
            .collect();
        let requested_evidence_ids = unique(
            completions
                .iter()
                .flat_map(|completion| completion.evidence_ids.clone()),
        );
        let evidence_ids: Vec<String> = requested_evidence_ids
            .iter()
            .filter(|id| {
                project_evidence
                    .iter()
                    .any(|record| is(record.get("evidenceId"), id))
            })
            .cloned()
            .collect();
        issues.extend(
            requested_evidence_ids
                .iter()
                .filter(|id| !evidence_ids.contains(id))
                .map(|id| format!("Mission completion references missing Project Evidence {id}")),
        );
        let scoped_claim_ids: HashSet<String> = record_values
            .iter()
            .filter(|record| is_schema(record, "claim") && is_str(record.get("claimId")))
            .map(|record| string(record.get("claimId")))
            .collect();
        let all_submitted = self.submitted_records(project_id, None)?;
        let mut claims = Vec::new();
        for claim in latest_scoped_claims(&all_submitted, &mut issues) {
            let keep = scoped_claim_ids.contains(&string(claim.get("claimId"))) || {
                let mut references = string_values(claim.get("supportingEvidenceIds"))?;
                references.extend(string_values(claim.get("contradictingEvidenceIds"))?);
                references.extend(string_values(claim.get("qualifyingEvidenceIds"))?);
                references.iter().any(|id| evidence_ids.contains(id))
            };
            if keep {
                claims.push(claim);
            }
        }
        let mut evidence_by_id: HashMap<String, Value> = HashMap::new();
        for record in project_evidence
            .iter()
            .filter(|record| is_str(record.get("evidenceId")))
        {
            evidence_by_id.insert(string(record.get("evidenceId")), (*record).clone());
        }
        let mut unresolved_claim_ids = Vec::new();
        for claim in &claims {
            let claim_issues = claim_resolution_issues(claim, &evidence_by_id, &all_submitted)?;
            if !claim_issues.is_empty() {
                unresolved_claim_ids.push(string(claim.get("claimId")));
                issues.extend(claim_issues);
            }
        }
        let terminal_nodes: HashSet<String> = nodes
            .iter()
            .filter(|node| {
                matches!(
                    node["state"].as_str(),
                    Some("accepted" | "waived" | "superseded")
                )
            })
            .map(|node| string(node.get("id")))
            .collect();
        let blockers: Vec<String> = record_values
            .iter()
            .filter(|record| is_schema(record, "blocker"))
            .filter(|blocker| {
                !is(blocker.get("scopeType"), "graph_node")
                    || blocker
                        .get("scopeId")
                        .and_then(Value::as_str)
                        .is_none_or(|scope_id| !terminal_nodes.contains(scope_id))
            })
            .map(|blocker| string(blocker.get("blockerId")))
            .collect();
        let review_ids = unique(
            record_values
                .iter()
                .filter(|record| {
                    (is_schema(record, "review-request") || is_schema(record, "review-verdict"))
                        && is_str(record.get("reviewId"))
                })
                .map(|record| string(record.get("reviewId"))),
        );
        // `Map<reviewType:targetType:targetId, current Reviews>`, in first-insertion order.
        let mut current_reviews: Vec<(String, Vec<(Value, Value)>)> = Vec::new();
        for review in review_ids
            .iter()
            .map(|review_id| review_authority(&record_values, review_id))
        {
            let (state, Some(request), Some(verdict)) =
                (review.state, review.request, review.verdict)
            else {
                continue;
            };
            if state != "resolved" {
                continue;
            }
            let target = &request["target"];
            let key = format!(
                "{}:{}:{}",
                string(request.get("reviewType")),
                string(target.get("targetType")),
                string(target.get("targetId"))
            );
            let version = js_number(target.get("targetVersion"));
            let position = current_reviews
                .iter()
                .position(|(existing, _)| *existing == key);
            let current_version = position
                .and_then(|index| current_reviews[index].1.first())
                .map_or(-1.0, |(request, _)| {
                    js_number(request["target"].get("targetVersion"))
                });
            if version > current_version {
                match position {
                    Some(index) => current_reviews[index].1 = vec![(request, verdict)],
                    None => current_reviews.push((key, vec![(request, verdict)])),
                }
            } else if version == current_version
                && let Some(index) = position
            {
                // A first Review at version -1 lands in a fresh list the TypeScript never stores.
                current_reviews[index].1.push((request, verdict));
            }
        }
        let ambiguous_review_ids: Vec<String> = current_reviews
            .iter()
            .filter(|(_, reviews)| reviews.len() != 1)
            .map(|(key, reviews)| {
                format!(
                    "ambiguous_review:{key}@{}",
                    string(reviews[0].0["target"].get("targetVersion"))
                )
            })
            .collect();
        issues.extend(
            ambiguous_review_ids
                .iter()
                .map(|issue| format!("Mission completion has {issue}")),
        );
        // A Mission completion Review is always re-run and must PASS to close; its stale defects
        // must not block the next one from starting.
        let review_defects: Vec<String> = current_reviews
            .iter()
            .filter(|(key, reviews)| !key.starts_with("mission_completion:") && reviews.len() == 1)
            .flat_map(|(_, reviews)| {
                objects(reviews[0].1.get("defects"))
                    .filter(|defect| {
                        defect.get("blocking") == Some(&Value::Bool(true))
                            || is(defect.get("severity"), "blocking")
                            || is(defect.get("severity"), "major")
                    })
                    .map(|defect| string(defect.get("defectId")))
                    .collect::<Vec<_>>()
            })
            .collect();
        let open_defect_ids = unique(
            blockers
                .into_iter()
                .chain(ambiguous_review_ids)
                .chain(review_defects),
        );
        let validator_run_ids = unique(
            criteria
                .iter()
                .flat_map(|criterion| criterion.validator_run_ids.clone()),
        );
        if criteria.len() != value["successCriteria"].as_array().map_or(0, Vec::len) {
            issues.push("every Mission contract criterion requires an exact task, validator, and independent Review mapping".into());
        }
        if artifact_ids.is_empty() {
            issues.push("accepted task completions provide no stored deliverable artifacts".into());
        }
        if !unresolved_claim_ids.is_empty() {
            issues.push(format!(
                "unresolved claims: {}",
                unresolved_claim_ids.join(", ")
            ));
        }
        if !open_defect_ids.is_empty() {
            issues.push(format!(
                "open blocking or major defects: {}",
                open_defect_ids.join(", ")
            ));
        }
        Ok(CompletionBasis {
            completions,
            criteria,
            artifact_ids,
            evidence_ids: unique(evidence_ids.into_iter().chain(child_evidence_ids)),
            validator_run_ids,
            claims,
            unresolved_claim_ids,
            open_defect_ids,
            issues: unique(issues),
        })
    }

    /// The latest Direction or Autoresearch spawned for a scope's node, if any.
    pub fn spawned_child(
        &self,
        project_id: &str,
        scope_id: &str,
        node_id: &str,
    ) -> Result<Option<SpawnedChild>> {
        let mut matches: Vec<(SpawnedChild, String)> = Vec::new();
        for (kind, entries) in [
            ("direction", self.directions(project_id)?),
            ("autoresearch", self.autoresearch(project_id)?),
        ] {
            for entry in entries {
                let origin = &entry["value"]["origin"];
                if origin.is_object()
                    && is(origin.get("scopeId"), scope_id)
                    && is(origin.get("nodeId"), node_id)
                {
                    let child = SpawnedChild {
                        kind,
                        entity_id: string(entry.get("entityId")),
                        state: string(entry.get("state")),
                        version: entry["version"].as_i64().unwrap_or_default(),
                        attempt: js_number(origin.get("attempt")),
                    };
                    matches.push((child, string(entry["value"].get("createdAt"))));
                }
            }
        }
        // `a.attempt - b.attempt || a.createdAt.localeCompare(b.createdAt)`; ISO timestamps
        // collate in code-unit order.
        nosh_wire::js::sort_by_number(&mut matches, |a, b| {
            let by_attempt = a.0.attempt - b.0.attempt;
            if by_attempt != 0.0 && !by_attempt.is_nan() {
                return by_attempt;
            }
            match a.1.encode_utf16().cmp(b.1.encode_utf16()) {
                std::cmp::Ordering::Less => -1.0,
                std::cmp::Ordering::Equal => 0.0,
                std::cmp::Ordering::Greater => 1.0,
            }
        });
        Ok(matches.pop().map(|(child, _)| child))
    }

    /// What a Mission node builds on from its finished research children (the spawn nodes it
    /// depends on): their Evidence, each child's question and answer, and each promoted experiment
    /// with its change, frozen metric, commit, and code diff. Workers cannot read other branches,
    /// so the diff (bounded, protected state excluded) is what lets them port a winner.
    /// Returns `{ evidenceIds, summaries }`.
    pub fn research_inputs(
        &self,
        project_id: &str,
        mission_id: &str,
        node_ids: &[String],
    ) -> Result<Value> {
        let root = self.repository_root(project_id)?;
        let mut evidence_ids: Vec<String> = Vec::new();
        let mut summaries: Vec<String> = Vec::new();
        let results = self.records(project_id, Some("experiment-result"))?;
        let proposals = self.records(project_id, Some("experiment-proposal"))?;
        for node_id in node_ids {
            let Some(child) = self
                .spawned_child(project_id, mission_id, node_id)?
                .filter(|child| matches!(child.state.as_str(), "closed" | "completed"))
            else {
                continue;
            };
            evidence_ids.extend(self.child_evidence_ids(project_id, &child.entity_id)?);
            let direction = if child.kind == "direction" {
                Some(self.direction(project_id, &child.entity_id)?)
            } else {
                None
            };
            let runs: Vec<String> = match &direction {
                Some(_) => self
                    .autoresearch(project_id)?
                    .iter()
                    .filter(|run| is(run["value"].get("directionId"), &child.entity_id))
                    .map(|run| string(run.get("entityId")))
                    .collect(),
                None => vec![child.entity_id.clone()],
            };
            let closure = match &direction {
                Some(_) => self
                    .records(project_id, Some("direction-closure-packet"))?
                    .into_iter()
                    .rfind(|record| is(record.get("directionId"), &child.entity_id)),
                None => None,
            };
            let question = match &direction {
                Some(direction) => direction["value"]["question"].clone(),
                None => self.autoresearch_execution(project_id, &child.entity_id)?["value"]["decisionQuestion"].clone(),
            };
            // `[...new Map(promoted.map((result) => [result.experimentId, result])).values()]`.
            let mut promoted: Vec<(String, &Value)> = Vec::new();
            for result in results.iter().filter(|result| {
                runs.contains(&string(result.get("autoresearchId")))
                    && is(result.get("promotionDecision"), "promoted")
            }) {
                let key = canonical_json(&or_null(result.get("experimentId")))
                    + if result.get("experimentId").is_none() {
                        "u"
                    } else {
                        ""
                    };
                match promoted.iter_mut().find(|(existing, _)| *existing == key) {
                    Some(entry) => entry.1 = result,
                    None => promoted.push((key, result)),
                }
            }
            let mut experiments = Vec::new();
            for (_, result) in promoted {
                let execution = if runs.len() == 1 && direction.is_none() {
                    child.entity_id.clone()
                } else {
                    string(result.get("autoresearchId"))
                };
                let base =
                    self.autoresearch_execution(project_id, &execution)?["value"]["baseCommit"]
                        .clone();
                let evaluated = result.get("evaluatedCommit");
                let diff = if nosh_wire::js::truthy(Some(&base)) && nosh_wire::js::truthy(evaluated)
                {
                    git_diff(&root, &string(Some(&base)), &string(evaluated))
                } else {
                    String::new()
                };
                let proposal = proposals.iter().find(|proposal| {
                    same(proposal.get("experimentId"), result.get("experimentId"))
                });
                let description = proposal
                    .and_then(|proposal| proposal.get("primaryChange"))
                    .and_then(|change| change.get("description"));
                let score = result
                    .get("comparison")
                    .and_then(|comparison| comparison.get("candidateScore"));
                let code = if diff.is_empty() {
                    String::new()
                } else {
                    format!("\nCode change:\n{}", slice_to(&diff, 4_000))
                };
                experiments.push(format!(
                    "{} ({}; frozen metric {}; commit {} on branch {}){code}",
                    string(result.get("experimentId")),
                    clip(description, 300),
                    string(score),
                    string(evaluated),
                    string(result.get("branch"))
                ));
            }
            let answer = closure.map_or(String::new(), |closure| {
                format!(
                    " Answer ({}): {}",
                    string(closure.get("questionDisposition")),
                    clip(closure.get("answer"), 600)
                )
            });
            let promoted_text = if experiments.is_empty() {
                "none".to_owned()
            } else {
                experiments.join("\n")
            };
            summaries.push(format!(
                "{} ({}, node {node_id}) asked: {}{answer} Promoted experiments: {promoted_text}",
                child.entity_id,
                child.kind,
                clip(Some(&question), 300)
            ));
        }
        Ok(json!({ "evidenceIds": unique(evidence_ids), "summaries": summaries }))
    }

    /// Evidence recorded inside a research child's scope (a Direction includes its Autoresearch).
    pub fn child_evidence_ids(&self, project_id: &str, child_id: &str) -> Result<Vec<String>> {
        Ok(unique(
            self.submitted(project_id, None)?
                .iter()
                .filter(|event| {
                    is_schema(&event["payload"], "evidence")
                        && (is(event["scope"].get("directionId"), child_id)
                            || is(event["scope"].get("autoresearchId"), child_id))
                        && is_str(event["payload"].get("evidenceId"))
                })
                .map(|event| string(event["payload"].get("evidenceId"))),
        ))
    }

    /// Model-token use inside a Direction, including its Autoresearch, under the daemon's budget
    /// formula: `{ modelTokens }`.
    pub fn direction_budget_use(&self, project_id: &str, direction_id: &str) -> Result<Value> {
        let events: Vec<Value> = self
            .store(project_id)?
            .replay(project_id, 0)?
            .into_iter()
            .filter(|event| is(event["scope"].get("directionId"), direction_id))
            .collect();
        number_json(json!({ "modelTokens": event_resource_use(&events).0 }))
    }

    /// `{ modelTokens, gpuSeconds, wallClockSeconds, diskBytes, experiments, rounds }`.
    pub fn autoresearch_budget_use(
        &self,
        project_id: &str,
        autoresearch_id: &str,
    ) -> Result<Value> {
        let execution = self.autoresearch_execution(project_id, autoresearch_id)?;
        let project_events = self.store(project_id)?.replay(project_id, 0)?;
        let events: Vec<Value> = project_events
            .iter()
            .filter(|event| is(event["scope"].get("autoresearchId"), autoresearch_id))
            .cloned()
            .collect();
        let records: Vec<Value> = self
            .submitted(project_id, None)?
            .into_iter()
            .filter(|event| is(event["scope"].get("autoresearchId"), autoresearch_id))
            .map(|mut event| event["payload"].take())
            .collect();
        let experiment_ids = unique(
            records
                .iter()
                .filter(|record| {
                    is_schema(record, "experiment-proposal")
                        && nosh_wire::js::truthy(record.get("experimentId"))
                })
                .map(|record| string(record.get("experimentId"))),
        );
        let root = self.repository_root(project_id)?;
        let nosh = root.join(".nosh");
        let mut disk_bytes = records
            .iter()
            .filter(|record| is_schema(record, "artifact"))
            .fold(0.0, |sum, record| {
                sum + max(
                    0.0,
                    js_number(Some(
                        record
                            .get("sizeBytes")
                            .filter(|size| !size.is_null())
                            .unwrap_or(&json!(0)),
                    )),
                )
            });
        disk_bytes += directory_bytes(&nosh.join("autoresearch").join(autoresearch_id))?;
        for id in &experiment_ids {
            disk_bytes += directory_bytes(
                &nosh
                    .join("worktrees")
                    .join(format!("wt_{}", utf16_from(id, 4))),
            )?;
        }
        disk_bytes += directory_bytes(
            &nosh
                .join("worktrees")
                .join(format!("integration_{}", utf16_from(autoresearch_id, 3))),
        )?;
        let (model_tokens, gpu_seconds) = event_resource_use(&events);
        // Evaluated experiments; implementation crashes are counted apart, so they cannot starve the climb.
        let experiments = unique(
            records
                .iter()
                .filter(|record| {
                    is_schema(record, "experiment-result")
                        && nosh_wire::js::truthy(record.get("experimentId"))
                })
                .map(|record| canonical_json(&record["experimentId"])),
        )
        .len();
        let implementation_failures = events
            .iter()
            .filter(|event| {
                event["type"] == "autoresearch.experiment_failed"
                    && is(event["payload"].get("phase"), "implementation")
            })
            .count();
        number_json(
            json!({ "modelTokens": model_tokens, "gpuSeconds": gpu_seconds, "wallClockSeconds": running_seconds(&events, "autoresearch", &downtime_intervals(&project_events), entities::now_ms()), "diskBytes": disk_bytes, "experiments": experiments, "implementationFailures": implementation_failures, "rounds": execution["value"]["currentRound"] }),
        )
    }

    /// `{ modelTokens, gpuSeconds, wallClockSeconds, diskBytes }`. Budgeted wall time counts only
    /// intervals spent running, as for Autoresearch.
    pub fn mission_budget_use(&self, project_id: &str, mission_id: &str) -> Result<Value> {
        let mission = self.mission(project_id, mission_id)?;
        let events = self.scoped_events(project_id, mission_id, None)?;
        let running: Vec<Value> = events
            .iter()
            .filter(|event| {
                same(event["payload"].get("entityId"), mission.get("entityId"))
                    || !event["type"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("mission.")
            })
            .cloned()
            .collect();
        let (model_tokens, gpu_seconds) = event_resource_use(&events);
        let downtime = downtime_intervals(&self.store(project_id)?.replay(project_id, 0)?);
        number_json(
            json!({ "modelTokens": model_tokens, "gpuSeconds": gpu_seconds, "wallClockSeconds": running_seconds(&running, "mission", &downtime, entities::now_ms()), "diskBytes": directory_bytes(&self.repository_root(project_id)?.join(".nosh"))? }),
        )
    }

    pub fn mission_completion_issues(
        &self,
        project_id: &str,
        mission_id: &str,
    ) -> Result<Vec<String>> {
        let mission = self.mission(project_id, mission_id)?;
        let value = &mission["value"];
        let mut issues = Vec::new();
        for node in value["nodes"].as_array().into_iter().flatten() {
            if nosh_wire::js::truthy(node.get("required"))
                && !matches!(
                    node["state"].as_str(),
                    Some("accepted" | "waived" | "superseded")
                )
            {
                issues.push(format!(
                    "required node {} is {}",
                    string(node.get("id")),
                    string(node.get("state"))
                ));
            }
        }
        let used = self.mission_budget_use(project_id, mission_id)?;
        let budgets = &value["budgets"];
        for (key, maximum, message) in [
            (
                "modelTokens",
                "maximumModelTokens",
                "Mission model-token budget is exceeded",
            ),
            (
                "wallClockSeconds",
                "maximumWallClockSeconds",
                "Mission wall-clock budget is exceeded",
            ),
            (
                "gpuSeconds",
                "maximumGpuSeconds",
                "Mission GPU budget is exceeded",
            ),
            (
                "diskBytes",
                "maximumDiskBytes",
                "Mission disk budget is exceeded",
            ),
        ] {
            if js_number(used.get(key)) > js_number(budgets.get(maximum)) {
                issues.push(message.to_owned());
            }
        }
        let evidence_ids: HashSet<String> = self
            .records(project_id, Some("evidence"))?
            .iter()
            .filter(|record| nosh_wire::js::truthy(record.get("evidenceId")))
            .map(|record| string(record.get("evidenceId")))
            .collect();
        for claim in self.records(project_id, Some("claim"))? {
            let claim_id = claim
                .get("claimId")
                .filter(|id| !id.is_null())
                .map_or_else(|| "unknown".to_owned(), |id| string(Some(id)));
            let list = |key: &str| {
                claim
                    .get(key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            };
            let references: Vec<Value> = [
                "supportingEvidenceIds",
                "contradictingEvidenceIds",
                "qualifyingEvidenceIds",
            ]
            .iter()
            .flat_map(|key| list(key))
            .collect();
            if references.is_empty() && list("limitations").is_empty() {
                issues.push(format!("claim {claim_id} is unsupported and unqualified"));
            }
            for id in references {
                if !id.as_str().is_some_and(|id| evidence_ids.contains(id)) {
                    issues.push(format!(
                        "claim {claim_id} references missing evidence {}",
                        string(Some(&id))
                    ));
                }
            }
        }
        let artifacts = ArtifactStore::open(
            &self
                .repository_root(project_id)?
                .join(".nosh")
                .join("artifacts"),
        )?;
        for record in self.records(project_id, Some("artifact"))? {
            if let Some(id) = record
                .get("artifactId")
                .filter(|id| nosh_wire::js::truthy(Some(id)))
            {
                let id = string(Some(id));
                if artifacts
                    .resolve(&id, record.get("version").and_then(Value::as_i64))
                    .is_err()
                {
                    issues.push(format!("artifact {id} does not resolve to its stored hash"));
                }
            }
        }
        Ok(unique(issues))
    }
}

/// Integral floats as integers, as JavaScript prints them.
fn number_json(mut value: Value) -> Result<Value> {
    normalize_numbers(&mut value);
    Ok(value)
}

/// `text.slice(start)` by UTF-16 code units.
fn utf16_from(text: &str, start: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().skip(start).collect::<Vec<_>>())
}

/// `String(value ?? "")` with whitespace runs collapsed, trimmed, and cut to `limit` units with
/// an ellipsis.
fn clip(value: Option<&Value>, limit: usize) -> String {
    let text = value
        .filter(|value| !value.is_null())
        .map_or_else(String::new, |value| string(Some(value)));
    // JavaScript's `\s`: Rust's would also match U+0085 and miss U+FEFF.
    let collapsed = crate::regex!(r"[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]+").replace_all(&text, " ");
    let text = crate::episodes::js_trim(&collapsed);
    if nosh_wire::js::length(text) > limit {
        format!("{}…", slice_to(text, limit - 1))
    } else {
        text.to_owned()
    }
}

fn git_diff(root: &Path, base: &str, commit: &str) -> String {
    nosh_sys::process::hidden("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            base,
            commit,
            "--",
            ".",
            ":(exclude).nosh",
            ":(exclude)docs",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_or_else(
            |_| String::new(),
            |output| String::from_utf8_lossy(&output.stdout).into_owned(),
        )
}

fn artifact_exists(root: &Path, artifact_id: &str) -> bool {
    ArtifactStore::open(root)
        .and_then(|store| store.resolve(artifact_id, None))
        .is_ok()
}

pub(super) fn current_mission_completion_packet(
    records: &[Value],
    mission_id: &str,
    target_version: i64,
    graph_version: &Value,
) -> Option<Value> {
    let requests: Vec<&Value> = records
        .iter()
        .filter(|record| {
            is_schema(record, "review-request")
                && is(record.get("reviewType"), "mission_completion")
                && record["target"].is_object()
                && is(record["target"].get("targetType"), "mission_record")
                && is(record["target"].get("targetId"), mission_id)
                && number(record["target"].get("targetVersion")) == Some(target_version as f64)
        })
        .collect();
    let [request] = requests.as_slice() else {
        return None;
    };
    let review = exact_review_join(records, request.get("reviewId")?.as_str()?)?;
    if !is(review.verdict.get("verdict"), "PASS") || !review.request["contractRefs"].is_array() {
        return None;
    }
    let hashes: Vec<&str> = objects(review.request.get("contractRefs"))
        .filter(|reference| is(reference.get("kind"), "contract_mission.completion"))
        .filter_map(|reference| reference.get("hash").and_then(Value::as_str))
        .collect();
    let [hash] = hashes.as_slice() else {
        return None;
    };
    let packets: Vec<&Value> = records
        .iter()
        .filter(|record| {
            is_schema(record, "mission-completion-packet")
                && is(record.get("missionId"), mission_id)
                && number(record.get("objectiveVersion")) == Some(1.0)
                && same(record.get("graphVersion"), Some(graph_version))
                && sha256(record) == *hash
        })
        .collect();
    match packets.as_slice() {
        [packet] => Some((*packet).clone()),
        _ => None,
    }
}

pub(super) fn same_mission_criterion_mappings(
    value: Option<&Value>,
    expected: &[BasisCriterion],
) -> bool {
    let Some(items) = value
        .and_then(Value::as_array)
        .filter(|items| items.len() == expected.len())
    else {
        return false;
    };
    expected.iter().all(|criterion| {
        items
            .iter()
            .find(|entry| {
                entry.is_object() && is(entry.get("criterionId"), &criterion.criterion_id)
            })
            .is_some_and(|actual| {
                is(actual.get("taskId"), &criterion.task_id)
                    && is(actual.get("reviewId"), &criterion.review_id)
                    && same_string_set(actual.get("validatorRunIds"), &criterion.validator_run_ids)
                    && same_string_set(actual.get("artifactIds"), &criterion.artifact_ids)
                    && same_string_set(actual.get("evidenceIds"), &criterion.evidence_ids)
            })
    })
}

/// The Artifacts and Evidence a task completion cites: `(artifactIds, evidenceIds)`.
fn completion_references(record: &Value) -> (Vec<String>, Vec<String>) {
    let lists = |key: &str, field: &str| {
        objects(record.get(key))
            .flat_map(|entry| {
                entry
                    .get(field)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
    };
    let code = record.get("codeChanges").filter(|code| code.is_object());
    let mut artifact_ids: Vec<Value> = [
        code.and_then(|code| code.get("diffArtifactId")),
        record.get("bibliographyArtifactId"),
        record.get("reportArtifactId"),
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect();
    artifact_ids.extend(lists("workPerformed", "artifactIds"));
    artifact_ids.extend(lists("criteria", "artifactIds"));
    artifact_ids.extend(
        objects(record.get("sources")).filter_map(|source| source.get("artifactId").cloned()),
    );
    artifact_ids.extend(objects(record.get("findings")).flat_map(|finding| {
        objects(finding.get("support"))
            .filter_map(|support| support.get("artifactId").cloned())
            .collect::<Vec<_>>()
    }));
    let impact = record
        .get("scientificImpact")
        .filter(|impact| impact.is_object());
    let evidence_ids = impact
        .and_then(|impact| impact.get("evidenceIds"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let strings = |items: Vec<Value>| {
        unique(
            items
                .into_iter()
                .filter_map(|item| item.as_str().map(str::to_owned)),
        )
    };
    (strings(artifact_ids), strings(evidence_ids))
}

fn claim_resolution_issues(
    claim: &Value,
    evidence: &HashMap<String, Value>,
    records: &[Value],
) -> Result<Vec<String>> {
    let claim_id = string(claim.get("claimId"));
    let supporting = string_values(claim.get("supportingEvidenceIds"))?;
    let contradicting = string_values(claim.get("contradictingEvidenceIds"))?;
    let qualifying = string_values(claim.get("qualifyingEvidenceIds"))?;
    let mut issues: Vec<String> = supporting
        .iter()
        .chain(&contradicting)
        .chain(&qualifying)
        .filter(|id| !evidence.contains_key(*id))
        .map(|id| format!("Claim {claim_id} references missing Project Evidence {id}"))
        .collect();
    let reviewed = |ids: &[String]| {
        ids.iter().any(|evidence_id| {
            let Some(review_id) = evidence
                .get(evidence_id)
                .and_then(|source| source.get("quality"))
                .filter(|quality| quality.is_object())
                .and_then(|quality| quality.get("reviewId"))
                .and_then(Value::as_str)
            else {
                return false;
            };
            exact_review_join(records, review_id).is_some_and(|review| {
                is(review.verdict.get("verdict"), "PASS")
                    && includes(review.request.get("requiredEvidenceIds"), evidence_id)
                    && includes(review.verdict.get("reviewedEvidenceIds"), evidence_id)
            })
        })
    };
    let limitations = string_values(claim.get("limitations"))?;
    let status = claim["status"].as_str().unwrap_or_default();
    match status {
        "hypothesis" | "under_test" => issues.push(format!("Claim {claim_id} is {status}")),
        "supported" | "partially_supported" if !reviewed(&supporting) => issues.push(format!("Claim {claim_id} lacks independently PASS-reviewed supporting Evidence")),
        "contradicted" | "not_supported" if !reviewed(&contradicting) => issues.push(format!("Claim {claim_id} lacks independently PASS-reviewed contradicting Evidence")),
        "qualified" if !reviewed(&qualifying) && limitations.is_empty() => issues.push(format!("Claim {claim_id} lacks independently PASS-reviewed qualifying Evidence or explicit limitations")),
        "limited" if limitations.is_empty() => issues.push(format!("Claim {claim_id} is limited without explicit limitations")),
        _ => {}
    }
    Ok(issues)
}

/// The latest valid version of each Claim; invalid and conflicting versions become issues.
fn latest_scoped_claims(records: &[Value], issues: &mut Vec<String>) -> Vec<Value> {
    let mut claims: Vec<(String, f64, String, Value)> = Vec::new();
    for record in records.iter().filter(|record| is_schema(record, "claim")) {
        let claim_id = record.get("claimId").and_then(Value::as_str);
        let (Some(claim_id), true, true) = (
            claim_id,
            valid("claim", record),
            is_integer(record.get("claimVersion")),
        ) else {
            issues.push(format!(
                "invalid Claim record {}",
                claim_id.unwrap_or("unknown")
            ));
            continue;
        };
        let version = number(record.get("claimVersion")).expect("checked");
        let hash = sha256(record);
        match claims.iter_mut().find(|entry| entry.0 == claim_id) {
            Some(current) if current.1 == version && current.2 != hash => issues.push(format!(
                "ambiguous Claim version {claim_id}@{}",
                string(record.get("claimVersion"))
            )),
            Some(current) if version > current.1 => {
                *current = (claim_id.to_owned(), version, hash, record.clone())
            }
            Some(_) => {}
            None => claims.push((claim_id.to_owned(), version, hash, record.clone())),
        }
    }
    claims.into_iter().map(|claim| claim.3).collect()
}

/// `(modelTokens, gpuSeconds)` of a set of events.
fn event_resource_use(events: &[Value]) -> (f64, f64) {
    let model_tokens = events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("agent.completed" | "agent.retrying" | "agent.failed")
            )
        })
        .fold(0.0, |sum, event| {
            sum + event_budget_tokens(&event["payload"])
        });
    let mut jobs: Vec<(String, &Value)> = Vec::new();
    for event in events
        .iter()
        .filter(|event| event["type"] == "job.state_changed")
    {
        let Some(job_id) = event["scope"]
            .get("jobId")
            .filter(|id| nosh_wire::js::truthy(Some(id)))
            .map(|id| string(Some(id)))
        else {
            continue;
        };
        match jobs.iter_mut().find(|(id, _)| *id == job_id) {
            Some(entry) => entry.1 = &event["payload"],
            None => jobs.push((job_id, &event["payload"])),
        }
    }
    let now = now_utc();
    let gpu_seconds = jobs
        .iter()
        .filter(|(_, job)| {
            nosh_wire::js::truthy(job.get("usesGpu")) && nosh_wire::js::truthy(job.get("startedAt"))
        })
        .fold(0.0, |sum, (_, job)| {
            let finished = job
                .get("finishedAt")
                .and_then(Value::as_str)
                .unwrap_or(&now);
            let started = job
                .get("startedAt")
                .and_then(Value::as_str)
                .map_or(f64::NAN, parse_ms);
            sum + max(0.0, ((parse_ms(finished) - started) / 1000.0).floor())
        });
    (model_tokens, gpu_seconds)
}

/// Budgeted wall time counts only intervals spent running; paused or blocked waiting is not
/// research time, and neither is time the daemon was down.
pub fn running_seconds(events: &[Value], scope: &str, downtime: &[(f64, f64)], now: f64) -> f64 {
    let stops: Vec<String> = ["paused", "blocked", "stopped", "completed", "failed"]
        .iter()
        .map(|state| format!("{scope}.{state}"))
        .collect();
    let running = format!("{scope}.running");
    let mut intervals: Vec<(f64, f64)> = Vec::new();
    let mut since: Option<f64> = None;
    for event in events {
        let kind = event["type"].as_str().unwrap_or_default();
        let at = || event["timestamp"].as_str().map_or(f64::NAN, parse_ms);
        if kind == running {
            since.get_or_insert_with(at);
        } else if let Some(start) = since
            && stops.iter().any(|stop| stop == kind)
        {
            intervals.push((start, at()));
            since = None;
        }
    }
    if let Some(start) = since {
        intervals.push((start, now));
    }
    let total = intervals.iter().fold(0.0, |sum, (start, end)| {
        sum + (end - start)
            - downtime.iter().fold(0.0, |overlap, (down, up)| {
                overlap + max(0.0, nosh_wire::js::min(*end, *up) - max(*start, *down))
            })
    });
    max(0.0, (total / 1000.0).floor())
}

/// Daemon downtime per project: each start writes `recovery.report`; the gap before it runs from
/// the `daemon.shutdown` written on a clean stop, or after a crash from the last event the daemon
/// recorded.
pub fn downtime_intervals(events: &[Value]) -> Vec<(f64, f64)> {
    let mut intervals = Vec::new();
    let mut last: Option<&Value> = None;
    for event in events {
        if event["type"] == "recovery.report"
            && let Some(last) = last
        {
            let down = last["timestamp"].as_str().map_or(f64::NAN, parse_ms);
            let up = event["timestamp"].as_str().map_or(f64::NAN, parse_ms);
            if up > down {
                intervals.push((down, up));
            }
        }
        last = Some(event);
    }
    intervals
}

/// Bytes of the regular files under `path`, not following links (junctions included).
fn directory_bytes(path: &Path) -> Result<f64> {
    if !path.exists() {
        return Ok(0.0);
    }
    let mut bytes = 0.0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            bytes += directory_bytes(&entry.path())?;
        } else if kind.is_file() {
            bytes += entry.metadata()?.len() as f64;
        }
    }
    Ok(bytes)
}
