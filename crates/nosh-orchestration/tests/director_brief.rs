//! Port of `apps/noshd/src/director-brief.test.ts`. The digest-fitting and closure-answer tests
//! live with the supervisor ports, beside the functions they check.

mod common;

use common::{FakeAgents, assert_match, daemon, local};
use nosh_orchestration::director_brief::{direction_brief, mission_brief};
use nosh_orchestration::task_templates::{
    DaemonEvaluation, REGISTER_ARTIFACT, UNDECIDED_STATUS, UNDECIDED_VERDICT,
    completion_instructions, general_worker_completion_template, librarian_instructions,
    reviewer_instructions,
};
use nosh_research::research_control::{DirectionInput, MissionInput, librarian_network_allowed};
use nosh_wire::{IdPrefix, create_id, episode_type_for_role, schema_uri, validate_record};
use serde_json::{Value, json};
use std::path::Path;
use std::rc::Rc;

fn valid(schema: &str, record: &Value) -> bool {
    validate_record(&schema_uri(schema), record).is_ok()
}

fn last_line(text: &str) -> Value {
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn give_the_research_director_its_scope_state_and_daemon_allocated_cycle_identity() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let project = daemon
            .initialize_project(
                &directory.path().join("repository"),
                true,
                Some("Brief fixture"),
                None,
            )
            .unwrap();
        let project_id = project["projectId"].as_str().unwrap();
        let input = DirectionInput {
            question: "Does a bounded threshold improve score?".into(),
            decision_use: "Adopt one configuration or not.".into(),
            mission_id: None,
            evaluation_contract: Some(
                json!({ "primaryMetric": { "name": "score", "objective": "maximize", "minimumEffect": 0.01 }, "execution": { "runner": "native", "command": ["node", "evaluate.mjs"], "resultPath": "metrics.json", "timeoutSeconds": 60 } }),
            ),
            idempotency_key: "brief-direction-0001".into(),
        };
        let direction = daemon
            .research
            .create_direction(project_id, &input, None)
            .unwrap();
        let node = &direction["value"]["nodes"][0];
        let agent_id = create_id(IdPrefix::Agt);
        let brief = direction_brief(
            &daemon.research,
            &project,
            &direction,
            node,
            &agent_id,
            None,
            daemon.schema_dir(),
        )
        .unwrap();
        let (prompt, ids) = (&brief.prompt, &brief.ids);
        for expected in [
            "Does a bounded threshold improve score?".to_owned(),
            "Adopt one configuration or not.".to_owned(),
            direction["value"]["evaluationContractHash"]
                .as_str()
                .unwrap()
                .to_owned(),
            format!(
                "Selected by the deterministic scheduler to run next: {}",
                node["id"].as_str().unwrap()
            ),
            "Failure ledger:".to_owned(),
            "Event digest".to_owned(),
            ids.cycle_id.clone(),
            "research-director-cycle.v1.schema.json".to_owned(),
            episode_type_for_role("research_director"),
        ] {
            assert!(prompt.contains(&expected), "{expected}");
        }
        // Live, a director read one run's exhausted budget as the Direction's and halted: the brief states both.
        assert!(
            regex::Regex::new(r"Budget: this Direction has used \d+ of \S+ model tokens")
                .unwrap()
                .is_match(prompt)
        );
        assert!(prompt.contains("one exhausting its own budget does not exhaust this Direction"));
        assert!(prompt.contains("resolved on them"));
        for item in direction["value"]["nodes"].as_array().unwrap() {
            assert!(prompt.contains(item["id"].as_str().unwrap()));
        }
        assert!(ids.to_sequence >= ids.from_sequence);
        assert!(!prompt.contains("nosh_response_submit"));

        // The templates a director copies must already validate exactly as the host will check them.
        let lines: Vec<&str> = prompt.lines().collect();
        let template = |label: &str| -> Value {
            serde_json::from_str(
                lines[lines
                    .iter()
                    .position(|line| line.starts_with(label))
                    .unwrap()
                    + 1],
            )
            .unwrap()
        };
        let (cycle, episode) = (template("Record 1 ("), template("Record 2 ("));
        assert_match(
            &cycle,
            &json!({ "cycleId": ids.cycle_id, "directionId": direction["entityId"], "directorAgentId": agent_id, "consumedEventRange": ids.consumed_event_range(), "submittedAt": ids.submitted_at }),
        );
        assert!(valid("research-director-cycle", &cycle));
        assert!(valid("episode-draft", &episode));
        assert_eq!(
            episode["episodeType"],
            json!(episode_type_for_role("research_director"))
        );
        daemon.stop();
    });
}

#[test]
fn record_user_steering_durably_and_show_it_to_the_next_mission_director_cycle() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let project = daemon
            .initialize_project(
                &directory.path().join("repository"),
                true,
                Some("Steer fixture"),
                None,
            )
            .unwrap();
        let project_id = project["projectId"].as_str().unwrap();
        let input = MissionInput {
            title: "Steer".into(),
            objective: "Steer durably".into(),
            deliverables: vec!["Note".into()],
            success_criteria: vec!["Recorded".into()],
            non_objectives: None,
            starting_evidence: None,
            research_plan: None,
            budgets: None,
            idempotency_key: "steer-mission".into(),
        };
        let mut mission = daemon.research.create_mission(project_id, &input).unwrap();
        let mission_id = mission["entityId"].as_str().unwrap().to_owned();
        let version = |mission: &Value| mission["version"].as_i64().unwrap();
        for state in ["planning", "awaiting_approval", "running"] {
            mission = daemon
                .research
                .transition_mission(
                    project_id,
                    &mission_id,
                    version(&mission),
                    state,
                    &format!("steer-{state}"),
                )
                .unwrap();
        }
        // No Director session is live: the steer must still be accepted and recorded.
        daemon
            .steer_mission(
                project_id,
                &mission_id,
                version(&mission),
                "Prioritize the baseline check.",
                "steer-key-0001",
            )
            .await
            .unwrap();
        let current = daemon.research.mission(project_id, &mission_id).unwrap();
        let brief = mission_brief(
            &daemon.research,
            &project,
            &current,
            &mission["value"]["nodes"][0],
            &create_id(IdPrefix::Agt),
            None,
            daemon.schema_dir(),
        )
        .unwrap();
        assert!(brief.prompt.contains("User steering"));
        assert!(brief.prompt.contains("Prioritize the baseline check."));
        // Node churn after the inspected version does not invalidate a steer; a state change does.
        let inspected = version(&current);
        let expires = nosh_wire::iso_from_ms(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as f64
                + 60_000.0,
        )
        .unwrap();
        let lease = json!({ "leaseId": create_id(IdPrefix::Tsk), "ownerId": create_id(IdPrefix::Agt), "version": inspected, "expiresAt": expires });
        let node_id = mission["value"]["nodes"][0]["id"].as_str().unwrap();
        let churned = daemon
            .research
            .transition_mission_node(
                project_id,
                &mission_id,
                inspected,
                node_id,
                "leased",
                "steer-churn",
                Some(&lease),
                false,
            )
            .unwrap();
        assert!(version(&churned) > inspected);
        daemon
            .steer_mission(
                project_id,
                &mission_id,
                inspected,
                "Still in scope.",
                "steer-key-0002",
            )
            .await
            .unwrap();
        daemon
            .research
            .transition_mission(
                project_id,
                &mission_id,
                version(&churned),
                "pausing",
                "steer-pausing",
            )
            .unwrap();
        let late = daemon
            .steer_mission(
                project_id,
                &mission_id,
                inspected,
                "Too late.",
                "steer-key-0003",
            )
            .await
            .unwrap_err();
        assert!(
            late.to_string()
                .contains("state changed after the inspected version"),
            "{late}"
        );
        daemon.stop();
    });
}

#[test]
fn librarian_network_authority_is_denied_under_the_default_project_policy() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let mut project = daemon
            .initialize_project(
                &directory.path().join("repository"),
                true,
                Some("Network fixture"),
                None,
            )
            .unwrap();
        assert!(!librarian_network_allowed(&project));
        project["repositoryRoot"] = json!(directory.path().join("missing"));
        assert!(!librarian_network_allowed(&project));
        daemon.stop();
    });
}

#[test]
fn the_librarian_template_cites_files_by_path_and_is_invalid_until_the_daemon_resolves_every_citation()
 {
    let task_id = create_id(IdPrefix::Tsk);
    let text = librarian_instructions(&task_id, false, Path::new("schemas"));
    let envelope = last_line(&text);
    let record = &envelope["records"][0];
    assert!(!valid("librarian-completion", record));
    // Report and bibliography paths are daemon-known and prefilled; only sources are the librarian's to name.
    assert_match(
        record,
        &json!({ "reportArtifactId": format!("artifact:research/{task_id}/report.md"), "bibliographyArtifactId": format!("artifact:research/{task_id}/bibliography.bib") }),
    );
    assert!(record.to_string().contains(REGISTER_ARTIFACT));
    let filled = regex::Regex::new(r#""artifact:[^"]+""#)
        .unwrap()
        .replace_all(&record.to_string(), |_: &regex::Captures| {
            json!(create_id(IdPrefix::Art)).to_string()
        })
        .into_owned();
    assert!(valid(
        "librarian-completion",
        &serde_json::from_str(&filled).unwrap()
    ));
    assert!(valid("episode-draft", &envelope["records"][1]));
    assert!(text.contains("network is disabled"));
}

#[test]
fn worker_templates_are_schema_valid_as_issued_so_a_worker_only_edits_judgment() {
    let schema_dir = Path::new("schemas");
    let packet = json!({ "taskId": create_id(IdPrefix::Tsk), "attempt": 1, "assignedAgentId": create_id(IdPrefix::Agt), "requiredOutputs": [{ "outputId": "output_x" }], "acceptanceCriteria": [{ "criterionId": "dnode_x", "validatorIds": ["validator_direction.postflight"], "required": true }], "lease": { "leaseId": "lease_x" }, "workspace": { "startingCommit": "a".repeat(40), "branch": "nosh/task-x" } });
    let evaluation = DaemonEvaluation {
        job_id: create_id(IdPrefix::Job),
        state: "completed".into(),
        exit_code: Some(0),
        display_command: "node evaluate.mjs".into(),
        metrics: json!({ "score": 0.84 }),
        metric_artifact_id: Some(create_id(IdPrefix::Art)),
        failure: None,
    };
    for template in [
        general_worker_completion_template(&packet, None),
        general_worker_completion_template(&packet, Some(&evaluation)),
    ] {
        assert!(valid("general-worker-completion", &template), "{template}");
    }
    let envelope = last_line(&completion_instructions(
        &general_worker_completion_template(&packet, Some(&evaluation)),
        "general_worker",
        schema_dir,
    ));
    assert_eq!(envelope["$schema"], json!(schema_uri("terminal-output")));
    assert!(valid("general-worker-completion", &envelope["records"][0]));
    assert!(valid("episode-draft", &envelope["records"][1]));
    assert_eq!(
        envelope["records"][1]["episodeType"],
        json!(episode_type_for_role("general_worker"))
    );

    let request = json!({ "reviewId": create_id(IdPrefix::Rev), "reviewRequestId": "request_x", "reviewType": "task", "target": { "targetType": "graph_node", "targetId": "dnode_x", "targetVersion": 1 }, "reviewerAgentId": create_id(IdPrefix::Agt), "requiredArtifactIds": [evaluation.metric_artifact_id], "requiredEvidenceIds": [], "criteria": [{ "criterionId": "dnode_x" }] });
    let review = last_line(&reviewer_instructions(&request, schema_dir));
    // As issued the verdict is undecided and must not validate; a reviewer has to choose.
    assert!(!valid("review-verdict", &review["records"][0]));
    let decided: Value = serde_json::from_str(
        &review["records"][0]
            .to_string()
            .replace(UNDECIDED_VERDICT, "PASS")
            .replace(UNDECIDED_STATUS, "PASS"),
    )
    .unwrap();
    assert!(valid("review-verdict", &decided));
    assert!(valid("episode-draft", &review["records"][1]));
}
