//! What Episodes may cite, and the intervention record each instruction leaves.

use super::*;
use nosh_sys::paths::{relative_inside, resolve};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

impl<H: RuntimeHost + 'static> OrchestrationRuntime<H> {
    /// Every Episode reference must exist, except the IDs a host-issued Review Request gives this
    /// reviewer: candidates such as frozen experiment Evidence are published only after their
    /// review. Changed files must stay inside the repository.
    pub fn validate_draft(
        &self,
        project_id: &str,
        value: &Value,
        agent_id: Option<&str>,
    ) -> Result<()> {
        let pending = match agent_id.filter(|id| !id.is_empty()) {
            Some(agent_id) => self.review_inputs(project_id, agent_id)?,
            None => HashSet::new(),
        };
        let cited = |key: &str| {
            value[key]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|item| strings(&item["evidenceRefs"]))
                .collect::<Vec<_>>()
        };
        let ids = strings(&value["artifactIds"])
            .into_iter()
            .chain(strings(&value["evidenceIds"]))
            .chain(cited("facts"))
            .chain(cited("decisions"));
        for id in ids {
            if !pending.contains(&id) && self.reference_value(project_id, &id)?.is_none() {
                return rejected(format!("Episode references unknown record {id}"));
            }
        }
        let io = |error: std::io::Error| Error::Rejected(error.to_string());
        let root = resolve(&self.host.repository_root(project_id)?).map_err(io)?;
        for file in strings(&value["changedFiles"]) {
            let inside = relative_inside(&root, &root.join(&file)).map_err(io)?;
            if node_is_absolute(&file) || inside.is_none_or(|path| path.as_os_str().is_empty()) {
                return rejected(format!("Changed file escapes the repository: {file}"));
            }
        }
        Ok(())
    }

    fn review_inputs(&self, project_id: &str, agent_id: &str) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        for event in self.host.store(project_id)?.replay(project_id, 0)? {
            let request = &event["payload"];
            if event["type"] != "record.submitted"
                || !request.is_object()
                || request["$schema"] != uri("review-request")
                || request["reviewerAgentId"] != agent_id
            {
                continue;
            }
            ids.extend(strings(&request["requiredEvidenceIds"]));
            ids.extend(strings(&request["requiredArtifactIds"]));
        }
        Ok(ids)
    }

    pub(super) fn reference_value(
        &self,
        project_id: &str,
        reference: &str,
    ) -> Result<Option<Value>> {
        let store = self.host.store(project_id)?;
        for entity_type in [
            "episode",
            "execution_thread",
            "skill",
            "orchestration_program",
            "mission",
            "direction",
            "autoresearch",
        ] {
            if let Some(projection) = store.projection(project_id, entity_type, reference)? {
                return Ok(Some(projection["value"].clone()));
            }
        }
        let events = store.replay(project_id, 0)?;
        // Jobs (evaluations, nosh_run validators) are host-issued references recorded in the event log.
        if reference.starts_with("job_")
            && let Some(job) = events
                .iter()
                .find(|event| event["scope"]["jobId"] == reference)
        {
            return Ok(Some(job["payload"].clone()));
        }
        let rule = match nosh_wire::js::slice_to(reference, 3).as_str() {
            "art" => Some(("artifact", "artifactId")),
            "evd" => Some(("evidence", "evidenceId")),
            "clm" => Some(("claim", "claimId")),
            "rev" => Some(("review-verdict", "reviewId")),
            _ => None,
        };
        let draft_schema = uri("episode-draft");
        Ok(events
            .into_iter()
            .filter(|event| {
                event["type"] == "record.submitted"
                    && event["payload"].is_object()
                    && event["payload"]["$schema"] != draft_schema.as_str()
            })
            .map(|event| event["payload"].clone())
            .find(|payload| match rule {
                Some((schema, key)) => {
                    payload["$schema"] == uri(schema).as_str() && payload[key] == reference
                }
                None => contains_id(payload, reference),
            }))
    }

    pub(super) fn thread_state(&self, instruction: &Value) -> Result<Value> {
        let id = instruction
            .get("threadId")
            .or_else(|| instruction.get("targetThreadId"));
        let Some(id) = id.and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            return Ok(Value::Null);
        };
        let project_id = text(&instruction["projectId"]);
        Ok(self
            .host
            .store(project_id)?
            .projection(project_id, "execution_thread", id)?
            .map_or(Value::Null, |projection| projection["value"].clone()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_intervention(
        &self,
        instruction: &Value,
        assignment: Assignment,
        reasons: &[String],
        eligible: bool,
        executed: bool,
        before: &Value,
        after: &Value,
        result: &Value,
        latency: f64,
    ) -> Result<()> {
        let project_id = text(&instruction["projectId"]);
        let instruction_id = text(&instruction["instructionId"]);
        let store = self.host.store(project_id)?;
        let intervention_id = deterministic_id("ivn", instruction_id);
        let output_refs = collect_refs(result);
        let input_refs: Vec<String> = collect_refs(instruction)
            .into_iter()
            .filter(|reference| reference != instruction_id && reference != project_id)
            .collect();
        let episodes: Vec<String> = output_refs
            .iter()
            .filter(|reference| reference.starts_with("epi_"))
            .cloned()
            .collect();
        let mut costs = Vec::new();
        for reference in &episodes {
            if let Some(episode) = store.projection(project_id, "episode", reference)? {
                costs.push(episode["value"]["usage"].clone());
            }
        }
        let total = |key: &str| costs.iter().map(|usage| num(&usage[key])).sum::<f64>();
        let wall = total("wallClockSeconds");
        let wall = if wall == 0.0 || wall.is_nan() {
            (latency / 1_000.0).ceil()
        } else {
            wall
        };
        let value = parse(
            "runtime-intervention",
            &json!({ "$schema": uri("runtime-intervention"), "schemaVersion": 1, "interventionId": intervention_id, "projectId": project_id, "instructionId": instruction_id, "operation": instruction["operation"], "proposed": instruction, "eligibility": { "eligible": eligible, "reasons": reasons }, "assignment": assignment.as_str(), "executed": executed, "inputRefs": input_refs, "outputRefs": output_refs, "stateBefore": before, "stateAfter": after, "cost": { "toolCalls": number(total("toolCalls")), "modelTokens": number(total("modelTokens")), "wallClockSeconds": number(wall) }, "latencyMilliseconds": number(latency), "downstreamEpisodeIds": episodes, "recordedAt": now_utc() }),
        )?;
        let prior = store.projection(project_id, "runtime_intervention", &intervention_id)?;
        let draft = draft(
            project_id,
            "runtime.intervention_recorded",
            instruction_id,
            &value,
            None,
        );
        let input = ProjectionInput {
            entity_type: "runtime_intervention".into(),
            entity_id: intervention_id,
            state: if executed { "executed" } else { "not_executed" }.into(),
            value,
            graph: None,
            records: vec![],
            intent: None,
        };
        self.mutate(
            project_id,
            &format!("{}:intervention", text(&instruction["idempotencyKey"])),
            prior.as_ref().map_or(0, version),
            &draft,
            &input,
        )?;
        Ok(())
    }
}

/// Node's win32 `path.isAbsolute`: a leading separator, or a drive letter, colon, and separator.
fn node_is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let separator = |byte: u8| byte == b'/' || byte == b'\\';
    bytes.first().is_some_and(|&first| separator(first))
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && separator(bytes[2]))
}

fn contains_id(value: &Value, id: &str) -> bool {
    match value {
        Value::String(text) => text == id,
        Value::Array(items) => items.iter().any(|item| contains_id(item, id)),
        Value::Object(map) => map.values().any(|item| contains_id(item, id)),
        _ => false,
    }
}

/// Every distinct string in `value` shaped like a NOSH reference.
fn collect_refs(value: &Value) -> Vec<String> {
    static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[a-z][a-z0-9]*_[a-z0-9][a-z0-9.:-]*$").expect("a valid pattern")
    });
    fn visit(item: &Value, refs: &mut Vec<String>) {
        match item {
            Value::String(text) if REFERENCE.is_match(text) => {
                if !refs.contains(text) {
                    refs.push(text.clone());
                }
            }
            Value::Array(items) => items.iter().for_each(|item| visit(item, refs)),
            Value::Object(map) => map.values().for_each(|item| visit(item, refs)),
            _ => {}
        }
    }
    let mut refs = Vec::new();
    visit(value, &mut refs);
    refs
}
