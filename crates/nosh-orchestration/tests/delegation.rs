//! Rust-only checks of daemon delegation (the TypeScript suite covers only its domain effect):
//! durable refusal of an over-budget sibling and its replay, authorization through the
//! runtime to a terminal outcome, and recovery after a restart. A worker's live leased Mission
//! node is `common::leased_worker`.

mod common;

use common::{FakeAgents, Worker, assert_match, daemon, leased_worker, local};
use nosh_orchestration::daemon::NoshDaemon;
use serde_json::{Value, json};
use std::rc::Rc;
use std::time::Duration;

fn request(worker: &Worker, request_id: &str, model_tokens: u64, wall_clock_seconds: u64) -> Value {
    let path = common::repository_root()
        .join("conformance/schemas/fixtures/delegation-request/minimal.valid.json");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    record["requestId"] = json!(request_id);
    record["requestingTaskId"] = json!(worker.task_id);
    record["requestingAgentId"] = json!(worker.agent_id);
    record["requestedRole"] = json!("general_worker");
    record["proposedObjective"] = json!("Check the baseline");
    record["submittedAt"] = json!(nosh_wire::now_utc());
    record["budgetEstimate"] =
        json!({ "modelTokens": model_tokens, "wallClockSeconds": wall_clock_seconds });
    record
}

async fn delegate(daemon: &Rc<NoshDaemon>, worker: &Worker, record: &Value) -> Value {
    let submitted = daemon
        .submit_tool(
            "nosh_delegation_request",
            &worker.project_id,
            &format!("task:{}", worker.task_id),
            record,
            Some(&worker.agent_id),
        )
        .await
        .unwrap();
    assert_eq!(submitted["accepted"], true, "{submitted}");
    submitted["delegation"].clone()
}

fn delegation(daemon: &Rc<NoshDaemon>, worker: &Worker, request_id: &str) -> Value {
    daemon
        .store_for(&worker.project_id)
        .unwrap()
        .projection(&worker.project_id, "delegation", request_id)
        .unwrap()
        .expect("a durable delegation")
}

fn events(daemon: &Rc<NoshDaemon>, worker: &Worker, kind: &str) -> Vec<Value> {
    daemon
        .replay(&worker.project_id, 0)
        .unwrap()
        .into_iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

fn intents(daemon: &Rc<NoshDaemon>, worker: &Worker, operation: &str) -> Vec<Value> {
    daemon
        .store_for(&worker.project_id)
        .unwrap()
        .operation_intents(&worker.project_id, None)
        .unwrap()
        .into_iter()
        .filter(|intent| intent["operationType"] == operation)
        .collect()
}

/// Waits for the delegation's run to reach a terminal state.
async fn settled(daemon: &Rc<NoshDaemon>, worker: &Worker, request_id: &str) -> Value {
    for _ in 0..400 {
        let current = delegation(daemon, worker, request_id);
        if current["state"] != "authorized" {
            return current;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("delegation {request_id} never settled");
}

#[test]
fn refuses_a_sibling_beyond_the_remaining_task_packet_budget_durably_and_replays_the_same_refusal()
{
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let worker = leased_worker(&daemon, &agents, directory.path());
        delegate(&daemon, &worker, &request(&worker, "ref_first", 60, 10)).await;
        // Reservations count whether pending or completed, so the first's 60 tokens leave 40.
        let record = request(&worker, "ref_sibling", 60, 10);
        let expected = json!({ "state": "failed", "error": "Delegation siblings exceed the durable parent Task Packet budget" });
        assert_eq!(delegate(&daemon, &worker, &record).await, expected);
        let stored = delegation(&daemon, &worker, "ref_sibling");
        assert_match(
            &stored,
            &json!({ "state": "rejected", "value": { "state": "rejected", "error": expected["error"] } }),
        );
        // The same request replays its recorded refusal as the asserted rejection authority; nothing new is written.
        let replayed = delegate(&daemon, &worker, &record).await;
        assert_match(
            &replayed,
            &json!({ "state": "rejected", "error": expected["error"] }),
        );
        assert_eq!(replayed["terminalHash"], stored["value"]["terminalHash"]);
        assert_eq!(events(&daemon, &worker, "delegation.rejected").len(), 1);
        // The same request ID with other content conflicts at the submission gate, not as a fresh request.
        let rewritten = request(&worker, "ref_sibling", 10, 10);
        let refused = daemon
            .submit_tool(
                "nosh_delegation_request",
                &worker.project_id,
                &format!("task:{}", worker.task_id),
                &rewritten,
                Some(&worker.agent_id),
            )
            .await
            .unwrap();
        assert_match(
            &refused,
            &json!({ "accepted": false, "errors": [{ "code": "domain_policy", "message": "Domain record domain-delegation:ref_sibling conflicts with its accepted canonical payload" }] }),
        );
        assert_eq!(intents(&daemon, &worker, "delegation.reservation").len(), 1);
        daemon.stop();
    });
}

#[test]
fn authorizes_within_budget_runs_the_child_to_a_terminal_outcome_and_settles_its_reservation_once()
{
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let worker = leased_worker(&daemon, &agents, directory.path());
        let record = request(&worker, "ref_first", 60, 10);
        let authorized = delegate(&daemon, &worker, &record).await;
        for field in [
            "childTaskId",
            "childAgentId",
            "childThreadId",
            "reservationIntentId",
            "requestHash",
        ] {
            assert!(authorized[field].is_string(), "{field}: {authorized}");
        }
        assert_eq!(authorized["parentTaskId"], json!(worker.task_id));
        // The child opened in the parent's Mission scope as a background general worker.
        assert!(
            agents
                .started
                .borrow()
                .iter()
                .any(
                    |options| options.agent_id == authorized["childAgentId"].as_str().unwrap()
                        && options.mission_id == agents.sessions.borrow()[0].mission_id
                )
        );
        let outcome = settled(&daemon, &worker, "ref_first").await;
        // The fake child submits no Episode, so the runtime's own refusal is the terminal outcome.
        assert_match(
            &outcome,
            &json!({ "state": "failed", "value": { "error": "Thread step did not submit its required typed episode-draft" } }),
        );
        assert!(outcome["value"]["terminalHash"].is_string());
        let reservations = intents(&daemon, &worker, "delegation.reservation");
        assert_eq!(
            reservations
                .iter()
                .filter(|intent| intent["state"] == "completed")
                .count(),
            1,
            "{reservations:?}"
        );
        assert!(
            intents(&daemon, &worker, "delegation.runtime")
                .iter()
                .all(|intent| intent["state"] != "pending")
        );
        // A replayed request answers with the asserted terminal authority, and no second run starts.
        let started = agents.started.borrow().len();
        let replayed = delegate(&daemon, &worker, &record).await;
        assert_eq!(replayed["terminalHash"], outcome["value"]["terminalHash"]);
        assert_eq!(agents.started.borrow().len(), started);
        daemon.stop();
    });
}

#[test]
fn recovers_every_recorded_request_after_a_restart_without_rerunning_settled_ones() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let worker = local(async {
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&data, Rc::clone(&agents));
        let worker = leased_worker(&daemon, &agents, directory.path());
        delegate(&daemon, &worker, &request(&worker, "ref_run", 60, 10)).await;
        delegate(&daemon, &worker, &request(&worker, "ref_refused", 60, 10)).await;
        settled(&daemon, &worker, "ref_run").await;
        daemon.stop();
        worker
    });
    local(async {
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&data, Rc::clone(&agents));
        daemon.start().unwrap();
        // Initialization wrote the first report; the restart's is the second.
        let report = events(&daemon, &worker, "recovery.report");
        assert_eq!(report.len(), 2, "{report:?}");
        assert_match(
            &report[1]["payload"],
            &json!({ "recoveredDelegations": 0, "rejectedDelegations": 1 }),
        );
        assert!(events(&daemon, &worker, "recovery.delegation_failed").is_empty());
        assert!(agents.started.borrow().is_empty());
        daemon.stop();
    });
}
