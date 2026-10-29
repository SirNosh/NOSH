//! Ports of `apps/noshd/src/daemon-jobs.test.ts`, `daemon-activity.test.ts`, and
//! `daemon-code-events.test.ts`, plus the Rust-only checks of the single-instance lock and of the
//! bridge between a real Pi sidecar and the daemon thread.
//!
//! The bridge test needs `npm ci` in `sidecar/pi-host`.

mod common;

use common::{FakeAgents, daemon, daemon_options, inspection, local, repository_root};
use nosh_agent::pi::{PiAdapter, PiConfig};
use nosh_agent::policy::Role;
use nosh_orchestration::daemon::{NoshDaemon, bridge};
use nosh_orchestration::single_instance::SingleInstanceLock;
use nosh_sys::jobs::{JobSpec, Runner};
use nosh_wire::{EVENT_SCHEMA, IdPrefix, create_id};
use serde_json::{Value, json};
use std::path::Path;
use std::rc::Rc;

fn project(daemon: &Rc<NoshDaemon>, directory: &Path, title: &str) -> (String, String) {
    let project = daemon
        .initialize_project(&directory.join("repository"), true, Some(title), None)
        .unwrap();
    (
        project["projectId"].as_str().unwrap().to_owned(),
        project["repositoryRoot"].as_str().unwrap().to_owned(),
    )
}

fn spec(
    project_id: &str,
    root: &str,
    run_id: String,
    experiment_id: String,
    command: &[&str],
    timeout_seconds: f64,
) -> JobSpec {
    JobSpec {
        job_id: create_id(IdPrefix::Job),
        project_id: project_id.into(),
        mission_id: None,
        direction_id: None,
        autoresearch_id: None,
        run_id,
        experiment_id: Some(experiment_id),
        commit_sha: "0".repeat(40),
        working_directory: root.into(),
        runner: Runner::Native,
        distribution: None,
        command: command.iter().map(|arg| (*arg).to_owned()).collect(),
        checkpoint_command: None,
        environment_lock_hash: format!("sha256:{}", "0".repeat(64)),
        evaluation_contract_hash: format!("sha256:{}", "0".repeat(64)),
        timeout_seconds,
        uses_gpu: false,
    }
}

#[test]
fn rejects_malformed_scope_ids_before_persisting_a_job_that_could_never_leave_starting() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let (project_id, root) = project(&daemon, directory.path(), "Job fixture");
        let spec = spec(
            &project_id,
            &root,
            "run_live".into(),
            "exp_live".into(),
            &["node", "-e", "0"],
            30.0,
        );
        assert!(daemon.start_job(&spec).is_err());
        assert!(
            daemon
                .jobs
                .list()
                .unwrap()
                .iter()
                .all(|job| job.spec.job_id != spec.job_id)
        );
        daemon.stop();
    });
}

#[test]
fn activity_is_empty_while_idle_and_names_a_running_job_which_nosh_stop_if_idle_refuses_to_interrupt()
 {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let (project_id, root) = project(&daemon, directory.path(), "Activity fixture");
        assert!(daemon.activity().unwrap().is_empty());
        let spec = spec(
            &project_id,
            &root,
            create_id(IdPrefix::Run),
            create_id(IdPrefix::Exp),
            &["node", "-e", "setTimeout(() => {}, 30000)"],
            60.0,
        );
        daemon.start_job(&spec).unwrap();
        let activity = daemon.activity().unwrap();
        daemon
            .jobs
            .cancel(&spec.job_id, "cancelled_by_user")
            .unwrap();
        assert_eq!(
            activity
                .iter()
                .map(|(project, _)| project.as_str())
                .collect::<Vec<_>>(),
            [project_id.as_str()]
        );
        assert!(
            activity[0]
                .1
                .iter()
                .any(|entry| entry.starts_with(&format!("job {}:", spec.job_id))),
            "{activity:?}"
        );
        daemon.stop();
    });
}

#[test]
fn activity_skips_idle_sessions_and_names_a_working_one() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let (project_id, _) = project(&daemon, directory.path(), "Agent activity");
        let agent_id = create_id(IdPrefix::Agt);
        agents
            .sessions
            .borrow_mut()
            .push(inspection(&project_id, &agent_id, None, Role::Nosh));
        assert!(daemon.activity().unwrap().is_empty());
        agents.sessions.borrow_mut()[0].status = "running".into();
        assert_eq!(
            daemon.activity().unwrap(),
            [(project_id, vec![format!("agent {agent_id}:running")])]
        );
        daemon.stop();
    });
}

#[test]
fn a_user_cancel_records_its_reason_once_and_replays_under_the_same_key() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let (project_id, root) = project(&daemon, directory.path(), "Job control");
        let spec = spec(
            &project_id,
            &root,
            create_id(IdPrefix::Run),
            create_id(IdPrefix::Exp),
            &["node", "-e", "setTimeout(() => {}, 30000)"],
            60.0,
        );
        daemon.start_job(&spec).unwrap();
        let cancelled = daemon
            .control_job(&project_id, &spec.job_id, "cancel", "cancel-once")
            .unwrap();
        assert_eq!(
            cancelled.failure_reason.as_deref(),
            Some("cancelled_by_user")
        );
        assert_eq!(
            daemon
                .control_job(&project_id, &spec.job_id, "cancel", "cancel-once")
                .unwrap(),
            cancelled
        );
        let completed = daemon
            .replay(&project_id, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event["type"] == "job.control_completed")
            .count();
        assert_eq!(completed, 1);
        daemon.stop();
    });
}

#[test]
fn nosh_code_inner_call_events_are_stored_and_replayed_with_their_parent_tool_call_id() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let (project_id, _) = project(&daemon, directory.path(), "Code events");
        let scope = json!({ "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": create_id(IdPrefix::Agt) });
        let inner = |kind: &str, payload: Value| json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": kind, "source": "pi", "correlationId": create_id(IdPrefix::Tsk), "causationId": null, "scope": scope, "payload": payload });
        // The adapter's event sink is the daemon's append_draft: the same path every Pi tool event takes.
        daemon.append_draft(&inner("agent.tool_started", json!({ "toolCallId": "call_code:0", "toolName": "nosh_workspace_read", "args": ["src/a.ts"], "parentToolCallId": "call_code" }))).unwrap();
        daemon.append_draft(&inner("agent.tool_completed", json!({ "toolCallId": "call_code:0", "toolName": "nosh_workspace_read", "isError": false, "result": "export const a = 1;\n", "parentToolCallId": "call_code" }))).unwrap();
        let replayed: Vec<Value> = daemon
            .replay(&project_id, 0)
            .unwrap()
            .iter()
            .filter(|event| event["type"].as_str().unwrap().starts_with("agent.tool_"))
            .map(|event| json!([event["type"], event["payload"]["parentToolCallId"]]))
            .collect();
        assert_eq!(
            replayed,
            [
                json!(["agent.tool_started", "call_code"]),
                json!(["agent.tool_completed", "call_code"])
            ]
        );
        daemon.stop();
    });
}

#[test]
fn a_second_instance_is_refused_until_the_first_releases_and_a_dead_owners_lock_is_reclaimed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("noshd.lock");
    let first = SingleInstanceLock::new(&path);
    first.acquire().unwrap();
    let second = SingleInstanceLock::new(&path);
    let refused = second.acquire().unwrap_err().to_string();
    assert!(refused.contains("Another noshd instance owns"), "{refused}");
    // Release removes only a lock this instance wrote.
    second.release();
    assert!(path.exists());
    first.release();
    assert!(!path.exists());
    second.acquire().unwrap();
    drop(second);
    assert!(!path.exists());
    // A lock whose owner has exited is stale.
    let mut exited = std::process::Command::new("node")
        .args(["-e", "0"])
        .spawn()
        .unwrap();
    let pid = exited.id();
    exited.wait().unwrap();
    std::fs::write(&path, json!({ "pid": pid, "token": "stale" }).to_string()).unwrap();
    let third = SingleInstanceLock::new(&path);
    third.acquire().unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains(&std::process::id().to_string())
    );
}

#[test]
fn a_real_pi_session_reaches_the_daemon_through_the_bridge_and_its_events_are_stored_before_the_prompt_returns()
 {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let (connected, inbox) = bridge();
        let config = PiConfig {
            node: "node".into(),
            host_script: repository_root().join("sidecar/pi-host/host.mjs"),
            env: vec![("NOSH_PI_HOST_TEST".into(), "1".into())],
            schema_dir: repository_root().join("conformance/schemas/json"),
            forward_stderr: true,
        };
        let adapter = Rc::new(
            PiAdapter::spawn(
                config,
                connected.emitter(),
                Some(connected.agent_host()),
                Some(connected.terminal_host()),
            )
            .await
            .unwrap(),
        );
        let daemon = NoshDaemon::new(
            daemon_options(&directory.path().join("data")),
            Rc::clone(&adapter) as Rc<dyn nosh_orchestration::Agents>,
            (connected, inbox),
        )
        .unwrap();
        let (project_id, root) = project(&daemon, directory.path(), "Bridge fixture");
        adapter.call("test.runtime", json!({ "faux": { "provider": "bridge-proof", "api": "bridge-proof-api", "models": [{ "id": "deterministic", "reasoning": false }] } })).await.unwrap();
        adapter
            .call("test.respond", json!({ "steps": [{ "toolCalls": [{ "name": "nosh_project_contract_submit", "arguments": { "record": { "$schema": "https://nosh.dev/schemas/project-contract/v1" } }, "id": "call_contract" }] }, { "text": "The contract needs every field first." }] }))
            .await
            .unwrap();
        let agent_id = create_id(IdPrefix::Agt);
        let options = serde_json::from_value(json!({
            "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "taskId": null,
            "agentId": agent_id, "role": "nosh", "cwd": root, "packagePath": repository_root().join("pi-package"),
            "model": { "provider": "bridge-proof", "id": "deterministic" },
        }))
        .unwrap();
        daemon.start_agent(options).await.unwrap();
        daemon
            .agents
            .prompt(&agent_id, "Approve the contract.", None)
            .await
            .unwrap();
        // Emitted on the daemon's thread, each event was stored before the prompt's reply.
        let events: Vec<Value> = daemon
            .replay(&project_id, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event["scope"]["agentId"] == agent_id.as_str())
            .collect();
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert!(types.contains(&"agent.completed"), "{types:?}");
        let submitted = events
            .iter()
            .find(|event| {
                event["type"] == "agent.tool_completed"
                    && event["payload"]["toolName"] == "nosh_project_contract_submit"
            })
            .expect("the submission result");
        // The tool's submission crossed into the daemon and its submission gate's refusal came back.
        let receipt = &submitted["payload"]["result"]["details"]["receipt"];
        assert_eq!(receipt["retryAllowed"], true, "{submitted}");
        assert!(
            receipt["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["pointer"] == "/templateVersion"),
            "{submitted}"
        );
        adapter.stop(&agent_id);
        daemon.stop();
    });
}

#[test]
fn starts_its_research_supervisors_with_the_daemon_and_holds_them_without_a_reference_cycle() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        // The supervisors reach the daemon weakly: a cycle would keep it, and its bridge, alive.
        assert_eq!(Rc::strong_count(&daemon), 1);
        let (project_id, _) = project(&daemon, directory.path(), "Supervised");
        let input = serde_json::from_value(json!({ "title": "Supervised", "objective": "Run under the daemon", "deliverables": ["Result"], "successCriteria": ["Reviewed"], "idempotencyKey": "supervised-create" })).unwrap();
        let mut mission = daemon.research.create_mission(&project_id, &input).unwrap();
        let mission_id = mission["entityId"].as_str().unwrap().to_owned();
        for next in ["planning", "awaiting_approval", "running"] {
            mission = daemon
                .research
                .transition_mission(
                    &project_id,
                    &mission_id,
                    mission["version"].as_i64().unwrap(),
                    next,
                    &format!("supervised-{next}"),
                )
                .unwrap();
        }
        let director = || {
            agents
                .started
                .borrow()
                .iter()
                .any(|session| session.role == Role::MissionDirector)
        };
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        assert!(!director(), "supervisors tick only once the daemon starts");
        daemon.start().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !director() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(director());
        daemon.stop();
        let weak = Rc::downgrade(&daemon);
        drop(daemon);
        assert!(weak.upgrade().is_none());
    });
}
