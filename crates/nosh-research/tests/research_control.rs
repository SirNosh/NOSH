//! The daemon's research control against the TypeScript one: one scenario of Missions,
//! Directions, Autoresearch, graph proposals, domain records and handoffs, user Evidence and
//! Claims, Reviews, the paper, and completion and budget queries runs through both
//! (`scripts/research-control-ts.mjs`, needs a built TypeScript tree). Every result, the events
//! each op published, and the final events, projections, and operation intents must agree, with
//! random IDs, timestamps, hashes, and paths masked.

use nosh_research::research_control::*;
use nosh_store::EventStore;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

const PROJECT: &str = "prj_55555555555555555555555555555555";
const AGENT_A: &str = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AGENT_B: &str = "agt_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REVIEWER: &str = "agt_cccccccccccccccccccccccccccccccc";
const DIRECTOR: &str = "agt_dddddddddddddddddddddddddddddddd";
const TASK: &str = "tsk_11111111111111111111111111111111";
const HANDOFF: &str = "hnd_11111111111111111111111111111111";
const BLOCKER: &str = "blk_11111111111111111111111111111111";
const FUTURE: &str = "2099-01-01T00:00:00.000Z";

struct TestHost {
    store: EventStore,
    project: Value,
    published: RefCell<Vec<String>>,
}

impl Host for TestHost {
    fn store(&self, _project_id: &str) -> nosh_research::Result<&EventStore> {
        Ok(&self.store)
    }
    fn project(&self, _project_id: &str) -> nosh_research::Result<Value> {
        Ok(self.project.clone())
    }
    fn publish(&self, event: &Value) {
        self.published
            .borrow_mut()
            .push(event["type"].as_str().unwrap_or_default().to_owned());
    }
}

/// A Project repository with one commit at a fixed date, so both copies share its hash.
fn repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("README.md"), "seed\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "README.md"],
        &["commit", "-q", "-m", "seed"],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .status()
            .unwrap();
        assert!(status.success());
    }
}

fn project(root: &Path, database: &Path) -> Value {
    json!({ "projectId": PROJECT, "repositoryRoot": root, "databasePath": database, "registeredAt": "2026-10-17T00:00:00.000Z" })
}

// ---- Running ops ----

/// Replaces each `$ref` with the earlier result it names; naming a failed op fails this one, with
/// the TypeScript harness's message.
fn resolve(value: &Value, results: &[Result<Value, String>]) -> Result<Value, String> {
    Ok(match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve(item, results))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => match map.get("$ref").and_then(Value::as_array) {
            Some(reference) => {
                let index = reference[0].as_i64().unwrap();
                let index = if index < 0 {
                    results.len() as i64 + index
                } else {
                    index
                } as usize;
                let Ok(mut value) = results[index].clone() else {
                    return Err(format!(
                        "$ref {} names a failed op",
                        serde_json::to_string(reference).unwrap()
                    ));
                };
                for key in &reference[1..] {
                    value = match key {
                        Value::String(key) => value[key.as_str()].clone(),
                        index => value[index.as_u64().unwrap() as usize].clone(),
                    };
                }
                value
            }
            None => Value::Object(
                map.iter()
                    .map(|(key, item)| Ok((key.clone(), resolve(item, results)?)))
                    .collect::<Result<_, String>>()?,
            ),
        },
        other => other.clone(),
    })
}

fn to<T: serde::Serialize>(value: T) -> nosh_research::Result<Value> {
    let mut value = serde_json::to_value(value)?;
    nosh_wire::normalize_numbers(&mut value);
    Ok(value)
}

fn dispatch(
    research: &ResearchControl<TestHost>,
    op: &Value,
    args: &[Value],
) -> nosh_research::Result<Value> {
    let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Null);
    let text = |index: usize| arg(index).as_str().map(str::to_owned).unwrap_or_default();
    let optional = |index: usize| arg(index).as_str().map(str::to_owned);
    let int = |index: usize| arg(index).as_f64().unwrap_or(f64::NAN) as i64;
    let float = |index: usize| arg(index).as_f64().unwrap_or(f64::NAN);
    let strings =
        |index: usize| -> Vec<String> { serde_json::from_value(arg(index)).unwrap_or_default() };
    let p = text(0);
    if let Some(method) = op.get("store").and_then(Value::as_str) {
        let store = &research.host().store;
        return match method {
            "appendIdempotent" => {
                let appended = store.append_idempotent(&text(0), &arg(1))?;
                Ok(json!({ "receipt": appended.receipt, "replayed": appended.replayed }))
            }
            other => panic!("no store op {other}"),
        };
    }
    if let Some(name) = op.get("fn").and_then(Value::as_str) {
        return match name {
            "librarianNetworkAllowed" => Ok(json!(librarian_network_allowed(&arg(0)))),
            "declaredTaskCommands" => Ok(json!(declared_task_commands(&arg(0)))),
            "taskCommandInstructions" => {
                Ok(json!(task_command_instructions(&serde_json::from_value::<
                    Vec<Value>,
                >(arg(
                    0
                ))?)))
            }
            "daemonTaskFacts" => Ok(json!(daemon_task_facts(research, &p, &text(1))?)),
            "missionOutcomes" => mission_outcomes(
                research,
                &p,
                &text(1),
                arg(2).as_u64().map_or(14_000, |n| n as usize),
            ),
            "taskCommandRuns" => Ok(json!(task_command_runs(research, &p)?)),
            "runningSeconds" => to(running_seconds(
                &serde_json::from_value::<Vec<Value>>(arg(0))?,
                &text(1),
                &serde_json::from_value::<Vec<(f64, f64)>>(arg(2))?,
                float(3),
            )),
            "downtimeIntervals" => to(downtime_intervals(&serde_json::from_value::<Vec<Value>>(
                arg(0),
            )?)),
            other => panic!("no function {other}"),
        };
    }
    let session = || serde_json::from_value::<DomainSession>(arg(1));
    let lease = || Some(arg(6)).filter(|lease| !lease.is_null());
    let method = op["method"].as_str().unwrap();
    match method {
        "mission" => research.mission(&p, &text(1)),
        "direction" => research.direction(&p, &text(1)),
        "autoresearchExecution" => research.autoresearch_execution(&p, &text(1)),
        "graphProposals" => to(research.graph_proposals(&p)?),
        "createMission" => research.create_mission(&p, &serde_json::from_value(arg(1))?),
        "assertInspectedMissionState" => {
            research.assert_inspected_mission_state(&p, &text(1), int(2))
        }
        "start" => research.start(&p, &text(1), &text(2), int(3), &text(4)),
        "transitionMission" => {
            research.transition_mission(&p, &text(1), int(2), &text(3), &text(4))
        }
        "mutateMissionGraph" => research.mutate_mission_graph(
            &p,
            &text(1),
            int(2),
            int(3),
            &arg(4),
            &text(5),
            &strings(6),
            &text(7),
            arg(8) == true,
        ),
        "transitionMissionNode" => research.transition_mission_node(
            &p,
            &text(1),
            int(2),
            &text(3),
            &text(4),
            &text(5),
            lease().as_ref(),
            arg(7) == true,
        ),
        "createDirection" => research.create_direction(
            &p,
            &serde_json::from_value(arg(1))?,
            serde_json::from_value::<Option<SpawnedDirection>>(arg(2))?.as_ref(),
        ),
        "transitionDirection" => {
            research.transition_direction(&p, &text(1), int(2), &text(3), &text(4))
        }
        "transitionDirectionNode" => research.transition_direction_node(
            &p,
            &text(1),
            int(2),
            &text(3),
            &text(4),
            &text(5),
            lease().as_ref(),
            arg(7) == true,
        ),
        "mutateDirectionGraph" => research.mutate_direction_graph(
            &p,
            &text(1),
            int(2),
            int(3),
            &arg(4),
            &text(5),
            &strings(6),
            &text(7),
        ),
        "approveGraphProposal" => research.approve_graph_proposal(&p, &text(1), int(2), &text(3)),
        "acceptDirectionBaseline" => research.accept_direction_baseline(
            &p,
            &text(1),
            int(2),
            &serde_json::from_value(arg(3))?,
        ),
        "createAutoresearch" => research.create_autoresearch(
            &p,
            &serde_json::from_value(arg(1))?,
            serde_json::from_value::<Option<SpawnedAutoresearch>>(arg(2))?.as_ref(),
        ),
        "transitionAutoresearch" => {
            research.transition_autoresearch(&p, &text(1), int(2), &text(3), &text(4))
        }
        "advanceAutoresearch" => {
            research.advance_autoresearch(&p, &text(1), int(2), float(3), &strings(4), &text(5))
        }
        "validateDomainRecord" => research
            .validate_domain_record(&p, &session()?, &arg(2))
            .map(|()| Value::Null),
        "applyDomainEffect" => research
            .apply_domain_effect(&p, &session()?, &arg(2))
            .map(|()| Value::Null),
        "canonicalDomainRecord" => Ok(research
            .canonical_domain_record(&p, &arg(1))?
            .map_or(Value::Null, |event| json!({ "event": event }))),
        "reconcileAcceptedDomainEffects" => research.reconcile_accepted_domain_effects(&p),
        "submitted" => Ok(Value::Array(
            research
                .submitted(&p, optional(1).as_deref())?
                .into_iter()
                .map(|event| json!({ "event": event, "record": event["payload"] }))
                .collect(),
        )),
        "terminalRecord" => Ok(research
            .terminal_record(&p, &text(1), optional(2).as_deref())?
            .unwrap_or(Value::Null)),
        "eventsAfter" => Ok(json!(research.events_after(&p, int(1))?)),
        "offerCandidateEvidence" => {
            research.offer_candidate_evidence(&p, arg(1));
            Ok(Value::Null)
        }
        "withdrawCandidateEvidence" => {
            research.withdraw_candidate_evidence(&p, &text(1));
            Ok(Value::Null)
        }
        "candidateEvidence" => Ok(research
            .candidate_evidence(&p, &text(1))
            .unwrap_or(Value::Null)),
        "records" => Ok(json!(research.records(&p, optional(1).as_deref())?)),
        "reviews" => to(research.reviews(&p)?),
        "isDomainEffectRecord" => Ok(json!(research.is_domain_effect_record(&arg(0)))),
        "submitDaemonRecord" => {
            research.submit_daemon_record(&p, &arg(1), &text(2), &arg(3), &text(4))
        }
        "submitUserEvidence" => research.submit_user_evidence(&p, &arg(1), &text(2)),
        "latestClaims" => Ok(json!(research.latest_claims(&p)?)),
        "submitUserClaim" => research.submit_user_claim(&p, &arg(1), &text(2)),
        "editUserClaim" => research.edit_user_claim(&p, &text(1), float(2), &arg(3), &text(4)),
        "beginExternalOperation" => {
            research.begin_external_operation(&p, &text(1), &text(2), &arg(3))
        }
        "completeExternalOperation" => research.complete_external_operation(&p, &text(1), &arg(2)),
        "externalOperation" => Ok(research
            .external_operation(&p, &text(1))?
            .unwrap_or(Value::Null)),
        "pendingExternalOperations" => Ok(json!(
            research.pending_external_operations(&p, optional(1).as_deref())?
        )),
        "scopedEvents" => Ok(json!(research.scoped_events(
            &p,
            &text(1),
            optional(2).as_deref()
        )?)),
        "missionBudgetUse" => research.mission_budget_use(&p, &text(1)),
        "missionCompletionBasis" => to(research.mission_completion_basis(&p, &text(1))?),
        "spawnedChild" => to(research.spawned_child(&p, &text(1), &text(2))?),
        "researchInputs" => research.research_inputs(&p, &text(1), &strings(2)),
        "childEvidenceIds" => Ok(json!(research.child_evidence_ids(&p, &text(1))?)),
        "directionBudgetUse" => research.direction_budget_use(&p, &text(1)),
        "autoresearchBudgetUse" => research.autoresearch_budget_use(&p, &text(1)),
        "missionCompletionIssues" => Ok(json!(research.mission_completion_issues(&p, &text(1))?)),
        "readPaper" => research.read_paper(&p),
        "savePaper" => research.save_paper(&p, &text(1), &text(2), &arg(3), &text(4)),
        "reconcilePendingOperations" => Ok(json!(research.reconcile_pending_operations(&p)?)),
        other => panic!("no method {other}"),
    }
}

/// `{ results: [{ ok, published } | { error }], state }`, as the TypeScript harness prints it.
fn run_rust(root: &Path, database: &Path, ops: &[Value]) -> Value {
    let host = TestHost {
        store: EventStore::open(database).unwrap(),
        project: project(root, database),
        published: RefCell::new(Vec::new()),
    };
    let research = ResearchControl::new(host);
    let mut results: Vec<Result<Value, String>> = Vec::new();
    let mut printed = Vec::new();
    for op in ops {
        if let Some(result) = op.get("result") {
            printed.push(json!({ "ok": result, "published": [] }));
            results.push(Ok(result.clone()));
            continue;
        }
        if let Some(failed) = op.get("failed") {
            printed.push(json!({ "error": failed }));
            results.push(Err(failed.as_str().unwrap_or_default().to_owned()));
            continue;
        }
        let before = research.host().published.borrow().len();
        let outcome = resolve(op.get("args").unwrap_or(&json!([])), &results).and_then(|args| {
            dispatch(&research, op, args.as_array().unwrap()).map_err(|error| error.to_string())
        });
        printed.push(match &outcome {
            Ok(value) => {
                json!({ "ok": value, "published": research.host().published.borrow()[before..] })
            }
            Err(error) => json!({ "error": error }),
        });
        results.push(outcome);
    }
    let store = &research.host().store;
    let projections: serde_json::Map<String, Value> = [
        "mission",
        "direction",
        "autoresearch",
        "graph_proposal",
        "handoff",
        "handoff_decision",
        "logical_ownership",
    ]
    .iter()
    .map(|kind| {
        (
            kind.to_string(),
            json!(store.projections(PROJECT, kind).unwrap()),
        )
    })
    .collect();
    json!({ "results": printed, "state": { "events": store.replay(PROJECT, 0).unwrap(), "projections": projections, "intents": store.operation_intents(PROJECT, None).unwrap() } })
}

fn run_typescript(root: &Path, database: &Path, ops: &[Value]) -> Value {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/research-control-ts.mjs");
    let request = json!({ "database": database, "project": project(root, database), "ops": ops });
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node is on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "research-control-ts.mjs failed (is the TypeScript tree built?):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// ---- Masking ----

/// Numbers random IDs and hashes by first appearance, and blanks timestamps and the run's paths.
/// IDs derived from fixed input (Evidence and Claim IDs) stay as they are, so they compare.
struct Mask {
    root: String,
    database: String,
    seen: HashMap<String, usize>,
}

impl Mask {
    fn text(&mut self, text: &str) -> String {
        let text = text
            .replace(&self.root, "<root>")
            .replace(&self.database, "<database>")
            .replace(&self.root.replace('\\', "/"), "<root>");
        static TIME: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z").unwrap()
        });
        static TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"\b(?:mis|dir|ar|exp|evt|cmd|question|evaluation|stop|rubric|mnode|dnode)_[0-9a-f]{32}\b|sha256:[0-9a-f]{64}").unwrap()
        });
        let text = TIME.replace_all(&text, "<t>").into_owned();
        let mut out = String::new();
        let mut last = 0;
        for found in TOKEN.find_iter(&text) {
            out.push_str(&text[last..found.start()]);
            let next = self.seen.len();
            let number = *self.seen.entry(found.as_str().to_owned()).or_insert(next);
            out.push_str(&format!(
                "<{}:{number}>",
                found.as_str().split(['_', ':']).next().unwrap()
            ));
            last = found.end();
        }
        out.push_str(&text[last..]);
        out
    }

    fn value(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (self.text(key), self.value(item)))
                    .collect(),
            ),
            other => {
                let mut other = other.clone();
                nosh_wire::normalize_numbers(&mut other);
                other
            }
        }
    }

    /// Results and events first, so every entity is numbered in a deterministic order; lists the
    /// store returns in `updated_at` order (ties are timing-dependent) then compare as sets.
    fn output(&mut self, output: &Value) -> (Vec<Value>, Value) {
        let results: Vec<Value> = output["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| self.value(result))
            .collect();
        let events = self.value(&output["state"]["events"]);
        let receipts = self.value(&output["state"]["receipts"]);
        let sorted = |mask: &mut Mask, list: &Value| {
            let mut items: Vec<Value> = list
                .as_array()
                .unwrap()
                .iter()
                .map(|item| mask.value(item))
                .collect();
            items.sort_by_key(nosh_wire::canonical_json);
            Value::Array(items)
        };
        let intents = sorted(self, &output["state"]["intents"]);
        let projections: serde_json::Map<String, Value> = output["state"]["projections"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(kind, list)| (kind.clone(), sorted(self, list)))
            .collect();
        (
            results,
            json!({ "events": events, "receipts": receipts, "intents": intents, "projections": projections }),
        )
    }
}

// ---- The scenario ----

#[derive(Default)]
struct Ops(Vec<Value>);

impl Ops {
    fn call(&mut self, method: &str, args: Value) -> usize {
        self.0.push(json!({ "method": method, "args": args }));
        self.0.len() - 1
    }
    fn function(&mut self, name: &str, args: Value) -> usize {
        self.0.push(json!({ "fn": name, "args": args }));
        self.0.len() - 1
    }
    fn store(&mut self, name: &str, args: Value) -> usize {
        self.0.push(json!({ "store": name, "args": args }));
        self.0.len() - 1
    }
}

/// `{ "$ref": [index, ...path] }`.
fn r(index: usize, path: Value) -> Value {
    let mut reference = vec![json!(index)];
    reference.extend(path.as_array().unwrap().iter().cloned());
    json!({ "$ref": reference })
}

fn fixture(name: &str, changes: Value) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance/schemas/fixtures")
        .join(name)
        .join("minimal.valid.json");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for (key, value) in changes.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

fn scope(mission: Value, direction: Value, agent: Value) -> Value {
    json!({ "projectId": PROJECT, "missionId": mission, "directionId": direction, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": agent })
}

fn event(kind: &str, scope: Value, payload: Value) -> Value {
    json!({ "$schema": "https://nosh.dev/schemas/event/v1", "schemaVersion": 1, "retention": "persistent", "type": kind, "source": "noshd", "scope": scope, "correlationId": null, "causationId": null, "payload": payload })
}

fn session(agent: &str, role: &str, task: Value, mission: Value) -> Value {
    json!({ "agentId": agent, "role": role, "taskId": task, "missionId": mission, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null })
}

fn review(id: &str, review_type: &str, target: Value, evidence: Value) -> (Value, Value) {
    let request = fixture(
        "review-request",
        json!({ "reviewId": id, "reviewRequestId": id, "reviewType": review_type, "target": target, "producerAgentIds": [AGENT_A], "reviewerAgentId": REVIEWER, "requiredEvidenceIds": evidence, "scope": { "projectId": PROJECT, "missionId": null, "directionId": null, "autoresearchId": null } }),
    );
    let verdict = fixture(
        "review-verdict",
        json!({ "reviewId": id, "reviewRequestId": id, "reviewType": review_type, "target": target, "reviewerAgentId": REVIEWER, "reviewedEvidenceIds": evidence, "criteria": [{ "confidence": "low", "criterionId": "ref_1", "defectIds": [], "evidenceRefs": [], "finding": "holds", "status": "PASS" }] }),
    );
    (request, verdict)
}

/// Submits a Review: its request, then its verdict, a terminal record of the reviewer's own task
/// (`tsk_` and `digit` repeated).
fn submit_review(s: &mut Ops, (request, verdict): (Value, Value), digit: char) {
    let project_scope = scope(Value::Null, Value::Null, Value::Null);
    let task: String = std::iter::repeat_n(digit, 32).collect();
    s.call(
        "submitDaemonRecord",
        json!([
            PROJECT,
            project_scope,
            "review",
            request,
            format!("review-request-{digit}")
        ]),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            PROJECT,
            project_scope,
            format!("task:tsk_{task}"),
            verdict,
            format!("review-verdict-{digit}")
        ]),
    );
}

fn scenario() -> Vec<Value> {
    let p = PROJECT;
    let mut s = Ops::default();
    let project_scope = scope(Value::Null, Value::Null, Value::Null);

    // Missions: creation, replay, and refusals.
    let mission_input = json!({ "title": " Calibrate ", "objective": "Compare calibrators", "deliverables": ["Report", " "], "successCriteria": ["ECE drops", "F1 holds"], "researchPlan": { "directions": 1, "missionAutoresearch": true }, "idempotencyKey": "m1" });
    let m1 = s.call("createMission", json!([p, mission_input]));
    s.call("createMission", json!([p, mission_input]));
    let mut changed = mission_input.clone();
    changed["title"] = json!("Other");
    s.call("createMission", json!([p, changed]));
    s.call("createMission", json!([p, { "title": "B", "objective": "O", "deliverables": ["D"], "successCriteria": ["C"], "budgets": { "maximumConcurrentAgents": 9 }, "idempotencyKey": "m-bad" }]));
    s.call("createMission", json!([p, { "title": "  ", "objective": "O", "deliverables": ["D"], "successCriteria": ["C"], "idempotencyKey": "m-empty" }]));
    s.call("createMission", json!([p, { "title": "P", "objective": "O", "deliverables": ["D"], "successCriteria": ["C"], "researchPlan": { "directions": 5, "missionAutoresearch": false }, "idempotencyKey": "m-plan" }]));
    s.call("createMission", json!([p, { "title": "E", "objective": "O", "deliverables": ["D"], "successCriteria": ["C"], "startingEvidence": ["evd_missing"], "idempotencyKey": "m-evd" }]));
    let m2 = s.call("createMission", json!([p, { "title": "Second", "objective": "O", "deliverables": ["D"], "successCriteria": ["C"], "budgets": { "maximumModelTokens": 2000000 }, "idempotencyKey": "m2" }]));
    let (mid1, mid2) = (r(m1, json!(["entityId"])), r(m2, json!(["entityId"])));
    let node = |mission: usize, index: usize| r(mission, json!(["value", "nodes", index, "id"]));

    // Graph mutation before approval.
    let extra = json!({ "id": "mnode_extra", "type": "analysis", "title": "Analysis", "required": false, "criterionIds": [], "hardDependencies": [node(m2, 0)], "softDependencies": [], "state": "pending", "attempt": 0, "maximumAttempts": 2, "priority": 3, "criticalWeight": 1, "createdAt": "2026-10-17T00:00:00.000Z", "lease": null });
    let add = json!([{ "type": "add_node", "node": extra }]);
    let g2 = s.call(
        "mutateMissionGraph",
        json!([
            p,
            mid2,
            1,
            1,
            add,
            "add analysis",
            ["evd_2", "evd_1"],
            "m2-graph"
        ]),
    );
    s.call(
        "mutateMissionGraph",
        json!([
            p,
            mid2,
            1,
            1,
            add,
            "add analysis",
            ["evd_2", "evd_1"],
            "m2-graph"
        ]),
    );
    let v2 = r(g2, json!(["version"]));
    s.call(
        "mutateMissionGraph",
        json!([p, mid2, v2, 1, add, "again", [], "m2-stale"]),
    );
    s.call(
        "mutateMissionGraph",
        json!([p, mid2, v2, 2, [{ "type": "bogus" }], "bad", [], "m2-bad"]),
    );
    let mut owner = extra.clone();
    owner["id"] = json!("mnode_owner");
    owner["criterionIds"] = json!(["criterion_1"]);
    owner["hardDependencies"] = json!([]);
    s.call(
        "mutateMissionGraph",
        json!([p, mid2, v2, 2, [{ "type": "add_node", "node": owner }], "owner", [], "m2-owner"]),
    );
    let g2b = s.call("mutateMissionGraph", json!([p, mid2, v2, 2, [{ "type": "supersede_node", "nodeId": "mnode_extra", "replacementId": null }], "drop analysis", [], "m2-supersede"]));

    // Approval walks the first Mission to running; one active Mission per Project.
    let started = s.call("start", json!([p, "missions", mid1, 1, "start-m1"]));
    s.call(
        "start",
        json!([p, "missions", mid2, r(g2b, json!(["version"])), "start-m2"]),
    );
    s.call("start", json!([p, "missions", mid1, 1, "start-m1-stale"]));
    let running = r(started, json!(["version"]));
    s.call("assertInspectedMissionState", json!([p, mid1, running]));
    s.call("assertInspectedMissionState", json!([p, mid1, 2]));
    s.call(
        "transitionMission",
        json!([p, mid1, running, "draft", "m1-illegal"]),
    );

    // Leasing the literature node, and a task working it.
    let literature = node(m1, 3);
    let lease = |version: Value| json!({ "leaseId": "lease_1", "ownerId": AGENT_A, "version": version, "expiresAt": FUTURE });
    s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            running,
            literature,
            "leased",
            "lease-stale",
            lease(json!(1))
        ]),
    );
    s.call(
        "transitionMissionNode",
        json!([p, mid1, running, literature, "leased", "lease-none"]),
    );
    let leased = s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            running,
            literature,
            "leased",
            "lease-1",
            lease(running.clone())
        ]),
    );
    s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            running,
            literature,
            "leased",
            "lease-1",
            lease(running.clone())
        ]),
    );
    let working = s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            r(leased, json!(["version"])),
            literature,
            "working",
            "work-1"
        ]),
    );
    let packet = fixture(
        "task-packet",
        json!({ "taskId": TASK, "assignedAgentId": AGENT_A, "assignedRole": "librarian_researcher", "scope": { "projectId": p, "missionId": mid1, "directionId": null, "autoresearchId": null, "experimentId": null, "graphNodeId": literature }, "lease": { "leaseId": "lease_1", "expiresAt": FUTURE, "heartbeatSeconds": 60 }, "observedVersions": { "leaseVersion": r(working, json!(["version"])), "missionGraphVersion": 1 }, "budget": { "deadline": FUTURE, "maximumModelTokens": 1000, "maximumRepairAttempts": 1, "maximumToolCalls": 10, "maximumWallClockSeconds": 600 } }),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            scope(mid1.clone(), Value::Null, Value::Null),
            format!("task:{TASK}"),
            packet,
            "packet-1"
        ]),
    );
    let worker = session(AGENT_A, "librarian_researcher", json!(TASK), mid1.clone());
    let progress = fixture(
        "progress-update",
        json!({ "taskId": TASK, "agentId": AGENT_A, "progressId": "prog_1" }),
    );
    s.call("validateDomainRecord", json!([p, worker, progress]));
    s.call("applyDomainEffect", json!([p, worker, progress]));
    s.call("applyDomainEffect", json!([p, worker, progress]));
    s.call(
        "applyDomainEffect",
        json!([
            p,
            worker,
            fixture(
                "progress-update",
                json!({ "taskId": TASK, "agentId": AGENT_B, "progressId": "prog_2" })
            )
        ]),
    );
    s.call("applyDomainEffect", json!([p, worker, fixture("delegation-request", json!({ "requestId": "req_1", "requestingAgentId": AGENT_A, "requestingTaskId": TASK, "budgetEstimate": { "modelTokens": 100, "wallClockSeconds": 60 } }))]));
    s.call("applyDomainEffect", json!([p, worker, fixture("delegation-request", json!({ "requestId": "req_2", "requestingAgentId": AGENT_A, "requestingTaskId": TASK, "budgetEstimate": { "modelTokens": 5000, "wallClockSeconds": 60 } }))]));
    s.call("isDomainEffectRecord", json!([progress]));

    // A handoff to a second agent, its teach-back, and the released lease.
    let goal = json!({ "currentGraphNodeId": literature, "directionQuestionId": null, "missionCriterionIds": [], "projectGoalId": p });
    let head = "0123456789abcdef0123456789abcdef01234567";
    let handoff = fixture(
        "handoff",
        json!({ "handoffId": HANDOFF, "fromAgentId": AGENT_A, "toAgentId": AGENT_B, "logicalOwnerId": "owner_literature", "oldLeaseReleaseId": "lease_1", "goalStack": goal, "scope": scope(mid1.clone(), Value::Null, json!(AGENT_A)), "observedVersions": { "mission": 5 }, "branch": { "head": head, "name": "work", "uncommittedChanges": false } }),
    );
    let teachback = fixture(
        "handoff-teachback",
        json!({ "handoffId": HANDOFF, "toAgentId": AGENT_B, "logicalOwnerId": "owner_literature", "decision": "accepted", "understoodGoalStack": goal, "observedVersions": { "mission": 5 }, "observedBranchHead": head }),
    );
    let recipient = session(AGENT_B, "librarian_researcher", Value::Null, mid1.clone());
    s.call("validateDomainRecord", json!([p, recipient, teachback]));
    s.call("applyDomainEffect", json!([p, worker, handoff]));
    s.call("applyDomainEffect", json!([p, worker, handoff]));
    let mut wrong = teachback.clone();
    wrong["observedBranchHead"] = json!("ffffffffffffffffffffffffffffffffffffffff");
    s.call("validateDomainRecord", json!([p, recipient, wrong]));
    s.call("applyDomainEffect", json!([p, recipient, teachback]));
    s.call("applyDomainEffect", json!([p, recipient, teachback]));
    let blocker = fixture(
        "blocker",
        json!({ "blockerId": BLOCKER, "taskId": TASK, "scopeType": "graph_node", "scopeId": literature }),
    );
    s.call("applyDomainEffect", json!([p, worker, blocker]));
    let current = s.call("mission", json!([p, mid1]));
    let version = r(current, json!(["version"]));
    s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            version,
            literature,
            "leased",
            "lease-2",
            lease(version.clone())
        ]),
    );
    s.call("applyDomainEffect", json!([p, worker, blocker]));
    let current = s.call("mission", json!([p, mid1]));
    s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            r(current, json!(["version"])),
            literature,
            "ready",
            "retry-1",
            null,
            true
        ]),
    );
    // Two more working attempts exhaust the node (three): only a user retry grants one more.
    for attempt in 2..=3 {
        let current = s.call("mission", json!([p, mid1]));
        let version = r(current, json!(["version"]));
        let leased = s.call(
            "transitionMissionNode",
            json!([
                p,
                mid1,
                version,
                literature,
                "leased",
                format!("lease-{attempt}b"),
                lease(version.clone())
            ]),
        );
        let working = s.call(
            "transitionMissionNode",
            json!([
                p,
                mid1,
                r(leased, json!(["version"])),
                literature,
                "working",
                format!("work-{attempt}")
            ]),
        );
        let blocked = s.call(
            "transitionMissionNode",
            json!([
                p,
                mid1,
                r(working, json!(["version"])),
                literature,
                "blocked",
                format!("block-{attempt}")
            ]),
        );
        if attempt == 2 {
            s.call(
                "transitionMissionNode",
                json!([
                    p,
                    mid1,
                    r(blocked, json!(["version"])),
                    literature,
                    "ready",
                    "retry-2"
                ]),
            );
        }
    }
    let current = s.call("mission", json!([p, mid1]));
    s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            r(current, json!(["version"])),
            literature,
            "ready",
            "retry-auto"
        ]),
    );
    s.call(
        "transitionMissionNode",
        json!([
            p,
            mid1,
            r(current, json!(["version"])),
            literature,
            "ready",
            "retry-user",
            null,
            true
        ]),
    );

    // A director's graph proposal needs approval once the Mission is approved.
    let director = session(DIRECTOR, "mission_director", Value::Null, mid1.clone());
    let proposal = fixture(
        "graph-change-proposal",
        json!({ "proposalId": "prop_1", "proposerRole": "mission_director", "scopeType": "mission", "scopeId": mid1, "baseGraphVersion": 1, "approvalRequired": true, "contractImpact": "none", "operations": [{ "type": "add_hard_edge", "from": literature, "to": node(m1, 2) }] }),
    );
    let mut unapproved = proposal.clone();
    unapproved["approvalRequired"] = json!(false);
    s.call("validateDomainRecord", json!([p, director, unapproved]));
    s.call("validateDomainRecord", json!([p, worker, proposal]));
    s.call("applyDomainEffect", json!([p, director, proposal]));
    let before_approval = s.call("mission", json!([p, mid1]));
    s.call("approveGraphProposal", json!([p, "prop_1", 1, "approve-1"]));
    s.call("approveGraphProposal", json!([p, "prop_1", 1, "approve-1"]));
    // The approval's own graph mutation, as a daemon restarted between its two steps replays it.
    s.call("mutateMissionGraph", json!([p, mid1, r(before_approval, json!(["version"])), 1, [{ "type": "add_hard_edge", "from": literature, "to": node(m1, 2) }], "sample", [], "graph-proposal-apply:prop_1", true]));
    s.call("approveGraphProposal", json!([p, "prop_1", 1, "approve-2"]));
    s.call("graphProposals", json!([p]));

    // Accepted domain records without their effect: one recovers, one cannot be bound.
    let accepted = |record: Value| json!({ "$schema": "https://nosh.dev/schemas/event/v1", "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "noshd", "scope": scope(mid1.clone(), Value::Null, json!(AGENT_A)), "correlationId": format!("task:{TASK}"), "causationId": null, "payload": record });
    s.store(
        "appendIdempotent",
        json!([
            "accepted-progress",
            accepted(fixture(
                "progress-update",
                json!({ "taskId": TASK, "agentId": AGENT_A, "progressId": "prog_recovered" })
            ))
        ]),
    );
    s.store("appendIdempotent", json!(["accepted-orphan", accepted(fixture("blocker", json!({ "blockerId": "blk_22222222222222222222222222222222", "taskId": "tsk_99999999999999999999999999999999", "scopeType": "graph_node", "scopeId": literature })))]));
    s.call("reconcileAcceptedDomainEffects", json!([p]));
    s.call("reconcileAcceptedDomainEffects", json!([p]));
    s.call(
        "canonicalDomainRecord",
        json!([
            p,
            fixture(
                "progress-update",
                json!({ "taskId": TASK, "agentId": AGENT_A, "progressId": "prog_recovered" })
            )
        ]),
    );
    s.call("canonicalDomainRecord", json!([p, fixture("progress-update", json!({ "taskId": TASK, "agentId": AGENT_A, "progressId": "prog_recovered", "summary": "changed" }))]));

    // Directions.
    let direction_input = json!({ "question": " Does binning help? ", "decisionUse": "Pick a calibrator", "idempotencyKey": "d1" });
    let d1 = s.call("createDirection", json!([p, direction_input]));
    s.call("createDirection", json!([p, direction_input]));
    s.call(
        "createDirection",
        json!([p, { "question": "Other", "decisionUse": "Pick", "idempotencyKey": "d1" }]),
    );
    s.call(
        "createDirection",
        json!([p, { "question": " ", "decisionUse": "Pick", "idempotencyKey": "d-empty" }]),
    );
    s.call("createDirection", json!([p, { "question": "Q", "decisionUse": "Pick", "evaluationContract": [1], "idempotencyKey": "d-contract" }]));
    let did1 = r(d1, json!(["entityId"]));
    let active = s.call("start", json!([p, "directions", did1, 1, "start-d1"]));
    let dv = r(active, json!(["version"]));
    s.call(
        "transitionDirection",
        json!([p, did1, dv, "closed", "close-d1"]),
    );
    let contract_hash = r(d1, json!(["value", "evaluationContractHash"]));
    s.call("acceptDirectionBaseline", json!([p, did1, dv, { "commit": "abcdef1", "reviewId": "rev_x", "evaluationContractHash": "sha256:00", "idempotencyKey": "baseline-1" }]));
    s.call("acceptDirectionBaseline", json!([p, did1, dv, { "commit": "not-a-commit", "reviewId": "rev_x", "evaluationContractHash": contract_hash, "idempotencyKey": "baseline-2" }]));
    s.call("acceptDirectionBaseline", json!([p, did1, dv, { "commit": "abcdef1", "reviewId": "rev_x", "evaluationContractHash": contract_hash, "idempotencyKey": "baseline-3" }]));
    s.call(
        "transitionDirection",
        json!([p, did1, dv, "reviewing", "review-d1"]),
    );
    let current = s.call("direction", json!([p, did1]));
    let dv = r(current, json!(["version"]));
    s.call(
        "mutateDirectionGraph",
        json!([p, did1, dv, 1, [], "nothing", [], "d1-empty"]),
    );
    s.call("mutateDirectionGraph", json!([p, did1, dv, 1, [{ "type": "add_hard_edge", "from": r(d1, json!(["value", "nodes", 0, "id"])), "to": r(d1, json!(["value", "nodes", 2, "id"])) }], "wire", ["evd_1"], "d1-graph"]));
    let current = s.call("direction", json!([p, did1]));
    let dv = r(current, json!(["version"]));
    let baseline = r(d1, json!(["value", "nodes", 0, "id"]));
    let dlease =
        json!({ "leaseId": "lease_d", "ownerId": AGENT_A, "version": dv, "expiresAt": FUTURE });
    s.call(
        "transitionDirectionNode",
        json!([p, did1, dv, baseline, "leased", "d-lease", dlease]),
    );
    let spawned_direction = s.call("createDirection", json!([p, { "question": "Spawned question", "decisionUse": "Feed the Mission", "missionId": mid1, "idempotencyKey": "d-spawn" }, { "origin": { "scopeType": "mission", "scopeId": mid1, "nodeId": node(m1, 0), "attempt": 1 }, "baseCommit": head, "budget": { "maximumModelTokens": 1000, "maximumWallClockSeconds": 600 } }]));
    // A retried spawn: the later attempt is the node's child.
    s.call("createDirection", json!([p, { "question": "Spawned question, again", "decisionUse": "Feed the Mission", "missionId": mid1, "idempotencyKey": "d-spawn-2" }, { "origin": { "scopeType": "mission", "scopeId": mid1, "nodeId": node(m1, 0), "attempt": 2 }, "baseCommit": head, "budget": { "maximumModelTokens": 1000, "maximumWallClockSeconds": 600 } }]));
    s.call("createDirection", json!([p, { "question": "Spawned question", "decisionUse": "Feed the Mission", "missionId": mid1, "idempotencyKey": "d-spawn" }, { "origin": { "scopeType": "mission", "scopeId": mid1, "nodeId": node(m1, 0), "attempt": 3 }, "baseCommit": head, "budget": { "maximumModelTokens": 1000, "maximumWallClockSeconds": 600 } }]));
    s.call("createDirection", json!([p, { "question": "Q", "decisionUse": "Pick", "missionId": "mis_00000000000000000000000000000000", "idempotencyKey": "d-unknown" }]));

    // Autoresearch.
    let ar_input = json!({ "decisionQuestion": "Tune the learning rate", "familyTags": ["b", "a", "a"], "scope": ["src"], "idempotencyKey": "ar1" });
    let ar1 = s.call("createAutoresearch", json!([p, ar_input]));
    s.call("createAutoresearch", json!([p, ar_input]));
    s.call("createAutoresearch", json!([p, { "decisionQuestion": "TUNE THE LEARNING RATE", "familyTags": ["a", "b"], "scope": ["src"], "idempotencyKey": "ar-dup" }]));
    s.call("createAutoresearch", json!([p, { "decisionQuestion": "Rounds", "maximumRounds": 17, "idempotencyKey": "ar-rounds" }]));
    s.call("createAutoresearch", json!([p, { "decisionQuestion": "In direction", "directionId": did1, "idempotencyKey": "ar-direction" }]));
    s.call("createAutoresearch", json!([p, { "decisionQuestion": "Contract", "evaluationContract": "nope", "idempotencyKey": "ar-contract" }]));
    let arid = r(ar1, json!(["entityId"]));
    let ar_running = s.call("start", json!([p, "autoresearch", arid, 1, "start-ar1"]));
    let advanced = s.call(
        "advanceAutoresearch",
        json!([
            p,
            arid,
            r(ar_running, json!(["version"])),
            1,
            ["exp_b", "exp_a", "exp_b"],
            "advance-1"
        ]),
    );
    let av = r(advanced, json!(["version"]));
    s.call(
        "advanceAutoresearch",
        json!([p, arid, av, 99, [], "advance-2"]),
    );
    s.call(
        "transitionAutoresearch",
        json!([p, arid, av, "completed", "ar-complete"]),
    );
    s.call(
        "transitionAutoresearch",
        json!([p, arid, av, "draft", "ar-illegal"]),
    );
    s.call(
        "transitionAutoresearch",
        json!([p, arid, av, "paused", "ar-pause"]),
    );
    s.call("autoresearchBudgetUse", json!([p, arid]));
    let spawned_ar = s.call("createAutoresearch", json!([p, { "decisionQuestion": "Spawned improvement", "missionId": mid1, "idempotencyKey": "ar-spawn" }, { "invokedByRole": "mission_director", "origin": { "scopeType": "mission", "scopeId": mid1, "nodeId": node(m1, 1), "attempt": 1 }, "baseCommit": null }]));
    let _ = (spawned_direction, spawned_ar);

    // User Evidence and Claims, Reviews, and daemon records.
    let evidence_input = json!({ "evidenceType": "evidence_observation", "statement": "ECE fell", "polarity": "supports", "sourceRefs": [{ "locator": "report.md", "refId": "ref_1", "refType": "ref_file" }], "evaluationContractHash": null, "scopeLimitations": [] });
    let ev = s.call("submitUserEvidence", json!([p, evidence_input, "ev1"]));
    s.call("submitUserEvidence", json!([p, evidence_input, "ev1"]));
    s.call("submitUserEvidence", json!([p, { "evidenceType": "evidence_observation", "statement": "Different", "polarity": "supports", "sourceRefs": [], "evaluationContractHash": null, "scopeLimitations": [] }, "ev1"]));
    s.call(
        "submitUserEvidence",
        json!([p, { "statement": "x", "secret": true }, "ev-bad"]),
    );
    s.call(
        "submitUserEvidence",
        json!([p, { "projectId": "prj_00000000000000000000000000000000" }, "ev-project"]),
    );
    let evd = r(ev, json!(["payload", "evidenceId"]));
    let claim_input = json!({ "text": "Binning helps", "claimType": "claim_finding", "status": "hypothesis", "supportingEvidenceIds": [evd], "contradictingEvidenceIds": [], "qualifyingEvidenceIds": [], "limitations": [], "paperLocations": [], "requiredReviewId": null });
    let cl = s.call("submitUserClaim", json!([p, claim_input, "cl1"]));
    s.call("submitUserClaim", json!([p, { "text": "Missing", "claimType": "claim_finding", "status": "hypothesis", "supportingEvidenceIds": ["evd_00000000000000000000000000000000"], "contradictingEvidenceIds": [], "qualifyingEvidenceIds": [], "limitations": [], "paperLocations": [], "requiredReviewId": null }, "cl-missing"]));
    s.call("submitUserClaim", json!([p, { "text": "Unreviewed", "claimType": "claim_finding", "status": "supported", "supportingEvidenceIds": [evd], "contradictingEvidenceIds": [], "qualifyingEvidenceIds": [], "limitations": [], "paperLocations": [], "requiredReviewId": null }, "cl-review"]));
    let clm = r(cl, json!(["payload", "claimId"]));
    s.call(
        "editUserClaim",
        json!([p, clm, 1, { "text": "Binning helps at 30%" }, "cl1-edit"]),
    );
    s.call(
        "editUserClaim",
        json!([p, clm, 1, { "text": "Stale" }, "cl1-stale"]),
    );
    s.call(
        "editUserClaim",
        json!([p, "clm_00000000000000000000000000000000", 1, { "text": "x" }, "cl-unknown"]),
    );
    s.call(
        "editUserClaim",
        json!([p, clm, 0, { "text": "x" }, "cl-zero"]),
    );
    submit_review(
        &mut s,
        review(
            "rev_22222222222222222222222222222222",
            "claim",
            json!({ "targetId": clm, "targetType": "claim_record", "targetVersion": 2 }),
            json!([evd]),
        ),
        '2',
    );
    submit_review(
        &mut s,
        review(
            "rev_11111111111111111111111111111111",
            "claim",
            json!({ "targetId": clm, "targetType": "claim_record", "targetVersion": 3 }),
            json!([evd]),
        ),
        '3',
    );
    s.call("editUserClaim", json!([p, clm, 2, { "status": "supported", "requiredReviewId": "rev_22222222222222222222222222222222" }, "cl1-wrong-review"]));
    s.call("editUserClaim", json!([p, clm, 2, { "status": "supported", "requiredReviewId": "rev_11111111111111111111111111111111" }, "cl1-supported"]));
    s.call("latestClaims", json!([p]));
    s.call("reviews", json!([p]));
    let reviewed_evidence = "evd_77777777777777777777777777777777";
    submit_review(
        &mut s,
        review(
            "rev_33333333333333333333333333333333",
            "task",
            json!({ "targetId": "node_x", "targetType": "graph_node", "targetVersion": 1 }),
            json!([reviewed_evidence]),
        ),
        '4',
    );
    // A second verdict for the same Review makes it ambiguous.
    let (_, duplicate) = review(
        "rev_22222222222222222222222222222222",
        "claim",
        json!({ "targetId": clm, "targetType": "claim_record", "targetVersion": 2 }),
        json!([]),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            "task:tsk_66666666666666666666666666666666",
            duplicate,
            "review-verdict-dup"
        ]),
    );
    s.call("reviews", json!([p]));
    let evidence_record = fixture(
        "evidence",
        json!({ "evidenceId": reviewed_evidence, "projectId": p, "quality": { "confidence": "high", "reviewId": "rev_33333333333333333333333333333333", "status": "reviewed" } }),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            reviewed_evidence,
            evidence_record,
            "evidence-1"
        ]),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            reviewed_evidence,
            evidence_record,
            "evidence-1"
        ]),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            reviewed_evidence,
            evidence_record,
            "evidence-2"
        ]),
    );
    let mut disputed = evidence_record.clone();
    disputed["evidenceId"] = json!("evd_88888888888888888888888888888888");
    disputed["quality"]["status"] = json!("rejected");
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            "evd_88888888888888888888888888888888",
            disputed,
            "evidence-3"
        ]),
    );
    s.call(
        "submitDaemonRecord",
        json!([p, project_scope, "x", progress, "domain-path"]),
    );
    s.call("submitDaemonRecord", json!([p, scope(json!("mis_00000000000000000000000000000000"), Value::Null, Value::Null), "x", { "$schema": "https://nosh.dev/schemas/blocker/v1" }, "invalid"]));
    s.call(
        "submitDaemonRecord",
        json!([p, project_scope, "x", { "schemaVersion": 1 }, "no-schema"]),
    );
    let failure = fixture("task-failure", json!({ "taskId": TASK }));
    s.call(
        "submitDaemonRecord",
        json!([p, project_scope, "task:bad", failure, "failure-bad"]),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            format!("task:{TASK}"),
            failure,
            "failure-1"
        ]),
    );
    s.call(
        "submitDaemonRecord",
        json!([
            p,
            project_scope,
            format!("task:{TASK}"),
            failure,
            "failure-1"
        ]),
    );
    s.call("terminalRecord", json!([p, TASK, "task-failure"]));
    s.call(
        "terminalRecord",
        json!([p, TASK, "general-worker-completion"]),
    );
    s.call(
        "terminalRecord",
        json!([p, "tsk_00000000000000000000000000000000"]),
    );
    s.call("records", json!([p, "claim"]));
    s.call("submitted", json!([p, format!("task:{TASK}")]));
    s.call("offerCandidateEvidence", json!([p, evidence_record]));
    s.call("candidateEvidence", json!([p, reviewed_evidence]));
    s.call("withdrawCandidateEvidence", json!([p, reviewed_evidence]));
    s.call("candidateEvidence", json!([p, reviewed_evidence]));
    let external = s.call(
        "beginExternalOperation",
        json!([p, "custom.op", "ext-1", { "a": 1 }]),
    );
    s.call(
        "beginExternalOperation",
        json!([p, "custom.op", "ext-1", { "a": 2 }]),
    );
    s.call("pendingExternalOperations", json!([p, "custom.op"]));
    s.call(
        "completeExternalOperation",
        json!([p, r(external, json!(["intentId"])), { "done": true }]),
    );
    s.call("externalOperation", json!([p, "ext-1"]));
    s.call("externalOperation", json!([p, "ext-none"]));
    s.call("pendingExternalOperations", json!([p]));

    // The paper.
    let paper = s.call("readPaper", json!([p]));
    let expected = json!({ "markdownHash": r(paper, json!(["markdownHash"])), "bibliographyHash": r(paper, json!(["bibliographyHash"])), "version": r(paper, json!(["version"])) });
    s.call(
        "savePaper",
        json!([
            p,
            "# Paper\n\nBody é.\n",
            "@article{a, title={T}}\n",
            expected,
            "paper-1"
        ]),
    );
    s.call(
        "savePaper",
        json!([
            p,
            "# Paper\n\nBody é.\n",
            "@article{a, title={T}}\n",
            expected,
            "paper-1"
        ]),
    );
    s.call(
        "savePaper",
        json!([p, "# Other\n", "", expected, "paper-1"]),
    );
    s.call(
        "savePaper",
        json!([p, "# Other\n", "", expected, "paper-2"]),
    );
    s.call("readPaper", json!([p]));
    s.call("reconcilePendingOperations", json!([p]));

    // Usage, runs, and the completion queries.
    s.store("appendIdempotent", json!(["usage-1", event("agent.completed", scope(Value::Null, did1.clone(), Value::Null), json!({ "modelTokens": 100, "inputTokens": 20, "outputTokens": 10, "cacheReadTokens": 60, "cacheWriteTokens": 10 }))]));
    s.store(
        "appendIdempotent",
        json!([
            "usage-2",
            event(
                "agent.failed",
                scope(mid2.clone(), Value::Null, Value::Null),
                json!({ "modelTokens": 42 })
            )
        ]),
    );
    let mut job_scope = scope(mid2.clone(), Value::Null, Value::Null);
    job_scope["jobId"] = json!("job_11111111111111111111111111111111");
    s.store("appendIdempotent", json!(["job-1", event("job.state_changed", job_scope.clone(), json!({ "usesGpu": true, "startedAt": "2026-10-17T00:00:00.000Z", "finishedAt": null }))]));
    s.store("appendIdempotent", json!(["job-2", event("job.state_changed", job_scope, json!({ "usesGpu": true, "startedAt": "2026-10-17T00:00:00.000Z", "finishedAt": "2026-10-17T00:01:30.500Z" }))]));
    s.store("appendIdempotent", json!(["run-1", event("task.command_run", scope(mid1.clone(), Value::Null, Value::Null), json!({ "taskId": TASK, "jobId": "job_22222222222222222222222222222222", "commandId": "test", "description": "Unit tests", "argv": ["node", "--test"], "commit": head, "dirty": false, "state": "succeeded", "exitCode": 0, "stdoutTail": "ok \"1\"\n" }))]));
    s.call("directionBudgetUse", json!([p, did1]));
    s.call("missionBudgetUse", json!([p, mid2]));
    s.call("missionCompletionBasis", json!([p, mid1]));
    s.call("missionCompletionIssues", json!([p, mid1]));
    s.call("spawnedChild", json!([p, mid1, node(m1, 0)]));
    s.call("spawnedChild", json!([p, mid1, node(m1, 1)]));
    s.call("spawnedChild", json!([p, mid1, node(m1, 2)]));
    s.call(
        "researchInputs",
        json!([p, mid1, [node(m1, 0), node(m1, 1)]]),
    );
    s.call("childEvidenceIds", json!([p, did1]));
    s.call("scopedEvents", json!([p, mid2, "agent.failed"]));
    s.call("eventsAfter", json!([p, 80]));
    s.function("taskCommandRuns", json!([p]));
    s.function("daemonTaskFacts", json!([p, TASK]));
    s.function("missionOutcomes", json!([p, mid1, 3000]));
    s.function("taskCommandInstructions", json!([[{ "commandId": "test", "description": "Unit tests", "argv": ["node", "--test"], "timeoutSeconds": 60 }]]));
    s.function("taskCommandInstructions", json!([[]]));
    let running_events = json!([
        { "type": "mission.running", "timestamp": "1970-01-01T00:00:00.000Z" },
        { "type": "mission.running", "timestamp": "1970-01-01T00:01:00.000Z" },
        { "type": "mission.paused", "timestamp": "1970-01-01T00:10:00.000Z" },
        { "type": "mission.running", "timestamp": "1970-01-01T00:20:00.000Z" },
    ]);
    s.function(
        "runningSeconds",
        json!([running_events, "mission", [[300000, 420000]], 1800000]),
    );
    s.function("downtimeIntervals", json!([[{ "type": "a", "timestamp": "1970-01-01T00:00:01.000Z" }, { "type": "recovery.report", "timestamp": "1970-01-01T00:00:05.000Z" }, { "type": "recovery.report", "timestamp": "1970-01-01T00:00:05.000Z" }]]));
    s.0
}

#[test]
fn matches_the_typescript_research_control() {
    let dir = tempfile::tempdir().unwrap();
    let ops = scenario();
    let mut outputs = Vec::new();
    let mut runs = Vec::new();
    for name in ["ts", "rust"] {
        let root = dir.path().join(name).join("repo");
        repository(&root);
        let root = nosh_sys::paths::realpath_native(&root).unwrap();
        let database = dir.path().join(name).join("nosh.sqlite");
        let mut output = if name == "ts" {
            run_typescript(&root, &database, &ops)
        } else {
            run_rust(&root, &database, &ops)
        };
        output["state"]["receipts"] = receipts(&database);
        let mut mask = Mask {
            root: root.to_string_lossy().into_owned(),
            database: database.to_string_lossy().into_owned(),
            seen: HashMap::new(),
        };
        outputs.push(mask.output(&output));
        runs.push((root, database, output));
    }
    // Each implementation replays the other's keyed commands against the other's database: it
    // must find every stored receipt (the same intent hashes), return what the original returned,
    // and publish nothing.
    for (from, replayer) in [(0, "rust"), (1, "ts")] {
        let (root, database, original) = &runs[from];
        let (replay, replayed) = replay_ops(&ops, original["results"].as_array().unwrap());
        let output = if replayer == "ts" {
            run_typescript(root, database, &replay)
        } else {
            run_rust(root, database, &replay)
        };
        for index in replayed {
            let expected = json!({ "ok": original["results"][index]["ok"], "published": [] });
            assert_eq!(
                nosh_wire::canonical_json(&output["results"][index]),
                nosh_wire::canonical_json(&expected),
                "{replayer} replaying op {index} {}",
                ops[index]
            );
        }
    }
    let (ts, rust) = (&outputs[0], &outputs[1]);
    if std::env::var_os("NOSH_SCENARIO_LOG").is_some() {
        for (index, result) in ts.0.iter().enumerate() {
            let label = ops[index]
                .get("method")
                .or(ops[index].get("fn"))
                .or(ops[index].get("store"))
                .unwrap();
            eprintln!(
                "{index:>3} {label}: {}",
                nosh_wire::js::slice_to(&nosh_wire::canonical_json(result), 160)
            );
        }
    }
    let mut differences = Vec::new();
    for (index, (theirs, ours)) in ts.0.iter().zip(&rust.0).enumerate() {
        if theirs != ours {
            differences.push(format!(
                "op {index} {}:\n  ts:   {}\n  rust: {}",
                ops[index],
                nosh_wire::canonical_json(theirs),
                nosh_wire::canonical_json(ours)
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "{} ops differ:\n{}",
        differences.len(),
        differences.join("\n")
    );
    for part in ["events", "receipts", "intents", "projections"] {
        assert_eq!(
            nosh_wire::canonical_json(&ts.1[part]),
            nosh_wire::canonical_json(&rust.1[part]),
            "final {part}"
        );
    }
    // The scenario exercises successes as well as refusals.
    let failed =
        ts.0.iter()
            .filter(|result| result.get("error").is_some())
            .count();
    assert!(
        failed > 20 && failed < ts.0.len() / 2,
        "{failed} of {} ops failed",
        ts.0.len()
    );
}

/// Keyed commands that replay from their stored receipt once they have succeeded.
const REPLAYABLE: &[&str] = &[
    "createMission",
    "transitionMission",
    "mutateMissionGraph",
    "transitionMissionNode",
    "createDirection",
    "transitionDirection",
    "transitionDirectionNode",
    "mutateDirectionGraph",
    "approveGraphProposal",
    "acceptDirectionBaseline",
    "createAutoresearch",
    "transitionAutoresearch",
    "advanceAutoresearch",
    "submitDaemonRecord",
    "submitUserEvidence",
    "submitUserClaim",
    "editUserClaim",
    "savePaper",
    "applyDomainEffect",
];

/// The scenario as a replay of an earlier run: its successful keyed commands run again, and every
/// other op stands in with its original outcome. Returns the ops and the replayed indices.
fn replay_ops(ops: &[Value], results: &[Value]) -> (Vec<Value>, Vec<usize>) {
    let mut replayed = Vec::new();
    let replay = ops
        .iter()
        .zip(results)
        .enumerate()
        .map(|(index, (op, result))| match result.get("ok") {
            Some(_)
                if op["method"]
                    .as_str()
                    .is_some_and(|method| REPLAYABLE.contains(&method)) =>
            {
                replayed.push(index);
                op.clone()
            }
            Some(ok) => json!({ "result": ok }),
            None => json!({ "failed": result["error"] }),
        })
        .collect();
    (replay, replayed)
}

/// Every command receipt, by idempotency key: what a replay in either implementation reads.
fn receipts(database: &Path) -> Value {
    let connection = rusqlite::Connection::open(database).unwrap();
    let mut statement = connection.prepare("SELECT idempotency_key, result_json FROM command_receipts WHERE project_id = ? ORDER BY idempotency_key").unwrap();
    let rows = statement
        .query_map([PROJECT], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    Value::Array(rows.map(|row| row.unwrap()).map(|(key, result)| json!({ "key": key, "result": serde_json::from_str::<Value>(&result).unwrap() })).collect())
}
