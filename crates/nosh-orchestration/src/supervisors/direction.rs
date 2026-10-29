//! Port of `apps/noshd/src/direction-supervisor.ts`: runs each active Direction one node at a
//! time. The Research Director's typed cycle record confirms the scheduler's selected node; the
//! daemon issues the Task Packet, runs the worker in its own worktree, checks it deterministically
//! (postflight), and has an independent reviewer judge it. The baseline node's frozen evaluation
//! runs as a supervised Job; the hypothesis node is answered by the Direction's Autoresearch,
//! which it starts on its own up to a cap; a complete graph goes to the closure Review.
//!
//! Divergences: prompt-embedded JSON has sorted keys.

use super::{
    Busy, Dependencies, completion_refs, from_packet, git, iso_in, node, node_state, nodes, number,
    postflight, repository_root, review_passes_request, strings, unique, version,
};
use crate::daemon::{error_text, event_draft, spawn_eager};
use crate::director_brief::{
    DirectionSpawnContext, direction_autoresearch_outcomes, direction_brief, director_retry_allowed,
};
use crate::research_spawner::{
    CHILD_BLOCKED_MS, DirectionChild, EvaluationExecution, EvaluationJob, MIN_AUTORESEARCH_TOKENS,
    frozen_evaluation_files, run_evaluation_job, scope_issues, spawn_direction_autoresearch,
};
use crate::runtime::{now_ms, parse_ms};
use crate::task_postflight::{completion_run_issues, write_up_only};
use crate::task_templates::{
    DaemonEvaluation, completion_instructions, general_worker_completion_template,
    librarian_instructions, reviewer_instructions, stringify,
};
use crate::task_worktree::{
    ResumePoint, create_task_worktree, resume_instruction, resume_point, task_worktree_path,
};
use crate::{Error, Result, rejected};
use nosh_agent::policy::{Role, SessionOptions, TaskPermissions, TaskWorkspace};
use nosh_research::artifacts::ArtifactStore;
use nosh_research::graph::{GraphNode, VersionedDag};
use nosh_research::research_control::{
    BaselineInput, Host, LIBRARIAN_RESEARCH_HOSTS, daemon_task_facts, declared_task_commands,
    librarian_network_allowed, prior_attempt_instructions, task_command_instructions,
    task_command_runs,
};
use nosh_research::scheduler::{Scheduler, SchedulingContext};
use nosh_sys::jobs::{JobState, Runner};
use nosh_wire::js::{slice_to, strict_equal, string, truthy};
use nosh_wire::{
    IdPrefix, canonical_json, create_id, episode_type_for_role, normalize_numbers, now_utc,
    schema_uri, sha256,
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Autoresearch a Direction starts on its own per hypothesis node; after that it waits for its
/// user.
const MAX_AUTONOMOUS_AUTORESEARCH: usize = 2;

/// What a schema-valid Research Director cycle that judged the selected node should not run
/// reports (TypeScript's `DirectorHalt`).
const HALTED: &str = "Research Director halted the selected node; user attention required";

/// A valid Research Director cycle's decision.
enum Cycle {
    /// Run the node; a spawn node's cycle carries the Autoresearch it specified.
    Proceed(Option<Value>),
    /// The current work does not contribute: the director halts it.
    Halted,
}

/// `spawnAutoresearch`'s `"spawned" | "concluded" | false`.
#[derive(PartialEq)]
enum Spawn {
    Started,
    Concluded,
    NotStarted,
}

/// One task session's identity and authority.
struct Session<'a> {
    agent_id: &'a str,
    task_id: &'a str,
    role: Role,
    permissions: Option<TaskPermissions>,
    workspace: Option<TaskWorkspace>,
}

/// Who a Task Packet assigns, and the record it must answer with.
struct Assignment<'a> {
    agent_id: &'a str,
    task_id: &'a str,
    role: Role,
    response_schema: &'a str,
    expires_at: &'a str,
}

/// What an independent Review must judge.
struct ReviewSpec<'a> {
    task_id: &'a str,
    reviewer_agent_id: &'a str,
    producer_agent_ids: Vec<String>,
    review_type: &'a str,
    target_type: &'a str,
    target_id: &'a str,
    target_version: i64,
    criterion_id: &'a str,
    statement: String,
    artifacts: Vec<String>,
    evidence: Vec<String>,
}

fn role_for(node_type: &str) -> Role {
    match node_type {
        "literature_review" | "librarian_researcher" => Role::LibrarianResearcher,
        "reviewer" | "claim_review" | "final_review" => Role::Reviewer,
        _ => Role::GeneralWorker,
    }
}

fn event(project_id: &str, direction: &Value, kind: &str, payload: Value) -> Value {
    let direction_id = string(direction.get("entityId"));
    event_draft(
        kind,
        "direction_supervisor",
        json!({ "projectId": project_id, "missionId": direction["value"]["missionId"], "directionId": direction_id, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null }),
        json!(direction_id),
        Value::Null,
        payload,
    )
}

fn scope(project_id: &str, direction: &Value, agent_id: Option<&str>) -> Value {
    json!({ "projectId": project_id, "missionId": direction["value"]["missionId"], "directionId": direction["entityId"], "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": agent_id })
}

/// `id.slice(n)`.
fn from_index(id: &str, n: usize) -> &str {
    id.get(n..).unwrap_or_default()
}

/// Closure answer for promoted experiments: what was selected, its daemon-measured result, and
/// that selection is not a held-out evaluation.
pub fn accepted_closure_answer(accepted: &[String], results: &[Value]) -> String {
    let findings: Vec<String> = accepted
        .iter()
        .map(|id| {
            let result = results.iter().find(|entry| {
                entry["experimentId"] == id.as_str() && entry["promotionDecision"] == "promoted"
            });
            let field = |pointer: &str| result.and_then(|result| result.pointer(pointer));
            let metric = field("/metrics/0")
                .filter(|metric| !metric.is_null())
                .cloned()
                .unwrap_or(Value::Null);
            format!(
                "Reviewed experiment {id} is the accepted frontier: daemon-measured {}, primary-metric improvement {} over its parent ({}), guardrails passing",
                slice_to(&stringify(&metric), 400),
                string(field("/comparison/improvement")),
                string(field("/comparison/parentScore"))
            )
        })
        .collect();
    format!(
        "{}. This selects a configuration on the frozen contract's evaluation data; it is not a held-out evaluation.",
        findings.join("; ")
    )
}

/// The canonical hypothesis node created with every Direction; it depends on the baseline.
fn is_hypothesis_node(node: &GraphNode) -> bool {
    node.node_type == "general_worker" && node.title == "Evaluate bounded hypothesis families"
}

/// The canonical baseline node created with every Direction (see research control's default
/// Direction nodes).
fn is_baseline_node(node: &Value) -> bool {
    node["type"] == "general_worker"
        && node["title"] == "Reproduce accepted baseline"
        && strings(&node["hardDependencies"]).is_empty()
        && strings(&node["softDependencies"]).is_empty()
}

/// Whether an Autoresearch was started by this Direction for `node_id`.
fn autonomous(run: &Value, direction_id: &str, node_id: &str) -> bool {
    let origin = &run["value"]["origin"];
    origin["scopeType"] == "direction"
        && origin["scopeId"] == direction_id
        && origin["nodeId"] == node_id
}

pub struct DirectionSupervisor<H: Host> {
    deps: Dependencies<H>,
    busy: RefCell<HashSet<String>>,
    awaiting: RefCell<HashSet<String>>,
    scheduler: RefCell<Scheduler>,
    timer: RefCell<Option<JoinHandle<()>>>,
}

impl<H: Host + 'static> DirectionSupervisor<H> {
    pub fn new(deps: Dependencies<H>) -> Rc<Self> {
        Rc::new(DirectionSupervisor {
            deps,
            busy: RefCell::default(),
            awaiting: RefCell::default(),
            scheduler: RefCell::default(),
            timer: RefCell::default(),
        })
    }

    /// Ticks once a second until [`stop`](Self::stop). Each cycle records its own failures as
    /// events; only failures after shutdown (closed stores) reach here, and they are dropped.
    pub fn start(self: &Rc<Self>) {
        let supervisor = Rc::downgrade(self);
        let timer = tokio::task::spawn_local(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(supervisor) = supervisor.upgrade() else {
                    break;
                };
                let _ = supervisor.tick();
            }
        });
        if let Some(previous) = self.timer.replace(Some(timer)) {
            previous.abort();
        }
    }

    pub fn stop(&self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }

    /// Starts a cycle for every active Direction; each runs on until its first wait.
    pub fn tick(self: &Rc<Self>) -> Result<()> {
        for project in (self.deps.projects)()? {
            let project_id = string(project.get("projectId"));
            for direction in self.deps.research.directions(&project_id)? {
                let direction_id = string(direction.get("entityId"));
                match direction["state"].as_str().unwrap_or_default() {
                    "active" => {
                        let supervisor = Rc::clone(self);
                        let project = project.clone();
                        spawn_eager(async move {
                            let _ = supervisor.cycle(project, direction_id).await;
                        });
                    }
                    "reviewing" => self.recover_interrupted_review(&project_id, &direction_id)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn emit(&self, draft: &Value) -> Result<()> {
        (self.deps.emit)(draft)
    }

    fn transition(
        &self,
        project_id: &str,
        direction: &Value,
        next: &str,
        key: &str,
    ) -> Result<Value> {
        Ok(self.deps.research.transition_direction(
            project_id,
            &string(direction.get("entityId")),
            version(direction),
            next,
            key,
        )?)
    }

    fn transition_node(
        &self,
        project_id: &str,
        direction: &Value,
        node_id: &str,
        next: &str,
        key: &str,
        lease: Option<&Value>,
    ) -> Result<Value> {
        Ok(self.deps.research.transition_direction_node(
            project_id,
            &string(direction.get("entityId")),
            version(direction),
            node_id,
            next,
            key,
            lease,
            false,
        )?)
    }

    /// A closure review cut short by a daemon restart leaves the Direction in reviewing, which no
    /// cycle handles; return it to active so closure runs again.
    fn recover_interrupted_review(&self, project_id: &str, direction_id: &str) -> Result<()> {
        if self.busy.borrow().contains(direction_id)
            || self.deps.agents.inspect().iter().any(|agent| {
                agent.direction_id.as_deref() == Some(direction_id)
                    && agent.autoresearch_id.as_deref().is_none_or(str::is_empty)
            })
        {
            return Ok(());
        }
        let direction = self.deps.research.direction(project_id, direction_id)?;
        if direction["state"] != "reviewing" {
            return Ok(());
        }
        let from = version(&direction);
        self.transition(
            project_id,
            &direction,
            "active",
            &format!("review-interrupted-{from}"),
        )?;
        self.emit(&event(
            project_id,
            &direction,
            "direction.review_recovered",
            json!({ "fromVersion": from }),
        ))
    }

    async fn cycle(self: Rc<Self>, project: Value, direction_id: String) -> Result<()> {
        let project_id = string(project.get("projectId"));
        if self.busy.borrow().contains(&direction_id)
            || self
                .deps
                .agents
                .inspect()
                .iter()
                .any(|agent| agent.direction_id.as_deref() == Some(direction_id.as_str()))
        {
            return Ok(());
        }
        let owner = self.deps.research.direction(&project_id, &direction_id)?;
        if let Some(runtime) = &self.deps.runtime
            && runtime.has_active_fork(
                &project_id,
                owner["value"]["missionId"].as_str(),
                Some(&direction_id),
            )?
        {
            return Ok(());
        }
        let origin = &owner["value"]["origin"];
        if origin["scopeType"] == "mission"
            && self
                .deps
                .research
                .mission(&project_id, &string(origin.get("scopeId")))?["state"]
                != "running"
        {
            return Ok(());
        }
        let _busy = Busy::hold(&self.busy, &direction_id);
        match self.advance(&project, &direction_id).await {
            Ok(()) => Ok(()),
            Err(error) => self.cycle_failed(&project_id, &direction_id, &error),
        }
    }

    async fn advance(&self, project: &Value, direction_id: &str) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let research = &self.deps.research;
        let mut direction = research.direction(&project_id, direction_id)?;
        if direction["state"] != "active" {
            return Ok(());
        }
        direction = self.reconcile_orphaned_nodes(&project_id, direction)?;
        if direction["state"] != "active" {
            return Ok(());
        }
        let graph_nodes: Vec<GraphNode> =
            serde_json::from_value(direction["value"]["nodes"].clone())
                .map_err(|error| Error::Rejected(error.to_string()))?;
        let graph = VersionedDag::new(
            direction_id,
            graph_nodes,
            "hydrated",
            direction["value"]["graphVersion"].as_i64().unwrap_or(0),
        )?;
        if graph.completion_ready() {
            return self.close(project, direction).await;
        }
        let context = SchedulingContext {
            paused: false,
            workspace_conflicts: HashSet::new(),
            available_roles: ["librarian_researcher", "general_worker", "reviewer"]
                .map(str::to_owned)
                .into(),
            available_agents: 1.0,
        };
        let frontier = self.scheduler.borrow().frontier(&graph, &context, now_ms());
        let Some(node) = frontier.into_iter().next() else {
            if !nodes(&direction).iter().any(|item| {
                matches!(
                    item["state"].as_str(),
                    Some("leased" | "working" | "postflight" | "reviewing")
                )
            }) {
                self.transition(
                    &project_id,
                    &direction,
                    "blocked",
                    &format!("direction-blocked-{}", create_id(IdPrefix::Cmd)),
                )?;
            }
            return Ok(());
        };
        // Hypothesis evaluation is experiment work: the Direction-backed Autoresearch fulfils it,
        // not a single worker.
        if is_hypothesis_node(&node) {
            return self.await_autoresearch(project, direction, &node).await;
        }
        let selected =
            serde_json::to_value(&node).map_err(|error| Error::Rejected(error.to_string()))?;
        if let Cycle::Halted = self.director(project, &direction, &selected, None).await? {
            return rejected(HALTED);
        }
        direction = research.direction(&project_id, direction_id)?;
        if direction["state"] == "active" {
            self.run_node(project, direction, &node.id).await?;
        }
        Ok(())
    }

    fn cycle_failed(&self, project_id: &str, direction_id: &str, error: &Error) -> Result<()> {
        let research = &self.deps.research;
        let mut direction = research.direction(project_id, direction_id)?;
        // An invalid cycle record is a model slip, not a stuck Direction: the next tick runs a
        // fresh cycle.
        if let Error::Slip(message) = error
            && direction["state"] == "active"
        {
            let own: Vec<Value> = research
                .events_after(project_id, 0)?
                .into_iter()
                .filter(|entry| {
                    entry["scope"]["directionId"] == direction_id
                        && !truthy(entry["scope"].get("autoresearchId"))
                })
                .collect();
            if director_retry_allowed(
                &own,
                &schema_uri("research-director-cycle"),
                "direction.director_retry",
            ) {
                return self.emit(&event(
                    project_id,
                    &direction,
                    "direction.director_retry",
                    json!({ "message": message }),
                ));
            }
        }
        self.emit(&event(
            project_id,
            &direction,
            "direction.supervisor_error",
            json!({ "message": error_text(error, "Direction supervisor failed") }),
        ))?;
        if direction["state"] == "reviewing" {
            direction = self.transition(
                project_id,
                &direction,
                "active",
                &format!("direction-review-failed-{}", create_id(IdPrefix::Cmd)),
            )?;
        }
        if direction["state"] == "active" {
            self.transition(
                project_id,
                &direction,
                "blocked",
                &format!("direction-error-{}", create_id(IdPrefix::Cmd)),
            )?;
        }
        Ok(())
    }

    /// Runs the Research Director on the selected node.
    async fn director(
        &self,
        project: &Value,
        direction: &Value,
        node: &Value,
        spawn: Option<&DirectionSpawnContext>,
    ) -> Result<Cycle> {
        let project_id = string(project.get("projectId"));
        let direction_id = string(direction.get("entityId"));
        let agent_id = create_id(IdPrefix::Agt);
        let task_id = create_id(IdPrefix::Tsk);
        let brief = direction_brief(
            &self.deps.research,
            project,
            direction,
            node,
            &agent_id,
            spawn,
            &self.deps.schema_dir,
        )?;
        let session = Session {
            agent_id: &agent_id,
            task_id: &task_id,
            role: Role::ResearchDirector,
            permissions: None,
            workspace: None,
        };
        if let Err(error) = self
            .run_session(project, direction, session, &brief.prompt)
            .await
        {
            let message = error.to_string();
            return Err(if message.starts_with("Terminal output rejected") {
                Error::Slip(message)
            } else {
                error
            });
        }
        let record = self.deps.research.terminal_record(
            &project_id,
            &task_id,
            Some("research-director-cycle"),
        )?;
        let value = &direction["value"];
        let Some(record) = record.filter(|record| {
            record["cycleId"] == brief.ids.cycle_id
                && record["directorAgentId"] == agent_id
                && record["directionId"] == direction_id
                && strict_equal(
                    record.get("observedGraphVersion"),
                    value.get("graphVersion"),
                )
                && strict_equal(
                    record.get("evaluationContractHash"),
                    value.get("evaluationContractHash"),
                )
        }) else {
            return Err(Error::Slip(
                "Research Director did not submit the current cycle record".into(),
            ));
        };
        // A valid "does not contribute" decision is the director halting work, not a supervisor
        // fault.
        if record.pointer("/questionCheck/currentWorkContributes") != Some(&Value::Bool(true)) {
            self.emit(&event(
                &project_id,
                direction,
                "direction.director_halted",
                json!({ "cycleId": record["cycleId"], "nodeId": node["id"] }),
            ))?;
            return Ok(Cycle::Halted);
        }
        Ok(Cycle::Proceed(
            record
                .get("spawn")
                .filter(|spawn| truthy(Some(spawn)))
                .cloned(),
        ))
    }

    /// Resolves the hypothesis node from the Direction's Autoresearch outcome (Autoresearch carries
    /// its own closure Review). Each execution is consumed once: a stopped or failed one costs a
    /// node attempt, after which the node waits for a new execution while attempts remain.
    async fn await_autoresearch(
        &self,
        project: &Value,
        direction: Value,
        node: &GraphNode,
    ) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let id = string(direction.get("entityId"));
        let consumed: HashSet<String> = research
            .events_after(&project_id, 0)?
            .iter()
            .filter(|entry| {
                matches!(
                    entry["type"].as_str(),
                    Some("direction.hypothesis_resolved" | "direction.hypothesis_continued")
                ) && entry["scope"]["directionId"] == id.as_str()
                    && entry["payload"]["nodeId"] == node.id.as_str()
            })
            .map(|entry| string(entry["payload"].get("autoresearchId")))
            .collect();
        let unconsumed = || -> Result<Vec<Value>> {
            Ok(research
                .autoresearch(&project_id)?
                .into_iter()
                .filter(|run| {
                    run["value"]["directionId"] == id.as_str()
                        && !consumed.contains(&string(run.get("entityId")))
                })
                .collect())
        };
        let mut runs = unconsumed()?;
        // A run left blocked with nobody working on it would hold the node forever: stop it, as a
        // Mission does with a stale child.
        let stale: Vec<Value> =
            runs.iter()
                .filter(|run| {
                    matches!(run["state"].as_str(), Some("blocked" | "paused"))
                        && now_ms() - parse_ms(&string(run["value"].get("updatedAt")))
                            > CHILD_BLOCKED_MS
                        && !self.deps.agents.inspect().iter().any(|agent| {
                            agent.autoresearch_id.as_deref() == run["entityId"].as_str()
                        })
                })
                .cloned()
                .collect();
        for run in stale {
            let run_id = string(run.get("entityId"));
            research.transition_autoresearch(
                &project_id,
                &run_id,
                version(&run),
                "stopped",
                &format!("child-stop:{run_id}:{}", version(&run)),
            )?;
            self.emit(&event(
                &project_id,
                &direction,
                "direction.autoresearch_stopped",
                json!({ "nodeId": node.id, "autoresearchId": run_id, "priorState": run["state"] }),
            ))?;
            runs = unconsumed()?;
        }
        let completed = runs.iter().find(|run| run["state"] == "completed").cloned();
        let finished = runs
            .iter()
            .find(|run| matches!(run["state"].as_str(), Some("failed" | "stopped")))
            .cloned();
        // A first Autoresearch that improved nothing is not the answer yet while a second, distinct
        // one can still run. Only when a second one actually starts; otherwise the completed one
        // answers the node, as before.
        if let Some(done) = &completed
            && finished.is_none()
            && strings(&done["value"]["acceptedFrontierExperimentIds"]).is_empty()
            && self.may_start_another(&project_id, &id, &node.id)?
            && self.autoresearch_tokens(&project_id, &direction)? >= MIN_AUTORESEARCH_TOKENS
        {
            match self.spawn_autoresearch(project, &direction, node).await? {
                Spawn::Started => {
                    return self.emit(&event(
                        &project_id,
                        &direction,
                        "direction.hypothesis_continued",
                        json!({ "nodeId": node.id, "autoresearchId": done["entityId"] }),
                    ));
                }
                Spawn::Concluded => return Ok(()),
                Spawn::NotStarted => {}
            }
        }
        let source = match (&completed, &finished) {
            (Some(source), _) | (None, Some(source)) => source.clone(),
            (None, None) => {
                if runs.is_empty()
                    && self.spawn_autoresearch(project, &direction, node).await?
                        != Spawn::NotStarted
                {
                    return Ok(());
                }
                if self.awaiting.borrow_mut().insert(node.id.clone()) {
                    let ids: Vec<String> =
                        runs.iter().map(|run| string(run.get("entityId"))).collect();
                    let action = if ids.is_empty() {
                        "Create and start a Direction-backed Autoresearch (TUI: /create autoresearch <file>, then /start autoresearch <id> <version>).".to_owned()
                    } else {
                        format!(
                            "Waiting for Autoresearch {}; if one is blocked, resume it (TUI: /transition autoresearch <id> <version> running).",
                            ids.join(", ")
                        )
                    };
                    self.emit(&event(
                        &project_id,
                        &direction,
                        "direction.awaiting_autoresearch",
                        json!({ "nodeId": node.id, "autoresearchIds": ids, "action": action }),
                    ))?;
                }
                return Ok(());
            }
        };
        let source_id = string(source.get("entityId"));
        let key = format!("direction-hypothesis-{source_id}");
        self.awaiting.borrow_mut().remove(&node.id);
        let lease = json!({ "leaseId": format!("lease_{}", from_index(&source_id, 3)), "ownerId": source_id, "version": version(&direction), "expiresAt": iso_in(60_000.0)? });
        let mut current = self.transition_node(
            &project_id,
            &direction,
            &node.id,
            "leased",
            &format!("{key}-leased"),
            Some(&lease),
        )?;
        current = self.transition_node(
            &project_id,
            &current,
            &node.id,
            "working",
            &format!("{key}-working"),
            None,
        )?;
        let outcome = if completed.is_some() {
            json!("accepted")
        } else {
            source["state"].clone()
        };
        self.emit(&event(
            &project_id,
            &direction,
            "direction.hypothesis_resolved",
            json!({ "nodeId": node.id, "autoresearchId": source_id, "outcome": outcome }),
        ))?;
        if completed.is_some() {
            for next in ["postflight", "reviewing", "accepted"] {
                current = self.transition_node(
                    &project_id,
                    &current,
                    &node.id,
                    next,
                    &format!("{key}-{next}"),
                    None,
                )?;
            }
            Ok(())
        } else {
            self.fail(&project_id, &id, &node.id, &source_id)
        }
    }

    /// Autoresearch this Direction started on its own for a hypothesis node.
    fn autonomous_runs(
        &self,
        project_id: &str,
        direction_id: &str,
        node_id: &str,
    ) -> Result<Vec<Value>> {
        Ok(self
            .deps
            .research
            .autoresearch(project_id)?
            .into_iter()
            .filter(|run| autonomous(run, direction_id, node_id))
            .collect())
    }

    /// Runs that count against `MAX_AUTONOMOUS_AUTORESEARCH`: those that evaluated at least one
    /// experiment. A run stopped before measuring anything (a harness fault, a stale block) did no
    /// research; at most twice the cap may start in all.
    fn may_start_another(
        &self,
        project_id: &str,
        direction_id: &str,
        node_id: &str,
    ) -> Result<bool> {
        let runs = self.autonomous_runs(project_id, direction_id, node_id)?;
        let results = self
            .deps
            .research
            .records(project_id, Some("experiment-result"))?;
        let researched = runs
            .iter()
            .filter(|run| {
                results
                    .iter()
                    .any(|result| strict_equal(result.get("autoresearchId"), run.get("entityId")))
            })
            .count();
        Ok(
            researched < MAX_AUTONOMOUS_AUTORESEARCH
                && runs.len() < 2 * MAX_AUTONOMOUS_AUTORESEARCH,
        )
    }

    /// Model tokens the next Autoresearch may use: 80% of the Direction budget goes to
    /// Autoresearch, the first gets at most 60% of that.
    fn autoresearch_tokens(&self, project_id: &str, direction: &Value) -> Result<f64> {
        let research = &self.deps.research;
        let id = string(direction.get("entityId"));
        let maximum = number(&direction["value"]["budget"]["maximumModelTokens"]);
        if maximum == 0.0 || maximum.is_nan() {
            return Ok(500_000.0);
        }
        let used = number(&research.direction_budget_use(project_id, &id)?["modelTokens"]);
        let remaining = maximum - used - maximum * 0.2;
        let first = !research.autoresearch(project_id)?.iter().any(|run| {
            let origin = &run["value"]["origin"];
            origin["scopeType"] == "direction" && origin["scopeId"] == id.as_str()
        });
        Ok(if first {
            nosh_wire::js::min(remaining, maximum * 0.8 * 0.6)
        } else {
            remaining
        }
        .floor())
    }

    /// The Research Director judged that more exploration does not contribute. With measured
    /// results for this node, the hypothesis is resolved on them (the latest completed run with an
    /// experiment result), so synthesis and the independent closure Review judge the evidence;
    /// without any, the halt stands and the Direction waits for the user.
    fn conclude_on_evidence(
        &self,
        project_id: &str,
        direction: &Value,
        node: &GraphNode,
    ) -> Result<bool> {
        let research = &self.deps.research;
        let id = string(direction.get("entityId"));
        let results = research.records(project_id, Some("experiment-result"))?;
        let Some(source) = research.autoresearch(project_id)?.into_iter().rfind(|run| {
            autonomous(run, &id, &node.id)
                && run["state"] == "completed"
                && results
                    .iter()
                    .any(|result| strict_equal(result.get("autoresearchId"), run.get("entityId")))
        }) else {
            return Ok(false);
        };
        let source_id = string(source.get("entityId"));
        let key = format!("direction-concluded-{}-{source_id}", node.id);
        self.awaiting.borrow_mut().remove(&node.id);
        let lease = json!({ "leaseId": format!("lease_{}", from_index(&source_id, 3)), "ownerId": source_id, "version": version(direction), "expiresAt": iso_in(60_000.0)? });
        let mut current = self.transition_node(
            project_id,
            direction,
            &node.id,
            "leased",
            &format!("{key}-leased"),
            Some(&lease),
        )?;
        current = self.transition_node(
            project_id,
            &current,
            &node.id,
            "working",
            &format!("{key}-working"),
            None,
        )?;
        self.emit(&event(
            project_id,
            direction,
            "direction.director_concluded",
            json!({ "nodeId": node.id, "autoresearchId": source_id }),
        ))?;
        self.emit(&event(
            project_id,
            direction,
            "direction.hypothesis_resolved",
            json!({ "nodeId": node.id, "autoresearchId": source_id, "outcome": "accepted", "concludedBy": "research_director" }),
        ))?;
        for next in ["postflight", "reviewing", "accepted"] {
            current = self.transition_node(
                project_id,
                &current,
                &node.id,
                next,
                &format!("{key}-{next}"),
                None,
            )?;
        }
        Ok(true)
    }

    /// Starts this Direction's next Autoresearch without a human: a Mission-spawned Direction's
    /// first one follows the Mission's spec; otherwise the Research Director specifies a distinct
    /// one. `Spawn::NotStarted` when none may start.
    async fn spawn_autoresearch(
        &self,
        project: &Value,
        direction: &Value,
        node: &GraphNode,
    ) -> Result<Spawn> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let direction_id = string(direction.get("entityId"));
        let value = &direction["value"];
        let ordinal = self
            .autonomous_runs(&project_id, &direction_id, &node.id)?
            .len()
            + 1;
        if !self.may_start_another(&project_id, &direction_id, &node.id)? {
            return Ok(Spawn::NotStarted);
        }
        let tokens = self.autoresearch_tokens(&project_id, direction)?;
        if tokens < MIN_AUTORESEARCH_TOKENS {
            return Ok(Spawn::NotStarted);
        }
        let key = format!("child-spawn:{direction_id}:{}:{ordinal}", node.id);
        let prior = research.external_operation(&project_id, &key)?;
        let rejections: Vec<Value> = research
            .events_after(&project_id, 0)?
            .into_iter()
            .filter(|entry| {
                entry["type"] == "research.spawn_rejected"
                    && entry["scope"]["directionId"] == direction_id.as_str()
                    && entry["payload"]["ordinal"].as_f64() == Some(ordinal as f64)
            })
            .collect();
        if rejections.len() >= 3 {
            return Ok(Spawn::NotStarted);
        }
        let mut spawn = prior
            .map(|prior| prior["request"]["spawn"].clone())
            .filter(|spawn| !spawn.is_null());
        let origin = &value["origin"];
        if spawn.is_none() && ordinal == 1 && origin["scopeType"] == "mission" {
            let mission_spec = research
                .external_operation(
                    &project_id,
                    &format!(
                        "child-spawn:{}:{}:{}",
                        string(origin.get("scopeId")),
                        string(origin.get("nodeId")),
                        string(origin.get("attempt"))
                    ),
                )?
                .map(|operation| operation["request"].clone())
                .unwrap_or(Value::Null);
            let field = |key: &str, fallback: Value| match mission_spec["spawn"].get(key) {
                Some(found) if !found.is_null() => found.clone(),
                _ => fallback,
            };
            spawn = Some(
                json!({ "nodeId": node.id, "kind": "autoresearch", "question": value["question"], "familyTags": field("familyTags", json!(["direction"])), "scope": field("scope", json!(["**"])) }),
            );
        }
        if spawn.is_none() {
            let siblings = research
                .autoresearch(&project_id)?
                .iter()
                .filter(|run| run["value"]["directionId"] == direction_id.as_str())
                .map(|run| {
                    format!(
                        "{} ({}): {}; familyTags {}; scope {}",
                        string(run.get("entityId")),
                        string(run.get("state")),
                        string(run["value"].get("decisionQuestion")),
                        strings(&run["value"]["familyTags"]).join(", "),
                        strings(&run["value"]["scope"]).join(", ")
                    )
                })
                .collect();
            let context = DirectionSpawnContext {
                siblings,
                rejections: rejections
                    .iter()
                    .map(|entry| strings(&entry["payload"]["issues"]).join("; "))
                    .collect(),
            };
            let selected =
                serde_json::to_value(node).map_err(|error| Error::Rejected(error.to_string()))?;
            match self
                .director(project, direction, &selected, Some(&context))
                .await?
            {
                Cycle::Proceed(found) => spawn = found,
                Cycle::Halted => {
                    if self.conclude_on_evidence(&project_id, direction, node)? {
                        return Ok(Spawn::Concluded);
                    }
                    return rejected(HALTED);
                }
            }
        }
        let started = (|| -> Result<()> {
            let Some(spawn) = spawn
                .as_ref()
                .filter(|spawn| spawn["nodeId"] == node.id.as_str())
            else {
                return rejected(format!(
                    "the Research Director did not fill spawn for node {}",
                    node.id
                ));
            };
            let execution = &value["evaluationContract"]["execution"];
            let problems = if truthy(Some(execution)) {
                scope_issues(
                    &strings(&spawn["scope"]),
                    &frozen_evaluation_files(
                        &repository_root(project),
                        &strings(&execution["command"]),
                        &string(execution.get("resultPath")),
                    ),
                )
            } else {
                Vec::new()
            };
            if !problems.is_empty() {
                return rejected(problems.join("; "));
            }
            let child_id = spawn_direction_autoresearch(
                research,
                &project_id,
                DirectionChild {
                    direction_id: &direction_id,
                    mission_id: value["missionId"].as_str(),
                    node_id: &node.id,
                    ordinal: ordinal as f64,
                    spawn,
                    maximum_model_tokens: tokens,
                },
            )?;
            self.emit(&event(
                &project_id,
                direction,
                "direction.autoresearch_spawned",
                json!({ "nodeId": node.id, "ordinal": ordinal, "autoresearchId": child_id }),
            ))
        })();
        match started {
            Ok(()) => Ok(Spawn::Started),
            Err(error) => {
                self.emit(&event(
                    &project_id,
                    direction,
                    "research.spawn_rejected",
                    json!({ "nodeId": node.id, "ordinal": ordinal, "issues": [error.to_string()] }),
                ))?;
                Ok(Spawn::NotStarted)
            }
        }
    }

    fn reconcile_orphaned_nodes(&self, project_id: &str, mut direction: Value) -> Result<Value> {
        let stale_nodes: Vec<Value> = nodes(&direction)
            .into_iter()
            .filter(|node| {
                matches!(
                    node["state"].as_str(),
                    Some("leased" | "working" | "postflight" | "reviewing")
                )
            })
            .collect();
        for stale in stale_nodes {
            let stale_id = string(stale.get("id"));
            let key = || match stale["lease"]["leaseId"].as_str() {
                Some(lease_id) => lease_id.to_owned(),
                None => create_id(IdPrefix::Cmd),
            };
            if stale["state"] == "leased" {
                direction = self.transition_node(
                    project_id,
                    &direction,
                    &stale_id,
                    "ready",
                    &format!("direction-recovery-release-{}", key()),
                    None,
                )?;
            } else {
                direction = self.transition_node(
                    project_id,
                    &direction,
                    &stale_id,
                    "failed",
                    &format!("direction-recovery-failed-{}", key()),
                    None,
                )?;
                let current = node(&direction, &stale_id).cloned().unwrap_or_default();
                if number(&current["attempt"]) < number(&current["maximumAttempts"]) {
                    direction = self.transition_node(
                        project_id,
                        &direction,
                        &stale_id,
                        "ready",
                        &format!("direction-recovery-ready-{}", key()),
                        None,
                    )?;
                } else {
                    self.emit(&event(
                        project_id,
                        &direction,
                        "direction.node_recovered",
                        json!({ "nodeId": stale_id, "priorState": stale["state"], "disposition": "attempt_budget_exhausted" }),
                    ))?;
                    return self.transition(
                        project_id,
                        &direction,
                        "blocked",
                        &format!("direction-recovery-blocked-{}", key()),
                    );
                }
            }
            self.emit(&event(
                project_id,
                &direction,
                "direction.node_recovered",
                json!({ "nodeId": stale_id, "priorState": stale["state"], "disposition": "ready_for_new_lease" }),
            ))?;
        }
        Ok(direction)
    }

    async fn run_node(&self, project: &Value, mut direction: Value, node_id: &str) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let direction_id = string(direction.get("entityId"));
        let schema_dir = &self.deps.schema_dir;
        match node(&direction, node_id) {
            Some(node) if node["state"] == "ready" => {}
            _ => return Ok(()),
        }
        self.scheduler.borrow_mut().record_allocation(node_id);
        let role = role_for(
            node(&direction, node_id)
                .and_then(|node| node["type"].as_str())
                .unwrap_or_default(),
        );
        let agent_id = create_id(IdPrefix::Agt);
        let task_id = create_id(IdPrefix::Tsk);
        let expires_at = iso_in(30.0 * 60_000.0)?;
        let lease = json!({ "leaseId": task_id, "ownerId": agent_id, "version": version(&direction), "expiresAt": expires_at });
        direction = self.transition_node(
            &project_id,
            &direction,
            node_id,
            "leased",
            &format!("direction-lease-{task_id}"),
            Some(&lease),
        )?;
        direction = self.transition_node(
            &project_id,
            &direction,
            node_id,
            "working",
            &format!("direction-working-{task_id}"),
            None,
        )?;
        let node = node(&direction, node_id).cloned().unwrap_or_default();
        let title = string(node.get("title"));
        let schema = match role {
            Role::Reviewer => "review-verdict",
            Role::LibrarianResearcher => "librarian-completion",
            _ => "general-worker-completion",
        };
        let attempt: Result<(Value, Option<Value>, Option<DaemonEvaluation>)> = async {
            let packet = self.issue_task_packet(
                project,
                &direction,
                &node,
                &Assignment {
                    agent_id: &agent_id,
                    task_id: &task_id,
                    role,
                    response_schema: schema,
                    expires_at: &expires_at,
                },
            )?;
            let evaluation = if role == Role::GeneralWorker && is_baseline_node(&node) {
                self.evaluate_baseline(project, &direction, &packet).await?
            } else {
                None
            };
            let mut direct_review = None;
            let mut digest = String::new();
            if role == Role::Reviewer {
                let producer = research
                    .records(&project_id, Some("research-director-cycle"))?
                    .into_iter()
                    .rev()
                    .find(|record| record["directionId"] == direction_id.as_str())
                    .and_then(|record| record["directorAgentId"].as_str().map(str::to_owned))
                    .filter(|id| !id.is_empty());
                let Some(producer) = producer else {
                    return rejected("Direction Review requires a durable producer identity");
                };
                let (evidence_ids, outcomes) =
                    direction_autoresearch_outcomes(research, &project_id, &direction_id)?;
                digest = outcomes;
                direct_review = Some(self.issue_review_request(
                    project,
                    &direction,
                    ReviewSpec {
                        task_id: &task_id,
                        reviewer_agent_id: &agent_id,
                        producer_agent_ids: vec![producer],
                        review_type: "task",
                        target_type: "graph_node",
                        target_id: node_id,
                        target_version: version(&direction),
                        criterion_id: node_id,
                        statement: format!("{}: the synthesis follows from the resolved Autoresearch outcomes and Evidence under the frozen contract and its guardrails. Judge fidelity, not whether the Direction's goal was reached: an honestly inconclusive or negative answer passes; an overstated, understated, or unsupported one fails.", slice_to(&title, 600)),
                        artifacts: Vec::new(),
                        evidence: evidence_ids,
                    },
                )?);
            }
            let mut prompt = format!(
                "Daemon-issued Task Packet: {}. Complete only Direction node {node_id}: {title}. You work in an isolated worktree on branch {}. ",
                stringify(&packet),
                string(packet.pointer("/workspace/branch"))
            );
            if let Some(evaluation) = &evaluation {
                prompt.push_str(&format!(
                    "The daemon ran the frozen evaluation command as supervised Job {} at commit {}: state {}, exit code {}, metrics {}, metrics artifact {}{}. You cannot run commands; audit this result against the frozen evaluation contract and its baseline, and claim each criterion only as far as this evidence supports it. This node only reproduces and records the baseline: it is satisfied when the frozen evaluation completed on the starting commit and you report its metrics faithfully, even if the baseline falls short of the Direction's goal (closing that gap is the job of later nodes and Autoresearch), so set taskOutcome to completed in that case. ",
                    evaluation.job_id,
                    string(packet.pointer("/workspace/startingCommit")),
                    evaluation.state,
                    exit_code(evaluation),
                    stringify(&evaluation.metrics),
                    evaluation.metric_artifact_id.as_deref().unwrap_or("none"),
                    evaluation
                        .failure
                        .as_ref()
                        .map(|failure| format!(", failure: {failure}"))
                        .unwrap_or_default()
                ));
            }
            if let Some(review) = &direct_review {
                if !digest.is_empty() {
                    prompt.push_str(&format!("Resolved Autoresearch outcomes for this Direction (daemon-authored; the required Evidence holds these statements): {digest} "));
                }
                prompt.push_str(&format!("Review Request: {}. ", stringify(review)));
            }
            prompt.push_str(&match (role, &direct_review) {
                (Role::GeneralWorker, _) => completion_instructions(
                    &general_worker_completion_template(&packet, evaluation.as_ref()),
                    "general_worker",
                    schema_dir,
                ),
                (Role::Reviewer, Some(review)) => reviewer_instructions(review, schema_dir),
                (Role::LibrarianResearcher, _) => librarian_instructions(
                    &task_id,
                    packet.pointer("/permissions/network") == Some(&json!("allowlisted")),
                    schema_dir,
                ),
                _ => format!(
                    "Submit exactly one {} in the host terminal JSON envelope.",
                    schema_uri(schema)
                ),
            });
            let session = Session {
                agent_id: &agent_id,
                task_id: &task_id,
                role,
                permissions: Some(from_packet(&packet["permissions"])?),
                workspace: Some(from_packet(&packet["workspace"])?),
            };
            self.run_session(project, &direction, session, &prompt)
                .await?;
            Ok((packet, direct_review, evaluation))
        }
        .await;
        let (packet, direct_review, evaluation) = match attempt {
            Ok(issued) => issued,
            Err(error) => {
                self.emit(&event(
                    &project_id,
                    &direction,
                    "direction.task_failed",
                    json!({ "taskId": task_id, "nodeId": node_id, "message": error_text(&error, "Direction task failed") }),
                ))?;
                return self.fail(&project_id, &direction_id, node_id, &task_id);
            }
        };
        if role == Role::Reviewer {
            direction = self.transition_node(
                &project_id,
                &direction,
                node_id,
                "postflight",
                &format!("direction-postflight-{task_id}"),
                None,
            )?;
            direction = self.transition_node(
                &project_id,
                &direction,
                node_id,
                "reviewing",
                &format!("direction-reviewing-{task_id}"),
                None,
            )?;
            let request = direct_review.expect("a reviewer node always issues its Review Request");
            return self.apply_review(
                &project_id,
                &direction,
                node_id,
                &task_id,
                &agent_id,
                None,
                &request,
            );
        }
        let completion = research.terminal_record(&project_id, &task_id, Some(schema))?;
        let issues = match &completion {
            Some(completion) => {
                let root = repository_root(project);
                let write_up = write_up_only(&root);
                let daemon_jobs: Vec<String> = evaluation
                    .iter()
                    .map(|evaluation| evaluation.job_id.clone())
                    .collect();
                let mut issues = postflight(project, &packet, schema, completion)?;
                issues.extend(completion_run_issues(
                    &task_command_runs(research, &project_id)?,
                    &task_id,
                    completion,
                    &daemon_jobs,
                    Some(&write_up),
                ));
                issues
            }
            None => vec!["missing completion record".to_owned()],
        };
        if !issues.is_empty() {
            self.emit(&event(
                &project_id,
                &direction,
                "direction.postflight_failed",
                json!({ "taskId": task_id, "nodeId": node_id, "issues": issues }),
            ))?;
            return self.fail(&project_id, &direction_id, node_id, &task_id);
        }
        let completion = completion.expect("postflight passed only with a completion");
        direction = self.transition_node(
            &project_id,
            &direction,
            node_id,
            "postflight",
            &format!("direction-postflight-{task_id}"),
            None,
        )?;
        direction = self.transition_node(
            &project_id,
            &direction,
            node_id,
            "reviewing",
            &format!("direction-reviewing-{task_id}"),
            None,
        )?;
        let (artifacts, evidence) = completion_refs(&completion);
        let reviewer_id = create_id(IdPrefix::Agt);
        let review_task = create_id(IdPrefix::Tsk);
        let request = self.issue_review_request(
            project,
            &direction,
            ReviewSpec {
                task_id: &review_task,
                reviewer_agent_id: &reviewer_id,
                producer_agent_ids: vec![agent_id.clone()],
                review_type: "task",
                target_type: "graph_node",
                target_id: node_id,
                target_version: version(&direction),
                criterion_id: node_id,
                statement: title.clone(),
                artifacts,
                evidence,
            },
        )?;
        let recorded = evaluation
            .as_ref()
            .map(|evaluation| {
                format!(
                    "Daemon-recorded evaluation (authoritative): Job {}, state {}, exit code {}, metrics {}, metrics artifact {}. ",
                    evaluation.job_id,
                    evaluation.state,
                    exit_code(evaluation),
                    stringify(&evaluation.metrics),
                    evaluation.metric_artifact_id.as_deref().unwrap_or("none")
                )
            })
            .unwrap_or_default();
        let prompt = format!(
            "Independently review Direction node {node_id}: {title}. Worker {agent_id} submitted {} under contract {}. {} Review Request: {}. {recorded}{}",
            stringify(&completion),
            string(direction["value"].get("evaluationContractHash")),
            daemon_task_facts(research, &project_id, &task_id)?,
            stringify(&request),
            reviewer_instructions(&request, schema_dir)
        );
        let session = Session {
            agent_id: &reviewer_id,
            task_id: &review_task,
            role: Role::Reviewer,
            permissions: None,
            workspace: None,
        };
        self.run_session(project, &direction, session, &prompt)
            .await?;
        let commit = completion
            .pointer("/codeChanges/endingCommit")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.apply_review(
            &project_id,
            &direction,
            node_id,
            &review_task,
            &reviewer_id,
            commit.as_deref(),
            &request,
        )
    }

    /// Baseline nodes are reproduced by the daemon, not the model: run the frozen command as a
    /// supervised Job.
    async fn evaluate_baseline(
        &self,
        project: &Value,
        direction: &Value,
        packet: &Value,
    ) -> Result<Option<DaemonEvaluation>> {
        let Some(job_runner) = &self.deps.job_runner else {
            return Ok(None);
        };
        let project_id = string(project.get("projectId"));
        let value = &direction["value"];
        let declared = &value["evaluationContract"]["execution"];
        let command = strings(&declared["command"]);
        if command.is_empty() || !truthy(declared.get("resultPath")) {
            return rejected("Direction evaluation contract has no runnable execution command");
        }
        let worktree = task_worktree_path(
            &repository_root(project),
            &string(packet.pointer("/workspace/worktreeId")),
        );
        let display_command = command.join(" ");
        let execution = EvaluationExecution {
            runner: if declared["runner"] == "wsl2" {
                Runner::Wsl2
            } else {
                Runner::Native
            },
            distribution: declared["distribution"].as_str().map(str::to_owned),
            command,
            result_path: string(declared.get("resultPath")),
            timeout_seconds: declared["timeoutSeconds"].as_f64().unwrap_or(600.0),
            uses_gpu: Some(declared["usesGpu"] == true),
        };
        let run = run_evaluation_job(
            job_runner.as_ref(),
            EvaluationJob {
                project_id: &project_id,
                mission_id: value["missionId"].as_str(),
                direction_id: direction["entityId"].as_str(),
                autoresearch_id: None,
                experiment_id: &string(value.get("plannedBaselineExperimentId")),
                commit_sha: &string(packet.pointer("/workspace/startingCommit")),
                worktree: &worktree,
                execution: &execution,
                evaluation_contract_hash: &string(value.get("evaluationContractHash")),
            },
        )
        .await?;
        let mut metric_artifact_id = None;
        if run.job.state == JobState::Completed && run.result_path.exists() {
            let retention = if run.failure.is_some() {
                "negative_evidence"
            } else {
                "accepted_evidence"
            };
            metric_artifact_id = Some(self.job_artifact(
                project,
                direction,
                "artifact_metrics",
                "application/json",
                &run.result_path,
                retention,
            )?);
            // An untracked result file would make the task worktree dirty for postflight; its
            // content is now an artifact.
            if git(
                &worktree,
                &["status", "--porcelain", "--", &execution.result_path],
            )?
            .starts_with("??")
            {
                std::fs::remove_file(&run.result_path)?;
            }
        }
        for (kind, path) in [
            ("artifact_stdout.log", &run.job.stdout_path),
            ("artifact_stderr.log", &run.job.stderr_path),
        ] {
            let path = Path::new(path);
            if !path.as_os_str().is_empty() && path.exists() {
                self.job_artifact(
                    project,
                    direction,
                    kind,
                    "text/plain",
                    path,
                    "negative_evidence",
                )?;
            }
        }
        let state = string(serde_json::to_value(run.job.state).ok().as_ref());
        self.emit(&event(
            &project_id,
            direction,
            "direction.baseline_evaluated",
            json!({ "jobId": run.job_id, "state": state, "exitCode": run.job.exit_code, "metrics": run.metrics, "metricArtifactId": metric_artifact_id, "failure": run.failure }),
        ))?;
        Ok(Some(DaemonEvaluation {
            job_id: run.job_id,
            state,
            exit_code: run.job.exit_code,
            display_command,
            metrics: run.metrics,
            metric_artifact_id,
            failure: run.failure,
        }))
    }

    fn job_artifact(
        &self,
        project: &Value,
        direction: &Value,
        kind: &str,
        media_type: &str,
        source_path: &Path,
        retention_class: &str,
    ) -> Result<String> {
        let project_id = string(project.get("projectId"));
        let artifact_id = create_id(IdPrefix::Art);
        let input = json!({ "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": media_type, "sourcePath": source_path.to_string_lossy(), "retentionClass": retention_class });
        let artifact =
            ArtifactStore::open(&repository_root(project).join(".nosh").join("artifacts"))?
                .add(input.as_object().expect("an Artifact input is an object"))?;
        let value = &direction["value"];
        let record = json!({ "$schema": schema_uri("artifact"), "schemaVersion": 1, "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": media_type, "contentHash": artifact["contentHash"], "sizeBytes": artifact["sizeBytes"], "version": artifact["version"], "producer": { "type": "noshd" }, "scope": { "missionId": value["missionId"], "directionId": direction["entityId"], "autoresearchId": null, "experimentId": null }, "git": null, "evaluationContractHash": value["evaluationContractHash"], "retentionClass": retention_class, "remotePreviewPolicy": "encrypted_allowed", "redactionStatus": "checked", "createdAt": artifact["createdAt"] });
        self.deps.research.submit_daemon_record(
            &project_id,
            &scope(&project_id, direction, None),
            &format!("artifact:{artifact_id}"),
            &record,
            &format!("direction-job-artifact:{artifact_id}"),
        )?;
        Ok(artifact_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_review(
        &self,
        project_id: &str,
        direction: &Value,
        node_id: &str,
        task_id: &str,
        reviewer_id: &str,
        commit: Option<&str>,
        request: &Value,
    ) -> Result<()> {
        let research = &self.deps.research;
        let review = research.terminal_record(project_id, task_id, Some("review-verdict"))?;
        if let Some(review) = &review
            && review_passes_request(review, request, reviewer_id)
        {
            let direction = self.transition_node(
                project_id,
                direction,
                node_id,
                "accepted",
                &format!("direction-accepted-{task_id}"),
                None,
            )?;
            let review_id = review["reviewId"].as_str().filter(|id| !id.is_empty());
            if !truthy(direction["value"].get("acceptedBaseline"))
                && let (Some(commit), Some(review_id)) =
                    (commit.filter(|commit| !commit.is_empty()), review_id)
            {
                research.accept_direction_baseline(
                    project_id,
                    &string(direction.get("entityId")),
                    version(&direction),
                    &BaselineInput {
                        commit: commit.to_owned(),
                        review_id: review_id.to_owned(),
                        evaluation_contract_hash: string(
                            direction["value"].get("evaluationContractHash"),
                        ),
                        idempotency_key: format!("direction-baseline-{task_id}"),
                    },
                )?;
            }
            return Ok(());
        }
        if let Some(review) = &review
            && review["verdict"] == "BLOCKED"
            && strict_equal(review.get("reviewId"), request.get("reviewId"))
            && strict_equal(
                review.get("reviewRequestId"),
                request.get("reviewRequestId"),
            )
        {
            self.transition_node(
                project_id,
                direction,
                node_id,
                "blocked",
                &format!("direction-review-blocked-{task_id}"),
                None,
            )?;
            return Ok(());
        }
        self.fail(
            project_id,
            &string(direction.get("entityId")),
            node_id,
            task_id,
        )
    }

    fn issue_task_packet(
        &self,
        project: &Value,
        direction: &Value,
        node: &Value,
        assigned: &Assignment<'_>,
    ) -> Result<Value> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let direction_id = string(direction.get("entityId"));
        let root = repository_root(project);
        let value = &direction["value"];
        let node_id = string(node.get("id"));
        let title = string(node.get("title"));
        let role = assigned.role;
        // A Mission-spawned Direction works from the commit its baseline was measured at; a user
        // Direction from the checkout.
        let head = match value["baseCommit"].as_str() {
            Some(commit) => commit.to_owned(),
            None => git(&root, &["rev-parse", "HEAD"])?,
        };
        let branch = git(&root, &["branch", "--show-current"])?;
        if branch.is_empty() || !git(&root, &["status", "--porcelain"])?.is_empty() {
            return rejected("Direction Task preflight requires a clean checked-out Git branch");
        }
        // A retried worker builds on its node's last postflight-passed attempt instead of
        // discarding that work.
        let events: Vec<Value> = research
            .events_after(&project_id, 0)?
            .into_iter()
            .filter(|entry| entry["scope"]["directionId"] == direction_id.as_str())
            .collect();
        let resume = if role == Role::GeneralWorker {
            resume_point(research, project, &events, &node_id, &head)?
        } else {
            None
        };
        let resumed = match &resume {
            Some(ResumePoint::Resume { commit, .. }) => Some(commit.clone()),
            Some(ResumePoint::Skipped {
                task_id,
                commit,
                reason,
            }) => {
                self.emit(&event(
                    &project_id,
                    direction,
                    "task.resume_skipped",
                    json!({ "taskId": assigned.task_id, "nodeId": node_id, "priorTaskId": task_id, "commit": commit, "reason": reason }),
                ))?;
                None
            }
            None => None,
        };
        let tree = create_task_worktree(
            research,
            project,
            assigned.task_id,
            &head,
            Some(resumed.as_deref().unwrap_or(&head)),
        )?;
        let library_network =
            role == Role::LibrarianResearcher && librarian_network_allowed(project);
        let commands = if role == Role::GeneralWorker {
            declared_task_commands(project)
        } else {
            Vec::new()
        };
        let mut tools = vec![
            if role == Role::Reviewer {
                "tool_nosh.review.submit"
            } else {
                "tool_nosh.response.submit"
            },
            "tool_nosh.blocker.submit",
        ];
        if role != Role::Reviewer {
            tools.push("tool_nosh.delegation.request");
        }
        tools.push("tool_pi.read");
        if role != Role::Reviewer {
            tools.extend(["tool_pi.edit", "tool_pi.write"]);
        }
        if role == Role::GeneralWorker {
            tools.push("tool_nosh.git.commit");
        }
        if library_network {
            tools.push("tool_nosh.network.read");
        }
        if !commands.is_empty() {
            tools.push("tool_nosh.run");
        }
        let permissions = json!({
            "network": if library_network { "allowlisted" } else { "disabled" },
            "subprocess": if commands.is_empty() { "disabled" } else { "allowlisted" },
            "gitCommit": role == Role::GeneralWorker,
            "gitPush": false,
            "delegation": "request_only",
            "networkAllowlist": if library_network { LIBRARIAN_RESEARCH_HOSTS } else { &[] },
            "allowedToolIds": tools,
        });
        let mut instructions = vec![
            format!("Complete only {title}"),
            "Preserve the frozen evaluation contract".to_owned(),
        ];
        if commands.is_empty() {
            instructions.push("You cannot run commands: report only what you verified by reading, and claim a criterion satisfied only as far as that evidence supports".to_owned());
        } else {
            instructions.extend(task_command_instructions(&commands, true));
        }
        instructions.extend(prior_attempt_instructions(&events, &node_id));
        if let Some(commit) = &resumed {
            instructions.push(resume_instruction(commit));
        }
        let output_kind = match role {
            Role::GeneralWorker => "output_code.commit",
            Role::LibrarianResearcher => "output_research.report",
            _ => "output_review.verdict",
        };
        let attempt = number(&node["attempt"]);
        let mission_criteria: Vec<&Value> = value
            .get("missionId")
            .filter(|id| truthy(Some(id)))
            .into_iter()
            .collect();
        let mut packet = json!({
            "$schema": schema_uri("task-packet"), "schemaVersion": 1, "templateVersion": "1.0.0",
            "taskId": assigned.task_id, "attempt": attempt, "taskType": format!("task_{}", string(node.get("type")).replace('_', ".")), "assignedRole": role.as_str(), "assignedAgentId": assigned.agent_id,
            "scope": { "projectId": project_id, "missionId": value["missionId"], "directionId": direction_id, "autoresearchId": null, "experimentId": null, "graphNodeId": node_id },
            "goalStack": { "projectGoalId": project_id, "projectGoalSummary": value["decisionUse"], "missionCriterionIds": mission_criteria, "missionCriterionSummary": value["decisionUse"], "directionQuestionId": format!("question_{}", from_index(&direction_id, 4)), "directionQuestionSummary": value["question"], "nodeObjective": title },
            "whyNow": "The deterministic Direction frontier selected this ready node.",
            "instructions": instructions,
            "inScope": ["**"], "outOfScope": [".nosh/contracts/**"], "inputArtifactIds": [], "inputEvidenceIds": [],
            "requiredOutputs": [{ "outputId": format!("output_{}", from_index(assigned.task_id, 4)), "kind": output_kind, "required": true }],
            "acceptanceCriteria": [{ "criterionId": node_id, "statement": title, "validatorIds": ["validator_direction.postflight"], "reviewRubricIds": ["rubric_scientific.correctness"], "required": true }],
            "workspace": { "worktreeId": tree.worktree_id, "branch": tree.branch, "startingCommit": head, "writeScopes": ["**"], "protectedScopes": [".nosh/contracts/**"] },
            "permissions": permissions,
            "budget": { "deadline": assigned.expires_at, "maximumWallClockSeconds": 7_200, "maximumModelTokens": 60_000, "maximumToolCalls": 100, "maximumRepairAttempts": nosh_wire::js::max(0.0, number(&node["maximumAttempts"]) - attempt) },
            "progressPolicy": { "milestoneIntervalSeconds": 900, "emitOnFirstDurableDelta": true, "emitOnBlocker": true, "emitOnAnomaly": true, "maximumSilentSeconds": 1_200 },
            "responseSchema": schema_uri(assigned.response_schema),
            "observedVersions": { "directionGraphVersion": value["graphVersion"], "directionProjectionVersion": version(direction) },
            "lease": { "leaseId": assigned.task_id, "expiresAt": assigned.expires_at, "heartbeatSeconds": 15 }, "issuedBy": "research_director", "issuedAt": now_utc(),
        });
        normalize_numbers(&mut packet);
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, direction, Some(assigned.agent_id)),
            &format!("task:{}", assigned.task_id),
            &packet,
            &format!("direction-task-packet:{}", assigned.task_id),
        )?;
        Ok(packet)
    }

    fn issue_review_request(
        &self,
        project: &Value,
        direction: &Value,
        spec: ReviewSpec<'_>,
    ) -> Result<Value> {
        let project_id = string(project.get("projectId"));
        let direction_id = string(direction.get("entityId"));
        let value = &direction["value"];
        let producers = unique(spec.producer_agent_ids);
        if producers.is_empty() || producers.iter().any(|id| id == spec.reviewer_agent_id) {
            return rejected("Direction Review requires an independent producer");
        }
        let review_id = create_id(IdPrefix::Rev);
        let validator = if spec.review_type == "direction_closure" {
            "validator_direction.closure"
        } else {
            "validator_direction.postflight"
        };
        let mut request = json!({
            "$schema": schema_uri("review-request"), "schemaVersion": 1, "reviewRequestId": format!("request_{}", from_index(&review_id, 4)), "reviewId": review_id, "reviewType": spec.review_type,
            "target": { "targetType": spec.target_type, "targetId": spec.target_id, "targetVersion": spec.target_version },
            "scope": { "projectId": project_id, "missionId": value["missionId"], "directionId": direction_id, "autoresearchId": null },
            "producerAgentIds": producers, "reviewerAgentId": spec.reviewer_agent_id, "independenceCheck": "pass",
            "contractRefs": [{ "kind": "contract_evaluation", "id": format!("contract_{}", from_index(&direction_id, 4)), "hash": value["evaluationContractHash"] }],
            "requiredArtifactIds": unique(spec.artifacts), "requiredEvidenceIds": unique(spec.evidence),
            "deterministicPostflight": { "status": "pass", "validatorRunIds": [validator] },
            "criteria": [{ "criterionId": spec.criterion_id, "statement": spec.statement, "required": true, "severityIfFailed": "blocking" }],
            "allowedVerdicts": ["PASS", "REVISE", "REDESIGN", "BLOCKED"], "issuedAt": now_utc(),
        });
        normalize_numbers(&mut request);
        self.deps.research.submit_daemon_record(
            &project_id,
            &scope(&project_id, direction, Some(spec.reviewer_agent_id)),
            &format!("task:{}", spec.task_id),
            &request,
            &format!("direction-review-request:{}", spec.task_id),
        )?;
        Ok(request)
    }

    fn fail(
        &self,
        project_id: &str,
        direction_id: &str,
        node_id: &str,
        task_id: &str,
    ) -> Result<()> {
        let mut direction = self.deps.research.direction(project_id, direction_id)?;
        if matches!(node_state(&direction, node_id), "working" | "reviewing") {
            direction = self.transition_node(
                project_id,
                &direction,
                node_id,
                "failed",
                &format!("direction-failed-{task_id}"),
                None,
            )?;
        }
        let current = node(&direction, node_id).cloned().unwrap_or_default();
        if number(&current["attempt"]) < number(&current["maximumAttempts"]) {
            self.transition_node(
                project_id,
                &direction,
                node_id,
                "ready",
                &format!("direction-retry-{task_id}"),
                None,
            )?;
        } else {
            self.transition(
                project_id,
                &direction,
                "blocked",
                &format!("direction-attempts-exhausted-{task_id}"),
            )?;
        }
        Ok(())
    }

    async fn close(&self, project: &Value, mut direction: Value) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let id = string(direction.get("entityId"));
        direction = self.transition(
            &project_id,
            &direction,
            "reviewing",
            &format!("direction-final-{}", create_id(IdPrefix::Cmd)),
        )?;
        let value = direction["value"].clone();
        let reviewer_id = create_id(IdPrefix::Agt);
        let task_id = create_id(IdPrefix::Tsk);
        let mut producers: Vec<String> = research
            .records(&project_id, Some("task-packet"))?
            .iter()
            .filter(|packet| {
                packet["scope"]["directionId"] == id.as_str()
                    && packet["assignedRole"] != "reviewer"
            })
            .map(|packet| string(packet.get("assignedAgentId")))
            .collect();
        if producers.is_empty() {
            producers = research
                .records(&project_id, Some("research-director-cycle"))?
                .iter()
                .filter(|cycle| {
                    cycle["directionId"] == id.as_str() && truthy(cycle.get("directorAgentId"))
                })
                .map(|cycle| string(cycle.get("directorAgentId")))
                .collect();
        }
        let scoped: Vec<Value> = research
            .submitted(&project_id, None)?
            .into_iter()
            .filter(|entry| entry["scope"]["directionId"] == id.as_str())
            .map(|mut entry| entry["payload"].take())
            .collect();
        let of = |schema: &str| -> Vec<&Value> {
            let uri = schema_uri(schema);
            scoped
                .iter()
                .filter(|record| record["$schema"] == uri.as_str())
                .collect()
        };
        let present = |records: Vec<&Value>, key: &str| -> Vec<String> {
            records
                .into_iter()
                .filter(|record| truthy(record.get(key)))
                .map(|record| string(record.get(key)))
                .collect()
        };
        let evidence = present(of("evidence"), "evidenceId");
        let results: Vec<Value> = of("experiment-result").into_iter().cloned().collect();
        let experiments = |keep: &dyn Fn(&Value) -> bool| -> Vec<String> {
            unique(
                results
                    .iter()
                    .filter(|result| keep(result))
                    .map(|result| string(result.get("experimentId"))),
            )
        };
        let accepted = experiments(&|result| result["promotionDecision"] == "promoted");
        let rejected_ids = experiments(&|result| result["promotionDecision"] == "rejected");
        // "held" = met the keep rule and passed experiment Review but no round Review promoted it:
        // neither accepted nor refuted.
        let held = experiments(&|result| {
            result["promotionDecision"] == "held"
                && !accepted.contains(&string(result.get("experimentId")))
        });
        let inconclusive = experiments(&|result| {
            let experiment = string(result.get("experimentId"));
            (result["deterministicValidation"] != "pass" && !rejected_ids.contains(&experiment))
                || held.contains(&experiment)
        });
        let completion_packets =
            present(of("autoresearch-completion-packet"), "completionPacketId");
        let synthesis_artifact_id = self.closure_artifact(
            project,
            &direction,
            "synthesis",
            &json!({ "question": value["question"], "decisionUse": value["decisionUse"], "accepted": accepted, "rejected": rejected_ids, "inconclusive": inconclusive, "evidence": evidence }),
        )?;
        let reproducibility_artifact_id = self.closure_artifact(
            project,
            &direction,
            "reproducibility",
            &json!({ "evaluationContractHash": value["evaluationContractHash"], "baselineCommit": value["acceptedBaseline"]["commit"], "graphVersion": value["graphVersion"], "gitHead": git(&repository_root(project), &["rev-parse", "HEAD"])? }),
        )?;
        let closure_attempt = of("direction-closure-packet").len() + 1;
        let disposition = if !accepted.is_empty() {
            "supported"
        } else if !inconclusive.is_empty() || rejected_ids.is_empty() {
            "inconclusive"
        } else {
            "refuted"
        };
        let answer = if !accepted.is_empty() {
            accepted_closure_answer(&accepted, &results)
        } else if !held.is_empty() {
            format!(
                "No experiment was promoted, so adoption is not established. {} met the keep rule and passed experiment Review but no round Review promoted {}{}.",
                held.join(", "),
                if held.len() == 1 { "it" } else { "them" },
                if rejected_ids.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} {} rejected",
                        rejected_ids.join(", "),
                        if rejected_ids.len() == 1 {
                            "was"
                        } else {
                            "were"
                        }
                    )
                }
            )
        } else if !rejected_ids.is_empty() && inconclusive.is_empty() {
            format!(
                "Every tested experiment ({}) was rejected under the frozen contract and its guardrails; the bounded change is not supported.",
                rejected_ids.join(", ")
            )
        } else {
            format!(
                "No experiment produced an accepted result{}.",
                if inconclusive.is_empty() {
                    String::new()
                } else {
                    format!("; {} remained inconclusive", inconclusive.join(", "))
                }
            )
        };
        let limitations = if accepted.is_empty() {
            json!(["No reviewed promoted Autoresearch experiment was attached"])
        } else {
            json!([
                "Selection used only the frozen contract's evaluation data; a held-out evaluation is still required before adoption."
            ])
        };
        let fingerprints: Vec<String> = rejected_ids
            .iter()
            .map(|experiment| format!("hypothesis_{}", from_index(experiment, 4)))
            .collect();
        let mut closure = json!({
            "$schema": schema_uri("direction-closure-packet"), "schemaVersion": 1, "directionId": id, "graphVersion": value["graphVersion"], "closureAttempt": closure_attempt, "evaluationContractHash": value["evaluationContractHash"],
            "questionDisposition": disposition, "answer": answer,
            "acceptedExperimentIds": accepted, "rejectedExperimentIds": rejected_ids, "inconclusiveExperimentIds": inconclusive, "autoresearchCompletionPacketIds": completion_packets, "evidenceIds": evidence,
            "claimEffects": [], "failedHypothesisFingerprintIds": fingerprints, "limitations": limitations, "openDefectIds": [],
            "synthesisArtifactId": synthesis_artifact_id, "reproducibilityManifestArtifactId": reproducibility_artifact_id,
            "deterministicPreReview": { "status": "pass", "validatorRunIds": ["validator_direction.closure"] }, "builtAt": now_utc(),
        });
        normalize_numbers(&mut closure);
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, &direction, None),
            &format!("direction:{id}"),
            &closure,
            &format!("direction-closure:{id}:{closure_attempt}"),
        )?;
        let request = self.issue_review_request(
            project,
            &direction,
            ReviewSpec {
                task_id: &task_id,
                reviewer_agent_id: &reviewer_id,
                producer_agent_ids: producers,
                review_type: "direction_closure",
                target_type: "direction_record",
                target_id: &id,
                target_version: version(&direction),
                criterion_id: &id,
                statement: format!("Closure fidelity for the decision \"{}\": the packet's questionDisposition and answer follow from the attached results and Evidence under the frozen contract and its guardrails. An honestly inconclusive or refuted answer passes; an overstated, understated, or unsupported one fails.", slice_to(&string(value.get("decisionUse")), 600)),
                artifacts: vec![synthesis_artifact_id, reproducibility_artifact_id],
                evidence,
            },
        )?;
        let (_, outcomes) = direction_autoresearch_outcomes(research, &project_id, &id)?;
        let prompt = format!(
            "Perform the independent final closure Review for Direction {id}. Audit this canonical closure packet: {}. {}Review Request: {}. {}",
            stringify(&closure),
            if outcomes.is_empty() {
                String::new()
            } else {
                format!("Resolved Autoresearch outcomes (daemon-authored): {outcomes}. ")
            },
            stringify(&request),
            reviewer_instructions(&request, &self.deps.schema_dir)
        );
        let session = Session {
            agent_id: &reviewer_id,
            task_id: &task_id,
            role: Role::Reviewer,
            permissions: None,
            workspace: None,
        };
        self.run_session(project, &direction, session, &prompt)
            .await?;
        let review = research.terminal_record(&project_id, &task_id, Some("review-verdict"))?;
        if !review.is_some_and(|review| review_passes_request(&review, &request, &reviewer_id)) {
            return rejected("Direction final Review did not pass");
        }
        self.transition(
            &project_id,
            &direction,
            "closed",
            &format!("direction-closed-{task_id}"),
        )?;
        Ok(())
    }

    /// Writes a daemon-authored Direction file under a durable intent and records it as an
    /// Artifact.
    fn closure_artifact(
        &self,
        project: &Value,
        direction: &Value,
        name: &str,
        value: &Value,
    ) -> Result<String> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let direction_id = string(direction.get("entityId"));
        let root = repository_root(project);
        let directory = root.join(".nosh").join("direction").join(&direction_id);
        let path = directory.join(format!("{name}.json"));
        let path_text = path.to_string_lossy().into_owned();
        let content = format!("{}\n", canonical_json(value));
        let digest = sha256(&json!(content));
        let file_digest =
            |path: &Path| -> Result<String> { Ok(sha256(&json!(std::fs::read_to_string(path)?))) };
        let write = research.begin_external_operation(
            &project_id,
            "filesystem.write",
            &format!("direction-file:{direction_id}:{name}:{}", &digest[7..23]),
            &json!({ "path": path_text, "content": content, "digest": digest }),
        )?;
        std::fs::create_dir_all(&directory)?;
        if !path.exists() || file_digest(&path)? != digest {
            let temporary =
                PathBuf::from(format!("{path_text}.{}.tmp", string(write.get("intentId"))));
            std::fs::write(&temporary, &content)?;
            std::fs::rename(&temporary, &path)?;
        }
        if file_digest(&path)? != digest {
            return rejected("Direction closure file verification failed");
        }
        research.complete_external_operation(
            &project_id,
            &string(write.get("intentId")),
            &json!({ "path": path_text, "digest": digest }),
        )?;
        let artifact_id = format!(
            "art_{}",
            &sha256(&json!({ "directionId": direction_id, "name": name, "digest": digest }))[7..39]
        );
        let copy = research.begin_external_operation(
            &project_id,
            "artifact.copy",
            &format!("direction-artifact:{artifact_id}"),
            &json!({ "artifactId": artifact_id, "path": path_text, "digest": digest }),
        )?;
        let kind = format!("artifact_direction.{name}");
        let input = json!({ "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": "application/json", "sourcePath": path_text, "retentionClass": "accepted_evidence" });
        let artifact = ArtifactStore::open(&root.join(".nosh").join("artifacts"))?
            .add(input.as_object().expect("an Artifact input is an object"))?;
        research.complete_external_operation(
            &project_id,
            &string(copy.get("intentId")),
            &json!({ "artifactId": artifact_id, "contentHash": artifact["contentHash"], "version": artifact["version"] }),
        )?;
        let scope_value = &direction["value"];
        let record = json!({ "$schema": schema_uri("artifact"), "schemaVersion": 1, "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": "application/json", "contentHash": artifact["contentHash"], "sizeBytes": artifact["sizeBytes"], "version": artifact["version"], "producer": { "type": "noshd" }, "scope": { "missionId": scope_value["missionId"], "directionId": direction_id, "autoresearchId": null, "experimentId": null }, "git": null, "evaluationContractHash": scope_value["evaluationContractHash"], "retentionClass": "accepted_evidence", "remotePreviewPolicy": "encrypted_allowed", "redactionStatus": "checked", "createdAt": artifact["createdAt"] });
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, direction, None),
            &format!("artifact:{artifact_id}"),
            &record,
            &format!("direction-artifact-record:{artifact_id}"),
        )?;
        Ok(artifact_id)
    }

    async fn run_session(
        &self,
        project: &Value,
        direction: &Value,
        session: Session<'_>,
        prompt: &str,
    ) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let mission_id = direction["value"]["missionId"].as_str().map(str::to_owned);
        let direction_id = string(direction.get("entityId"));
        let Some(runtime) = &self.deps.runtime else {
            let root = repository_root(project);
            let cwd = match &session.workspace {
                Some(workspace) => task_worktree_path(&root, &workspace.worktree_id),
                None => root,
            };
            let options = SessionOptions {
                project_id,
                mission_id,
                direction_id: Some(direction_id),
                autoresearch_id: None,
                experiment_id: None,
                run_id: None,
                job_id: None,
                task_id: Some(session.task_id.to_owned()),
                agent_id: session.agent_id.to_owned(),
                role: session.role,
                cwd,
                package_path: self.deps.package_path.clone(),
                tools: None,
                runtime_scoped: false,
                task_permissions: session.permissions,
                task_workspace: session.workspace,
                thinking_level: None,
                model: None,
            };
            self.deps
                .agents
                .start(options)
                .await
                .map_err(Error::Rejected)?;
            let prompted = self
                .deps
                .agents
                .prompt(session.agent_id, prompt, None)
                .await;
            self.deps.agents.stop(session.agent_id);
            return prompted.map_err(Error::Rejected);
        };
        let thread_id = create_id(IdPrefix::Thr);
        let issued_at = now_utc();
        let key = format!("runtime:{}:{thread_id}", session.task_id);
        let instruction = |operation: Value| {
            let mut instruction = json!({ "$schema": schema_uri("runtime-instruction"), "schemaVersion": 1, "instructionId": create_id(IdPrefix::Ins), "projectId": project_id, "proposedByAgentId": null, "issuedAt": issued_at, "threadId": thread_id });
            if let (Some(target), Value::Object(fields)) = (instruction.as_object_mut(), operation)
            {
                target.extend(fields);
            }
            instruction
        };
        runtime
            .execute(&instruction(json!({
                "idempotencyKey": format!("{key}:open"), "operation": "THREAD_OPEN", "taskId": session.task_id, "initialAgentId": session.agent_id,
                "ownerScope": { "missionId": mission_id, "directionId": direction_id, "autoresearchId": null, "experimentId": null, "graphNodeId": null },
                "role": session.role.as_str(), "purpose": format!("Execute Direction task {}", session.task_id), "executionMode": "background", "parentThreadId": null, "inputRefs": [], "skillIds": [], "capabilities": [],
                "taskPermissions": session.permissions, "taskWorkspace": session.workspace,
                "budget": { "maximumToolCalls": 200, "maximumModelTokens": 100_000, "maximumWallClockSeconds": 7_200 },
            })))
            .await?;
        let stepped = runtime
            .execute(&instruction(json!({ "idempotencyKey": format!("{key}:step"), "operation": "THREAD_STEP", "objective": prompt, "expectedEpisodeType": episode_type_for_role(session.role.as_str()), "inputRefs": [], "skillIds": [] })))
            .await;
        runtime
            .execute(&instruction(json!({ "idempotencyKey": format!("{key}:stop"), "issuedAt": now_utc(), "operation": "STOP", "programId": null, "reason": "Task session reached its boundary" })))
            .await?;
        stepped.map(drop)
    }
}

/// `${evaluation.exitCode}`: `null` when the Job never exited.
fn exit_code(evaluation: &DaemonEvaluation) -> String {
    evaluation
        .exit_code
        .map_or_else(|| "null".to_owned(), |code| code.to_string())
}
