//! Port of the supervisor cases of `apps/noshd/src/mission-supervisor.test.ts` (Mission and
//! Direction), plus the `fitDigest` and `acceptedClosureAnswer` cases of `director-brief.test.ts`. The file's research-control cases (budgets,
//! blocker scope, canonical contracts, claim versions, domain effects, leases, downtime) belong to
//! nosh-research. Two cases need TypeScript's `vi.spyOn` on the research control and are not
//! ported here: the completion-basis corruption checks and the research-inputs Artifact handoff.

mod common;

use common::assert_match;
use common::local;
use common::supervision::{Supervised, git, git_succeeds, mission_input, tick_until};
use nosh_agent::policy::{Role, SessionOptions};
use nosh_orchestration::Agents;
use nosh_orchestration::supervisors::direction::{DirectionSupervisor, accepted_closure_answer};
use nosh_orchestration::supervisors::mission::{DIGEST_SLOT, MissionSupervisor, fit_digest};
use nosh_store::ProjectionInput;
use nosh_wire::{IdPrefix, create_id, schema_uri, sha256};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

fn text(value: &Value) -> &str {
    value.as_str().unwrap()
}

fn joined(instructions: &Value) -> String {
    instructions
        .as_array()
        .unwrap()
        .iter()
        .map(text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn all_running() -> [&'static str; 3] {
    ["planning", "awaiting_approval", "running"]
}

/// Ticks a fresh supervisor (as after a restart) until the Mission completes or blocks.
async fn run_fresh(fixture: &Supervised, mission_id: &str, seconds: u64) {
    tick_until(
        seconds,
        || {
            MissionSupervisor::new(fixture.dependencies())
                .tick()
                .unwrap()
        },
        || {
            matches!(
                fixture.mission(mission_id)["state"].as_str(),
                Some("completed" | "blocked")
            )
        },
    )
    .await;
}

#[test]
fn runs_typed_director_worker_independent_review_and_final_review_gates() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let project_id = fixture.project_id.clone();
        let research = &fixture.research;
        let supervisor = MissionSupervisor::new(fixture.dependencies());
        let seed = research
            .submit_user_evidence(
                &project_id,
                &json!({ "evidenceType": "evidence_observation", "statement": "Seed result", "polarity": "supports", "sourceRefs": [{ "refType": "ref_note", "refId": "note_seed", "locator": "fixture" }], "evaluationContractHash": null, "scopeLimitations": [], "quality": { "status": "unreviewed", "reviewId": null, "confidence": "medium" } }),
                "seed-evidence",
            )
            .unwrap()["payload"]["evidenceId"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut input = mission_input(
            "Fixture",
            "Produce a reviewed result",
            &["Reviewed result"],
            &[
                "Deliverable artifact is preserved",
                "Independent review accepts every contract criterion",
            ],
            "fixture-create",
        );
        input.starting_evidence = Some(vec![seed.clone()]);
        let mission = fixture.running_mission(input, "fixture", &all_running());
        let mission_id = text(&mission["entityId"]).to_owned();
        tick_until(
            60,
            || supervisor.tick().unwrap(),
            || fixture.mission(&mission_id)["state"] == "completed",
        )
        .await;
        let completed = fixture.mission(&mission_id);
        let failures: Vec<Value> = research
            .scoped_events(&project_id, &mission_id, None)
            .unwrap()
            .into_iter()
            .filter(|entry| {
                [
                    "task.execution_failed",
                    "task.postflight_failed",
                    "mission.supervisor_error",
                    "mission.completion_rejected",
                    "focus.alarm",
                ]
                .contains(&text(&entry["type"]))
            })
            .collect();
        assert_eq!(
            json!({ "state": completed["state"], "failures": failures }),
            json!({ "state": "completed", "failures": [] })
        );
        assert!(
            completed["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| node["state"] == "accepted")
        );
        assert!(fixture.records("mission-director-cycle").len() >= 3);
        assert!(fixture.records("task-packet").len() >= 3);
        assert!(fixture.records("task-acknowledgement").is_empty());
        assert!(fixture.records("review-request").len() >= 4);
        assert!(fixture.records("review-verdict").len() >= 4);
        assert!(
            research
                .submitted(&project_id, None)
                .unwrap()
                .iter()
                .any(|entry| entry["scope"]["missionId"] == mission_id.as_str()
                    && entry["payload"]["$schema"] == schema_uri("mission-completion-packet"))
        );
        assert_eq!(
            fixture
                .store
                .operation_intents(&project_id, Some("pending"))
                .unwrap(),
            Vec::<Value>::new()
        );
        let basis = research
            .mission_completion_basis(&project_id, &mission_id)
            .unwrap();
        // Accepted worker commits are integrated on the Mission's own branch, and later tasks start
        // from that work.
        let branch = format!("nosh/mission-{}", &mission_id[4..16]);
        let integrated = git(
            &fixture.repository,
            &["ls-tree", "-r", "--name-only", &branch],
        );
        let endings: Vec<String> = basis
            .completions
            .iter()
            .filter_map(|entry| {
                let code = &entry.completion["codeChanges"];
                let ending = code["endingCommit"].as_str()?;
                (!ending.is_empty() && code["endingCommit"] != code["startingCommit"])
                    .then(|| ending.to_owned())
            })
            .collect();
        assert!(!endings.is_empty());
        for ending in &endings {
            assert!(git_succeeds(
                &fixture.repository,
                &["merge-base", "--is-ancestor", ending, &branch]
            ));
        }
        assert_eq!(
            integrated
                .lines()
                .filter(|name| name.starts_with("fixture-tsk_"))
                .count(),
            endings.len()
        );
        let packets = fixture.records("task-packet");
        let later = packets.iter().filter(|packet| {
            packet["scope"]["missionId"] == mission_id.as_str()
                && git_succeeds(
                    &fixture.repository,
                    &[
                        "merge-base",
                        "--is-ancestor",
                        &endings[0],
                        text(&packet["workspace"]["startingCommit"]),
                    ],
                )
        });
        assert!(later.count() > 0);
        // The researcher's starting Evidence reaches every worker packet, with an instruction to
        // read it.
        let worker_packets: Vec<&Value> = packets
            .iter()
            .filter(|packet| {
                packet["scope"]["missionId"] == mission_id.as_str()
                    && packet["assignedRole"] != "reviewer"
            })
            .collect();
        assert!(!worker_packets.is_empty());
        for packet in worker_packets {
            assert_eq!(packet["inputEvidenceIds"], json!([seed]));
            assert!(joined(&packet["instructions"]).contains(&seed));
        }
        assert_eq!(
            git(&fixture.repository, &["branch", "--show-current"]),
            "main"
        );
        let completion = fixture
            .records("mission-completion-packet")
            .last()
            .cloned()
            .unwrap();
        assert_eq!(basis.criteria.len(), 2);
        assert!(basis.criteria.iter().all(|criterion| {
            !criterion.validator_run_ids.is_empty()
                && !criterion.artifact_ids.is_empty()
                && !criterion.review_id.is_empty()
        }));
        assert_eq!(completion["criteria"].as_array().unwrap().len(), 2);
        assert_eq!(
            completion["deliverableArtifactIds"],
            json!(basis.artifact_ids)
        );
        assert_eq!(completion["openDefectIds"], json!(basis.open_defect_ids));
        let entries = research.submitted(&project_id, None).unwrap();
        let accepted = &basis.completions[0];
        let worker_packet = &entries
            .iter()
            .find(|entry| {
                entry["payload"]["$schema"] == schema_uri("task-packet")
                    && entry["payload"]["taskId"] == accepted.task_id.as_str()
            })
            .unwrap()["payload"];
        let request = entries
            .iter()
            .find(|entry| {
                entry["payload"]["$schema"] == schema_uri("review-request")
                    && entry["payload"]["reviewId"] == accepted.review["reviewId"]
            })
            .unwrap();
        assert_ne!(
            request["correlationId"],
            json!(format!("task:{}", accepted.task_id))
        );
        assert!(
            request["payload"]["contractRefs"]
                .as_array()
                .unwrap()
                .contains(
                    &json!({ "kind": "contract_task.packet", "id": accepted.task_id, "hash": sha256(worker_packet) })
                )
        );
    });
}

#[test]
fn completes_a_mission_whose_review_only_criterion_the_worker_defers_on_the_reviewers_criterion_pass_alone()
 {
    local(async {
        let fixture = Supervised::new(false, true, false);
        let supervisor = MissionSupervisor::new(fixture.dependencies());
        let mission = fixture.running_mission(
            mission_input(
                "Fixture",
                "Produce a reviewed result",
                &["Reviewed result"],
                &[
                    "Deliverable artifact is preserved",
                    "Independent review accepts every contract criterion",
                ],
                "fixture-create",
            ),
            "fixture",
            &all_running(),
        );
        let mission_id = text(&mission["entityId"]).to_owned();
        tick_until(
            60,
            || supervisor.tick().unwrap(),
            || fixture.mission(&mission_id)["state"] == "completed",
        )
        .await;
        assert_eq!(fixture.mission(&mission_id)["state"], "completed");
        let basis = fixture
            .research
            .mission_completion_basis(&fixture.project_id, &mission_id)
            .unwrap();
        let deferred = basis
            .criteria
            .iter()
            .find(|criterion| criterion.criterion_id == "criterion_2")
            .unwrap();
        assert_eq!(deferred.validator_run_ids, vec![deferred.review_id.clone()]);
    });
}

#[test]
fn reports_a_safe_pause_as_paused_only_after_every_mission_agent_has_stopped_at_its_boundary() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let supervisor = MissionSupervisor::new(fixture.dependencies());
        let mission = fixture.running_mission(
            mission_input(
                "Pause",
                "Pause safely",
                &["Note"],
                &["Paused"],
                "pause-create",
            ),
            "pause",
            &["planning", "awaiting_approval", "running", "pausing"],
        );
        let mission_id = text(&mission["entityId"]).to_owned();
        fixture
            .agents
            .start(SessionOptions {
                project_id: fixture.project_id.clone(),
                mission_id: Some(mission_id.clone()),
                direction_id: None,
                autoresearch_id: None,
                experiment_id: None,
                run_id: None,
                job_id: None,
                task_id: Some(create_id(IdPrefix::Tsk)),
                agent_id: create_id(IdPrefix::Agt),
                role: Role::GeneralWorker,
                cwd: PathBuf::new(),
                package_path: PathBuf::new(),
                tools: None,
                runtime_scoped: false,
                task_permissions: None,
                task_workspace: None,
                thinking_level: None,
                model: None,
            })
            .await
            .unwrap();
        supervisor.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fixture.mission(&mission_id)["state"], "pausing");
        for agent in fixture.agents.inspect() {
            fixture.agents.stop(&agent.agent_id);
        }
        supervisor.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fixture.mission(&mission_id)["state"], "paused");
    });
}

fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[test]
fn runs_typed_direction_director_immutable_baseline_evidence_and_closure_review_gates() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let project_id = fixture.project_id.clone();
        let research = &fixture.research;
        let supervisor = DirectionSupervisor::new(fixture.dependencies());
        let direction_id = fixture.direction_through(
            "Does the bounded change improve the metric?",
            "Choose whether to adopt it",
            "direction",
            &["proposed", "active"],
        );
        // The hypothesis node waits for the Direction's Autoresearch instead of dispatching a
        // worker.
        let awaiting = || {
            research
                .events_after(&project_id, 0)
                .unwrap()
                .iter()
                .any(|event| {
                    event["type"] == "direction.awaiting_autoresearch"
                        && event["scope"]["directionId"] == direction_id.as_str()
                })
        };
        tick_until(60, || supervisor.tick().unwrap(), awaiting).await;
        assert!(awaiting());
        let baseline = |direction: &Value| {
            direction["value"]["acceptedBaseline"]["commit"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        assert!(is_commit(&baseline(&fixture.direction(&direction_id))));
        // TypeScript stubs `research.autoresearch` to list one more, completed run; here that run's
        // projection is written to the store.
        let run_id = create_id(IdPrefix::Ar);
        fixture
            .store
            .mutate_projection(
                &format!("fixture-autoresearch-{run_id}"),
                Some(0),
                &json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "fixture.autoresearch_listed", "source": "fixture", "scope": { "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": run_id, "experimentId": null, "runId": null, "jobId": null, "agentId": null }, "correlationId": run_id, "causationId": null, "payload": {} }),
                &ProjectionInput {
                    entity_type: "autoresearch".into(),
                    entity_id: run_id.clone(),
                    state: "completed".into(),
                    value: json!({ "directionId": direction_id }),
                    graph: None,
                    records: Vec::new(),
                    intent: None,
                },
            )
            .unwrap();
        tick_until(
            60,
            || supervisor.tick().unwrap(),
            || fixture.direction(&direction_id)["state"] == "closed",
        )
        .await;
        let closed = fixture.direction(&direction_id);
        assert_eq!(closed["state"], "closed");
        let commit = baseline(&closed);
        assert!(is_commit(&commit));
        assert_eq!(
            git(&fixture.repository, &["cat-file", "-t", &commit]),
            "commit"
        );
        // Task work stays on its worktree branch; the main branch is not advanced by workers.
        assert_eq!(
            git(
                &fixture.repository,
                &["merge-base", "--is-ancestor", "HEAD", &commit]
            ),
            ""
        );
        assert!(
            closed["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| node["state"] == "accepted")
        );
        let submitted = research.submitted(&project_id, None).unwrap();
        let submitted_in_direction = |schema: &str| {
            submitted.iter().any(|entry| {
                entry["scope"]["directionId"] == direction_id.as_str()
                    && entry["payload"]["$schema"] == schema_uri(schema)
            })
        };
        let recorded_in_direction = |schema: &str| {
            fixture
                .records(schema)
                .iter()
                .any(|record| record["scope"]["directionId"] == direction_id.as_str())
        };
        assert!(submitted_in_direction("evidence"));
        assert!(recorded_in_direction("task-packet"));
        assert!(recorded_in_direction("review-request"));
        // The hypothesis node is resolved from Autoresearch without a director turn.
        assert!(fixture.records("research-director-cycle").len() >= 2);
        assert!(submitted_in_direction("direction-closure-packet"));
        // Rust-only: with no experiment results the closure answers honestly inconclusive.
        let closure = submitted
            .iter()
            .find(|entry| entry["payload"]["$schema"] == schema_uri("direction-closure-packet"))
            .unwrap();
        assert_eq!(closure["payload"]["questionDisposition"], "inconclusive");
        assert_eq!(
            closure["payload"]["answer"],
            "No experiment produced an accepted result."
        );
        assert_eq!(
            fixture
                .store
                .operation_intents(&project_id, Some("pending"))
                .unwrap(),
            Vec::<Value>::new()
        );
    });
}

#[test]
fn reconstructs_focus_fingerprints_when_the_supervisor_process_state_is_replaced() {
    local(async {
        let fixture = Supervised::new(true, false, false);
        let mission = fixture.running_mission(
            mission_input(
                "Focus",
                "Reject tunnel vision",
                &["Reviewed result"],
                &["Repeated work is stopped"],
                "focus-create",
            ),
            "focus",
            &all_running(),
        );
        let mission_id = text(&mission["entityId"]).to_owned();
        tick_until(
            60,
            || {
                MissionSupervisor::new(fixture.dependencies())
                    .tick()
                    .unwrap()
            },
            || !fixture.scoped(&mission_id, "focus.alarm").is_empty(),
        )
        .await;
        assert!(fixture.scoped(&mission_id, "focus.attempt_recorded").len() >= 2);
        assert_match(
            &fixture.scoped(&mission_id, "focus.alarm")[0]["payload"],
            &json!({ "reasons": ["repeated_attempt_fingerprint"] }),
        );
        assert!(
            fixture.mission(&mission_id)["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["state"] == "blocked")
        );
    });
}

#[test]
fn releases_an_orphaned_working_lease_after_supervisor_restart() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let research = &fixture.research;
        let project_id = fixture.project_id.clone();
        let mut mission = fixture.running_mission(
            mission_input(
                "Lease recovery",
                "Resume without a lost owner",
                &["Recovered result"],
                &["Every node is accepted"],
                "lease-create",
            ),
            "lease",
            &all_running(),
        );
        let mission_id = text(&mission["entityId"]).to_owned();
        let node_id = text(&mission["value"]["nodes"][0]["id"]).to_owned();
        let expires = nosh_wire::iso_from_ms(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as f64
                + 60_000.0,
        )
        .unwrap();
        let lease = json!({ "leaseId": create_id(IdPrefix::Tsk), "ownerId": create_id(IdPrefix::Agt), "version": mission["version"], "expiresAt": expires });
        mission = research
            .transition_mission_node(
                &project_id,
                &mission_id,
                mission["version"].as_i64().unwrap(),
                &node_id,
                "leased",
                "orphan-lease",
                Some(&lease),
                false,
            )
            .unwrap();
        research
            .transition_mission_node(
                &project_id,
                &mission_id,
                mission["version"].as_i64().unwrap(),
                &node_id,
                "working",
                "orphan-working",
                None,
                false,
            )
            .unwrap();
        tick_until(
            60,
            || {
                MissionSupervisor::new(fixture.dependencies())
                    .tick()
                    .unwrap()
            },
            || fixture.mission(&mission_id)["state"] == "completed",
        )
        .await;
        assert_eq!(fixture.mission(&mission_id)["state"], "completed");
        assert_match(
            &fixture.scoped(&mission_id, "mission.node_recovered")[0]["payload"],
            &json!({ "nodeId": node_id, "priorState": "working", "disposition": "ready_for_new_lease" }),
        );
    });
}

#[test]
fn retries_transient_worker_failures_without_a_focus_alarm_so_a_node_gets_all_its_attempts() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        fixture.agents.transient_worker_failures.set(2);
        let mission = fixture.running_mission(
            mission_input(
                "Transient",
                "Survive provider glitches",
                &["Reviewed result"],
                &["Every node is accepted"],
                "transient-create",
            ),
            "transient",
            &all_running(),
        );
        let mission_id = text(&mission["entityId"]).to_owned();
        run_fresh(&fixture, &mission_id, 80).await;
        assert_eq!(
            fixture.scoped(&mission_id, "task.execution_failed").len(),
            2
        );
        assert_eq!(fixture.scoped(&mission_id, "focus.alarm").len(), 0);
        // The retried worker is told what ended its node's earlier attempts.
        let workers: Vec<Value> = fixture
            .records("task-packet")
            .into_iter()
            .filter(|packet| packet["assignedRole"] == "general_worker")
            .collect();
        assert!(
            !joined(&workers[0]["instructions"]).contains("Earlier attempts at this node failed")
        );
        assert!(
            joined(&workers[2]["instructions"]).contains("Earlier attempts at this node failed")
        );
        assert!(workers.iter().all(|packet| {
            packet["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|line| text(line).encode_utf16().count() <= 1_000)
        }));
        assert_eq!(fixture.mission(&mission_id)["state"], "completed");
    });
}

struct Resumed {
    first: Value,
    second: Value,
    started: Vec<String>,
    tree: Vec<String>,
    state: Value,
}

async fn resume_with(verdict: &str) -> Resumed {
    let fixture = Supervised::new(false, false, false);
    fixture
        .agents
        .worker_review_verdicts
        .borrow_mut()
        .push_back(verdict.to_owned());
    let mission = fixture.running_mission(
        mission_input(
            "Resume",
            "Build on reviewed work",
            &["Reviewed result"],
            &["Every node is accepted"],
            "resume-create",
        ),
        "resume",
        &all_running(),
    );
    let mission_id = text(&mission["entityId"]).to_owned();
    run_fresh(&fixture, &mission_id, 80).await;
    let workers: Vec<Value> = fixture
        .records("task-packet")
        .into_iter()
        .filter(|packet| packet["assignedRole"] == "general_worker")
        .collect();
    let (first, second) = (workers[0].clone(), workers[1].clone());
    let second_task = text(&second["taskId"]).to_owned();
    let ending = fixture
        .research
        .terminal_record(
            &fixture.project_id,
            &second_task,
            Some("general-worker-completion"),
        )
        .unwrap()
        .unwrap()["codeChanges"]["endingCommit"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = fixture
        .agents
        .worker_start_files
        .borrow()
        .get(&second_task)
        .cloned()
        .unwrap_or_default();
    let tree = git(
        &fixture.repository,
        &["ls-tree", "-r", "--name-only", &ending],
    )
    .split('\n')
    .map(str::to_owned)
    .collect();
    Resumed {
        first,
        second,
        started,
        tree,
        state: fixture.mission(&mission_id)["state"].clone(),
    }
}

#[test]
fn resumes_a_revised_worker_from_its_passed_attempts_commit_and_starts_a_redesign_over() {
    local(async {
        let revised = resume_with("REVISE").await;
        let first_file = |run: &Resumed| format!("fixture-{}.txt", text(&run.first["taskId"]));
        let second_file = |run: &Resumed| format!("fixture-{}.txt", text(&run.second["taskId"]));
        assert_eq!(revised.state, "completed");
        // The retry starts with the first attempt's file present, yet its diff base stays the
        // integration head, so the Review sees the node's whole change and the accepted commit
        // carries both attempts' work.
        assert!(revised.started.contains(&first_file(&revised)));
        assert_eq!(
            revised.second["workspace"]["startingCommit"],
            revised.first["workspace"]["startingCommit"]
        );
        assert!(
            joined(&revised.second["instructions"])
                .contains("already contains an earlier attempt's work")
        );
        assert!(revised.tree.contains(&first_file(&revised)));
        assert!(revised.tree.contains(&second_file(&revised)));
        let redesigned = resume_with("REDESIGN").await;
        assert_eq!(redesigned.state, "completed");
        assert!(!redesigned.started.contains(&first_file(&redesigned)));
        assert!(
            !joined(&redesigned.second["instructions"])
                .contains("already contains an earlier attempt's work")
        );
        assert!(!redesigned.tree.contains(&first_file(&redesigned)));
    });
}

#[test]
fn runs_a_fresh_director_cycle_after_an_invalid_cycle_record_and_blocks_only_after_three_in_a_row()
{
    local(async {
        let fixture = Supervised::new(false, false, false);
        let run = |title: &'static str| {
            let fixture = &fixture;
            async move {
                let mission = fixture.running_mission(
                    mission_input(
                        title,
                        "Survive director slips",
                        &["Reviewed result"],
                        &["Every node is accepted"],
                        &format!("{title}-create"),
                    ),
                    title,
                    &all_running(),
                );
                let mission_id = text(&mission["entityId"]).to_owned();
                // A fresh supervisor every tick: the slip count must come from durable events, not
                // process memory.
                run_fresh(fixture, &mission_id, 80).await;
                mission_id
            }
        };
        fixture.agents.director_slips.set(2);
        let recovered = run("slip").await;
        assert_eq!(
            fixture.scoped(&recovered, "mission.director_retry").len(),
            2
        );
        assert_eq!(fixture.mission(&recovered)["state"], "completed");
        fixture.agents.director_slips.set(3);
        let stuck = run("stuck").await;
        assert_eq!(fixture.scoped(&stuck, "mission.director_retry").len(), 2);
        assert_eq!(fixture.mission(&stuck)["state"], "blocked");
    });
}

#[test]
fn returns_a_mission_and_a_direction_left_in_reviewing_by_a_restart_to_their_running_state() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let mission = fixture.running_mission(
            mission_input(
                "Interrupted review",
                "Resume after restart",
                &["Result"],
                &["Reviewed"],
                "review-recovery-mission",
            ),
            "review-recovery",
            &["planning", "awaiting_approval", "running", "reviewing"],
        );
        let mission_id = text(&mission["entityId"]).to_owned();
        let direction_id = fixture.direction_through(
            "Does it hold?",
            "Decide",
            "review-recovery-d",
            &["proposed", "active", "reviewing"],
        );
        // A fresh supervisor (as after a daemon restart) finds both in reviewing with nothing
        // running.
        let missions = MissionSupervisor::new(fixture.dependencies());
        missions.stop();
        missions.tick().unwrap();
        DirectionSupervisor::new(fixture.dependencies())
            .tick()
            .unwrap();
        assert_eq!(fixture.mission(&mission_id)["state"], "running");
        assert_eq!(
            fixture
                .scoped(&mission_id, "mission.review_recovered")
                .len(),
            1
        );
        assert_eq!(fixture.direction(&direction_id)["state"], "active");
        assert_eq!(
            fixture
                .research
                .events_after(&fixture.project_id, 0)
                .unwrap()
                .iter()
                .filter(|event| {
                    event["type"] == "direction.review_recovered"
                        && event["scope"]["directionId"] == direction_id.as_str()
                })
                .count(),
            1
        );
    });
}

#[test]
fn fits_the_accepted_work_digest_into_the_step_objective_limit() {
    let prompt = format!("{} {DIGEST_SLOT} tail", "p".repeat(12_000));
    let fitted = fit_digest(&prompt, &"d".repeat(14_000));
    assert!(fitted.encode_utf16().count() <= 20_000);
    assert!(fitted.contains("digest truncated to fit"));
    assert!(fitted.ends_with(" tail"));
    assert_eq!(
        fit_digest(&format!("short {DIGEST_SLOT}"), "small digest"),
        "short small digest"
    );
    assert_eq!(fit_digest("no slot here", "ignored"), "no slot here");
}

#[test]
fn states_the_selected_result_its_improvement_and_that_selection_is_not_a_held_out_evaluation() {
    let answer = accepted_closure_answer(
        &["exp_1".to_owned()],
        &[
            json!({ "experimentId": "exp_1", "promotionDecision": "held" }),
            json!({ "experimentId": "exp_1", "promotionDecision": "promoted", "metrics": [{ "threshold": 0.7, "cost_reduction": 0.455 }], "comparison": { "parentScore": -0.1667, "improvement": 0.6217 } }),
        ],
    );
    // The metric's keys are sorted, where TypeScript keeps their insertion order.
    assert_eq!(
        answer,
        r#"Reviewed experiment exp_1 is the accepted frontier: daemon-measured {"cost_reduction":0.455,"threshold":0.7}, primary-metric improvement 0.6217 over its parent (-0.1667), guardrails passing. This selects a configuration on the frozen contract's evaluation data; it is not a held-out evaluation."#
    );
}

// Rust-only cases below: each pins a supervisor decision the TypeScript suite leaves unexercised
// (found by mutation testing the port).

fn default_mission(fixture: &Supervised, title: &str) -> String {
    let mission = fixture.running_mission(
        mission_input(
            title,
            "Produce a reviewed result",
            &["Reviewed result"],
            &["Every node is accepted"],
            &format!("{title}-create"),
        ),
        title,
        &all_running(),
    );
    text(&mission["entityId"]).to_owned()
}

fn node_of_type(mission: &Value, node_type: &str) -> Value {
    mission["value"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == node_type)
        .unwrap()
        .clone()
}

fn packets_for(fixture: &Supervised, role: &str) -> usize {
    fixture
        .records("task-packet")
        .iter()
        .filter(|packet| packet["assignedRole"] == role)
        .count()
}

#[test]
fn blocks_a_node_whose_review_returns_blocked_instead_of_retrying_it() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        fixture
            .agents
            .worker_review_verdicts
            .borrow_mut()
            .push_back("BLOCKED".into());
        let mission_id = default_mission(&fixture, "blocked-review");
        run_fresh(&fixture, &mission_id, 60).await;
        let mission = fixture.mission(&mission_id);
        assert_eq!(mission["state"], "blocked");
        assert_eq!(node_of_type(&mission, "implementation")["state"], "blocked");
        assert_eq!(packets_for(&fixture, "general_worker"), 1);
    });
}

#[test]
fn blocks_a_mission_whose_model_token_budget_is_spent_before_issuing_work() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let mut input = mission_input(
            "Budget",
            "Stay within budget",
            &["Reviewed result"],
            &["Every node is accepted"],
            "budget-create",
        );
        input.budgets = Some(
            json!({ "maximumModelTokens": 1_000 })
                .as_object()
                .unwrap()
                .clone(),
        );
        let mission = fixture.running_mission(input, "budget", &all_running());
        let mission_id = text(&mission["entityId"]).to_owned();
        common::supervision::append_fixture_event(
            &fixture.store,
            &json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "agent.completed", "source": "pi_adapter", "scope": { "projectId": fixture.project_id, "missionId": mission_id, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": create_id(IdPrefix::Agt) }, "correlationId": mission_id, "causationId": null, "payload": { "modelTokens": 5_000 } }),
        );
        run_fresh(&fixture, &mission_id, 30).await;
        assert_eq!(fixture.mission(&mission_id)["state"], "blocked");
        let exhausted = fixture.scoped(&mission_id, "mission.budget_exhausted");
        assert_eq!(exhausted.len(), 1);
        assert_eq!(
            exhausted[0]["payload"]["exhausted"],
            json!(["model_tokens"])
        );
        assert!(fixture.records("task-packet").is_empty());
    });
}

#[test]
fn fails_postflight_on_a_declared_command_that_did_not_pass_and_retries_the_node() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        fixture.agents.failing_commands.set(1);
        let mission_id = default_mission(&fixture, "failing-command");
        run_fresh(&fixture, &mission_id, 80).await;
        let failed = fixture.scoped(&mission_id, "task.postflight_failed");
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0]["payload"]["issues"]
                .as_array()
                .unwrap()
                .contains(&json!("a declared command did not pass"))
        );
        assert_eq!(fixture.mission(&mission_id)["state"], "completed");
    });
}

#[test]
fn releases_an_orphaned_lease_that_never_started_work_back_to_ready() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        let mission_id = default_mission(&fixture, "leased-orphan");
        let mission = fixture.mission(&mission_id);
        let node_id = text(&mission["value"]["nodes"][0]["id"]).to_owned();
        let expires = nosh_wire::iso_from_ms(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as f64
                + 60_000.0,
        )
        .unwrap();
        let lease = json!({ "leaseId": create_id(IdPrefix::Tsk), "ownerId": create_id(IdPrefix::Agt), "version": mission["version"], "expiresAt": expires });
        fixture
            .research
            .transition_mission_node(
                &fixture.project_id,
                &mission_id,
                mission["version"].as_i64().unwrap(),
                &node_id,
                "leased",
                "orphan-lease",
                Some(&lease),
                false,
            )
            .unwrap();
        run_fresh(&fixture, &mission_id, 60).await;
        assert_eq!(fixture.mission(&mission_id)["state"], "completed");
        assert_match(
            &fixture.scoped(&mission_id, "mission.node_recovered")[0]["payload"],
            &json!({ "nodeId": node_id, "priorState": "leased", "disposition": "ready_for_new_lease" }),
        );
    });
}

#[test]
fn blocks_a_mission_whose_director_judges_the_selected_node_does_not_contribute() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        fixture.agents.director_halts.set(1);
        let mission_id = default_mission(&fixture, "director-halt");
        run_fresh(&fixture, &mission_id, 30).await;
        assert_eq!(fixture.mission(&mission_id)["state"], "blocked");
        assert_eq!(
            fixture.scoped(&mission_id, "mission.supervisor_error")[0]["payload"]["message"],
            "Mission Director halted the selected node; user attention required"
        );
        assert!(fixture.records("task-packet").is_empty());
    });
}

#[test]
fn blocks_a_mission_whose_final_completion_review_does_not_pass() {
    local(async {
        let fixture = Supervised::new(false, false, false);
        fixture
            .agents
            .closure_review_verdicts
            .borrow_mut()
            .push_back("REVISE".into());
        let mission_id = default_mission(&fixture, "final-revise");
        run_fresh(&fixture, &mission_id, 80).await;
        let mission = fixture.mission(&mission_id);
        assert_eq!(mission["state"], "blocked");
        // Blocked by the supervisor's own verdict check, not by research control refusing the
        // completion after it.
        assert!(
            fixture
                .scoped(&mission_id, "mission.supervisor_error")
                .is_empty()
        );
        assert!(
            mission["value"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| node["state"] == "accepted")
        );
    });
}

#[test]
fn counts_a_rejected_reviewer_terminal_as_one_failed_review_but_blocks_on_any_other_reviewer_error()
{
    local(async {
        let fixture = Supervised::new(false, false, false);
        fixture
            .agents
            .reviewer_failures
            .borrow_mut()
            .push_back("Terminal output rejected: fixture reviewer slip".into());
        let slipped = default_mission(&fixture, "reviewer-slip");
        run_fresh(&fixture, &slipped, 80).await;
        assert_eq!(fixture.scoped(&slipped, "task.review_failed").len(), 1);
        assert_eq!(fixture.mission(&slipped)["state"], "completed");
        fixture
            .agents
            .reviewer_failures
            .borrow_mut()
            .push_back("provider unavailable".into());
        let broken = default_mission(&fixture, "reviewer-error");
        run_fresh(&fixture, &broken, 80).await;
        assert_eq!(fixture.mission(&broken)["state"], "blocked");
        assert_eq!(
            fixture.scoped(&broken, "mission.supervisor_error")[0]["payload"]["message"],
            "provider unavailable"
        );
        assert!(fixture.scoped(&broken, "task.review_failed").is_empty());
    });
}
