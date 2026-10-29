//! Port of `apps/noshd/src/daemon-chat.test.ts`. The active-contract preflight is asserted on the
//! daemon's error; its HTTP 400/202 mapping is the server's (P6d `server` tests).

mod common;

use common::{FakeAgents, assert_match, daemon, inspection, local, schema_uri};
use nosh_agent::policy::Role;
use nosh_orchestration::daemon::{NoshDaemon, contract_successor_template, intake_prompt};
use nosh_research::project::read_project_contract;
use nosh_research::research_control::MissionInput;
use nosh_wire::{EVENT_SCHEMA, validate_record};
use serde_json::{Value, json};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct Fixture {
    _directory: tempfile::TempDir,
    agents: Rc<FakeAgents>,
    daemon: Rc<NoshDaemon>,
    project_id: String,
    metadata_path: PathBuf,
    draft_path: PathBuf,
    draft_bytes: String,
    contract: Value,
    active_path: PathBuf,
    emitted: Rc<Cell<usize>>,
    history: Vec<Value>,
}

/// A Project whose active contract is an approved version 2.
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let agents = Rc::new(FakeAgents::default());
    let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
    let project = daemon
        .initialize_project(
            &directory.path().join("repository"),
            true,
            Some("Chat fixture"),
            None,
        )
        .unwrap();
    let root = PathBuf::from(project["repositoryRoot"].as_str().unwrap());
    let metadata_path = root.join(".nosh").join("project.json");
    let mut metadata: Value =
        serde_json::from_str(&std::fs::read_to_string(&metadata_path).unwrap()).unwrap();
    let draft_path = root.join(".nosh").join("contracts").join("project.v1.json");
    let draft_bytes = std::fs::read_to_string(&draft_path).unwrap();
    let mut contract: Value = serde_json::from_str(&draft_bytes).unwrap();
    contract["contractVersion"] = json!(2);
    contract["approvedAt"] = json!(nosh_wire::now_utc());
    let active_path = root.join(".nosh").join("contracts").join("project.v2.json");
    std::fs::write(&active_path, contract.to_string()).unwrap();
    metadata["activeProjectContractVersion"] = json!(2);
    metadata["activeProjectContractPath"] = json!(".nosh/contracts/project.v2.json");
    std::fs::write(&metadata_path, metadata.to_string()).unwrap();
    let emitted = Rc::new(Cell::new(0));
    let counter = Rc::clone(&emitted);
    daemon.subscribe(Rc::new(move |_| counter.set(counter.get() + 1)));
    let project_id = project["projectId"].as_str().unwrap().to_owned();
    let history = daemon.replay(&project_id, 0).unwrap();
    Fixture {
        _directory: directory,
        agents,
        daemon,
        project_id,
        metadata_path,
        draft_path,
        draft_bytes,
        contract,
        active_path,
        emitted,
        history,
    }
}

impl Fixture {
    fn corrupt(&self) -> String {
        let mut invalid = self.contract.clone();
        invalid["northStar"]["contributionType"] = json!("invalid");
        invalid["baseline"] = json!({});
        let object = invalid.as_object_mut().unwrap();
        object.remove("templateVersion");
        object.remove("paper");
        let bytes = invalid.to_string();
        std::fs::write(&self.active_path, &bytes).unwrap();
        bytes
    }

    fn assert_no_effects(&self) {
        assert!(self.agents.untouched());
        assert_eq!(self.emitted.get(), 0);
        assert_eq!(
            self.daemon.replay(&self.project_id, 0).unwrap(),
            self.history
        );
    }

    async fn chat(
        &self,
        message: &str,
        key: &str,
        selection: Option<&Value>,
    ) -> nosh_orchestration::Result<nosh_orchestration::daemon::ChatReceipt> {
        self.daemon
            .chat(&self.project_id, message, key, selection)
            .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.daemon.stop();
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[test]
fn returns_an_actionable_error_without_ghost_messages_and_permits_the_same_key_after_explicit_repair()
 {
    local(async {
        let f = fixture();
        let invalid_bytes = f.corrupt();
        let metadata_bytes = std::fs::read_to_string(&f.metadata_path).unwrap();
        for key in [
            "chat-contract-retry-0001",
            "chat-contract-retry-0001",
            "chat-contract-retry-0002",
        ] {
            let error = f
                .chat("Hello", key, None)
                .await
                .expect_err("an invalid contract refuses chat")
                .to_string();
            for expected in [
                path_text(&f.active_path).as_str(),
                "templateVersion: Required",
                "northStar.contributionType:",
                "paper: Required",
                "+1 more validation issues",
                "Repair this file",
            ] {
                assert!(error.contains(expected), "{expected} missing from {error}");
            }
            assert!(!error.contains("\"code\""), "{error}");
        }
        f.assert_no_effects();
        assert_eq!(
            std::fs::read_to_string(&f.active_path).unwrap(),
            invalid_bytes
        );
        assert_eq!(
            std::fs::read_to_string(&f.metadata_path).unwrap(),
            metadata_bytes
        );
        assert_eq!(
            std::fs::read_to_string(&f.draft_path).unwrap(),
            f.draft_bytes
        );
        // Repair only this disposable fixture, without selecting the valid older draft.
        std::fs::write(&f.active_path, f.contract.to_string()).unwrap();
        let accepted = f
            .chat("Hello", "chat-contract-retry-0001", None)
            .await
            .unwrap();
        assert!(!accepted.replayed);
        assert_eq!(f.agents.started.borrow().len(), 1);
        assert_eq!(f.agents.prompts.borrow().len(), 1);
        assert_eq!(f.agents.prompts.borrow()[0].1, "Hello");
        assert_eq!(
            f.daemon
                .replay(&f.project_id, 0)
                .unwrap()
                .iter()
                .filter(|event| event["type"] == "chat.user_message")
                .count(),
            1
        );
        f.corrupt();
        let replay = f
            .chat("Hello", "chat-contract-retry-0001", None)
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(f.agents.prompts.borrow().len(), 1);
    });
}

#[test]
fn does_not_change_an_existing_session_when_its_contract_is_invalid() {
    for id in ["new", "old"] {
        local(async {
            let f = fixture();
            f.agents.sessions.borrow_mut().push(inspection(
                &f.project_id,
                "agt_00000000000000000000000000",
                None,
                Role::Nosh,
            ));
            f.corrupt();
            let selection = json!({ "provider": "test", "id": id, "thinkingLevel": "high" });
            let error = f
                .chat("Hello", "chat-contract-session-0001", Some(&selection))
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(&path_text(&f.active_path)), "{id}: {error}");
            assert_eq!(f.agents.validated.get(), 0);
            f.assert_no_effects();
        });
    }
}

#[test]
fn rejects_an_unreadable_contract_or_metadata_before_any_chat_side_effect() {
    for failure in [
        "malformed contract",
        "missing contract",
        "malformed metadata",
        "invalid metadata",
    ] {
        local(async {
            let f = fixture();
            match failure {
                "malformed contract" => std::fs::write(&f.active_path, "{broken").unwrap(),
                "missing contract" => std::fs::remove_file(&f.active_path).unwrap(),
                "malformed metadata" => std::fs::write(&f.metadata_path, "{broken").unwrap(),
                _ => std::fs::write(&f.metadata_path, "{}").unwrap(),
            }
            let error = f
                .chat("Hello", "chat-contract-unreadable-0001", None)
                .await
                .err()
                .unwrap()
                .to_string();
            let path = if failure.contains("metadata") {
                &f.metadata_path
            } else {
                &f.active_path
            };
            assert!(error.contains(&path_text(path)), "{failure}: {error}");
            assert_eq!(f.agents.validated.get(), 0);
            f.assert_no_effects();
        });
    }
}

#[test]
fn restores_the_recent_conversation_into_a_fresh_session_so_a_daemon_restart_does_not_erase_discovery()
 {
    local(async {
        let f = fixture();
        let first = f
            .chat("We study early exit.", "chat-restore-first-0001", None)
            .await
            .unwrap();
        assert_eq!(f.agents.prompts.borrow()[0].1, "We study early exit.");
        f.daemon
            .append_draft(&json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "agent.completed", "source": "pi", "scope": { "projectId": f.project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": first.agent_id }, "correlationId": null, "causationId": null, "payload": { "message": "Which decision should it inform?" } }))
            .unwrap();
        // The restart: the Pi session is gone.
        f.agents.sessions.borrow_mut().clear();
        f.chat("Whether to ship it.", "chat-restore-second-0001", None)
            .await
            .unwrap();
        let restored = f.agents.prompts.borrow()[1].1.clone();
        assert!(
            restored
                .contains("Conversation so far in this Project (restored after a session restart")
        );
        assert!(
            restored.contains("User: We study early exit.\nNOSH: Which decision should it inform?")
        );
        assert!(restored.ends_with("Whether to ship it."));
        assert!(!restored.contains("User: Whether to ship it."));
    });
}

#[test]
fn successor_template_is_schema_valid_once_its_edit_placeholders_are_filled_keeps_daemon_facts_and_prefills_detected_commands()
 {
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
                Some("Successor fixture"),
                None,
            )
            .unwrap();
        let draft =
            read_project_contract(Path::new(project["repositoryRoot"].as_str().unwrap())).unwrap();
        let commands = vec![
            json!({ "commandId": "command_test", "description": "Unit tests", "argv": ["node", "--test"], "timeoutSeconds": 120 }),
        ];
        let template = contract_successor_template(&draft, &commands);
        assert_match(
            &template,
            &json!({ "projectId": draft["projectId"], "contractVersion": draft["contractVersion"].as_f64().unwrap() + 1.0, "approvedAt": null, "createdAt": draft["createdAt"], "execution": { "runner": "native", "commands": commands } }),
        );
        let text = template.to_string();
        assert!(text.contains("\"EDIT:"));
        let filled = regex::Regex::new(r#""EDIT: a ref such as [^"]*""#)
            .unwrap()
            .replace_all(&text, "\"contribution_empirical.evaluation\"");
        let filled = regex::Regex::new(r#""EDIT: [^"]*""#)
            .unwrap()
            .replace_all(&filled, "\"filled by discovery\"");
        let filled: Value = serde_json::from_str(&filled).unwrap();
        validate_record(&schema_uri("project-contract"), &filled).unwrap();
        daemon.stop();
    });
}

#[test]
fn intake_prompt_lets_the_user_add_commands_beyond_the_detected_ones() {
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
                Some("Intake fixture"),
                None,
            )
            .unwrap();
        let root = Path::new(project["repositoryRoot"].as_str().unwrap());
        std::fs::write(
            root.join("package.json"),
            json!({ "scripts": { "test": "node --test" } }).to_string(),
        )
        .unwrap();
        let prompt = intake_prompt(
            &read_project_contract(root).unwrap(),
            root,
            &common::repository_root().join("conformance/schemas/json"),
        );
        assert!(prompt.contains("Runnable commands detected"));
        assert!(prompt.contains("Add any other command the user names"));
        daemon.stop();
    });
}

#[test]
fn one_step_start_walks_a_draft_mission_to_running_under_one_approval_idempotently_and_refuses_a_stale_version()
 {
    local(async {
        let f = fixture();
        let research = &f.daemon.research;
        let input = |title: &str, objective: &str, key: &str| MissionInput {
            title: title.into(),
            objective: objective.into(),
            deliverables: vec!["a result".into()],
            success_criteria: vec!["it runs".into()],
            non_objectives: None,
            starting_evidence: None,
            research_plan: None,
            budgets: None,
            idempotency_key: key.into(),
        };
        let mission = research
            .create_mission(
                &f.project_id,
                &input(
                    "Start fixture",
                    "Exercise start",
                    "start-fixture-create-0001",
                ),
            )
            .unwrap();
        let (id, version) = (
            mission["entityId"].as_str().unwrap().to_owned(),
            mission["version"].as_i64().unwrap(),
        );
        let stale = research
            .start(
                &f.project_id,
                "missions",
                &id,
                version + 1,
                "start-fixture-stale-0001",
            )
            .unwrap_err()
            .to_string();
        assert!(stale.contains("Stale missions version"), "{stale}");
        let started = research
            .start(
                &f.project_id,
                "missions",
                &id,
                version,
                "start-fixture-run-0001",
            )
            .unwrap();
        assert_match(
            &started,
            &json!({ "state": "running", "version": version + 3 }),
        );
        let again = research
            .start(
                &f.project_id,
                "missions",
                &id,
                started["version"].as_i64().unwrap(),
                "start-fixture-run-0002",
            )
            .unwrap();
        assert_match(
            &again,
            &json!({ "state": "running", "version": started["version"] }),
        );
        // A start refused by the one-active-Mission rule leaves the second Mission where it was, not half-walked.
        let second = research
            .create_mission(
                &f.project_id,
                &input("Second", "Wait its turn", "start-fixture-create-0002"),
            )
            .unwrap();
        let (second_id, second_version) = (
            second["entityId"].as_str().unwrap().to_owned(),
            second["version"].as_i64().unwrap(),
        );
        let refused = research
            .start(
                &f.project_id,
                "missions",
                &second_id,
                second_version,
                "start-fixture-run-0003",
            )
            .unwrap_err()
            .to_string();
        assert!(refused.contains("one active Mission"), "{refused}");
        assert_match(
            &research.mission(&f.project_id, &second_id).unwrap(),
            &json!({ "state": "draft", "version": second_version }),
        );
    });
}
