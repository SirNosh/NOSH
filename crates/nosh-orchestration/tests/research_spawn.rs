//! Port of `apps/noshd/src/research-spawn.test.ts`. The supervisors run over the daemon's research
//! control, Jobs, and event delivery, with scripted Directors in place of Pi.
//!
//! Where TypeScript stubs a research read with `vi.spyOn`, the read's answer is written to the
//! store instead (a projection mutation or a submitted record), and where it moves `Date.now`
//! ahead, the blocked run's `updatedAt` is dated back. Not ported: the task-session resume case
//! (it drives the supervisors' private session methods), and the research-inputs handoff case (it
//! stubs five reads at once; `research_inputs` is covered in nosh-research's own tests).

mod common;

use common::supervision::git;
use common::{FakeAgents, assert_match, local};
use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::pi::{Inspection, TerminalStep};
use nosh_agent::policy::{Role, SessionOptions};
use nosh_orchestration::Agents;
use nosh_orchestration::daemon::{NoshDaemon, ResearchHost};
use nosh_orchestration::research_spawner::{
    JobRunner, frozen_evaluation_files, scope_issues, spawn_issues,
};
use nosh_orchestration::supervisors::Dependencies;
use nosh_orchestration::supervisors::autoresearch::autoresearcher_id;
use nosh_orchestration::supervisors::direction::DirectionSupervisor;
use nosh_orchestration::supervisors::mission::MissionSupervisor;
use nosh_research::project::{approve_project_contract, read_project_contract};
use nosh_research::research_control::MissionInput;
use nosh_store::{EventStore, ProjectionInput};
use nosh_wire::{EVENT_SCHEMA, schema_uri};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static CYCLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""cycleId":"(cycle_[0-9a-f]+)""#).unwrap());
static GRAPH_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"graph version (\d+)").unwrap());
static MISSION_NODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Research child to specify: node (mnode_[0-9a-f]+)").unwrap());
static DIRECTION_NODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Autoresearch to specify for node ([a-z]+_[0-9a-z]+)").unwrap());
static CONTRACT_HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sha256:[0-9a-f]{64}").unwrap());

type Spec = Box<dyn Fn(&str) -> Option<Value>>;

/// Directors that answer from the brief: Mission Directors fill `spawn` when the brief asks for a
/// research child.
struct Directors {
    store: Rc<EventStore>,
    sessions: RefCell<Vec<SessionOptions>>,
    prompts: RefCell<Vec<String>>,
    agent_ids: RefCell<Vec<String>>,
    /// A Research Director that judges further work does not contribute (it halts the selected
    /// node).
    halts: Cell<bool>,
    spec: Spec,
}

impl Directors {
    fn new(store: Rc<EventStore>, spec: impl Fn(&str) -> Option<Value> + 'static) -> Rc<Self> {
        Rc::new(Directors {
            store,
            sessions: RefCell::default(),
            prompts: RefCell::default(),
            agent_ids: RefCell::default(),
            halts: Cell::new(false),
            spec: Box::new(spec),
        })
    }

    fn submit(&self, session: &SessionOptions, record: Value) {
        let task_id = session.task_id.clone().unwrap();
        let draft = json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "pi", "scope": { "projectId": session.project_id, "missionId": session.mission_id, "directionId": session.direction_id, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": session.agent_id }, "correlationId": format!("task:{task_id}"), "causationId": null, "payload": record });
        let schema = record["$schema"].as_str().unwrap();
        self.store
            .append_terminal_submission(&task_id, "fixture", schema, &record, &draft)
            .unwrap();
    }

    fn answer(&self, agent_id: &str, prompt: &str) -> Result<(), String> {
        let session = self
            .sessions
            .borrow()
            .iter()
            .find(|session| session.agent_id == agent_id)
            .cloned()
            .unwrap();
        self.prompts.borrow_mut().push(prompt.to_owned());
        let mut record = Map::new();
        let mission = session.role == Role::MissionDirector;
        record.insert(
            "$schema".into(),
            json!(schema_uri(if mission {
                "mission-director-cycle"
            } else {
                "research-director-cycle"
            })),
        );
        if let Some(cycle) = CYCLE.captures(prompt) {
            record.insert("cycleId".into(), json!(&cycle[1]));
        }
        record.insert("directorAgentId".into(), json!(agent_id));
        // A worker's Task Packet names no graph version: TypeScript's `Number(undefined)` is NaN,
        // which the event schema refuses, so the submission throws and the task fails.
        let version = GRAPH_VERSION
            .captures(prompt)
            .ok_or("Invalid event draft: payload.observedGraphVersion is NaN")?;
        record.insert(
            "observedGraphVersion".into(),
            json!(version[1].parse::<i64>().unwrap()),
        );
        if mission {
            record.insert("missionId".into(), json!(session.mission_id));
            record.insert(
                "northStarCheck".into(),
                json!({ "currentWorkContributes": true }),
            );
        } else {
            record.insert("directionId".into(), json!(session.direction_id));
            record.insert(
                "evaluationContractHash".into(),
                json!(CONTRACT_HASH.find(prompt).map_or("", |hash| hash.as_str())),
            );
            record.insert(
                "questionCheck".into(),
                json!({ "currentWorkContributes": !self.halts.get() }),
            );
        }
        let node = MISSION_NODE
            .captures(prompt)
            .or_else(|| DIRECTION_NODE.captures(prompt))
            .map(|captures| captures[1].to_owned());
        if let Some(spawn) = node.and_then(|node| (self.spec)(&node)) {
            record.insert("spawn".into(), spawn);
        }
        self.submit(&session, Value::Object(record));
        Ok(())
    }
}

impl Agents for Directors {
    fn start(&self, options: SessionOptions) -> LocalBoxFuture<'_, Result<Inspection, String>> {
        self.agent_ids.borrow_mut().push(options.agent_id.clone());
        let agent_id = options.agent_id.clone();
        {
            let mut sessions = self.sessions.borrow_mut();
            sessions.retain(|session| session.agent_id != agent_id);
            sessions.push(options);
        }
        let found = self
            .inspect()
            .into_iter()
            .find(|agent| agent.agent_id == agent_id)
            .unwrap();
        async move { Ok(found) }.boxed_local()
    }

    fn prompt<'a>(
        &'a self,
        agent_id: &'a str,
        prompt: &'a str,
        _step: Option<&'a TerminalStep>,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        let answered = self.answer(agent_id, prompt);
        async move { answered }.boxed_local()
    }

    fn steer<'a>(
        &'a self,
        _agent_id: &'a str,
        _message: &'a str,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        async { Ok(()) }.boxed_local()
    }

    fn abort<'a>(&'a self, _agent_id: &'a str) -> LocalBoxFuture<'a, Result<(), String>> {
        async { Ok(()) }.boxed_local()
    }

    fn stop(&self, agent_id: &str) {
        self.sessions
            .borrow_mut()
            .retain(|session| session.agent_id != agent_id);
    }

    fn inspect(&self) -> Vec<Inspection> {
        self.sessions
            .borrow()
            .iter()
            .map(|session| {
                let mut agent = common::inspection(
                    &session.project_id,
                    &session.agent_id,
                    session.task_id.as_deref(),
                    session.role,
                );
                agent.mission_id = session.mission_id.clone();
                agent.direction_id = session.direction_id.clone();
                agent.status = "running".into();
                agent
            })
            .collect()
    }
}

/// A daemon with one Project whose contract declares `command_evaluate`.
struct Spawn {
    daemon: Rc<NoshDaemon>,
    project: Value,
    project_id: String,
    store: Rc<EventStore>,
    _directory: tempfile::TempDir,
}

impl Drop for Spawn {
    fn drop(&mut self) {
        self.daemon.stop();
    }
}

impl Spawn {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let daemon = common::daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let project = daemon
            .initialize_project(
                &directory.path().join("repository"),
                true,
                Some("Spawn fixture"),
                None,
            )
            .unwrap();
        let project_id = project["projectId"].as_str().unwrap().to_owned();
        let root = std::path::PathBuf::from(project["repositoryRoot"].as_str().unwrap());
        std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
        let commit = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=T", "-c", "user.email=t@example.invalid"];
            all.extend_from_slice(args);
            git(&root, &all);
        };
        commit(&["add", "README.md"]);
        commit(&["commit", "-m", "initial"]);
        let mut contract = read_project_contract(&root).unwrap();
        contract["contractVersion"] = json!(2);
        let mut approved = approve_project_contract(&root, &contract).unwrap();
        approved["contractVersion"] = json!(3);
        approved["execution"] = json!({ "runner": "native", "commands": [
            { "commandId": "command_evaluate", "description": "Write the evaluation metrics", "argv": ["node", "-e", "require('fs').writeFileSync('metrics.json', JSON.stringify({ score: 0.5, f1: 0.9 }))"], "timeoutSeconds": 60 },
        ] });
        daemon
            .amend_project_contract(&project_id, &approved, "amend-commands-0001")
            .unwrap();
        let store = daemon.store_for(&project_id).unwrap();
        Spawn {
            daemon,
            project,
            project_id,
            store,
            _directory: directory,
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self.project["repositoryRoot"].as_str().unwrap().into()
    }

    fn deps(&self, agents: &Rc<Directors>) -> Dependencies<ResearchHost> {
        let project = self.project.clone();
        let daemon = Rc::clone(&self.daemon);
        Dependencies {
            research: Rc::clone(&self.daemon.research),
            agents: Rc::clone(agents) as Rc<dyn Agents>,
            projects: Rc::new(move || Ok(vec![project.clone()])),
            package_path: ".".into(),
            schema_dir: self.daemon.schema_dir().to_owned(),
            emit: Rc::new(move |draft: &Value| daemon.append_draft(draft).map(|_| ())),
            runtime: None,
            job_runner: Some(Rc::clone(&self.daemon) as Rc<dyn JobRunner>),
        }
    }

    fn missions(&self, agents: &Rc<Directors>) -> Rc<MissionSupervisor<ResearchHost>> {
        MissionSupervisor::new(self.deps(agents))
    }

    fn directions_supervisor(
        &self,
        agents: &Rc<Directors>,
    ) -> Rc<DirectionSupervisor<ResearchHost>> {
        DirectionSupervisor::new(self.deps(agents))
    }

    /// Creates a Mission and moves it to running.
    fn running(&self, input: Value) -> Value {
        let input: MissionInput = serde_json::from_value(input).unwrap();
        let research = &self.daemon.research;
        let mut mission = research.create_mission(&self.project_id, &input).unwrap();
        for next in ["planning", "awaiting_approval", "running"] {
            let id = mission["entityId"].as_str().unwrap().to_owned();
            mission = research
                .transition_mission(
                    &self.project_id,
                    &id,
                    mission["version"].as_i64().unwrap(),
                    next,
                    &format!("{id}-{next}"),
                )
                .unwrap();
        }
        mission
    }

    fn mission(&self, mission_id: &str) -> Value {
        self.daemon
            .research
            .mission(&self.project_id, mission_id)
            .unwrap()
    }

    fn nodes(&self, mission_id: &str) -> Vec<Value> {
        self.mission(mission_id)["value"]["nodes"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn working(&self, mission_id: &str) -> Vec<Value> {
        self.nodes(mission_id)
            .into_iter()
            .filter(|node| node["state"] == "working")
            .collect()
    }

    fn directions(&self) -> Vec<Value> {
        self.daemon.research.directions(&self.project_id).unwrap()
    }

    fn direction(&self, direction_id: &str) -> Value {
        self.daemon
            .research
            .direction(&self.project_id, direction_id)
            .unwrap()
    }

    fn runs(&self) -> Vec<Value> {
        self.daemon.research.autoresearch(&self.project_id).unwrap()
    }

    /// The payloads of a Mission's scoped events of one type.
    fn scoped(&self, mission_id: &str, kind: &str) -> Vec<Value> {
        self.daemon
            .research
            .scoped_events(&self.project_id, mission_id, Some(kind))
            .unwrap()
            .into_iter()
            .map(|mut entry| entry["payload"].take())
            .collect()
    }

    fn run(&self, run_id: &str) -> Value {
        self.daemon
            .research
            .autoresearch_execution(&self.project_id, run_id)
            .unwrap()
    }

    /// Rewrites a Direction or Autoresearch projection with its state and value edited, as
    /// TypeScript's `vi.spyOn(research.direction)` (or `.autoresearch`) makes every reader see them.
    fn rewrite(&self, current: &Value, state: Option<&str>, edit: impl Fn(&mut Value)) {
        let id = text(&current["entityId"]);
        let direction = id.starts_with("dir_");
        let mut value = current["value"].clone();
        edit(&mut value);
        let scope = if direction {
            json!({ "projectId": self.project_id, "missionId": value["missionId"], "directionId": id, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null })
        } else {
            json!({ "projectId": self.project_id, "missionId": value["missionId"], "directionId": value["directionId"], "autoresearchId": id, "experimentId": null, "runId": null, "jobId": null, "agentId": null })
        };
        self.store
            .mutate_projection(
                &format!("fixture-rewrite-{id}-{}", current["version"]),
                current["version"].as_i64(),
                &json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "fixture.projection_rewritten", "source": "fixture", "scope": scope, "correlationId": id, "causationId": null, "payload": {} }),
                &ProjectionInput {
                    entity_type: if direction { "direction" } else { "autoresearch" }.into(),
                    entity_id: id,
                    state: state.unwrap_or(current["state"].as_str().unwrap()).to_owned(),
                    value,
                    graph: None,
                    records: Vec::new(),
                    intent: None,
                },
            )
            .unwrap();
    }

    /// Stands in for the reviewed baseline, then moves the Direction's baseline node to accepted.
    fn accept_baseline(&self, direction_id: &str, prefix: &str) {
        let research = &self.daemon.research;
        self.rewrite(&self.direction(direction_id), None, |value| {
            value["acceptedBaseline"] =
                json!({ "commit": value["baseCommit"], "reviewId": "rev_fixture" });
        });
        let baseline = self.direction(direction_id)["value"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["title"] == "Reproduce accepted baseline")
            .unwrap()
            .clone();
        let path = [
            "pending",
            "ready",
            "leased",
            "working",
            "postflight",
            "reviewing",
            "accepted",
        ];
        let from = path
            .iter()
            .position(|state| baseline["state"] == *state)
            .unwrap();
        for next in &path[from + 1..] {
            let version = self.direction(direction_id)["version"].as_i64().unwrap();
            let expires = nosh_wire::iso_from_ms(now_ms() + 60_000.0).unwrap();
            let lease = json!({ "leaseId": "tsk_fixture", "ownerId": "agt_fixture", "version": version, "expiresAt": expires });
            research
                .transition_direction_node(
                    &self.project_id,
                    direction_id,
                    version,
                    &text(&baseline["id"]),
                    next,
                    &format!("{prefix}-{next}"),
                    (*next == "leased").then_some(&lease),
                    false,
                )
                .unwrap();
        }
    }

    /// A submitted record outside any task, as TypeScript's stubbed `research.records` lists it.
    fn record(&self, record: Value) {
        self.store
            .append(&json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "fixture", "scope": { "projectId": self.project_id, "missionId": null, "directionId": null, "autoresearchId": record["autoresearchId"], "experimentId": null, "runId": null, "jobId": null, "agentId": null }, "correlationId": null, "causationId": null, "payload": record }))
            .unwrap();
    }
}

fn spec(kind: &str, node_id: &str) -> Value {
    json!({ "nodeId": node_id, "kind": kind, "question": format!("Does change {node_id} lower the score?"), "decisionUse": if kind == "research_direction" { json!("Choose what the Mission builds on.") } else { Value::Null }, "evaluation": { "commandId": "command_evaluate", "resultPath": "metrics.json", "primaryMetric": { "name": "score", "objective": "minimize", "minimumEffect": 0.01 }, "guardrails": [{ "name": "f1", "direction": "maximize", "threshold": -0.05 }], "target": null, "maximumNoProgressRounds": 1 }, "familyTags": [format!("family-{}", &node_id[node_id.len() - 6..])], "scope": ["src/**"] })
}

fn mission_input(title: &str, research_plan: Value, key: &str) -> Value {
    json!({ "title": title, "objective": format!("{title} objective"), "deliverables": ["Result"], "successCriteria": ["Reviewed"], "researchPlan": research_plan, "idempotencyKey": key })
}

/// Ticks a supervisor every 50 ms until `done` or the deadline.
async fn tick_until(seconds: u64, mut tick: impl FnMut(), done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && !done() {
        tick();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

fn sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort();
    items
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

// ---- frozen evaluation files ----

#[test]
fn names_the_evaluation_program_files_and_the_result_file_and_refuses_a_scope_made_only_of_them() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("evaluate.mjs"), "// evaluator\n").unwrap();
    let frozen = frozen_evaluation_files(
        root.path(),
        &["node".into(), "evaluate.mjs".into(), "--split=dev".into()],
        "./metrics.json",
    );
    assert_eq!(sorted(frozen.clone()), ["evaluate.mjs", "metrics.json"]);
    assert!(
        scope_issues(&["evaluate.mjs".into(), "./metrics.json".into()], &frozen)
            .join(" ")
            .contains("frozen evaluation")
    );
    assert_eq!(
        scope_issues(
            &["src/calibrate.mjs".into(), "evaluate.mjs".into()],
            &frozen
        ),
        Vec::<String>::new()
    );
}

// ---- Mission research spawning ----

#[test]
fn spawns_its_research_directions_and_its_own_autoresearch_from_director_specs_with_a_measured_baseline()
 {
    local(async {
        let fixture = Spawn::new();
        let mission = fixture.running(mission_input(
            "Research plan",
            json!({ "directions": 2, "missionAutoresearch": true }),
            "plan-create",
        ));
        let mission_id = text(&mission["entityId"]);
        assert_eq!(
            mission["value"]["budgets"]["maximumModelTokens"],
            json!(4_000_000)
        );
        let nodes = mission["value"]["nodes"].as_array().unwrap();
        let spawn_nodes: Vec<&Value> = nodes
            .iter()
            .filter(|node| node["type"] == "research_direction" || node["type"] == "autoresearch")
            .collect();
        assert_eq!(
            sorted(spawn_nodes.iter().map(|node| text(&node["type"])).collect()),
            ["autoresearch", "research_direction", "research_direction"]
        );
        let implementation = nodes
            .iter()
            .find(|node| node["type"] == "implementation")
            .unwrap();
        assert_eq!(
            sorted(
                implementation["hardDependencies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(text)
                    .collect()
            ),
            sorted(spawn_nodes.iter().map(|node| text(&node["id"])).collect())
        );
        let kinds: Vec<(String, String)> = spawn_nodes
            .iter()
            .map(|node| (text(&node["id"]), text(&node["type"])))
            .collect();
        let directors = Directors::new(Rc::clone(&fixture.store), move |node_id| {
            kinds
                .iter()
                .find(|(id, _)| id == node_id)
                .map(|(_, kind)| spec(kind, node_id))
        });
        let supervisor = fixture.missions(&directors);
        tick_until(
            90,
            || supervisor.tick().unwrap(),
            || fixture.working(&mission_id).len() >= 3,
        )
        .await;
        let directions = fixture.directions();
        let runs = fixture.runs();
        assert_eq!(directions.len(), 2);
        assert_eq!(runs.len(), 1);
        for direction in &directions {
            assert_match(
                &direction["value"],
                &json!({ "createdBy": "mission_director", "missionId": mission_id, "origin": { "scopeType": "mission", "scopeId": mission_id, "attempt": 1 } }),
            );
        }
        assert!(
            directions
                .iter()
                .all(|direction| direction["state"] == "active")
        );
        assert_match(
            &runs[0]["value"],
            &json!({ "directionId": null, "missionId": mission_id, "invokedByRole": "mission_director", "state": "running" }),
        );
        // The Autoresearch is run by its own autoresearcher, never by the Director that specified
        // it.
        assert!(
            !directors
                .agent_ids
                .borrow()
                .contains(&autoresearcher_id(&text(&runs[0]["entityId"])))
        );
        // The baseline is what the declared command measured, never a number from the Director.
        assert_eq!(
            runs[0]["value"]["evaluationContract"]["baselineMetrics"],
            json!({ "score": 0.5, "f1": 0.9 })
        );
        // A spawn guardrail is a change from that baseline: "f1 may drop 0.05" freezes as
        // f1 >= 0.85.
        assert_eq!(
            runs[0]["value"]["evaluationContract"]["guardrails"],
            json!([{ "name": "f1", "direction": "maximize", "threshold": 0.85 }])
        );
        assert!(
            directors
                .prompts
                .borrow()
                .iter()
                .any(|prompt| prompt.contains("command_evaluate:"))
        );
        let owners = || -> Vec<String> {
            sorted(
                fixture
                    .working(&mission_id)
                    .iter()
                    .filter(|node| {
                        node["type"] == "research_direction" || node["type"] == "autoresearch"
                    })
                    .map(|node| text(&node["lease"]["ownerId"]))
                    .collect(),
            )
        };
        assert_eq!(fixture.working(&mission_id).len(), 3);
        let mut children: Vec<String> = directions
            .iter()
            .map(|direction| text(&direction["entityId"]))
            .collect();
        children.push(text(&runs[0]["entityId"]));
        let children = sorted(children);
        assert_eq!(owners(), children);
        // A fresh supervisor (a restart) leaves child-owned nodes alone and spawns nothing twice.
        // TypeScript checks for exactly three working nodes as soon as the tick returns; here the
        // fresh cycle is given time to run, so a worker may meanwhile lease the literature node.
        fixture.missions(&directors).tick().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fixture.directions().len(), 2);
        assert_eq!(fixture.runs().len(), 1);
        assert_eq!(owners(), children);
    });
}

#[test]
fn feeds_a_rejected_spec_back_to_the_director_and_refuses_a_spawn_the_budget_cannot_carry() {
    local(async {
        let fixture = Spawn::new();
        let mission = fixture.running(mission_input(
            "Reject",
            json!({ "directions": 1, "missionAutoresearch": false }),
            "reject-create",
        ));
        let mission_id = text(&mission["entityId"]);
        let directors = Directors::new(Rc::clone(&fixture.store), |node_id| {
            let mut spawn = spec("research_direction", node_id);
            spawn["evaluation"]["commandId"] = json!("command_missing");
            Some(spawn)
        });
        let supervisor = fixture.missions(&directors);
        for _ in 0..2 {
            supervisor.tick().unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let rejected = fixture.scoped(&mission_id, "research.spawn_rejected");
        assert!(!rejected.is_empty());
        assert!(
            rejected[0]
                .to_string()
                .contains("command_missing is not a command declared in the Project contract")
        );
        {
            let prompts = directors.prompts.borrow();
            let last = prompts.last().unwrap();
            assert!(last.contains("command_missing is not a command declared"));
            assert!(last.contains("change from the measured baseline"));
        }
        let node_id = text(
            &mission["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["type"] == "research_direction")
                .unwrap()["id"],
        );
        let mut demanding = spec("research_direction", &node_id);
        demanding["evaluation"]["guardrails"] =
            json!([{ "name": "f1", "direction": "maximize", "threshold": 0.005 }]);
        assert!(
            spawn_issues(&fixture.project, &demanding, &node_id, "research_direction")
                .join(" ")
                .contains("guardrail f1")
        );
        assert!(fixture.directions().is_empty());

        // One active Mission per Project: finish the first before the tight one.
        let research = &fixture.daemon.research;
        let stopping = research
            .transition_mission(
                &fixture.project_id,
                &mission_id,
                fixture.mission(&mission_id)["version"].as_i64().unwrap(),
                "stopping",
                "reject-stop",
            )
            .unwrap();
        research
            .transition_mission(
                &fixture.project_id,
                &mission_id,
                stopping["version"].as_i64().unwrap(),
                "stopped",
                "reject-stopped",
            )
            .unwrap();
        let mut input = mission_input(
            "Tight",
            json!({ "directions": 2, "missionAutoresearch": true }),
            "tight-create",
        );
        input["budgets"] = json!({ "maximumModelTokens": 600_000 });
        let tight = fixture.running(input);
        let tight_id = text(&tight["entityId"]);
        fixture.missions(&directors).tick().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let refused = fixture.scoped(&tight_id, "research.spawn_rejected");
        assert!(
            refused
                .first()
                .is_some_and(|payload| payload.to_string().contains("budget_insufficient")),
            "{refused:?}"
        );
        assert!(
            fixture
                .nodes(&tight_id)
                .iter()
                .any(|node| node["state"] == "blocked")
        );
    });
}

#[test]
fn tells_the_director_which_json_file_the_command_wrote_and_which_metric_keys_it_holds_even_when_the_file_is_gitignored()
 {
    local(async {
        let fixture = Spawn::new();
        let root = fixture.root();
        std::fs::write(root.join(".gitignore"), "metrics.json\n").unwrap();
        let identity = ["-c", "user.name=T", "-c", "user.email=t@example.invalid"];
        git(&root, &[&identity[..], &["add", ".gitignore"]].concat());
        git(
            &root,
            &[&identity[..], &["commit", "-qm", "ignore metrics"]].concat(),
        );
        let mission = fixture.running(mission_input(
            "Hint",
            json!({ "directions": 1, "missionAutoresearch": false }),
            "hint-create",
        ));
        let mission_id = text(&mission["entityId"]);
        let wrong = [("results/dev.json", "score"), ("metrics.json", "ece")];
        let call = Cell::new(0usize);
        let directors = Directors::new(Rc::clone(&fixture.store), move |node_id| {
            let mut spawn = spec("research_direction", node_id);
            let (result_path, metric) = wrong[call.get().min(wrong.len() - 1)];
            call.set(call.get() + 1);
            spawn["evaluation"]["resultPath"] = json!(result_path);
            spawn["evaluation"]["primaryMetric"] =
                json!({ "name": metric, "objective": "minimize", "minimumEffect": 0.01 });
            spawn["evaluation"]["guardrails"] = json!([]);
            Some(spawn)
        });
        let supervisor = fixture.missions(&directors);
        tick_until(
            60,
            || supervisor.tick().unwrap(),
            || fixture.scoped(&mission_id, "research.spawn_rejected").len() >= 2,
        )
        .await;
        let rejected: Vec<String> = fixture
            .scoped(&mission_id, "research.spawn_rejected")
            .iter()
            .map(Value::to_string)
            .collect();
        let (absent, missing_key) = (&rejected[0], &rejected[1]);
        assert!(absent.contains("results/dev.json"));
        assert!(absent.contains("metrics.json"));
        // Files the checkout itself brought in are not outputs of the command.
        assert!(!absent.contains(".nosh/project.json"));
        assert!(missing_key.contains("score"));
        assert!(missing_key.contains("f1"));
        assert!(
            directors
                .prompts
                .borrow()
                .last()
                .unwrap()
                .contains("metrics.json")
        );
    });
}

#[test]
fn accepts_a_spawn_node_when_its_child_closes_and_retries_it_when_the_child_stops() {
    local(async {
        let fixture = Spawn::new();
        let mission = fixture.running(mission_input(
            "Resolve",
            json!({ "directions": 2, "missionAutoresearch": false }),
            "resolve-create",
        ));
        let mission_id = text(&mission["entityId"]);
        let directors = Directors::new(Rc::clone(&fixture.store), |node_id| {
            Some(spec("research_direction", node_id))
        });
        let supervisor = fixture.missions(&directors);
        tick_until(
            90,
            || supervisor.tick().unwrap(),
            || fixture.directions().len() >= 2,
        )
        .await;
        let directions = fixture.directions();
        let (closed, stopped) = (&directions[0], &directions[1]);
        // Closure needs a reviewed closure packet; the mapping only reads the child's terminal
        // state.
        fixture.rewrite(
            &fixture.direction(&text(&closed["entityId"])),
            Some("closed"),
            |_| {},
        );
        fixture
            .daemon
            .research
            .transition_direction(
                &fixture.project_id,
                &text(&stopped["entityId"]),
                stopped["version"].as_i64().unwrap(),
                "stopped",
                "child-stopped",
            )
            .unwrap();
        supervisor.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let nodes = fixture.nodes(&mission_id);
        let node = |child: &Value| {
            nodes
                .iter()
                .find(|node| node["id"] == child["value"]["origin"]["nodeId"])
                .unwrap()
                .clone()
        };
        assert_eq!(node(closed)["state"], "accepted");
        assert_match(&node(stopped), &json!({ "state": "ready", "attempt": 1 }));
        assert_eq!(
            sorted(
                fixture
                    .scoped(&mission_id, "mission.child_resolved")
                    .iter()
                    .map(|payload| text(&payload["outcome"]))
                    .collect()
            ),
            ["accepted", "failed"]
        );
        // The stopped child's unused share returns to the pool, so the retried node can spawn a
        // replacement.
        tick_until(
            60,
            || supervisor.tick().unwrap(),
            || {
                fixture.directions().len() >= 3
                    || !fixture
                        .scoped(&mission_id, "research.spawn_rejected")
                        .is_empty()
            },
        )
        .await;
        assert_eq!(
            fixture.scoped(&mission_id, "research.spawn_rejected"),
            Vec::<Value>::new()
        );
        assert_eq!(fixture.directions().len(), 3);
    });
}

#[test]
fn concludes_a_hypothesis_on_measured_results_when_the_research_director_stops_exploring_and_halts_only_without_any()
 {
    for measured in [true, false] {
        local(async {
            let fixture = Spawn::new();
            fixture.running(mission_input(
                "Conclude",
                json!({ "directions": 1, "missionAutoresearch": false }),
                &format!("conclude-create-{measured}"),
            ));
            let directors = Directors::new(Rc::clone(&fixture.store), |node_id| {
                Some(if node_id.starts_with("dnode_") {
                    json!({ "nodeId": node_id, "kind": "autoresearch", "question": "A distinct question", "familyTags": ["family-next"], "scope": ["src/**"] })
                } else {
                    spec("research_direction", node_id)
                })
            });
            let supervisor = fixture.missions(&directors);
            tick_until(
                90,
                || supervisor.tick().unwrap(),
                || !fixture.directions().is_empty(),
            )
            .await;
            let child = fixture.directions()[0].clone();
            let child_id = text(&child["entityId"]);
            let mission_id = text(&child["value"]["missionId"]);
            fixture.accept_baseline(&child_id, "fixture-conclude-baseline");
            let directions = fixture.directions_supervisor(&directors);
            tick_until(
                60,
                || directions.tick().unwrap(),
                || !fixture.runs().is_empty(),
            )
            .await;
            let run_id = text(&fixture.runs()[0]["entityId"]);
            // The first run finished without a promoted winner; with `measured` it evaluated one
            // experiment.
            fixture.rewrite(&fixture.run(&run_id), Some("completed"), |_| {});
            if measured {
                fixture.record(
                    json!({ "$schema": schema_uri("experiment-result"), "autoresearchId": run_id }),
                );
            }
            directors.halts.set(true);
            for _ in 0..20 {
                if fixture.direction(&child_id)["state"] == "blocked"
                    || !fixture
                        .scoped(&mission_id, "direction.director_concluded")
                        .is_empty()
                {
                    break;
                }
                directions.tick().unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let hypothesis = fixture.direction(&child_id)["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["title"] == "Evaluate bounded hypothesis families")
                .unwrap()
                .clone();
            if measured {
                assert_eq!(
                    fixture
                        .scoped(&mission_id, "direction.director_concluded")
                        .iter()
                        .map(|payload| text(&payload["autoresearchId"]))
                        .collect::<Vec<_>>(),
                    [run_id.as_str()]
                );
                assert_eq!(hypothesis["state"], "accepted");
                assert_ne!(fixture.direction(&child_id)["state"], "blocked");
            } else {
                assert_eq!(fixture.direction(&child_id)["state"], "blocked");
            }
        });
    }
}

#[test]
fn lets_a_mission_spawned_direction_start_its_own_autoresearch_and_stops_its_children_when_the_mission_stops()
 {
    local(async {
        let fixture = Spawn::new();
        let mission = fixture.running(mission_input(
            "Direction child",
            json!({ "directions": 1, "missionAutoresearch": false }),
            "direction-child-create",
        ));
        let mission_id = text(&mission["entityId"]);
        let direction_specs = Cell::new(0usize);
        // The first Director spec scopes the search to the frozen evaluation's result file only,
        // which is refused with a reason.
        let directors = Directors::new(Rc::clone(&fixture.store), move |node_id| {
            Some(if node_id.starts_with("dnode_") {
                direction_specs.set(direction_specs.get() + 1);
                let count = direction_specs.get();
                let scope = if count == 1 { "metrics.json" } else { "src/**" };
                json!({ "nodeId": node_id, "kind": "autoresearch", "question": format!("Distinct question {count}"), "familyTags": [format!("family-{count}")], "scope": [scope] })
            } else {
                spec("research_direction", node_id)
            })
        });
        let supervisor = fixture.missions(&directors);
        tick_until(
            90,
            || supervisor.tick().unwrap(),
            || !fixture.directions().is_empty(),
        )
        .await;
        let child = fixture.directions()[0].clone();
        let child_id = text(&child["entityId"]);
        fixture.accept_baseline(&child_id, "fixture-baseline");
        let directions = fixture.directions_supervisor(&directors);
        tick_until(
            60,
            || directions.tick().unwrap(),
            || !fixture.runs().is_empty(),
        )
        .await;
        let runs = fixture.runs();
        assert_eq!(runs.len(), 1);
        assert_match(
            &runs[0]["value"],
            &json!({ "directionId": child_id, "invokedByRole": "research_director", "decisionQuestion": child["value"]["question"], "origin": { "scopeType": "direction", "scopeId": child_id } }),
        );
        let first_id = text(&runs[0]["entityId"]);
        // An Autoresearch left blocked with nobody working on it must not hold the Direction
        // forever. TypeScript moves `Date.now` 11 minutes ahead; here the block is dated 11
        // minutes back.
        let block_stale = |run_id: &str, key: &str| {
            let run = fixture.run(run_id);
            let blocked = fixture
                .daemon
                .research
                .transition_autoresearch(
                    &fixture.project_id,
                    run_id,
                    run["version"].as_i64().unwrap(),
                    "blocked",
                    key,
                )
                .unwrap();
            let stale = nosh_wire::iso_from_ms(now_ms() - 11.0 * 60_000.0).unwrap();
            fixture.rewrite(&blocked, None, |value| value["updatedAt"] = json!(stale));
        };
        block_stale(&first_id, "fixture-ar-blocked");
        directions.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fixture.run(&first_id)["state"], "stopped");
        assert_eq!(
            fixture
                .scoped(&mission_id, "direction.autoresearch_stopped")
                .iter()
                .map(|payload| text(&payload["autoresearchId"]))
                .collect::<Vec<_>>(),
            [first_id.as_str()]
        );
        // Runs stopped before evaluating anything did no research, so they do not use up the
        // Direction's Autoresearch cap.
        let next_run = async |count: usize| {
            for _ in 0..40 {
                if fixture.runs().len() >= count {
                    break;
                }
                directions.tick().unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        next_run(2).await;
        assert!(
            Value::Array(fixture.scoped(&mission_id, "research.spawn_rejected"))
                .to_string()
                .contains("frozen evaluation")
        );
        let second = fixture
            .runs()
            .into_iter()
            .find(|run| run["entityId"] != first_id.as_str())
            .expect("a second Autoresearch");
        block_stale(&text(&second["entityId"]), "fixture-ar-blocked-2");
        next_run(3).await;
        assert_eq!(fixture.runs().len(), 3);
        fixture
            .daemon
            .control_mission(
                &fixture.project_id,
                &mission_id,
                fixture.mission(&mission_id)["version"].as_i64().unwrap(),
                "stop",
                "immediate",
                "stop-mission-0001",
            )
            .await
            .unwrap();
        assert_eq!(fixture.direction(&child_id)["state"], "stopped");
        assert_eq!(fixture.run(&first_id)["state"], "stopped");
    });
}

// Rust-only: the TypeScript suite never reaches the give-up path (found by mutation testing).

#[test]
fn spends_one_node_attempt_after_three_rejected_specs_so_a_director_that_cannot_specify_does_not_loop()
 {
    local(async {
        let fixture = Spawn::new();
        let mission = fixture.running(mission_input(
            "Give up",
            json!({ "directions": 1, "missionAutoresearch": false }),
            "giveup-create",
        ));
        let mission_id = text(&mission["entityId"]);
        let node_id = text(
            &mission["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["type"] == "research_direction")
                .unwrap()["id"],
        );
        let directors = Directors::new(Rc::clone(&fixture.store), |node_id| {
            let mut spawn = spec("research_direction", node_id);
            spawn["evaluation"]["commandId"] = json!("command_missing");
            Some(spawn)
        });
        let supervisor = fixture.missions(&directors);
        let attempt = || {
            fixture
                .nodes(&mission_id)
                .iter()
                .find(|node| node["id"] == node_id.as_str())
                .unwrap()["attempt"]
                .as_i64()
                .unwrap()
        };
        tick_until(60, || supervisor.tick().unwrap(), || attempt() >= 1).await;
        assert_eq!(attempt(), 1);
        let first_attempt = fixture
            .scoped(&mission_id, "research.spawn_rejected")
            .into_iter()
            // A spawn names the attempt it would start: the node's attempt plus one.
            .filter(|payload| payload["nodeId"] == node_id.as_str() && payload["attempt"] == 1)
            .count();
        assert_eq!(first_attempt, 3);
        assert!(fixture.directions().is_empty());
    });
}
