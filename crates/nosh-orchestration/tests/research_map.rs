//! Port of `apps/noshd/src/research-map.test.ts`.

mod common;

use common::assert_match;
use nosh_orchestration::research_map::{ResearchMapInput, build_research_map};
use serde_json::{Map, Value, json};

fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

fn node(id: &str, state: &str, extra: Value) -> Value {
    let mut node = json!({ "id": id, "title": format!("Node {id}"), "type": "implementation", "state": state, "attempt": 1, "maximumAttempts": 3, "lease": null });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        node[key] = value;
    }
    node
}

fn event(kind: &str, payload: Value) -> Value {
    json!({ "type": kind, "payload": payload })
}

fn submitted(record: Value) -> (Value, Value) {
    (event("record.submitted", record.clone()), record)
}

fn ids(nodes: &Value) -> Vec<String> {
    nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap().to_owned())
        .collect()
}

fn paths(entry: &Value, prefix: &str) -> Vec<String> {
    let path = format!("{prefix}{}", entry["id"].as_str().unwrap());
    let mut all = vec![path.clone()];
    for child in entry["children"].as_array().unwrap() {
        all.extend(paths(child, &format!("{path}/")));
    }
    all
}

#[test]
fn shows_missions_directions_with_nested_autoresearch_experiments_lease_holding_workers_and_running_jobs()
 {
    let worker = json!({ "agentId": "agt_w", "role": "general_worker", "status": "running", "currentTool": "nosh_run", "modelId": "gpt-6-luna", "thinkingLevel": "low", "contextPercent": 23.4, "experimentId": null });
    let implementer = json!({ "agentId": "agt_i", "role": "general_worker", "status": "running", "currentTool": null, "modelId": null, "thinkingLevel": "low", "contextPercent": null, "experimentId": "exp_c" });
    let job = json!({ "jobId": "job_1", "command": ["node", "evaluate.mjs"], "state": "running", "runner": "native", "missionId": null, "directionId": "dir_1", "autoresearchId": "ar_1" });
    let mut done = job.clone();
    done["jobId"] = json!("job_done");
    done["state"] = json!("completed");
    let missions = [
        json!({ "entityId": "mis_done", "version": 9, "state": "completed", "value": { "title": "Old mission", "nodes": [node("m1", "accepted", json!({}))], "updatedAt": "2026-10-01T00:00:00.000Z" } }),
        json!({ "entityId": "mis_1", "version": 4, "state": "running", "value": { "title": "Boundary tests", "nodes": [node("m1", "accepted", json!({})), node("m2", "working", json!({ "lease": { "ownerId": "agt_w" } })), node("m3", "failed", json!({ "attempt": 2 }))], "updatedAt": "2026-10-03T00:00:00.000Z" } }),
    ];
    let directions = [
        json!({ "entityId": "dir_1", "version": 3, "state": "awaiting_autoresearch", "value": { "question": "Does early exit help?", "nodes": [node("d1", "accepted", json!({}))], "updatedAt": "2026-10-02T00:00:00.000Z" } }),
    ];
    let autoresearch = [
        json!({ "entityId": "ar_1", "version": 5, "state": "running", "value": { "directionId": "dir_1", "decisionQuestion": "Tune threshold", "currentRound": 2, "maximumRounds": 3, "maximumExperiments": 6, "acceptedFrontierExperimentIds": ["exp_a"] } }),
    ];
    let mut records: Vec<(Value, Value)> = ["exp_a", "exp_b", "exp_c", "exp_d", "exp_e"]
        .iter()
        .enumerate()
        .map(|(index, id)| submitted(json!({ "$schema": schema_uri("experiment-proposal"), "experimentId": id, "autoresearchId": "ar_1", "round": if index < 2 { 1 } else { 2 }, "hypothesis": format!("Hypothesis {id}") })))
        .collect();
    records.push(submitted(json!({ "$schema": schema_uri("experiment-result"), "experimentId": "exp_a", "promotionDecision": "promoted", "comparison": { "candidateScore": 0.86, "improvement": 0.02 }, "guardrails": [{ "passed": true }] })));
    records.push(submitted(json!({ "$schema": schema_uri("experiment-result"), "experimentId": "exp_b", "promotionDecision": "rejected", "comparison": { "candidateScore": 0.9, "improvement": 0.06 }, "guardrails": [{ "passed": false }] })));
    records.push(submitted(
        json!({ "$schema": schema_uri("run-manifest"), "experimentId": "exp_d" }),
    ));
    let events = [event(
        "autoresearch.experiment_failed",
        json!({ "experimentId": "exp_e", "phase": "implementation" }),
    )];
    let map = build_research_map(&ResearchMapInput {
        missions: &missions,
        directions: &directions,
        autoresearch: &autoresearch,
        submitted: &records,
        events: &events,
        agents: &[worker, implementer],
        jobs: &[job, done],
    });
    // Live work first; finished work after it.
    assert_eq!(ids(&map["roots"]), ["mis_1", "dir_1", "mis_done"]);
    let mission = &map["roots"][0];
    assert_match(
        mission,
        &json!({ "kind": "mission", "title": "Boundary tests", "state": "running", "progress": { "done": 1, "total": 3 } }),
    );
    // The version lets a client stage a retry or resume without parsing the display detail ("v3").
    assert_eq!(
        mission["version"].as_f64(),
        mission["detail"].as_str().unwrap()[1..].parse::<f64>().ok()
    );
    assert_match(
        &mission["children"][1],
        &json!({ "kind": "node", "state": "working", "children": [{ "kind": "worker", "id": "agt_w", "state": "running nosh_run", "detail": "gpt-6-luna:low · ctx 23%" }] }),
    );
    assert_match(
        &mission["children"][2],
        &json!({ "state": "failed", "detail": "implementation · attempt 2/3" }),
    );
    let run = map["roots"][1]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["kind"] == "autoresearch")
        .unwrap();
    assert_match(
        run,
        &json!({ "id": "ar_1", "detail": "round 2/3 · 5/6 experiments · best 0.86" }),
    );
    let states: Map<String, Value> = run["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| {
            (
                child["id"].as_str().unwrap().to_owned(),
                child["state"].clone(),
            )
        })
        .collect();
    assert_eq!(
        Value::Object(states),
        json!({ "exp_a": "promoted", "exp_b": "rejected", "exp_c": "implementing", "exp_d": "evaluating", "exp_e": "failed", "job_1": "running" })
    );
    let child = |id: &str| {
        run["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|child| child["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        child("exp_b")["detail"],
        "round 1 · score 0.9 (+0.06) · guardrail failed"
    );
    assert_eq!(
        child("exp_e")["detail"],
        "round 2 · failed in implementation"
    );
    // A Direction's Autoresearch is nested, never repeated as a root; finished Jobs are not shown.
    assert!(
        map["roots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|root| root["kind"] != "autoresearch")
    );
    assert!(!map.to_string().contains("job_done"));
    assert_eq!(ids(&map["workers"]), ["agt_w", "agt_i"]);
}

#[test]
fn shows_the_current_best_of_a_minimizing_climb_not_the_largest_promoted_score() {
    // Live: an ECE climb promoted 0.0097 then 0.0060, and the map reported best 0.0097.
    let autoresearch = [
        json!({ "entityId": "ar_2", "version": 3, "state": "running", "value": { "directionId": null, "decisionQuestion": "Lower ECE", "currentRound": 2, "maximumRounds": 30, "maximumExperiments": 30, "acceptedFrontierExperimentIds": ["exp_y"] } }),
    ];
    let mut records: Vec<(Value, Value)> = ["exp_x", "exp_y"]
        .iter()
        .enumerate()
        .map(|(index, id)| submitted(json!({ "$schema": schema_uri("experiment-proposal"), "experimentId": id, "autoresearchId": "ar_2", "round": index + 1, "hypothesis": format!("Hypothesis {id}") })))
        .collect();
    records.push(submitted(json!({ "$schema": schema_uri("experiment-result"), "experimentId": "exp_x", "promotionDecision": "promoted", "comparison": { "candidateScore": 0.0097, "improvement": 0.009 }, "guardrails": [] })));
    records.push(submitted(json!({ "$schema": schema_uri("experiment-result"), "experimentId": "exp_y", "promotionDecision": "promoted", "comparison": { "candidateScore": 0.006, "improvement": 0.0037 }, "guardrails": [] })));
    let map = build_research_map(&ResearchMapInput {
        missions: &[],
        directions: &[],
        autoresearch: &autoresearch,
        submitted: &records,
        events: &[],
        agents: &[],
        jobs: &[],
    });
    assert_match(
        &map["roots"][0],
        &json!({ "id": "ar_2", "detail": "round 2/30 · 2/30 experiments · best 0.006" }),
    );
}

#[test]
fn nests_each_experiment_under_the_experiment_it_started_from_and_shows_the_runs_baseline() {
    // A greedy climb: x is kept from the baseline, y discarded and z kept on top of x, w discarded on top of z.
    let autoresearch = [
        json!({ "entityId": "ar_3", "version": 4, "state": "completed", "value": { "directionId": null, "decisionQuestion": "Lower ECE", "currentRound": 4, "maximumRounds": 30, "maximumExperiments": 30, "acceptedFrontierExperimentIds": ["exp_z"], "updatedAt": "2026-10-18T02:13:29.000Z", "evaluationContract": { "primaryMetric": { "name": "ece", "objective": "minimize" }, "baselineMetrics": { "ece": 0.0187, "f1": 0.9 } } } }),
    ];
    let mut records: Vec<(Value, Value)> = [("exp_x", "exp_root"), ("exp_y", "exp_x"), ("exp_z", "exp_x"), ("exp_w", "exp_z")]
        .iter()
        .enumerate()
        .map(|(index, (id, parent))| submitted(json!({ "$schema": schema_uri("experiment-proposal"), "experimentId": id, "parentExperimentId": parent, "autoresearchId": "ar_3", "round": index + 1, "hypothesis": format!("Hypothesis {id}") })))
        .collect();
    for (id, decision, score) in [
        ("exp_x", "promoted", 0.0097),
        ("exp_y", "rejected", 0.0115),
        ("exp_z", "promoted", 0.006),
        ("exp_w", "rejected", 0.016),
    ] {
        records.push(submitted(json!({ "$schema": schema_uri("experiment-result"), "experimentId": id, "promotionDecision": decision, "comparison": { "candidateScore": score, "improvement": 0 }, "guardrails": [] })));
    }
    let map = build_research_map(&ResearchMapInput {
        missions: &[],
        directions: &[],
        autoresearch: &autoresearch,
        submitted: &records,
        events: &[],
        agents: &[],
        jobs: &[],
    });
    let run = &map["roots"][0];
    assert_eq!(
        run["detail"],
        "round 4/30 · 4/30 experiments · best 0.006 · baseline 0.0187"
    );
    assert_eq!(run["updatedAt"], "2026-10-18T02:13:29.000Z");
    fn shape(node: &Value) -> Value {
        json!({ node["id"].as_str().unwrap(): node["children"].as_array().unwrap().iter().map(shape).collect::<Vec<_>>() })
    }
    let shapes: Vec<Value> = run["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(shape)
        .collect();
    assert_eq!(
        Value::Array(shapes),
        json!([{ "exp_x": [{ "exp_y": [] }, { "exp_z": [{ "exp_w": [] }] }] }])
    );
}

#[test]
fn nests_mission_spawned_directions_and_autoresearch_under_their_spawning_nodes_each_exactly_once()
{
    let origin = |scope_type: &str, scope_id: &str, node_id: &str| json!({ "scopeType": scope_type, "scopeId": scope_id, "nodeId": node_id, "attempt": 1 });
    let job = json!({ "jobId": "job_d", "command": ["node", "evaluate.mjs"], "state": "running", "runner": "native", "missionId": "mis_2", "directionId": "dir_a", "autoresearchId": "ar_d" });
    let missions = [
        json!({ "entityId": "mis_2", "version": 7, "state": "running", "value": { "title": "Early exit", "updatedAt": "2026-10-05T00:00:00.000Z", "nodes": [
        node("n_spawn1", "working", json!({ "type": "research_direction", "lease": { "ownerId": "dir_a" } })),
        node("n_spawn2", "accepted", json!({ "type": "research_direction" })),
        node("n_ar", "working", json!({ "type": "autoresearch", "lease": { "ownerId": "ar_m" } })),
        node("n_spawn3", "working", json!({ "type": "research_direction", "lease": { "ownerId": "dir_lease" } })),
    ] } }),
    ];
    let directions = [
        json!({ "entityId": "dir_a", "version": 2, "state": "awaiting_autoresearch", "value": { "question": "Does early exit help?", "missionId": "mis_2", "origin": origin("mission", "mis_2", "n_spawn1"), "nodes": [node("h1", "working", json!({ "type": "hypothesis" })), node("h2", "ready", json!({ "type": "hypothesis" }))] } }),
        json!({ "entityId": "dir_b", "version": 1, "state": "planning", "value": { "question": "Is the threshold stable?", "missionId": "mis_2", "origin": origin("mission", "mis_2", "n_spawn2"), "nodes": [] } }),
        json!({ "entityId": "dir_c", "version": 1, "state": "planning", "value": { "question": "User-created under the Mission", "missionId": "mis_2", "nodes": [] } }),
        json!({ "entityId": "dir_lease", "version": 1, "state": "planning", "value": { "question": "Spawned before origin existed", "missionId": "mis_2", "nodes": [] } }),
        json!({ "entityId": "dir_user", "version": 1, "state": "planning", "value": { "question": "Unrelated question", "nodes": [], "updatedAt": "2026-10-04T00:00:00.000Z" } }),
    ];
    let autoresearch = [
        json!({ "entityId": "ar_d", "version": 1, "state": "running", "value": { "directionId": "dir_a", "missionId": "mis_2", "origin": origin("direction", "dir_a", "h1"), "decisionQuestion": "Tune exit threshold" } }),
        json!({ "entityId": "ar_m", "version": 1, "state": "running", "value": { "directionId": null, "missionId": "mis_2", "origin": origin("mission", "mis_2", "n_ar"), "decisionQuestion": "Mission-level sweep" } }),
    ];
    let records = [submitted(
        json!({ "$schema": schema_uri("experiment-proposal"), "experimentId": "exp_d", "autoresearchId": "ar_d", "round": 1, "hypothesis": "Raise threshold" }),
    )];
    let map = build_research_map(&ResearchMapInput {
        missions: &missions,
        directions: &directions,
        autoresearch: &autoresearch,
        submitted: &records,
        events: &[],
        agents: &[],
        jobs: &[job],
    });
    let all: Vec<String> = map["roots"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|root| paths(root, ""))
        .collect();
    assert_eq!(ids(&map["roots"]), ["mis_2", "dir_user"]);
    for expected in [
        "mis_2/n_spawn1/dir_a/h1/ar_d/exp_d",
        "mis_2/n_spawn1/dir_a/h1/ar_d/job_d",
        "mis_2/n_spawn2/dir_b",
        "mis_2/n_ar/ar_m",
        "mis_2/n_spawn3/dir_lease",
        "mis_2/dir_c",
    ] {
        assert!(
            all.iter().any(|path| path == expected),
            "{expected} missing from {all:?}"
        );
    }
    // Every Direction and Autoresearch appears exactly once in the whole map.
    for id in [
        "dir_a",
        "dir_b",
        "dir_c",
        "dir_lease",
        "dir_user",
        "ar_d",
        "ar_m",
    ] {
        assert_eq!(
            all.iter()
                .filter(|path| path.rsplit('/').next() == Some(id))
                .count(),
            1,
            "{id}"
        );
    }
    let spawned = map["roots"][0]["children"][0]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["id"] == "dir_a")
        .unwrap();
    assert_match(
        spawned,
        &json!({ "kind": "direction", "title": "Does early exit help?", "detail": "v2", "version": 2, "progress": { "done": 0, "total": 2 } }),
    );
    assert_eq!(ids(&spawned["children"]), ["h1", "h2"]);
}
