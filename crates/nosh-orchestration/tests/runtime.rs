//! `packages/orchestration-runtime/src/runtime.test.ts`: the runtime against fake Pi sessions that
//! append the events a real turn would (started, extra tool calls, an episode-draft, completed).

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, join};
use nosh_agent::pi::{Inspection, TerminalStep};
use nosh_agent::policy::SessionOptions;
use nosh_orchestration::Agents;
use nosh_orchestration::runtime::{Assignment, HookDecision, OrchestrationRuntime, RuntimeHost};
use nosh_store::{EventStore, ProjectionInput};
use nosh_wire::{IdPrefix, create_id, now_utc, random_hex};
use regex::Regex;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;

type Runtime = OrchestrationRuntime<TestHost>;
type Draft = Box<dyn Fn(&str) -> Value>;

fn uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

struct Sessions {
    store: Rc<EventStore>,
    active: RefCell<Vec<SessionOptions>>,
    prompts: RefCell<Vec<String>>,
    delay: Cell<u64>,
    omit_draft: Cell<bool>,
    fail_message: RefCell<String>,
    extra_tools: RefCell<Vec<String>>,
    draft: RefCell<Option<Draft>>,
    /// The tools each session was started with, in start order.
    started_tools: RefCell<Vec<Option<Vec<String>>>>,
}

impl Sessions {
    fn new(store: &Rc<EventStore>) -> Rc<Sessions> {
        Rc::new(Sessions {
            store: Rc::clone(store),
            active: RefCell::default(),
            prompts: RefCell::default(),
            delay: Cell::new(0),
            omit_draft: Cell::new(false),
            fail_message: RefCell::default(),
            extra_tools: RefCell::default(),
            draft: RefCell::new(None),
            started_tools: RefCell::default(),
        })
    }

    fn last_prompt(&self) -> String {
        self.prompts.borrow().last().cloned().unwrap_or_default()
    }

    fn append(&self, session: &SessionOptions, event_type: &str, payload: Value) {
        self.store.append(&json!({ "$schema": uri("event"), "schemaVersion": 1, "retention": "persistent", "type": event_type, "source": "fixture", "scope": { "projectId": session.project_id, "missionId": session.mission_id, "directionId": session.direction_id, "autoresearchId": session.autoresearch_id, "experimentId": session.experiment_id, "runId": session.run_id, "jobId": session.job_id, "agentId": session.agent_id }, "correlationId": session.task_id, "causationId": null, "payload": payload })).unwrap();
    }
}

fn inspection(session: &SessionOptions, pi_session_id: String) -> Inspection {
    Inspection {
        project_id: session.project_id.clone(),
        mission_id: session.mission_id.clone(),
        direction_id: session.direction_id.clone(),
        autoresearch_id: session.autoresearch_id.clone(),
        experiment_id: session.experiment_id.clone(),
        run_id: session.run_id.clone(),
        job_id: session.job_id.clone(),
        task_id: session.task_id.clone(),
        agent_id: session.agent_id.clone(),
        role: session.role,
        pi_session_id,
        status: "idle".into(),
        current_tool: None,
        active_tool_ids: vec![],
        started_at: now_utc(),
        last_event_at: now_utc(),
        model_provider: None,
        model_id: None,
        model_name: None,
        thinking_level: "off".into(),
        context_tokens: None,
        context_window: None,
        context_percent: None,
    }
}

impl Agents for Sessions {
    fn start(&self, options: SessionOptions) -> LocalBoxFuture<'_, Result<Inspection, String>> {
        self.started_tools.borrow_mut().push(options.tools.clone());
        let mut active = self.active.borrow_mut();
        let value = inspection(
            &options,
            format!("pi-{}-{}", options.agent_id, active.len()),
        );
        match active
            .iter_mut()
            .find(|session| session.agent_id == options.agent_id)
        {
            Some(existing) => *existing = options,
            None => active.push(options),
        }
        async move { Ok(value) }.boxed_local()
    }

    fn prompt<'a>(
        &'a self,
        agent_id: &'a str,
        prompt: &'a str,
        _step: Option<&'a TerminalStep>,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        static TYPE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"episode-draft of type ([a-z0-9_.:-]+)").unwrap());
        async move {
            let session = self.active.borrow().iter().find(|session| session.agent_id == agent_id).cloned().expect("an active session");
            self.prompts.borrow_mut().push(prompt.to_owned());
            if self.delay.get() > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay.get())).await;
            }
            let found = TYPE.captures(prompt).map_or("episode_test".to_owned(), |found| found[1].to_owned());
            let episode_type = found.strip_suffix('.').unwrap_or(&found);
            self.append(&session, "agent.started", json!({}));
            let failure = self.fail_message.borrow().clone();
            if !failure.is_empty() {
                return Err(failure);
            }
            for tool_name in self.extra_tools.borrow().iter() {
                self.append(&session, "agent.tool_completed", json!({ "toolName": tool_name }));
            }
            if !self.omit_draft.get() {
                let draft = match &*self.draft.borrow() {
                    Some(draft) => draft(episode_type),
                    None => json!({ "$schema": uri("episode-draft"), "schemaVersion": 1, "episodeType": episode_type, "summary": format!("summary {}", session.task_id.clone().unwrap_or_default()), "facts": [], "decisions": [], "artifactIds": [], "evidenceIds": [], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [] }),
                };
                self.append(&session, "record.submitted", draft);
                self.append(&session, "agent.tool_completed", json!({ "toolName": "nosh_episode_submit" }));
            }
            self.append(&session, "agent.completed", json!({ "message": "done", "modelTokens": 12 }));
            Ok(())
        }
        .boxed_local()
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
        self.active
            .borrow_mut()
            .retain(|session| session.agent_id != agent_id);
    }

    fn inspect(&self) -> Vec<Inspection> {
        self.active
            .borrow()
            .iter()
            .map(|session| inspection(session, format!("pi-{}", session.agent_id)))
            .collect()
    }
}

struct TestHost {
    store: Rc<EventStore>,
    root: PathBuf,
    actions: Option<Rc<RefCell<Vec<String>>>>,
}

impl RuntimeHost for TestHost {
    fn store(&self, _project_id: &str) -> nosh_orchestration::Result<Rc<EventStore>> {
        Ok(Rc::clone(&self.store))
    }
    fn repository_root(&self, _project_id: &str) -> nosh_orchestration::Result<PathBuf> {
        Ok(self.root.clone())
    }
    fn package_path(&self) -> PathBuf {
        self.root.clone()
    }
    fn direct_action(
        &self,
        action: &str,
        _payload: &Value,
        _instruction: &Value,
    ) -> Option<LocalBoxFuture<'static, nosh_orchestration::Result<Value>>> {
        let actions = self.actions.as_ref()?;
        actions.borrow_mut().push(action.to_owned());
        let result = json!({ "action": action });
        Some(async move { Ok(result) }.boxed_local())
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    project_id: String,
    database: PathBuf,
    store: Rc<EventStore>,
    sessions: Rc<Sessions>,
    runtime: Rc<Runtime>,
}

fn setup() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let database = root.join("project.sqlite");
    let store = Rc::new(EventStore::open(&database).unwrap());
    let sessions = Sessions::new(&store);
    let runtime = runtime(&root, &store, &sessions, None);
    Fixture {
        _directory: directory,
        root,
        project_id: create_id(IdPrefix::Prj),
        database,
        store,
        sessions,
        runtime,
    }
}

fn runtime(
    root: &Path,
    store: &Rc<EventStore>,
    sessions: &Rc<Sessions>,
    actions: Option<Rc<RefCell<Vec<String>>>>,
) -> Rc<Runtime> {
    let host = TestHost {
        store: Rc::clone(store),
        root: root.to_path_buf(),
        actions,
    };
    OrchestrationRuntime::new(host, Rc::clone(sessions) as Rc<dyn Agents>)
}

/// Runs a test body on one thread inside a `LocalSet`, as the daemon runs the runtime.
fn local<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(tokio::task::LocalSet::new().run_until(future))
}

fn fails<T: std::fmt::Debug>(result: nosh_orchestration::Result<T>, expected: &str) {
    let message = result.expect_err("an error").to_string();
    assert!(
        message.contains(expected),
        "{message:?} does not contain {expected:?}"
    );
}

fn base(project_id: &str, operation: Value) -> Value {
    let mut instruction = json!({ "$schema": uri("runtime-instruction"), "schemaVersion": 1, "instructionId": create_id(IdPrefix::Ins), "projectId": project_id, "idempotencyKey": format!("test-{}", random_hex()), "proposedByAgentId": null, "issuedAt": now_utc() });
    for (key, value) in operation.as_object().unwrap() {
        instruction[key] = value.clone();
    }
    instruction
}

#[derive(Default)]
struct Overrides {
    skill_ids: Vec<String>,
    capabilities: Vec<&'static str>,
    budget: Option<Value>,
}

fn open(project_id: &str, purpose: &str, overrides: Overrides) -> Value {
    let budget = overrides.budget.unwrap_or_else(|| json!({ "maximumToolCalls": 20, "maximumModelTokens": 1_000, "maximumWallClockSeconds": 120 }));
    base(
        project_id,
        json!({ "operation": "THREAD_OPEN", "threadId": create_id(IdPrefix::Thr), "taskId": create_id(IdPrefix::Tsk), "initialAgentId": null, "ownerScope": { "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "graphNodeId": null }, "role": "general_worker", "purpose": purpose, "executionMode": "background", "parentThreadId": null, "inputRefs": [], "skillIds": overrides.skill_ids, "capabilities": overrides.capabilities, "budget": budget }),
    )
}

fn fork(project_id: &str, purpose: &str, controlling_thread_id: &Value) -> Value {
    let mut fork = open(project_id, purpose, Overrides::default());
    fork["operation"] = json!("THREAD_FORK");
    fork["controllingThreadId"] = controlling_thread_id.clone();
    fork["executionMode"] = json!("foreground_fork");
    fork["parentThreadId"] = Value::Null;
    fork
}

fn step(project_id: &str, thread_id: &Value, objective: &str) -> Value {
    base(
        project_id,
        json!({ "operation": "THREAD_STEP", "threadId": thread_id, "objective": objective, "expectedEpisodeType": "episode_test", "inputRefs": [], "skillIds": [] }),
    )
}

fn skill(permitted_tools: &[&str]) -> Value {
    json!({ "$schema": uri("skill-manifest"), "schemaVersion": 1, "skillId": create_id(IdPrefix::Skl), "name": format!("skill_{}", random_hex()), "version": "1.0.0", "description": "Adversarial policy fixture", "activation": { "roles": ["general_worker"], "requiredCapabilities": [], "episodeTypes": ["episode_test"] }, "promptFragment": "Apply the policy fixture.", "permittedTools": permitted_tools, "inputEpisodeTypes": [], "outputEpisodeType": "episode_test", "executionMode": "prompt", "programId": null, "preflightChecks": [], "postflightChecks": [] })
}

fn program(
    program_id: &str,
    name: &str,
    start: &str,
    state: Value,
    steps: Value,
    maximum_model_tokens: u64,
) -> Value {
    json!({ "$schema": uri("orchestration-program"), "schemaVersion": 1, "programId": program_id, "name": name, "version": "1.0.0", "startStepId": start, "maximumSteps": 2, "maximumWallClockSeconds": 30, "maximumModelTokens": maximum_model_tokens, "maximumToolCalls": 20, "state": state, "steps": steps })
}

fn only_step(step_id: &str, instruction: Value, background: bool) -> Value {
    json!([{ "stepId": step_id, "instruction": instruction, "guard": null, "background": background, "outputStateKey": null, "nextStepId": null, "failureStepId": null }])
}

fn empty_state(namespace: &str) -> Value {
    json!({ "namespace": namespace, "schema": {}, "initial": {} })
}

fn key(prefix: &str) -> String {
    format!("{prefix}-{}", random_hex())
}

fn event_scope(project_id: &str, job_id: Value, agent_id: Value) -> Value {
    json!({ "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": job_id, "agentId": agent_id })
}

#[test]
fn episodes_may_cite_host_issued_job_ids_but_not_invented_ones() {
    let fixture = setup();
    let job_id = create_id(IdPrefix::Job);
    fixture.store.append(&json!({ "$schema": uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "task.command_run", "source": "noshd", "scope": event_scope(&fixture.project_id, json!(job_id), Value::Null), "correlationId": "task:x", "causationId": null, "payload": { "jobId": job_id, "exitCode": 0 } })).unwrap();
    let draft = json!({ "$schema": uri("episode-draft"), "schemaVersion": 1, "episodeType": "episode_general-worker", "summary": "s", "facts": [{ "statement": "tests pass", "evidenceRefs": [job_id], "confidence": "high" }], "decisions": [], "artifactIds": [], "evidenceIds": [], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [] });
    fixture
        .runtime
        .validate_draft(&fixture.project_id, &draft, None)
        .unwrap();
    let mut invented = draft.clone();
    invented["facts"][0]["evidenceRefs"] = json!([create_id(IdPrefix::Job)]);
    fails(
        fixture
            .runtime
            .validate_draft(&fixture.project_id, &invented, None),
        "unknown record",
    );
}

#[test]
fn a_reviewers_episode_may_cite_the_candidate_its_review_request_names_before_that_candidate_is_published()
 {
    let fixture = setup();
    let reviewer = create_id(IdPrefix::Agt);
    let candidate = create_id(IdPrefix::Evd);
    fixture.store.append(&json!({ "$schema": uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "record.submitted", "source": "noshd", "scope": event_scope(&fixture.project_id, Value::Null, json!(reviewer)), "correlationId": "review:x", "causationId": null, "payload": { "$schema": uri("review-request"), "reviewerAgentId": reviewer, "requiredEvidenceIds": [candidate], "requiredArtifactIds": [] } })).unwrap();
    let draft = json!({ "$schema": uri("episode-draft"), "schemaVersion": 1, "episodeType": "episode_reviewer", "summary": "s", "facts": [{ "statement": "metrics match", "evidenceRefs": [candidate], "confidence": "high" }], "decisions": [], "artifactIds": [], "evidenceIds": [candidate], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [] });
    fixture
        .runtime
        .validate_draft(&fixture.project_id, &draft, Some(&reviewer))
        .unwrap();
    // Only that reviewer, and only the IDs its request names.
    fails(
        fixture.runtime.validate_draft(
            &fixture.project_id,
            &draft,
            Some(&create_id(IdPrefix::Agt)),
        ),
        "unknown record",
    );
    fails(
        fixture
            .runtime
            .validate_draft(&fixture.project_id, &draft, None),
        "unknown record",
    );
    let mut other = draft.clone();
    other["evidenceIds"] = json!([create_id(IdPrefix::Evd)]);
    fails(
        fixture
            .runtime
            .validate_draft(&fixture.project_id, &other, Some(&reviewer)),
        "unknown record",
    );
}

#[test]
fn runs_parallel_threads_awaits_episodes_composes_context_and_survives_pi_rotation_and_restart() {
    local(async {
        let Fixture {
            _directory,
            root,
            project_id,
            database,
            store,
            sessions,
            runtime: rt,
        } = setup();
        let first = open(&project_id, "first", Overrides::default());
        let second = open(&project_id, "second", Overrides::default());
        let (a, b) = join(rt.execute(&first), rt.execute(&second)).await;
        a.unwrap();
        b.unwrap();
        sessions.delay.set(20);
        let step_one = rt.execute(&step(&project_id, &first["threadId"], "one"));
        let step_two = rt.execute(&step(&project_id, &second["threadId"], "two"));
        let awaited = rt.execute(&base(&project_id, json!({ "operation": "THREAD_AWAIT", "threadIds": [first["threadId"], second["threadId"]], "policy": "all", "minimumCount": null, "deadline": null }))).await.unwrap();
        step_one.await.unwrap();
        step_two.await.unwrap();
        assert_eq!(awaited["episodeIds"].as_array().unwrap().len(), 2);
        let episodes = rt.episodes(&project_id).unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(
            episodes
                .iter()
                .map(|entry| entry["value"]["usage"]["modelTokens"].clone())
                .collect::<Vec<_>>(),
            [json!(12), json!(12)]
        );
        assert_eq!(
            episodes
                .iter()
                .map(|entry| entry["value"]["usage"]["toolCalls"].clone())
                .collect::<Vec<_>>(),
            [json!(1), json!(1)]
        );
        let trace = rt
            .trace(&project_id, episodes[0]["entityId"].as_str().unwrap())
            .unwrap();
        assert!(trace.iter().any(|event| event["type"] == "agent.completed"));
        let from_second = episodes
            .iter()
            .find(|entry| entry["value"]["threadId"] == second["threadId"])
            .unwrap();
        rt.execute(&base(&project_id, json!({ "operation": "THREAD_COMPOSE", "targetThreadId": first["threadId"], "episodeIds": [from_second["entityId"]] }))).await.unwrap();
        rt.execute(&step(&project_id, &first["threadId"], "composed"))
            .await
            .unwrap();
        assert!(sessions.last_prompt().contains("Selected prior episodes"));
        drop((rt, sessions, store));
        let store = Rc::new(EventStore::open(&database).unwrap());
        let sessions = Sessions::new(&store);
        let restarted = runtime(&root, &store, &sessions, None);
        let rotated = restarted
            .rotate(
                &project_id,
                first["threadId"].as_str().unwrap(),
                &key("rotate"),
            )
            .await
            .unwrap();
        assert_eq!(rotated["sessionHistory"].as_array().unwrap().len(), 2);
        assert_eq!(restarted.episodes(&project_id).unwrap().len(), 3);
    });
}

#[test]
fn applies_dynamic_skills_bounded_programs_foreground_forks_and_causal_suppression_hooks() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let skill_id = create_id(IdPrefix::Skl);
        rt.register_skill(project_id, &json!({ "$schema": uri("skill-manifest"), "schemaVersion": 1, "skillId": skill_id, "name": "skill_check", "version": "1.0.0", "description": "Check a focused output", "activation": { "roles": ["general_worker"], "requiredCapabilities": ["action_write"], "episodeTypes": [] }, "promptFragment": "Follow the dynamic skill checklist.", "permittedTools": ["nosh_episode_submit"], "inputEpisodeTypes": [], "outputEpisodeType": "episode_test", "executionMode": "prompt", "programId": null, "preflightChecks": ["check_budget"], "postflightChecks": ["check_episode"] }), &key("skill")).unwrap();
        let opened = open(
            project_id,
            "skilled",
            Overrides {
                skill_ids: vec![skill_id.clone()],
                capabilities: vec!["action_write"],
                budget: None,
            },
        );
        rt.execute(&opened).await.unwrap();
        rt.execute(&step(project_id, &opened["threadId"], "use skill"))
            .await
            .unwrap();
        assert!(
            fixture
                .sessions
                .last_prompt()
                .contains("Follow the dynamic skill checklist")
        );
        let thread_id = opened["threadId"].as_str().unwrap();
        assert_eq!(
            rt.thread(project_id, thread_id).unwrap()["value"]["activeSkillIds"],
            json!([])
        );

        let program_id = create_id(IdPrefix::Prg);
        let pause = json!({ "operation": "THREAD_PAUSE", "threadId": thread_id, "reason": "program pause" });
        let steps = json!([{ "stepId": "step_pause", "instruction": pause, "guard": { "ref": "paused", "operator": "equals", "value": null }, "background": false, "outputStateKey": "paused", "nextStepId": null, "failureStepId": null }]);
        rt.register_program(project_id, &program(&program_id, "program_pause", "step_pause", json!({ "namespace": "state_pause", "schema": { "paused": "json" }, "initial": { "paused": null } }), steps, 1_000), &key("program")).unwrap();
        let (a, b) = join(
            rt.run_program(project_id, &program_id),
            rt.run_program(project_id, &program_id),
        )
        .await;
        a.unwrap();
        b.unwrap();
        assert_eq!(rt.thread(project_id, thread_id).unwrap()["state"], "paused");
        assert_eq!(
            rt.program_states(project_id).unwrap()[0]["value"]["status"],
            "completed"
        );
        assert_eq!(
            rt.program_states(project_id).unwrap()[0]["value"]["executionCount"],
            1
        );
        rt.execute(&base(project_id, json!({ "operation": "STOP", "threadId": null, "programId": program_id, "reason": "operator stop" }))).await.unwrap();
        assert_eq!(
            rt.program_states(project_id).unwrap()[0]["value"]["status"],
            "stopped"
        );

        let background_thread = open(project_id, "program background", Overrides::default());
        rt.execute(&background_thread).await.unwrap();
        let background_program_id = create_id(IdPrefix::Prg);
        fixture.sessions.delay.set(20);
        let background_step = json!({ "operation": "THREAD_STEP", "threadId": background_thread["threadId"], "objective": "background action", "expectedEpisodeType": "episode_test", "inputRefs": [], "skillIds": [] });
        rt.register_program(
            project_id,
            &program(
                &background_program_id,
                "program_background",
                "step_background",
                empty_state("state_background"),
                only_step("step_background", background_step, true),
                1_000,
            ),
            &key("program"),
        )
        .unwrap();
        let background_results = rt
            .run_program(project_id, &background_program_id)
            .await
            .unwrap();
        assert_eq!(background_results.len(), 1);
        assert!(
            rt.episodes(project_id)
                .unwrap()
                .iter()
                .any(|entry| entry["value"]["threadId"] == background_thread["threadId"])
        );
        fixture.sessions.delay.set(0);

        rt.execute(&fork(project_id, "foreground", &opened["threadId"]))
            .await
            .unwrap();
        assert!(rt.has_active_fork(project_id, None, None).unwrap());

        rt.register_hook(Rc::new(|instruction: &Value| {
            (instruction["operation"] == "DIRECT_ACTION").then(|| HookDecision {
                assignment: Assignment::Suppress,
                reasons: vec!["controlled experiment".into()],
                replacement: None,
            })
        }));
        let suppressed = rt.execute(&base(project_id, json!({ "operation": "DIRECT_ACTION", "threadId": thread_id, "action": "action_write", "payload": {} }))).await.unwrap();
        assert_eq!(
            suppressed,
            json!({ "suppressed": true, "reasons": ["controlled experiment"] })
        );
        assert!(
            !fixture
                .store
                .projections(project_id, "runtime_intervention")
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn executes_direct_actions_and_enforces_cancel_stop_and_proposer_ownership() {
    local(async {
        let fixture = setup();
        let project_id = fixture.project_id.as_str();
        let actions = Rc::new(RefCell::new(Vec::new()));
        let controlled = runtime(
            &fixture.root,
            &fixture.store,
            &fixture.sessions,
            Some(Rc::clone(&actions)),
        );
        let first = open(
            project_id,
            "owned",
            Overrides {
                capabilities: vec!["action_write"],
                ..Overrides::default()
            },
        );
        let second = open(project_id, "other", Overrides::default());
        controlled.execute(&first).await.unwrap();
        controlled.execute(&second).await.unwrap();
        let result = controlled.execute(&base(project_id, json!({ "operation": "DIRECT_ACTION", "threadId": first["threadId"], "action": "action_write", "payload": { "path": "result.json" } }))).await.unwrap();
        assert_eq!(result, json!({ "result": { "action": "action_write" } }));
        assert_eq!(*actions.borrow(), ["action_write"]);
        controlled.execute(&base(project_id, json!({ "operation": "THREAD_CANCEL", "threadId": second["threadId"], "reason": "no longer needed" }))).await.unwrap();
        assert_eq!(
            controlled
                .thread(project_id, second["threadId"].as_str().unwrap())
                .unwrap()["state"],
            "cancelled"
        );
        let owner = controlled
            .thread(project_id, first["threadId"].as_str().unwrap())
            .unwrap()["value"]["currentAgentId"]
            .clone();
        let mut unauthorized = step(project_id, &first["threadId"], "hijack");
        unauthorized["proposedByAgentId"] = json!(create_id(IdPrefix::Agt));
        fails(
            controlled.execute(&unauthorized).await,
            "not an active Pi session",
        );
        let escape = second["threadId"].clone();
        controlled.register_hook(Rc::new(move |instruction: &Value| {
            (instruction["operation"] == "THREAD_PAUSE").then(|| {
                let mut replacement = instruction.clone();
                replacement["operation"] = json!("THREAD_CANCEL");
                replacement["threadId"] = escape.clone();
                replacement["reason"] = json!("escaped");
                HookDecision {
                    assignment: Assignment::Replace,
                    reasons: vec!["try scope escape".into()],
                    replacement: Some(replacement),
                }
            })
        }));
        let mut pause = base(
            project_id,
            json!({ "operation": "THREAD_PAUSE", "threadId": first["threadId"], "reason": "replace" }),
        );
        pause["proposedByAgentId"] = owner.clone();
        fails(controlled.execute(&pause).await, "does not control");
        let mut stop = base(
            project_id,
            json!({ "operation": "STOP", "threadId": first["threadId"], "programId": null, "reason": "done" }),
        );
        stop["proposedByAgentId"] = owner;
        controlled.execute(&stop).await.unwrap();
        assert_eq!(
            controlled
                .thread(project_id, first["threadId"].as_str().unwrap())
                .unwrap()["state"],
            "completed"
        );
    });
}

#[test]
fn fails_closed_on_unknown_episode_evidence_and_leaves_a_durable_failed_thread() {
    local(async {
        let fixture = setup();
        let project_id = fixture.project_id.as_str();
        let opened = open(project_id, "invalid", Overrides::default());
        fixture.runtime.execute(&opened).await.unwrap();
        *fixture.sessions.draft.borrow_mut() = Some(Box::new(
            |episode_type: &str| json!({ "$schema": uri("episode-draft"), "schemaVersion": 1, "episodeType": episode_type, "summary": "invalid reference", "facts": [], "decisions": [], "artifactIds": [create_id(IdPrefix::Art)], "evidenceIds": [], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [] }),
        ));
        fails(
            fixture
                .runtime
                .execute(&step(
                    project_id,
                    &opened["threadId"],
                    "reject unknown evidence",
                ))
                .await,
            "unknown record",
        );
        assert_eq!(
            fixture
                .runtime
                .thread(project_id, opened["threadId"].as_str().unwrap())
                .unwrap()["state"],
            "failed"
        );
        assert_eq!(
            fixture
                .store
                .operation_intents(project_id, Some("failed"))
                .unwrap()
                .len(),
            1
        );
    });
}

#[test]
fn gates_successful_completion_on_typed_output_and_persists_loud_agent_failures_as_failed_episodes()
{
    local(async {
        let missing = setup();
        let first = open(&missing.project_id, "missing output", Overrides::default());
        missing.runtime.execute(&first).await.unwrap();
        missing.sessions.omit_draft.set(true);
        fails(
            missing
                .runtime
                .execute(&step(
                    &missing.project_id,
                    &first["threadId"],
                    "must be typed",
                ))
                .await,
            "required typed episode-draft",
        );
        assert!(
            missing
                .runtime
                .episodes(&missing.project_id)
                .unwrap()
                .is_empty()
        );

        let failed = setup();
        let second = open(&failed.project_id, "agent failure", Overrides::default());
        failed.runtime.execute(&second).await.unwrap();
        *failed.sessions.fail_message.borrow_mut() = "model transport failed".into();
        fails(
            failed
                .runtime
                .execute(&step(
                    &failed.project_id,
                    &second["threadId"],
                    "fail loudly",
                ))
                .await,
            "model transport failed",
        );
        assert_eq!(
            failed.runtime.episodes(&failed.project_id).unwrap()[0]["value"]["status"],
            "failed"
        );
        assert_eq!(
            failed
                .runtime
                .thread(&failed.project_id, second["threadId"].as_str().unwrap())
                .unwrap()["state"],
            "failed"
        );
        // A failed thread releases its session; a leaked live agent would block its owner's scheduling forever.
        assert!(failed.sessions.active.borrow().is_empty());
        failed.runtime.execute(&base(&failed.project_id, json!({ "operation": "STOP", "threadId": second["threadId"], "programId": null, "reason": "boundary" }))).await.unwrap();
    });
}

#[test]
fn rejects_malformed_programs_duplicate_execution_skill_policy_widening_and_budget_overruns() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let opened = open(
            project_id,
            "adversarial",
            Overrides {
                budget: Some(
                    json!({ "maximumToolCalls": 20, "maximumModelTokens": 1_000, "maximumWallClockSeconds": 120 }),
                ),
                ..Overrides::default()
            },
        );
        rt.execute(&opened).await.unwrap();
        let instruction = step(project_id, &opened["threadId"], "once only");
        let before = fixture.sessions.prompts.borrow().len();
        let (a, b) = join(rt.execute(&instruction), rt.execute(&instruction)).await;
        a.unwrap();
        b.unwrap();
        assert_eq!(fixture.sessions.prompts.borrow().len() - before, 1);

        let shared_skill_id = create_id(IdPrefix::Skl);
        let mut first_project_skill = skill(&["nosh_episode_submit"]);
        first_project_skill["skillId"] = json!(shared_skill_id);
        first_project_skill["promptFragment"] = json!("Project one skill.");
        let mut other_project_skill = first_project_skill.clone();
        other_project_skill["promptFragment"] = json!("Project two skill.");
        rt.register_skill(project_id, &first_project_skill, &key("skill"))
            .unwrap();
        rt.register_skill(
            &create_id(IdPrefix::Prj),
            &other_project_skill,
            &key("skill"),
        )
        .unwrap();
        let mut isolated = step(project_id, &opened["threadId"], "project isolation");
        isolated["skillIds"] = json!([shared_skill_id]);
        rt.execute(&isolated).await.unwrap();
        assert!(
            fixture
                .sessions
                .last_prompt()
                .contains("Project one skill.")
        );

        let invalid_program = json!({ "$schema": uri("orchestration-program"), "schemaVersion": 1, "programId": create_id(IdPrefix::Prg), "name": "invalid_program", "version": "1.0.0", "startStepId": "missing", "maximumSteps": 1, "maximumWallClockSeconds": 1, "maximumModelTokens": 1, "maximumToolCalls": 1, "state": empty_state("invalid_state"), "steps": [{ "stepId": "only_step", "instruction": {}, "guard": null, "background": false, "outputStateKey": null, "nextStepId": null, "failureStepId": null }] });
        assert!(
            rt.register_program(project_id, &invalid_program, &key("invalid"))
                .is_err()
        );
        let mut envelope_in_template = invalid_program.clone();
        envelope_in_template["programId"] = json!(create_id(IdPrefix::Prg));
        envelope_in_template["startStepId"] = json!("only_step");
        envelope_in_template["steps"] = only_step(
            "only_step",
            json!({ "operation": "THREAD_PAUSE", "threadId": opened["threadId"], "reason": "must be a body template", "projectId": project_id }),
            false,
        );
        assert!(
            rt.register_program(project_id, &envelope_in_template, &key("envelope"))
                .is_err()
        );

        let first_skill = skill(&["nosh_episode_submit"]);
        let second_skill = skill(&["nosh_episode_submit", "action_write"]);
        rt.register_skill(project_id, &first_skill, &key("skill"))
            .unwrap();
        rt.register_skill(project_id, &second_skill, &key("skill"))
            .unwrap();
        let ids = vec![
            first_skill["skillId"].as_str().unwrap().to_owned(),
            second_skill["skillId"].as_str().unwrap().to_owned(),
        ];
        let policy_thread = open(
            project_id,
            "policy",
            Overrides {
                skill_ids: ids,
                ..Overrides::default()
            },
        );
        rt.execute(&policy_thread).await.unwrap();
        *fixture.sessions.extra_tools.borrow_mut() = vec!["action_write".into()];
        fails(
            rt.execute(&step(
                project_id,
                &policy_thread["threadId"],
                "must satisfy both policies",
            ))
            .await,
            "Skill tool policy rejected",
        );

        fixture.sessions.extra_tools.borrow_mut().clear();
        let small = open(
            project_id,
            "small budget",
            Overrides {
                budget: Some(
                    json!({ "maximumToolCalls": 20, "maximumModelTokens": 5, "maximumWallClockSeconds": 120 }),
                ),
                ..Overrides::default()
            },
        );
        rt.execute(&small).await.unwrap();
        // A step that crosses the budget keeps its delivered episode (tokens are already spent);
        // the ceiling then blocks any further step.
        rt.execute(&step(project_id, &small["threadId"], "exceed budget"))
            .await
            .unwrap();
        let episodes = rt.episodes(project_id).unwrap();
        assert_eq!(
            episodes
                .iter()
                .find(|entry| entry["value"]["threadId"] == small["threadId"])
                .unwrap()["value"]["status"],
            "completed"
        );
        assert!(
            fixture
                .store
                .replay(project_id, 0)
                .unwrap()
                .iter()
                .any(|event| event["type"] == "runtime.thread_step_over_budget")
        );
        fails(
            rt.execute(&step(project_id, &small["threadId"], "one more"))
                .await,
            "exhausted its model-token budget",
        );
    });
}

#[test]
fn pauses_at_an_in_flight_episode_boundary_and_preserves_terminal_thread_states() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let opened = open(project_id, "pause boundary", Overrides::default());
        let thread_id = opened["threadId"].as_str().unwrap();
        rt.execute(&opened).await.unwrap();
        fixture.sessions.delay.set(40);
        let running = rt.execute(&step(
            project_id,
            &opened["threadId"],
            "finish before pausing",
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let child = fork(project_id, "in-flight child", &opened["threadId"]);
        rt.execute(&child).await.unwrap();
        rt.execute(&base(project_id, json!({ "operation": "THREAD_PAUSE", "threadId": thread_id, "reason": "operator boundary" }))).await.unwrap();
        running.await.unwrap();
        let paused = rt.thread(project_id, thread_id).unwrap();
        assert_eq!(paused["state"], "paused");
        assert!(
            paused["value"]["childThreadIds"]
                .as_array()
                .unwrap()
                .contains(&child["threadId"])
        );
        fixture.sessions.delay.set(0);
        fails(
            rt.execute(&step(project_id, &opened["threadId"], "blocked"))
                .await,
            "foreground fork",
        );
        rt.execute(&base(project_id, json!({ "operation": "STOP", "threadId": child["threadId"], "programId": null, "reason": "fork done" }))).await.unwrap();
        rt.execute(&step(project_id, &opened["threadId"], "resume"))
            .await
            .unwrap();
        rt.execute(&base(project_id, json!({ "operation": "STOP", "threadId": thread_id, "programId": null, "reason": "done" }))).await.unwrap();
        assert_eq!(
            rt.thread(project_id, thread_id).unwrap()["state"],
            "completed"
        );
        fails(
            rt.execute(&step(project_id, &opened["threadId"], "must not reopen"))
                .await,
            "cannot step",
        );
        assert_eq!(
            rt.thread(project_id, thread_id).unwrap()["state"],
            "completed"
        );
    });
}

#[test]
fn implements_first_minimum_count_and_deadline_joins_without_returning_a_stale_episode() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let ready = open(project_id, "ready", Overrides::default());
        let slow = open(project_id, "slow", Overrides::default());
        let (a, b) = join(rt.execute(&ready), rt.execute(&slow)).await;
        a.unwrap();
        b.unwrap();
        let ready_result = rt
            .execute(&step(project_id, &ready["threadId"], "ready result"))
            .await
            .unwrap();
        let ready_episode = ready_result["episode"]["episodeId"].clone();
        fixture.sessions.delay.set(50);
        let slow_run = rt.execute(&step(project_id, &slow["threadId"], "slow result"));
        let both = json!([ready["threadId"], slow["threadId"]]);
        let first = rt.execute(&base(project_id, json!({ "operation": "THREAD_AWAIT", "threadIds": both, "policy": "first_success", "minimumCount": null, "deadline": null }))).await.unwrap();
        assert_eq!(first["episodeIds"], json!([ready_episode]));
        let minimum = rt.execute(&base(project_id, json!({ "operation": "THREAD_AWAIT", "threadIds": both, "policy": "minimum_count", "minimumCount": 1, "deadline": null }))).await.unwrap();
        assert_eq!(minimum["episodeIds"], json!([ready_episode]));
        slow_run.await.unwrap();
        let second_slow_run = rt.execute(&step(project_id, &slow["threadId"], "new slow result"));
        let deadline = nosh_wire::iso_from_ms(time_ms() + 10.0).unwrap();
        let timed = rt.execute(&base(project_id, json!({ "operation": "THREAD_AWAIT", "threadIds": [slow["threadId"]], "policy": "deadline", "minimumCount": null, "deadline": deadline }))).await.unwrap();
        assert_eq!(timed["episodeIds"], json!([]));
        second_slow_run.await.unwrap();
    });
}

fn time_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

#[test]
fn persists_stop_as_stopped_instead_of_stale_completed() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let program_id = create_id(IdPrefix::Prg);
        let stop = json!({ "operation": "STOP", "threadId": null, "programId": program_id, "reason": "operator stop" });
        rt.register_program(
            project_id,
            &program(
                &program_id,
                "program_stop",
                "step_stop",
                empty_state("state_stop"),
                only_step("step_stop", stop, false),
                100,
            ),
            "program-stop",
        )
        .unwrap();
        rt.run_program(project_id, &program_id).await.unwrap();
        let state = rt.program_states(project_id).unwrap()[0]["value"].clone();
        assert_eq!(state["status"], "stopped");
        assert!(state["currentStepId"].is_null());
        assert_eq!(state["checkpoint"]["outcome"], "stopped");
        assert_eq!(state["checkpoint"]["reason"], "operator stop");
    });
}

fn stop_program(program_id: &str, name: &str, namespace: &str, step_id: &str) -> Value {
    let stop = json!({ "operation": "STOP", "threadId": null, "programId": program_id, "reason": "program_stop" });
    program(
        program_id,
        name,
        step_id,
        empty_state(namespace),
        only_step(step_id, stop, false),
        100,
    )
}

#[test]
fn register_program_replay_is_side_effect_free_and_changed_key_reuse_rejects() {
    let fixture = setup();
    let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
    let program_id = create_id(IdPrefix::Prg);
    let body = stop_program(&program_id, "program_replay", "state_replay", "step_replay");
    rt.register_program(project_id, &body, "program-replay")
        .unwrap();
    let events = fixture.store.replay(project_id, 0).unwrap().len();
    let version = rt.program_states(project_id).unwrap()[0]["version"].clone();
    rt.register_program(project_id, &body, "program-replay")
        .unwrap();
    assert_eq!(fixture.store.replay(project_id, 0).unwrap().len(), events);
    assert_eq!(
        rt.program_states(project_id).unwrap()[0]["version"],
        version
    );
    let mut changed = body.clone();
    changed["name"] = json!("program_changed");
    assert!(
        rt.register_program(project_id, &changed, "program-replay")
            .is_err()
    );
}

#[test]
fn concurrent_duplicate_program_runs_share_one_execution_count() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let program_id = create_id(IdPrefix::Prg);
        rt.register_program(
            project_id,
            &stop_program(
                &program_id,
                "program_concurrent",
                "state_concurrent",
                "step_concurrent",
            ),
            "program-concurrent",
        )
        .unwrap();
        let (a, b) = join(
            rt.run_program(project_id, &program_id),
            rt.run_program(project_id, &program_id),
        )
        .await;
        a.unwrap();
        b.unwrap();
        assert_eq!(
            rt.program_states(project_id).unwrap()[0]["value"]["executionCount"],
            1
        );
    });
}

#[test]
fn failed_program_runs_persist_terminal_state_and_permit_one_clean_retry() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let program_id = create_id(IdPrefix::Prg);
        let missing = json!({ "operation": "DIRECT_ACTION", "threadId": null, "action": "action_missing", "payload": {} });
        rt.register_program(
            project_id,
            &program(
                &program_id,
                "program_failed",
                "step_fail",
                empty_state("state_failed"),
                only_step("step_fail", missing, false),
                100,
            ),
            "program-failed",
        )
        .unwrap();
        assert!(rt.run_program(project_id, &program_id).await.is_err());
        assert_eq!(
            rt.program_states(project_id).unwrap()[0]["value"]["status"],
            "failed"
        );
        assert!(rt.run_program(project_id, &program_id).await.is_err());
        assert_eq!(
            rt.program_states(project_id).unwrap()[0]["value"]["executionCount"],
            2
        );
    });
}

#[test]
fn rejects_orphan_program_state_authority_without_mutating_it() {
    let fixture = setup();
    let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
    let program_id = create_id(IdPrefix::Prg);
    let state = json!({ "$schema": uri("program-state"), "schemaVersion": 1, "projectId": project_id, "programId": program_id, "namespace": "state_orphan", "fieldTypes": {}, "values": {}, "status": "idle", "currentStepId": null, "stepsExecuted": 0, "executionCount": 0, "usage": { "toolCalls": 0, "modelTokens": 0, "wallClockSeconds": 0 }, "checkpoint": null, "startedAt": null, "updatedAt": now_utc() });
    let draft = json!({ "$schema": uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "runtime.program_state_declared", "source": "test", "scope": event_scope(project_id, Value::Null, Value::Null), "correlationId": program_id, "causationId": null, "payload": state });
    fixture
        .store
        .mutate_projection(
            "orphan-state",
            Some(0),
            &draft,
            &ProjectionInput {
                entity_type: "program_state".into(),
                entity_id: program_id.clone(),
                state: "idle".into(),
                value: state.clone(),
                graph: None,
                records: vec![],
                intent: None,
            },
        )
        .unwrap();
    let stop = json!({ "operation": "STOP", "threadId": null, "programId": program_id, "reason": "orphan" });
    let mut orphan = program(
        &program_id,
        "program_orphan",
        "step_orphan",
        empty_state("state_orphan"),
        only_step("step_orphan", stop, false),
        1,
    );
    for field in [
        "maximumSteps",
        "maximumWallClockSeconds",
        "maximumToolCalls",
    ] {
        orphan[field] = json!(1);
    }
    fails(
        rt.register_program(project_id, &orphan, "program-orphan"),
        "orphaned state",
    );
    assert_eq!(
        fixture
            .store
            .projection(project_id, "program_state", &program_id)
            .unwrap()
            .unwrap()["value"],
        state
    );
}

#[test]
fn checks_each_call_inside_nosh_code_by_its_own_tool_name_and_counts_it_against_the_budget() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let read_only = skill(&["nosh_episode_submit", "nosh_workspace_read"]);
        rt.register_skill(project_id, &read_only, &key("skill"))
            .unwrap();
        let skill_ids = vec![read_only["skillId"].as_str().unwrap().to_owned()];
        let allowed = open(
            project_id,
            "program reads",
            Overrides {
                skill_ids: skill_ids.clone(),
                ..Overrides::default()
            },
        );
        rt.execute(&allowed).await.unwrap();
        // The program call itself is exempt; its two reads are permitted by name.
        *fixture.sessions.extra_tools.borrow_mut() = vec![
            "nosh_code".into(),
            "nosh_workspace_read".into(),
            "nosh_workspace_read".into(),
        ];
        rt.execute(&step(
            project_id,
            &allowed["threadId"],
            "read through a program",
        ))
        .await
        .unwrap();
        assert_eq!(
            rt.episodes(project_id).unwrap().last().unwrap()["value"]["usage"]["toolCalls"],
            4
        );
        let denied = open(
            project_id,
            "program writes",
            Overrides {
                skill_ids,
                ..Overrides::default()
            },
        );
        rt.execute(&denied).await.unwrap();
        *fixture.sessions.extra_tools.borrow_mut() =
            vec!["nosh_code".into(), "nosh_workspace_write".into()];
        fails(
            rt.execute(&step(
                project_id,
                &denied["threadId"],
                "write through a program",
            ))
            .await,
            "Skill tool policy rejected: nosh_workspace_write",
        );
    });
}

// Rust-port additions: behavior the TypeScript tests leave unpinned (found by mutation testing).

#[test]
fn hooks_may_not_change_the_instruction_envelope() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let opened = open(project_id, "envelope", Overrides::default());
        rt.execute(&opened).await.unwrap();
        rt.register_hook(Rc::new(|instruction: &Value| {
            let mut replacement = instruction.clone();
            replacement["instructionId"] = json!(create_id(IdPrefix::Ins));
            Some(HookDecision {
                assignment: Assignment::Replace,
                reasons: vec!["rewrite".into()],
                replacement: Some(replacement),
            })
        }));
        let pause = base(
            project_id,
            json!({ "operation": "THREAD_PAUSE", "threadId": opened["threadId"], "reason": "hooked" }),
        );
        fails(rt.execute(&pause).await, "immutable instruction envelope");
    });
}

#[test]
fn a_step_under_several_skills_starts_its_session_with_only_the_tools_they_share() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let reader = skill(&["nosh_episode_submit", "nosh_workspace_read"]);
        let writer = skill(&["nosh_workspace_write", "nosh_episode_submit"]);
        rt.register_skill(project_id, &reader, &key("skill"))
            .unwrap();
        rt.register_skill(project_id, &writer, &key("skill"))
            .unwrap();
        let skill_ids = [&reader, &writer]
            .map(|skill| skill["skillId"].as_str().unwrap().to_owned())
            .to_vec();
        let opened = open(
            project_id,
            "shared tools",
            Overrides {
                skill_ids,
                ..Overrides::default()
            },
        );
        rt.execute(&opened).await.unwrap();
        rt.execute(&step(project_id, &opened["threadId"], "use both"))
            .await
            .unwrap();
        assert_eq!(
            fixture
                .sessions
                .started_tools
                .borrow()
                .last()
                .cloned()
                .flatten(),
            Some(vec!["nosh_episode_submit".to_owned()])
        );
    });
}

#[test]
fn a_background_step_finishes_even_when_its_program_fails_first() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let background_thread = open(project_id, "outlives its program", Overrides::default());
        rt.execute(&background_thread).await.unwrap();
        fixture.sessions.delay.set(20);
        let program_id = create_id(IdPrefix::Prg);
        let background = json!({ "operation": "THREAD_STEP", "threadId": background_thread["threadId"], "objective": "background action", "expectedEpisodeType": "episode_test", "inputRefs": [], "skillIds": [] });
        let missing = json!({ "operation": "DIRECT_ACTION", "threadId": null, "action": "action_missing", "payload": {} });
        let steps = json!([
            { "stepId": "step_background", "instruction": background, "guard": null, "background": true, "outputStateKey": null, "nextStepId": "step_fail", "failureStepId": null },
            { "stepId": "step_fail", "instruction": missing, "guard": null, "background": false, "outputStateKey": null, "nextStepId": null, "failureStepId": null },
        ]);
        rt.register_program(
            project_id,
            &program(
                &program_id,
                "program_outlived",
                "step_background",
                empty_state("state_outlived"),
                steps,
                1_000,
            ),
            &key("program"),
        )
        .unwrap();
        fails(
            rt.run_program(project_id, &program_id).await,
            "No direct action handler",
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let episodes = rt.episodes(project_id).unwrap();
        assert!(episodes.iter().any(|entry| entry["value"]["threadId"]
            == background_thread["threadId"]
            && entry["value"]["status"] == "completed"));
    });
}

#[test]
fn foreground_program_steps_add_their_episode_usage_to_the_program_state() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let opened = open(project_id, "program usage", Overrides::default());
        rt.execute(&opened).await.unwrap();
        let program_id = create_id(IdPrefix::Prg);
        let thread_step = json!({ "operation": "THREAD_STEP", "threadId": opened["threadId"], "objective": "counted", "expectedEpisodeType": "episode_test", "inputRefs": [], "skillIds": [] });
        rt.register_program(
            project_id,
            &program(
                &program_id,
                "program_usage",
                "step_counted",
                empty_state("state_usage"),
                only_step("step_counted", thread_step, false),
                1_000,
            ),
            &key("program"),
        )
        .unwrap();
        rt.run_program(project_id, &program_id).await.unwrap();
        let usage = rt.program_states(project_id).unwrap()[0]["value"]["usage"].clone();
        assert_eq!(
            (usage["modelTokens"].clone(), usage["toolCalls"].clone()),
            (json!(12), json!(1))
        );
    });
}

#[test]
fn changed_files_must_stay_inside_the_repository() {
    let fixture = setup();
    let draft = |file: &str| json!({ "$schema": uri("episode-draft"), "schemaVersion": 1, "episodeType": "episode_test", "summary": "s", "facts": [], "decisions": [], "artifactIds": [], "evidenceIds": [], "changedFiles": [file], "unresolvedQuestions": [], "recommendedNextActions": [] });
    for inside in ["src/main.rs", r"src\lib.rs", "a/../b.txt"] {
        fixture
            .runtime
            .validate_draft(&fixture.project_id, &draft(inside), None)
            .unwrap();
    }
    // Node's win32 `isAbsolute` refuses an absolute path even when it points inside.
    let absolute_inside = fixture.root.join("a.txt").to_string_lossy().into_owned();
    for escaping in [
        "../outside.txt",
        "src/../../outside.txt",
        r"\a.txt",
        "",
        ".",
        absolute_inside.as_str(),
    ] {
        fails(
            fixture
                .runtime
                .validate_draft(&fixture.project_id, &draft(escaping), None),
            "Changed file escapes the repository",
        );
    }
}

#[test]
fn an_agent_stopping_its_own_thread_keeps_its_session_until_its_call_returns() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let opened = open(project_id, "self stop", Overrides::default());
        rt.execute(&opened).await.unwrap();
        let owner = rt
            .thread(project_id, opened["threadId"].as_str().unwrap())
            .unwrap()["value"]["currentAgentId"]
            .clone();
        let live = || {
            fixture
                .sessions
                .active
                .borrow()
                .iter()
                .any(|session| owner == session.agent_id.as_str())
        };
        let mut stop = base(
            project_id,
            json!({ "operation": "STOP", "threadId": opened["threadId"], "programId": null, "reason": "done" }),
        );
        stop["proposedByAgentId"] = owner.clone();
        rt.execute(&stop).await.unwrap();
        assert!(
            live(),
            "the session must outlive the tool call that stopped it"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!live());
    });
}

#[test]
fn a_trace_ends_at_its_episode() {
    local(async {
        let fixture = setup();
        let (rt, project_id) = (&fixture.runtime, fixture.project_id.as_str());
        let opened = open(project_id, "traced", Overrides::default());
        rt.execute(&opened).await.unwrap();
        let first = rt
            .execute(&step(project_id, &opened["threadId"], "first"))
            .await
            .unwrap();
        rt.execute(&step(project_id, &opened["threadId"], "later"))
            .await
            .unwrap();
        let episode = &first["episode"];
        let trace = rt
            .trace(project_id, episode["episodeId"].as_str().unwrap())
            .unwrap();
        assert!(!trace.is_empty());
        assert!(
            trace.iter().all(
                |event| event["sequence"].as_i64() <= episode["trace"]["lastSequence"].as_i64()
            )
        );
    });
}
