//! Port of `apps/noshd/src/director-brief.ts`: deterministic director briefs (spec: directors
//! receive compact structured state "rendered from typed fields without a separate summarization
//! LLM"; they decide semantics, the daemon executes). A brief is the director's whole view of its
//! scope: goal stack, frozen contract, graph, ledgers, evidence, and the event digest since its
//! previous cycle. No raw logs, no transcripts, no tools. Values the model must not invent (cycle
//! ID, timestamps, event range) are allocated here.
//!
//! Divergences: embedded JSON has sorted keys; a field missing from a stored record renders as
//! `null` where TypeScript's `canonicalJson(undefined)` would throw.

use crate::Result;
use crate::task_templates::stringify;
use nosh_agent::terminal::is_js_whitespace;
use nosh_research::project::read_project_contract;
use nosh_research::research_control::{Host, ResearchControl};
use nosh_wire::js::{length, slice_to, strict_equal, string, truthy};
use nosh_wire::{
    IdPrefix, canonical_json, create_id, episode_type_for_role, now_utc, schema_document_path,
    schema_uri,
};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;

const MAX_BRIEF: usize = 14_000;
const MAX_ITEMS: usize = 12;
const MAX_DIRECTOR_SLIPS: usize = 3;

/// Whether a fresh cycle may follow this slip: slips in a row since the scope's last accepted
/// cycle, read from durable events.
pub fn director_retry_allowed(events: &[Value], cycle_schema: &str, retry_type: &str) -> bool {
    let after = events
        .iter()
        .rposition(|event| {
            event["type"] == "record.submitted" && event["payload"]["$schema"] == cycle_schema
        })
        .map_or(0, |last| last + 1);
    events[after..]
        .iter()
        .filter(|event| event["type"] == retry_type)
        .count()
        + 1
        < MAX_DIRECTOR_SLIPS
}

/// The identity the daemon allocates for one director cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefIds {
    pub cycle_id: String,
    pub submitted_at: String,
    pub from_sequence: i64,
    pub to_sequence: i64,
}

impl BriefIds {
    pub fn consumed_event_range(&self) -> Value {
        json!({ "fromSequence": self.from_sequence, "toSequence": self.to_sequence })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    pub prompt: String,
    pub ids: BriefIds,
}

/// What a Mission Director needs to specify a research child for a spawn node; each command is
/// `{commandId, description, argv}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpawnBriefContext {
    pub commands: Vec<Value>,
    pub siblings: Vec<String>,
    pub rejections: Vec<String>,
}

/// What a Research Director needs to specify an Autoresearch for a spawn node.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirectionSpawnContext {
    pub siblings: Vec<String>,
    pub rejections: Vec<String>,
}

fn clip(value: &Value, limit: usize) -> String {
    let raw = match value {
        Value::String(text) => text.clone(),
        other => canonical_json(other),
    };
    let mut text = String::with_capacity(raw.len());
    let mut space = false;
    for character in raw.chars() {
        if is_js_whitespace(character) {
            space = true;
        } else {
            if space && !text.is_empty() {
                text.push(' ');
            }
            space = false;
            text.push(character);
        }
    }
    if length(&text) > limit {
        format!("{}…", slice_to(&text, limit - 1))
    } else {
        text
    }
}

fn north_star(project: &Value) -> String {
    let contract = read_project_contract(Path::new(&string(project.get("repositoryRoot"))));
    match contract {
        Ok(contract)
            if contract
                .get("northStar")
                .is_some_and(|star| !star.is_null()) =>
        {
            format!(
                "{} (decision use: {})",
                string(contract["northStar"].get("question")),
                string(contract["northStar"].get("decisionUse"))
            )
        }
        _ => "unavailable: the active Project contract could not be read".to_owned(),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| string(Some(item)))
        .collect()
}

fn node_line(node: &Value) -> String {
    let dependencies = strings(&node["hardDependencies"]);
    let deps = if dependencies.is_empty() {
        String::new()
    } else {
        format!(" after {}", dependencies.join(","))
    };
    format!(
        "- {} [{}] {} ({}, attempt {}/{}{}{deps})",
        string(node.get("id")),
        string(node.get("state")),
        clip(&node["title"], 120),
        string(node.get("type")),
        string(node.get("attempt")),
        string(node.get("maximumAttempts")),
        if truthy(node.get("required")) {
            ", required"
        } else {
            ""
        }
    )
}

/// Allocates the cycle identity and the event range consumed since this scope's previous cycle
/// record.
fn allocate<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    previous: Option<&Value>,
) -> Result<(BriefIds, Vec<Value>)> {
    let prior = previous
        .and_then(|record| record["consumedEventRange"]["toSequence"].as_i64())
        .unwrap_or(0);
    let events = research.events_after(project_id, prior)?;
    let last = events
        .last()
        .and_then(|event| event["sequence"].as_i64())
        .unwrap_or(prior);
    let ids = BriefIds {
        cycle_id: format!("cycle_{}", &create_id(IdPrefix::Cmd)[4..]),
        submitted_at: now_utc(),
        from_sequence: if events.is_empty() { prior } else { prior + 1 },
        to_sequence: last,
    };
    Ok((ids, events))
}

static NOTABLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"fail|block|error|reject|cancel|accepted|baseline|review|state_changed|^direction\.|^mission\.").expect("regex")
});
static AGENT_STREAM: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^agent\.(text_delta|tool_)").expect("regex"));

/// Typed digest of scoped events: counts by type plus notable failures, transitions, and verdicts.
fn digest(events: &[Value], in_scope: impl Fn(&Value) -> bool) -> Vec<String> {
    let scoped: Vec<&Value> = events.iter().filter(|event| in_scope(event)).collect();
    if scoped.is_empty() {
        return vec!["- no new events in this scope since the previous cycle".to_owned()];
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for event in &scoped {
        let kind = string(event.get("type"));
        match counts.iter_mut().find(|(seen, _)| *seen == kind) {
            Some((_, count)) => *count += 1,
            None => counts.push((kind, 1)),
        }
    }
    let counted: Vec<String> = counts
        .iter()
        .map(|(kind, count)| format!("{kind}×{count}"))
        .collect();
    let mut lines = vec![format!("- counts: {}", counted.join(", "))];
    let notable: Vec<&&Value> = scoped
        .iter()
        .filter(|event| {
            let kind = string(event.get("type"));
            NOTABLE.is_match(&kind) && !AGENT_STREAM.is_match(&kind)
        })
        .collect();
    for event in &notable[notable.len().saturating_sub(MAX_ITEMS)..] {
        let payload = match event.get("payload") {
            None | Some(Value::Null) => json!({}),
            Some(payload) => payload.clone(),
        };
        lines.push(format!(
            "- #{} {} {}",
            string(event.get("sequence")),
            string(event.get("type")),
            clip(&payload, 160)
        ));
    }
    lines
}

fn last_n<T>(items: &[T], count: usize) -> &[T] {
    &items[items.len().saturating_sub(count)..]
}

fn steering_lines<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    mission_id: &str,
) -> Result<Vec<String>> {
    let steers: Vec<Value> = research
        .events_after(project_id, 0)?
        .into_iter()
        .filter(|event| {
            event["type"] == "mission.steering_applied" && event["scope"]["missionId"] == mission_id
        })
        .collect();
    let lines: Vec<String> = last_n(&steers, 5)
        .iter()
        .map(|event| {
            format!(
                "- #{}: {}",
                string(event.get("sequence")),
                clip(&event["payload"]["message"], 600)
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        vec!["- none".to_owned()]
    } else {
        lines
    })
}

fn verdict_lines<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    target_ids: &HashSet<String>,
) -> Result<Vec<String>> {
    let verdicts: Vec<Value> = research
        .records(project_id, Some("review-verdict"))?
        .into_iter()
        .filter(|record| {
            let target = match record["target"].get("targetId") {
                None | Some(Value::Null) => String::new(),
                id => string(id),
            };
            target_ids.contains(&target)
        })
        .collect();
    let mut lines = Vec::new();
    for review in last_n(&verdicts, MAX_ITEMS) {
        let target = &review["target"];
        lines.push(format!(
            "- {} {} on {} v{}: {}",
            string(review.get("reviewId")),
            string(review.get("verdict")),
            string(target.get("targetId")),
            string(target.get("targetVersion")),
            clip(&review["summary"], 140)
        ));
        for defect in review["defects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["severity"] == "blocking" || item["severity"] == "major")
            .take(3)
        {
            lines.push(format!(
                "  - defect {} ({}): {}",
                string(defect.get("defectId")),
                string(defect.get("severity")),
                clip(&defect["title"], 120)
            ));
        }
    }
    Ok(if lines.is_empty() {
        vec!["- none".to_owned()]
    } else {
        lines
    })
}

fn finish(sections: &[String], instructions: &[String]) -> String {
    let mut body = sections.join("\n");
    if length(&body) > MAX_BRIEF {
        body = format!(
            "{}\n… [brief clipped at {MAX_BRIEF} chars]",
            slice_to(&body, MAX_BRIEF - 40)
        );
    }
    format!("{body}\n\n{}", instructions.join("\n"))
}

/// Records are given as complete, schema-valid templates: the daemon fills identity, versions,
/// hashes, ranges, and timestamps; the director edits only judgment fields. Paraphrased field
/// names were the observed failure mode when the contract was described in prose.
fn template_instructions(
    cycle: &Value,
    editable: &str,
    role: &str,
    schema_dir: &Path,
) -> Vec<String> {
    let episode = json!({ "$schema": schema_uri("episode-draft"), "schemaVersion": 1, "episodeType": episode_type_for_role(role), "summary": "EDIT: one-paragraph summary of this cycle's decision.", "facts": [{ "statement": "EDIT: a fact taken from the brief.", "evidenceRefs": [], "confidence": "high" }], "decisions": [{ "statement": "EDIT: the decision.", "rationale": "EDIT: why.", "evidenceRefs": [] }], "artifactIds": [], "evidenceIds": [], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [{ "operation": "THREAD_STEP", "objective": "EDIT: the next bounded action." }] });
    let schema = string(cycle.get("$schema"));
    let parts: Vec<&str> = schema.split('/').collect();
    let name = parts
        .len()
        .checked_sub(2)
        .map_or("undefined", |at| parts[at]);
    vec![
        "Return the host terminal JSON envelope whose records are exactly these two objects. Keep every key, and keep every value that is not an EDIT placeholder unless listed as editable below. Replace each \"EDIT: ...\" string. New reference values use one lowercase prefix, exactly one underscore, then lowercase letters, digits, '.', ':' or '-' (for example uncertainty_baseline-pending).".to_owned(),
        format!("Editable judgment fields in record 1: {editable}. Record 2 is your episode; fill facts, decisions, unresolvedQuestions, and recommendedNextActions from this brief (artifactIds/evidenceIds only with IDs listed above)."),
        format!("Record 1 ({}):", schema_document_path(schema_dir, name).display()),
        stringify(cycle),
        format!("Record 2 ({}):", schema_document_path(schema_dir, "episode-draft").display()),
        stringify(&episode),
    ]
}

/// `JSON.stringify` drops undefined members: only the keys `record` has.
fn pick(record: &Value, keys: &[&str]) -> Value {
    let mut picked = Map::new();
    for key in keys {
        if let Some(value) = record.get(*key) {
            picked.insert((*key).to_owned(), value.clone());
        }
    }
    Value::Object(picked)
}

/// Every Autoresearch this Direction has consumed: completion, measured results, and Evidence
/// statements, as `(evidenceIds, digest)`. Shared by the Director brief and Direction reviewer
/// nodes.
pub fn direction_autoresearch_outcomes<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    direction_id: &str,
) -> Result<(Vec<String>, String)> {
    let consumed: HashSet<String> = research
        .events_after(project_id, 0)?
        .iter()
        .filter(|entry| {
            entry["type"] == "direction.hypothesis_resolved"
                && entry["scope"]["directionId"] == direction_id
        })
        .map(|entry| string(entry["payload"].get("autoresearchId")))
        .collect();
    let consumed_by = |record: &Value| consumed.contains(&string(record.get("autoresearchId")));
    let completions: Vec<Value> = research
        .records(project_id, Some("autoresearch-completion-packet"))?
        .into_iter()
        .filter(|record| consumed_by(record))
        .collect();
    let mut evidence_ids: Vec<String> = Vec::new();
    for completion in &completions {
        for id in strings(&completion["evidenceIds"]) {
            if !evidence_ids.contains(&id) {
                evidence_ids.push(id);
            }
        }
    }
    let results: Vec<Value> = research
        .records(project_id, Some("experiment-result"))?
        .into_iter()
        .filter(|record| consumed_by(record))
        .collect();
    let evidence: Vec<Value> = research
        .records(project_id, Some("evidence"))?
        .into_iter()
        .filter(|record| evidence_ids.contains(&string(record.get("evidenceId"))))
        .collect();
    let digest = if completions.is_empty() {
        String::new()
    } else {
        let completions: Vec<Value> = completions
            .iter()
            .map(|completion| {
                pick(
                    completion,
                    &[
                        "autoresearchId",
                        "terminalReason",
                        "decisionAnswer",
                        "bestExperimentId",
                        "failedHypothesisIds",
                    ],
                )
            })
            .collect();
        let results: Vec<Value> = results
            .iter()
            .map(|result| {
                pick(
                    result,
                    &[
                        "experimentId",
                        "evaluatedCommit",
                        "metrics",
                        "guardrails",
                        "comparison",
                        "reviewVerdict",
                        "promotionRecommendation",
                        "promotionDecision",
                    ],
                )
            })
            .collect();
        let evidence: Vec<Value> = evidence
            .iter()
            .map(|record| pick(record, &["evidenceId", "polarity", "statement", "quality"]))
            .collect();
        slice_to(
            &stringify(
                &json!({ "completions": completions, "results": results, "evidence": evidence }),
            ),
            12_000,
        )
    };
    Ok((evidence_ids, digest))
}

fn listed(lines: &[String], limit: usize) -> Vec<String> {
    if lines.is_empty() {
        vec!["  - none".to_owned()]
    } else {
        lines
            .iter()
            .map(|line| format!("  - {}", clip(&json!(line), limit)))
            .collect()
    }
}

/// The Research Director's brief for `node` of `direction`.
pub fn direction_brief<H: Host>(
    research: &ResearchControl<H>,
    project: &Value,
    direction: &Value,
    node: &Value,
    agent_id: &str,
    spawn: Option<&DirectionSpawnContext>,
    schema_dir: &Path,
) -> Result<Brief> {
    let value = &direction["value"];
    let project_id = string(project.get("projectId"));
    let direction_id = string(direction.get("entityId"));
    let previous = research
        .records(&project_id, Some("research-director-cycle"))?
        .into_iter()
        .rfind(|record| record["directionId"] == direction_id.as_str());
    let (ids, events) = allocate(research, &project_id, previous.as_ref())?;
    let nodes: Vec<Value> = value["nodes"].as_array().cloned().unwrap_or_default();
    let mut node_ids: HashSet<String> = nodes.iter().map(|item| string(item.get("id"))).collect();
    node_ids.insert(direction_id.clone());
    let evidence: Vec<Value> = research
        .records(&project_id, Some("evidence"))?
        .into_iter()
        .filter(|record| {
            strict_equal(
                record.get("evaluationContractHash"),
                value.get("evaluationContractHash"),
            )
        })
        .collect();
    let failed: Vec<String> = nodes
        .iter()
        .filter(|item| item["state"] == "failed" || item["state"] == "blocked")
        .map(node_line)
        .collect();
    let falsifiability = &value["falsifiability"];
    let baseline = &value["acceptedBaseline"];
    let baseline = if truthy(Some(baseline)) {
        format!(
            "accepted at commit {} (review {})",
            string(baseline.get("commit")),
            string(baseline.get("reviewId"))
        )
    } else {
        format!(
            "not yet accepted (planned experiment {})",
            string(value.get("plannedBaselineExperimentId"))
        )
    };
    let rules: Vec<String> = value["stoppingRules"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|rule| {
            format!(
                "{}: {}",
                string(rule.get("ruleId")),
                clip(&rule["statement"], 140)
            )
        })
        .collect();
    let rules = if rules.is_empty() {
        "none".to_owned()
    } else {
        rules.join("; ")
    };
    let used = research.direction_budget_use(&project_id, &direction_id)?;
    let ceiling = match value["budget"].get("maximumModelTokens") {
        None | Some(Value::Null) => "unlimited".to_owned(),
        maximum => string(maximum),
    };
    let outcomes = direction_autoresearch_outcomes(research, &project_id, &direction_id)?.1;
    let mut sections = vec![
        format!(
            "DIRECTION BRIEF (daemon-rendered from durable state at {}; authoritative for this cycle)",
            ids.submitted_at
        ),
        String::new(),
        "Goal stack:".to_owned(),
        format!(
            "- Project north star: {}",
            clip(&json!(north_star(project)), 400)
        ),
        format!(
            "- Direction {direction_id} question {}: {}",
            string(value.get("questionId")),
            clip(&value["question"], 400)
        ),
        format!("- Decision use: {}", clip(&value["decisionUse"], 300)),
        format!(
            "- Falsifiability: supports if {}; refutes if {}; inconclusive if {}",
            clip(&falsifiability["supportingOutcome"], 160),
            clip(&falsifiability["refutingOutcome"], 160),
            clip(&falsifiability["inconclusiveOutcome"], 160)
        ),
        String::new(),
        format!(
            "State: {}, version {}, graph version {}",
            string(direction.get("state")),
            string(direction.get("version")),
            string(value.get("graphVersion"))
        ),
        format!(
            "Frozen evaluation contract {}: {}",
            string(value.get("evaluationContractHash")),
            clip(&value["evaluationContract"], 1_500)
        ),
        format!("Baseline: {baseline}"),
        format!("Stopping rules: {rules}"),
        format!(
            "Budget: this Direction has used {} of {ceiling} model tokens. Each Autoresearch has its own experiment and token budget; one exhausting its own budget does not exhaust this Direction.",
            string(used.get("modelTokens"))
        ),
        String::new(),
        "Graph nodes:".to_owned(),
    ];
    sections.extend(nodes.iter().map(node_line));
    sections.push(format!(
        "Selected by the deterministic scheduler to run next: {} ({})",
        string(node.get("id")),
        clip(&node["title"], 160)
    ));
    sections.extend([String::new(), "Failure ledger:".to_owned()]);
    if failed.is_empty() {
        sections.push("- no failed or blocked nodes".to_owned());
    } else {
        sections.extend(failed);
    }
    sections.extend([
        String::new(),
        "Review verdicts on this Direction:".to_owned(),
    ]);
    sections.extend(verdict_lines(research, &project_id, &node_ids)?);
    sections.extend([
        String::new(),
        "Resolved Autoresearch outcomes (reviewer nodes receive these as required Evidence):"
            .to_owned(),
        if outcomes.is_empty() {
            "- none yet".to_owned()
        } else {
            outcomes
        },
        String::new(),
        "Evidence under this contract:".to_owned(),
    ]);
    if evidence.is_empty() {
        sections.push("- none yet".to_owned());
    } else {
        sections.extend(last_n(&evidence, MAX_ITEMS).iter().map(|record| {
            format!(
                "- {} {}: {}",
                string(record.get("evidenceId")),
                string(record.get("polarity")),
                clip(&record["statement"], 160)
            )
        }));
    }
    sections.extend([
        String::new(),
        format!(
            "Event digest for #{}..#{}:",
            ids.from_sequence, ids.to_sequence
        ),
    ]);
    sections.extend(digest(&events, |event| {
        event["scope"]["directionId"] == direction_id.as_str()
    }));
    if let Some(spawn) = spawn {
        sections.extend([
            String::new(),
            format!("Autoresearch to specify for node {}: the daemon starts it from your spawn object under this Direction's frozen evaluation contract, then runs it without you.", string(node.get("id"))),
            "- It must be meaningfully separate from this Direction's earlier Autoresearch: a different question, familyTags, or scope (an identical fingerprint is refused).".to_owned(),
            "- Earlier Autoresearch on this Direction:".to_owned(),
        ]);
        sections.extend(listed(&spawn.siblings, 400));
        sections.push("- Earlier specs the daemon rejected (fix what each says):".to_owned());
        sections.extend(listed(&spawn.rejections, 600));
    }
    let mut cycle = json!({ "$schema": schema_uri("research-director-cycle"), "schemaVersion": 1, "cycleId": ids.cycle_id, "directionId": direction_id, "directorAgentId": agent_id, "observedGraphVersion": value["graphVersion"], "evaluationContractHash": value["evaluationContractHash"], "consumedEventRange": ids.consumed_event_range(), "questionCheck": { "questionId": value["questionId"], "currentDisposition": "unresolved", "remainingUncertaintyIds": ["uncertainty_question-open"], "currentWorkContributes": true }, "acceptedFrontierExperimentIds": [], "failedHypothesisIdsAdded": [], "decisions": [{ "action": "run_node", "targetId": node["id"], "reason": "EDIT: why running this node now advances the question." }], "createdTaskIds": [], "invokedAutoresearchIds": [], "graphChangeProposalIds": [], "directionSuggestionIds": [], "userInputRequestId": null, "nextWake": { "type": "event", "deadline": null }, "submittedAt": ids.submitted_at });
    if spawn.is_some() {
        cycle["spawn"] = json!({ "nodeId": node["id"], "kind": "autoresearch", "question": "EDIT: the question this Autoresearch answers, distinct from earlier ones.", "familyTags": ["EDIT: short tag naming the hypothesis family"], "scope": ["EDIT: repository paths the experiments may change: the method's source and configuration, never the evaluation program or its result file"] });
    }
    let editable = format!(
        "questionCheck.currentDisposition (unresolved | supported | refuted | inconclusive), questionCheck.remainingUncertaintyIds, questionCheck.currentWorkContributes (false ends exploration of the selected node: if it already has measured Autoresearch results the hypothesis is resolved on them and the Direction proceeds to synthesis and closure Review; without any it halts for user attention), and decisions[].reason (decisions may also add stop_direction or request_review entries targeting IDs above){}",
        if spawn.is_some() {
            "; and spawn.question, spawn.familyTags, spawn.scope"
        } else {
            ""
        }
    );
    let mut instructions = vec!["Your cycle: decide whether running the selected node now advances the Direction question within the frozen contract. You do not execute work, lease nodes, or read files; the daemon runs the node, its worker, Job, postflight, and independent Review after you. Base every statement only on this brief.".to_owned()];
    instructions.extend(template_instructions(
        &cycle,
        &editable,
        "research_director",
        schema_dir,
    ));
    Ok(Brief {
        prompt: finish(&sections, &instructions),
        ids,
    })
}

/// The Mission Director's brief for `node` of `mission`.
pub fn mission_brief<H: Host>(
    research: &ResearchControl<H>,
    project: &Value,
    mission: &Value,
    node: &Value,
    agent_id: &str,
    spawn: Option<&SpawnBriefContext>,
    schema_dir: &Path,
) -> Result<Brief> {
    let value = &mission["value"];
    let project_id = string(project.get("projectId"));
    let mission_id = string(mission.get("entityId"));
    let previous = research
        .records(&project_id, Some("mission-director-cycle"))?
        .into_iter()
        .rfind(|record| record["missionId"] == mission_id.as_str());
    let (ids, events) = allocate(research, &project_id, previous.as_ref())?;
    let nodes: Vec<Value> = value["nodes"].as_array().cloned().unwrap_or_default();
    let mut node_ids: HashSet<String> = nodes.iter().map(|item| string(item.get("id"))).collect();
    node_ids.insert(mission_id.clone());
    let mut unsatisfied: Vec<String> = Vec::new();
    for item in nodes
        .iter()
        .filter(|item| truthy(item.get("required")) && item["state"] != "accepted")
    {
        for id in strings(&item["criterionIds"]) {
            if !unsatisfied.contains(&id) {
                unsatisfied.push(id);
            }
        }
    }
    let ready: Vec<String> = nodes
        .iter()
        .filter(|item| item["state"] == "ready")
        .map(|item| string(item.get("id")))
        .collect();
    let failed: Vec<String> = nodes
        .iter()
        .filter(|item| item["state"] == "failed" || item["state"] == "blocked")
        .map(node_line)
        .collect();
    let clipped = |items: &Value, limit: usize| -> Vec<String> {
        items
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| clip(item, limit))
            .collect()
    };
    let or_none = |items: Vec<String>, separator: &str| {
        if items.is_empty() {
            "none".to_owned()
        } else {
            items.join(separator)
        }
    };
    let mut sections = vec![
        format!(
            "MISSION BRIEF (daemon-rendered from durable state at {}; authoritative for this cycle)",
            ids.submitted_at
        ),
        String::new(),
        "Goal stack:".to_owned(),
        format!(
            "- Project north star: {}",
            clip(&json!(north_star(project)), 400)
        ),
        format!(
            "- Mission {mission_id} \"{}\": {}",
            clip(&value["title"], 160),
            clip(&value["objective"], 500)
        ),
        format!(
            "- Deliverables: {}",
            clipped(&value["deliverables"], 140).join("; ")
        ),
        format!(
            "- Success criteria: {}",
            clipped(&value["successCriteria"], 140).join("; ")
        ),
        format!(
            "- Non-objectives: {}",
            or_none(clipped(&value["nonObjectives"], 140), "; ")
        ),
        String::new(),
        format!(
            "State: {}, version {}, graph version {}",
            string(mission.get("state")),
            string(mission.get("version")),
            string(value.get("graphVersion"))
        ),
        format!(
            "Unsatisfied criterion IDs: {}",
            or_none(unsatisfied.clone(), ", ")
        ),
        format!("Ready node IDs: {}", or_none(ready.clone(), ", ")),
        String::new(),
        "Graph nodes:".to_owned(),
    ];
    sections.extend(nodes.iter().map(node_line));
    sections.push(format!(
        "Selected by the deterministic scheduler to run next: {} ({})",
        string(node.get("id")),
        clip(&node["title"], 160)
    ));
    sections.extend([String::new(), "Failure ledger:".to_owned()]);
    if failed.is_empty() {
        sections.push("- no failed or blocked nodes".to_owned());
    } else {
        sections.extend(failed);
    }
    sections.extend([
        String::new(),
        "User steering (most recent last; binding within the approved Mission contract):"
            .to_owned(),
    ]);
    sections.extend(steering_lines(research, &project_id, &mission_id)?);
    sections.extend([String::new(), "Review verdicts on this Mission:".to_owned()]);
    sections.extend(verdict_lines(research, &project_id, &node_ids)?);
    sections.extend([
        String::new(),
        format!(
            "Event digest for #{}..#{}:",
            ids.from_sequence, ids.to_sequence
        ),
    ]);
    sections.extend(digest(&events, |event| {
        event["scope"]["missionId"] == mission_id.as_str()
    }));
    let node_type = string(node.get("type"));
    if let Some(spawn) = spawn {
        sections.extend([
            String::new(),
            format!("Research child to specify: node {} is a {node_type} node; the daemon spawns it from your spawn object, then runs it without you.", string(node.get("id"))),
            "- A research_direction answers one falsifiable question: the daemon measures a baseline, Autoresearch searches for improvements, and a reviewed closure answers it. An autoresearch searches for changes that improve the primary metric without breaking the guardrails.".to_owned(),
            "- Declared evaluation commands (spawn.evaluation.commandId must be one; the daemon runs it once and the metrics it writes become the frozen baseline, so never state baseline numbers yourself):".to_owned(),
        ]);
        if spawn.commands.is_empty() {
            sections.push("  - none declared; this node cannot be spawned".to_owned());
        } else {
            sections.extend(spawn.commands.iter().map(|command| {
                format!(
                    "  - {}: {} ({})",
                    string(command.get("commandId")),
                    clip(&json!(strings(&command["argv"]).join(" ")), 200),
                    clip(&command["description"], 160)
                )
            }));
        }
        sections.extend([
            "- Choose spawn.evaluation.primaryMetric.name and guardrail names from the keys that command writes to spawn.evaluation.resultPath (a relative JSON file).".to_owned(),
            "- A guardrail threshold is the allowed change from the measured baseline, not an absolute value: {name: \"f1\", direction: \"maximize\", threshold: -0.005} lets f1 drop at most 0.005 below the baseline; for minimize use zero or a positive allowance.".to_owned(),
            "- Research children already specified for this Mission (this one must answer a different question):".to_owned(),
        ]);
        sections.extend(listed(&spawn.siblings, 300));
        sections.push(
            "- Earlier specs for this node the daemon rejected (fix what each says):".to_owned(),
        );
        sections.extend(listed(&spawn.rejections, 600));
    }
    let mut cycle = json!({ "$schema": schema_uri("mission-director-cycle"), "schemaVersion": 1, "cycleId": ids.cycle_id, "missionId": mission_id, "directorAgentId": agent_id, "observedObjectiveVersion": 1, "observedGraphVersion": value["graphVersion"], "consumedEventRange": ids.consumed_event_range(), "northStarCheck": { "missionObjective": value["objective"], "unsatisfiedCriterionIds": unsatisfied, "criticalPathNodeIds": [node["id"]], "currentWorkContributes": true }, "portfolioCheck": { "readyNodeIds": ready, "starvedNodeIds": [], "overfocusedNodeId": null, "fairnessAction": "fairness_none" }, "decisions": [{ "action": "run_node", "targetId": node["id"], "reason": "EDIT: why running this node now advances the objective." }], "createdTaskIds": [], "invokedDirectionIds": [], "invokedAutoresearchIds": [], "graphChangeProposalIds": [], "userInputRequestId": null, "budgetWarnings": [], "blockerIds": [], "nextWake": { "type": "event", "deadline": null }, "submittedAt": ids.submitted_at });
    if let Some(spawn) = spawn {
        let command_id = spawn
            .commands
            .first()
            .and_then(|command| command.get("commandId"))
            .filter(|id| !id.is_null())
            .cloned()
            .unwrap_or_else(|| json!("command_none"));
        let decision_use = if node_type == "research_direction" {
            json!("EDIT: what the answer will decide for the Mission.")
        } else {
            Value::Null
        };
        cycle["spawn"] = json!({ "nodeId": node["id"], "kind": node["type"], "question": "EDIT: one concrete research question this child answers, in service of the Mission objective.", "decisionUse": decision_use, "evaluation": { "commandId": command_id, "resultPath": "EDIT: relative path of the JSON metrics file that command writes", "primaryMetric": { "name": "EDIT: metric key in that file", "objective": "minimize", "minimumEffect": 0.01 }, "guardrails": [], "target": null, "maximumNoProgressRounds": 6 }, "familyTags": ["EDIT: short tag naming the hypothesis family"], "scope": ["EDIT: repository paths the research may change: the method's source and configuration, never the evaluation program or its result file"] });
    }
    let editable = format!(
        "northStarCheck.criticalPathNodeIds, northStarCheck.currentWorkContributes (false halts the Mission for user attention), portfolioCheck.starvedNodeIds/overfocusedNodeId, budgetWarnings, and decisions[].reason{}",
        if spawn.is_some() {
            "; and every field of spawn except nodeId and kind (objective is maximize or minimize; primaryMetric.minimumEffect is the smallest improvement that counts and must be greater than 0; guardrails are {name, direction, threshold}, threshold being the allowed change from the measured baseline; target is a number or null; maximumNoProgressRounds is 1-20: consecutive non-improving experiments before the climb stops (6-10 suits a cheap metric))"
        } else {
            ""
        }
    );
    let mut instructions = vec!["Your cycle: decide whether running the selected node now advances the Mission objective and its unsatisfied criteria. You do not execute work, lease nodes, read files, or claim completion; the daemon runs the node, its worker, postflight, and independent Review after you. Base every statement only on this brief.".to_owned()];
    instructions.extend(template_instructions(
        &cycle,
        &editable,
        "mission_director",
        schema_dir,
    ));
    Ok(Brief {
        prompt: finish(&sections, &instructions),
        ids,
    })
}
