//! Port of `apps/noshd/src/task-commands.test.ts`. Task Packets are recorded as `record.submitted`
//! events instead of mocking `research.records` (the daemon reads the latest one), and the frozen
//! Autoresearch execution is a real projection.

mod common;

use common::{FakeAgents, assert_match, daemon, inspection, leased_worker, local, schema_uri};
use nosh_agent::policy::Role;
use nosh_agent::terminal::TerminalContext;
use nosh_orchestration::daemon::NoshDaemon;
use nosh_orchestration::task_postflight::{
    CitedCriterion, cited_run_issues, completion_run_issues, write_up_only,
};
use nosh_orchestration::task_worktree::{
    ResumePoint, create_task_worktree, resume_instruction, resume_point,
};
use nosh_research::project::{approve_project_contract, read_project_contract};
use nosh_research::research_control::{
    prior_attempt_instructions, resumable_attempt, task_command_instructions,
};
use nosh_store::ProjectionInput;
use nosh_wire::{EVENT_SCHEMA, IdPrefix, create_id};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::Instant;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit_all(root: &Path, file: &str, message: &str) {
    git(root, &["add", file]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-m",
            message,
        ],
    );
}

fn event(kind: &str, project_id: &str, correlation: Value, payload: Value) -> Value {
    json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": kind, "source": "noshd", "scope": { "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null }, "correlationId": correlation, "causationId": null, "payload": payload })
}

/// Records `packet` as the task's latest Task Packet.
fn record_packet(daemon: &Rc<NoshDaemon>, project_id: &str, packet: &Value) {
    let mut packet = packet.clone();
    packet["$schema"] = json!(schema_uri("task-packet"));
    let correlation = json!(format!("task:{}", packet["taskId"].as_str().unwrap()));
    daemon
        .append_draft(&event("record.submitted", project_id, correlation, packet))
        .unwrap();
}

fn task_context(project_id: &str, task_id: &str, agent_id: &str) -> TerminalContext {
    TerminalContext {
        project_id: project_id.into(),
        agent_id: agent_id.into(),
        task_id: Some(task_id.into()),
        instruction_id: None,
        thread_id: None,
        expected_version: None,
        turn_id: format!("task:{task_id}"),
        allowed_tools: Vec::new(),
        expected_episode_type: None,
    }
}

/// An initialized Project whose approved contract (version 3) declares `commands`.
fn project_with_commands(
    daemon: &Rc<NoshDaemon>,
    root: &Path,
    title: &str,
    commands: Value,
    key: &str,
) -> Value {
    let project = daemon
        .initialize_project(root, true, Some(title), None)
        .unwrap();
    let repository = Path::new(project["repositoryRoot"].as_str().unwrap()).to_path_buf();
    let mut draft = read_project_contract(&repository).unwrap();
    draft["contractVersion"] = json!(2);
    let mut approved = approve_project_contract(&repository, &draft).unwrap();
    if !commands.is_null() {
        approved["contractVersion"] = json!(3);
        approved["execution"] = json!({ "runner": "native", "commands": commands });
        daemon
            .amend_project_contract(project["projectId"].as_str().unwrap(), &approved, key)
            .unwrap();
    }
    project
}

async fn run(
    daemon: &Rc<NoshDaemon>,
    project_id: &str,
    task_id: &str,
    record: Value,
    agent_id: &str,
    tool: &str,
) -> Value {
    daemon
        .submit_tool(
            tool,
            project_id,
            &format!("task:{task_id}"),
            &record,
            Some(agent_id),
        )
        .await
        .unwrap()
}

fn criterion(id: &str, claim: Option<&str>, runs: &[&str]) -> CitedCriterion {
    CitedCriterion {
        criterion_id: id.into(),
        worker_claim: claim.map(str::to_owned),
        validator_run_ids: runs.iter().map(|run| (*run).to_owned()).collect(),
    }
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

#[test]
fn runs_only_contract_declared_commands_commits_worker_edits_itself_and_fills_completion_facts_without_model_tool_calls()
 {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let project = project_with_commands(
            &daemon,
            &directory.path().join("repository"),
            "Run fixture",
            Value::Null,
            "",
        );
        let project_id = project["projectId"].as_str().unwrap().to_owned();
        let root = Path::new(project["repositoryRoot"].as_str().unwrap()).to_path_buf();
        let mut approved = read_project_contract(&root).unwrap();
        // Before amendment no command is declared, so nothing can run.
        let (task_id, agent_id) = (create_id(IdPrefix::Tsk), create_id(IdPrefix::Agt));
        std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
        commit_all(&root, "README.md", "initial");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let tree = create_task_worktree(&daemon.research, &project, &task_id, &head, None).unwrap();
        agents.sessions.borrow_mut().push(inspection(
            &project_id,
            &agent_id,
            Some(&task_id),
            Role::GeneralWorker,
        ));
        let mut packet = json!({ "taskId": task_id, "permissions": { "subprocess": "allowlisted", "allowedToolIds": ["tool_nosh.run"] }, "workspace": { "worktreeId": tree.worktree_id } });
        record_packet(&daemon, &project_id, &packet);
        let check = json!({ "commandId": "command_check" });
        let refused = run(
            &daemon,
            &project_id,
            &task_id,
            check.clone(),
            &agent_id,
            "nosh_run",
        )
        .await;
        assert_match(&refused, &json!({ "accepted": false }));
        assert!(
            text(&refused["error"]).contains("declares: none"),
            "{refused}"
        );

        approved["contractVersion"] = json!(3);
        approved["execution"] = json!({ "runner": "native", "commands": [
            { "commandId": "command_check", "description": "Exit with the code in exit.txt", "argv": ["node", "-e", "const c=Number(require('fs').readFileSync('exit.txt','utf8'));console.log('code '+c);process.exit(c)"], "timeoutSeconds": 30 },
        ] });
        daemon
            .amend_project_contract(&project_id, &approved, "amend-0001")
            .unwrap();
        // An edit-test iteration: the first run fails, the worker fixes it. Only the final state counts as the outcome.
        std::fs::write(tree.path.join("exit.txt"), "1").unwrap();
        assert_match(
            &run(
                &daemon,
                &project_id,
                &task_id,
                check.clone(),
                &agent_id,
                "nosh_run",
            )
            .await,
            &json!({ "accepted": true, "exitCode": 1 }),
        );
        std::fs::write(tree.path.join("exit.txt"), "0").unwrap();
        let dirty = run(
            &daemon,
            &project_id,
            &task_id,
            check.clone(),
            &agent_id,
            "nosh_run",
        )
        .await;
        assert_match(
            &dirty,
            &json!({ "accepted": true, "exitCode": 0, "dirty": true, "citable": false, "commit": head }),
        );
        assert!(text(&dirty["stdoutTail"]).contains("code 0"));

        // With commit authority, the daemon commits the worker's edits before each run, so every
        // run is tied to an exact clean commit.
        packet["attempt"] = json!(1);
        packet["permissions"] = json!({ "network": "disabled", "subprocess": "allowlisted", "gitCommit": true, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": ["tool_nosh.run", "tool_nosh.git.commit", "tool_pi.read", "tool_pi.write"] });
        packet["workspace"] = json!({ "worktreeId": tree.worktree_id, "branch": tree.branch, "startingCommit": head, "writeScopes": ["**"], "protectedScopes": [".nosh/contracts/**"] });
        record_packet(&daemon, &project_id, &packet);
        let clean = run(
            &daemon,
            &project_id,
            &task_id,
            check.clone(),
            &agent_id,
            "nosh_run",
        )
        .await;
        let ending = git(&tree.path, &["rev-parse", "HEAD"]);
        assert_ne!(ending, head);
        assert_match(
            &clean,
            &json!({ "accepted": true, "exitCode": 0, "dirty": false, "citable": true, "commit": ending }),
        );
        assert_eq!(
            git(&tree.path, &["log", "-1", "--format=%an|%s"]),
            format!("NOSH general_worker|nosh: {task_id} edits before command_check")
        );
        // The same command on the same clean commit answers from the record instead of launching another Job.
        let started = Instant::now();
        let rerun = run(
            &daemon,
            &project_id,
            &task_id,
            check.clone(),
            &agent_id,
            "nosh_run",
        )
        .await;
        assert_match(
            &rerun,
            &json!({ "accepted": true, "cached": true, "jobId": clean["jobId"], "exitCode": 0, "citable": true, "commit": ending }),
        );
        assert!(started.elapsed().as_millis() < 1_000);

        let runs: Vec<Value> = daemon
            .replay(&project_id, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event["type"] == "task.command_run")
            .map(|event| event["payload"].clone())
            .collect();
        assert_eq!(runs.len(), 3);
        let (clean_job, dirty_job) = (text(&clean["jobId"]), text(&dirty["jobId"]));
        assert!(
            cited_run_issues(
                &runs,
                &task_id,
                &ending,
                &[criterion("criterion_1", None, &[&clean_job])],
                &[],
                None
            )
            .is_empty()
        );
        assert!(
            cited_run_issues(
                &runs,
                &task_id,
                &ending,
                &[criterion("criterion_1", None, &[&dirty_job])],
                &[],
                None
            )[0]
            .contains("clean ending commit")
        );
        assert!(
            cited_run_issues(
                &runs,
                &create_id(IdPrefix::Tsk),
                &ending,
                &[criterion("criterion_1", None, &[&clean_job])],
                &[],
                None
            )[0]
            .contains("not a nosh_run of this task")
        );
        // A Job the daemon itself ran for the task (a Direction baseline evaluation) is an authoritative validator.
        assert!(
            cited_run_issues(
                &runs,
                &task_id,
                &ending,
                &[criterion("criterion_1", None, &["job_daemonbaseline"])],
                &["job_daemonbaseline".into()],
                None
            )
            .is_empty()
        );
        assert!(
            cited_run_issues(
                &runs,
                &task_id,
                &ending,
                &[criterion("criterion_1", None, &["job_daemonbaseline"])],
                &[],
                None
            )[0]
            .contains("not a nosh_run of this task")
        );

        // commands[] is daemon-owned: whatever shape the model wrote, the completion carries each command's latest run.
        let context = task_context(&project_id, &task_id, &agent_id);
        let normalized = daemon.with_daemon_commands(&context, json!({ "$schema": schema_uri("general-worker-completion"), "commands": [{ "commandId": "command_check", "jobId": clean["jobId"] }] })).unwrap();
        assert_match(
            &normalized["commands"],
            &json!([{ "commandId": "command_check", "exitCode": 0, "resultArtifactId": null }]),
        );
        assert!(text(&normalized["commands"][0]["displayCommand"]).contains("exit.txt"));

        // Final answer: the daemon commits the remaining edits, fills codeChanges and changedFiles,
        // and snapshots "artifact:<path>" citations.
        std::fs::write(tree.path.join("report.md"), "# report\n").unwrap();
        std::fs::write(tree.path.join("notes.md"), "notes\n").unwrap();
        let with_facts = |records: Vec<Value>| daemon.with_daemon_facts(&context, records);
        let submitted = vec![
            json!({ "$schema": schema_uri("general-worker-completion"), "workPerformed": [{ "subject": "artifact: prose with spaces is untouched", "artifactIds": ["artifact:report.md"] }], "codeChanges": { "startingCommit": head, "endingCommit": head, "changedPaths": [], "diffArtifactId": null, "branch": tree.branch } }),
            json!({ "$schema": schema_uri("episode-draft"), "artifactIds": ["artifact:report.md"], "changedFiles": [] }),
        ];
        let filled = with_facts(submitted.clone()).unwrap();
        let (completion, episode) = (&filled[0], &filled[1]);
        let last = git(&tree.path, &["rev-parse", "HEAD"]);
        assert_ne!(last, ending);
        assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
        let changes = &completion["codeChanges"];
        assert_match(
            changes,
            &json!({ "startingCommit": head, "endingCommit": last, "branch": tree.branch }),
        );
        assert_eq!(changes.as_object().unwrap().len(), 5);
        let mut changed: Vec<String> = changes["changedPaths"]
            .as_array()
            .unwrap()
            .iter()
            .map(text)
            .collect();
        changed.sort();
        assert_eq!(changed, ["exit.txt", "notes.md", "report.md"]);
        let art = regex::Regex::new(r"^art_[0-9a-f]{32}$").unwrap();
        assert!(art.is_match(&text(&changes["diffArtifactId"])));
        // Each nosh_run is a daemon-produced Artifact cited by commands[] and by the criteria that
        // name its Job. Cited runs are checked when the worker answers, so a mis-claimed criterion
        // is correctable in-session: `clean` ran before the final commit.
        let claimed = |claim: &str, runs: &[&str]| {
            with_facts(vec![
                json!({ "$schema": schema_uri("general-worker-completion"), "commands": [], "criteria": [{ "criterionId": "criterion_1", "workerClaim": claim, "validatorRunIds": runs, "artifactIds": [] }] }),
            ])
        };
        let mis_claimed = claimed("satisfied", &[&clean_job]).unwrap_err().to_string();
        assert!(regex::Regex::new(r"^Cited runs: criterion criterion_1 cites job_\w+, which did not run on the clean ending commit").unwrap().is_match(&mis_claimed), "{mis_claimed}");
        claimed("deferred_to_review", &[&clean_job]).unwrap();
        // A job the daemon ran itself (a Direction baseline) is left to postflight, which knows those jobs.
        claimed("satisfied", &["job_daemonbaseline"]).unwrap();
        // Deliberately deferred: the cited run predates the final commit (comparison-style citation).
        let with_runs = claimed("deferred_to_review", &[&clean_job])
            .unwrap()
            .remove(0);
        let run_artifact = with_runs["commands"][0]["resultArtifactId"].clone();
        assert_eq!(
            with_runs["criteria"][0]["artifactIds"],
            json!([run_artifact])
        );
        let read = |artifact: Value| {
            run(
                &daemon,
                &project_id,
                &task_id,
                json!({ "artifactId": artifact }),
                &agent_id,
                "nosh_artifact_read",
            )
        };
        let run_read = read(run_artifact).await;
        assert_match(
            &run_read,
            &json!({ "accepted": true, "kind": "artifact_run-result" }),
        );
        assert!(text(&run_read["content"]).contains(&clean_job));
        // The exact change is a daemon-produced Artifact, so reviewers judge the code itself.
        let diff_read = read(changes["diffArtifactId"].clone()).await;
        assert_match(
            &diff_read,
            &json!({ "accepted": true, "kind": "artifact_diff" }),
        );
        assert!(text(&diff_read["content"]).contains("+notes"));
        assert_eq!(episode["changedFiles"], changes["changedPaths"]);
        let artifact_id = text(&completion["workPerformed"][0]["artifactIds"][0]);
        assert!(art.is_match(&artifact_id));
        assert_eq!(episode["artifactIds"], json!([artifact_id]));
        assert_eq!(
            completion["workPerformed"][0]["subject"],
            "artifact: prose with spaces is untouched"
        );
        let artifacts = daemon
            .research
            .records(&project_id, Some("artifact"))
            .unwrap();
        let recorded = artifacts
            .iter()
            .find(|entry| entry["artifactId"] == artifact_id.as_str())
            .unwrap();
        assert_match(
            recorded,
            &json!({ "kind": "artifact_file", "producer": { "type": "agent", "agentId": agent_id, "taskId": task_id, "path": "report.md" } }),
        );
        assert_match(
            &read(json!(artifact_id)).await,
            &json!({ "accepted": true, "kind": "artifact_file", "content": "# report\n", "remainingCharacters": 0 }),
        );
        assert_match(
            &read(json!("art_missing")).await,
            &json!({ "accepted": false }),
        );
        // Review Requests cite Evidence by evd_ id; the reader returns the record so a reviewer
        // can follow it to its Artifacts.
        let evidence = daemon.research.submit_user_evidence(&project_id, &json!({ "evidenceType": "evidence_observation", "statement": "The report exists.", "polarity": "supports", "sourceRefs": [{ "refType": "ref_artifact", "refId": artifact_id, "locator": "report.md" }], "evaluationContractHash": null, "scopeLimitations": [], "quality": { "status": "unreviewed", "reviewId": null, "confidence": "medium" } }), "evidence-read").unwrap();
        let evidence_id = text(&evidence["payload"]["evidenceId"]);
        let evidence_read = read(json!(evidence_id)).await;
        assert_match(
            &evidence_read,
            &json!({ "accepted": true, "kind": "evidence" }),
        );
        assert!(text(&evidence_read["content"]).contains(&artifact_id));
        assert_match(
            &read(json!("evd_0123abcd")).await,
            &json!({ "accepted": false }),
        );
        // Candidate Evidence frozen for an open Review is readable until the Review ends.
        daemon.research.offer_candidate_evidence(
            &project_id,
            json!({ "evidenceId": "evd_0123abcd", "statement": "candidate" }),
        );
        let candidate = read(json!("evd_0123abcd")).await;
        assert_match(
            &candidate,
            &json!({ "accepted": true, "kind": "evidence_candidate" }),
        );
        assert!(text(&candidate["content"]).contains("candidate"));
        daemon
            .research
            .withdraw_candidate_evidence(&project_id, "evd_0123abcd");
        assert_match(
            &read(json!("evd_0123abcd")).await,
            &json!({ "accepted": false }),
        );
        // A corrected or replayed answer is idempotent: same Artifact, no new commit.
        let again = with_facts(submitted).unwrap().remove(0);
        assert_eq!(
            again["workPerformed"][0]["artifactIds"],
            json!([artifact_id])
        );
        assert_eq!(again["codeChanges"]["endingCommit"], json!(last));
        // One file cited under different fields (report field and episode) is one Artifact with the most specific kind.
        let librarian = with_facts(vec![json!({ "$schema": schema_uri("librarian-completion"), "reportArtifactId": "artifact:report.md" }), json!({ "$schema": schema_uri("episode-draft"), "artifactIds": ["artifact:report.md"] })]).unwrap();
        assert_eq!(
            librarian[1]["artifactIds"],
            json!([librarian[0]["reportArtifactId"]])
        );
        let artifacts = daemon
            .research
            .records(&project_id, Some("artifact"))
            .unwrap();
        assert_match(
            artifacts
                .iter()
                .find(|entry| entry["artifactId"] == librarian[0]["reportArtifactId"])
                .unwrap(),
            &json!({ "kind": "artifact_report" }),
        );
        // A source's canonical URL is filled when derivable: a cited worktree file, or a DOI.
        let sourced = with_facts(vec![json!({ "$schema": schema_uri("librarian-completion"), "sources": [{ "sourceId": "source_1", "artifactId": "artifact:report.md" }, { "sourceId": "source_2", "persistentId": "doi:10.1109/ICPR.2016.7900006" }, { "sourceId": "source_3", "canonicalUrl": "https://example.org/kept" }] })]).unwrap().remove(0);
        let urls: Vec<Value> = sourced["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["canonicalUrl"].clone())
            .collect();
        assert_eq!(
            urls,
            [
                json!("file:///report.md"),
                json!("https://doi.org/10.1109/ICPR.2016.7900006"),
                json!("https://example.org/kept")
            ]
        );
        // A source may cite recorded Evidence: it becomes a readable snapshot Artifact of that record.
        let from_evidence = with_facts(vec![json!({ "$schema": schema_uri("librarian-completion"), "sources": [{ "sourceId": "source_4", "artifactId": evidence_id }] })]).unwrap().remove(0);
        let source = &from_evidence["sources"][0];
        assert!(art.is_match(&text(&source["artifactId"])));
        assert_eq!(
            source["canonicalUrl"],
            json!(format!("nosh://evidence/{evidence_id}"))
        );
        assert!(text(&read(source["artifactId"].clone()).await["content"]).contains(&evidence_id));
        let unknown = with_facts(vec![json!({ "$schema": schema_uri("librarian-completion"), "sources": [{ "sourceId": "source_5", "artifactId": "evd_0123abcd" }] })]).unwrap_err();
        assert!(
            unknown.to_string().contains("not recorded Evidence"),
            "{unknown}"
        );
        // Citations must name files inside the worktree; the reason is correctable by the model.
        for (path, reason) in [
            ("missing.md", "names no file"),
            ("../outside.md", "inside your worktree"),
            (".git/HEAD", "inside your worktree"),
        ] {
            let error = with_facts(vec![json!({ "$schema": schema_uri("general-worker-completion"), "workPerformed": [{ "artifactIds": [format!("artifact:{path}")] }] })]).unwrap_err();
            assert!(error.to_string().contains(reason), "{path}: {error}");
        }
        // Rust-only: a hard-linked file could be rewritten through its other name after it is cited.
        std::fs::write(
            tree.path.join("linked.md"),
            "# linked
",
        )
        .unwrap();
        std::fs::hard_link(tree.path.join("linked.md"), tree.path.join("alias.md")).unwrap();
        let linked = with_facts(vec![json!({ "$schema": schema_uri("general-worker-completion"), "workPerformed": [{ "artifactIds": ["artifact:alias.md"] }] })]).unwrap_err();
        assert!(
            linked
                .to_string()
                .contains("must be a regular, unlinked file"),
            "{linked}"
        );

        // Progress is a text line the daemon turns into a schema-valid progress-update (no model
        // tool call). TypeScript stubs submitRecord here; the record's fields are the facts.rs unit
        // test, and this fixture's three packets stop it at task authority, which runs only after
        // the submission gate accepted it as schema-valid.
        let noted = run(
            &daemon,
            &project_id,
            &task_id,
            json!({ "note": "tests pass -> write the report" }),
            &agent_id,
            "nosh_progress_note",
        )
        .await;
        assert_match(
            &noted,
            &json!({ "accepted": false, "errors": [{ "code": "domain_policy", "message": "Task authority requires exactly one matching durable Task Packet" }] }),
        );
        let spoofed = daemon
            .submit_tool(
                "nosh_progress_note",
                &project_id,
                &format!("task:{}", create_id(IdPrefix::Tsk)),
                &json!({ "note": "spoofed" }),
                Some(&agent_id),
            )
            .await
            .unwrap();
        assert_match(&spoofed, &json!({ "accepted": false }));

        // A session whose packet lacks subprocess authority cannot run anything.
        packet["permissions"]["subprocess"] = json!("disabled");
        record_packet(&daemon, &project_id, &packet);
        assert_match(
            &run(&daemon, &project_id, &task_id, check, &agent_id, "nosh_run").await,
            &json!({ "accepted": false, "error": "This task is not authorized to run commands" }),
        );
        daemon.stop();
    });
}

/// Rust-only: TypeScript stubs `submitRecord` for progress notes, so this follows each note to its
/// durable record, for a worker holding exactly one live leased Task Packet.
#[test]
fn records_each_progress_note_as_the_next_numbered_progress_update_of_the_attempt() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let worker = leased_worker(&daemon, &agents, directory.path());
        for note in [
            "tests pass -> write the report",
            "report drafted -> cite the runs",
        ] {
            let noted = run(
                &daemon,
                &worker.project_id,
                &worker.task_id,
                json!({ "note": note }),
                &worker.agent_id,
                "nosh_progress_note",
            )
            .await;
            assert_match(&noted, &json!({ "accepted": true }));
        }
        let progress: Vec<Value> = daemon
            .research
            .submitted(
                &worker.project_id,
                Some(&format!("task:{}", worker.task_id)),
            )
            .unwrap()
            .into_iter()
            .map(|entry| entry["payload"].clone())
            .filter(|record| record["$schema"] == schema_uri("progress-update").as_str())
            .collect();
        let suffix = &worker.task_id[4..];
        assert_match(
            &json!(progress),
            &json!([
                { "progressId": format!("progress_{suffix}_1"), "taskId": worker.task_id, "agentId": worker.agent_id, "attempt": 1, "summary": "tests pass", "nextOperation": "write the report" },
                { "progressId": format!("progress_{suffix}_2"), "summary": "report drafted", "nextOperation": "cite the runs" },
            ]),
        );
        let recorded = daemon
            .replay(&worker.project_id, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event["type"] == "task.progress_recorded")
            .count();
        assert_eq!(recorded, 2);
        daemon.stop();
    });
}

#[test]
fn refuses_an_experiment_worker_any_variant_of_the_frozen_evaluation_program_and_allows_its_other_commands()
 {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let commands = json!([
            { "commandId": "command_check", "description": "Unit tests", "argv": ["node", "-e", "process.exit(0)"], "timeoutSeconds": 30 },
            { "commandId": "command_evaluate-test", "description": "Held-out test evaluation", "argv": ["node", "evaluate.mjs", "--split=test"], "timeoutSeconds": 30 },
        ]);
        let project = project_with_commands(
            &daemon,
            &directory.path().join("repository"),
            "Experiment run fixture",
            commands,
            "amend-experiment-0001",
        );
        let project_id = project["projectId"].as_str().unwrap().to_owned();
        let root = Path::new(project["repositoryRoot"].as_str().unwrap()).to_path_buf();
        std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
        commit_all(&root, "README.md", "initial");
        let (task_id, agent_id, autoresearch_id) = (
            create_id(IdPrefix::Tsk),
            create_id(IdPrefix::Agt),
            create_id(IdPrefix::Ar),
        );
        let tree = create_task_worktree(
            &daemon.research,
            &project,
            &task_id,
            &git(&root, &["rev-parse", "HEAD"]),
            None,
        )
        .unwrap();
        let mut session = inspection(&project_id, &agent_id, Some(&task_id), Role::GeneralWorker);
        session.autoresearch_id = Some(autoresearch_id.clone());
        session.experiment_id = Some(create_id(IdPrefix::Exp));
        agents.sessions.borrow_mut().push(session);
        record_packet(
            &daemon,
            &project_id,
            &json!({ "taskId": task_id, "taskType": "task_experiment.implementation", "permissions": { "subprocess": "allowlisted", "allowedToolIds": ["tool_nosh.run"] }, "workspace": { "worktreeId": tree.worktree_id } }),
        );
        daemon
            .store_for(&project_id)
            .unwrap()
            .mutate_projection(
                "autoresearch-fixture",
                Some(0),
                &event("autoresearch.created", &project_id, Value::Null, json!({ "entityId": autoresearch_id })),
                &ProjectionInput {
                    entity_type: "autoresearch".into(),
                    entity_id: autoresearch_id.clone(),
                    state: "running".into(),
                    value: json!({ "evaluationContract": { "execution": { "command": ["node", "evaluate.mjs", "--split=dev"] } } }),
                    graph: None,
                    records: Vec::new(),
                    intent: None,
                },
            )
            .unwrap();
        // The daemon evaluates every variant with the frozen command; a worker running it (or its
        // held-out split) is refused.
        let refused = run(
            &daemon,
            &project_id,
            &task_id,
            json!({ "commandId": "command_evaluate-test" }),
            &agent_id,
            "nosh_run",
        )
        .await;
        assert_match(&refused, &json!({ "accepted": false }));
        assert!(
            text(&refused["error"]).contains("frozen evaluation"),
            "{refused}"
        );
        assert_match(
            &run(
                &daemon,
                &project_id,
                &task_id,
                json!({ "commandId": "command_check" }),
                &agent_id,
                "nosh_run",
            )
            .await,
            &json!({ "accepted": true, "exitCode": 0 }),
        );
        daemon.stop();
    });
}

#[test]
fn shows_the_worker_which_tests_failed_when_the_failure_scrolled_out_of_the_output_tail() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let commands = json!([{ "commandId": "command_test", "description": "Unit tests", "argv": ["node", "tap.cjs"], "timeoutSeconds": 30 }]);
        let project = project_with_commands(
            &daemon,
            &directory.path().join("repository"),
            "TAP fixture",
            commands,
            "amend-tap-0001",
        );
        let project_id = project["projectId"].as_str().unwrap().to_owned();
        let root = Path::new(project["repositoryRoot"].as_str().unwrap()).to_path_buf();
        // node --test writes TAP when not on a terminal: each failure where it happens, only the counts at the end.
        let mut tap: Vec<String> = [
            "TAP version 13",
            "# Subtest: parses",
            "ok 1 - parses",
            "  ---",
            "  duration_ms: 0.1",
            "  ...",
            "# Subtest: calibrates",
            "not ok 2 - calibrates",
            "  ---",
            "  error: |-",
            "    Expected values to be strictly equal:",
            "    0.3 !== 0.25",
            "  code: 'ERR_ASSERTION'",
            "  ...",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        tap.extend((0..60).map(|index| {
            format!(
                "# Subtest: passes {n}\nok {n} - passes {n}\n  ---\n  duration_ms: 0.2\n  ...",
                n = index + 3
            )
        }));
        tap.extend([
            "1..62".to_owned(),
            "# pass 61".to_owned(),
            "# fail 1".to_owned(),
        ]);
        std::fs::write(
            root.join("tap.cjs"),
            format!("console.log({}); process.exit(1);\n", json!(tap.join("\n"))),
        )
        .unwrap();
        commit_all(&root, "tap.cjs", "initial");
        let (task_id, agent_id) = (create_id(IdPrefix::Tsk), create_id(IdPrefix::Agt));
        let tree = create_task_worktree(
            &daemon.research,
            &project,
            &task_id,
            &git(&root, &["rev-parse", "HEAD"]),
            None,
        )
        .unwrap();
        agents.sessions.borrow_mut().push(inspection(
            &project_id,
            &agent_id,
            Some(&task_id),
            Role::GeneralWorker,
        ));
        record_packet(
            &daemon,
            &project_id,
            &json!({ "taskId": task_id, "permissions": { "subprocess": "allowlisted", "allowedToolIds": ["tool_nosh.run"] }, "workspace": { "worktreeId": tree.worktree_id } }),
        );
        let first = run(
            &daemon,
            &project_id,
            &task_id,
            json!({ "commandId": "command_test" }),
            &agent_id,
            "nosh_run",
        )
        .await;
        assert_match(&first, &json!({ "accepted": true, "exitCode": 1 }));
        assert!(!text(&first["stdoutTail"]).contains("not ok"));
        let failed = text(&first["failedTests"]);
        assert!(
            failed.contains("not ok 2 - calibrates")
                && failed.contains("0.3 !== 0.25")
                && !failed.contains("passes"),
            "{failed}"
        );
        // Re-running on the unchanged commit answers from the record, failures included.
        let again = run(
            &daemon,
            &project_id,
            &task_id,
            json!({ "commandId": "command_test" }),
            &agent_id,
            "nosh_run",
        )
        .await;
        assert_match(
            &again,
            &json!({ "cached": true, "jobId": first["jobId"], "failedTests": first["failedTests"] }),
        );
        daemon.stop();
    });
}

#[test]
fn keeps_a_run_valid_when_only_the_paper_write_up_changed_since_it_and_not_after_any_other_change()
{
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    let git = |args: &[&str]| {
        let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
        all.extend_from_slice(args);
        git(root, &all)
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("a.txt"), "a").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    let run_commit = git(&["rev-parse", "HEAD"]);
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs").join("paper.md"), "# results").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "paper"]);
    let paper = git(&["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "changed").unwrap();
    git(&["commit", "-qam", "code"]);
    let code = git(&["rev-parse", "HEAD"]);
    let runs = [
        json!({ "taskId": "tsk_1", "jobId": "job_1", "commit": run_commit, "dirty": false, "state": "completed", "exitCode": 0 }),
    ];
    let cited = [criterion("criterion_1", None, &["job_1"])];
    let write_up = write_up_only(root);
    assert!(cited_run_issues(&runs, "tsk_1", &paper, &cited, &[], Some(&write_up)).is_empty());
    assert!(
        cited_run_issues(&runs, "tsk_1", &code, &cited, &[], Some(&write_up))[0]
            .contains("only the paper write-up")
    );
    assert!(
        cited_run_issues(&runs, "tsk_1", &paper, &cited, &[], None)[0]
            .contains("did not run on the clean ending commit")
    );
}

#[test]
fn lets_a_criterion_deferred_to_review_cite_this_tasks_clean_passing_runs_on_earlier_commits_and_nothing_else()
 {
    let runs = [
        json!({ "taskId": "tsk_1", "jobId": "job_a", "commit": "commit_a", "dirty": false, "state": "completed", "exitCode": 0 }),
        json!({ "taskId": "tsk_1", "jobId": "job_b", "commit": "commit_b", "dirty": false, "state": "completed", "exitCode": 0 }),
        json!({ "taskId": "tsk_1", "jobId": "job_failed", "commit": "commit_b", "dirty": false, "state": "failed", "exitCode": 1 }),
        json!({ "taskId": "tsk_1", "jobId": "job_dirty", "commit": "commit_b", "dirty": true, "state": "completed", "exitCode": 0 }),
        json!({ "taskId": "tsk_2", "jobId": "job_other", "commit": "commit_b", "dirty": false, "state": "completed", "exitCode": 0 }),
    ];
    let claim = |claim: &str, runs: &[&str]| [criterion("criterion_2", Some(claim), runs)];
    assert!(
        cited_run_issues(
            &runs,
            "tsk_1",
            "commit_final",
            &claim("deferred_to_review", &["job_a", "job_b"]),
            &[],
            None
        )
        .is_empty()
    );
    let completion = json!({ "codeChanges": { "endingCommit": "commit_final" }, "criteria": [{ "criterionId": "criterion_2", "workerClaim": "deferred_to_review", "validatorRunIds": ["job_a", "job_b"] }] });
    assert!(completion_run_issues(&runs, "tsk_1", &completion, &[], None).is_empty());
    // A claim about the final code still needs a run on the final code.
    assert!(
        cited_run_issues(
            &runs,
            "tsk_1",
            "commit_final",
            &claim("satisfied", &["job_a"]),
            &[],
            None
        )[0]
        .contains("did not run on the clean ending commit")
    );
    assert!(
        cited_run_issues(
            &runs,
            "tsk_1",
            "commit_final",
            &claim("deferred_to_review", &["job_failed"]),
            &[],
            None
        )[0]
        .contains("did not pass")
    );
    assert!(
        cited_run_issues(
            &runs,
            "tsk_1",
            "commit_final",
            &claim("deferred_to_review", &["job_dirty"]),
            &[],
            None
        )[0]
        .contains("uncommitted edits")
    );
    assert!(
        cited_run_issues(
            &runs,
            "tsk_1",
            "commit_final",
            &claim("deferred_to_review", &["job_other"]),
            &[],
            None
        )[0]
        .contains("not a nosh_run of this task")
    );
}

#[test]
fn tells_mission_and_direction_workers_how_to_evidence_a_comparison_within_the_packets_per_instruction_limit_and_not_autoresearch_implementers()
 {
    // The live spam-calibration contract's commands.
    let commands: Vec<Value> = [
        ("test", "node --test", "package.json script \"test\""),
        ("evaluate", "node evaluate.mjs --split=dev", "package.json script \"evaluate\""),
        ("evaluate-shift", "node evaluate.mjs --split=dev --shift=0.3", "package.json script \"evaluate:shift\""),
        ("evaluate-cv", "node evaluate.mjs --folds=5 --bootstrap=1000", "package.json script \"evaluate:cv\""),
        ("evaluate-test", "node evaluate.mjs --split=test", "Held-out test evaluation"),
        ("evaluate-test-shift", "node evaluate.mjs --split=test --shift=0.3", "Held-out test evaluation at 0.3 spam prevalence"),
    ]
    .iter()
    .map(|(id, argv, description)| json!({ "commandId": format!("command_{id}"), "description": description, "argv": argv.split(' ').collect::<Vec<_>>(), "timeoutSeconds": 600 }))
    .collect();
    let with_comparisons = task_command_instructions(&commands, true);
    assert_eq!(with_comparisons.len(), 2);
    assert!(with_comparisons[1].contains("deferred_to_review"));
    assert!(
        with_comparisons
            .iter()
            .all(|line| line.encode_utf16().count() <= 1_000)
    );
    // Experiment implementers do not compare methods (later rounds do), and their postflight accepts only satisfied claims.
    assert!(
        !task_command_instructions(&commands, false)
            .join(" ")
            .contains("deferred_to_review")
    );
}

fn at(kind: &str, payload: Value) -> Value {
    json!({ "type": kind, "payload": payload })
}

#[test]
fn reaches_the_next_worker_as_one_bounded_instruction_the_nodes_last_three_failures_newest_first_without_the_reviewers_own_errors()
 {
    let events = [
        at(
            "task.postflight_failed",
            json!({ "taskId": "tsk_1", "nodeId": "mnode_a", "issues": ["oldest issue"] }),
        ),
        at(
            "task.postflight_failed",
            json!({ "taskId": "tsk_2", "nodeId": "mnode_a", "issues": ["criterion criterion_1 cites job_x, which did not run on the clean ending commit", "x".repeat(2_000)] }),
        ),
        at(
            "task.postflight_failed",
            json!({ "taskId": "tsk_9", "nodeId": "mnode_b", "issues": ["another node's issue"] }),
        ),
        at(
            "task.review_failed",
            json!({ "taskId": "tsk_3", "nodeId": "mnode_a", "message": "Terminal output rejected: Episode references unknown record art_1" }),
        ),
        at(
            "record.submitted",
            json!({ "$schema": schema_uri("review-verdict"), "verdict": "REVISE", "summary": "The no-calibration baseline was never run.", "target": { "targetType": "graph_node", "targetId": "mnode_a", "targetVersion": 1 }, "criteria": [{ "criterionId": "criterion_1", "status": "PASS", "finding": "fine" }, { "criterionId": "criterion_2", "status": "FAIL", "finding": "only histogram binning was measured at 30%" }] }),
        ),
    ];
    let instructions = prior_attempt_instructions(&events, "mnode_a");
    assert_eq!(instructions.len(), 1);
    let instruction = &instructions[0];
    assert!(instruction.encode_utf16().count() <= 1_000);
    assert!(instruction.starts_with("Earlier attempts at this node failed"));
    let position = |needle: &str| {
        instruction
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing from {instruction}"))
    };
    assert!(position("REVISE") < position("did not complete"));
    assert!(position("did not complete") < position("clean ending commit"));
    assert!(instruction.contains("criterion_2 FAIL: only histogram binning was measured at 30%"));
    assert!(!instruction.contains("criterion_1 PASS"));
    for absent in ["oldest issue", "another node's issue", "art_1"] {
        assert!(!instruction.contains(absent), "{absent}");
    }
}

#[test]
fn is_empty_for_a_first_attempt_and_reads_the_direction_supervisors_failure_events() {
    assert!(prior_attempt_instructions(&[], "dnode_a").is_empty());
    let instructions = prior_attempt_instructions(
        &[
            at(
                "direction.postflight_failed",
                json!({ "taskId": "tsk_1", "nodeId": "dnode_a", "issues": ["a declared command did not pass"] }),
            ),
            at(
                "direction.task_failed",
                json!({ "taskId": "tsk_2", "nodeId": "dnode_a", "message": "Task preflight requires a clean checked-out Git branch" }),
            ),
        ],
        "dnode_a",
    );
    assert!(instructions[0].contains("a declared command did not pass"));
    assert!(instructions[0].contains("Task preflight requires a clean checked-out Git branch"));
}

#[test]
fn resumes_only_from_a_commit_that_still_contains_the_head_and_says_why_it_could_not() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        std::fs::create_dir(&root).unwrap();
        let git = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=T", "-c", "user.email=t@example.invalid"];
            all.extend_from_slice(args);
            git(&root, &all)
        };
        let commit = |name: &str| {
            std::fs::write(root.join(name), format!("{name}\n")).unwrap();
            git(&["add", name]);
            git(&["commit", "-m", name]);
            git(&["rev-parse", "HEAD"])
        };
        git(&["init", "-b", "main"]);
        let head = commit("base.txt");
        git(&["checkout", "-b", "attempt"]);
        let attempt = commit("attempt.txt");
        git(&["checkout", "main"]);
        let moved = commit("other.txt");
        // The research control reads each attempt's completion; one task per ending commit.
        let daemon = daemon(
            &directory.path().join("data"),
            Rc::new(FakeAgents::default()),
        );
        let project_id = create_id(IdPrefix::Prj);
        daemon
            .register_project(&project_id, &root, &directory.path().join("project.sqlite"))
            .unwrap();
        let project = json!({ "projectId": project_id, "repositoryRoot": root.to_string_lossy() });
        let attempted = |ending: &str| {
            let task_id = create_id(IdPrefix::Tsk);
            let completion = json!({ "$schema": schema_uri("general-worker-completion"), "codeChanges": { "endingCommit": ending } });
            let draft = event(
                "record.submitted",
                &project_id,
                json!(format!("task:{task_id}")),
                completion.clone(),
            );
            daemon
                .store_for(&project_id)
                .unwrap()
                .append_terminal_submission(
                    &task_id,
                    "nosh_response_submit",
                    &schema_uri("general-worker-completion"),
                    &completion,
                    &draft,
                )
                .unwrap();
            let passed = vec![
                at(
                    "mission.node_state_changed",
                    json!({ "nodeId": "mnode_a", "nodeState": "leased", "leaseId": task_id }),
                ),
                at(
                    "mission.node_state_changed",
                    json!({ "nodeId": "mnode_a", "nodeState": "postflight", "leaseId": task_id }),
                ),
            ];
            (task_id, passed)
        };
        let point = |events: &[Value], head: &str| {
            resume_point(&daemon.research, &project, events, "mnode_a", head).unwrap()
        };
        let (task, passed) = attempted(&attempt);
        assert_eq!(
            point(&passed, &head),
            Some(ResumePoint::Resume {
                task_id: task.clone(),
                commit: attempt.clone()
            })
        );
        assert_eq!(point(&[], &head), None);
        let (_, at_head) = attempted(&head);
        assert_eq!(point(&at_head, &head), None);
        // Another node was integrated since: resuming would drop it, so the retry starts at the head and says why.
        assert_eq!(
            point(&passed, &moved),
            Some(ResumePoint::Skipped {
                task_id: task,
                commit: Some(attempt.clone()),
                reason: "head_moved"
            })
        );
        let missing = "0".repeat(40);
        let (lost, lost_events) = attempted(&missing);
        assert_eq!(
            point(&lost_events, &head),
            Some(ResumePoint::Skipped {
                task_id: lost,
                commit: Some(missing),
                reason: "missing_commit"
            })
        );
        assert!(resume_instruction(&attempt).encode_utf16().count() <= 1_000);
        daemon.stop();
    });
}

#[test]
fn resumes_from_the_latest_attempt_that_passed_postflight_unless_its_review_asked_for_a_redesign() {
    let state = |kind: &str, node: &str, node_state: &str, lease: Value| {
        at(
            kind,
            json!({ "nodeId": node, "nodeState": node_state, "leaseId": lease, "attempt": 1 }),
        )
    };
    let verdict = |node: &str, value: &str| {
        at(
            "record.submitted",
            json!({ "$schema": schema_uri("review-verdict"), "verdict": value, "summary": "s", "target": { "targetType": "graph_node", "targetId": node, "targetVersion": 1 }, "criteria": [] }),
        )
    };
    let attempt_on = |task: &str, passed: bool, review: Option<&str>, node: &str, kind: &str| {
        let mut events = vec![
            state(kind, node, "leased", json!(task)),
            state(kind, node, "working", json!(task)),
        ];
        if passed {
            events.push(state(kind, node, "postflight", json!(task)));
            events.push(state(kind, node, "reviewing", json!(task)));
            if let Some(review) = review {
                events.push(verdict(node, review));
            }
        } else {
            events.push(at(
                "task.postflight_failed",
                json!({ "taskId": task, "nodeId": node, "issues": ["x"] }),
            ));
        }
        events.push(state(kind, node, "failed", Value::Null));
        events.push(state(kind, node, "ready", Value::Null));
        events
    };
    let attempt = |task: &str, passed: bool, review: Option<&str>| {
        attempt_on(
            task,
            passed,
            review,
            "mnode_a",
            "mission.node_state_changed",
        )
    };
    let concat = |parts: Vec<Vec<Value>>| parts.concat();
    assert_eq!(resumable_attempt(&[], "mnode_a"), Value::Null);
    // Run 2: attempt 7 failed postflight, attempt 8 passed it and lost to a reviewer error, attempt 9 failed postflight.
    assert_eq!(
        resumable_attempt(
            &concat(vec![
                attempt("tsk_7", false, None),
                attempt("tsk_8", true, None),
                attempt("tsk_9", false, None)
            ]),
            "mnode_a"
        ),
        json!({ "taskId": "tsk_8" })
    );
    assert_eq!(
        resumable_attempt(
            &concat(vec![
                attempt("tsk_1", true, Some("REVISE")),
                attempt("tsk_2", true, Some("REVISE"))
            ]),
            "mnode_a"
        ),
        json!({ "taskId": "tsk_2" })
    );
    assert_eq!(
        resumable_attempt(&attempt("tsk_1", true, Some("BLOCKED")), "mnode_a"),
        json!({ "taskId": "tsk_1" })
    );
    // A redesign verdict rejects the approach itself, so the next attempt starts over (or from an earlier passed attempt).
    assert_eq!(
        resumable_attempt(&attempt("tsk_1", true, Some("REDESIGN")), "mnode_a"),
        Value::Null
    );
    assert_eq!(
        resumable_attempt(
            &concat(vec![
                attempt("tsk_1", true, Some("REVISE")),
                attempt("tsk_2", true, Some("REDESIGN"))
            ]),
            "mnode_a"
        ),
        json!({ "taskId": "tsk_1" })
    );
    // Another node's attempts and verdicts never count; the Direction supervisor's event names do.
    assert_eq!(
        resumable_attempt(
            &attempt_on(
                "tsk_1",
                true,
                Some("REVISE"),
                "mnode_b",
                "mission.node_state_changed"
            ),
            "mnode_a"
        ),
        Value::Null
    );
    let mixed = concat(vec![
        attempt("tsk_1", true, Some("REVISE")),
        vec![verdict("mnode_a", "PASS")],
        attempt_on(
            "tsk_2",
            true,
            Some("REDESIGN"),
            "mnode_b",
            "mission.node_state_changed",
        ),
    ]);
    assert_eq!(
        resumable_attempt(&mixed, "mnode_a"),
        json!({ "taskId": "tsk_1" })
    );
    assert_eq!(
        resumable_attempt(
            &attempt_on(
                "tsk_d",
                true,
                Some("REVISE"),
                "dnode_a",
                "direction.node_state_changed"
            ),
            "dnode_a"
        ),
        json!({ "taskId": "tsk_d" })
    );
}
