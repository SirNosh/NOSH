//! Port of `apps/noshd/src/autoresearch-supervisor.test.ts`. One case needs TypeScript's
//! `vi.spyOn` on the research control and is not ported: the completion packet recovered after
//! publication but before the state transition (`deferCompletion`). Job state events are not
//! appended to the store: no fixture Job uses a GPU, the only budget they feed.

mod common;

use common::supervision::{TestHost, append_fixture_event, git, initialize_repository};
use common::{assert_match, local};
use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::pi::{Inspection, TerminalStep};
use nosh_agent::policy::{Role, SessionOptions};
use nosh_orchestration::Agents;
use nosh_orchestration::research_spawner::JobRunner;
use nosh_orchestration::supervisors::Dependencies;
use nosh_orchestration::supervisors::autoresearch::{AutoresearchSupervisor, scope_snapshot};
use nosh_research::research_control::{
    AutoresearchInput, BaselineInput, DirectionInput, ResearchControl,
};
use nosh_store::EventStore;
use nosh_sys::jobs::{JobRecord, JobSpec, JobSupervisor};
use nosh_wire::{EVENT_SCHEMA, IdPrefix, create_id, now_utc, schema_uri, sha256, validate_record};
use regex::Regex;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// The Project store, replaceable as a daemon restart replaces it.
type Shared = Rc<RefCell<Rc<EventStore>>>;
type Transform = Box<dyn Fn(Value) -> Result<Value, String>>;

static EXPERIMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:experimentId=|"experimentId":")(exp_[0-9a-f]{32})"#).unwrap()
});
static AUTORESEARCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:autoresearchId=|"autoresearchId":")(ar_[0-9a-f]{32})"#).unwrap()
});
static PARENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:parentExperimentId=|"parentExperimentId":")(exp_[0-9a-f]{32})"#).unwrap()
});
static ROUND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:round=|"round":)(\d+)"#).unwrap());
static CONTRACT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:evaluationContractHash=|"evaluationContractHash":")(sha256:[0-9a-f]{64})"#)
        .unwrap()
});
static ORDINAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"variantOrdinal=(\d+)").unwrap());

fn required(text: &str, pattern: &Regex) -> String {
    pattern
        .captures(text)
        .map(|captures| captures[1].to_owned())
        .unwrap_or_else(|| panic!("Missing fixture prompt value for {pattern}"))
}

/// `text.split(start)[1]?.split(end)[0]`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    text.split(start)
        .nth(1)
        .map(|rest| rest.split(end).next().unwrap_or_default())
}

/// Spends one use of a knob; true while it had uses left.
fn take(counter: &Cell<usize>) -> bool {
    let left = counter.get();
    if left > 0 {
        counter.set(left - 1);
    }
    left > 0
}

struct AgentOptions {
    transform_review: Transform,
    fail_proposals: usize,
    refail_proposals: usize,
    scores: Vec<i64>,
    fail_implementations: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        AgentOptions {
            transform_review: Box::new(Ok),
            fail_proposals: 0,
            refail_proposals: 0,
            scores: vec![2, 3, 5, 1],
            fail_implementations: 0,
        }
    }
}

/// Sessions that answer the autoresearcher's proposal and implementation prompts and every
/// Review, by parsing the daemon's prompt anchors.
struct ExperimentAgents {
    store: Shared,
    sessions: RefCell<Vec<SessionOptions>>,
    started: RefCell<Vec<SessionOptions>>,
    proposal_prompts: RefCell<Vec<String>>,
    review_prompts: RefCell<Vec<String>>,
    reviewed_evidence_candidates: RefCell<Vec<Value>>,
    /// Proposal prompts the host rejects, as when a live model returns a malformed envelope.
    fail_proposals: Cell<usize>,
    /// Proposal rejections that follow the first valid proposal.
    refail_proposals: Cell<usize>,
    /// Implementation sessions that end without a completion record, like a worker that crashed.
    fail_implementations: Cell<usize>,
    scores: Vec<i64>,
    transform_review: Transform,
}

impl ExperimentAgents {
    fn submit(&self, session: &SessionOptions, record: Value) {
        let scope = json!({ "projectId": session.project_id, "missionId": session.mission_id, "directionId": session.direction_id, "autoresearchId": session.autoresearch_id, "experimentId": session.experiment_id, "runId": session.run_id, "jobId": session.job_id, "agentId": session.agent_id });
        let correlation = format!("task:{}", session.task_id.as_deref().unwrap_or_default());
        append_fixture_event(
            &self.store.borrow(),
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
        let now = now_utc();
        if prompt.contains(&schema_uri("experiment-proposal")) {
            if take(&self.fail_proposals) {
                return Err(r#"Terminal output rejected: {"accepted":false,"retryAllowed":false,"error":"Expected ',' or ']' after array element in JSON"}"#.into());
            }
            self.fail_proposals.set(self.refail_proposals.take());
            self.proposal_prompts.borrow_mut().push(prompt.to_owned());
            let experiment_id = required(prompt, &EXPERIMENT);
            let round: i64 = required(prompt, &ROUND).parse().unwrap();
            self.submit(
                &session,
                json!({
                    "$schema": schema_uri("experiment-proposal"), "schemaVersion": 1, "proposalId": format!("proposal_{}", &experiment_id[4..]), "autoresearchId": required(prompt, &AUTORESEARCH),
                    "experimentId": experiment_id, "parentExperimentId": required(prompt, &PARENT), "round": round,
                    "hypothesis": format!("Variant {} improves the primary metric", &experiment_id[4..10]), "rationale": "Exercise the frozen evaluation contract",
                    "primaryChange": { "category": "change_variant", "description": "Write the bounded variant input", "expectedFiles": ["variant.json"] },
                    "expectedEffect": { "score": "measurable" }, "guardrailRisks": ["Fixture only"],
                    "novelty": { "ideaFingerprint": sha256(&json!({ "experimentId": experiment_id })), "nearestAttemptFingerprint": null, "difference": "Distinct fixture ordinal", "duplicateStatus": "distinct" },
                    "evaluationContractHash": required(prompt, &CONTRACT), "estimatedGpuSeconds": 0, "proposedByAgentId": agent_id, "submittedAt": now,
                }),
            );
            return Ok(());
        }
        if prompt.contains(". Implement exactly")
            && let Some(packet) = between(prompt, "Task Packet: ", ". Implement exactly")
        {
            let packet: Value = serde_json::from_str(packet).unwrap();
            if take(&self.fail_implementations) {
                return Ok(());
            }
            let ordinal: usize = required(prompt, &ORDINAL).parse().unwrap();
            let score = self.scores[ordinal - 1];
            std::fs::write(
                session.cwd.join("variant.json"),
                format!("{{\"score\":{score}}}\n"),
            )
            .unwrap();
            // Stands in for the daemon's completion commit.
            git(&session.cwd, &["add", "--", "variant.json"]);
            git(
                &session.cwd,
                &["commit", "-m", &format!("experiment variant {ordinal}")],
            );
            let ending = git(&session.cwd, &["rev-parse", "HEAD"]);
            let validator = format!("validator_{}", &packet["taskId"].as_str().unwrap()[4..]);
            self.submit(
                &session,
                json!({
                    "$schema": schema_uri("general-worker-completion"), "schemaVersion": 1, "taskOutcome": "completed",
                    "workPerformed": [{ "action": "action_implementation", "subject": "Bounded variant", "artifactIds": [] }],
                    "codeChanges": { "startingCommit": packet["workspace"]["startingCommit"], "endingCommit": ending, "changedPaths": ["variant.json"], "diffArtifactId": null, "branch": packet["workspace"]["branch"] },
                    "commands": [{ "commandId": validator, "displayCommand": "fixture validation", "exitCode": 0, "resultArtifactId": null }],
                    "criteria": [{ "criterionId": packet["acceptanceCriteria"][0]["criterionId"], "workerClaim": "satisfied", "validatorRunIds": [validator], "artifactIds": [], "notes": "Fixture passed" }],
                    "scientificImpact": { "claimIds": [], "evidenceIds": [], "interpretation": "Fixture variant" },
                    "deviations": [], "newRisks": [], "unresolvedItems": [], "suggestedNextActions": [], "readyForDeterministicPostflight": true, "readyForReview": true,
                }),
            );
            return Ok(());
        }
        if let Some(request) = between(prompt, "Review Request: ", ". Submit") {
            self.review_prompts.borrow_mut().push(prompt.to_owned());
            let request: Value = serde_json::from_str(request).unwrap();
            if request["reviewType"] == "experiment" {
                let candidate = between(
                    prompt,
                    "Review this candidate Evidence for publication with your exact disposition: ",
                    " Review Request: ",
                )
                .expect("the keep Review shows its candidate Evidence");
                let candidate: Value = serde_json::from_str(candidate).unwrap();
                assert_eq!(
                    request["requiredEvidenceIds"],
                    json!([candidate["evidenceId"]])
                );
                assert!(validate_record(&schema_uri("evidence"), &candidate).is_ok());
                self.reviewed_evidence_candidates
                    .borrow_mut()
                    .push(candidate);
            }
            let criteria: Vec<Value> = request["criteria"]
                .as_array()
                .unwrap()
                .iter()
                .map(|criterion| json!({ "criterionId": criterion["criterionId"], "required": criterion["required"], "status": "PASS", "finding": "Frozen inputs and comparison passed", "evidenceRefs": [], "confidence": "high", "defectIds": [] }))
                .collect();
            let promotion = if request["reviewType"] == "autoresearch_closure" {
                "not_applicable"
            } else {
                "promote"
            };
            let verdict = (self.transform_review)(json!({
                "$schema": schema_uri("review-verdict"), "schemaVersion": 1, "templateVersion": "1.0.0", "reviewId": request["reviewId"], "reviewRequestId": request["reviewRequestId"],
                "reviewType": request["reviewType"], "target": request["target"], "reviewerAgentId": agent_id, "independenceCheck": "pass", "verdict": "PASS",
                "summary": "Independent fixture Review passed", "criteria": criteria, "defects": [], "missingRequiredInputs": [], "scientificIntegrityFlags": [],
                "recommendedGraphAction": "action_accept", "recommendedPromotion": promotion,
                "reviewedArtifactIds": request["requiredArtifactIds"], "reviewedEvidenceIds": request["requiredEvidenceIds"], "submittedAt": now,
            }))?;
            self.submit(&session, verdict);
            return Ok(());
        }
        Err("Unexpected fixture prompt".into())
    }
}

impl Agents for ExperimentAgents {
    fn start(&self, options: SessionOptions) -> LocalBoxFuture<'_, Result<Inspection, String>> {
        self.started.borrow_mut().push(options.clone());
        self.sessions.borrow_mut().push(options.clone());
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
                let mut agent = common::inspection(
                    &session.project_id,
                    &session.agent_id,
                    session.task_id.as_deref(),
                    session.role,
                );
                agent.mission_id = session.mission_id.clone();
                agent.direction_id = session.direction_id.clone();
                agent.autoresearch_id = session.autoresearch_id.clone();
                agent
            })
            .collect()
    }
}

/// `startJob`: a durable launch intent around the Job supervisor's start.
struct FixtureJobs {
    supervisor: JobSupervisor,
    store: Shared,
    project_id: String,
}

impl JobRunner for FixtureJobs {
    fn start(&self, spec: &JobSpec) -> nosh_orchestration::Result<JobRecord> {
        let store = Rc::clone(&self.store.borrow());
        let intent = store.begin_operation(
            &self.project_id,
            "job.launch",
            &format!("job-launch:{}", spec.job_id),
            &serde_json::to_value(spec).unwrap(),
        )?;
        let job = self.supervisor.start(spec)?;
        store.complete_operation(
            &self.project_id,
            intent["intentId"].as_str().unwrap(),
            &serde_json::to_value(&job).unwrap(),
        )?;
        Ok(job)
    }

    fn get(&self, job_id: &str) -> nosh_orchestration::Result<JobRecord> {
        Ok(self.supervisor.get(job_id)?)
    }
}

/// One Project with its store, research control, fixture sessions, and real Jobs.
struct Lab {
    directory: tempfile::TempDir,
    repository: PathBuf,
    project_id: String,
    project: Value,
    store: Shared,
    research: RefCell<Rc<ResearchControl<TestHost>>>,
    agents: Rc<ExperimentAgents>,
    jobs: Rc<FixtureJobs>,
}

impl Lab {
    fn new(initialize: bool, options: AgentOptions) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        if initialize {
            initialize_repository(&repository);
        }
        let project_id = create_id(IdPrefix::Prj);
        let database = directory.path().join("project.sqlite");
        let project = json!({ "projectId": project_id, "repositoryRoot": repository.to_string_lossy(), "databasePath": database.to_string_lossy(), "registeredAt": now_utc() });
        let store: Shared = Rc::new(RefCell::new(Rc::new(EventStore::open(&database).unwrap())));
        let research = RefCell::new(Rc::new(ResearchControl::new(TestHost {
            store: Rc::clone(&store.borrow()),
            project: project.clone(),
        })));
        let agents = Rc::new(ExperimentAgents {
            store: Rc::clone(&store),
            sessions: RefCell::default(),
            started: RefCell::default(),
            proposal_prompts: RefCell::default(),
            review_prompts: RefCell::default(),
            reviewed_evidence_candidates: RefCell::default(),
            fail_proposals: Cell::new(options.fail_proposals),
            refail_proposals: Cell::new(options.refail_proposals),
            fail_implementations: Cell::new(options.fail_implementations),
            scores: options.scores,
            transform_review: options.transform_review,
        });
        let supervisor = JobSupervisor::new(
            &directory.path().join("jobs"),
            common::job_wrapper(&directory.path().join("wrapper")),
            |_: &JobRecord| {},
        )
        .unwrap();
        let jobs = Rc::new(FixtureJobs {
            supervisor,
            store: Rc::clone(&store),
            project_id: project_id.clone(),
        });
        Lab {
            directory,
            repository,
            project_id,
            project,
            store,
            research,
            agents,
            jobs,
        }
    }

    fn research(&self) -> Rc<ResearchControl<TestHost>> {
        Rc::clone(&self.research.borrow())
    }

    fn store(&self) -> Rc<EventStore> {
        Rc::clone(&self.store.borrow())
    }

    /// Closes and reopens the Project store, as a daemon restart does.
    fn reopen(&self) {
        let store =
            Rc::new(EventStore::open(&self.directory.path().join("project.sqlite")).unwrap());
        *self.research.borrow_mut() = Rc::new(ResearchControl::new(TestHost {
            store: Rc::clone(&store),
            project: self.project.clone(),
        }));
        *self.store.borrow_mut() = store;
    }

    /// A fresh supervisor, as after a daemon restart.
    fn supervisor(&self) -> Rc<AutoresearchSupervisor<TestHost>> {
        let store = Rc::clone(&self.store);
        let project = self.project.clone();
        AutoresearchSupervisor::new(Dependencies {
            research: self.research(),
            agents: Rc::clone(&self.agents) as Rc<dyn Agents>,
            projects: Rc::new(move || Ok(vec![project.clone()])),
            package_path: self.directory.path().join("pi-package"),
            schema_dir: common::repository_root().join("conformance/schemas/json"),
            emit: Rc::new(move |draft: &Value| {
                append_fixture_event(&store.borrow(), draft);
                Ok(())
            }),
            runtime: None,
            job_runner: Some(Rc::clone(&self.jobs) as Rc<dyn JobRunner>),
        })
    }

    fn jobs_running(&self) -> bool {
        self.jobs
            .supervisor
            .list()
            .unwrap()
            .iter()
            .any(|job| !job.state.is_terminal())
    }

    async fn wait_for_jobs(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && self.jobs_running() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn create_autoresearch(&self, input: Value) -> Value {
        let input: AutoresearchInput = serde_json::from_value(input).unwrap();
        let research = self.research();
        let execution = research
            .create_autoresearch(&self.project_id, &input, None)
            .unwrap();
        let id = execution["entityId"].as_str().unwrap().to_owned();
        research
            .transition_autoresearch(
                &self.project_id,
                &id,
                execution["version"].as_i64().unwrap(),
                "running",
                &format!("running-{id}"),
            )
            .unwrap()
    }

    fn execution(&self, id: &str) -> Value {
        self.research()
            .autoresearch_execution(&self.project_id, id)
            .unwrap()
    }

    fn records(&self, id: &str) -> Vec<Value> {
        self.research()
            .submitted(&self.project_id, None)
            .unwrap()
            .into_iter()
            .filter(|entry| entry["scope"]["autoresearchId"] == id)
            .map(|mut entry| entry["payload"].take())
            .collect()
    }

    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        self.research()
            .events_after(&self.project_id, 0)
            .unwrap()
            .into_iter()
            .filter(|entry| entry["scope"]["autoresearchId"] == id && entry["type"] == kind)
            .map(|mut entry| entry["payload"].take())
            .collect()
    }
}

struct RunOptions {
    agents: AgentOptions,
    resumes: usize,
    maximum_experiments: usize,
    maximum_rounds: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            agents: AgentOptions::default(),
            resumes: 0,
            maximum_experiments: 1,
            maximum_rounds: 1,
        }
    }
}

struct Run {
    lab: Lab,
    execution: Value,
    records: Vec<Value>,
    errors: Vec<Value>,
    proposal_failures: usize,
    budget: Value,
}

/// `runAutoresearchFixture`: ticks a fresh supervisor each cycle until the run stops, resuming a
/// blocked run up to `resumes` times as a researcher would.
async fn run_autoresearch(evaluation_contract: Value, options: RunOptions) -> Run {
    let lab = Lab::new(true, options.agents);
    let execution = lab.create_autoresearch(json!({ "decisionQuestion": "Does the bounded variant meet the frozen stop condition?", "evaluationContract": evaluation_contract, "maximumExperiments": options.maximum_experiments, "maximumRounds": options.maximum_rounds, "idempotencyKey": format!("terminal-{}", create_id(IdPrefix::Cmd)) }));
    let id = execution["entityId"].as_str().unwrap().to_owned();
    let mut resumes = options.resumes;
    for cycle in 0..80 {
        let current = lab.execution(&id);
        if current["state"] == "blocked" && resumes > 0 {
            resumes -= 1;
            lab.research()
                .transition_autoresearch(
                    &lab.project_id,
                    &id,
                    current["version"].as_i64().unwrap(),
                    "running",
                    &format!("fixture-resume-{cycle}"),
                )
                .unwrap();
        } else if current["state"] != "running" {
            break;
        }
        // Reconstruct the supervisor to exercise its durable recovery path on every cycle.
        lab.supervisor().tick().await.unwrap();
        lab.wait_for_jobs().await;
    }
    Run {
        execution: lab.execution(&id),
        records: lab.records(&id),
        errors: lab.events(&id, "autoresearch.supervisor_error"),
        proposal_failures: lab.events(&id, "autoresearch.proposal_failed").len(),
        budget: lab
            .research()
            .autoresearch_budget_use(&lab.project_id, &id)
            .unwrap(),
        lab,
    }
}

fn of_schema<'a>(records: &'a [Value], name: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["$schema"] == schema_uri(name))
        .collect()
}

fn first<'a>(records: &'a [Value], name: &str) -> &'a Value {
    of_schema(records, name)[0]
}

/// Each experiment's latest result, in first-recorded order.
fn latest_results(records: &[Value]) -> Vec<Value> {
    let mut results: Vec<Value> = Vec::new();
    for record in of_schema(records, "experiment-result") {
        match results
            .iter_mut()
            .find(|result| result["experimentId"] == record["experimentId"])
        {
            Some(result) => *result = record.clone(),
            None => results.push(record.clone()),
        }
    }
    results
}

fn reviews_of_type(records: &[Value], review_type: &str) -> usize {
    of_schema(records, "review-request")
        .iter()
        .filter(|request| request["reviewType"] == review_type)
        .count()
}

fn node_command(script: &str) -> Value {
    json!(["node", "-e", script])
}

/// Scores the fixture's variants produce, in order; lower is better and the baseline is 10.
fn climb_contract(stop: usize) -> Value {
    json!({ "primaryMetric": { "name": "score", "objective": "minimize", "minimumEffect": 0.5 }, "baselineMetrics": { "score": 10 }, "stopConditions": { "maximumNoProgressRounds": stop }, "execution": { "runner": "native", "command": node_command("require('fs').copyFileSync('variant.json','metrics.json')"), "resultPath": "metrics.json", "timeoutSeconds": 30, "usesGpu": false } })
}

fn target_contract(stop_conditions: Value, round_widths: &[usize]) -> Value {
    json!({ "primaryMetric": { "name": "score", "objective": "minimize", "minimumEffect": 1 }, "baselineMetrics": { "score": 4 }, "stopConditions": stop_conditions, "roundWidths": round_widths, "execution": { "runner": "native", "command": node_command(r#"require('fs').writeFileSync('metrics.json','{"score":2}')"#), "resultPath": "metrics.json", "timeoutSeconds": 30, "usesGpu": false } })
}

/// Scores 2 against a baseline of 4 but fails its `guardrail_pass` guardrail.
fn guarded_contract(extra: Value) -> Value {
    let mut contract = json!({ "primaryMetric": { "name": "score", "objective": "minimize", "minimumEffect": 1 }, "baselineMetrics": { "score": 4 }, "guardrails": [{ "name": "guardrail_pass", "direction": "maximize", "threshold": 1 }], "roundWidths": [1], "execution": { "runner": "native", "command": node_command(r#"require('fs').writeFileSync('metrics.json','{"score":2,"guardrail_pass":false}')"#), "resultPath": "metrics.json", "timeoutSeconds": 30, "usesGpu": false } });
    contract
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    contract
}

fn transform(edit: impl Fn(Value) -> Value + 'static) -> Transform {
    Box::new(move |record| Ok(edit(record)))
}

/// Marks a verdict of `review_type` REVISE with every criterion INCONCLUSIVE.
fn inconclusive(review_type: &'static str) -> Transform {
    transform(move |mut record| {
        if record["reviewType"] == review_type {
            record["verdict"] = json!("REVISE");
            record["recommendedPromotion"] = json!("reject");
            for criterion in record["criteria"].as_array_mut().unwrap() {
                criterion["status"] = json!("INCONCLUSIVE");
            }
        }
        record
    })
}

fn expect_completion_authority(records: &[Value], execution: &Value) {
    let packets = of_schema(records, "autoresearch-completion-packet");
    assert_eq!(packets.len(), 1);
    let packet = packets[0];
    assert!(validate_record(&schema_uri("autoresearch-completion-packet"), packet).is_ok());
    let with_review = |name: &str| -> Vec<&Value> {
        of_schema(records, name)
            .into_iter()
            .filter(|record| record["reviewId"] == packet["closureReviewId"])
            .collect()
    };
    let (requests, verdicts) = (with_review("review-request"), with_review("review-verdict"));
    assert_eq!(requests.len(), 1);
    assert_eq!(verdicts.len(), 1);
    let (request, verdict) = (requests[0], verdicts[0]);
    assert!(validate_record(&schema_uri("review-request"), request).is_ok());
    assert!(validate_record(&schema_uri("review-verdict"), verdict).is_ok());
    assert_match(
        request,
        &json!({ "reviewType": "autoresearch_closure", "target": { "targetType": "target_autoresearch", "targetId": execution["entityId"], "targetVersion": execution["version"].as_i64().unwrap() - 1 }, "requiredArtifactIds": [packet["synthesisArtifactId"]], "requiredEvidenceIds": packet["evidenceIds"] }),
    );
    assert_match(
        verdict,
        &json!({ "reviewRequestId": request["reviewRequestId"], "reviewType": request["reviewType"], "target": request["target"], "reviewerAgentId": request["reviewerAgentId"], "independenceCheck": "pass", "verdict": "PASS", "reviewedArtifactIds": request["requiredArtifactIds"], "reviewedEvidenceIds": request["requiredEvidenceIds"] }),
    );
    assert!(
        !request["producerAgentIds"]
            .as_array()
            .unwrap()
            .contains(&request["reviewerAgentId"])
    );
}

fn expect_reviewed_evidence_authority(records: &[Value]) {
    let reviewed: Vec<&Value> = of_schema(records, "evidence")
        .into_iter()
        .filter(|evidence| evidence["quality"]["status"] == "reviewed")
        .collect();
    assert!(!reviewed.is_empty());
    for evidence in reviewed {
        let review_id = &evidence["quality"]["reviewId"];
        let with_review = |name: &str| -> Vec<&Value> {
            of_schema(records, name)
                .into_iter()
                .filter(|record| record["reviewId"] == *review_id)
                .collect()
        };
        let (requests, verdicts) = (with_review("review-request"), with_review("review-verdict"));
        assert_eq!(requests.len(), 1);
        assert_eq!(verdicts.len(), 1);
        let evidence_id = &evidence["evidenceId"];
        assert!(
            requests[0]["requiredEvidenceIds"]
                .as_array()
                .unwrap()
                .contains(evidence_id)
        );
        assert_match(
            verdicts[0],
            &json!({ "reviewRequestId": requests[0]["reviewRequestId"], "target": requests[0]["target"], "reviewerAgentId": requests[0]["reviewerAgentId"], "verdict": "PASS" }),
        );
        assert!(
            verdicts[0]["reviewedEvidenceIds"]
                .as_array()
                .unwrap()
                .contains(evidence_id)
        );
    }
}

// ---- proposal grounding ----

#[test]
fn shows_the_proposer_the_in_scope_files_as_they_are_at_the_parent_commit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    initialize_repository(root);
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("src").join("calibrate.mjs"),
        "export const CALIBRATORS = { none: (s) => s };\n",
    )
    .unwrap();
    std::fs::write(root.join("config.json"), "{ \"calibration\": \"none\" }\n").unwrap();
    std::fs::write(root.join("evaluate.mjs"), "// frozen\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "scope fixture"]);
    let parent = git(root, &["rev-parse", "HEAD"]);
    // Later edits on the working tree are not what the variant starts from.
    std::fs::write(
        root.join("config.json"),
        "{ \"calibration\": \"isotonic\" }\n",
    )
    .unwrap();
    let snapshot = scope_snapshot(
        root,
        &parent,
        &["./src/**".to_owned(), "config.json".to_owned()],
        8,
        6_000,
    )
    .join("\n");
    assert!(snapshot.contains("--- src/calibrate.mjs"));
    assert!(snapshot.contains("none: (s) => s"));
    assert!(snapshot.contains("\"calibration\": \"none\""));
    assert!(!snapshot.contains("isotonic"));
    assert!(!snapshot.contains("evaluate.mjs"));
}

// ---- Autoresearch hill climbing ----

#[test]
fn climbs_greedily_from_the_current_best_reviewing_only_the_keeps_run_by_one_dedicated_autoresearcher()
 {
    local(async {
        let run = run_autoresearch(
            climb_contract(6),
            RunOptions {
                agents: AgentOptions {
                    scores: vec![8, 9, 9, 9, 9, 6, 7, 4],
                    ..AgentOptions::default()
                },
                maximum_experiments: 8,
                maximum_rounds: 8,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.errors, Vec::<Value>::new());
        let results = latest_results(&run.records);
        assert_eq!(results.len(), 8);
        // Each keep descends from the previous best: 8, then 6 (after four discards on the same
        // parent), then 4.
        let promoted: Vec<&Value> = results
            .iter()
            .filter(|result| result["promotionDecision"] == "promoted")
            .collect();
        let scores: Vec<&Value> = promoted
            .iter()
            .map(|result| &result["comparison"]["candidateScore"])
            .collect();
        assert_eq!(scores, [&json!(8), &json!(6), &json!(4)]);
        assert_eq!(
            promoted[1]["parentExperimentId"],
            promoted[0]["experimentId"]
        );
        assert_eq!(
            promoted[2]["parentExperimentId"],
            promoted[1]["experimentId"]
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result["parentExperimentId"] == promoted[0]["experimentId"])
                .count(),
            5
        );
        // Only promotion candidates get an experiment Review; single-experiment rounds need no
        // round Review.
        assert_eq!(reviews_of_type(&run.records, "experiment"), 3);
        assert_eq!(reviews_of_type(&run.records, "experiment_round"), 0);
        // One autoresearcher runs every proposal and implementation; it is never a director, and
        // every reviewer is someone else.
        let started = run.lab.agents.started.borrow();
        let workers: Vec<&SessionOptions> = started
            .iter()
            .filter(|session| session.role == Role::GeneralWorker)
            .collect();
        let reviewers: Vec<&SessionOptions> = started
            .iter()
            .filter(|session| session.role == Role::Reviewer)
            .collect();
        let distinct = |sessions: &[&SessionOptions]| {
            sessions
                .iter()
                .map(|session| session.agent_id.clone())
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        assert_eq!(distinct(&workers), 1);
        assert!(
            !started.iter().any(|session| matches!(
                session.role,
                Role::MissionDirector | Role::ResearchDirector
            ))
        );
        assert!(
            reviewers
                .iter()
                .all(|session| session.agent_id != workers[0].agent_id)
        );
        assert_eq!(distinct(&reviewers), reviewers.len());
        // The autoresearcher reads a log of every experiment with its outcome.
        let prompts = run.lab.agents.proposal_prompts.borrow();
        let last_brief = prompts.last().unwrap();
        for expected in ["keep", "discard", "consecutive"] {
            assert!(last_brief.contains(expected), "{expected}");
        }
    });
}

#[test]
fn shows_the_keep_review_the_variants_code_change_and_tells_the_autoresearcher_when_a_review_stopped_a_keep()
 {
    // Live: the keep Review was asked to confirm how a calibrator was fit but saw only metrics,
    // so every real improvement came back INCONCLUSIVE.
    local(async {
        let run = run_autoresearch(
            climb_contract(3),
            RunOptions {
                agents: AgentOptions {
                    scores: vec![8, 9],
                    transform_review: inconclusive("experiment"),
                    ..AgentOptions::default()
                },
                maximum_experiments: 2,
                maximum_rounds: 2,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.errors, Vec::<Value>::new());
        let reviews = run.lab.agents.review_prompts.borrow();
        let keep_review = reviews
            .iter()
            .find(|prompt| prompt.contains(r#""reviewType":"experiment""#))
            .unwrap();
        assert!(keep_review.contains(r#"+{"score":8}"#));
        let request = of_schema(&run.records, "review-request")
            .into_iter()
            .find(|request| request["reviewType"] == "experiment")
            .unwrap();
        assert!(
            request["criteria"][0]["statement"]
                .as_str()
                .unwrap()
                .contains("code diff")
        );
        assert!(run.lab.agents.proposal_prompts.borrow()[1].contains("discard (review REVISE)"));
    });
}

#[test]
fn logs_implementation_crashes_for_the_autoresearcher_without_spending_the_experiment_budget() {
    // Two crashes, below the crash cap (one per allowed experiment): all three experiments are
    // still evaluated.
    local(async {
        let run = run_autoresearch(
            climb_contract(3),
            RunOptions {
                agents: AgentOptions {
                    scores: vec![8, 6, 4],
                    fail_implementations: 2,
                    ..AgentOptions::default()
                },
                maximum_experiments: 3,
                maximum_rounds: 3,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_match(
            &run.budget,
            &json!({ "experiments": 3, "implementationFailures": 2 }),
        );
        assert!(
            run.lab.agents.proposal_prompts.borrow()[1..]
                .iter()
                .any(|prompt| prompt.contains("crash"))
        );
        // Live: a round with a crash and its retry evaluated one experiment, yet still spent a
        // round Review.
        assert_eq!(reviews_of_type(&run.records, "experiment_round"), 0);
    });
}

// ---- AutoresearchSupervisor ----

#[test]
fn recovers_a_two_round_frozen_direction_experiment_lineage_with_jobs_evidence_and_reviewed_promotion()
 {
    local(async {
        let lab = Lab::new(true, AgentOptions::default());
        let project_id = lab.project_id.clone();
        let research = lab.research();
        let evaluation_contract = json!({ "primaryMetric": { "name": "score", "objective": "minimize", "minimumEffect": 0.5 }, "baselineMetrics": { "score": 4 }, "roundWidths": [3, 1], "execution": { "runner": "native", "command": node_command("const fs=require('fs');const v=JSON.parse(fs.readFileSync('variant.json','utf8'));fs.writeFileSync('metrics.json',JSON.stringify({score:v.score}));"), "resultPath": "metrics.json", "timeoutSeconds": 30, "usesGpu": false } });
        let mut direction = research
            .create_direction(
                &project_id,
                &DirectionInput {
                    question: "Which bounded variant improves score?".into(),
                    decision_use: "Select the reviewed winner".into(),
                    mission_id: None,
                    evaluation_contract: Some(evaluation_contract),
                    idempotency_key: "direction-create".into(),
                },
                None,
            )
            .unwrap();
        let direction_id = direction["entityId"].as_str().unwrap().to_owned();
        let version = |projection: &Value| projection["version"].as_i64().unwrap();
        for next in ["proposed", "active"] {
            direction = research
                .transition_direction(
                    &project_id,
                    &direction_id,
                    version(&direction),
                    next,
                    &format!("direction-{next}"),
                )
                .unwrap();
        }
        let review_id = create_id(IdPrefix::Rev);
        let baseline_task = create_id(IdPrefix::Tsk);
        let reviewer_id = create_id(IdPrefix::Agt);
        let producer_id = create_id(IdPrefix::Agt);
        let review_request_id = format!("request_{}", &review_id[4..]);
        let baseline_node = direction["value"]["nodes"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let expires = nosh_wire::iso_from_ms(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as f64
                + 60_000.0,
        )
        .unwrap();
        let lease = json!({ "leaseId": baseline_task, "ownerId": producer_id, "version": direction["version"], "expiresAt": expires });
        direction = research
            .transition_direction_node(
                &project_id,
                &direction_id,
                version(&direction),
                &baseline_node,
                "leased",
                "baseline-leased",
                Some(&lease),
                false,
            )
            .unwrap();
        for state in ["working", "postflight", "reviewing"] {
            direction = research
                .transition_direction_node(
                    &project_id,
                    &direction_id,
                    version(&direction),
                    &baseline_node,
                    state,
                    &format!("baseline-{state}"),
                    None,
                    false,
                )
                .unwrap();
        }
        let target = json!({ "targetType": "graph_node", "targetId": baseline_node, "targetVersion": direction["version"] });
        let scope = json!({ "projectId": project_id, "missionId": null, "directionId": direction_id, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": reviewer_id });
        let correlation = format!("task:{baseline_task}");
        research
            .submit_daemon_record(
                &project_id,
                &scope,
                &correlation,
                &json!({ "$schema": schema_uri("review-request"), "schemaVersion": 1, "reviewRequestId": review_request_id, "reviewId": review_id, "reviewType": "task", "target": target, "scope": { "projectId": project_id, "missionId": null, "directionId": direction_id, "autoresearchId": null }, "producerAgentIds": [producer_id], "reviewerAgentId": reviewer_id, "independenceCheck": "pass", "contractRefs": [{ "kind": "contract_evaluation", "id": format!("contract_{}", &direction_id[4..]), "hash": direction["value"]["evaluationContractHash"] }], "requiredArtifactIds": [], "requiredEvidenceIds": [], "deterministicPostflight": { "status": "pass", "validatorRunIds": ["validator_direction.baseline"] }, "criteria": [{ "criterionId": baseline_node, "statement": "Validate the immutable baseline", "required": true, "severityIfFailed": "blocking" }], "allowedVerdicts": ["PASS", "REVISE", "REDESIGN", "BLOCKED"], "issuedAt": now_utc() }),
                &format!("baseline-review-request:{review_id}"),
            )
            .unwrap();
        research
            .submit_daemon_record(
                &project_id,
                &scope,
                &correlation,
                &json!({ "$schema": schema_uri("review-verdict"), "schemaVersion": 1, "templateVersion": "1.0.0", "reviewId": review_id, "reviewRequestId": review_request_id, "reviewType": "task", "target": target, "reviewerAgentId": reviewer_id, "independenceCheck": "pass", "verdict": "PASS", "summary": "Frozen baseline passed", "criteria": [{ "criterionId": baseline_node, "required": true, "status": "PASS", "finding": "The baseline commit and contract resolve", "evidenceRefs": [], "confidence": "high", "defectIds": [] }], "defects": [], "missingRequiredInputs": [], "scientificIntegrityFlags": [], "recommendedGraphAction": "action_accept", "recommendedPromotion": "not_applicable", "reviewedArtifactIds": [], "reviewedEvidenceIds": [], "submittedAt": now_utc() }),
                &format!("baseline-review-verdict:{review_id}"),
            )
            .unwrap();
        direction = research
            .transition_direction_node(
                &project_id,
                &direction_id,
                version(&direction),
                &baseline_node,
                "accepted",
                "baseline-node-accepted",
                None,
                false,
            )
            .unwrap();
        direction = research
            .accept_direction_baseline(
                &project_id,
                &direction_id,
                version(&direction),
                &BaselineInput {
                    commit: git(&lab.repository, &["rev-parse", "HEAD"]),
                    review_id,
                    evaluation_contract_hash: direction["value"]["evaluationContractHash"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    idempotency_key: "baseline-accepted".into(),
                },
            )
            .unwrap();
        let execution = lab.create_autoresearch(json!({ "decisionQuestion": direction["value"]["question"], "directionId": direction_id, "familyTags": ["fixture"], "scope": ["variant.json"], "maximumExperiments": 4, "maximumRounds": 2, "idempotencyKey": "autoresearch-create" }));
        let id = execution["entityId"].as_str().unwrap().to_owned();
        let mut restarted = false;
        for _ in 0..80 {
            if lab.execution(&id)["state"] == "completed" {
                break;
            }
            lab.supervisor().tick().await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
            lab.wait_for_jobs().await;
            if !restarted && lab.execution(&id)["value"]["currentRound"].as_i64() >= Some(1) {
                lab.reopen();
                restarted = true;
            }
        }
        let completed = lab.execution(&id);
        let records = lab.records(&id);
        assert_eq!(
            json!({ "state": completed["state"], "errors": lab.events(&id, "autoresearch.supervisor_error") }),
            json!({ "state": "completed", "errors": [] })
        );
        assert_eq!(completed["value"]["currentRound"], 2);
        let frontier = completed["value"]["acceptedFrontierExperimentIds"]
            .as_array()
            .unwrap();
        assert_eq!(frontier.len(), 1);
        assert_eq!(latest_results(&records).len(), 4);
        let first_round = of_schema(&records, "autoresearch-round")
            .into_iter()
            .find(|round| round["round"] == 1)
            .unwrap();
        let first_winner = of_schema(&records, "experiment-result")
            .into_iter()
            .find(|result| result["experimentId"] == first_round["promotedExperimentIds"][0])
            .unwrap();
        assert_eq!(first_winner["comparison"]["candidateScore"], 2);
        assert_eq!(of_schema(&records, "autoresearch-round").len(), 2);
        assert_eq!(of_schema(&records, "job-result").len(), 4);
        assert!(
            of_schema(&records, "evidence")
                .iter()
                .any(|evidence| evidence["polarity"] == "contradicts")
        );
        assert_eq!(of_schema(&records, "experiment-manifest").len(), 4);
        assert_eq!(
            lab.store()
                .operation_intents(&project_id, Some("pending"))
                .unwrap(),
            Vec::<Value>::new()
        );
        assert!(restarted);
        expect_completion_authority(&records, &completed);
        expect_reviewed_evidence_authority(&records);
        assert_eq!(
            first(&records, "autoresearch-completion-packet")["bestExperimentId"],
            frontier[0]
        );
    });
}

#[test]
fn rebuilds_model_token_use_and_blocks_before_issuing_work_when_its_budget_is_exhausted() {
    local(async {
        let lab = Lab::new(false, AgentOptions::default());
        let contract = json!({ "primaryMetric": { "name": "score", "objective": "maximize", "minimumEffect": 1 }, "baselineMetrics": { "score": 0 }, "execution": { "runner": "native", "command": node_command("process.exit(0)"), "resultPath": "metrics.json", "timeoutSeconds": 30 } });
        let execution = lab.create_autoresearch(json!({ "decisionQuestion": "Honor the recovered token ceiling", "evaluationContract": contract, "idempotencyKey": "budget-create" }));
        let id = execution["entityId"].as_str().unwrap().to_owned();
        let ceiling = execution["value"]["maximumModelTokens"].clone();
        lab.store()
            .append(&json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "agent.completed", "source": "fixture", "scope": { "projectId": lab.project_id, "missionId": null, "directionId": null, "autoresearchId": id, "experimentId": null, "runId": null, "jobId": null, "agentId": create_id(IdPrefix::Agt) }, "correlationId": id, "causationId": null, "payload": { "modelTokens": ceiling } }))
            .unwrap();
        lab.supervisor().tick().await.unwrap();
        assert_eq!(lab.execution(&id)["state"], "blocked");
        assert_eq!(
            lab.research()
                .autoresearch_budget_use(&lab.project_id, &id)
                .unwrap()["modelTokens"],
            ceiling
        );
        assert_eq!(
            lab.research()
                .records(&lab.project_id, Some("experiment-proposal"))
                .unwrap(),
            Vec::<Value>::new()
        );
        // Rust-only: blocked by the budget itself, not by the missing repository failing the
        // first experiment.
        assert_eq!(
            lab.events(&id, "autoresearch.supervisor_error"),
            Vec::<Value>::new()
        );
        assert_eq!(
            lab.events(&id, "autoresearch.budget_exhausted")[0]["exhausted"],
            json!(["model_tokens"])
        );
    });
}

// Rust-only: the TypeScript suite never exercises these decisions (found by mutation testing).

#[test]
fn discards_an_improvement_smaller_than_the_contracts_minimum_effect() {
    local(async {
        let mut contract = target_contract(json!({ "maximumNoProgressRounds": 1 }), &[1]);
        contract["execution"]["command"] =
            node_command(r#"require('fs').writeFileSync('metrics.json','{"score":3.5}')"#);
        let run = run_autoresearch(contract, RunOptions::default()).await;
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_match(
            first(&run.records, "experiment-result"),
            &json!({ "deterministicValidation": "pass", "comparison": { "improvement": 0.5, "minimumEffect": 1 }, "promotionDecision": "rejected" }),
        );
        assert_eq!(reviews_of_type(&run.records, "experiment"), 0);
        assert_eq!(
            run.execution["value"]["acceptedFrontierExperimentIds"],
            json!([])
        );
    });
}

#[test]
fn counts_only_consecutive_proposal_failures_toward_the_cap() {
    // Four rejected proposals in all, never three in a row.
    local(async {
        let run = run_autoresearch(
            target_contract(json!({ "maximumNoProgressRounds": 3 }), &[1, 1]),
            RunOptions {
                agents: AgentOptions {
                    fail_proposals: 2,
                    refail_proposals: 2,
                    ..AgentOptions::default()
                },
                maximum_experiments: 2,
                maximum_rounds: 2,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_eq!(run.proposal_failures, 4);
        assert_eq!(run.execution["state"], "completed");
        assert_eq!(latest_results(&run.records).len(), 2);
    });
}

#[test]
fn blocks_when_implementation_crashes_reach_one_per_allowed_experiment() {
    local(async {
        let run = run_autoresearch(
            climb_contract(3),
            RunOptions {
                agents: AgentOptions {
                    fail_implementations: 2,
                    ..AgentOptions::default()
                },
                maximum_experiments: 2,
                maximum_rounds: 2,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_eq!(run.execution["state"], "blocked");
        assert_match(
            &run.budget,
            &json!({ "experiments": 0, "implementationFailures": 2 }),
        );
        let exhausted = run.lab.events(
            run.execution["entityId"].as_str().unwrap(),
            "autoresearch.budget_exhausted",
        );
        assert_eq!(
            exhausted[0]["exhausted"],
            json!(["implementation_failures"])
        );
    });
}

/// `persists durable negative evidence for %s`.
fn persists_durable_negative_evidence(script: &str) {
    local(async {
        let run = run_autoresearch(
            json!({ "primaryMetric": { "name": "score", "objective": "maximize", "minimumEffect": 1 }, "baselineMetrics": { "score": 0 }, "roundWidths": [1], "execution": { "runner": "native", "command": node_command(script), "resultPath": "metrics.json", "timeoutSeconds": 30, "usesGpu": false } }),
            RunOptions::default(),
        )
        .await;
        assert_eq!(run.execution["state"], "completed");
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_eq!(of_schema(&run.records, "job-result").len(), 1);
        assert_match(
            first(&run.records, "experiment-result"),
            &json!({ "runSetStatus": "invalid", "deterministicValidation": "fail", "promotionDecision": "rejected" }),
        );
        assert_match(
            first(&run.records, "evidence"),
            &json!({ "evidenceType": "evidence_negative.result", "polarity": "contradicts" }),
        );
        assert_match(
            first(&run.records, "autoresearch-completion-packet"),
            &json!({ "terminalReason": "all_hypotheses_rejected", "bestExperimentId": null }),
        );
        expect_completion_authority(&run.records, &run.execution);
    });
}

#[test]
fn persists_durable_negative_evidence_for_a_failed_job() {
    persists_durable_negative_evidence("process.exit(1)");
}

#[test]
fn persists_durable_negative_evidence_for_an_absent_result_file() {
    persists_durable_negative_evidence("process.exit(0)");
}

#[test]
fn persists_durable_negative_evidence_for_malformed_result_json() {
    persists_durable_negative_evidence("require('fs').writeFileSync('metrics.json','{')");
}

#[test]
fn persists_durable_negative_evidence_for_a_result_without_the_primary_metric() {
    persists_durable_negative_evidence("require('fs').writeFileSync('metrics.json','{}')");
}

#[test]
fn persists_durable_negative_evidence_for_a_non_finite_primary_metric() {
    persists_durable_negative_evidence(
        r#"require('fs').writeFileSync('metrics.json','{"score":"NaN"}')"#,
    );
}

#[test]
fn retries_a_malformed_proposal_with_a_fresh_one_and_blocks_only_after_consecutive_failures() {
    local(async {
        let recovered = run_autoresearch(
            target_contract(json!({ "target": 2 }), &[1]),
            RunOptions {
                agents: AgentOptions {
                    fail_proposals: 2,
                    ..AgentOptions::default()
                },
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(recovered.errors, Vec::<Value>::new());
        assert_eq!(recovered.proposal_failures, 2);
        assert_eq!(recovered.execution["state"], "completed");
        let exhausted = run_autoresearch(
            target_contract(json!({ "target": 2 }), &[1]),
            RunOptions {
                agents: AgentOptions {
                    fail_proposals: 3,
                    ..AgentOptions::default()
                },
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(exhausted.proposal_failures, 3);
        assert_eq!(exhausted.execution["state"], "blocked");
        assert_eq!(
            exhausted.errors,
            [json!({ "message": "No valid experiment proposal after 3 consecutive attempts" })]
        );
    });
}

#[test]
fn rejects_an_improving_experiment_that_violates_a_contract_guardrail() {
    local(async {
        let run = run_autoresearch(guarded_contract(json!({})), RunOptions::default()).await;
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_match(
            first(&run.records, "experiment-result"),
            &json!({ "deterministicValidation": "pass", "guardrails": [{ "metric": "guardrail_pass", "value": false, "passed": false }], "promotionDecision": "rejected" }),
        );
        let evidence = first(&run.records, "evidence");
        assert_eq!(evidence["evidenceType"], "evidence_negative.result");
        assert!(
            evidence["statement"]
                .as_str()
                .unwrap()
                .contains("violated contract guardrails: guardrail_pass=false violates >= 1")
        );
        // The experiment cannot write the frozen evaluation's files: its program (none here) and
        // its result file.
        let packet = of_schema(&run.records, "task-packet")
            .into_iter()
            .find(|packet| packet["taskType"] == "task_experiment.implementation")
            .unwrap();
        let protected = packet["workspace"]["protectedScopes"].as_array().unwrap();
        assert!(protected.contains(&json!(".nosh/**")));
        assert!(protected.contains(&json!("metrics.json")));
    });
}

#[test]
fn branches_the_next_round_from_the_root_when_the_previous_round_promoted_nothing() {
    local(async {
        let run = run_autoresearch(
            guarded_contract(
                json!({ "roundWidths": [1, 1], "stopConditions": { "maximumNoProgressRounds": 3 } }),
            ),
            RunOptions {
                maximum_experiments: 2,
                maximum_rounds: 2,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.errors, Vec::<Value>::new());
        let results = of_schema(&run.records, "experiment-result");
        assert_eq!(results.len(), 2);
        // Round 2 has no accepted frontier, so it starts again from the root: one level deep, not
        // two.
        assert_eq!(
            results[0]["parentExperimentId"],
            results[1]["parentExperimentId"]
        );
    });
}

#[test]
fn records_a_discarded_experiments_negative_evidence_without_spending_a_review_session_on_it() {
    local(async {
        let run = run_autoresearch(guarded_contract(json!({})), RunOptions::default()).await;
        // A discard is a measured negative: no experiment Review runs, and the closure Review
        // judges it with the rest.
        assert_eq!(reviews_of_type(&run.records, "experiment"), 0);
        assert_match(
            first(&run.records, "evidence"),
            &json!({ "evidenceType": "evidence_negative.result", "quality": { "status": "unreviewed", "reviewId": null } }),
        );
        assert_match(
            first(&run.records, "experiment-result"),
            &json!({ "promotionRecommendation": "reject", "promotionDecision": "rejected" }),
        );
    });
}

#[test]
fn stops_after_a_reviewed_winner_reaches_the_frozen_target() {
    local(async {
        let run = run_autoresearch(
            target_contract(json!({ "target": 2 }), &[1]),
            RunOptions::default(),
        )
        .await;
        assert_eq!(run.execution["state"], "completed");
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_eq!(of_schema(&run.records, "experiment-result").len(), 2);
        assert_eq!(
            first(&run.records, "autoresearch-completion-packet")["terminalReason"],
            "target_reached"
        );
        expect_completion_authority(&run.records, &run.execution);
        expect_reviewed_evidence_authority(&run.records);
        let candidates = run.lab.agents.reviewed_evidence_candidates.borrow();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        let evidence = of_schema(&run.records, "evidence")
            .into_iter()
            .find(|evidence| evidence["evidenceId"] == candidate["evidenceId"])
            .unwrap();
        let mut expected = candidate.clone();
        expected["quality"]["status"] = json!("reviewed");
        expected["quality"]["reviewId"] = evidence["quality"]["reviewId"].clone();
        assert_eq!(*evidence, expected);
    });
}

/// `does not mark Evidence reviewed or promote with %s`.
fn does_not_mark_evidence_reviewed_or_promote(edit: impl Fn(Value) -> Value + 'static) {
    local(async {
        let run = run_autoresearch(
            target_contract(json!({ "target": 2 }), &[1]),
            RunOptions {
                agents: AgentOptions {
                    transform_review: transform(move |record| {
                        if record["reviewType"] == "experiment" {
                            edit(record)
                        } else {
                            record
                        }
                    }),
                    ..AgentOptions::default()
                },
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.execution["state"], "completed");
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_eq!(
            run.execution["value"]["acceptedFrontierExperimentIds"],
            json!([])
        );
        assert_match(
            first(&run.records, "evidence"),
            &json!({ "quality": { "status": "unreviewed", "reviewId": null } }),
        );
        assert_match(
            first(&run.records, "experiment-result"),
            &json!({ "promotionRecommendation": "reject", "promotionDecision": "rejected" }),
        );
        assert_match(
            first(&run.records, "autoresearch-completion-packet"),
            &json!({ "terminalReason": "all_hypotheses_rejected", "bestExperimentId": null }),
        );
        expect_completion_authority(&run.records, &run.execution);
    });
}

#[test]
fn does_not_mark_evidence_reviewed_or_promote_with_missing_evidence_coverage() {
    does_not_mark_evidence_reviewed_or_promote(|mut record| {
        record["reviewedEvidenceIds"] = json!([]);
        record
    });
}

#[test]
fn does_not_mark_evidence_reviewed_or_promote_with_a_stale_target_version() {
    does_not_mark_evidence_reviewed_or_promote(|mut record| {
        record["target"]["targetVersion"] = json!(2);
        record
    });
}

#[test]
fn does_not_mark_evidence_reviewed_or_promote_with_a_non_pass_verdict() {
    does_not_mark_evidence_reviewed_or_promote(|mut record| {
        record["verdict"] = json!("REVISE");
        record["recommendedPromotion"] = json!("hold");
        record
    });
}

#[test]
fn does_not_report_target_reached_when_the_round_review_holds_the_winner() {
    // Two siblings in one round: the round Review decides between them (single-experiment rounds
    // need none).
    local(async {
        let run = run_autoresearch(
            target_contract(json!({ "target": 2, "maximumNoProgressRounds": 2 }), &[2]),
            RunOptions {
                agents: AgentOptions {
                    transform_review: transform(|mut record| {
                        if record["reviewType"] == "experiment_round" {
                            record["verdict"] = json!("REVISE");
                            record["recommendedPromotion"] = json!("hold");
                        }
                        record
                    }),
                    ..AgentOptions::default()
                },
                maximum_experiments: 2,
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.execution["state"], "completed");
        assert_eq!(run.errors, Vec::<Value>::new());
        assert_eq!(
            run.execution["value"]["acceptedFrontierExperimentIds"],
            json!([])
        );
        assert_match(
            first(&run.records, "autoresearch-completion-packet"),
            &json!({ "terminalReason": "budget_exhausted", "bestExperimentId": null }),
        );
        expect_completion_authority(&run.records, &run.execution);
    });
}

#[test]
fn says_when_an_experiment_met_the_keep_rule_but_its_experiment_review_stopped_it() {
    local(async {
        let run = run_autoresearch(
            target_contract(json!({ "target": 2, "maximumNoProgressRounds": 1 }), &[1]),
            RunOptions {
                agents: AgentOptions {
                    transform_review: inconclusive("experiment"),
                    ..AgentOptions::default()
                },
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.execution["state"], "completed");
        let packet = first(&run.records, "autoresearch-completion-packet");
        assert_eq!(packet["bestExperimentId"], Value::Null);
        let answer = Regex::new(r"^Experiments exp_[0-9a-f]+ \(experiment Review REVISE\).* met the keep rule on daemon-measured metrics, but their experiment Review did not pass").unwrap();
        assert!(
            answer.is_match(packet["decisionAnswer"].as_str().unwrap()),
            "{}",
            packet["decisionAnswer"]
        );
    });
}

#[test]
fn finishes_a_round_interrupted_after_its_experiments_when_the_run_is_resumed() {
    // E.g. a failed promotion.
    local(async {
        let interrupted = Rc::new(Cell::new(false));
        let seen = Rc::clone(&interrupted);
        let run = run_autoresearch(
            target_contract(json!({ "target": 2 }), &[2]),
            RunOptions {
                agents: AgentOptions {
                    transform_review: Box::new(move |record| {
                        if record["reviewType"] == "experiment_round" && !seen.get() {
                            seen.set(true);
                            return Err("round interrupted".into());
                        }
                        Ok(record)
                    }),
                    ..AgentOptions::default()
                },
                maximum_experiments: 2,
                resumes: 1,
                ..RunOptions::default()
            },
        )
        .await;
        assert!(interrupted.get());
        assert_eq!(run.execution["state"], "completed");
        let round = of_schema(&run.records, "autoresearch-round")
            .into_iter()
            .find(|round| round["round"] == 1)
            .unwrap();
        assert_eq!(round["promotedExperimentIds"].as_array().unwrap().len(), 1);
        assert_eq!(
            run.execution["value"]["acceptedFrontierExperimentIds"],
            round["promotedExperimentIds"]
        );
    });
}

/// `blocks closure with %s`.
fn blocks_closure(edit: impl Fn(Value) -> Value + 'static) {
    local(async {
        let run = run_autoresearch(
            target_contract(json!({ "target": 2 }), &[1]),
            RunOptions {
                agents: AgentOptions {
                    transform_review: transform(move |record| {
                        if record["reviewType"] == "autoresearch_closure" {
                            edit(record)
                        } else {
                            record
                        }
                    }),
                    ..AgentOptions::default()
                },
                ..RunOptions::default()
            },
        )
        .await;
        assert_eq!(run.execution["state"], "blocked");
        assert_eq!(
            run.errors,
            [json!({ "message": "Autoresearch closure Review did not pass" })]
        );
        assert!(of_schema(&run.records, "autoresearch-completion-packet").is_empty());
    });
}

#[test]
fn blocks_closure_with_a_stale_target_version() {
    blocks_closure(|mut record| {
        record["target"]["targetVersion"] = json!(1);
        record
    });
}

#[test]
fn blocks_closure_with_a_non_pass_verdict() {
    blocks_closure(|mut record| {
        record["verdict"] = json!("REVISE");
        record
    });
}
