//! Thread steps and joins (`step`, `awaitThreads`, `waitForEpisode`, `minimumSuccess`).

use super::*;
use futures_util::StreamExt;
use futures_util::future::{join_all, select_ok};
use futures_util::stream::FuturesUnordered;
use nosh_agent::pi::TerminalStep;
use nosh_research::episodes::{create_episode, fallback_episode_draft};
use nosh_research::threads::render_thread_context;
use nosh_wire::event_budget_tokens;

impl<H: RuntimeHost + 'static> OrchestrationRuntime<H> {
    pub(super) async fn step(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]);
        let instruction_id = text(&instruction["instructionId"]);
        let key = text(&instruction["idempotencyKey"]);
        let expected_type = text(&instruction["expectedEpisodeType"]);
        let mut stored = self.thread(project_id, text(&instruction["threadId"]))?;
        let mut thread = stored["value"].clone();
        let thread_id = text(&thread["threadId"]).to_owned();
        let prior_episode = self
            .episodes(project_id)?
            .into_iter()
            .map(|entry| entry["value"].clone())
            .find(|episode| episode["instructionId"] == instruction_id);
        if let Some(prior) = prior_episode {
            if !thread["episodeIds"]
                .as_array()
                .is_some_and(|ids| ids.contains(&prior["episodeId"]))
            {
                if thread["state"] == "running"
                    && !strict_equal(
                        thread.get("activeInstructionId"),
                        instruction.get("instructionId"),
                    )
                {
                    return rejected(format!("Thread {thread_id} is running another instruction"));
                }
                if truthy(thread.get("currentAgentId")) && self.live_agent(&thread).is_none() {
                    self.end_session(&mut thread, "failed", false);
                }
                push(&mut thread["episodeIds"], prior["episodeId"].clone());
                thread["nextStepNumber"] = number(nosh_wire::js::max(
                    num(&thread["nextStepNumber"]),
                    num(&prior["stepNumber"]) + 1.0,
                ));
                for field in ["toolCalls", "modelTokens", "wallClockSeconds"] {
                    thread["usage"][field] =
                        number(num(&thread["usage"][field]) + num(&prior["usage"][field]));
                }
                thread["activeInstructionId"] = Value::Null;
                thread["state"] = json!(if prior["status"] == "completed" {
                    settled_state(&thread)
                } else {
                    "failed"
                });
                thread["updatedAt"] = json!(now_utc());
                self.save_thread(
                    &thread,
                    version(&stored),
                    &format!("{key}:episode-recovery"),
                    "runtime.thread_step_recovered",
                )?;
            }
            return Ok(prior);
        }
        if thread["state"] == "running" {
            if !strict_equal(
                thread.get("activeInstructionId"),
                instruction.get("instructionId"),
            ) || self.live_agent(&thread).is_some()
            {
                return rejected(format!(
                    "Thread {thread_id} is already running another instruction"
                ));
            }
            self.end_session(&mut thread, "failed", false);
            thread["activeInstructionId"] = Value::Null;
            thread["state"] = json!("open");
            thread["updatedAt"] = json!(now_utc());
            stored = self.save_thread(
                &thread,
                version(&stored),
                &format!("{key}:session-recovery"),
                "runtime.thread_session_recovered",
            )?;
        }
        ensure_can_step(&thread)?;
        if self.active_child_fork(project_id, &thread)? {
            return rejected(format!(
                "Thread {thread_id} is blocked by an active foreground fork"
            ));
        }
        let skill_ids = union(&thread["activeSkillIds"], &instruction["skillIds"]);
        let active_skills = strings(&skill_ids)
            .iter()
            .map(|skill_id| self.skill(project_id, skill_id, &thread))
            .collect::<Result<Vec<_>>>()?;
        let store = self.host.store(project_id)?;
        let mut input_episode_types = Vec::new();
        for reference in strings(&thread["inputRefs"])
            .into_iter()
            .chain(strings(&instruction["inputRefs"]))
        {
            if let Some(episode) = store.projection(project_id, "episode", &reference)?
                && let Some(episode_type) = episode["value"].get("episodeType")
            {
                input_episode_types.push(string(Some(episode_type)));
            }
        }
        for skill in &active_skills {
            let skill_id = string(skill.get("skillId"));
            if skill["outputEpisodeType"] != expected_type {
                return rejected(format!(
                    "Skill {skill_id} outputs {}, not {expected_type}",
                    string(skill.get("outputEpisodeType"))
                ));
            }
            let activation = strings(&skill["activation"]["episodeTypes"]);
            if !activation.is_empty()
                && !activation.iter().any(|t| t == expected_type)
                && !activation.iter().any(|t| input_episode_types.contains(t))
            {
                return rejected(format!(
                    "Skill {skill_id} is not activated for this episode context"
                ));
            }
            self.run_checks(&skill["preflightChecks"], "preflight", &thread, None)?;
            for episode_type in strings(&skill["inputEpisodeTypes"]) {
                if !input_episode_types.contains(&episode_type) {
                    return rejected(format!(
                        "Skill {skill_id} requires an input episode of type {episode_type}"
                    ));
                }
            }
        }
        thread["activeSkillIds"] = skill_ids;
        let permitted_tools = active_skills
            .iter()
            .map(|skill| strings(&skill["permittedTools"]))
            .reduce(|allowed, tools| {
                allowed
                    .into_iter()
                    .filter(|tool| tools.contains(tool))
                    .collect()
            });
        if let Some(tools) = &permitted_tools {
            if !tools.iter().any(|tool| tool == "nosh_episode_submit") {
                return rejected("Active skills must jointly permit nosh_episode_submit");
            }
            if truthy(thread.get("currentAgentId")) {
                self.end_session(&mut thread, "rotated", false);
            }
        }
        self.ensure_session(&mut thread, None, permitted_tools)
            .await?;
        thread["state"] = json!("running");
        thread["activeInstructionId"] = json!(instruction_id);
        thread["updatedAt"] = json!(now_utc());
        let history = thread["sessionHistory"].as_array().map_or(0, Vec::len);
        stored = self.save_thread(
            &thread,
            version(&stored),
            &format!("{key}:running:{history}"),
            "runtime.thread_step_started",
        )?;
        let first_sequence = store.current_sequence(project_id)?;
        let all_episodes: Vec<Value> = self
            .episodes(project_id)?
            .into_iter()
            .map(|entry| entry["value"].clone())
            .collect();
        let prompts = strings(&thread["activeSkillIds"])
            .iter()
            .map(|id| {
                Ok(string(
                    self.skill(project_id, id, &thread)?.get("promptFragment"),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let context = render_thread_context(
            &thread,
            &all_episodes,
            &strings(&instruction["inputRefs"]),
            &prompts,
        );
        let started_at = now_utc();
        let started = Instant::now();
        let agent_id = text(&thread["currentAgentId"]).to_owned();
        let prompt = format!(
            "Work only on the bounded objective below. Use selected context as leads, verify before acting, and do not repeat supplied context or raw logs. Finish with the host terminal JSON envelope containing exactly one episode-draft and any required task outcome. Do not call terminal response/review/episode tools. Prose cannot complete the step.\n\n{context}\n\nObjective:\n{}\n\nSubmit an episode-draft of type {expected_type}. Include only a concise summary, verified facts and decisions, artifact/evidence IDs, changed repository-relative files, unresolved questions, and recommended next actions.",
            string(instruction.get("objective"))
        );
        let terminal = TerminalStep {
            instruction_id: Some(instruction_id.to_owned()),
            thread_id: Some(thread_id.clone()),
            expected_version: Some(version(&stored) as f64),
            expected_episode_type: expected_type.to_owned(),
        };
        let mut failure = self
            .agents
            .prompt(&agent_id, &prompt, Some(&terminal))
            .await
            .err();
        let observed = self.thread(project_id, &thread_id)?;
        let mut paused_at_boundary = false;
        if version(&observed) != version(&stored) {
            let state = text(&observed["value"]["state"]);
            if matches!(state, "running" | "paused")
                && observed["value"]["activeInstructionId"] == instruction_id
            {
                thread["childThreadIds"] = union(
                    &thread["childThreadIds"],
                    &observed["value"]["childThreadIds"],
                );
                paused_at_boundary = state == "paused";
                stored = observed;
            } else {
                return rejected(format!(
                    "Thread {thread_id} changed to {} during its step",
                    string(observed["value"].get("state"))
                ));
            }
        }
        let last_sequence = first_sequence.max(store.current_sequence(project_id)?);
        let agent = thread.get("currentAgentId");
        let events: Vec<Value> = store
            .replay(project_id, first_sequence - 1)?
            .into_iter()
            .filter(|event| {
                event["sequence"]
                    .as_i64()
                    .is_some_and(|sequence| sequence <= last_sequence)
                    && strict_equal(event["scope"].get("agentId"), agent)
            })
            .collect();
        let is_draft = |event: &Value| {
            event["type"] == "record.submitted"
                && event["payload"].is_object()
                && event["payload"]["$schema"] == uri("episode-draft")
        };
        let correlation = format!("instruction:{instruction_id}");
        let bound: Vec<Value> = store
            .replay(project_id, 0)?
            .into_iter()
            .filter(|event| {
                event["correlationId"] == correlation.as_str()
                    && strict_equal(event["scope"].get("agentId"), agent)
                    && is_draft(event)
            })
            .collect();
        let submissions: Vec<&Value> = if bound.is_empty() {
            events.iter().filter(|event| is_draft(event)).collect()
        } else {
            bound.iter().collect()
        };
        let submitted = submissions.last().map(|event| event["payload"].clone());
        if submissions.len() > 1 {
            return rejected("Thread step submitted more than one distinct episode-draft");
        }
        let episode_draft = match (&submitted, &failure) {
            (Some(payload), _) => parse("episode-draft", payload)?,
            (None, Some(message)) => fallback_episode_draft(expected_type, message),
            (None, None) => {
                return rejected("Thread step did not submit its required typed episode-draft");
            }
        };
        if episode_draft["episodeType"] != expected_type {
            return rejected(format!(
                "Episode type {} does not match expected {expected_type}",
                string(episode_draft.get("episodeType"))
            ));
        }
        self.validate_draft(
            project_id,
            &episode_draft,
            thread["currentAgentId"].as_str(),
        )?;
        let model_tokens: f64 = events
            .iter()
            .filter(|event| event["type"] == "agent.completed")
            .map(|event| event_budget_tokens(&event["payload"]))
            .sum();
        let tool_calls = events
            .iter()
            .filter(|event| event["type"] == "agent.tool_completed")
            .count() as f64;
        let elapsed = (elapsed_ms(started) / 1_000.0).ceil().max(0.0);
        if !active_skills.is_empty() {
            // `nosh_code` only composes other tools; each of its calls is checked by its own name.
            let used = events
                .iter()
                .filter(|event| event["type"] == "agent.tool_completed")
                .filter_map(|event| event["payload"]["toolName"].as_str())
                .filter(|tool| *tool != "nosh_code");
            let mut forbidden: Vec<&str> = Vec::new();
            for tool in used {
                if active_skills.iter().any(|skill| {
                    !strings(&skill["permittedTools"])
                        .iter()
                        .any(|permitted| permitted == tool)
                }) && !forbidden.contains(&tool)
                {
                    forbidden.push(tool);
                }
            }
            if !forbidden.is_empty() {
                return rejected(format!(
                    "Skill tool policy rejected: {}",
                    forbidden.join(", ")
                ));
            }
        }
        let usage = &thread["usage"];
        let total = [
            num(&usage["toolCalls"]) + tool_calls,
            num(&usage["modelTokens"]) + model_tokens,
            num(&usage["wallClockSeconds"]) + elapsed,
        ];
        let budget = &thread["budget"];
        let overrun = if total[0] > num(&budget["maximumToolCalls"]) {
            Some(format!("Thread {thread_id} exceeded its tool-call budget"))
        } else if total[1] > num(&budget["maximumModelTokens"]) {
            Some(format!(
                "Thread {thread_id} exceeded its model-token budget"
            ))
        } else if total[2] > num(&budget["maximumWallClockSeconds"]) {
            Some(format!("Thread {thread_id} exceeded its wall-clock budget"))
        } else {
            None
        };
        // Budgets are checked after a turn; a step that delivered its typed episode keeps its
        // (already paid) result. The recorded usage still blocks further steps (ensureCanStep).
        if failure.is_none() && submitted.is_none() {
            failure.clone_from(&overrun);
        }
        let context_refs = union(&thread["inputRefs"], &instruction["inputRefs"]);
        let field = |key: &str| episode_draft.get(key).cloned().unwrap_or(Value::Null);
        let input = json!({ "projectId": project_id, "threadId": thread_id, "instructionId": instruction_id, "stepNumber": thread["nextStepNumber"], "episodeType": field("episodeType"), "objective": instruction["objective"], "status": if failure.is_some() { "failed" } else { "completed" }, "summary": field("summary"), "facts": field("facts"), "decisions": field("decisions"), "artifactIds": field("artifactIds"), "evidenceIds": field("evidenceIds"), "changedFiles": field("changedFiles"), "unresolvedQuestions": field("unresolvedQuestions"), "recommendedNextActions": field("recommendedNextActions"), "contextInputRefs": context_refs, "trace": { "firstSequence": first_sequence, "lastSequence": last_sequence }, "usage": { "toolCalls": number(tool_calls), "modelTokens": number(model_tokens), "wallClockSeconds": number(elapsed) }, "startedAt": started_at, "completedAt": now_utc() });
        let mut episode = create_episode(input.as_object().expect("an object literal"))?;
        if failure.is_none() {
            let checked = active_skills.iter().try_for_each(|skill| {
                self.run_checks(
                    &skill["postflightChecks"],
                    "postflight",
                    &thread,
                    Some(&episode),
                )
            });
            if let Err(error) = checked {
                failure = Some(error.to_string());
                let mut failed = episode.as_object().cloned().unwrap_or_default();
                failed.remove("episodeHash");
                failed.insert("status".into(), json!("failed"));
                failed.insert("completedAt".into(), json!(now_utc()));
                episode = create_episode(&failed)?;
            }
        }
        self.save_episode(&episode, &format!("{key}:episode"))?;
        push(&mut thread["episodeIds"], episode["episodeId"].clone());
        thread["nextStepNumber"] = number(num(&thread["nextStepNumber"]) + 1.0);
        thread["inputRefs"] = union(&thread["inputRefs"], &instruction["inputRefs"]);
        thread["activeSkillIds"] = json!([]);
        thread["usage"] = json!({ "toolCalls": number(total[0]), "modelTokens": number(total[1]), "wallClockSeconds": number(total[2]) });
        thread["activeInstructionId"] = Value::Null;
        if !active_skills.is_empty() {
            self.end_session(&mut thread, "completed", false);
        }
        // A failed thread is terminal: release its session, or its live agent outlives the
        // thread and blocks its owner's scheduling.
        if failure.is_some() {
            self.end_session(&mut thread, "failed", false);
        }
        let finished = |thread: &Value, paused: bool| {
            if failure.is_some() {
                "failed"
            } else if paused {
                "paused"
            } else {
                settled_state(thread)
            }
        };
        thread["state"] = json!(finished(&thread, paused_at_boundary));
        thread["updatedAt"] = json!(now_utc());
        let event_type = if failure.is_some() {
            "runtime.thread_step_failed"
        } else if overrun.is_some() {
            "runtime.thread_step_over_budget"
        } else {
            "runtime.thread_step_completed"
        };
        if let Err(error) = self.save_thread(
            &thread,
            version(&stored),
            &format!("{key}:completed"),
            event_type,
        ) {
            let current = self.thread(project_id, &thread_id)?;
            if !matches!(text(&current["value"]["state"]), "running" | "paused")
                || current["value"]["activeInstructionId"] != instruction_id
            {
                return Err(error);
            }
            thread["childThreadIds"] = union(
                &thread["childThreadIds"],
                &current["value"]["childThreadIds"],
            );
            thread["state"] = json!(finished(&thread, current["value"]["state"] == "paused"));
            self.save_thread(
                &thread,
                version(&current),
                &format!("{key}:completed-after-merge"),
                event_type,
            )?;
        }
        if let Some(message) = failure {
            return rejected(message);
        }
        Ok(episode)
    }

    pub(super) async fn await_threads(&self, instruction: &Value) -> Result<Value> {
        let project_id = text(&instruction["projectId"]).to_owned();
        let deadline = instruction["deadline"]
            .as_str()
            .filter(|deadline| !deadline.is_empty())
            .map(str::to_owned);
        let waits: Vec<LocalBoxFuture<'_, Result<Value>>> = strings(&instruction["threadIds"])
            .into_iter()
            .map(|thread_id| {
                let running = self
                    .in_flight
                    .borrow()
                    .get(&format!("{project_id}:{thread_id}"))
                    .cloned();
                match running {
                    Some(running) => running.boxed_local(),
                    None => self
                        .wait_for_episode(project_id.clone(), thread_id, deadline.clone())
                        .boxed_local(),
                }
            })
            .collect();
        let count = waits.len();
        let policy = text(&instruction["policy"]);
        let episodes = match policy {
            "first_success" => vec![
                select_ok(waits)
                    .await
                    .map_err(|_| Error::Rejected("All promises were rejected".into()))?
                    .0,
            ],
            "minimum_count" => {
                minimum_success(
                    waits,
                    instruction["minimumCount"].as_u64().unwrap_or(0) as usize,
                )
                .await?
            }
            _ => {
                let settled = match &deadline {
                    Some(deadline) if policy == "deadline" => {
                        let wait = nosh_wire::js::max(0.0, parse_ms(deadline) - now_ms());
                        let wait = if wait.is_nan() { 0.0 } else { wait };
                        let mut all = std::pin::pin!(join_all(waits));
                        // Waits that settle at once win, as JavaScript microtasks run before any
                        // timer. After that the deadline is polled first: a late wake can find it
                        // and a step that finished after it both ready, where JavaScript fires the
                        // earlier timer first (`tokio::time::timeout` polls its future first).
                        match all.as_mut().now_or_never() {
                            Some(settled) => settled,
                            None => tokio::select! {
                                biased;
                                () = tokio::time::sleep(Duration::from_millis(wait as u64)) => Vec::new(),
                                settled = all => settled,
                            },
                        }
                    }
                    _ => join_all(waits).await,
                };
                let episodes: Vec<Value> = settled.into_iter().filter_map(Result::ok).collect();
                if policy == "all" && episodes.len() != count {
                    return rejected("Not all awaited threads completed successfully");
                }
                episodes
            }
        };
        Ok(
            json!({ "episodeIds": episodes.iter().map(|episode| episode["episodeId"].clone()).collect::<Vec<_>>() }),
        )
    }

    async fn wait_for_episode(
        &self,
        project_id: String,
        thread_id: String,
        deadline: Option<String>,
    ) -> Result<Value> {
        loop {
            let running = self
                .in_flight
                .borrow()
                .get(&format!("{project_id}:{thread_id}"))
                .cloned();
            if let Some(running) = running {
                return running.await;
            }
            let thread = self.thread(&project_id, &thread_id)?["value"].clone();
            if thread["state"] != "running" {
                let Some(id) = thread["episodeIds"].as_array().and_then(|ids| ids.last()) else {
                    return rejected(format!("Thread {thread_id} has no completed episode"));
                };
                let episode = self.episode(&project_id, text(id))?["value"].clone();
                if episode["status"] != "completed" {
                    return rejected(format!(
                        "Thread {thread_id} ended with a {} Episode",
                        string(episode.get("status"))
                    ));
                }
                return Ok(episode);
            }
            if truthy(thread.get("activeInstructionId")) {
                let episodes = self
                    .host
                    .store(&project_id)?
                    .projections(&project_id, "episode")?;
                if let Some(recovered) = episodes
                    .into_iter()
                    .map(|entry| entry["value"].clone())
                    .find(|episode| {
                        strict_equal(
                            episode.get("instructionId"),
                            thread.get("activeInstructionId"),
                        )
                    })
                {
                    if !episode_integrity(&recovered) {
                        return rejected(format!(
                            "Episode {} failed its integrity check",
                            string(recovered.get("episodeId"))
                        ));
                    }
                    if recovered["status"] != "completed" {
                        return rejected(format!(
                            "Thread {thread_id} ended with a {} Episode",
                            string(recovered.get("status"))
                        ));
                    }
                    return Ok(recovered);
                }
            }
            if let Some(deadline) = &deadline
                && now_ms() >= parse_ms(deadline)
            {
                return rejected(format!(
                    "Thread {thread_id} did not finish before the deadline"
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The first `count` successful waits, in completion order.
async fn minimum_success(
    waits: Vec<LocalBoxFuture<'_, Result<Value>>>,
    count: usize,
) -> Result<Vec<Value>> {
    let mut pending: FuturesUnordered<_> = waits.into_iter().collect();
    let mut episodes = Vec::new();
    while let Some(result) = pending.next().await {
        if let Ok(episode) = result {
            episodes.push(episode);
            if episodes.len() == count {
                return Ok(episodes);
            }
        }
    }
    rejected("Awaited threads did not reach minimum_count")
}

/// Where a thread rests after a completed step.
fn settled_state(thread: &Value) -> &'static str {
    if thread["executionMode"] == "foreground_fork" {
        "awaiting_user"
    } else {
        "open"
    }
}

fn push(list: &mut Value, item: Value) {
    if let Some(items) = list.as_array_mut() {
        items.push(item);
    }
}
