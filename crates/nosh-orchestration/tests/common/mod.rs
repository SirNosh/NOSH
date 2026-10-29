//! Helpers shared by the daemon test files.
#![allow(dead_code)]

use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::pi::{Inspection, TerminalStep};
use nosh_agent::policy::{Role, SessionOptions};
use nosh_agent::terminal::TerminalContext;
use nosh_orchestration::Agents;
use nosh_orchestration::daemon::{DaemonOptions, NoshDaemon, bridge};
use nosh_research::research_control::MissionInput;
use nosh_wire::{EVENT_SCHEMA, IdPrefix, create_id};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Vitest's `toMatchObject`: every field of `expected` is in `actual`, recursively; arrays match
/// element-wise with equal lengths.
pub fn assert_match(actual: &Value, expected: &Value) {
    fn check(actual: &Value, expected: &Value, path: &str) -> Result<(), String> {
        match (actual, expected) {
            (Value::Object(actual), Value::Object(expected)) => {
                for (key, value) in expected {
                    let found = actual
                        .get(key)
                        .ok_or_else(|| format!("{path}/{key} is missing"))?;
                    check(found, value, &format!("{path}/{key}"))?;
                }
                Ok(())
            }
            (Value::Array(actual), Value::Array(expected)) => {
                if actual.len() != expected.len() {
                    return Err(format!(
                        "{path} has {} items, expected {}",
                        actual.len(),
                        expected.len()
                    ));
                }
                for (index, (found, value)) in actual.iter().zip(expected).enumerate() {
                    check(found, value, &format!("{path}/{index}"))?;
                }
                Ok(())
            }
            (Value::Number(a), Value::Number(b)) if a.as_f64() == b.as_f64() => Ok(()),
            _ if actual == expected => Ok(()),
            _ => Err(format!("{path}: {actual} != {expected}")),
        }
    }
    if let Err(message) = check(actual, expected, "") {
        panic!("{message}\nactual: {actual:#}");
    }
}

/// Runs a test body on one thread inside a `LocalSet`, as the daemon runs.
pub fn local<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(tokio::task::LocalSet::new().run_until(future))
}

pub fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

/// A live session's inspection.
pub fn inspection(
    project_id: &str,
    agent_id: &str,
    task_id: Option<&str>,
    role: Role,
) -> Inspection {
    Inspection {
        project_id: project_id.to_owned(),
        mission_id: None,
        direction_id: None,
        autoresearch_id: None,
        experiment_id: None,
        run_id: None,
        job_id: None,
        task_id: task_id.map(str::to_owned),
        agent_id: agent_id.to_owned(),
        role,
        pi_session_id: format!("pi-{agent_id}"),
        status: "idle".into(),
        current_tool: None,
        active_tool_ids: Vec::new(),
        started_at: "2026-10-28T00:00:00.000Z".into(),
        last_event_at: "2026-10-28T00:00:00.000Z".into(),
        model_provider: Some("test".into()),
        model_id: Some("old".into()),
        model_name: Some("Old".into()),
        thinking_level: "off".into(),
        context_tokens: None,
        context_window: None,
        context_percent: None,
    }
}

/// Session liveness as a fixture: the sessions `inspect` reports, whether terminal turns are
/// active, and the prompts and starts the daemon sent.
#[derive(Default)]
pub struct FakeAgents {
    pub sessions: RefCell<Vec<Inspection>>,
    pub turn_active: Cell<bool>,
    pub started: RefCell<Vec<SessionOptions>>,
    pub prompts: RefCell<Vec<(String, String)>>,
    pub stopped: RefCell<Vec<String>>,
    pub followed: RefCell<Vec<(String, String)>>,
    pub thinking: RefCell<Vec<(String, String)>>,
    pub validated: Cell<usize>,
}

impl FakeAgents {
    /// Whether the daemon asked these sessions for anything.
    pub fn untouched(&self) -> bool {
        self.started.borrow().is_empty()
            && self.prompts.borrow().is_empty()
            && self.stopped.borrow().is_empty()
            && self.followed.borrow().is_empty()
            && self.thinking.borrow().is_empty()
            && self.validated.get() == 0
    }
}

impl Agents for FakeAgents {
    fn start(&self, options: SessionOptions) -> LocalBoxFuture<'_, Result<Inspection, String>> {
        let mut session = inspection(
            &options.project_id,
            &options.agent_id,
            options.task_id.as_deref(),
            options.role,
        );
        session.mission_id = options.mission_id.clone();
        session.direction_id = options.direction_id.clone();
        session.autoresearch_id = options.autoresearch_id.clone();
        session.experiment_id = options.experiment_id.clone();
        self.sessions
            .borrow_mut()
            .retain(|existing| existing.agent_id != options.agent_id);
        self.sessions.borrow_mut().push(session.clone());
        self.started.borrow_mut().push(options);
        async move { Ok(session) }.boxed_local()
    }

    fn prompt<'a>(
        &'a self,
        agent_id: &'a str,
        prompt: &'a str,
        _step: Option<&'a TerminalStep>,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        self.prompts
            .borrow_mut()
            .push((agent_id.to_owned(), prompt.to_owned()));
        async { Ok(()) }.boxed_local()
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
        self.stopped.borrow_mut().push(agent_id.to_owned());
        self.sessions
            .borrow_mut()
            .retain(|session| session.agent_id != agent_id);
    }

    fn inspect(&self) -> Vec<Inspection> {
        self.sessions.borrow().clone()
    }

    fn follow_up<'a>(
        &'a self,
        agent_id: &'a str,
        message: &'a str,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        self.followed
            .borrow_mut()
            .push((agent_id.to_owned(), message.to_owned()));
        async { Ok(()) }.boxed_local()
    }

    fn set_thinking_level<'a>(
        &'a self,
        agent_id: &'a str,
        level: &'a str,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        self.thinking
            .borrow_mut()
            .push((agent_id.to_owned(), level.to_owned()));
        async { Ok(()) }.boxed_local()
    }

    fn validate_model_selection<'a>(
        &'a self,
        _selection: Option<&'a Value>,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        self.validated.set(self.validated.get() + 1);
        async { Ok(()) }.boxed_local()
    }

    fn terminal_turn_active(&self, _context: &TerminalContext) -> bool {
        self.turn_active.get()
    }
}

pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `packages/jobs/src/exit-wrapper.ts` as plain JavaScript: the nosh-sys wrapper binary is not
/// built for another crate's tests, and both write the same `exit.json`.
const EXIT_WRAPPER: &str = r#"import { spawn } from "node:child_process";
import { renameSync, writeFileSync } from "node:fs";
const [exitFile, separator, file, ...args] = process.argv.slice(2);
if (!exitFile || separator !== "--" || !file) process.exit(2);
let recorded = false;
function record(exitCode, signal, error) {
  if (recorded) return;
  recorded = true;
  const temporary = `${exitFile}.tmp`;
  writeFileSync(temporary, `${JSON.stringify({ exitCode, signal, finishedAt: new Date().toISOString(), ...(error === undefined ? {} : { error }) })}\n`, "utf8");
  renameSync(temporary, exitFile);
}
const child = spawn(file, args, { stdio: "inherit", windowsHide: true });
child.once("error", (error) => { record(null, null, error.message); process.exit(1); });
child.once("exit", (code, signal) => {
  record(code, signal);
  process.exitCode = code ?? 1;
  if (signal) process.kill(process.pid, signal);
});
"#;

/// The Job exit-wrapper command, written into `directory`.
pub fn job_wrapper(directory: &Path) -> Vec<std::ffi::OsString> {
    std::fs::create_dir_all(directory).unwrap();
    let script = directory.join("exit-wrapper.mjs");
    std::fs::write(&script, EXIT_WRAPPER).unwrap();
    vec!["node".into(), script.into_os_string()]
}

pub fn daemon_options(data_directory: &Path) -> DaemonOptions {
    DaemonOptions {
        data_directory: data_directory.to_path_buf(),
        bootstrap_token: "test".into(),
        package_path: repository_root().join("pi-package"),
        maximum_active_pi_sessions: None,
        default_model: None,
        job_wrapper: job_wrapper(data_directory),
        schema_dir: repository_root().join("conformance/schemas/json"),
    }
}

/// A daemon over fake sessions; call inside [`local`].
pub fn daemon(data_directory: &Path, agents: Rc<FakeAgents>) -> Rc<NoshDaemon> {
    NoshDaemon::new(daemon_options(data_directory), agents, bridge()).unwrap()
}

pub struct Worker {
    pub project_id: String,
    pub task_id: String,
    pub agent_id: String,
}

/// A running Mission whose first node is leased to a live worker session holding a Task Packet
/// (attempt 1) with 100 model tokens and 100 seconds, built as `mission-supervisor.test.ts`
/// builds it.
pub fn leased_worker(daemon: &Rc<NoshDaemon>, agents: &FakeAgents, directory: &Path) -> Worker {
    let project = daemon
        .initialize_project(
            &directory.join("repository"),
            true,
            Some("Delegation fixture"),
            None,
        )
        .unwrap();
    let project_id = project["projectId"].as_str().unwrap().to_owned();
    let research = &daemon.research;
    let input = MissionInput {
        title: "Delegation".into(),
        objective: "Delegate a check".into(),
        deliverables: vec!["State".into()],
        success_criteria: vec!["Delegations are bounded".into()],
        non_objectives: None,
        starting_evidence: None,
        research_plan: None,
        budgets: None,
        idempotency_key: "delegation-create".into(),
    };
    let mut mission = research.create_mission(&project_id, &input).unwrap();
    let mission_id = mission["entityId"].as_str().unwrap().to_owned();
    let version = |mission: &Value| mission["version"].as_i64().unwrap();
    for next in ["planning", "awaiting_approval", "running"] {
        mission = research
            .transition_mission(
                &project_id,
                &mission_id,
                version(&mission),
                next,
                &format!("delegation-{next}"),
            )
            .unwrap();
    }
    let (task_id, agent_id) = (create_id(IdPrefix::Tsk), create_id(IdPrefix::Agt));
    let node_id = mission["value"]["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let expires = nosh_wire::iso_from_ms(now + 600_000.0).unwrap();
    let lease = json!({ "leaseId": task_id, "ownerId": agent_id, "version": version(&mission), "expiresAt": expires });
    mission = research
        .transition_mission_node(
            &project_id,
            &mission_id,
            version(&mission),
            &node_id,
            "leased",
            "delegation-lease",
            Some(&lease),
            false,
        )
        .unwrap();
    mission = research
        .transition_mission_node(
            &project_id,
            &mission_id,
            version(&mission),
            &node_id,
            "working",
            "delegation-working",
            None,
            false,
        )
        .unwrap();
    let scope = json!({ "projectId": project_id, "missionId": mission_id, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": agent_id });
    let packet = json!({ "$schema": schema_uri("task-packet"), "taskId": task_id, "attempt": 1, "assignedRole": "general_worker", "assignedAgentId": agent_id, "scope": { "projectId": project_id, "missionId": mission_id, "directionId": null, "autoresearchId": null, "experimentId": null, "graphNodeId": node_id }, "observedVersions": { "missionGraphVersion": mission["value"]["graphVersion"], "leaseVersion": mission["version"] }, "lease": { "leaseId": task_id, "expiresAt": expires }, "budget": { "maximumModelTokens": 100, "maximumWallClockSeconds": 100 }, "permissions": { "delegation": "request_only" } });
    daemon
        .append_draft(&json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "noshd", "scope": scope, "correlationId": format!("task:{task_id}"), "causationId": null, "payload": packet }))
        .unwrap();
    let mut session = inspection(&project_id, &agent_id, Some(&task_id), Role::GeneralWorker);
    session.mission_id = Some(mission_id);
    agents.sessions.borrow_mut().push(session);
    Worker {
        project_id,
        task_id,
        agent_id,
    }
}
