//! Port of `apps/noshd/src/mission-supervisor.ts`: runs each running Mission one node at a time.
//! The Mission Director's typed cycle record confirms the scheduler's selected node; the daemon
//! issues the Task Packet, runs the worker in its own worktree, checks it deterministically
//! (postflight), and has an independent reviewer judge it. Accepted commits merge onto the
//! Mission's integration branch; a completed graph goes to the final Mission review. Spawn nodes
//! start research children instead (see `research_spawner`).
//!
//! Divergences: prompt-embedded JSON has sorted keys; the digest replaces its slot literally,
//! where JavaScript's `String.replace` would expand `$&`-style patterns in it.

use super::{
    Busy, Dependencies, completion_refs, daemon_git, from_packet, git, iso_in, node, node_state,
    nodes, number, postflight, repository_root, review_passes_request, strings, unique, version,
};
use crate::daemon::{error_text, event_draft, js_trim, spawn_eager};
use crate::director_brief::{SpawnBriefContext, director_retry_allowed, mission_brief};
use crate::research_spawner::{
    CHILD_BLOCKED_MS, ChildBudget, MissionChild, Probe, allocate_budget, freeze_contract,
    probe_evaluation, spawn_issues, spawn_mission_child,
};
use crate::runtime::{now_ms, parse_ms};
use crate::task_postflight::{completion_run_issues, write_up_only};
use crate::task_templates::{
    completion_instructions, general_worker_completion_template, librarian_instructions,
    reviewer_instructions, stringify,
};
use crate::task_worktree::{
    ResumePoint, create_task_worktree, resume_instruction, resume_point, task_worktree_path,
};
use crate::{Error, Result, rejected};
use nosh_agent::pi::Inspection;
use nosh_agent::policy::{Role, SessionOptions, TaskPermissions, TaskWorkspace};
use nosh_research::artifacts::ArtifactStore;
use nosh_research::graph::{GraphNode, VersionedDag};
use nosh_research::research_control::{
    Host, LIBRARIAN_RESEARCH_HOSTS, SPAWN_NODE_TYPES, daemon_task_facts, declared_task_commands,
    librarian_network_allowed, mission_outcomes, prior_attempt_instructions,
    task_command_instructions, task_command_runs,
};
use nosh_research::scheduler::{FocusGovernor, Scheduler, SchedulingContext};
use nosh_sys::git::GitWorkspaceManager;
use nosh_wire::js::{length, number_string, parse_number, slice_to, strict_equal, string, truthy};
use nosh_wire::{
    IdPrefix, canonical_json, create_id, episode_type_for_role, normalize_numbers, now_utc,
    schema_uri, sha256,
};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// What a failed attempt produced, for the focus governor.
enum Failure {
    /// A session that never delivered a result: a transient retry.
    Transient,
    /// The changed commit and the issues or review that rejected it, so only a genuinely repeated
    /// failure alarms.
    Rejected {
        diff_hash: Option<String>,
        evidence: Value,
    },
}

/// One task session's identity and authority.
struct Session<'a> {
    agent_id: &'a str,
    task_id: &'a str,
    mission_id: &'a str,
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
    target: Value,
    criterion_id: &'a str,
    criterion: &'a str,
    contract_refs: Vec<Value>,
    required_artifact_ids: Vec<String>,
    required_evidence_ids: Vec<String>,
}

/// Agents of the Mission itself, not of the Directions and Autoresearch it spawned.
fn own_agent(agent: &Inspection, mission_id: &str) -> bool {
    agent.mission_id.as_deref() == Some(mission_id)
        && agent.direction_id.as_deref().is_none_or(str::is_empty)
        && agent.autoresearch_id.as_deref().is_none_or(str::is_empty)
}

/// A spawn node leased to the research child it started.
fn child_owned(node: &Value) -> bool {
    let owner = node["lease"]["ownerId"].as_str().unwrap_or_default();
    SPAWN_NODE_TYPES.contains(&node["type"].as_str().unwrap_or_default())
        && (owner.starts_with("dir_") || owner.starts_with("ar_"))
}

fn changed_commit(completion: &Value) -> Option<String> {
    let code = &completion["codeChanges"];
    let ending = code["endingCommit"]
        .as_str()
        .filter(|ending| !ending.is_empty())?;
    (!strict_equal(code.get("endingCommit"), code.get("startingCommit"))).then(|| ending.to_owned())
}

fn role_for(node_type: &str) -> Role {
    match node_type {
        "literature_review" => Role::LibrarianResearcher,
        "claim_review" | "final_review" | "approval" => Role::Reviewer,
        _ => Role::GeneralWorker,
    }
}

fn event(project_id: &str, mission_id: &str, kind: &str, payload: Value) -> Value {
    event_draft(
        kind,
        "mission_supervisor",
        scope(project_id, mission_id, None),
        json!(mission_id),
        Value::Null,
        payload,
    )
}

fn scope(project_id: &str, mission_id: &str, agent_id: Option<&str>) -> Value {
    json!({ "projectId": project_id, "missionId": mission_id, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": agent_id })
}

/// The Mission's own integration branch; the user merges it when the Mission is accepted.
fn integration_branch(mission_id: &str) -> String {
    format!("nosh/mission-{}", &mission_id[4..16])
}

/// A runtime step objective holds at most this many characters (wire `text`); the accepted-work
/// digest is the part that grows.
const OBJECTIVE_LIMIT: isize = 20_000;
pub const DIGEST_SLOT: &str = " DIGEST ";

/// Puts as much of the accepted-work digest into the prompt as fits the step objective limit (the
/// digest is already a bounded excerpt).
pub fn fit_digest(prompt: &str, digest: &str) -> String {
    if !prompt.contains(DIGEST_SLOT) {
        return prompt.to_owned();
    }
    let room = OBJECTIVE_LIMIT - 200 - (length(prompt) as isize - length(DIGEST_SLOT) as isize);
    let fitted = if length(digest) as isize <= room {
        digest.to_owned()
    } else {
        format!(
            "{}… [digest truncated to fit; read the required Artifacts]",
            slice_to(digest, (room - 60).max(0) as usize)
        )
    };
    prompt.replacen(DIGEST_SLOT, &fitted, 1)
}

/// The keys of `value` that are present, as JavaScript's `{ a: value.a, … }` serializes.
fn pick(value: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                value
                    .get(*key)
                    .map(|item| ((*key).to_owned(), item.clone()))
            })
            .collect(),
    )
}

/// UTF-16 order, as `Array.prototype.sort` compares strings.
fn js_sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    items
}

pub struct MissionSupervisor<H: Host> {
    deps: Dependencies<H>,
    busy: RefCell<HashSet<String>>,
    scheduler: RefCell<Scheduler>,
    timer: RefCell<Option<JoinHandle<()>>>,
}

impl<H: Host + 'static> MissionSupervisor<H> {
    pub fn new(deps: Dependencies<H>) -> Rc<Self> {
        Rc::new(MissionSupervisor {
            deps,
            busy: RefCell::default(),
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

    /// Starts a cycle for every running or pausing Mission; each runs on until its first wait.
    pub fn tick(self: &Rc<Self>) -> Result<()> {
        for project in (self.deps.projects)()? {
            let project_id = string(project.get("projectId"));
            for mission in self.deps.research.missions(&project_id)? {
                let mission_id = string(mission.get("entityId"));
                match mission["state"].as_str().unwrap_or_default() {
                    "completed" | "stopped" | "failed" => self
                        .scheduler
                        .borrow_mut()
                        .release_mission(&project_id, &mission_id),
                    "running" | "pausing" => {
                        let supervisor = Rc::clone(self);
                        let project = project.clone();
                        spawn_eager(async move {
                            let _ = supervisor.cycle(project, mission_id).await;
                        });
                    }
                    "reviewing" => self.recover_interrupted_review(&project_id, &mission_id)?,
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
        mission: &Value,
        next: &str,
        key: &str,
    ) -> Result<Value> {
        Ok(self.deps.research.transition_mission(
            project_id,
            &string(mission.get("entityId")),
            version(mission),
            next,
            key,
        )?)
    }

    fn transition_node(
        &self,
        project_id: &str,
        mission: &Value,
        node_id: &str,
        next: &str,
        key: &str,
        lease: Option<&Value>,
    ) -> Result<Value> {
        Ok(self.deps.research.transition_mission_node(
            project_id,
            &string(mission.get("entityId")),
            version(mission),
            node_id,
            next,
            key,
            lease,
            false,
        )?)
    }

    /// A final review cut short by a daemon restart leaves the Mission in reviewing, which no cycle
    /// handles; return it to running so the review runs again.
    fn recover_interrupted_review(&self, project_id: &str, mission_id: &str) -> Result<()> {
        if self.busy.borrow().contains(mission_id)
            || self
                .deps
                .agents
                .inspect()
                .iter()
                .any(|agent| own_agent(agent, mission_id))
        {
            return Ok(());
        }
        let mission = self.deps.research.mission(project_id, mission_id)?;
        if mission["state"] != "reviewing" {
            return Ok(());
        }
        let from = version(&mission);
        self.transition(
            project_id,
            &mission,
            "running",
            &format!("review-interrupted-{from}"),
        )?;
        self.emit(&event(
            project_id,
            mission_id,
            "mission.review_recovered",
            json!({ "fromVersion": from }),
        ))
    }

    async fn cycle(self: Rc<Self>, project: Value, mission_id: String) -> Result<()> {
        let project_id = string(project.get("projectId"));
        // A safe pause is reported as paused only once every Mission agent has actually stopped
        // at its boundary.
        let current = self.deps.research.mission(&project_id, &mission_id)?;
        if current["state"] == "pausing" {
            if !self.busy.borrow().contains(&mission_id)
                && !self
                    .deps
                    .agents
                    .inspect()
                    .iter()
                    .any(|agent| agent.mission_id.as_deref() == Some(mission_id.as_str()))
            {
                let at = version(&current);
                self.transition(
                    &project_id,
                    &current,
                    "paused",
                    &format!("mission-paused-at-boundary-{at}"),
                )?;
            }
            return Ok(());
        }
        let forked = match &self.deps.runtime {
            Some(runtime) => runtime.has_active_fork(&project_id, Some(&mission_id), None)?,
            None => false,
        };
        if self.busy.borrow().contains(&mission_id)
            || self
                .deps
                .agents
                .inspect()
                .iter()
                .any(|agent| own_agent(agent, &mission_id))
            || forked
        {
            return Ok(());
        }
        let _busy = Busy::hold(&self.busy, &mission_id);
        match self.advance(&project, &mission_id).await {
            Ok(()) => Ok(()),
            Err(error) => self.cycle_failed(&project_id, &mission_id, &error),
        }
    }

    async fn advance(&self, project: &Value, mission_id: &str) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let research = &self.deps.research;
        let mut mission = research.mission(&project_id, mission_id)?;
        if mission["state"] != "running" {
            return Ok(());
        }
        mission = self.reconcile_orphaned_nodes(&project_id, mission)?;
        if mission["state"] != "running" {
            return Ok(());
        }
        mission = self.reconcile_child_nodes(&project_id, mission)?;
        if mission["state"] != "running" {
            return Ok(());
        }
        self.scheduler
            .borrow_mut()
            .activate_mission(&project_id, mission_id)?;
        let allocations: Map<String, Value> = nodes(&mission)
            .iter()
            .map(|node| (string(node.get("id")), node["attempt"].clone()))
            .collect();
        self.scheduler
            .borrow_mut()
            .restore_allocations(&allocations);
        let usage = research.mission_budget_use(&project_id, mission_id)?;
        let budget = mission["value"]["budgets"].clone();
        let (used, limit) = (
            |key: &str| number(&usage[key]),
            |key: &str| number(&budget[key]),
        );
        let exhausted: Vec<&str> = [
            (used("modelTokens") >= limit("maximumModelTokens")).then_some("model_tokens"),
            (used("wallClockSeconds") >= limit("maximumWallClockSeconds")).then_some("wall_clock"),
            (used("gpuSeconds") > limit("maximumGpuSeconds")).then_some("gpu_seconds"),
            (used("diskBytes") >= limit("maximumDiskBytes")).then_some("disk_bytes"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !exhausted.is_empty() {
            self.emit(&event(
                &project_id,
                mission_id,
                "mission.budget_exhausted",
                json!({ "exhausted": exhausted, "use": usage, "limits": budget }),
            ))?;
            self.transition(
                &project_id,
                &mission,
                "blocked",
                &format!("budget-exhausted-{}", create_id(IdPrefix::Cmd)),
            )?;
            return Ok(());
        }
        let graph_nodes: Vec<GraphNode> = serde_json::from_value(mission["value"]["nodes"].clone())
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let graph = VersionedDag::new(
            mission_id,
            graph_nodes,
            "hydrated",
            mission["value"]["graphVersion"].as_i64().unwrap_or(0),
        )?;
        if graph.completion_ready() {
            return self.final_review(project, mission).await;
        }
        let own_agents = self
            .deps
            .agents
            .inspect()
            .iter()
            .filter(|agent| own_agent(agent, mission_id))
            .count();
        let context = SchedulingContext {
            paused: false,
            workspace_conflicts: HashSet::new(),
            available_roles: ["librarian_researcher", "general_worker", "reviewer"]
                .map(str::to_owned)
                .into(),
            available_agents: nosh_wire::js::max(
                0.0,
                limit("maximumConcurrentAgents") - own_agents as f64,
            ),
        };
        let frontier = self.scheduler.borrow().frontier(&graph, &context, now_ms());
        let Some(node) = frontier.into_iter().next() else {
            if !nodes(&mission).iter().any(|item| {
                matches!(
                    item["state"].as_str(),
                    Some("leased" | "working" | "postflight" | "reviewing")
                )
            }) {
                self.transition(
                    &project_id,
                    &mission,
                    "blocked",
                    &format!("supervisor-blocked-{}", create_id(IdPrefix::Cmd)),
                )?;
            }
            return Ok(());
        };
        if SPAWN_NODE_TYPES.contains(&node.node_type.as_str()) {
            return self.spawn_node(project, mission, &node).await;
        }
        let selected =
            serde_json::to_value(&node).map_err(|error| Error::Rejected(error.to_string()))?;
        self.director_cycle(project, &mission, &selected, None)
            .await?;
        mission = research.mission(&project_id, mission_id)?;
        if mission["state"] == "running" {
            self.run_node(project, mission, &node.id).await?;
        }
        Ok(())
    }

    fn cycle_failed(&self, project_id: &str, mission_id: &str, error: &Error) -> Result<()> {
        let research = &self.deps.research;
        let mut mission = research.mission(project_id, mission_id)?;
        // Work cut short by a user Stop is the expected effect of that Stop, not a supervisor
        // failure.
        if matches!(
            mission["state"].as_str(),
            Some("stopping" | "stopped" | "completed" | "failed")
        ) {
            return Ok(());
        }
        // An invalid cycle record is a model slip, not a stuck Mission: the next tick runs a fresh
        // cycle.
        if let Error::Slip(message) = error {
            let own: Vec<Value> = research
                .scoped_events(project_id, mission_id, None)?
                .into_iter()
                .filter(|entry| {
                    !truthy(entry["scope"].get("directionId"))
                        && !truthy(entry["scope"].get("autoresearchId"))
                })
                .collect();
            if director_retry_allowed(
                &own,
                &schema_uri("mission-director-cycle"),
                "mission.director_retry",
            ) {
                return self.emit(&event(
                    project_id,
                    mission_id,
                    "mission.director_retry",
                    json!({ "message": message }),
                ));
            }
        }
        self.emit(&event(
            project_id,
            mission_id,
            "mission.supervisor_error",
            json!({ "message": error_text(error, "Mission supervisor failed") }),
        ))?;
        if mission["state"] == "reviewing" {
            mission = self.transition(
                project_id,
                &mission,
                "running",
                &format!("supervisor-review-failed-{}", create_id(IdPrefix::Cmd)),
            )?;
        }
        if mission["state"] == "running" {
            self.transition(
                project_id,
                &mission,
                "blocked",
                &format!("supervisor-error-{}", create_id(IdPrefix::Cmd)),
            )?;
        }
        Ok(())
    }

    /// Runs the Mission Director on the selected node; returns the cycle record's spawn, if any.
    async fn director_cycle(
        &self,
        project: &Value,
        mission: &Value,
        node: &Value,
        spawn: Option<&SpawnBriefContext>,
    ) -> Result<Option<Value>> {
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        let agent_id = create_id(IdPrefix::Agt);
        let task_id = create_id(IdPrefix::Tsk);
        let brief = mission_brief(
            &self.deps.research,
            project,
            mission,
            node,
            &agent_id,
            spawn,
            &self.deps.schema_dir,
        )?;
        let session = Session {
            agent_id: &agent_id,
            task_id: &task_id,
            mission_id: &mission_id,
            role: Role::MissionDirector,
            permissions: None,
            workspace: None,
        };
        if let Err(error) = self.run_session(project, session, &brief.prompt).await {
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
            Some("mission-director-cycle"),
        )?;
        let Some(record) = record.filter(|record| {
            record["cycleId"] == brief.ids.cycle_id
                && record["directorAgentId"] == agent_id
                && record["missionId"] == mission_id
                && strict_equal(
                    record.get("observedGraphVersion"),
                    mission["value"].get("graphVersion"),
                )
        }) else {
            return Err(Error::Slip(
                "Mission Director did not submit the current cycle record".into(),
            ));
        };
        if record.pointer("/northStarCheck/currentWorkContributes") != Some(&Value::Bool(true)) {
            return rejected("Mission Director halted the selected node; user attention required");
        }
        Ok(record
            .get("spawn")
            .filter(|spawn| truthy(Some(spawn)))
            .cloned())
    }

    /// A research_direction or autoresearch node: the Mission Director specifies the child, the
    /// daemon probes and freezes its evaluation, then creates and starts it. The node stays
    /// working, leased to the child, until the child finishes.
    async fn spawn_node(
        &self,
        project: &Value,
        mut mission: Value,
        node: &GraphNode,
    ) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        let attempt = node.attempt + 1.0;
        let attempt_text = number_string(attempt);
        let key = format!("child-spawn:{mission_id}:{}:{attempt_text}", node.id);
        // A spawn interrupted by a restart resumes from its durable intent instead of asking the
        // Director again.
        let child_id = if let Some(prior) = research.external_operation(&project_id, &key)? {
            let request = &prior["request"];
            spawn_mission_child(
                research,
                &project_id,
                MissionChild {
                    mission: &mission,
                    node_id: &node.id,
                    attempt,
                    spawn: &request["spawn"],
                    evaluation_contract: &request["evaluationContract"],
                    base_commit: &string(request.get("baseCommit")),
                    budget: ChildBudget {
                        maximum_model_tokens: number(&request["budget"]["maximumModelTokens"]),
                        maximum_wall_clock_seconds: number(
                            &request["budget"]["maximumWallClockSeconds"],
                        ),
                    },
                },
            )?
        } else {
            let Some(job_runner) = &self.deps.job_runner else {
                return rejected("Spawning research children requires the daemon's job runner");
            };
            let allocation =
                match allocate_budget(research, &project_id, &mission, &node.node_type)? {
                    Ok(allocation) => allocation,
                    Err(reason) => {
                        self.emit(&event(
                            &project_id,
                            &mission_id,
                            "research.spawn_rejected",
                            json!({ "nodeId": node.id, "issues": [reason] }),
                        ))?;
                        self.transition_node(
                            &project_id,
                            &mission,
                            &node.id,
                            "blocked",
                            &format!("spawn-budget-{}-{attempt_text}", node.id),
                            None,
                        )?;
                        return Ok(());
                    }
                };
            let rejections: Vec<Value> = research
                .scoped_events(&project_id, &mission_id, Some("research.spawn_rejected"))?
                .into_iter()
                .filter(|entry| {
                    entry["payload"]["nodeId"] == node.id.as_str()
                        && entry["payload"]["attempt"].as_f64() == Some(attempt)
                })
                .collect();
            let context = SpawnBriefContext {
                commands: declared_task_commands(project)
                    .iter()
                    .map(|command| pick(command, &["commandId", "description", "argv"]))
                    .collect(),
                siblings: self.spawn_siblings(&project_id, &mission, &node.id)?,
                rejections: rejections
                    .iter()
                    .map(|entry| strings(&entry["payload"]["issues"]).join("; "))
                    .collect(),
            };
            let selected =
                serde_json::to_value(node).map_err(|error| Error::Rejected(error.to_string()))?;
            let spawn = self
                .director_cycle(project, &mission, &selected, Some(&context))
                .await?;
            mission = research.mission(&project_id, &mission_id)?;
            if mission["state"] != "running" {
                return Ok(());
            }
            let base_commit = self.integration_head(project, &mission_id)?;
            let issues = match &spawn {
                Some(spawn) => spawn_issues(project, spawn, &node.id, &node.node_type),
                None => vec!["the Mission Director did not fill spawn for this node".to_owned()],
            };
            let probe = match &spawn {
                Some(spawn) if issues.is_empty() => Some(
                    probe_evaluation(
                        research,
                        job_runner.as_ref(),
                        project,
                        &mission_id,
                        None,
                        spawn,
                        &base_commit,
                    )
                    .await?,
                ),
                _ => None,
            };
            let (
                Some(spawn),
                Some(Probe::Measured {
                    metrics,
                    execution,
                    probe_job_id,
                }),
            ) = (spawn, &probe)
            else {
                let (refused, metric_keys, written_json) = match probe {
                    Some(Probe::Refused {
                        issues,
                        metric_keys,
                        written_json,
                    }) => (issues, metric_keys, written_json),
                    _ => Default::default(),
                };
                self.emit(&event(
                    &project_id,
                    &mission_id,
                    "research.spawn_rejected",
                    json!({ "nodeId": node.id, "attempt": attempt, "issues": ([issues, refused].concat()), "metricKeys": metric_keys, "writtenJson": written_json }),
                ))?;
                // Repeated rejections cost the node one attempt, so a Director that cannot specify
                // a child does not loop forever.
                if rejections.len() + 1 >= 3 {
                    mission = research.mission(&project_id, &mission_id)?;
                    let lease_id = create_id(IdPrefix::Tsk);
                    let lease = json!({ "leaseId": lease_id, "ownerId": "agt_spawn", "version": version(&mission), "expiresAt": iso_in(60_000.0)? });
                    mission = self.transition_node(
                        &project_id,
                        &mission,
                        &node.id,
                        "leased",
                        &format!("spawn-giveup-lease-{}-{attempt_text}", node.id),
                        Some(&lease),
                    )?;
                    self.transition_node(
                        &project_id,
                        &mission,
                        &node.id,
                        "working",
                        &format!("spawn-giveup-working-{}-{attempt_text}", node.id),
                        None,
                    )?;
                    self.retry_or_block(
                        &project_id,
                        &mission_id,
                        &node.id,
                        &lease_id,
                        Failure::Rejected {
                            diff_hash: None,
                            evidence: json!({ "spawnRejected": node.id, "attempt": attempt }),
                        },
                    )?;
                }
                return Ok(());
            };
            let evaluation_contract = freeze_contract(
                &spawn,
                metrics,
                execution,
                probe_job_id,
                &mission_id,
                &base_commit,
            );
            self.emit(&event(
                &project_id,
                &mission_id,
                "research.contract_probed",
                json!({ "nodeId": node.id, "attempt": attempt, "probeJobId": probe_job_id, "baselineMetrics": metrics }),
            ))?;
            spawn_mission_child(
                research,
                &project_id,
                MissionChild {
                    mission: &mission,
                    node_id: &node.id,
                    attempt,
                    spawn: &spawn,
                    evaluation_contract: &evaluation_contract,
                    base_commit: &base_commit,
                    budget: allocation,
                },
            )?
        };
        mission = research.mission(&project_id, &mission_id)?;
        if node_state(&mission, &node.id) == "ready" {
            let lease = json!({
                "leaseId": format!("lease_{}", after_prefix(&child_id)),
                "ownerId": child_id,
                "version": version(&mission),
                "expiresAt": iso_in(number(&mission["value"]["budgets"]["maximumWallClockSeconds"]) * 1_000.0)?,
            });
            mission = self.transition_node(
                &project_id,
                &mission,
                &node.id,
                "leased",
                &format!("{key}:leased"),
                Some(&lease),
            )?;
        }
        if node_state(&mission, &node.id) == "leased" {
            self.transition_node(
                &project_id,
                &mission,
                &node.id,
                "working",
                &format!("{key}:working"),
                None,
            )?;
            self.emit(&event(
                &project_id,
                &mission_id,
                "mission.child_spawned",
                json!({ "nodeId": node.id, "attempt": attempt, "childId": child_id }),
            ))?;
        }
        Ok(())
    }

    /// One line per research child already specified for this Mission, so the Director keeps them
    /// distinct.
    fn spawn_siblings(
        &self,
        project_id: &str,
        mission: &Value,
        node_id: &str,
    ) -> Result<Vec<String>> {
        let research = &self.deps.research;
        let mission_id = string(mission.get("entityId"));
        let mut lines = Vec::new();
        for item in nodes(mission).iter().filter(|item| {
            item["id"] != node_id
                && SPAWN_NODE_TYPES.contains(&item["type"].as_str().unwrap_or_default())
        }) {
            let item_id = string(item.get("id"));
            let Some(child) = research.spawned_child(project_id, &mission_id, &item_id)? else {
                continue;
            };
            let question = if child.kind == "direction" {
                string(research.direction(project_id, &child.entity_id)?["value"].get("question"))
            } else {
                string(
                    research.autoresearch_execution(project_id, &child.entity_id)?["value"]
                        .get("decisionQuestion"),
                )
            };
            lines.push(format!(
                "{item_id} ({} {}, {}): {question}",
                child.kind, child.entity_id, child.state
            ));
        }
        Ok(lines)
    }

    /// Maps each running research child onto its spawn node: a reviewed closure accepts the node;
    /// a stopped or failed child fails it (respawned while attempts remain); a child blocked for 10
    /// minutes with nothing running is stopped.
    fn reconcile_child_nodes(&self, project_id: &str, mut mission: Value) -> Result<Value> {
        let research = &self.deps.research;
        let mission_id = string(mission.get("entityId"));
        let owned: Vec<Value> = nodes(&mission)
            .into_iter()
            .filter(|item| item["state"] == "working" && child_owned(item))
            .collect();
        for node in owned {
            let node_id = string(node.get("id"));
            let child_id = string(node["lease"].get("ownerId"));
            let direction = child_id.starts_with("dir_");
            let child = if direction {
                research.direction(project_id, &child_id)?
            } else {
                research.autoresearch_execution(project_id, &child_id)?
            };
            let state = child["state"].as_str().unwrap_or_default();
            if state == if direction { "closed" } else { "completed" } {
                for next in ["postflight", "reviewing", "accepted"] {
                    mission = self.transition_node(
                        project_id,
                        &mission,
                        &node_id,
                        next,
                        &format!("child-resolve:{child_id}:{next}"),
                        None,
                    )?;
                }
                self.emit(&event(
                    project_id,
                    &mission_id,
                    "mission.child_resolved",
                    json!({ "nodeId": node_id, "childId": child_id, "outcome": "accepted" }),
                ))?;
                continue;
            }
            let stale = matches!(state, "blocked" | "paused")
                && now_ms() - parse_ms(&string(child["value"].get("updatedAt"))) > CHILD_BLOCKED_MS
                && !self.deps.agents.inspect().iter().any(|agent| {
                    agent.direction_id.as_deref() == Some(child_id.as_str())
                        || agent.autoresearch_id.as_deref() == Some(child_id.as_str())
                });
            if stale {
                let key = format!("child-stop:{child_id}:{}", version(&child));
                if direction {
                    research.transition_direction(
                        project_id,
                        &child_id,
                        version(&child),
                        "stopped",
                        &key,
                    )?;
                } else {
                    research.transition_autoresearch(
                        project_id,
                        &child_id,
                        version(&child),
                        "stopped",
                        &key,
                    )?;
                }
                self.emit(&event(
                    project_id,
                    &mission_id,
                    "mission.child_stopped",
                    json!({ "nodeId": node_id, "childId": child_id, "priorState": state }),
                ))?;
            }
            let ended = stale
                || if direction {
                    matches!(state, "rejected" | "stopped")
                } else {
                    matches!(state, "failed" | "stopped")
                };
            if ended {
                self.emit(&event(
                    project_id,
                    &mission_id,
                    "mission.child_resolved",
                    json!({ "nodeId": node_id, "childId": child_id, "outcome": "failed", "childState": if stale { "stopped" } else { state } }),
                ))?;
                self.retry_or_block(
                    project_id,
                    &mission_id,
                    &node_id,
                    &format!("child_{}", after_prefix(&child_id)),
                    Failure::Rejected {
                        diff_hash: None,
                        evidence: json!({ "childId": child_id, "state": state }),
                    },
                )?;
                mission = research.mission(project_id, &mission_id)?;
            }
        }
        Ok(mission)
    }

    fn reconcile_orphaned_nodes(&self, project_id: &str, mut mission: Value) -> Result<Value> {
        let mission_id = string(mission.get("entityId"));
        let stale_nodes: Vec<Value> = nodes(&mission)
            .into_iter()
            .filter(|node| {
                matches!(
                    node["state"].as_str(),
                    Some("leased" | "working" | "postflight" | "reviewing")
                ) && !child_owned(node)
            })
            .collect();
        for stale in stale_nodes {
            let stale_id = string(stale.get("id"));
            let key = || match stale["lease"]["leaseId"].as_str() {
                Some(lease_id) => lease_id.to_owned(),
                None => create_id(IdPrefix::Cmd),
            };
            if stale["state"] == "leased" {
                mission = self.transition_node(
                    project_id,
                    &mission,
                    &stale_id,
                    "ready",
                    &format!("recovery-release-{}", key()),
                    None,
                )?;
            } else {
                mission = self.transition_node(
                    project_id,
                    &mission,
                    &stale_id,
                    "failed",
                    &format!("recovery-failed-{}", key()),
                    None,
                )?;
                let current = node(&mission, &stale_id).cloned().unwrap_or_default();
                if number(&current["attempt"]) < number(&current["maximumAttempts"]) {
                    mission = self.transition_node(
                        project_id,
                        &mission,
                        &stale_id,
                        "ready",
                        &format!("recovery-ready-{}", key()),
                        None,
                    )?;
                } else {
                    self.emit(&event(
                        project_id,
                        &mission_id,
                        "mission.node_recovered",
                        json!({ "nodeId": stale_id, "priorState": stale["state"], "disposition": "attempt_budget_exhausted" }),
                    ))?;
                    return self.transition(
                        project_id,
                        &mission,
                        "blocked",
                        &format!("recovery-blocked-{}", key()),
                    );
                }
            }
            self.emit(&event(
                project_id,
                &mission_id,
                "mission.node_recovered",
                json!({ "nodeId": stale_id, "priorState": stale["state"], "disposition": "ready_for_new_lease" }),
            ))?;
        }
        Ok(mission)
    }

    async fn run_node(&self, project: &Value, mut mission: Value, node_id: &str) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        let schema_dir = &self.deps.schema_dir;
        match node(&mission, node_id) {
            Some(node) if node["state"] == "ready" => {}
            _ => return Ok(()),
        }
        self.scheduler.borrow_mut().record_allocation(node_id);
        let role = role_for(
            node(&mission, node_id)
                .and_then(|node| node["type"].as_str())
                .unwrap_or_default(),
        );
        let agent_id = create_id(IdPrefix::Agt);
        let task_id = create_id(IdPrefix::Tsk);
        let expires_at = iso_in(30.0 * 60_000.0)?;
        let lease = json!({ "leaseId": task_id, "ownerId": agent_id, "version": version(&mission), "expiresAt": expires_at });
        mission = self.transition_node(
            &project_id,
            &mission,
            node_id,
            "leased",
            &format!("lease-{task_id}"),
            Some(&lease),
        )?;
        mission = self.transition_node(
            &project_id,
            &mission,
            node_id,
            "working",
            &format!("working-{task_id}"),
            None,
        )?;
        let node = node(&mission, node_id).cloned().unwrap_or_default();
        let title = string(node.get("title"));
        let schema = match role {
            Role::Reviewer => "review-verdict",
            Role::LibrarianResearcher => "librarian-completion",
            _ => "general-worker-completion",
        };
        let attempt: Result<(Value, Option<Value>)> = async {
            let packet = self.issue_task_packet(
                project,
                &mission,
                &node,
                &Assignment {
                    agent_id: &agent_id,
                    task_id: &task_id,
                    role,
                    response_schema: schema,
                    expires_at: &expires_at,
                },
            )?;
            let mut direct_review = None;
            let mut digest = String::new();
            if role == Role::Reviewer {
                let director = research
                    .records(&project_id, Some("mission-director-cycle"))?
                    .into_iter()
                    .rev()
                    .find(|record| record["missionId"] == mission_id)
                    .and_then(|record| record["directorAgentId"].as_str().map(str::to_owned))
                    .filter(|id| !id.is_empty());
                let Some(director) = director else {
                    return rejected("Review requires a durable producer identity");
                };
                let outcomes = mission_outcomes(research, &project_id, &mission_id, 14_000)?;
                digest = string(outcomes.get("digest"));
                direct_review = Some(self.issue_review_request(
                    project,
                    &mission,
                    ReviewSpec {
                        task_id: &task_id,
                        reviewer_agent_id: &agent_id,
                        producer_agent_ids: vec![director],
                        review_type: "task",
                        target: json!({ "targetType": "graph_node", "targetId": node_id, "targetVersion": version(&mission) }),
                        criterion_id: node_id,
                        criterion: &title,
                        contract_refs: vec![json!({ "kind": "contract_task.packet", "id": task_id, "hash": sha256(&packet) })],
                        required_artifact_ids: strings(&outcomes["artifactIds"]),
                        required_evidence_ids: strings(&outcomes["evidenceIds"]),
                    },
                )?);
            }
            let mut prompt = format!(
                "Daemon-issued Task Packet: {}. Complete only graph node {node_id}: {title}. ",
                stringify(&packet)
            );
            if let Some(review) = &direct_review {
                if !digest.is_empty() {
                    prompt.push_str(&format!("This is an independent review of the Mission's accepted work against its success criteria. Accepted work (daemon-authored; required Artifacts/Evidence hold these records): {DIGEST_SLOT} "));
                }
                prompt.push_str(&format!("Review Request: {}. ", stringify(review)));
            }
            match (role, &direct_review) {
                (Role::GeneralWorker, _) => prompt.push_str(&format!(
                    "You work in an isolated worktree on branch {}. {} ",
                    string(packet.pointer("/workspace/branch")),
                    completion_instructions(
                        &general_worker_completion_template(&packet, None),
                        "general_worker",
                        schema_dir
                    )
                )),
                (Role::Reviewer, Some(review)) => prompt.push_str(&format!(
                    "{} Your worktree is the Mission integration branch at {}, holding every accepted change: read the files there (nosh_workspace_read) to verify what accepted work claims. Artifacts captured before a change was accepted can predate it; judge the current branch, citing its diff and run-result Artifacts. ",
                    reviewer_instructions(review, schema_dir),
                    string(packet.pointer("/workspace/startingCommit"))
                )),
                (Role::LibrarianResearcher, _) => prompt.push_str(&format!(
                    "{} ",
                    librarian_instructions(
                        &task_id,
                        packet.pointer("/permissions/network") == Some(&json!("allowlisted")),
                        schema_dir
                    )
                )),
                _ => prompt.push_str(&format!(
                    "Submit exactly one {} in the host terminal JSON envelope. ",
                    schema_uri(schema)
                )),
            }
            prompt.push_str(&if role == Role::Reviewer {
                format!("Set reviewerAgentId={agent_id} and target.targetId={node_id}.")
            } else {
                "Prose cannot change node state; deterministic postflight checks the packet, Git state, validators, and Artifact references before Review.".to_owned()
            });
            let session = Session {
                agent_id: &agent_id,
                task_id: &task_id,
                mission_id: &mission_id,
                role,
                permissions: Some(from_packet(&packet["permissions"])?),
                workspace: Some(from_packet(&packet["workspace"])?),
            };
            self.run_session(project, session, &fit_digest(&prompt, &digest))
                .await?;
            Ok((packet, direct_review))
        }
        .await;
        let (packet, direct_review) = match attempt {
            Ok(issued) => issued,
            Err(error) => {
                self.emit(&event(
                    &project_id,
                    &mission_id,
                    "task.execution_failed",
                    json!({ "taskId": task_id, "nodeId": node_id, "message": error_text(&error, "Task execution failed") }),
                ))?;
                return self.retry_or_block(
                    &project_id,
                    &mission_id,
                    node_id,
                    &task_id,
                    Failure::Transient,
                );
            }
        };
        if role == Role::Reviewer {
            mission = self.transition_node(
                &project_id,
                &mission,
                node_id,
                "postflight",
                &format!("postflight-{task_id}"),
                None,
            )?;
            mission = self.transition_node(
                &project_id,
                &mission,
                node_id,
                "reviewing",
                &format!("reviewing-{task_id}"),
                None,
            )?;
            let request = direct_review.expect("a reviewer node always issues its Review Request");
            return self.apply_review(
                &project_id,
                &mission,
                node_id,
                &task_id,
                &agent_id,
                &request,
            );
        }
        let completion = research.terminal_record(&project_id, &task_id, Some(schema))?;
        let postflight = match &completion {
            Some(completion) => {
                let root = repository_root(project);
                let write_up = write_up_only(&root);
                let mut issues = postflight(project, &packet, schema, completion)?;
                issues.extend(completion_run_issues(
                    &task_command_runs(research, &project_id)?,
                    &task_id,
                    completion,
                    &[],
                    Some(&write_up),
                ));
                issues
            }
            None => vec!["missing completion record".to_owned()],
        };
        if !postflight.is_empty() {
            self.emit(&event(
                &project_id,
                &mission_id,
                "task.postflight_failed",
                json!({ "taskId": task_id, "nodeId": node_id, "issues": postflight }),
            ))?;
            let failure = match &completion {
                Some(completion) => Failure::Rejected {
                    diff_hash: changed_commit(completion),
                    evidence: json!(js_sorted(postflight)),
                },
                None => Failure::Transient,
            };
            return self.retry_or_block(&project_id, &mission_id, node_id, &task_id, failure);
        }
        let completion = completion.expect("postflight passed only with a completion");
        mission = self.transition_node(
            &project_id,
            &mission,
            node_id,
            "postflight",
            &format!("postflight-{task_id}"),
            None,
        )?;
        mission = self.transition_node(
            &project_id,
            &mission,
            node_id,
            "reviewing",
            &format!("reviewing-{task_id}"),
            None,
        )?;
        let reviewer_id = create_id(IdPrefix::Agt);
        let review_task = create_id(IdPrefix::Tsk);
        let (artifact_ids, evidence_ids) = completion_refs(&completion);
        let request = self.issue_review_request(
            project,
            &mission,
            ReviewSpec {
                task_id: &review_task,
                reviewer_agent_id: &reviewer_id,
                producer_agent_ids: vec![agent_id.clone()],
                review_type: "task",
                target: json!({ "targetType": "graph_node", "targetId": node_id, "targetVersion": version(&mission) }),
                criterion_id: node_id,
                criterion: &title,
                contract_refs: vec![json!({ "kind": "contract_task.packet", "id": task_id, "hash": sha256(&packet) })],
                required_artifact_ids: artifact_ids,
                required_evidence_ids: evidence_ids,
            },
        )?;
        let prompt = format!(
            "Independently review node {node_id} ({title}) at Mission projection version {}. {}Worker {agent_id} submitted: {}. {} Review Request: {}. {}",
            version(&mission),
            if role == Role::LibrarianResearcher {
                "The worker is a librarian: judge the sources and citations it was asked to collect (are they real, relevant, and cited without overclaiming?). It cannot run commands, so do not require run outputs or implementation results from it; those belong to the implementation node. "
            } else {
                ""
            },
            stringify(&completion),
            daemon_task_facts(research, &project_id, &task_id)?,
            stringify(&request),
            reviewer_instructions(&request, schema_dir)
        );
        let session = Session {
            agent_id: &reviewer_id,
            task_id: &review_task,
            mission_id: &mission_id,
            role: Role::Reviewer,
            permissions: None,
            workspace: None,
        };
        if let Err(error) = self.run_session(project, session, &prompt).await {
            // A reviewer that never produced a valid verdict is one failed review attempt (retry or
            // block below), not a Mission-wide supervisor error.
            let message = error.to_string();
            if !message.starts_with("Terminal output rejected") {
                return Err(error);
            }
            self.emit(&event(
                &project_id,
                &mission_id,
                "task.review_failed",
                json!({ "nodeId": node_id, "taskId": review_task, "message": slice_to(&message, 2_000) }),
            ))?;
        }
        self.apply_review(
            &project_id,
            &mission,
            node_id,
            &review_task,
            &reviewer_id,
            &request,
        )
    }

    fn apply_review(
        &self,
        project_id: &str,
        mission: &Value,
        node_id: &str,
        task_id: &str,
        reviewer_id: &str,
        request: &Value,
    ) -> Result<()> {
        let review =
            self.deps
                .research
                .terminal_record(project_id, task_id, Some("review-verdict"))?;
        if review
            .as_ref()
            .is_some_and(|review| review_passes_request(review, request, reviewer_id))
        {
            self.transition_node(
                project_id,
                mission,
                node_id,
                "accepted",
                &format!("accepted-{task_id}"),
                None,
            )?;
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
                mission,
                node_id,
                "blocked",
                &format!("review-blocked-{task_id}"),
                None,
            )?;
            return Ok(());
        }
        let failure = match review {
            Some(review) => {
                let mut evidence = Map::new();
                if let Some(verdict) = review.get("verdict") {
                    evidence.insert("verdict".into(), verdict.clone());
                }
                evidence.insert("review".into(), review);
                Failure::Rejected {
                    diff_hash: None,
                    evidence: Value::Object(evidence),
                }
            }
            None => Failure::Transient,
        };
        self.retry_or_block(
            project_id,
            &string(mission.get("entityId")),
            node_id,
            task_id,
            failure,
        )
    }

    fn retry_or_block(
        &self,
        project_id: &str,
        mission_id: &str,
        node_id: &str,
        task_id: &str,
        failure: Failure,
    ) -> Result<()> {
        let research = &self.deps.research;
        let mut mission = research.mission(project_id, mission_id)?;
        if matches!(node_state(&mission, node_id), "working" | "reviewing") {
            mission = self.transition_node(
                project_id,
                &mission,
                node_id,
                "failed",
                &format!("failed-{task_id}"),
                None,
            )?;
        }
        let current = node(&mission, node_id).cloned().unwrap_or_default();
        if number(&current["attempt"]) >= number(&current["maximumAttempts"]) {
            self.transition(
                project_id,
                &mission,
                "blocked",
                &format!("attempts-exhausted-{task_id}"),
            )?;
            return Ok(());
        }
        let (hashes, diff_hash, override_reason) = match &failure {
            Failure::Transient => (Vec::new(), Value::Null, Some("transient_retry")),
            Failure::Rejected {
                diff_hash,
                evidence,
            } => (vec![sha256(evidence)], json!(diff_hash), None),
        };
        let attempt = json!({ "projectId": project_id, "missionId": mission_id, "directionId": null, "nodeId": node_id, "parentExperimentId": null, "taskType": current["type"], "hypothesisFamilyTags": [], "commandClass": current["type"], "inScopeFiles": [], "inputArtifactHashes": hashes, "diffHash": diff_hash, "evaluationContractHash": null, "intendedDecision": current["title"], "expectedOutputType": "reviewed_node" });
        let mut governor = FocusGovernor::default();
        for prior in
            research.scoped_events(project_id, mission_id, Some("focus.attempt_recorded"))?
        {
            let payload = &prior["payload"];
            if truthy(payload.get("input")) {
                let delta = match payload.get("delta") {
                    None | Some(Value::Null) => json!({}),
                    Some(delta) => delta.clone(),
                };
                governor.record(
                    &payload["input"],
                    &delta,
                    payload["overrideReason"].as_str(),
                )?;
            }
        }
        let focus = governor.record(&attempt, &json!({}), override_reason)?;
        self.emit(&event(
            project_id,
            mission_id,
            "focus.attempt_recorded",
            json!({ "input": attempt, "delta": {}, "overrideReason": override_reason, "fingerprint": focus["fingerprint"], "progress": focus["progress"] }),
        ))?;
        mission = self.transition_node(
            project_id,
            &mission,
            node_id,
            "ready",
            &format!("retry-{task_id}"),
            None,
        )?;
        if truthy(focus.get("alarm")) {
            self.emit(&event(
                project_id,
                mission_id,
                "focus.alarm",
                focus["alarm"].clone(),
            ))?;
            self.transition_node(
                project_id,
                &mission,
                node_id,
                "blocked",
                &format!("focus-blocked-{task_id}"),
                None,
            )?;
        }
        Ok(())
    }

    async fn final_review(&self, project: &Value, mut mission: Value) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        let basis = research.mission_completion_basis(&project_id, &mission_id)?;
        let issues = unique(
            research
                .mission_completion_issues(&project_id, &mission_id)?
                .into_iter()
                .chain(basis.issues.iter().cloned()),
        );
        if !issues.is_empty() {
            self.emit(&event(
                &project_id,
                &mission_id,
                "mission.completion_rejected",
                json!({ "issues": issues }),
            ))?;
            self.transition(
                &project_id,
                &mission,
                "blocked",
                &format!("completion-gate-{}", create_id(IdPrefix::Cmd)),
            )?;
            return Ok(());
        }
        mission = self.transition(
            &project_id,
            &mission,
            "reviewing",
            &format!("final-review-{}", create_id(IdPrefix::Cmd)),
        )?;
        let reviewer_id = create_id(IdPrefix::Agt);
        let task_id = create_id(IdPrefix::Tsk);
        let value = &mission["value"];
        let contract = pick(
            value,
            &[
                "objective",
                "deliverables",
                "successCriteria",
                "finalReviewRubric",
            ],
        );
        let claims: Vec<(String, String)> = basis
            .claims
            .iter()
            .map(|claim| (string(claim.get("claimId")), string(claim.get("status"))))
            .collect();
        let claims_in = |statuses: &[&str]| -> Vec<String> {
            claims
                .iter()
                .filter(|(_, status)| statuses.contains(&status.as_str()))
                .map(|(claim_id, _)| claim_id.clone())
                .collect()
        };
        let accepted: Vec<Value> = basis
            .completions
            .iter()
            .map(|completion| json!({ "nodeId": completion.node_id, "taskId": completion.task_id, "completion": completion.completion, "review": completion.review, "artifactIds": completion.artifact_ids, "evidenceIds": completion.evidence_ids, "validatorRunIds": completion.validator_run_ids }))
            .collect();
        let summary_artifact_id = self.completion_artifact(
            project,
            &mission,
            "accepted-task-evidence",
            &json!({ "contract": contract, "acceptedCompletions": accepted, "unresolvedClaimIds": basis.unresolved_claim_ids, "openDefectIds": basis.open_defect_ids }),
        )?;
        let reproducibility_artifact_id = self.completion_artifact(
            project,
            &mission,
            "reproducibility",
            &json!({
                "graphVersion": value["graphVersion"],
                "gitHead": self.integration_head(project, &mission_id)?,
                "integrationBranch": integration_branch(&mission_id),
                "acceptedTaskIds": basis.completions.iter().map(|completion| completion.task_id.clone()).collect::<Vec<_>>(),
                "validatorRunIds": basis.validator_run_ids,
                "deliverableArtifactIds": basis.artifact_ids,
                "evidenceIds": basis.evidence_ids,
            }),
        )?;
        let completion_attempt = research
            .submitted(&project_id, None)?
            .iter()
            .filter(|entry| {
                entry["scope"]["missionId"] == mission_id
                    && entry["payload"]["$schema"] == schema_uri("mission-completion-packet")
            })
            .count()
            + 1;
        let criteria: Vec<Value> = basis
            .criteria
            .iter()
            .map(|criterion| json!({ "criterionId": criterion.criterion_id, "statement": criterion.statement, "status": "pass", "taskId": criterion.task_id, "reviewId": criterion.review_id, "validatorRunIds": criterion.validator_run_ids, "artifactIds": criterion.artifact_ids, "evidenceIds": criterion.evidence_ids }))
            .collect();
        let required: Vec<Value> = nodes(&mission)
            .iter()
            .filter(|node| truthy(node.get("required")))
            .map(|node| json!({ "nodeId": node["id"], "state": node["state"], "attempt": node["attempt"] }))
            .collect();
        let mut completion = json!({
            "$schema": schema_uri("mission-completion-packet"),
            "schemaVersion": 1,
            "missionId": mission_id,
            "objectiveVersion": 1,
            "graphVersion": value["graphVersion"],
            "completionAttempt": completion_attempt,
            "requiredNodeDisposition": required,
            "criteria": criteria,
            "deliverableArtifactIds": basis.artifact_ids,
            "claimAudit": {
                "supportedClaimIds": claims_in(&["supported", "partially_supported"]),
                "qualifiedClaimIds": claims_in(&["qualified", "limited"]),
                "contradictedClaimIds": claims_in(&["contradicted", "not_supported"]),
                "unresolvedClaimIds": basis.unresolved_claim_ids,
            },
            "openDefectIds": basis.open_defect_ids,
            "policyExceptions": [],
            "budgetReconciliation": research.mission_budget_use(&project_id, &mission_id)?,
            "reproducibilityManifestArtifactId": reproducibility_artifact_id,
            "directorSummaryArtifactId": summary_artifact_id,
            "deterministicPreReview": { "status": "pass", "validatorRunIds": basis.validator_run_ids },
            "builtAt": now_utc(),
        });
        normalize_numbers(&mut completion);
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, &mission_id, None),
            &format!("mission:{mission_id}"),
            &completion,
            &format!("mission-completion:{mission_id}:{completion_attempt}"),
        )?;
        let producers = unique(
            basis
                .completions
                .iter()
                .map(|completion| completion.agent_id.clone()),
        );
        let required_artifacts = unique(
            basis
                .artifact_ids
                .iter()
                .cloned()
                .chain([summary_artifact_id, reproducibility_artifact_id]),
        );
        let criterion = strings(&value["successCriteria"]).join("; ");
        let request = self.issue_review_request(
            project,
            &mission,
            ReviewSpec {
                task_id: &task_id,
                reviewer_agent_id: &reviewer_id,
                producer_agent_ids: producers,
                review_type: "mission_completion",
                target: json!({ "targetType": "mission_record", "targetId": mission_id, "targetVersion": version(&mission) }),
                criterion_id: &mission_id,
                criterion: &criterion,
                contract_refs: vec![
                    json!({ "kind": "contract_mission", "id": mission_id, "hash": sha256(&contract) }),
                    json!({ "kind": "contract_mission.completion", "id": mission_id, "hash": sha256(&completion) }),
                ],
                required_artifact_ids: required_artifacts,
                required_evidence_ids: basis.evidence_ids.clone(),
            },
        )?;
        let digest =
            string(mission_outcomes(research, &project_id, &mission_id, 9_000)?.get("digest"));
        let packet_summary = json!({ "deliverableArtifactIds": completion["deliverableArtifactIds"], "openDefectIds": completion["openDefectIds"], "deterministicPreReview": completion["deterministicPreReview"], "unresolvedClaimIds": basis.unresolved_claim_ids });
        let prompt = format!(
            "Perform the independent final Mission review for {mission_id}, projection version {}. Audit accepted task completions, their exact independent Reviews, stored deliverable artifacts (read them with nosh_artifact_read when available), evidence, validators, claims, and defects. Accepted work (daemon-authored digest): {digest}. Completion packet: {}. Review Request: {}. {}",
            version(&mission),
            stringify(&packet_summary),
            stringify(&request),
            reviewer_instructions(&request, &self.deps.schema_dir)
        );
        let session = Session {
            agent_id: &reviewer_id,
            task_id: &task_id,
            mission_id: &mission_id,
            role: Role::Reviewer,
            permissions: None,
            workspace: None,
        };
        self.run_session(project, session, &prompt).await?;
        let review = research.terminal_record(&project_id, &task_id, Some("review-verdict"))?;
        if review.is_some_and(|review| review_passes_request(&review, &request, &reviewer_id)) {
            self.transition(
                &project_id,
                &mission,
                "completed",
                &format!("completed-{task_id}"),
            )?;
        } else {
            mission = self.transition(
                &project_id,
                &mission,
                "running",
                &format!("final-repair-{task_id}"),
            )?;
            self.transition(
                &project_id,
                &mission,
                "blocked",
                &format!("final-review-blocked-{task_id}"),
            )?;
        }
        Ok(())
    }

    /// Writes a daemon-authored Mission file under a durable intent and records it as an Artifact.
    fn completion_artifact(
        &self,
        project: &Value,
        mission: &Value,
        name: &str,
        value: &Value,
    ) -> Result<String> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        let root = repository_root(project);
        let directory = root.join(".nosh").join("mission").join(&mission_id);
        let path = directory.join(format!("{name}.json"));
        let path_text = path.to_string_lossy().into_owned();
        let content = format!("{}\n", canonical_json(value));
        let digest = sha256(&json!(content));
        let file_digest =
            |path: &Path| -> Result<String> { Ok(sha256(&json!(std::fs::read_to_string(path)?))) };
        let write = research.begin_external_operation(
            &project_id,
            "filesystem.write",
            &format!("mission-file:{mission_id}:{name}:{}", &digest[7..23]),
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
            return rejected("Mission completion file verification failed");
        }
        research.complete_external_operation(
            &project_id,
            &string(write.get("intentId")),
            &json!({ "path": path_text, "digest": digest }),
        )?;
        let artifact_id = format!(
            "art_{}",
            &sha256(&json!({ "missionId": mission_id, "name": name, "digest": digest }))[7..39]
        );
        let copy = research.begin_external_operation(
            &project_id,
            "artifact.copy",
            &format!("mission-artifact:{artifact_id}"),
            &json!({ "artifactId": artifact_id, "path": path_text, "digest": digest }),
        )?;
        let kind = format!("artifact_mission.{name}");
        let input = json!({ "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": "application/json", "sourcePath": path_text, "retentionClass": "accepted_evidence" });
        let artifact = ArtifactStore::open(&root.join(".nosh").join("artifacts"))?
            .add(input.as_object().expect("an Artifact input is an object"))?;
        research.complete_external_operation(
            &project_id,
            &string(copy.get("intentId")),
            &json!({ "artifactId": artifact_id, "contentHash": artifact["contentHash"], "version": artifact["version"] }),
        )?;
        let record = json!({ "$schema": schema_uri("artifact"), "schemaVersion": 1, "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": "application/json", "contentHash": artifact["contentHash"], "sizeBytes": artifact["sizeBytes"], "version": artifact["version"], "producer": { "type": "noshd" }, "scope": { "missionId": mission_id, "directionId": null, "autoresearchId": null, "experimentId": null }, "git": null, "evaluationContractHash": null, "retentionClass": "accepted_evidence", "remotePreviewPolicy": "encrypted_allowed", "redactionStatus": "checked", "createdAt": artifact["createdAt"] });
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, &mission_id, None),
            &format!("artifact:{artifact_id}"),
            &record,
            &format!("mission-artifact-record:{artifact_id}"),
        )?;
        Ok(artifact_id)
    }

    fn issue_review_request(
        &self,
        project: &Value,
        mission: &Value,
        spec: ReviewSpec<'_>,
    ) -> Result<Value> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        if spec.producer_agent_ids.is_empty()
            || spec
                .producer_agent_ids
                .iter()
                .any(|id| id == spec.reviewer_agent_id)
        {
            return rejected("Review requires an independent producer");
        }
        let packet = match spec
            .contract_refs
            .iter()
            .find(|reference| reference["kind"] == "contract_task.packet")
        {
            Some(reference) => research
                .submitted(
                    &project_id,
                    Some(&format!("task:{}", string(reference.get("id")))),
                )?
                .into_iter()
                .find(|entry| entry["payload"]["$schema"] == schema_uri("task-packet"))
                .map(|mut entry| entry["payload"].take()),
            None => None,
        };
        let criterion = |criterion_id: &Value, statement: &Value| json!({ "criterionId": criterion_id, "statement": statement, "required": true, "severityIfFailed": "blocking" });
        let criteria: Vec<Value> = if spec.review_type == "mission_completion" {
            mission["value"]["successCriteria"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, statement)| {
                    criterion(&json!(format!("criterion_{}", index + 1)), statement)
                })
                .collect()
        } else if let Some(packet) = &packet {
            packet["acceptanceCriteria"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|entry| criterion(&entry["criterionId"], &entry["statement"]))
                .collect()
        } else {
            vec![criterion(&json!(spec.criterion_id), &json!(spec.criterion))]
        };
        let review_id = create_id(IdPrefix::Rev);
        let mut request = json!({
            "$schema": schema_uri("review-request"), "schemaVersion": 1, "reviewRequestId": format!("request_{}", &review_id[4..]), "reviewId": review_id, "reviewType": spec.review_type, "target": spec.target, "scope": { "projectId": project_id, "missionId": mission_id, "directionId": null, "autoresearchId": null },
            "producerAgentIds": unique(spec.producer_agent_ids), "reviewerAgentId": spec.reviewer_agent_id, "independenceCheck": "pass", "contractRefs": spec.contract_refs, "requiredArtifactIds": unique(spec.required_artifact_ids), "requiredEvidenceIds": unique(spec.required_evidence_ids),
            "deterministicPostflight": { "status": "pass", "validatorRunIds": [if spec.review_type == "mission_completion" { "validator_mission.completion" } else { "validator_task.postflight" }] }, "criteria": criteria, "allowedVerdicts": ["PASS", "REVISE", "REDESIGN", "BLOCKED"], "issuedAt": now_utc(),
        });
        normalize_numbers(&mut request);
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, &mission_id, Some(spec.reviewer_agent_id)),
            &format!("task:{}", spec.task_id),
            &request,
            &format!("review-request:{}", spec.task_id),
        )?;
        Ok(request)
    }

    fn issue_task_packet(
        &self,
        project: &Value,
        mission: &Value,
        node: &Value,
        assigned: &Assignment<'_>,
    ) -> Result<Value> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let mission_id = string(mission.get("entityId"));
        let root = repository_root(project);
        let value = &mission["value"];
        let node_id = string(node.get("id"));
        let title = string(node.get("title"));
        let role = assigned.role;
        let branch = git(&root, &["branch", "--show-current"])?;
        if branch.is_empty() || !git(&root, &["status", "--porcelain"])?.is_empty() {
            return rejected("Task preflight requires a clean checked-out Git branch");
        }
        // Every task builds on the Mission's accepted work, not on the user's checkout alone.
        let head = self.integration_head(project, &mission_id)?;
        // A retried worker builds on its node's last postflight-passed attempt instead of
        // discarding that work.
        let events = research.scoped_events(&project_id, &mission_id, None)?;
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
                    &mission_id,
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
        let usage = research.mission_budget_use(&project_id, &mission_id)?;
        let budgets = &value["budgets"];
        let remaining_wall = nosh_wire::js::max(
            1.0,
            number(&budgets["maximumWallClockSeconds"]) - number(&usage["wallClockSeconds"]),
        );
        let remaining_tokens = nosh_wire::js::max(
            1.0,
            number(&budgets["maximumModelTokens"]) - number(&usage["modelTokens"]),
        );
        let output_kind = match role {
            Role::GeneralWorker => "output_code.commit",
            Role::LibrarianResearcher => "output_research.report",
            _ => "output_review.verdict",
        };
        let criterion_ids = strings(&node["criterionIds"]);
        let criterion = |criterion_id: &str, statement: Value| json!({ "criterionId": criterion_id, "statement": statement, "validatorIds": ["validator_task.postflight"], "reviewRubricIds": ["rubric_scientific.correctness"], "required": true });
        let acceptance_criteria: Vec<Value> = if criterion_ids.is_empty() {
            vec![criterion(&node_id, json!(title))]
        } else {
            criterion_ids
                .iter()
                .map(|criterion_id| {
                    let index =
                        parse_number(criterion_id.get("criterion_".len()..).unwrap_or_default())
                            - 1.0;
                    let statement = if index >= 0.0 && index.fract() == 0.0 {
                        value["successCriteria"][index as usize].clone()
                    } else {
                        Value::Null
                    };
                    criterion(criterion_id, statement)
                })
                .collect()
        };
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
        // Research children this node depends on: their Evidence and outcomes reach the worker, not
        // only the final gate.
        let inputs = research.research_inputs(
            &project_id,
            &mission_id,
            &strings(&node["hardDependencies"]),
        )?;
        // Answers and code diffs exceed a packet instruction's 1,000 characters, so they travel as
        // an Artifact the worker reads.
        let research_artifact = if inputs["summaries"]
            .as_array()
            .is_some_and(|summaries| !summaries.is_empty())
        {
            Some(self.completion_artifact(
                project,
                mission,
                "research-inputs",
                &json!({ "nodeId": node_id, "children": inputs["summaries"] }),
            )?)
        } else {
            None
        };
        let starting_evidence = strings(&value["startingEvidence"]);
        let mut instructions = vec![
            format!("Complete only {title}"),
            "Do not modify protected NOSH contracts".to_owned(),
        ];
        if let Some(artifact) = &research_artifact {
            instructions.push(format!("Build on this Mission's finished research: read Artifact {artifact} with nosh_artifact_read (each research child's question, answer, and promoted experiments with their measured results and code changes; port a promoted change rather than re-deriving it, and cite its results). Their Evidence is in inputEvidenceIds."));
        }
        if !starting_evidence.is_empty() {
            instructions.push(format!("Read the starting Evidence ({}) with nosh_artifact_read; each record cites the Artifacts that hold its measurements", starting_evidence.join(", ")));
        }
        if commands.is_empty() {
            instructions.push("You cannot run commands: report only what you verified by reading, and claim a criterion satisfied only as far as that evidence supports".to_owned());
        } else {
            instructions.extend(task_command_instructions(&commands, true));
        }
        instructions.extend(prior_attempt_instructions(&events, &node_id));
        if let Some(commit) = &resumed {
            instructions.push(resume_instruction(commit));
        }
        let summary = acceptance_criteria
            .iter()
            .map(|criterion| criterion["statement"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("; ");
        let attempt = number(&node["attempt"]);
        let maximum_wall = nosh_wire::js::min(7_200.0, remaining_wall);
        let mut packet = json!({
            "$schema": schema_uri("task-packet"), "schemaVersion": 1, "templateVersion": "1.0.0",
            "taskId": assigned.task_id, "attempt": attempt, "taskType": format!("task_{}", string(node.get("type")).replace('_', ".")), "assignedRole": role.as_str(), "assignedAgentId": assigned.agent_id,
            "scope": { "projectId": project_id, "missionId": mission_id, "directionId": null, "autoresearchId": null, "experimentId": null, "graphNodeId": node_id },
            "goalStack": { "projectGoalId": project_id, "projectGoalSummary": value["objective"], "missionCriterionIds": criterion_ids, "missionCriterionSummary": summary, "directionQuestionId": null, "directionQuestionSummary": null, "nodeObjective": title },
            "whyNow": "The deterministic Mission frontier selected this ready node.",
            "instructions": instructions,
            // The researcher's starting Evidence and this node's research children's Evidence
            // (readable with nosh_artifact_read).
            "inScope": ["**"], "outOfScope": [".nosh/contracts/**"], "inputArtifactIds": research_artifact.iter().collect::<Vec<_>>(), "inputEvidenceIds": unique(starting_evidence.into_iter().chain(strings(&inputs["evidenceIds"]))),
            "requiredOutputs": [{ "outputId": format!("out_{}", &assigned.task_id[4..]), "kind": output_kind, "required": true }],
            "acceptanceCriteria": acceptance_criteria,
            "workspace": { "worktreeId": tree.worktree_id, "branch": tree.branch, "startingCommit": head, "writeScopes": ["**"], "protectedScopes": [".nosh/contracts/**"] },
            "permissions": permissions,
            "budget": { "deadline": iso_in(maximum_wall * 1_000.0)?, "maximumWallClockSeconds": maximum_wall, "maximumModelTokens": nosh_wire::js::min(60_000.0, remaining_tokens), "maximumToolCalls": 100, "maximumRepairAttempts": nosh_wire::js::max(0.0, number(&node["maximumAttempts"]) - attempt) },
            "progressPolicy": { "milestoneIntervalSeconds": 900, "emitOnFirstDurableDelta": true, "emitOnBlocker": true, "emitOnAnomaly": true, "maximumSilentSeconds": 1_200 },
            "responseSchema": schema_uri(assigned.response_schema),
            "observedVersions": { "missionGraphVersion": value["graphVersion"], "missionProjectionVersion": version(mission), "leaseVersion": version(mission) },
            "lease": { "leaseId": assigned.task_id, "expiresAt": assigned.expires_at, "heartbeatSeconds": 15 }, "issuedBy": "nosh", "issuedAt": now_utc(),
        });
        normalize_numbers(&mut packet);
        research.submit_daemon_record(
            &project_id,
            &scope(&project_id, &mission_id, Some(assigned.agent_id)),
            &format!("task:{}", assigned.task_id),
            &packet,
            &format!("task-packet:{}", assigned.task_id),
        )?;
        Ok(packet)
    }

    /// Accepted worker commits are merged onto the Mission's own branch (never the user's), so
    /// later tasks build on accepted work and the Mission ends as one reviewable branch for the
    /// user to merge. Idempotent and crash-safe: Git ancestry is the record of what is
    /// integrated. A conflict blocks the Mission for attention instead of guessing a resolution.
    fn integration_head(&self, project: &Value, mission_id: &str) -> Result<String> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let root = repository_root(project);
        let base = research
            .records(&project_id, Some("task-packet"))?
            .into_iter()
            .find(|packet| packet["scope"]["missionId"] == mission_id)
            .and_then(|packet| {
                packet["workspace"]["startingCommit"]
                    .as_str()
                    .map(str::to_owned)
            })
            .filter(|base| !base.is_empty());
        let Some(base) = base else {
            return git(&root, &["rev-parse", "HEAD"]);
        };
        let branch = integration_branch(mission_id);
        let path = GitWorkspaceManager::new(
            &root,
            &root.join(".nosh").join("worktrees"),
            &root.join(".nosh").join("git"),
        )?
        .create(&branch, &base, &format!("wt_mission_{}", &mission_id[4..]))?;
        for accepted in research
            .mission_completion_basis(&project_id, mission_id)?
            .completions
        {
            let code = &accepted.completion["codeChanges"];
            let Some(ending) = code["endingCommit"]
                .as_str()
                .filter(|ending| !ending.is_empty())
            else {
                continue;
            };
            if strict_equal(code.get("endingCommit"), code.get("startingCommit"))
                || daemon_git(&path, &["merge-base", "--is-ancestor", ending, "HEAD"]).0 == Some(0)
            {
                continue;
            }
            let message = format!(
                "NOSH: integrate accepted node {} ({})",
                accepted.node_id, accepted.task_id
            );
            let (status, stdout, stderr) = daemon_git(
                &path,
                &["merge", "--no-ff", "--no-edit", "-m", &message, ending],
            );
            if status != Some(0) {
                daemon_git(&path, &["merge", "--abort"]);
                self.emit(&event(
                    &project_id,
                    mission_id,
                    "mission.integration_conflict",
                    json!({ "nodeId": accepted.node_id, "taskId": accepted.task_id, "commit": ending, "branch": branch, "detail": slice_to(js_trim(&format!("{stdout}{stderr}")), 2_000) }),
                ))?;
                return rejected(format!(
                    "Accepted node {} conflicts with Mission branch {branch}; resolve it in {} or retry the node",
                    accepted.node_id,
                    path.display()
                ));
            }
        }
        git(&path, &["rev-parse", "HEAD"])
    }

    async fn run_session(&self, project: &Value, session: Session<'_>, prompt: &str) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let Some(runtime) = &self.deps.runtime else {
            let root = repository_root(project);
            let cwd = match &session.workspace {
                Some(workspace) => task_worktree_path(&root, &workspace.worktree_id),
                None => root,
            };
            let options = SessionOptions {
                project_id,
                mission_id: Some(session.mission_id.to_owned()),
                direction_id: None,
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
                "ownerScope": { "missionId": session.mission_id, "directionId": null, "autoresearchId": null, "experimentId": null, "graphNodeId": null },
                "role": session.role.as_str(), "purpose": format!("Execute Mission task {}", session.task_id), "executionMode": "background", "parentThreadId": null, "inputRefs": [], "skillIds": [], "capabilities": [],
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

/// The part of an ID after its prefix (`childId.slice(childId.indexOf("_") + 1)`).
fn after_prefix(id: &str) -> &str {
    id.split_once('_').map_or(id, |(_, rest)| rest)
}
