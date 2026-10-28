//! Skills and bounded programs: registration, program runs with guards, budgets, checkpoints,
//! and background steps whose budget is reserved by a durable operation intent.

use super::*;
use nosh_research::skills::{compile_program, validate_state};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

const RESERVATION: &str = "runtime.program_background_reservation";

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Usage {
    model_tokens: f64,
    tool_calls: f64,
}

impl Usage {
    fn plus(self, other: Usage) -> Usage {
        Usage {
            model_tokens: self.model_tokens + other.model_tokens,
            tool_calls: self.tool_calls + other.tool_calls,
        }
    }
}

/// A background step's budget reservation: its operation intent, the estimate it reserved, and
/// what its instruction actually used once completed.
struct Reservation {
    intent: Value,
    estimate: Usage,
    usage: Usage,
}

struct Prepared {
    key: String,
    request: Value,
    estimate: Usage,
    intent: Option<Value>,
}

/// What a program step's `try` block does next.
enum Flow {
    Next,
    Stopped,
}

impl<H: RuntimeHost + 'static> OrchestrationRuntime<H> {
    pub fn register_skill(
        &self,
        project_id: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        let skill = parse("skill-manifest", input)?;
        let skill_id = text(&skill["skillId"]);
        let store = self.host.store(project_id)?;
        let existing = store.projection(project_id, "skill", skill_id)?;
        let draft = draft(
            project_id,
            "runtime.skill_registered",
            skill_id,
            &skill,
            None,
        );
        let input = ProjectionInput {
            entity_type: "skill".into(),
            entity_id: skill_id.into(),
            state: "active".into(),
            value: skill.clone(),
            graph: None,
            records: vec![],
            intent: None,
        };
        self.mutate(
            project_id,
            idempotency_key,
            existing.as_ref().map_or(0, version),
            &draft,
            &input,
        )?;
        self.skill_registries
            .borrow_mut()
            .entry(project_id.to_owned())
            .or_default()
            .add(&skill)?;
        Ok(skill)
    }

    pub fn register_program(
        &self,
        project_id: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        let program = compile_program(input)?;
        let program_id = text(&program["programId"]);
        let store = self.host.store(project_id)?;
        let existing_projection =
            store.projection(project_id, "orchestration_program", program_id)?;
        let state_projection = store.projection(project_id, "program_state", program_id)?;
        if existing_projection.is_none() && state_projection.is_some() {
            return rejected(format!("Program {program_id} has orphaned state"));
        }
        let existing = existing_projection
            .map(|projection| parse("orchestration-program", &projection["value"]))
            .transpose()?;
        let program_hash = sha256(&program);
        if let Some(existing) = &existing {
            if sha256(existing) != program_hash {
                return rejected(format!(
                    "Program {program_id} does not match the existing canonical program"
                ));
            }
            if existing["programId"] != program["programId"] {
                return rejected(format!("Stored program identity mismatch for {program_id}"));
            }
        }
        let prior_state = state_projection
            .as_ref()
            .map(|projection| parse("program-state", &projection["value"]))
            .transpose()?;
        if let Some(prior) = &prior_state {
            if prior["projectId"] != project_id
                || prior["programId"] != program["programId"]
                || prior["namespace"] != program["state"]["namespace"]
                || prior["fieldTypes"] != program["state"]["schema"]
            {
                return rejected(format!("Stored state identity mismatch for {program_id}"));
            }
            validate_state(
                object(&program["state"]["schema"]),
                object(&prior["values"]),
            )?;
        }
        if let Some(existing) = &existing {
            let intent = json!({ "command": "registerProgram", "programId": program_id, "programHash": program_hash });
            if let Some(receipt) = store.command_receipt(project_id, idempotency_key)?
                && (receipt["event"]["type"] != "runtime.program_registered"
                    || receipt["event"]["correlationId"] != program_id
                    || receipt["intentHash"] != sha256(&intent).as_str())
            {
                return rejected("Idempotency key was reused for a different program registration");
            }
            if prior_state.is_some() {
                self.programs
                    .borrow_mut()
                    .insert(format!("{project_id}:{program_id}"), existing.clone());
                return Ok(existing.clone());
            }
        }
        let state = match prior_state {
            Some(state) => state,
            None => parse(
                "program-state",
                &json!({ "$schema": uri("program-state"), "schemaVersion": 1, "projectId": project_id, "programId": program_id, "namespace": program["state"]["namespace"], "fieldTypes": program["state"]["schema"], "values": program["state"]["initial"], "status": "idle", "currentStepId": null, "stepsExecuted": 0, "executionCount": 0, "usage": { "toolCalls": 0, "modelTokens": 0, "wallClockSeconds": 0 }, "checkpoint": null, "startedAt": null, "updatedAt": now_utc() }),
            )?,
        };
        validate_state(
            object(&program["state"]["schema"]),
            object(&state["values"]),
        )?;
        if existing.is_none() {
            let draft = draft(
                project_id,
                "runtime.program_registered",
                program_id,
                &program,
                None,
            );
            let intent = json!({ "command": "registerProgram", "programId": program_id, "programHash": program_hash });
            let input = ProjectionInput {
                entity_type: "orchestration_program".into(),
                entity_id: program_id.into(),
                state: "active".into(),
                value: program.clone(),
                graph: None,
                records: vec![],
                intent: Some(intent),
            };
            self.mutate(project_id, idempotency_key, 0, &draft, &input)?;
        }
        let draft = draft(
            project_id,
            "runtime.program_state_declared",
            program_id,
            &state,
            None,
        );
        let intent = json!({ "command": "registerProgramState", "programId": program_id, "programHash": program_hash });
        let input = ProjectionInput {
            entity_type: "program_state".into(),
            entity_id: program_id.into(),
            state: text(&state["status"]).into(),
            value: state.clone(),
            graph: None,
            records: vec![],
            intent: Some(intent),
        };
        self.mutate(
            project_id,
            &format!("{idempotency_key}:state"),
            state_projection.as_ref().map_or(0, version),
            &draft,
            &input,
        )?;
        self.programs
            .borrow_mut()
            .insert(format!("{project_id}:{program_id}"), program.clone());
        Ok(program)
    }

    /// Runs a registered program to completion, once at a time per program: a concurrent call
    /// joins the running one.
    pub fn run_program(
        self: &Rc<Self>,
        project_id: &str,
        program_id: &str,
    ) -> SharedResult<Vec<Value>> {
        let key = format!("{project_id}:{program_id}");
        if let Some(running) = self.program_runs.borrow().get(&key).cloned() {
            return running;
        }
        let this = Rc::clone(self);
        let (project, program, entry) = (project_id.to_owned(), program_id.to_owned(), key.clone());
        let run = async move {
            let result = this.run_program_one(&project, &program).await;
            this.program_runs.borrow_mut().remove(&entry);
            result
        }
        .boxed_local()
        .shared();
        // Not started eagerly, unlike `execute`: every JavaScript `await` yields, so a run is still
        // in the map for a caller arriving in the same tick even when its steps finish at once.
        self.program_runs.borrow_mut().insert(key, run.clone());
        run
    }

    async fn run_program_one(
        self: &Rc<Self>,
        project_id: &str,
        program_id: &str,
    ) -> Result<Vec<Value>> {
        let program = self.program(project_id, program_id)?;
        let store = self.host.store(project_id)?;
        let Some(mut stored) = store.projection(project_id, "program_state", program_id)? else {
            return rejected(format!("Program {program_id} has no declared state"));
        };
        let mut state = stored["value"].clone();
        if state["status"] != "running" {
            let now = now_utc();
            state["status"] = json!("running");
            state["currentStepId"] = program["startStepId"].clone();
            state["stepsExecuted"] = json!(0);
            state["executionCount"] = number(num(&state["executionCount"]) + 1.0);
            state["usage"] = json!({ "toolCalls": 0, "modelTokens": 0, "wallClockSeconds": 0 });
            state["checkpoint"] = Value::Null;
            state["startedAt"] = json!(now);
            state["updatedAt"] = json!(now);
            stored = self.save_program_state(
                &state,
                version(&stored),
                &format!(
                    "program:{program_id}:{}:start",
                    number_text(&state["executionCount"])
                ),
                "runtime.program_started",
            )?;
        }
        let execution_count = num(&state["executionCount"]);
        let count = number_text(&state["executionCount"]);
        let steps = program["steps"].as_array().cloned().unwrap_or_default();
        let mut results: Vec<Value> = Vec::new();
        let started = parse_ms(text(&state["startedAt"]));
        while truthy(state.get("currentStepId")) {
            if num(&state["stepsExecuted"]) >= num(&program["maximumSteps"]) {
                return Err(self.fail_program(
                    &stored,
                    &mut state,
                    format!("Program {program_id} exceeded maximumSteps"),
                ));
            }
            if now_ms() - started > num(&program["maximumWallClockSeconds"]) * 1_000.0 {
                return Err(self.fail_program(
                    &stored,
                    &mut state,
                    format!("Program {program_id} exceeded its wall-clock bound"),
                ));
            }
            if num(&state["usage"]["modelTokens"]) >= num(&program["maximumModelTokens"])
                || num(&state["usage"]["toolCalls"]) >= num(&program["maximumToolCalls"])
            {
                return Err(self.fail_program(
                    &stored,
                    &mut state,
                    format!("Program {program_id} exhausted its agent budget"),
                ));
            }
            let Some(step) = steps
                .iter()
                .find(|candidate| strict_equal(candidate.get("stepId"), state.get("currentStepId")))
                .cloned()
            else {
                let message = format!(
                    "Program step {} is missing",
                    string(state.get("currentStepId"))
                );
                return Err(self.fail_program(&stored, &mut state, message));
            };
            let step_id = text(&step["stepId"]).to_owned();
            if !self.guard(project_id, &step["guard"], &state["values"])? {
                state["currentStepId"] = step["failureStepId"].clone();
                state["stepsExecuted"] = number(num(&state["stepsExecuted"]) + 1.0);
                state["checkpoint"] = json!({ "stepId": step_id, "outcome": "guard_false" });
                state["updatedAt"] = json!(now_utc());
                stored = self.save_program_state(
                    &state,
                    version(&stored),
                    &format!(
                        "program:{program_id}:{count}:step:{}",
                        number_text(&state["stepsExecuted"])
                    ),
                    "runtime.program_checkpointed",
                )?;
                continue;
            }
            let mut body = step["instruction"].clone();
            if body["operation"] == "THREAD_STEP"
                && let Some(objective) = body["objective"].as_str()
            {
                body["objective"] = json!(format!(
                    "{objective}\n\nProgram state ({}): {}",
                    string(state.get("namespace")),
                    canonical_json(&state["values"])
                ));
            }
            let step_number = num(&state["stepsExecuted"]);
            let seed = format!(
                "{program_id}:{count}:{}:{step_id}",
                number_text(&state["stepsExecuted"])
            );
            let mut envelope = body.as_object().cloned().unwrap_or_default();
            for (key, value) in [
                ("$schema", json!(uri("runtime-instruction"))),
                ("schemaVersion", json!(1)),
                ("instructionId", json!(deterministic_id("ins", &seed))),
                ("projectId", json!(project_id)),
                ("idempotencyKey", json!(format!("program:{seed}"))),
                ("proposedByAgentId", Value::Null),
                ("issuedAt", state["startedAt"].clone()),
            ] {
                envelope.insert(key.into(), value);
            }
            let instruction = parse("runtime-instruction", &Value::Object(envelope))?;
            let background = truthy(step.get("background"));
            let attempt: Result<Flow> = async {
                let instruction_key = text(&instruction["idempotencyKey"]);
                self.reconcile_background_reservations(project_id, program_id, execution_count, background.then_some(instruction_key))?;
                if !background {
                    let reservations = self.background_reservations(project_id, program_id, execution_count)?;
                    if reservations.iter().any(|reservation| reservation.intent["state"] == "failed") {
                        return Err(self.fail_program(&stored, &mut state, "Background program step failed".into()));
                    }
                    let consuming = instruction["operation"] == "THREAD_STEP"
                        || (instruction["operation"] == "SKILL_APPLY" && {
                            let thread = self.thread(project_id, text(&instruction["threadId"]))?["value"].clone();
                            let skill = self.skill(project_id, text(&instruction["skillId"]), &thread)?;
                            skill["executionMode"] == "program" && truthy(skill.get("programId"))
                        });
                    let usage = self.background_usage(project_id, program_id, execution_count)?;
                    if consuming && (num(&state["usage"]["modelTokens"]) + usage.model_tokens >= num(&program["maximumModelTokens"]) || num(&state["usage"]["toolCalls"]) + usage.tool_calls >= num(&program["maximumToolCalls"])) {
                        return Err(self.fail_program(&stored, &mut state, "Program background budget exhausted".into()));
                    }
                } else {
                    let reservation = self.prepare_background_reservation(project_id, program_id, execution_count, step_number, &step_id, &instruction)?;
                    let usage = self.background_usage(project_id, program_id, execution_count)?;
                    let candidate = if reservation.intent.is_some() { Usage::default() } else { reservation.estimate };
                    if num(&state["usage"]["modelTokens"]) + usage.model_tokens + candidate.model_tokens > num(&program["maximumModelTokens"]) || num(&state["usage"]["toolCalls"]) + usage.tool_calls + candidate.tool_calls > num(&program["maximumToolCalls"]) {
                        return Err(self.fail_program(&stored, &mut state, "Program background budget exhausted".into()));
                    }
                    if reservation.intent.is_none() {
                        store.begin_operation(project_id, RESERVATION, &reservation.key, &reservation.request)?;
                    }
                }
                if background {
                    // `void this.execute(instruction).catch(() => undefined)`.
                    if let Ok(execution) = self.start_execution(&instruction) {
                        tokio::task::spawn_local(async move {
                            let _ = execution.await;
                        });
                    }
                } else {
                    let result = self.execute(&instruction).await?;
                    results.push(result.clone());
                    if truthy(step.get("outputStateKey")) {
                        let mut values = state["values"].as_object().cloned().unwrap_or_default();
                        values.insert(text(&step["outputStateKey"]).into(), result.clone());
                        validate_state(object(&state["fieldTypes"]), &values)?;
                        state["values"] = Value::Object(values);
                    }
                    if instruction["operation"] == "THREAD_STEP" {
                        self.add_usage(&mut state, &result)?;
                    } else if instruction["operation"] == "SKILL_APPLY" && result.get("programResults").is_some() {
                        self.add_usage(&mut state, &result["programResults"])?;
                    }
                }
                if instruction["operation"] == "STOP"
                    && instruction["programId"] == program_id
                    && store.projection(project_id, "program_state", program_id)?.is_some_and(|projection| projection["value"]["status"] == "stopped")
                {
                    return Ok(Flow::Stopped);
                }
                state["currentStepId"] = step["nextStepId"].clone();
                state["stepsExecuted"] = number(num(&state["stepsExecuted"]) + 1.0);
                state["checkpoint"] = json!({ "stepId": step_id, "outcome": if background { "spawned" } else { "completed" } });
                state["updatedAt"] = json!(now_utc());
                stored = self.save_program_state(&state, version(&stored), &format!("program:{program_id}:{count}:step:{}", number_text(&state["stepsExecuted"])), "runtime.program_checkpointed")?;
                Ok(Flow::Next)
            }
            .await;
            match attempt {
                Ok(Flow::Stopped) => return Ok(results),
                Ok(Flow::Next) => {}
                Err(error) => {
                    if !truthy(step.get("failureStepId")) {
                        return Err(self.fail_program(&stored, &mut state, error.to_string()));
                    }
                    state["currentStepId"] = step["failureStepId"].clone();
                    state["stepsExecuted"] = number(num(&state["stepsExecuted"]) + 1.0);
                    state["checkpoint"] = json!({ "stepId": step_id, "outcome": "failed", "error": error.to_string() });
                    state["updatedAt"] = json!(now_utc());
                    stored = self.save_program_state(
                        &state,
                        version(&stored),
                        &format!(
                            "program:{program_id}:{count}:step:{}",
                            number_text(&state["stepsExecuted"])
                        ),
                        "runtime.program_checkpointed",
                    )?;
                }
            }
        }
        let prefix = format!("program:{program_id}:{count}:");
        for intent in store
            .operation_intents(project_id, Some("pending"))?
            .into_iter()
            .filter(|intent| text(&intent["idempotencyKey"]).starts_with(&prefix))
        {
            if let Err(error) = self.execute(&intent["request"]).await {
                return Err(self.fail_program(&stored, &mut state, error.to_string()));
            }
        }
        let reservations = match self
            .reconcile_background_reservations(project_id, program_id, execution_count, None)
            .and_then(|()| self.background_reservations(project_id, program_id, execution_count))
        {
            Ok(reservations) => reservations,
            Err(error) => return Err(self.fail_program(&stored, &mut state, error.to_string())),
        };
        if reservations
            .iter()
            .any(|reservation| reservation.intent["state"] == "failed")
        {
            return Err(self.fail_program(
                &stored,
                &mut state,
                "Background program step failed".into(),
            ));
        }
        for reservation in reservations
            .iter()
            .filter(|reservation| reservation.intent["state"] == "completed")
        {
            state["usage"]["modelTokens"] =
                number(num(&state["usage"]["modelTokens"]) + reservation.usage.model_tokens);
            state["usage"]["toolCalls"] =
                number(num(&state["usage"]["toolCalls"]) + reservation.usage.tool_calls);
        }
        let intents: Vec<Value> = store
            .operation_intents(project_id, None)?
            .into_iter()
            .filter(|intent| text(&intent["idempotencyKey"]).starts_with(&prefix))
            .collect();
        let ends_with_step = |intent: &Value, step: &Value| {
            text(&intent["idempotencyKey"]).ends_with(&format!(":{}", string(step.get("stepId"))))
        };
        let failed = intents.iter().any(|intent| {
            let request = &intent["request"];
            if !steps
                .iter()
                .any(|step| truthy(step.get("background")) && ends_with_step(intent, step))
                || intent["state"] != "failed"
                || !request["threadId"].is_string()
            {
                return false;
            }
            !steps.iter().any(|step| {
                step["instruction"].is_object()
                    && step["instruction"]["operation"] == "THREAD_STEP"
                    && step["instruction"]["threadId"] == request["threadId"]
                    && truthy(step.get("background"))
                    && intents
                        .iter()
                        .any(|other| ends_with_step(other, step) && other["state"] == "completed")
            })
        });
        if failed {
            return Err(self.fail_program(
                &stored,
                &mut state,
                "Background program step failed".into(),
            ));
        }
        let durable: Vec<Value> = intents
            .iter()
            .filter(|intent| intent["state"] == "completed")
            .map(|intent| or_empty(&intent["result"]))
            .collect();
        let wall = nosh_wire::js::max(
            num(&state["usage"]["wallClockSeconds"]),
            ((now_ms() - started) / 1_000.0).ceil(),
        );
        state["usage"]["wallClockSeconds"] = number(wall);
        if num(&state["usage"]["modelTokens"]) > num(&program["maximumModelTokens"])
            || num(&state["usage"]["toolCalls"]) > num(&program["maximumToolCalls"])
            || wall > num(&program["maximumWallClockSeconds"])
        {
            return Err(self.fail_program(&stored, &mut state, "Program budget exhausted".into()));
        }
        state["status"] = json!("completed");
        state["checkpoint"] = json!({ "outcome": "completed", "results": durable.len() });
        state["updatedAt"] = json!(now_utc());
        self.save_program_state(
            &state,
            version(&stored),
            &format!("program:{program_id}:{count}:completed"),
            "runtime.program_completed",
        )?;
        Ok(if durable.is_empty() { results } else { durable })
    }

    pub(super) fn save_program_state(
        &self,
        state: &Value,
        expected_version: i64,
        key: &str,
        event_type: &str,
    ) -> Result<Value> {
        validate_state(object(&state["fieldTypes"]), object(&state["values"]))?;
        let value = parse("program-state", state)?;
        let project_id = text(&state["projectId"]);
        let program_id = text(&state["programId"]);
        let draft = draft(project_id, event_type, program_id, &value, None);
        let input = ProjectionInput {
            entity_type: "program_state".into(),
            entity_id: program_id.into(),
            state: text(&state["status"]).into(),
            value,
            graph: None,
            records: vec![],
            intent: None,
        };
        self.mutate(project_id, key, expected_version, &draft, &input)
    }

    /// Records the program as failed and returns the failure (`failProgram` throws it).
    fn fail_program(&self, stored: &Value, state: &mut Value, message: String) -> Error {
        state["status"] = json!("failed");
        state["checkpoint"] = json!({ "outcome": "failed", "error": message });
        state["updatedAt"] = json!(now_utc());
        let key = format!(
            "program:{}:{}:failed:{}",
            text(&state["programId"]),
            number_text(&state["executionCount"]),
            number_text(&state["stepsExecuted"])
        );
        match self.save_program_state(state, version(stored), &key, "runtime.program_failed") {
            Ok(_) => Error::Rejected(message),
            Err(error) => error,
        }
    }

    fn program(&self, project_id: &str, program_id: &str) -> Result<Value> {
        let key = format!("{project_id}:{program_id}");
        if let Some(active) = self.programs.borrow().get(&key) {
            return Ok(active.clone());
        }
        let Some(stored) = self.host.store(project_id)?.projection(
            project_id,
            "orchestration_program",
            program_id,
        )?
        else {
            return rejected(format!("Unknown program {program_id}"));
        };
        let program = compile_program(&stored["value"])?;
        self.programs.borrow_mut().insert(key, program.clone());
        Ok(program)
    }

    fn guard(&self, project_id: &str, guard: &Value, values: &Value) -> Result<bool> {
        if !truthy(Some(guard)) {
            return Ok(true);
        }
        let reference = text(&guard["ref"]);
        let value = match values.get(reference) {
            Some(value) => Some(value.clone()),
            None => self.reference_value(project_id, reference)?,
        };
        if guard["operator"] == "exists" {
            return Ok(value.is_some());
        }
        let equal = value.as_ref() == guard.get("value");
        Ok(if guard["operator"] == "equals" {
            equal
        } else {
            !equal
        })
    }

    fn add_usage(&self, state: &mut Value, result: &Value) -> Result<()> {
        let project_id = text(&state["projectId"]).to_owned();
        let store = self.host.store(&project_id)?;
        for reference in unique(program_episode_refs(result)) {
            let Some(episode) = store.projection(&project_id, "episode", &reference)? else {
                return rejected(format!("Missing durable Episode {reference}"));
            };
            for field in ["toolCalls", "modelTokens", "wallClockSeconds"] {
                state["usage"][field] =
                    number(num(&state["usage"][field]) + num(&episode["value"]["usage"][field]));
            }
        }
        Ok(())
    }

    fn background_reservations(
        &self,
        project_id: &str,
        program_id: &str,
        execution_count: f64,
    ) -> Result<Vec<Reservation>> {
        let count = number_text(&number(execution_count));
        let prefix = format!("runtime-program-reservation:{program_id}:{count}:");
        let intents = self
            .host
            .store(project_id)?
            .operation_intents(project_id, None)?;
        intents
            .into_iter()
            .filter(|intent| {
                intent["operationType"] == RESERVATION
                    && text(&intent["idempotencyKey"]).starts_with(&prefix)
            })
            .map(|intent| {
                let request = &intent["request"];
                let step_number = request["stepNumber"].as_f64().unwrap_or(-1.0);
                let step_text = number_text(&number(step_number));
                let seed = format!(
                    "{program_id}:{count}:{step_text}:{}",
                    text(&request["stepId"])
                );
                let corrupt = !request.is_object()
                    || request["projectId"] != project_id
                    || request["programId"] != program_id
                    || request["executionCount"].as_f64() != Some(execution_count)
                    || !is_integer(step_number)
                    || step_number < 0.0
                    || !request["stepId"].is_string()
                    || intent["idempotencyKey"]
                        != format!("runtime-program-reservation:{seed}").as_str()
                    || request["instructionIdempotencyKey"] != format!("program:{seed}").as_str()
                    || request["instructionId"] != deterministic_id("ins", &seed).as_str()
                    || !count_value(&request["modelTokens"])
                    || !count_value(&request["toolCalls"]);
                if corrupt {
                    return rejected("Corrupt background reservation");
                }
                let result = &intent["result"];
                let usage = if intent["state"] == "completed" {
                    if !(count_value(&result["modelTokens"]) && count_value(&result["toolCalls"])) {
                        return rejected("Corrupt completed background reservation");
                    }
                    Usage {
                        model_tokens: num(&result["modelTokens"]),
                        tool_calls: num(&result["toolCalls"]),
                    }
                } else {
                    Usage::default()
                };
                let estimate = Usage {
                    model_tokens: num(&request["modelTokens"]),
                    tool_calls: num(&request["toolCalls"]),
                };
                Ok(Reservation {
                    intent,
                    estimate,
                    usage,
                })
            })
            .collect()
    }

    fn reconcile_background_reservations(
        &self,
        project_id: &str,
        program_id: &str,
        execution_count: f64,
        allowed_pending: Option<&str>,
    ) -> Result<()> {
        static STEP_KEY: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^([0-9]+):(.+)$").expect("a valid pattern"));
        let store = self.host.store(project_id)?;
        let count = number_text(&number(execution_count));
        let prefix = format!("program:{program_id}:{count}:");
        let program = self.program(project_id, program_id)?;
        let steps = program["steps"].as_array().cloned().unwrap_or_default();
        let reservations = self.background_reservations(project_id, program_id, execution_count)?;
        let intents = store.operation_intents(project_id, None)?;
        for instruction in intents.iter().filter(|intent| {
            intent["operationType"] != RESERVATION
                && text(&intent["idempotencyKey"]).starts_with(&prefix)
        }) {
            let key = text(&instruction["idempotencyKey"]);
            let Some(found) = STEP_KEY.captures(&key[prefix.len()..]) else {
                return rejected("Malformed Program instruction idempotency key");
            };
            let step_number = found[1].parse::<f64>().unwrap_or(f64::NAN);
            let step_id = &found[2];
            let seed = format!(
                "{program_id}:{count}:{}:{step_id}",
                number_text(&number(step_number))
            );
            let Some(step) = steps
                .iter()
                .find(|candidate| candidate["stepId"] == step_id)
            else {
                return rejected("Mismatched Program instruction identity");
            };
            if !is_safe_integer(step_number) || key != format!("program:{seed}") {
                return rejected("Mismatched Program instruction identity");
            }
            let parsed = parse("runtime-instruction", &instruction["request"])?;
            if parsed["projectId"] != project_id
                || parsed["idempotencyKey"] != key
                || parsed["instructionId"] != deterministic_id("ins", &seed).as_str()
                || instruction["operationType"]
                    != format!("runtime.{}", text(&parsed["operation"]).to_lowercase()).as_str()
            {
                return rejected("Mismatched Program instruction envelope");
            }
            let matches: Vec<&Reservation> = reservations
                .iter()
                .filter(|reservation| {
                    reservation.intent["request"]["instructionIdempotencyKey"] == key
                })
                .collect();
            let background = truthy(step.get("background"));
            if background && matches.len() != 1 {
                return rejected("Missing or duplicate background reservation");
            }
            if !background && !matches.is_empty() {
                return rejected("Foreground instruction has a background reservation");
            }
            if background {
                let reservation = matches[0];
                let reservation_id = text(&reservation.intent["intentId"]);
                if instruction["state"] == "completed" {
                    let usage = self.usage_from_instruction(
                        project_id,
                        &parsed,
                        &or_empty(&instruction["result"]),
                    )?;
                    if reservation.intent["state"] == "pending" {
                        store.complete_operation(project_id, reservation_id, &json!({ "modelTokens": number(usage.model_tokens), "toolCalls": number(usage.tool_calls) }))?;
                    } else if reservation.usage != usage {
                        return rejected("Mismatched completed background reservation");
                    }
                } else if instruction["state"] == "failed"
                    && reservation.intent["state"] == "pending"
                {
                    store.fail_operation(
                        project_id,
                        reservation_id,
                        instruction["error"]
                            .as_str()
                            .unwrap_or("Background instruction failed"),
                    )?;
                }
            }
        }
        for reservation in &reservations {
            let wanted = &reservation.intent["request"]["instructionIdempotencyKey"];
            let instruction = intents.iter().find(|intent| {
                intent["operationType"] != RESERVATION && intent["idempotencyKey"] == *wanted
            });
            if instruction.is_none()
                && (reservation.intent["state"] != "pending"
                    || allowed_pending.is_none_or(|allowed| wanted.as_str() != Some(allowed)))
            {
                return rejected("Missing background instruction for reservation");
            }
        }
        Ok(())
    }

    fn prepare_background_reservation(
        &self,
        project_id: &str,
        program_id: &str,
        execution_count: f64,
        step_number: f64,
        step_id: &str,
        instruction: &Value,
    ) -> Result<Prepared> {
        let store = self.host.store(project_id)?;
        let seed = format!(
            "{program_id}:{}:{}:{step_id}",
            number_text(&number(execution_count)),
            number_text(&number(step_number))
        );
        let key = format!("runtime-program-reservation:{seed}");
        let prior = store
            .operation_intents(project_id, None)?
            .into_iter()
            .find(|intent| {
                intent["operationType"] == RESERVATION && intent["idempotencyKey"] == key.as_str()
            });
        if instruction["idempotencyKey"] != format!("program:{seed}").as_str()
            || instruction["instructionId"] != deterministic_id("ins", &seed).as_str()
        {
            return rejected("Mismatched background instruction identity");
        }
        let thread = if truthy(instruction.get("threadId")) {
            Some(self.thread(project_id, text(&instruction["threadId"]))?["value"].clone())
        } else {
            None
        };
        if let Some(prior) = prior {
            let request = &prior["request"];
            if !request.is_object()
                || request["projectId"] != project_id
                || request["programId"] != program_id
                || request["executionCount"].as_f64() != Some(execution_count)
                || request["stepNumber"].as_f64() != Some(step_number)
                || request["stepId"] != step_id
                || request["instructionId"] != instruction["instructionId"]
                || request["instructionIdempotencyKey"] != instruction["idempotencyKey"]
            {
                return rejected("Mismatched background reservation");
            }
            let estimate = Usage {
                model_tokens: num(&request["modelTokens"]),
                tool_calls: num(&request["toolCalls"]),
            };
            return Ok(Prepared {
                key,
                request: request.clone(),
                estimate,
                intent: Some(prior),
            });
        }
        let mut estimate = Usage::default();
        if instruction["operation"] == "THREAD_STEP" {
            let Some(thread) = &thread else {
                return rejected("Background thread step has no thread");
            };
            ensure_can_step(thread)?;
            estimate = Usage {
                model_tokens: nosh_wire::js::max(
                    0.0,
                    num(&thread["budget"]["maximumModelTokens"])
                        - num(&thread["usage"]["modelTokens"]),
                ),
                tool_calls: nosh_wire::js::max(
                    0.0,
                    num(&thread["budget"]["maximumToolCalls"]) - num(&thread["usage"]["toolCalls"]),
                ),
            };
        } else if instruction["operation"] == "SKILL_APPLY"
            && let Some(thread) = &thread
        {
            let skill = self.skill(project_id, text(&instruction["skillId"]), thread)?;
            if skill["executionMode"] == "program" && truthy(skill.get("programId")) {
                let nested = self.program(project_id, text(&skill["programId"]))?;
                estimate = Usage {
                    model_tokens: num(&nested["maximumModelTokens"]),
                    tool_calls: num(&nested["maximumToolCalls"]),
                };
            }
        }
        let request = json!({ "projectId": project_id, "programId": program_id, "executionCount": number(execution_count), "stepNumber": number(step_number), "stepId": step_id, "instructionId": instruction["instructionId"], "instructionIdempotencyKey": instruction["idempotencyKey"], "modelTokens": number(estimate.model_tokens), "toolCalls": number(estimate.tool_calls) });
        Ok(Prepared {
            key,
            request,
            estimate,
            intent: None,
        })
    }

    fn background_usage(
        &self,
        project_id: &str,
        program_id: &str,
        execution_count: f64,
    ) -> Result<Usage> {
        Ok(self
            .background_reservations(project_id, program_id, execution_count)?
            .iter()
            .fold(Usage::default(), |total, reservation| {
                total.plus(match text(&reservation.intent["state"]) {
                    "completed" => reservation.usage,
                    "pending" => reservation.estimate,
                    _ => Usage::default(),
                })
            }))
    }

    fn usage_from_instruction(
        &self,
        project_id: &str,
        instruction: &Value,
        result: &Value,
    ) -> Result<Usage> {
        let source = if instruction["operation"] == "THREAD_STEP" {
            result.clone()
        } else if instruction["operation"] == "SKILL_APPLY"
            && result.get("programResults").is_some()
        {
            or_empty(&result["programResults"])
        } else {
            json!({})
        };
        let store = self.host.store(project_id)?;
        let mut total = Usage::default();
        for reference in unique(program_episode_refs(&source)) {
            let Some(episode) = store.projection(project_id, "episode", &reference)? else {
                return rejected(format!("Missing durable Episode {reference}"));
            };
            total = total.plus(Usage {
                model_tokens: num(&episode["value"]["usage"]["modelTokens"]),
                tool_calls: num(&episode["value"]["usage"]["toolCalls"]),
            });
        }
        Ok(total)
    }
}

/// The Episode IDs a step or program result carries, nested program results included.
fn program_episode_refs(value: &Value) -> Vec<String> {
    if let Some(items) = value.as_array() {
        return items.iter().flat_map(program_episode_refs).collect();
    }
    if !value.is_object() {
        return Vec::new();
    }
    let mut refs: Vec<String> = value["episode"]["episodeId"]
        .as_str()
        .map(str::to_owned)
        .into_iter()
        .collect();
    if let Some(nested) = value["programResults"].as_array() {
        refs.extend(nested.iter().flat_map(program_episode_refs));
    }
    refs
}

fn unique(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

fn object(value: &Value) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    value.as_object().unwrap_or(&EMPTY)
}

fn is_integer(value: f64) -> bool {
    value.is_finite() && value.trunc() == value
}

fn is_safe_integer(value: f64) -> bool {
    is_integer(value) && value.abs() <= 9_007_199_254_740_991.0
}

/// A non-negative integer count.
fn count_value(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|count| is_integer(count) && count >= 0.0)
}
