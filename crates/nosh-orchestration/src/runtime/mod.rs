//! The typed orchestration runtime, as `OrchestrationRuntime` in
//! `packages/orchestration-runtime/src/index.ts`: logical execution threads stepped by Pi
//! sessions into hashed Episodes, joins, skills, bounded programs, hooks, and durable
//! interventions.
//!
//! Records stay `serde_json::Value`s shaped like the TypeScript objects. An execution, a thread
//! step, and a program run are each shared by every caller awaiting them (TypeScript keeps their
//! promises in maps). `execute` and `run_program` start work before returning, as calling an
//! `async` function does in JavaScript, so a duplicate call made right after joins the first.
//!
//! Divergences: `JSON.stringify` comparisons (stored program state types, guard `equals`) are
//! structural here because serde_json does not keep key order, and the refs an intervention
//! collects from an object come in sorted-key order rather than insertion order.

mod programs;
mod references;
mod steps;

use crate::{Agents, Error, Result, rejected};
use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared};
use nosh_agent::policy::SessionOptions;
use nosh_research::episodes::episode_integrity;
use nosh_research::skills::SkillRegistry;
use nosh_research::threads::{create_thread, ensure_can_step};
use nosh_store::{EventStore, ProjectionInput};
use nosh_wire::js::{strict_equal, string, truthy};
use nosh_wire::{canonical_json, normalize_numbers, now_utc, sha256, validate_record};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the daemon provides (the TypeScript `RuntimeDependencies`, less the sessions).
pub trait RuntimeHost {
    /// The Project's event store; an unknown Project is an error.
    fn store(&self, project_id: &str) -> Result<Rc<EventStore>>;
    fn repository_root(&self, project_id: &str) -> Result<PathBuf>;
    /// The Pi package every runtime session loads.
    fn package_path(&self) -> PathBuf;
    /// Delivers a newly stored event to subscribers.
    fn publish(&self, _event: &Value) {}
    /// Runs a `DIRECT_ACTION`; `None` when the host has no handler.
    fn direct_action(
        &self,
        _action: &str,
        _payload: &Value,
        _instruction: &Value,
    ) -> Option<LocalBoxFuture<'static, Result<Value>>> {
        None
    }
    /// A named skill check; `None` when the host does not know it.
    fn skill_check(
        &self,
        _check_id: &str,
        _phase: &str,
        _thread: &Value,
        _episode: Option<&Value>,
    ) -> Option<bool> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    Execute,
    Suppress,
    Replace,
}

impl Assignment {
    pub fn as_str(self) -> &'static str {
        match self {
            Assignment::Execute => "execute",
            Assignment::Suppress => "suppress",
            Assignment::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookDecision {
    pub assignment: Assignment,
    pub reasons: Vec<String>,
    pub replacement: Option<Value>,
}

pub type RuntimeHook = Rc<dyn Fn(&Value) -> Option<HookDecision>>;

type SharedResult<T> = Shared<LocalBoxFuture<'static, Result<T>>>;

pub struct OrchestrationRuntime<H: RuntimeHost> {
    host: H,
    agents: Rc<dyn Agents>,
    skill_registries: RefCell<HashMap<String, SkillRegistry>>,
    programs: RefCell<HashMap<String, Value>>,
    hooks: RefCell<Vec<RuntimeHook>>,
    in_flight: RefCell<HashMap<String, SharedResult<Value>>>,
    executions: RefCell<HashMap<String, SharedResult<Value>>>,
    program_runs: RefCell<HashMap<String, SharedResult<Vec<Value>>>>,
}

impl<H: RuntimeHost + 'static> OrchestrationRuntime<H> {
    pub fn new(host: H, agents: Rc<dyn Agents>) -> Rc<Self> {
        Rc::new(OrchestrationRuntime {
            host,
            agents,
            skill_registries: RefCell::new(HashMap::new()),
            programs: RefCell::new(HashMap::new()),
            hooks: RefCell::new(Vec::new()),
            in_flight: RefCell::new(HashMap::new()),
            executions: RefCell::new(HashMap::new()),
            program_runs: RefCell::new(HashMap::new()),
        })
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn register_hook(&self, hook: RuntimeHook) {
        self.hooks.borrow_mut().push(hook);
    }

    /// Validates and runs one runtime instruction, once per `projectId:idempotencyKey`: a
    /// running duplicate joins it, and a finished one returns its durable result.
    pub fn execute(
        self: &Rc<Self>,
        input: &Value,
    ) -> impl Future<Output = Result<Value>> + 'static {
        let started = self.start_execution(input);
        async move { started?.await }
    }

    fn start_execution(self: &Rc<Self>, input: &Value) -> Result<SharedResult<Value>> {
        let proposed = parse("runtime-instruction", input)?;
        let project_id = text(&proposed["projectId"]).to_owned();
        let key = format!("{project_id}:{}", text(&proposed["idempotencyKey"]));
        if let Some(running) = self.executions.borrow().get(&key).cloned() {
            return Ok(running);
        }
        // `executeOne` records its intent before its first `await`.
        let store = self.host.store(&project_id)?;
        let operation = format!("runtime.{}", text(&proposed["operation"]).to_lowercase());
        let intent = store.begin_operation(
            &project_id,
            &operation,
            text(&proposed["idempotencyKey"]),
            &proposed,
        )?;
        let this = Rc::clone(self);
        let entry = key.clone();
        let execution = async move {
            let result = this.execute_one(proposed, intent).await;
            this.executions.borrow_mut().remove(&entry);
            result
        }
        .boxed_local()
        .shared();
        self.executions.borrow_mut().insert(key, execution.clone());
        let _ = execution.clone().now_or_never();
        Ok(execution)
    }

    async fn execute_one(self: &Rc<Self>, proposed: Value, intent: Value) -> Result<Value> {
        if intent["state"] == "completed" {
            return Ok(or_empty(&intent["result"]));
        }
        if intent["state"] == "failed" {
            return rejected(
                intent["error"]
                    .as_str()
                    .unwrap_or("Runtime instruction previously failed"),
            );
        }
        let project_id = text(&proposed["projectId"]).to_owned();
        let store = self.host.store(&project_id)?;
        let started = Instant::now();
        let before = self.thread_state(&proposed)?;
        let mut instruction = proposed.clone();
        let mut assignment = Assignment::Execute;
        let mut reasons: Vec<String> = Vec::new();
        let outcome: Result<Value> = async {
            self.authorize(&instruction)?;
            let hooks = self.hooks.borrow().clone();
            for hook in hooks {
                let Some(decision) = hook(&instruction) else {
                    continue;
                };
                assignment = decision.assignment;
                reasons.extend(decision.reasons);
                if decision.assignment == Assignment::Replace {
                    let Some(replacement) = decision.replacement else {
                        return rejected("Replacement hook omitted its instruction");
                    };
                    instruction = parse("runtime-instruction", &replacement)?;
                    if [
                        "projectId",
                        "instructionId",
                        "idempotencyKey",
                        "proposedByAgentId",
                    ]
                    .iter()
                    .any(|key| !strict_equal(instruction.get(*key), proposed.get(*key)))
                    {
                        return rejected(
                            "Replacement hook changed the immutable instruction envelope",
                        );
                    }
                    self.authorize(&instruction)?;
                }
                if decision.assignment == Assignment::Suppress {
                    break;
                }
            }
            let result = if assignment == Assignment::Suppress {
                json!({ "suppressed": true, "reasons": reasons })
            } else {
                self.dispatch(&instruction).await?
            };
            let after = self.thread_state(&instruction)?;
            self.record_intervention(
                &proposed,
                assignment,
                &reasons,
                true,
                assignment != Assignment::Suppress,
                &before,
                &after,
                &result,
                elapsed_ms(started),
            )?;
            store.complete_operation(&project_id, text(&intent["intentId"]), &result)?;
            Ok(result)
        }
        .await;
        let Err(error) = outcome else { return outcome };
        let message = error.to_string();
        if instruction["operation"] == "THREAD_STEP" {
            self.mark_step_failed(&instruction)?;
        }
        // The primary failure remains authoritative.
        let _ = self.thread_state(&instruction).and_then(|after| {
            let mut all = reasons.clone();
            all.push(message.clone());
            self.record_intervention(
                &proposed,
                assignment,
                &all,
                false,
                false,
                &before,
                &after,
                &json!({ "error": message }),
                elapsed_ms(started),
            )
        });
        store.fail_operation(&project_id, text(&intent["intentId"]), &message)?;
        Err(error)
    }

    pub fn threads(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .host
            .store(project_id)?
            .projections(project_id, "execution_thread")?)
    }

    pub fn thread(&self, project_id: &str, thread_id: &str) -> Result<Value> {
        self.host
            .store(project_id)?
            .projection(project_id, "execution_thread", thread_id)?
            .ok_or_else(|| Error::Rejected(format!("Unknown thread {thread_id}")))
    }

    pub fn episodes(&self, project_id: &str) -> Result<Vec<Value>> {
        let episodes = self
            .host
            .store(project_id)?
            .projections(project_id, "episode")?;
        for episode in &episodes {
            if !episode_integrity(&episode["value"]) {
                return rejected(format!(
                    "Episode {} failed its integrity check",
                    string(episode["value"].get("episodeId"))
                ));
            }
        }
        Ok(episodes)
    }

    pub fn skills(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .host
            .store(project_id)?
            .projections(project_id, "skill")?)
    }

    pub fn program_records(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .host
            .store(project_id)?
            .projections(project_id, "orchestration_program")?)
    }

    pub fn program_states(&self, project_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .host
            .store(project_id)?
            .projections(project_id, "program_state")?)
    }

    pub fn episode(&self, project_id: &str, episode_id: &str) -> Result<Value> {
        let Some(episode) = self
            .host
            .store(project_id)?
            .projection(project_id, "episode", episode_id)?
        else {
            return rejected(format!("Unknown episode {episode_id}"));
        };
        if !episode_integrity(&episode["value"]) {
            return rejected(format!("Episode {episode_id} failed its integrity check"));
        }
        Ok(episode)
    }

    /// The Project events an Episode's step produced.
    pub fn trace(&self, project_id: &str, episode_id: &str) -> Result<Vec<Value>> {
        let episode = self.episode(project_id, episode_id)?["value"].clone();
        let first = num(&episode["trace"]["firstSequence"]);
        let last = num(&episode["trace"]["lastSequence"]);
        let events = self
            .host
            .store(project_id)?
            .replay(project_id, (first - 1.0) as i64)?;
        Ok(events
            .into_iter()
            .filter(|event| {
                event["sequence"]
                    .as_f64()
                    .is_some_and(|sequence| sequence <= last)
            })
            .collect())
    }

    pub fn has_active_fork(
        &self,
        project_id: &str,
        mission_id: Option<&str>,
        direction_id: Option<&str>,
    ) -> Result<bool> {
        Ok(self.threads(project_id)?.iter().any(|projection| {
            let thread = &projection["value"];
            thread["executionMode"] == "foreground_fork"
                && matches!(
                    text(&thread["state"]),
                    "open" | "running" | "awaiting_user" | "awaiting"
                )
                && thread["ownerScope"]["missionId"].as_str() == mission_id
                && thread["ownerScope"]["directionId"].as_str() == direction_id
        }))
    }

    pub async fn rotate(
        &self,
        project_id: &str,
        thread_id: &str,
        idempotency_key: &str,
    ) -> Result<Value> {
        let stored = self.thread(project_id, thread_id)?;
        let mut thread = stored["value"].clone();
        if terminal_thread(&thread)
            || thread["state"] == "running"
            || truthy(thread.get("activeInstructionId"))
            || self.active_child_fork(project_id, &thread)?
        {
            return rejected(format!(
                "Thread {thread_id} cannot rotate from {}",
                string(thread.get("state"))
            ));
        }
        self.end_session(&mut thread, "rotated", false);
        self.ensure_session(&mut thread, None, None).await?;
        Ok(self.save_thread(
            &thread,
            version(&stored),
            idempotency_key,
            "runtime.thread_session_rotated",
        )?["value"]
            .clone())
    }

    pub async fn fork_message(
        self: &Rc<Self>,
        project_id: &str,
        thread_id: &str,
        message: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let text = message.trim_matches(nosh_agent::terminal::is_js_whitespace);
        if text.is_empty() {
            return rejected("Fork message is required");
        }
        let thread = self.thread(project_id, thread_id)?["value"].clone();
        if thread["executionMode"] != "foreground_fork" || thread["state"] != "awaiting_user" {
            return rejected("Thread is not awaiting foreground interaction");
        }
        let instruction = json!({ "$schema": uri("runtime-instruction"), "schemaVersion": 1, "instructionId": nosh_wire::create_id(nosh_wire::IdPrefix::Ins), "projectId": project_id, "idempotencyKey": idempotency_key, "proposedByAgentId": null, "issuedAt": now_utc(), "operation": "THREAD_STEP", "threadId": thread_id, "objective": text, "expectedEpisodeType": "episode_foreground", "inputRefs": [], "skillIds": [] });
        self.execute(&instruction).await?;
        Ok(())
    }

    async fn dispatch(self: &Rc<Self>, instruction: &Value) -> Result<Value> {
        match text(&instruction["operation"]) {
            "THREAD_OPEN" | "THREAD_FORK" => Ok(json!({ "thread": self.open(instruction).await? })),
            "THREAD_STEP" => {
                let key = format!(
                    "{}:{}",
                    text(&instruction["projectId"]),
                    text(&instruction["threadId"])
                );
                let this = Rc::clone(self);
                let owned = instruction.clone();
                let promise = async move { this.step(&owned).await }
                    .boxed_local()
                    .shared();
                self.in_flight
                    .borrow_mut()
                    .insert(key.clone(), promise.clone());
                let episode = promise.await;
                self.in_flight.borrow_mut().remove(&key);
                Ok(json!({ "episode": episode? }))
            }
            "THREAD_AWAIT" => self.await_threads(instruction).await,
            "THREAD_COMPOSE" => Ok(json!({ "thread": self.compose(instruction)? })),
            "THREAD_PAUSE" => Ok(json!({ "thread": self.pause(instruction).await? })),
            "THREAD_CANCEL" => Ok(json!({ "thread": self.cancel(instruction).await? })),
            "SKILL_APPLY" => self.apply_skill_instruction(instruction).await,
            "DIRECT_ACTION" => Ok(json!({ "result": self.direct_action(instruction).await? })),
            "STOP" => self.stop(instruction),
            other => rejected(format!("Unknown runtime operation {other}")),
        }
    }

    async fn open(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let key = text(&instruction["idempotencyKey"]);
        let store = self.host.store(project_id)?;
        if let Some(prior) = store.projection(
            project_id,
            "execution_thread",
            text(&instruction["threadId"]),
        )? {
            return Ok(prior["value"].clone());
        }
        let parent_id = if instruction["operation"] == "THREAD_FORK" {
            &instruction["controllingThreadId"]
        } else {
            &instruction["parentThreadId"]
        };
        if truthy(Some(parent_id)) {
            let parent = self.thread(project_id, text(parent_id))?["value"].clone();
            if terminal_thread(&parent) || self.active_child_fork(project_id, &parent)? {
                return rejected(format!(
                    "Thread {} cannot accept another child from {}",
                    string(Some(parent_id)),
                    string(parent.get("state"))
                ));
            }
        }
        let mut thread = create_thread(instruction)?;
        self.ensure_session(&mut thread, instruction["initialAgentId"].as_str(), None)
            .await?;
        let saved = self.save_thread(
            &thread,
            0,
            &format!("{key}:thread"),
            "runtime.thread_opened",
        )?;
        if truthy(thread.get("parentThreadId")) {
            self.add_child(
                project_id,
                text(&thread["parentThreadId"]),
                text(&thread["threadId"]),
                &format!("{key}:parent"),
            )?;
        }
        Ok(saved["value"].clone())
    }

    fn compose(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let stored = self.thread(project_id, text(&instruction["targetThreadId"]))?;
        let mut thread = stored["value"].clone();
        if terminal_thread(&thread)
            || thread["state"] == "running"
            || truthy(thread.get("activeInstructionId"))
            || self.active_child_fork(project_id, &thread)?
        {
            return rejected(format!(
                "Thread {} cannot compose from {}",
                string(thread.get("threadId")),
                string(thread.get("state"))
            ));
        }
        for id in strings(&instruction["episodeIds"]) {
            self.episode(project_id, &id)?;
        }
        thread["inputRefs"] = union(&thread["inputRefs"], &instruction["episodeIds"]);
        thread["state"] = json!("open");
        thread["updatedAt"] = json!(now_utc());
        Ok(self.save_thread(
            &thread,
            version(&stored),
            &format!("{}:compose", text(&instruction["idempotencyKey"])),
            "runtime.thread_composed",
        )?["value"]
            .clone())
    }

    async fn pause(&self, instruction: &Value) -> Result<Value> {
        let stored = self.thread(
            text(&instruction["projectId"]),
            text(&instruction["threadId"]),
        )?;
        let mut thread = stored["value"].clone();
        if terminal_thread(&thread) {
            return rejected(format!(
                "Thread {} cannot pause from {}",
                string(thread.get("threadId")),
                string(thread.get("state"))
            ));
        }
        if let Some(agent_id) = self.live_agent(&thread) {
            let message = format!(
                "Pause at the next safe boundary. Reason: {}",
                string(instruction.get("reason"))
            );
            self.agents
                .steer(&agent_id, &message)
                .await
                .map_err(Error::Rejected)?;
        }
        thread["state"] = json!("paused");
        thread["updatedAt"] = json!(now_utc());
        Ok(self.save_thread(
            &thread,
            version(&stored),
            &format!("{}:pause", text(&instruction["idempotencyKey"])),
            "runtime.thread_paused",
        )?["value"]
            .clone())
    }

    async fn cancel(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let stored = self.thread(project_id, text(&instruction["threadId"]))?;
        let mut thread = stored["value"].clone();
        if terminal_thread(&thread) || self.active_child_fork(project_id, &thread)? {
            return rejected(format!(
                "Thread {} cannot cancel from {}",
                string(thread.get("threadId")),
                string(thread.get("state"))
            ));
        }
        if let Some(agent_id) = self.live_agent(&thread) {
            self.agents
                .abort(&agent_id)
                .await
                .map_err(Error::Rejected)?;
        }
        self.end_session(&mut thread, "cancelled", false);
        thread["state"] = json!("cancelled");
        thread["activeInstructionId"] = Value::Null;
        thread["updatedAt"] = json!(now_utc());
        Ok(self.save_thread(
            &thread,
            version(&stored),
            &format!("{}:cancel", text(&instruction["idempotencyKey"])),
            "runtime.thread_cancelled",
        )?["value"]
            .clone())
    }

    fn apply_skill(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let stored = self.thread(project_id, text(&instruction["threadId"]))?;
        let mut thread = stored["value"].clone();
        if terminal_thread(&thread)
            || thread["state"] == "running"
            || truthy(thread.get("activeInstructionId"))
            || self.active_child_fork(project_id, &thread)?
        {
            return rejected(format!(
                "Thread {} cannot apply a skill from {}",
                string(thread.get("threadId")),
                string(thread.get("state"))
            ));
        }
        thread["inputRefs"] = union(&thread["inputRefs"], &instruction["inputRefs"]);
        self.skill(project_id, text(&instruction["skillId"]), &thread)?;
        thread["activeSkillIds"] =
            union(&thread["activeSkillIds"], &json!([instruction["skillId"]]));
        thread["updatedAt"] = json!(now_utc());
        Ok(self.save_thread(
            &thread,
            version(&stored),
            &format!("{}:skill", text(&instruction["idempotencyKey"])),
            "runtime.skill_applied",
        )?["value"]
            .clone())
    }

    async fn apply_skill_instruction(self: &Rc<Self>, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let thread_id = text(&instruction["threadId"]);
        let skill = self.skill(
            project_id,
            text(&instruction["skillId"]),
            &self.thread(project_id, thread_id)?["value"],
        )?;
        let program_id = text(&skill["programId"]).to_owned();
        let runs_program = skill["executionMode"] == "program" && truthy(skill.get("programId"));
        if runs_program
            && self
                .program_runs
                .borrow()
                .contains_key(&format!("{project_id}:{program_id}"))
        {
            return rejected(format!(
                "Program {program_id} is already running and cannot be invoked by SKILL_APPLY"
            ));
        }
        let thread = self.apply_skill(instruction)?;
        if !runs_program {
            return Ok(json!({ "thread": thread }));
        }
        let program_results = self.run_program(project_id, &program_id).await;
        // `finally`: the program skill is cleared whether or not the run succeeded.
        let stored = self.thread(project_id, thread_id)?;
        if strings(&stored["value"]["activeSkillIds"])
            .iter()
            .any(|id| skill["skillId"] == *id)
        {
            let mut cleared = stored["value"].clone();
            cleared["activeSkillIds"] = Value::Array(
                strings(&cleared["activeSkillIds"])
                    .into_iter()
                    .filter(|id| skill["skillId"] != *id)
                    .map(Value::String)
                    .collect(),
            );
            cleared["updatedAt"] = json!(now_utc());
            self.save_thread(
                &cleared,
                version(&stored),
                &format!(
                    "{}:program-skill-cleared",
                    text(&instruction["idempotencyKey"])
                ),
                "runtime.skill_cleared",
            )?;
        }
        let program_results = program_results?;
        Ok(
            json!({ "thread": self.thread(project_id, thread_id)?["value"], "programResults": program_results }),
        )
    }

    async fn direct_action(&self, instruction: &Value) -> Result<Value> {
        let action = text(&instruction["action"]);
        if truthy(instruction.get("threadId")) {
            let project_id = text(&instruction["projectId"]);
            let thread = self.thread(project_id, text(&instruction["threadId"]))?["value"].clone();
            if self.active_child_fork(project_id, &thread)? {
                return rejected(format!(
                    "Thread {} is blocked by an active foreground fork",
                    string(thread.get("threadId"))
                ));
            }
            if !strings(&thread["capabilities"])
                .iter()
                .any(|capability| capability == action)
            {
                return rejected(format!("Thread lacks capability {action}"));
            }
        }
        let Some(run) = self
            .host
            .direct_action(action, &instruction["payload"], instruction)
        else {
            return rejected(format!("No direct action handler for {action}"));
        };
        run.await
    }

    fn stop(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let key = text(&instruction["idempotencyKey"]);
        let mut program_state = None;
        if truthy(instruction.get("programId")) {
            let program_id = text(&instruction["programId"]);
            let Some(stored) =
                self.host
                    .store(project_id)?
                    .projection(project_id, "program_state", program_id)?
            else {
                return rejected(format!("Unknown program {program_id}"));
            };
            let mut state = stored["value"].clone();
            state["status"] = json!("stopped");
            state["currentStepId"] = Value::Null;
            state["checkpoint"] = json!({ "outcome": "stopped", "reason": instruction["reason"] });
            state["updatedAt"] = json!(now_utc());
            program_state = Some(
                self.save_program_state(
                    &state,
                    version(&stored),
                    &format!("{key}:program-stop"),
                    "runtime.program_stopped",
                )?["value"]
                    .clone(),
            );
        }
        let respond = |mut out: Map<String, Value>| {
            if let Some(state) = &program_state {
                out.insert("programState".into(), state.clone());
            }
            Value::Object(out)
        };
        if !truthy(instruction.get("threadId")) {
            return Ok(respond(Map::new()));
        }
        let stored = self.thread(project_id, text(&instruction["threadId"]))?;
        let mut thread = stored["value"].clone();
        if terminal_thread(&thread) {
            if let Some(agent_id) = self.live_agent(&thread) {
                self.agents.stop(&agent_id);
            }
            return Ok(respond(Map::from_iter([("thread".into(), thread)])));
        }
        if self.active_child_fork(project_id, &thread)? {
            return rejected(format!(
                "Thread {} is blocked by an active foreground fork",
                string(thread.get("threadId"))
            ));
        }
        let defer = strict_equal(
            instruction.get("proposedByAgentId"),
            thread.get("currentAgentId"),
        );
        self.end_session(&mut thread, "completed", defer);
        thread["state"] = json!("completed");
        thread["activeInstructionId"] = Value::Null;
        thread["updatedAt"] = json!(now_utc());
        let saved = self.save_thread(
            &thread,
            version(&stored),
            &format!("{key}:thread-stop"),
            "runtime.thread_stopped",
        )?;
        Ok(respond(Map::from_iter([(
            "thread".into(),
            saved["value"].clone(),
        )])))
    }

    async fn ensure_session(
        &self,
        thread: &mut Value,
        requested_agent_id: Option<&str>,
        tools: Option<Vec<String>>,
    ) -> Result<()> {
        if self.live_agent(thread).is_some() {
            return Ok(());
        }
        if truthy(thread.get("currentAgentId")) {
            self.end_session(thread, "failed", false);
        }
        let index = thread["sessionHistory"].as_array().map_or(0, Vec::len);
        let agent_id = match requested_agent_id {
            Some(id) if index == 0 && !id.is_empty() => id.to_owned(),
            _ => deterministic_id("agt", &format!("{}:{index}", text(&thread["threadId"]))),
        };
        let project_id = text(&thread["projectId"]).to_owned();
        let root = self.host.repository_root(&project_id)?;
        // A task workspace runs in its own worktree, never in the user's main checkout.
        let workspace = &thread["taskWorkspace"];
        let cwd = if truthy(Some(workspace)) {
            root.join(".nosh")
                .join("worktrees")
                .join(text(&workspace["worktreeId"]))
        } else {
            root
        };
        if truthy(Some(workspace)) && !cwd.exists() {
            return rejected(format!(
                "Task workspace worktree {} does not exist",
                string(workspace.get("worktreeId"))
            ));
        }
        let scope = &thread["ownerScope"];
        let options = SessionOptions {
            project_id,
            mission_id: optional_string(&scope["missionId"]),
            direction_id: optional_string(&scope["directionId"]),
            autoresearch_id: optional_string(&scope["autoresearchId"]),
            experiment_id: optional_string(&scope["experimentId"]),
            run_id: None,
            job_id: None,
            task_id: optional_string(&thread["taskId"]),
            agent_id: agent_id.clone(),
            role: from_value(&thread["role"])?,
            cwd,
            package_path: self.host.package_path(),
            tools,
            runtime_scoped: true,
            task_permissions: if truthy(thread.get("taskPermissions")) {
                Some(from_value(&thread["taskPermissions"])?)
            } else {
                None
            },
            task_workspace: if truthy(Some(workspace)) {
                Some(from_value(workspace)?)
            } else {
                None
            },
            thinking_level: None,
            model: None,
        };
        let inspection = self.agents.start(options).await.map_err(Error::Rejected)?;
        thread["currentAgentId"] = json!(agent_id);
        thread["currentPiSessionId"] = json!(inspection.pi_session_id);
        if let Some(history) = thread["sessionHistory"].as_array_mut() {
            history.push(json!({ "agentId": agent_id, "piSessionId": inspection.pi_session_id, "startedAt": inspection.started_at, "endedAt": null, "endReason": null }));
        }
        thread["updatedAt"] = json!(now_utc());
        Ok(())
    }

    fn end_session(&self, thread: &mut Value, reason: &str, defer: bool) {
        if let Some(agent_id) = self.live_agent(thread) {
            if defer {
                // Lets the stopping agent's own tool call finish before its session ends.
                let agents = Rc::clone(&self.agents);
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    agents.stop(&agent_id);
                });
            } else {
                self.agents.stop(&agent_id);
            }
        }
        if let Some(current) = thread["sessionHistory"]
            .as_array_mut()
            .and_then(|history| history.last_mut())
            && !truthy(current.get("endedAt"))
        {
            current["endedAt"] = json!(now_utc());
            current["endReason"] = json!(reason);
        }
        thread["currentAgentId"] = Value::Null;
        thread["currentPiSessionId"] = Value::Null;
        thread["updatedAt"] = json!(now_utc());
    }

    /// The thread's current agent, when its session is still active.
    fn live_agent(&self, thread: &Value) -> Option<String> {
        let agent_id = thread["currentAgentId"]
            .as_str()
            .filter(|id| !id.is_empty())?;
        self.active(agent_id).then(|| agent_id.to_owned())
    }

    fn active(&self, agent_id: &str) -> bool {
        self.agents
            .inspect()
            .iter()
            .any(|agent| agent.agent_id == agent_id)
    }

    fn authorize(&self, instruction: &Value) -> Result<()> {
        let Some(proposer) = instruction["proposedByAgentId"]
            .as_str()
            .filter(|id| !id.is_empty())
        else {
            return Ok(());
        };
        let project_id = text(&instruction["projectId"]);
        let Some(agent) = self
            .agents
            .inspect()
            .into_iter()
            .find(|entry| entry.agent_id == proposer && entry.project_id == project_id)
        else {
            return rejected("Instruction proposer is not an active Pi session");
        };
        let privileged = matches!(
            agent.role.as_str(),
            "nosh" | "mission_director" | "research_director"
        );
        let in_scope = |scope: &Value| {
            scope_is(&scope["missionId"], &agent.mission_id)
                && scope_is(&scope["directionId"], &agent.direction_id)
                && scope_is(&scope["autoresearchId"], &agent.autoresearch_id)
                && scope_is(&scope["experimentId"], &agent.experiment_id)
        };
        let controls = |thread_id: &str| -> Result<bool> {
            let thread = self.thread(project_id, thread_id)?["value"].clone();
            if !in_scope(&thread["ownerScope"]) {
                return Ok(false);
            }
            if privileged || thread["currentAgentId"].as_str() == Some(agent.agent_id.as_str()) {
                return Ok(true);
            }
            if !truthy(thread.get("parentThreadId")) {
                return Ok(false);
            }
            let parent = self.thread(project_id, text(&thread["parentThreadId"]))?;
            Ok(parent["value"]["currentAgentId"].as_str() == Some(agent.agent_id.as_str()))
        };
        let operation = text(&instruction["operation"]);
        match operation {
            "THREAD_OPEN" => {
                let parent = &instruction["parentThreadId"];
                if !in_scope(&instruction["ownerScope"])
                    || !(privileged || (truthy(Some(parent)) && controls(text(parent))?))
                {
                    return rejected("Agent is not authorized to open this logical thread");
                }
                return Ok(());
            }
            "THREAD_FORK" => {
                if !controls(text(&instruction["controllingThreadId"]))? {
                    return rejected("Agent does not control the foreground fork parent");
                }
                return Ok(());
            }
            "THREAD_AWAIT" => {
                for thread_id in strings(&instruction["threadIds"]) {
                    if !controls(&thread_id)? {
                        return rejected("Agent does not control every awaited thread");
                    }
                }
                return Ok(());
            }
            "THREAD_COMPOSE" => {
                if !controls(text(&instruction["targetThreadId"]))? {
                    return rejected("Agent does not control the compose target");
                }
                return Ok(());
            }
            _ => {}
        }
        if matches!(operation, "STOP" | "DIRECT_ACTION")
            && instruction.get("threadId") == Some(&Value::Null)
        {
            if !privileged {
                return rejected("Only a director may issue an unscoped instruction");
            }
            return Ok(());
        }
        if truthy(instruction.get("threadId")) && !controls(text(&instruction["threadId"]))? {
            return rejected("Agent does not control the target thread");
        }
        Ok(())
    }

    fn mark_step_failed(&self, instruction: &Value) -> Result<()> {
        let project_id = text(&instruction["projectId"]);
        let Some(stored) = self.host.store(project_id)?.projection(
            project_id,
            "execution_thread",
            text(&instruction["threadId"]),
        )?
        else {
            return Ok(());
        };
        let state = text(&stored["value"]["state"]);
        if !matches!(state, "running" | "paused")
            || !strict_equal(
                stored["value"].get("activeInstructionId"),
                instruction.get("instructionId"),
            )
        {
            return Ok(());
        }
        let mut thread = stored["value"].clone();
        self.end_session(&mut thread, "failed", false);
        thread["state"] = json!("failed");
        thread["activeInstructionId"] = Value::Null;
        thread["updatedAt"] = json!(now_utc());
        // Preserves the original failure.
        let _ = self.save_thread(
            &thread,
            version(&stored),
            &format!("{}:runtime-failure", text(&instruction["idempotencyKey"])),
            "runtime.thread_step_failed",
        );
        Ok(())
    }

    fn save_thread(
        &self,
        thread: &Value,
        expected_version: i64,
        key: &str,
        event_type: &str,
    ) -> Result<Value> {
        let value = parse("execution-thread", thread)?;
        let project_id = text(&thread["projectId"]);
        let thread_id = text(&thread["threadId"]);
        let draft = draft(project_id, event_type, thread_id, &value, Some(thread));
        let input = ProjectionInput {
            entity_type: "execution_thread".into(),
            entity_id: thread_id.into(),
            state: text(&thread["state"]).into(),
            value,
            graph: None,
            records: vec![],
            intent: None,
        };
        self.mutate(project_id, key, expected_version, &draft, &input)
    }

    fn save_episode(&self, episode: &Value, key: &str) -> Result<()> {
        let project_id = text(&episode["projectId"]);
        let draft = draft(
            project_id,
            "runtime.episode_created",
            text(&episode["threadId"]),
            episode,
            None,
        );
        let input = ProjectionInput {
            entity_type: "episode".into(),
            entity_id: text(&episode["episodeId"]).into(),
            state: text(&episode["status"]).into(),
            value: episode.clone(),
            graph: None,
            records: vec![],
            intent: None,
        };
        self.mutate(project_id, key, 0, &draft, &input)?;
        Ok(())
    }

    /// Writes a projection and publishes its event unless the key replayed; returns the projection.
    fn mutate(
        &self,
        project_id: &str,
        key: &str,
        expected_version: i64,
        draft: &Value,
        input: &ProjectionInput,
    ) -> Result<Value> {
        let result = self.host.store(project_id)?.mutate_projection(
            key,
            Some(expected_version),
            draft,
            input,
        )?;
        if !result.replayed {
            self.host.publish(&result.event);
        }
        Ok(result.projection)
    }

    fn add_child(
        &self,
        project_id: &str,
        parent_id: &str,
        child_id: &str,
        key: &str,
    ) -> Result<()> {
        for attempt in 0..3 {
            let stored = self.thread(project_id, parent_id)?;
            let mut parent = stored["value"].clone();
            parent["childThreadIds"] = union(&parent["childThreadIds"], &json!([child_id]));
            parent["updatedAt"] = json!(now_utc());
            match self.save_thread(&parent, version(&stored), key, "runtime.thread_child_added") {
                Ok(_) => return Ok(()),
                Err(error) if attempt == 2 => return Err(error),
                Err(_) => {}
            }
        }
        Ok(())
    }

    fn active_child_fork(&self, project_id: &str, thread: &Value) -> Result<bool> {
        let store = self.host.store(project_id)?;
        for id in strings(&thread["childThreadIds"]) {
            if let Some(child) = store.projection(project_id, "execution_thread", &id)? {
                let child = &child["value"];
                if child["executionMode"] == "foreground_fork"
                    && matches!(
                        text(&child["state"]),
                        "open" | "running" | "awaiting" | "awaiting_user" | "paused"
                    )
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn skill(&self, project_id: &str, skill_id: &str, thread: &Value) -> Result<Value> {
        let found = self
            .skill_registries
            .borrow_mut()
            .entry(project_id.to_owned())
            .or_default()
            .applicable(skill_id, thread);
        match found {
            Ok(skill) => Ok(skill),
            Err(error) => {
                let Some(stored) = self
                    .host
                    .store(project_id)?
                    .projection(project_id, "skill", skill_id)?
                else {
                    return Err(error.into());
                };
                let manifest = parse("skill-manifest", &stored["value"])?;
                let mut registries = self.skill_registries.borrow_mut();
                let registry = registries.entry(project_id.to_owned()).or_default();
                registry.add(&manifest)?;
                Ok(registry.applicable(skill_id, thread)?)
            }
        }
    }

    fn run_checks(
        &self,
        checks: &Value,
        phase: &str,
        thread: &Value,
        episode: Option<&Value>,
    ) -> Result<()> {
        for check in strings(checks) {
            let passed = match check.as_str() {
                "check_budget" => Some(ensure_can_step(thread).is_ok()),
                "check_episode" => {
                    Some(episode.is_some_and(|episode| episode["status"] == "completed"))
                }
                _ => self.host.skill_check(&check, phase, thread, episode),
            };
            if passed != Some(true) {
                return rejected(format!(
                    "Skill {phase} check failed or is unavailable: {check}"
                ));
            }
        }
        Ok(())
    }
}

fn draft(
    project_id: &str,
    event_type: &str,
    correlation_id: &str,
    payload: &Value,
    thread: Option<&Value>,
) -> Value {
    let field = |value: Option<&Value>| value.cloned().unwrap_or(Value::Null);
    let scope = |key: &str| field(thread.and_then(|thread| thread["ownerScope"].get(key)));
    json!({ "$schema": uri("event"), "schemaVersion": 1, "retention": "persistent", "type": event_type, "source": "orchestration_runtime", "scope": { "projectId": project_id, "missionId": scope("missionId"), "directionId": scope("directionId"), "autoresearchId": scope("autoresearchId"), "experimentId": scope("experimentId"), "runId": null, "jobId": null, "agentId": field(thread.and_then(|thread| thread.get("currentAgentId"))) }, "correlationId": correlation_id, "causationId": null, "payload": payload })
}

fn deterministic_id(prefix: &str, seed: &str) -> String {
    format!("{prefix}_{}", &sha256(&json!(seed))[7..39])
}

fn uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

fn parse(name: &str, value: &Value) -> Result<Value> {
    Ok(validate_record(&uri(name), value)?)
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| string(Some(item)))
        .collect()
}

/// `[...new Set([...a, ...b])]`.
fn union(a: &Value, b: &Value) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for item in a
        .as_array()
        .into_iter()
        .flatten()
        .chain(b.as_array().into_iter().flatten())
    {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    Value::Array(out)
}

fn num(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

/// A JavaScript number as JSON, integral values as integers.
fn number(value: f64) -> Value {
    let mut out = json!(value);
    normalize_numbers(&mut out);
    out
}

/// `${n}` for a JavaScript number.
fn number_text(value: &Value) -> String {
    canonical_json(&number(num(value)))
}

fn version(projection: &Value) -> i64 {
    projection["version"].as_i64().unwrap_or(0)
}

/// `x ?? {}` for a stored result.
fn or_empty(value: &Value) -> Value {
    if value.is_null() {
        json!({})
    } else {
        value.clone()
    }
}

fn optional_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// A thread's owner-scope field against an agent's (`===`, with `null` for none).
fn scope_is(value: &Value, agent: &Option<String>) -> bool {
    match agent {
        Some(id) => value.as_str() == Some(id.as_str()),
        None => value.is_null(),
    }
}

fn from_value<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|error| Error::Rejected(error.to_string()))
}

fn terminal_thread(thread: &Value) -> bool {
    matches!(text(&thread["state"]), "completed" | "cancelled" | "failed")
}

/// `Date.now()`.
pub(crate) fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// `Date.parse` for the ISO timestamps NOSH writes; NaN otherwise.
pub(crate) fn parse_ms(value: &str) -> f64 {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_or(f64::NAN, |at| {
            (at.unix_timestamp_nanos() / 1_000_000) as f64
        })
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_millis() as f64
}

#[cfg(test)]
mod tests {
    use super::deterministic_id;

    #[test]
    fn deterministic_ids_match_typescript() {
        // `${prefix}_${sha256(seed).slice(7, 39)}` from the TypeScript build.
        assert_eq!(
            deterministic_id("agt", "thr_00000000000000000000000000000001:0"),
            "agt_4c596f6662dfb93a1171f93115ae1940"
        );
        assert_eq!(
            deterministic_id("ins", "prg_00000000000000000000000000000002:1:0:step_a"),
            "ins_5fe79b8676458f48f4af79b45d5b8bf3"
        );
        assert_eq!(
            deterministic_id("ivn", "ins_00000000000000000000000000000003"),
            "ivn_9619106e03a86723a21d643f220abfa6"
        );
    }
}
