//! Port of `apps/noshd/src/research-spawner.ts`: research children spawned without a human, a
//! Mission's Research Directions and its own Autoresearch, and each Direction's Autoresearch. A
//! director supplies the judgment (question, metric, scope); the daemon checks it against the
//! Project contract, measures the baseline by running the declared command once, freezes the
//! evaluation contract from that measurement, and creates and starts the child under a durable
//! intent, so a restart between steps resumes instead of duplicating work.
//!
//! A spawn (`MissionSpawn`/`DirectionSpawn` in TypeScript) is the director's schema-validated
//! JSON, read as a [`Value`].
//!
//! Divergence: measured metrics keep sorted keys, so the probe's key listings are sorted.

use crate::Result;
use crate::task_worktree::create_task_worktree;
use nosh_research::research_control::{
    AutoresearchInput, DirectionInput, Host, ResearchControl, SpawnedAutoresearch,
    SpawnedDirection, declared_task_commands,
};
use nosh_sys::jobs::{JobRecord, JobSpec, Runner, load_native_evaluation};
use nosh_wire::js::{number_string, string};
use nosh_wire::{IdPrefix, create_id, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Starts and reads supervised Jobs.
pub trait JobRunner {
    fn start(&self, spec: &JobSpec) -> Result<JobRecord>;
    fn get(&self, job_id: &str) -> Result<JobRecord>;
}

/// How a frozen evaluation runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationExecution {
    pub runner: Runner,
    #[serde(default)]
    pub distribution: Option<String>,
    pub command: Vec<String>,
    pub result_path: String,
    pub timeout_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses_gpu: Option<bool>,
}

/// Smallest model-token allocations worth spawning; below them a child cannot finish its own
/// reviewed closure.
const MIN_DIRECTION_TOKENS: f64 = 300_000.0;
pub const MIN_AUTORESEARCH_TOKENS: f64 = 150_000.0;
/// A spawned Autoresearch hill-climbs one experiment per round; its evaluations are cheap, so it
/// gets many (tokens still bound it).
const SPAWNED_EXPERIMENTS: f64 = 30.0;
/// How long a spawned child may sit blocked or paused, with no agent working on it, before its
/// parent stops it.
pub const CHILD_BLOCKED_MS: f64 = 10.0 * 60_000.0;
/// Share of the Mission ceiling kept for the Mission's own nodes and reviews.
const MISSION_RESERVE: f64 = 0.3;

/// One evaluation command's run as a supervised Job, with its JSON result.
#[derive(Debug, Clone)]
pub struct EvaluationRun {
    pub job_id: String,
    pub job: JobRecord,
    pub result_path: PathBuf,
    pub metrics: Value,
    pub failure: Option<String>,
}

/// Where the evaluation runs and what it is bound to (`runEvaluationJob`'s input).
pub struct EvaluationJob<'a> {
    pub project_id: &'a str,
    pub mission_id: Option<&'a str>,
    pub direction_id: Option<&'a str>,
    pub autoresearch_id: Option<&'a str>,
    pub experiment_id: &'a str,
    pub commit_sha: &'a str,
    pub worktree: &'a Path,
    pub execution: &'a EvaluationExecution,
    pub evaluation_contract_hash: &'a str,
}

fn state_text(job: &JobRecord) -> String {
    string(serde_json::to_value(job.state).ok().as_ref())
}

/// Runs one evaluation command as a supervised Job in a worktree and reads its JSON result.
pub async fn run_evaluation_job(
    job_runner: &dyn JobRunner,
    input: EvaluationJob<'_>,
) -> Result<EvaluationRun> {
    let result_path = nosh_sys::paths::resolve(&input.worktree.join(&input.execution.result_path))?;
    if nosh_sys::paths::relative_inside(input.worktree, &result_path)?.is_none() {
        return crate::rejected("Evaluation result path escapes the worktree");
    }
    let job_id = create_id(IdPrefix::Job);
    let timeout_seconds = input.execution.timeout_seconds;
    job_runner.start(&JobSpec {
        job_id: job_id.clone(),
        project_id: input.project_id.into(),
        mission_id: input.mission_id.map(str::to_owned),
        direction_id: input.direction_id.map(str::to_owned),
        autoresearch_id: input.autoresearch_id.map(str::to_owned),
        run_id: create_id(IdPrefix::Run),
        experiment_id: Some(input.experiment_id.into()),
        commit_sha: input.commit_sha.into(),
        working_directory: input.worktree.to_string_lossy().into_owned(),
        runner: input.execution.runner,
        distribution: input.execution.distribution.clone(),
        command: input.execution.command.clone(),
        checkpoint_command: None,
        environment_lock_hash: crate::daemon::environment_lock_hash(),
        evaluation_contract_hash: input.evaluation_contract_hash.into(),
        timeout_seconds,
        uses_gpu: input.execution.uses_gpu == Some(true),
    })?;
    let mut job = job_runner.get(&job_id)?;
    let deadline = Instant::now() + Duration::from_secs_f64((timeout_seconds + 60.0).max(0.0));
    while !job.state.is_terminal() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        job = job_runner.get(&job_id)?;
    }
    if !job.state.is_terminal() {
        return crate::rejected(format!(
            "Evaluation Job {job_id} did not finish within its timeout"
        ));
    }
    let completed = state_text(&job) == "completed";
    let mut metrics =
        json!({ "error": job.failure_reason.clone().unwrap_or_else(|| state_text(&job)) });
    let mut failure = Some(if completed {
        "evaluation result file is absent".to_owned()
    } else {
        format!("job {}", state_text(&job))
    });
    if completed && result_path.exists() {
        match load_native_evaluation(&result_path) {
            Ok(loaded) if loaded.is_object() => {
                metrics = loaded;
                failure = None;
            }
            Ok(_) => failure = Some("evaluation result is not a JSON object".into()),
            Err(error) => {
                let message = error.to_string();
                failure = Some(if message.is_empty() {
                    "evaluation result is malformed".into()
                } else {
                    message
                });
            }
        }
    }
    Ok(EvaluationRun {
        job_id,
        job,
        result_path,
        metrics,
        failure,
    })
}

/// Whether `argv` runs the frozen evaluation's program with other flags (`node evaluate.mjs
/// --split=test` for `node evaluate.mjs --split=dev`). Experiment workers never run it: the daemon
/// evaluates each variant with the frozen command, and its other splits (held-out test) are
/// reserved.
pub fn is_evaluation_variant(argv: &[String], evaluation: &[String]) -> bool {
    let program = |args: &[String]| -> Vec<String> {
        args.iter()
            .filter(|arg| !arg.starts_with('-'))
            .cloned()
            .collect()
    };
    program(argv) == program(evaluation)
}

fn repository_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    path
}

/// The frozen evaluation's own repository files: its program (non-flag command arguments that are
/// files in the repository) and its result file. Experiments never write them, or a variant could
/// change how it is measured.
pub fn frozen_evaluation_files(root: &Path, command: &[String], result_path: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let program = command
        .iter()
        .filter(|arg| !arg.starts_with('-') && !node_is_absolute(arg) && root.join(arg).is_file());
    for path in program
        .map(String::as_str)
        .chain([result_path])
        .map(repository_path)
    {
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

/// Node's win32 `path.isAbsolute`.
fn node_is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    matches!(bytes.first(), Some(b'/' | b'\\'))
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// A scope naming only frozen evaluation files leaves the search nothing it may change.
pub fn scope_issues(scope: &[String], frozen: &[String]) -> Vec<String> {
    if scope.is_empty()
        || !scope
            .iter()
            .all(|entry| frozen.contains(&repository_path(entry)))
    {
        return Vec::new();
    }
    vec![format!(
        "scope {} names only the frozen evaluation ({}), which experiments never change; name the files a variant edits instead, such as the method's source and configuration",
        scope.join(", "),
        frozen.join(", ")
    )]
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| string(Some(item)))
        .collect()
}

fn declared_command(project: &Value, command_id: &Value) -> Option<Value> {
    declared_task_commands(project)
        .into_iter()
        .find(|command| command["commandId"] == *command_id)
}

static DRIVE_OR_ROOT: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-zA-Z]:|^[\\/]").expect("regex"));

/// Structural checks a spawn must pass before anything runs; the probe then checks it against a
/// real evaluation.
pub fn spawn_issues(project: &Value, spawn: &Value, node_id: &str, node_type: &str) -> Vec<String> {
    let mut issues = Vec::new();
    if spawn["nodeId"] != node_id {
        issues.push(format!(
            "spawn.nodeId must be {node_id}, the node being run"
        ));
    }
    if spawn["kind"] != node_type {
        issues.push(format!("spawn.kind must be {node_type} for this node"));
    }
    let evaluation = &spawn["evaluation"];
    let commands = declared_task_commands(project);
    if !commands
        .iter()
        .any(|command| command["commandId"] == evaluation["commandId"])
    {
        let declared: Vec<String> = commands
            .iter()
            .map(|command| string(command.get("commandId")))
            .collect();
        issues.push(format!(
            "spawn.evaluation.commandId {} is not a command declared in the Project contract (declared: {})",
            string(evaluation.get("commandId")),
            if declared.is_empty() { "none".to_owned() } else { declared.join(", ") }
        ));
    }
    let path = string(evaluation.get("resultPath"));
    if DRIVE_OR_ROOT.is_match(&path) || path.split(['\\', '/']).any(|part| part == "..") {
        issues.push(
            "spawn.evaluation.resultPath must be a relative path inside the repository".into(),
        );
    }
    if let Some(command) = declared_command(project, &evaluation["commandId"]) {
        let root = PathBuf::from(string(project.get("repositoryRoot")));
        let frozen = frozen_evaluation_files(&root, &strings(&command["argv"]), &path);
        issues.extend(scope_issues(&strings(&spawn["scope"]), &frozen));
    }
    // A guardrail threshold is a change from the measured baseline; one the baseline itself fails
    // would refuse every experiment.
    for guardrail in evaluation["guardrails"].as_array().into_iter().flatten() {
        let threshold = guardrail["threshold"].as_f64().unwrap_or(f64::NAN);
        let maximize = guardrail["direction"] == "maximize";
        if if maximize {
            threshold > 0.0
        } else {
            threshold < 0.0
        } {
            issues.push(format!(
                "guardrail {}: threshold is the allowed change from the measured baseline, so for {} it must be {}",
                string(guardrail.get("name")),
                string(guardrail.get("direction")),
                if maximize { "zero or negative (-0.005 allows a 0.005 drop)" } else { "zero or positive (0.005 allows a 0.005 rise)" }
            ));
        }
    }
    issues
}

/// A probe's outcome: the measured baseline, or what the director must fix.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    Measured {
        metrics: Map<String, Value>,
        execution: EvaluationExecution,
        probe_job_id: String,
    },
    Refused {
        issues: Vec<String>,
        metric_keys: Vec<String>,
        written_json: Vec<String>,
    },
}

/// Runs the declared command once at the base commit; the measured metrics become the frozen
/// baseline.
pub async fn probe_evaluation<H: Host>(
    research: &ResearchControl<H>,
    job_runner: &dyn JobRunner,
    project: &Value,
    mission_id: &str,
    direction_id: Option<&str>,
    spawn: &Value,
    base_commit: &str,
) -> Result<Probe> {
    let evaluation = &spawn["evaluation"];
    let command = declared_command(project, &evaluation["commandId"]).ok_or_else(|| {
        crate::Error::Rejected("Cannot read properties of undefined (reading 'argv')".into())
    })?;
    let argv = strings(&command["argv"]);
    let result_path = string(evaluation.get("resultPath"));
    let execution = EvaluationExecution {
        runner: Runner::Native,
        distribution: None,
        command: argv.clone(),
        result_path: result_path.clone(),
        timeout_seconds: command["timeoutSeconds"].as_f64().unwrap_or(f64::NAN),
        uses_gpu: Some(false),
    };
    let tree = create_task_worktree(
        research,
        project,
        &create_id(IdPrefix::Tsk),
        base_commit,
        None,
    )?;
    let before = json_files(&tree.path);
    let run = run_evaluation_job(
        job_runner,
        EvaluationJob {
            project_id: &string(project.get("projectId")),
            mission_id: Some(mission_id),
            direction_id,
            autoresearch_id: None,
            experiment_id: &create_id(IdPrefix::Exp),
            commit_sha: base_commit,
            worktree: &tree.path,
            execution: &execution,
            evaluation_contract_hash: &sha256(&json!({ "probe": evaluation })),
        },
    )
    .await?;
    let metrics: Map<String, Value> = run
        .metrics
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| value.as_f64().is_some_and(f64::is_finite))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut issues: Vec<String> = run
        .failure
        .iter()
        .map(|failure| format!("probe {}: {failure}", argv.join(" ")))
        .collect();
    let names = std::iter::once(&evaluation["primaryMetric"]["name"]).chain(
        evaluation["guardrails"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|guardrail| &guardrail["name"]),
    );
    for name in names.map(|name| string(Some(name))) {
        if run.failure.is_none() && !metrics.contains_key(&name) {
            issues.push(format!(
                "metric {name} is not a finite number in {result_path}"
            ));
        }
    }
    if issues.is_empty() {
        return Ok(Probe::Measured {
            metrics,
            execution,
            probe_job_id: run.job_id,
        });
    }
    // The rejection is all the Director sees next cycle, so it names what the command really
    // wrote. Outputs are often gitignored (metrics.json), so they are found by comparing the
    // worktree before and after the run, not Git status.
    let written_json: Vec<String> = json_files(&tree.path)
        .into_iter()
        .filter(|(path, modified)| {
            before
                .iter()
                .find(|(seen, _)| seen == path)
                .map(|(_, time)| *time)
                != Some(*modified)
        })
        .map(|(path, _)| path)
        .take(20)
        .collect();
    let metric_keys: Vec<String> = metrics.keys().take(40).cloned().collect();
    if run.failure.as_deref() == Some("evaluation result file is absent") {
        issues.push(if written_json.is_empty() {
            format!("{result_path} does not exist and the command wrote no JSON file in the repository")
        } else {
            format!(
                "{result_path} does not exist; the command wrote {}: use one of these as resultPath",
                written_json.join(", ")
            )
        });
    } else if run.failure.is_none() {
        issues.push(format!(
            "numeric keys in {result_path}: {}",
            if metric_keys.is_empty() {
                "none".to_owned()
            } else {
                metric_keys.join(", ")
            }
        ));
    }
    Ok(Probe::Refused {
        issues,
        metric_keys,
        written_json,
    })
}

/// Repository-relative JSON files and their modification times, skipping Git metadata and
/// dependencies.
fn json_files(root: &Path) -> Vec<(String, f64)> {
    fn walk(root: &Path, directory: &Path, depth: usize, found: &mut Vec<(String, f64)>) {
        if depth > 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(root, &path, depth + 1, found);
            } else if kind.is_file() && name.ends_with(".json") {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1_000.0);
                let relative = path.strip_prefix(root).unwrap_or(&path);
                found.push((relative.to_string_lossy().replace('\\', "/"), modified));
            }
        }
    }
    let mut found = Vec::new();
    walk(root, root, 0, &mut found);
    found
}

/// The frozen evaluation contract: the director's metric judgment plus the daemon-measured
/// baseline.
pub fn freeze_contract(
    spawn: &Value,
    metrics: &Map<String, Value>,
    execution: &EvaluationExecution,
    probe_job_id: &str,
    mission_id: &str,
    base_commit: &str,
) -> Value {
    let evaluation = &spawn["evaluation"];
    // The Director wrote each threshold as a change from a baseline it could not see; frozen, it is
    // an absolute bound.
    let guardrails: Vec<Value> = evaluation["guardrails"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|guardrail| {
            let baseline = metrics
                .get(&string(guardrail.get("name")))
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN);
            let threshold = baseline + guardrail["threshold"].as_f64().unwrap_or(f64::NAN);
            json!({ "name": guardrail["name"], "direction": guardrail["direction"], "threshold": threshold })
        })
        .collect();
    let mut stop_conditions = Map::new();
    if !evaluation["target"].is_null() {
        stop_conditions.insert("target".into(), evaluation["target"].clone());
    }
    stop_conditions.insert(
        "maximumNoProgressRounds".into(),
        evaluation["maximumNoProgressRounds"].clone(),
    );
    json!({
        "primaryMetric": evaluation["primaryMetric"], "baselineMetrics": metrics,
        "guardrails": guardrails,
        "metrics": metrics.keys().collect::<Vec<_>>(),
        "datasets": [format!("declared command {} ({})", string(evaluation.get("commandId")), execution.command.join(" "))],
        "seeds": [0],
        "stopConditions": stop_conditions,
        "execution": execution,
        "provenance": { "commandId": evaluation["commandId"], "probeJobId": probe_job_id, "probeCommit": base_commit, "authorizedBy": "mission_approval", "missionId": mission_id },
    })
}

/// A child's ceilings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChildBudget {
    pub maximum_model_tokens: f64,
    pub maximum_wall_clock_seconds: f64,
}

fn ended(state: &str) -> bool {
    matches!(state, "failed" | "stopped" | "rejected")
}

/// A child's share of the Mission ceiling: a reserve stays with the Mission's own nodes, and the
/// rest is split over the spawn slots not yet given out. Below the floor the spawn is refused
/// (`Err` with the reason), never silently starved.
pub fn allocate_budget<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    mission: &Value,
    kind: &str,
) -> Result<std::result::Result<ChildBudget, String>> {
    let mission_id = string(mission.get("entityId"));
    let budgets = &mission["value"]["budgets"];
    let number = |value: &Value| value.as_f64().unwrap_or(f64::NAN);
    let (maximum_tokens, maximum_seconds) = (
        number(&budgets["maximumModelTokens"]),
        number(&budgets["maximumWallClockSeconds"]),
    );
    let usage = research.mission_budget_use(project_id, &mission_id)?;
    let spawn_nodes: Vec<String> = mission["value"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node["type"] == "research_direction" || node["type"] == "autoresearch")
        .map(|node| string(node.get("id")))
        .collect();
    let mut given = 0.0;
    let mut open = 0;
    for node_id in &spawn_nodes {
        let Some(child) = research.spawned_child(project_id, &mission_id, node_id)? else {
            open += 1;
            continue;
        };
        // A live child holds its whole ceiling; an ended one (stopped, failed, rejected) holds only
        // what it spent, so a retried node can respawn.
        let ceiling = if child.kind == "direction" {
            research.direction(project_id, &child.entity_id)?["value"]["budget"]["maximumModelTokens"]
                .as_f64()
                .unwrap_or(0.0)
        } else {
            number(
                &research.autoresearch_execution(project_id, &child.entity_id)?["value"]["maximumModelTokens"],
            )
        };
        if !ended(&child.state) {
            given += ceiling;
            continue;
        }
        open += 1;
        let spent = if child.kind == "direction" {
            research.direction_budget_use(project_id, &child.entity_id)?
        } else {
            research.autoresearch_budget_use(project_id, &child.entity_id)?
        };
        given += nosh_wire::js::min(ceiling, number(&spent["modelTokens"]));
    }
    let open = if open == 0 { 1.0 } else { f64::from(open) };
    let pool = maximum_tokens * (1.0 - MISSION_RESERVE) - given;
    let tokens = nosh_wire::js::min(
        pool / open,
        maximum_tokens - number(&usage["modelTokens"]) - maximum_tokens * MISSION_RESERVE,
    )
    .floor();
    let floor = if kind == "research_direction" {
        MIN_DIRECTION_TOKENS
    } else {
        MIN_AUTORESEARCH_TOKENS
    };
    // `tokens < floor` is false for NaN, as in JavaScript.
    if tokens < floor {
        return Ok(Err(format!(
            "budget_insufficient: {} model tokens available for this {kind}, at least {} needed",
            number_string(tokens),
            number_string(floor)
        )));
    }
    Ok(Ok(ChildBudget {
        maximum_model_tokens: tokens,
        maximum_wall_clock_seconds: nosh_wire::js::max(
            600.0,
            maximum_seconds - number(&usage["wallClockSeconds"]),
        ),
    }))
}

/// `spawnMissionChild`'s input.
pub struct MissionChild<'a> {
    pub mission: &'a Value,
    pub node_id: &'a str,
    pub attempt: f64,
    pub spawn: &'a Value,
    pub evaluation_contract: &'a Value,
    pub base_commit: &'a str,
    pub budget: ChildBudget,
}

fn completed_child(intent: &Value) -> Option<String> {
    (intent["state"] == "completed").then(|| string(intent["result"].get("childId")))
}

/// Creates and starts a Mission-spawned child under a durable intent; replaying the same key
/// resumes the same child.
pub fn spawn_mission_child<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    input: MissionChild<'_>,
) -> Result<String> {
    let mission_id = string(input.mission.get("entityId"));
    let attempt = number_string(input.attempt);
    let key = format!("child-spawn:{mission_id}:{}:{attempt}", input.node_id);
    let origin = json!({ "scopeType": "mission", "scopeId": mission_id, "nodeId": input.node_id, "attempt": input.attempt });
    let budget = json!({ "maximumModelTokens": input.budget.maximum_model_tokens, "maximumWallClockSeconds": input.budget.maximum_wall_clock_seconds });
    let intent = research.begin_external_operation(
        project_id,
        "research.child_spawn",
        &key,
        &json!({ "origin": origin, "spawn": input.spawn, "evaluationContract": input.evaluation_contract, "baseCommit": input.base_commit, "budget": budget }),
    )?;
    if let Some(child) = completed_child(&intent) {
        return Ok(child);
    }
    let spawn = input.spawn;
    let created = if spawn["kind"] == "research_direction" {
        let decision_use = match spawn.get("decisionUse") {
            None | Some(Value::Null) => string(spawn.get("question")),
            decision => string(decision),
        };
        research.create_direction(
            project_id,
            &DirectionInput {
                question: string(spawn.get("question")),
                decision_use,
                mission_id: Some(mission_id.clone()),
                evaluation_contract: Some(input.evaluation_contract.clone()),
                idempotency_key: format!("{key}:create"),
            },
            Some(&SpawnedDirection {
                origin: origin.clone(),
                base_commit: input.base_commit.into(),
                budget: budget.as_object().cloned().unwrap_or_default(),
            }),
        )?
    } else {
        research.create_autoresearch(
            project_id,
            &autoresearch_input(
                spawn,
                None,
                Some(mission_id.clone()),
                Some(input.evaluation_contract.clone()),
                input.budget.maximum_model_tokens,
                Some(input.budget.maximum_wall_clock_seconds),
                format!("{key}:create"),
            ),
            Some(&SpawnedAutoresearch {
                invoked_by_role: "mission_director".into(),
                origin: origin.clone(),
                base_commit: Some(input.base_commit.into()),
            }),
        )?
    };
    let child_id = string(created.get("entityId"));
    start_child(research, project_id, &child_id, &key)?;
    research.complete_external_operation(
        project_id,
        &string(intent.get("intentId")),
        &json!({ "childId": child_id }),
    )?;
    Ok(child_id)
}

fn autoresearch_input(
    spawn: &Value,
    direction_id: Option<String>,
    mission_id: Option<String>,
    evaluation_contract: Option<Value>,
    maximum_model_tokens: f64,
    maximum_wall_clock_seconds: Option<f64>,
    idempotency_key: String,
) -> AutoresearchInput {
    AutoresearchInput {
        decision_question: string(spawn.get("question")),
        direction_id,
        mission_id,
        family_tags: Some(strings(&spawn["familyTags"])),
        scope: Some(strings(&spawn["scope"])),
        evaluation_contract,
        maximum_experiments: Some(SPAWNED_EXPERIMENTS),
        maximum_rounds: Some(SPAWNED_EXPERIMENTS),
        maximum_wall_clock_seconds,
        maximum_model_tokens: Some(maximum_model_tokens),
        maximum_gpu_seconds: Some(0.0),
        maximum_disk_bytes: None,
        idempotency_key,
    }
}

/// `spawnDirectionAutoresearch`'s input.
pub struct DirectionChild<'a> {
    pub direction_id: &'a str,
    pub mission_id: Option<&'a str>,
    pub node_id: &'a str,
    pub ordinal: f64,
    pub spawn: &'a Value,
    pub maximum_model_tokens: f64,
}

/// Creates and starts a Direction-spawned Autoresearch under the Direction's frozen contract.
pub fn spawn_direction_autoresearch<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    input: DirectionChild<'_>,
) -> Result<String> {
    let key = format!(
        "child-spawn:{}:{}:{}",
        input.direction_id,
        input.node_id,
        number_string(input.ordinal)
    );
    let origin = json!({ "scopeType": "direction", "scopeId": input.direction_id, "nodeId": input.node_id, "attempt": input.ordinal });
    let intent = research.begin_external_operation(
        project_id,
        "research.child_spawn",
        &key,
        &json!({ "origin": origin, "spawn": input.spawn, "maximumModelTokens": input.maximum_model_tokens }),
    )?;
    if let Some(child) = completed_child(&intent) {
        return Ok(child);
    }
    let created = research.create_autoresearch(
        project_id,
        &autoresearch_input(
            input.spawn,
            Some(input.direction_id.into()),
            input.mission_id.map(str::to_owned),
            None,
            input.maximum_model_tokens,
            None,
            format!("{key}:create"),
        ),
        Some(&SpawnedAutoresearch {
            invoked_by_role: "research_director".into(),
            origin,
            base_commit: None,
        }),
    )?;
    let child_id = string(created.get("entityId"));
    start_child(research, project_id, &child_id, &key)?;
    research.complete_external_operation(
        project_id,
        &string(intent.get("intentId")),
        &json!({ "childId": child_id }),
    )?;
    Ok(child_id)
}

fn start_child<H: Host>(
    research: &ResearchControl<H>,
    project_id: &str,
    child_id: &str,
    key: &str,
) -> Result<()> {
    let version = |entity: &Value| entity["version"].as_i64().unwrap_or(-1);
    if child_id.starts_with("dir_") {
        let direction = research.direction(project_id, child_id)?;
        if direction["state"] == "draft" || direction["state"] == "proposed" {
            research.start(
                project_id,
                "directions",
                child_id,
                version(&direction),
                &format!("{key}:start"),
            )?;
        }
        return Ok(());
    }
    let execution = research.autoresearch_execution(project_id, child_id)?;
    if execution["state"] == "draft" {
        research.start(
            project_id,
            "autoresearch",
            child_id,
            version(&execution),
            &format!("{key}:start"),
        )?;
    }
    Ok(())
}
