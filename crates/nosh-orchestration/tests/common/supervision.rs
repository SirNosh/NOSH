//! The supervisor test seam of `mission-supervisor.test.ts`: a standalone research control over
//! one Project store, a real Git repository, and `FixtureAgents`, which answers each prompt with
//! the record its role must submit by parsing the daemon's prompt anchors.

use super::inspection;
use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::pi::{Inspection, TerminalStep};
use nosh_agent::policy::{Role, SessionOptions};
use nosh_orchestration::Agents;
use nosh_orchestration::supervisors::Dependencies;
use nosh_research::artifacts::ArtifactStore;
use nosh_research::research_control::{DirectionInput, Host, MissionInput, ResearchControl};
use nosh_store::EventStore;
use nosh_wire::{EVENT_SCHEMA, IdPrefix, create_id, is_task_terminal_record, now_utc, schema_uri};
use regex::Regex;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// `new ResearchControl(() => store, () => project, () => undefined)`.
pub struct TestHost {
    pub store: Rc<EventStore>,
    pub project: Value,
}

impl Host for TestHost {
    fn store(&self, _project_id: &str) -> nosh_research::Result<Rc<EventStore>> {
        Ok(Rc::clone(&self.store))
    }
    fn project(&self, _project_id: &str) -> nosh_research::Result<Value> {
        Ok(self.project.clone())
    }
    fn publish(&self, _event: &Value) {}
}

/// `git -C <path> <args>` with the repository's own configuration; panics on failure.
pub fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Whether `git -C <path> <args>` exits 0.
pub fn git_succeeds(path: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap()
        .status
        .success()
}

pub fn initialize_repository(path: &Path) {
    git(path, &["init", "-b", "main"]);
    git(path, &["config", "user.email", "fixture@nosh.test"]);
    git(path, &["config", "user.name", "NOSH Fixture"]);
    std::fs::write(path.join(".gitignore"), ".nosh/\n.fixture-*\n").unwrap();
    std::fs::write(path.join("README.md"), "# Fixture\n").unwrap();
    git(path, &["add", "--", ".gitignore", "README.md"]);
    git(path, &["commit", "-m", "fixture baseline"]);
}

static TASK_CORRELATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^task:(tsk_[0-9a-f]{32})$").unwrap());

/// Appends a draft as the daemon would: task-terminal records through the terminal authority.
pub fn append_fixture_event(store: &EventStore, draft: &Value) {
    if draft["type"] == "record.submitted" && is_task_terminal_record(&draft["payload"]) {
        let correlation = draft["correlationId"].as_str().unwrap_or_default();
        let task_id = TASK_CORRELATION
            .captures(correlation)
            .map(|captures| captures[1].to_owned())
            .expect("Fixture terminal submission requires an exact Task correlation");
        let schema = draft["payload"]["$schema"].as_str().unwrap().to_owned();
        store
            .append_terminal_submission(&task_id, "fixture", &schema, &draft["payload"], draft)
            .unwrap();
    } else {
        store.append(draft).unwrap();
    }
}

/// Task sessions run in `.nosh/worktrees/<id>`; the Artifact store is Project-level.
fn project_root(cwd: &Path) -> PathBuf {
    let text = cwd.to_string_lossy();
    let marker = Path::new(".nosh").join("worktrees");
    match text.find(&*marker.to_string_lossy()) {
        Some(index) => PathBuf::from(&text[..index]),
        None => cwd.to_path_buf(),
    }
}

static TARGET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"target\.targetId=([a-z0-9_]+)").unwrap());
static CYCLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""cycleId":"(cycle_[0-9a-f]+)""#).unwrap());
static GRAPH_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"graph version (\d+)").unwrap());
static CONTRACT_HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sha256:[0-9a-f]{64}").unwrap());

/// `text.split(start)[1]?.split(end)[0]`.
/// Spends one use of a knob; true while it had uses left.
fn take(counter: &Cell<usize>) -> bool {
    let left = counter.get();
    if left > 0 {
        counter.set(left - 1);
    }
    left > 0
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    text.split(start)
        .nth(1)
        .map(|rest| rest.split(end).next().unwrap_or_default())
}

/// Sessions that answer every prompt with the record their role must submit.
pub struct FixtureAgents {
    store: Rc<EventStore>,
    sessions: RefCell<Vec<SessionOptions>>,
    fail_general_workers: bool,
    defer_last_criterion: bool,
    /// General-worker prompts that fail like a provider/terminal glitch (no completion at all)
    /// before workers succeed.
    pub transient_worker_failures: Cell<usize>,
    /// Mission Director prompts whose cycle record the host rejects, as when a model writes a
    /// schema-invalid spawn.
    pub director_slips: Cell<usize>,
    /// Director prompts whose valid cycle record judges the selected work not to contribute.
    pub director_halts: Cell<usize>,
    /// Verdicts for the next Reviews of general-worker nodes, in order; PASS once empty.
    pub worker_review_verdicts: RefCell<VecDeque<String>>,
    /// Verdicts for the next Mission completion or Direction closure Reviews; PASS once empty.
    pub closure_review_verdicts: RefCell<VecDeque<String>>,
    /// Errors the next reviewer prompts fail with, in order.
    pub reviewer_failures: RefCell<VecDeque<String>>,
    /// General workers that report their validator command as failed.
    pub failing_commands: Cell<usize>,
    /// Each general worker's worktree files when its session started, by Task ID.
    pub worker_start_files: RefCell<HashMap<String, Vec<String>>>,
    worker_nodes: RefCell<HashSet<String>>,
}

impl FixtureAgents {
    pub fn new(
        store: Rc<EventStore>,
        fail_general_workers: bool,
        defer_last_criterion: bool,
    ) -> Self {
        FixtureAgents {
            store,
            sessions: RefCell::default(),
            fail_general_workers,
            defer_last_criterion,
            transient_worker_failures: Cell::new(0),
            director_slips: Cell::new(0),
            director_halts: Cell::new(0),
            worker_review_verdicts: RefCell::default(),
            closure_review_verdicts: RefCell::default(),
            reviewer_failures: RefCell::default(),
            failing_commands: Cell::new(0),
            worker_start_files: RefCell::default(),
            worker_nodes: RefCell::default(),
        }
    }

    fn submit(&self, session: &SessionOptions, record: Value) {
        let scope = json!({ "projectId": session.project_id, "missionId": session.mission_id, "directionId": session.direction_id, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": session.agent_id });
        let correlation = format!("task:{}", session.task_id.as_deref().unwrap_or_default());
        append_fixture_event(
            &self.store,
            &json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "fixture", "scope": scope, "correlationId": correlation, "causationId": null, "payload": record }),
        );
    }

    fn answer(&self, agent_id: &str, prompt: &str) -> Result<(), String> {
        let session = self
            .sessions
            .borrow()
            .iter()
            .find(|session| session.agent_id == agent_id)
            .cloned()
            .unwrap();
        let task_id = session
            .task_id
            .clone()
            .ok_or("Fixture session requires a Task ID")?;
        if session.role == Role::MissionDirector && take(&self.director_slips) {
            return Err(r#"Terminal output rejected: {"accepted":false,"retryAllowed":false,"error":"Invalid terminal record: minimumEffect too_small"}"#.into());
        }
        if session.role == Role::GeneralWorker && take(&self.transient_worker_failures) {
            return Err("Terminal output rejected: fixture transient failure".into());
        }
        if session.role == Role::Reviewer
            && let Some(message) = self.reviewer_failures.borrow_mut().pop_front()
        {
            return Err(message);
        }
        let contributes = !(matches!(session.role, Role::MissionDirector | Role::ResearchDirector)
            && take(&self.director_halts));
        let target = TARGET
            .captures(prompt)
            .map(|captures| captures[1].to_owned())
            .unwrap_or_default();
        let packet: Option<Value> = between(prompt, "Task Packet: ", ". Complete only")
            .map(|text| serde_json::from_str(text).unwrap());
        let request: Option<Value> = between(prompt, "Review Request: ", ". Submit")
            .map(|text| serde_json::from_str(text).unwrap());
        let now = now_utc();
        let cycle_id = CYCLE
            .captures(prompt)
            .map(|captures| captures[1].to_owned());
        let graph_version = GRAPH_VERSION
            .captures(prompt)
            .and_then(|captures| captures[1].parse::<i64>().ok());
        let record = match (session.role, &packet) {
            (Role::MissionDirector, _) => {
                json!({ "$schema": schema_uri("mission-director-cycle"), "cycleId": cycle_id, "directorAgentId": agent_id, "missionId": session.mission_id, "observedGraphVersion": graph_version, "northStarCheck": { "currentWorkContributes": contributes } })
            }
            (Role::ResearchDirector, _) => {
                let hash = CONTRACT_HASH
                    .find(prompt)
                    .map(|found| found.as_str())
                    .unwrap_or_default();
                json!({ "$schema": schema_uri("research-director-cycle"), "cycleId": cycle_id, "directorAgentId": agent_id, "directionId": session.direction_id, "observedGraphVersion": graph_version, "evaluationContractHash": hash, "questionCheck": { "currentWorkContributes": contributes } })
            }
            (Role::Reviewer, _) => self.verdict(agent_id, &target, request.as_ref(), &now),
            (Role::LibrarianResearcher, Some(_)) => self.librarian(&session, &task_id, &now),
            (Role::GeneralWorker, Some(packet)) => self.worker(&session, &task_id, packet),
            (Role::LibrarianResearcher, None) => {
                json!({ "$schema": schema_uri("librarian-completion"), "taskOutcome": "completed", "readyForReview": true })
            }
            _ => {
                json!({ "$schema": schema_uri("general-worker-completion"), "taskOutcome": "completed", "readyForReview": true, "readyForDeterministicPostflight": true, "codeChanges": { "endingCommit": git(&session.cwd, &["rev-parse", "HEAD"]) } })
            }
        };
        self.submit(&session, record);
        if session.direction_id.is_some()
            && matches!(
                session.role,
                Role::GeneralWorker | Role::LibrarianResearcher
            )
        {
            self.submit(
                &session,
                json!({ "$schema": schema_uri("evidence"), "evidenceId": create_id(IdPrefix::Evd), "statement": "Fixture result", "sourceRefs": [{ "refType": "task", "refId": session.task_id, "locator": "fixture" }] }),
            );
        }
        Ok(())
    }

    fn verdict(&self, agent_id: &str, target: &str, request: Option<&Value>, now: &str) -> Value {
        let review_id = request
            .and_then(|request| request["reviewId"].as_str().map(str::to_owned))
            .unwrap_or_else(|| create_id(IdPrefix::Rev));
        let worker_review = request.is_some_and(|request| {
            request["reviewType"] == "task"
                && self
                    .worker_nodes
                    .borrow()
                    .contains(request["target"]["targetId"].as_str().unwrap_or_default())
        });
        let closure = request.is_some_and(|request| {
            matches!(
                request["reviewType"].as_str(),
                Some("mission_completion" | "direction_closure")
            )
        });
        let queued = if worker_review {
            self.worker_review_verdicts.borrow_mut().pop_front()
        } else if closure {
            self.closure_review_verdicts.borrow_mut().pop_front()
        } else {
            None
        };
        let verdict = queued.unwrap_or_else(|| "PASS".into());
        let pass = verdict == "PASS";
        let review_type = match request {
            Some(request) => request["reviewType"].clone(),
            None if target.starts_with("mis_") => json!("mission_completion"),
            None if target.starts_with("dir_") => json!("direction_closure"),
            None => json!("task"),
        };
        let criteria: Vec<Value> = match request {
            Some(request) => request["criteria"].as_array().unwrap().iter().map(|criterion| json!({ "criterionId": criterion["criterionId"], "required": criterion["required"], "status": if pass { "PASS" } else { "FAIL" }, "finding": "All fixture inputs passed", "evidenceRefs": [], "confidence": "high", "defectIds": [] })).collect(),
            None => vec![json!({ "criterionId": "criterion_fixture", "status": "PASS", "finding": "All fixture inputs passed", "evidenceRefs": [], "confidence": "high", "defectIds": [] })],
        };
        let field = |key: &str, fallback: Value| {
            request.map_or(fallback.clone(), |request| request[key].clone())
        };
        json!({
            "$schema": schema_uri("review-verdict"), "schemaVersion": 1, "templateVersion": "1.0.0", "reviewId": review_id,
            "reviewRequestId": field("reviewRequestId", json!(format!("request_{}", &review_id[4..]))),
            "reviewType": review_type,
            "target": field("target", json!({ "targetType": "graph_node", "targetId": target, "targetVersion": 1 })),
            "reviewerAgentId": agent_id, "independenceCheck": "pass", "verdict": verdict,
            "summary": if pass { "Fixture review passed".to_owned() } else { format!("Fixture review returned {verdict}") },
            "criteria": criteria, "defects": [], "missingRequiredInputs": [], "scientificIntegrityFlags": [],
            "recommendedGraphAction": if pass { "accept_node" } else { "revise_node" }, "recommendedPromotion": "not_applicable",
            "reviewedArtifactIds": field("requiredArtifactIds", json!([])), "reviewedEvidenceIds": field("requiredEvidenceIds", json!([])), "submittedAt": now,
        })
    }

    fn add_artifact(session: &SessionOptions, kind: &str, media_type: &str, path: &Path) -> String {
        let artifacts =
            ArtifactStore::open(&project_root(&session.cwd).join(".nosh").join("artifacts"))
                .unwrap();
        let input = json!({ "artifactId": create_id(IdPrefix::Art), "projectId": session.project_id, "kind": kind, "mediaType": media_type, "sourcePath": path.to_string_lossy(), "retentionClass": "accepted_evidence" });
        let artifact = artifacts.add(input.as_object().unwrap()).unwrap();
        artifact["artifactId"].as_str().unwrap().to_owned()
    }

    fn librarian(&self, session: &SessionOptions, task_id: &str, now: &str) -> Value {
        let suffix = &task_id[4..];
        let write = |name: String, text: &str| {
            let path = session.cwd.join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let source_path = write(
            format!(".fixture-source-{task_id}.txt"),
            "Primary fixture source\n",
        );
        let report_path = write(
            format!(".fixture-report-{task_id}.md"),
            "# Fixture report\n",
        );
        let bibliography_path = write(
            format!(".fixture-bibliography-{task_id}.bib"),
            "@misc{fixture}\n",
        );
        let source = Self::add_artifact(session, "source", "text/plain", &source_path);
        let report = Self::add_artifact(session, "report", "text/markdown", &report_path);
        let bibliography = Self::add_artifact(
            session,
            "bibliography",
            "application/x-bibtex",
            &bibliography_path,
        );
        json!({
            "$schema": schema_uri("librarian-completion"), "schemaVersion": 1, "taskOutcome": "completed", "researchQuestion": "Fixture literature question",
            "searchCoverage": { "databases": ["fixture"], "queries": ["fixture query"], "dateRange": { "from": null, "to": null }, "language": ["en"], "inclusionCriteria": ["primary"], "exclusionCriteria": ["none"] },
            "sources": [{ "sourceId": format!("source_{suffix}"), "sourceType": "primary_source", "title": "Fixture source", "authors": ["Fixture Author"], "publicationDate": null, "canonicalUrl": "https://example.com/fixture", "persistentId": null, "version": "1", "primarySource": true, "accessedAt": now, "artifactId": source, "relevance": "Direct fixture support" }],
            "findings": [{ "findingId": format!("finding_{suffix}"), "statement": "Fixture finding", "support": [{ "sourceId": format!("source_{suffix}"), "locator": "line 1", "artifactId": source }], "confidence": "high", "noveltyImplication": "Fixture only" }],
            "contradictions": [], "evaluationDifferences": [], "knowledgeGaps": [], "candidateClaimEffects": [],
            "bibliographyArtifactId": bibliography, "reportArtifactId": report, "readyForReview": true,
        })
    }

    fn worker(&self, session: &SessionOptions, task_id: &str, packet: &Value) -> Value {
        let suffix = &task_id[4..];
        let cwd = &session.cwd;
        if let Some(node_id) = packet["scope"]["graphNodeId"].as_str() {
            self.worker_nodes.borrow_mut().insert(node_id.to_owned());
        }
        let mut files: Vec<String> = std::fs::read_dir(cwd)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        self.worker_start_files
            .borrow_mut()
            .insert(task_id.to_owned(), files);
        let starting = packet["workspace"]["startingCommit"].as_str().unwrap();
        let mut ending = git(cwd, &["rev-parse", "HEAD"]);
        let mut changed: Vec<String> = Vec::new();
        let mut artifact = None;
        let failing = self.fail_general_workers;
        let failing_command = !failing && take(&self.failing_commands);
        if !failing {
            let name = format!("fixture-{task_id}.txt");
            let output = cwd.join(&name);
            std::fs::write(&output, "Reviewed fixture output\n").unwrap();
            git(cwd, &["add", "--", &name]);
            git(cwd, &["commit", "-m", &format!("fixture {task_id}")]);
            ending = git(cwd, &["rev-parse", "HEAD"]);
            changed.extend(
                git(cwd, &["diff", "--name-only", starting, &ending])
                    .split('\n')
                    .map(str::to_owned),
            );
            artifact = Some(Self::add_artifact(
                session,
                "fixture_output",
                "text/plain",
                &output,
            ));
        }
        let artifacts: Vec<&String> = artifact.iter().collect();
        let validator = format!("validator_{suffix}");
        let all = packet["acceptanceCriteria"].as_array().unwrap();
        let criteria: Vec<Value> = all
            .iter()
            .enumerate()
            .map(|(index, criterion)| {
                let mut entry = json!({ "criterionId": criterion["criterionId"], "artifactIds": artifacts, "notes": "Fixture validator" });
                if self.defer_last_criterion && index == all.len() - 1 {
                    entry["workerClaim"] = json!("deferred_to_review");
                    entry["validatorRunIds"] = json!([]);
                } else {
                    entry["workerClaim"] = json!(if failing { "unsatisfied" } else { "satisfied" });
                    entry["validatorRunIds"] = json!([validator]);
                }
                entry
            })
            .collect();
        json!({
            "$schema": schema_uri("general-worker-completion"), "schemaVersion": 1, "taskOutcome": if failing { "failed" } else { "completed" },
            "workPerformed": [{ "action": "action_implementation", "subject": "Fixture output", "artifactIds": artifacts }],
            "codeChanges": { "startingCommit": starting, "endingCommit": ending, "changedPaths": changed, "diffArtifactId": null, "branch": packet["workspace"]["branch"] },
            "commands": [{ "commandId": validator, "displayCommand": "fixture validator", "exitCode": if failing || failing_command { 1 } else { 0 }, "resultArtifactId": null }],
            "criteria": criteria,
            "scientificImpact": { "claimIds": [], "evidenceIds": [], "interpretation": "Fixture impact" },
            "deviations": [], "newRisks": [], "unresolvedItems": [], "suggestedNextActions": [],
            "readyForDeterministicPostflight": !failing, "readyForReview": !failing,
        })
    }
}

impl Agents for FixtureAgents {
    fn start(&self, options: SessionOptions) -> LocalBoxFuture<'_, Result<Inspection, String>> {
        {
            let mut sessions = self.sessions.borrow_mut();
            match sessions
                .iter_mut()
                .find(|session| session.agent_id == options.agent_id)
            {
                Some(existing) => *existing = options.clone(),
                None => sessions.push(options.clone()),
            }
        }
        let found = self
            .inspect()
            .into_iter()
            .find(|agent| agent.agent_id == options.agent_id)
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
                let mut agent = inspection(
                    &session.project_id,
                    &session.agent_id,
                    session.task_id.as_deref(),
                    session.role,
                );
                agent.mission_id = session.mission_id.clone();
                agent.direction_id = session.direction_id.clone();
                agent.autoresearch_id = session.autoresearch_id.clone();
                agent.pi_session_id = format!("pi_{}", session.agent_id);
                agent.status = "running".into();
                agent.thinking_level = session
                    .thinking_level
                    .clone()
                    .unwrap_or_else(|| "medium".into());
                agent
            })
            .collect()
    }
}

/// One Project with its store, research control, and fixture sessions.
pub struct Supervised {
    pub directory: tempfile::TempDir,
    pub repository: PathBuf,
    pub project_id: String,
    pub project: Value,
    pub store: Rc<EventStore>,
    pub research: Rc<ResearchControl<TestHost>>,
    pub agents: Rc<FixtureAgents>,
}

impl Supervised {
    /// A Project over a Git repository (`initializeRepository`) unless `bare_directory`.
    pub fn new(
        fail_general_workers: bool,
        defer_last_criterion: bool,
        bare_directory: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        if !bare_directory {
            initialize_repository(&repository);
        }
        let project_id = create_id(IdPrefix::Prj);
        let database = directory.path().join("project.sqlite");
        let project = json!({ "projectId": project_id, "repositoryRoot": repository.to_string_lossy(), "databasePath": database.to_string_lossy(), "registeredAt": now_utc() });
        let store = Rc::new(EventStore::open(&database).unwrap());
        let research = Rc::new(ResearchControl::new(TestHost {
            store: Rc::clone(&store),
            project: project.clone(),
        }));
        let agents = Rc::new(FixtureAgents::new(
            Rc::clone(&store),
            fail_general_workers,
            defer_last_criterion,
        ));
        Supervised {
            directory,
            repository,
            project_id,
            project,
            store,
            research,
            agents,
        }
    }

    /// The supervisor constructor arguments: fixture sessions, events appended as the fixture
    /// appends them, no runtime and no job runner.
    pub fn dependencies(&self) -> Dependencies<TestHost> {
        let store = Rc::clone(&self.store);
        let project = self.project.clone();
        Dependencies {
            research: Rc::clone(&self.research),
            agents: Rc::clone(&self.agents) as Rc<dyn Agents>,
            projects: Rc::new(move || Ok(vec![project.clone()])),
            package_path: self.directory.path().join("pi-package"),
            schema_dir: super::repository_root().join("conformance/schemas/json"),
            emit: Rc::new(move |draft: &Value| {
                append_fixture_event(&store, draft);
                Ok(())
            }),
            runtime: None,
            job_runner: None,
        }
    }

    /// Creates a Mission and moves it through planning and approval to running.
    pub fn running_mission(
        &self,
        input: MissionInput,
        key_prefix: &str,
        through: &[&str],
    ) -> Value {
        let mut mission = self
            .research
            .create_mission(&self.project_id, &input)
            .unwrap();
        let mission_id = mission["entityId"].as_str().unwrap().to_owned();
        for next in through {
            mission = self
                .research
                .transition_mission(
                    &self.project_id,
                    &mission_id,
                    mission["version"].as_i64().unwrap(),
                    next,
                    &format!("{key_prefix}-{next}"),
                )
                .unwrap();
        }
        mission
    }

    pub fn mission(&self, mission_id: &str) -> Value {
        self.research.mission(&self.project_id, mission_id).unwrap()
    }

    /// Creates a user Direction (no evaluation contract) and moves it through `through`.
    pub fn direction_through(
        &self,
        question: &str,
        decision_use: &str,
        key_prefix: &str,
        through: &[&str],
    ) -> String {
        let input = DirectionInput {
            question: question.into(),
            decision_use: decision_use.into(),
            mission_id: None,
            evaluation_contract: None,
            idempotency_key: format!("{key_prefix}-create"),
        };
        let mut direction = self
            .research
            .create_direction(&self.project_id, &input, None)
            .unwrap();
        let direction_id = direction["entityId"].as_str().unwrap().to_owned();
        for next in through {
            direction = self
                .research
                .transition_direction(
                    &self.project_id,
                    &direction_id,
                    direction["version"].as_i64().unwrap(),
                    next,
                    &format!("{key_prefix}-{next}"),
                )
                .unwrap();
        }
        direction_id
    }

    pub fn direction(&self, direction_id: &str) -> Value {
        self.research
            .direction(&self.project_id, direction_id)
            .unwrap()
    }

    pub fn scoped(&self, mission_id: &str, kind: &str) -> Vec<Value> {
        self.research
            .scoped_events(&self.project_id, mission_id, Some(kind))
            .unwrap()
    }

    pub fn records(&self, schema: &str) -> Vec<Value> {
        self.research
            .records(&self.project_id, Some(schema))
            .unwrap()
    }
}

pub fn mission_input(
    title: &str,
    objective: &str,
    deliverables: &[&str],
    criteria: &[&str],
    key: &str,
) -> MissionInput {
    MissionInput {
        title: title.into(),
        objective: objective.into(),
        deliverables: deliverables.iter().map(|item| (*item).to_owned()).collect(),
        success_criteria: criteria.iter().map(|item| (*item).to_owned()).collect(),
        non_objectives: None,
        starting_evidence: None,
        research_plan: None,
        budgets: None,
        idempotency_key: key.into(),
    }
}

/// `for (deadline = now + seconds; now < deadline && !done();) { tick(); await sleep(20) }`.
pub async fn tick_until(seconds: u64, mut tick: impl FnMut(), done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && !done() {
        tick();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
