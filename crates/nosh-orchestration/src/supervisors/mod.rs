//! The daemon's research supervisors (`apps/noshd/src/*-supervisor.ts`). Each ticks once a second
//! and advances every running Mission, Direction, or Autoresearch of every registered Project by
//! one deterministic step: directors decide semantics, the daemon executes.
//!
//! A supervisor's cycle runs as an un-awaited task (`void this.cycle()` in TypeScript); a busy set
//! keeps one cycle per scope, and each cycle records its own failures as events.

pub mod direction;
pub mod mission;

use crate::research_spawner::JobRunner;
use crate::runtime::{OrchestrationRuntime, RuntimeHost, now_ms};
use crate::task_postflight::validate_git_completion;
use crate::task_worktree::task_worktree_path;
use crate::{Agents, Error, Result};
use futures_util::future::LocalBoxFuture;
use nosh_research::artifacts::ArtifactStore;
use nosh_research::research_control::{Host, ResearchControl};
use nosh_sys::git::{DAEMON_IDENTITY, worktree_git_environment};
use nosh_wire::js::{strict_equal, string, truthy};
use nosh_wire::{iso_from_ms, schema_uri, validate_record};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;

/// The runtime as supervisors drive it: given one, task sessions run as runtime threads.
pub trait SupervisorRuntime {
    fn execute(&self, instruction: &Value) -> LocalBoxFuture<'static, Result<Value>>;
    fn has_active_fork(
        &self,
        project_id: &str,
        mission_id: Option<&str>,
        direction_id: Option<&str>,
    ) -> Result<bool>;
}

impl<H: RuntimeHost + 'static> SupervisorRuntime for Rc<OrchestrationRuntime<H>> {
    fn execute(&self, instruction: &Value) -> LocalBoxFuture<'static, Result<Value>> {
        Box::pin(OrchestrationRuntime::execute(self, instruction))
    }
    fn has_active_fork(
        &self,
        project_id: &str,
        mission_id: Option<&str>,
        direction_id: Option<&str>,
    ) -> Result<bool> {
        OrchestrationRuntime::has_active_fork(self, project_id, mission_id, direction_id)
    }
}

/// Appends a persistent event draft.
pub type Emit = Rc<dyn Fn(&Value) -> Result<()>>;
/// The registered Projects.
pub type Projects = Rc<dyn Fn() -> Result<Vec<Value>>>;

/// What every supervisor is built from (the TypeScript constructor arguments).
pub struct Dependencies<H: Host> {
    pub research: Rc<ResearchControl<H>>,
    pub agents: Rc<dyn Agents>,
    pub projects: Projects,
    pub package_path: PathBuf,
    /// The shipped record JSON Schemas prompts point at.
    pub schema_dir: PathBuf,
    pub emit: Emit,
    pub runtime: Option<Rc<dyn SupervisorRuntime>>,
    pub job_runner: Option<Rc<dyn JobRunner>>,
}

impl<H: Host> Clone for Dependencies<H> {
    fn clone(&self) -> Self {
        Dependencies {
            research: Rc::clone(&self.research),
            agents: Rc::clone(&self.agents),
            projects: Rc::clone(&self.projects),
            package_path: self.package_path.clone(),
            schema_dir: self.schema_dir.clone(),
            emit: Rc::clone(&self.emit),
            runtime: self.runtime.clone(),
            job_runner: self.job_runner.clone(),
        }
    }
}

/// Holds a scope in a supervisor's busy set until dropped (the TypeScript `finally`).
pub(crate) struct Busy<'a> {
    set: &'a RefCell<HashSet<String>>,
    id: String,
}

impl<'a> Busy<'a> {
    pub(crate) fn hold(set: &'a RefCell<HashSet<String>>, id: &str) -> Self {
        set.borrow_mut().insert(id.to_owned());
        Busy {
            set,
            id: id.to_owned(),
        }
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.set.borrow_mut().remove(&self.id);
    }
}

/// `git -C <cwd> <args>`'s trimmed stdout; a failure carries trimmed stderr.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    Ok(nosh_sys::git::git(cwd, args, &[])?)
}

/// `git -C <cwd> <args>` as NOSH's own identity (its merges never depend on a user Git identity):
/// the exit status (`None` when Git could not start) and untrimmed stdout and stderr.
pub(crate) fn daemon_git(cwd: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = nosh_sys::process::hidden("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .envs(worktree_git_environment(cwd))
        .envs(DAEMON_IDENTITY.iter().copied())
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) => (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(_) => (None, String::new(), String::new()),
    }
}

/// `[...new Set(items)]`.
pub(crate) fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

pub(crate) fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| string(Some(item)))
        .collect()
}

/// A JSON number as JavaScript reads it; anything else is NaN.
pub(crate) fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

/// A stored projection's version.
pub(crate) fn version(projection: &Value) -> i64 {
    projection["version"].as_i64().unwrap_or(-1)
}

/// The Artifact and Evidence IDs a completion record cites, each listed once.
pub(crate) fn completion_refs(record: &Value) -> (Vec<String>, Vec<String>) {
    let each = |key: &str| record[key].as_array().into_iter().flatten();
    let artifact_ids = [
        &record["codeChanges"]["diffArtifactId"],
        &record["bibliographyArtifactId"],
        &record["reportArtifactId"],
    ]
    .into_iter()
    .chain(
        each("workPerformed")
            .flat_map(|entry| entry["artifactIds"].as_array().into_iter().flatten()),
    )
    .chain(each("criteria").flat_map(|entry| entry["artifactIds"].as_array().into_iter().flatten()))
    .chain(each("sources").map(|entry| &entry["artifactId"]))
    .chain(each("findings").flat_map(|entry| {
        entry["support"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|support| &support["artifactId"])
    }))
    .filter_map(Value::as_str)
    .filter(|id| !id.is_empty())
    .map(str::to_owned);
    (
        unique(artifact_ids),
        unique(strings(&record["scientificImpact"]["evidenceIds"])),
    )
}

/// Whether a schema-valid verdict passes exactly this Review Request: its identities, an
/// independent PASS, every requested criterion PASS, and every required input reviewed.
pub(crate) fn review_passes_request(review: &Value, request: &Value, reviewer_id: &str) -> bool {
    let same = |pointer: &str| strict_equal(review.pointer(pointer), request.pointer(pointer));
    if validate_record(&schema_uri("review-verdict"), review).is_err()
        || !same("/reviewId")
        || !same("/reviewRequestId")
        || !same("/reviewType")
        || review["reviewerAgentId"] != reviewer_id
        || review["independenceCheck"] != "pass"
        || review["verdict"] != "PASS"
        || !same("/target/targetType")
        || !same("/target/targetId")
        || !same("/target/targetVersion")
    {
        return false;
    }
    let passed = |required: &Value| {
        review["criteria"].as_array().is_some_and(|criteria| {
            criteria.iter().any(|criterion| {
                strict_equal(criterion.get("criterionId"), required.get("criterionId"))
                    && criterion["status"] == "PASS"
            })
        })
    };
    let reviewed = |required: &str, seen: &str| {
        request[required]
            .as_array()
            .into_iter()
            .flatten()
            .all(|id| review[seen].as_array().is_some_and(|ids| ids.contains(id)))
    };
    request["criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .all(passed)
        && reviewed("requiredArtifactIds", "reviewedArtifactIds")
        && reviewed("requiredEvidenceIds", "reviewedEvidenceIds")
}

/// A scope projection's graph node.
pub(crate) fn node<'a>(scope: &'a Value, node_id: &str) -> Option<&'a Value> {
    scope["value"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|node| node["id"] == node_id)
}

pub(crate) fn nodes(scope: &Value) -> Vec<Value> {
    scope["value"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn repository_root(project: &Value) -> PathBuf {
    PathBuf::from(string(project.get("repositoryRoot")))
}

/// `new Date(Date.now() + ms).toISOString()`.
pub(crate) fn iso_in(ms: f64) -> Result<String> {
    iso_from_ms(now_ms() + ms).ok_or_else(|| Error::Rejected("Invalid time value".into()))
}

pub(crate) fn node_state<'a>(scope: &'a Value, node_id: &str) -> &'a str {
    node(scope, node_id)
        .and_then(|node| node["state"].as_str())
        .unwrap_or_default()
}

pub(crate) fn from_packet<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|error| Error::Rejected(error.to_string()))
}

/// Deterministic postflight of a worker's completion against its Task Packet: the record, its
/// Git change, declared commands, criteria validators, and every cited Artifact.
pub(crate) fn postflight(
    project: &Value,
    packet: &Value,
    schema: &str,
    record: &Value,
) -> Result<Vec<String>> {
    let value = match validate_record(&schema_uri(schema), record) {
        Ok(value) => value,
        Err(issues) => {
            return Ok(issues
                .iter()
                .map(|issue| format!("{} {}", issue.pointer, issue.message))
                .collect());
        }
    };
    let root = repository_root(project);
    let artifacts = ArtifactStore::open(&root.join(".nosh").join("artifacts"))?;
    let mut issues: Vec<String> = Vec::new();
    if value["taskOutcome"] != "completed" || value["readyForReview"] != true {
        issues.push("completion is not ready for Review".into());
    }
    let ids: Vec<String> = if schema == "general-worker-completion" {
        if value["readyForDeterministicPostflight"] != true {
            issues.push("worker did not request deterministic postflight".into());
        }
        let code = &value["codeChanges"];
        issues.extend(validate_git_completion(
            &task_worktree_path(&root, &string(packet["workspace"].get("worktreeId"))),
            &packet["workspace"],
            code,
        ));
        let each = |key: &str| value[key].as_array().cloned().unwrap_or_default();
        if each("commands")
            .iter()
            .any(|command| !strict_equal(command.get("exitCode"), Some(&json!(0))))
        {
            issues.push("a declared command did not pass".into());
        }
        let criteria = each("criteria");
        for criterion in packet["acceptanceCriteria"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| truthy(entry.get("required")))
        {
            let result = criteria
                .iter()
                .find(|entry| strict_equal(entry.get("criterionId"), criterion.get("criterionId")));
            // deferred_to_review: only the independent task Review can establish it, and completion
            // counts it only on that Review's criterion PASS.
            if !result.is_some_and(|result| {
                result["workerClaim"] == "deferred_to_review"
                    || (result["workerClaim"] == "satisfied"
                        && result["validatorRunIds"]
                            .as_array()
                            .is_some_and(|ids| !ids.is_empty()))
            }) {
                issues.push(format!(
                    "criterion {} lacks a passing validator",
                    string(criterion.get("criterionId"))
                ));
            }
        }
        std::iter::once(&code["diffArtifactId"])
            .chain(
                each("workPerformed")
                    .iter()
                    .flat_map(|work| work["artifactIds"].as_array().cloned().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .iter(),
            )
            .chain(
                criteria
                    .iter()
                    .flat_map(|criterion| {
                        criterion["artifactIds"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .iter(),
            )
            .filter(|id| truthy(Some(id)))
            .map(|id| string(Some(id)))
            .collect()
    } else {
        let each = |key: &str| value[key].as_array().cloned().unwrap_or_default();
        [
            value.get("bibliographyArtifactId"),
            value.get("reportArtifactId"),
        ]
        .into_iter()
        .map(string)
        .chain(
            each("sources")
                .iter()
                .map(|source| string(source.get("artifactId"))),
        )
        .chain(each("findings").iter().flat_map(|finding| {
            finding["support"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|support| string(support.get("artifactId")))
                .collect::<Vec<_>>()
        }))
        .collect()
    };
    for id in ids {
        if artifacts.resolve(&id, None).is_err() {
            issues.push(format!("artifact {id} does not resolve"));
        }
    }
    Ok(unique(issues))
}

#[cfg(test)]
mod tests {
    use super::review_passes_request;
    use nosh_wire::{IdPrefix, create_id, now_utc, random_hex, schema_uri, validate_record};
    use serde_json::{Value, json};

    /// A Review Request and the schema-valid verdict that passes it.
    fn passing() -> (Value, Value, String) {
        let review_id = create_id(IdPrefix::Rev);
        let reviewer = create_id(IdPrefix::Agt);
        let node = format!("mnode_{}", random_hex());
        let request = json!({ "reviewId": review_id, "reviewRequestId": format!("request_{}", &review_id[4..]), "reviewType": "task", "target": { "targetType": "graph_node", "targetId": node, "targetVersion": 3 }, "criteria": [{ "criterionId": node, "statement": "Done", "required": true, "severityIfFailed": "blocking" }], "requiredArtifactIds": [create_id(IdPrefix::Art)], "requiredEvidenceIds": [create_id(IdPrefix::Evd)] });
        let review = json!({
            "$schema": schema_uri("review-verdict"), "schemaVersion": 1, "templateVersion": "1.0.0", "reviewId": review_id, "reviewRequestId": request["reviewRequestId"], "reviewType": "task", "target": request["target"],
            "reviewerAgentId": reviewer, "independenceCheck": "pass", "verdict": "PASS", "summary": "Passed",
            "criteria": [{ "criterionId": node, "required": true, "status": "PASS", "finding": "Met", "evidenceRefs": [], "confidence": "high", "defectIds": [] }],
            "defects": [], "missingRequiredInputs": [], "scientificIntegrityFlags": [], "recommendedGraphAction": "accept_node", "recommendedPromotion": "not_applicable",
            "reviewedArtifactIds": request["requiredArtifactIds"], "reviewedEvidenceIds": request["requiredEvidenceIds"], "submittedAt": now_utc(),
        });
        (request, review, reviewer)
    }

    #[test]
    fn passes_only_a_verdict_that_answers_every_part_of_its_request() {
        let (request, review, reviewer) = passing();
        assert!(review_passes_request(&review, &request, &reviewer));
        assert!(!review_passes_request(&review, &request, "agt_other"));
        for (pointer, value) in [
            ("/reviewedArtifactIds", json!([])),
            ("/reviewedEvidenceIds", json!([])),
            // A PASS may mark a required criterion not applicable; the request still needs it PASS.
            ("/criteria/0/status", json!("NOT_APPLICABLE")),
            ("/target/targetVersion", json!(2)),
            ("/verdict", json!("BLOCKED")),
        ] {
            let mut changed = review.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            // Still a valid record: only the request comparison may reject it.
            assert!(
                validate_record(&schema_uri("review-verdict"), &changed).is_ok(),
                "{pointer}"
            );
            assert!(
                !review_passes_request(&changed, &request, &reviewer),
                "{pointer}"
            );
        }
    }
}
