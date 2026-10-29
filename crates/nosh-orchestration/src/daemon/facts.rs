//! Daemon-owned task facts, as the task methods of `apps/noshd/src/daemon.ts`: the facts filled
//! into a task's terminal records, `nosh_run` Jobs, progress notes, and Artifact reads and
//! snapshots.

use super::{NoshDaemon, error_text};
use crate::research_spawner::is_evaluation_variant;
use crate::task_postflight::{
    completion_run_issues, git_changed_paths, git_output, git_worktree_state, write_up_only,
};
use crate::task_worktree::task_worktree_path;
use crate::{Error, Result, rejected};
use nosh_agent::commit::{Commit, commit_all_workspace_changes};
use nosh_agent::pi::Inspection;
use nosh_agent::policy::SessionOptions;
use nosh_agent::terminal::TerminalContext;
use nosh_research::artifacts::{ArtifactInput, ArtifactStore};
use nosh_research::project::read_project_contract;
use nosh_research::research_control::task_command_runs;
use nosh_sys::jobs::{JobSpec, Stream};
use nosh_wire::js::{length, slice_to, string};
use nosh_wire::{IdPrefix, canonical_json, create_id, now_utc, sha256};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn schema_uri(name: &str) -> String {
    format!("https://nosh.dev/schemas/{name}/v1")
}

fn has_schema(record: &Value, name: &str) -> bool {
    record.get("$schema").and_then(Value::as_str) == Some(schema_uri(name).as_str())
}

fn regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("static pattern")
}

/// `text.slice(start, start + count)` by UTF-16 code units.
fn slice_units(text: &str, start: usize, count: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().skip(start).take(count).collect();
    String::from_utf16_lossy(&units)
}

/// `JSON.stringify(value, null, 2)`, with sorted keys.
fn pretty(value: &Value) -> String {
    let mut value = value.clone();
    nosh_wire::normalize_numbers(&mut value);
    serde_json::to_string_pretty(&value).expect("JSON values serialize")
}

/// The Rust host the daemon's Jobs run on; TypeScript hashes its Node version instead.
fn environment_lock_hash() -> String {
    sha256(
        &json!({ "runtime": "nosh-rust", "platform": std::env::consts::OS, "arch": std::env::consts::ARCH }),
    )
}

fn task_runs(events: &[Value], task_id: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            event["type"] == "task.command_run"
                && event["payload"].is_object()
                && event["payload"]["taskId"] == task_id
        })
        .map(|event| event["payload"].clone())
        .collect()
}

/// A completion's commands[] is each command's final state: its latest run. Earlier failing runs
/// are the ordinary edit-test iterations of the task (still recorded, and shown to reviewers).
fn latest_run_per_command(runs: &[Value]) -> Vec<Value> {
    let mut latest: Vec<(String, Value)> = Vec::new();
    for run in runs {
        let key = string(run.get("commandId"));
        latest.retain(|(command, _)| *command != key);
        latest.push((key, run.clone()));
    }
    latest.into_iter().map(|(_, run)| run).collect()
}

/// The `not ok` entries of TAP output (node --test off a terminal) with their diagnostics,
/// bounded: which tests failed, and why.
fn tap_failures(output: &str, maximum_characters: usize) -> String {
    let (not_ok, closes) = (regex(r"^\s*not ok\b"), regex(r"^\s*(ok\b|# Subtest:)"));
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut open = false;
    for line in regex(r"\r?\n").split(output) {
        if not_ok.is_match(line) {
            blocks.push(vec![line]);
            open = true;
        } else if open && closes.is_match(line) {
            open = false;
        } else if open {
            let block = blocks.last_mut().expect("an open block exists");
            block.push(line);
            if super::js_trim(line) == "..." || block.len() >= 40 {
                open = false;
            }
        }
    }
    let text: Vec<String> = blocks.iter().map(|block| block.join("\n")).collect();
    slice_to(&text.join("\n"), maximum_characters)
}

/// A literature source's canonical URL is derivable when the model omits it: a cited worktree
/// file is file:///<path>, a DOI is https://doi.org/<doi>.
fn with_source_urls(record: Value) -> Value {
    let Some(sources) = record
        .get("sources")
        .and_then(Value::as_array)
        .filter(|_| has_schema(&record, "librarian-completion"))
    else {
        return record;
    };
    let (citation, doi) = (
        regex(r"^artifact:(\S+)$"),
        regex(r"(?i)^(?:doi:|https?://(?:dx\.)?doi\.org/)(10\.\S+)$"),
    );
    let sources: Vec<Value> = sources
        .iter()
        .map(|source| {
            let named = source
                .get("canonicalUrl")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty());
            if !source.is_object() || named {
                return source.clone();
            }
            let cited = source
                .get("artifactId")
                .and_then(Value::as_str)
                .and_then(|id| citation.captures(id))
                .map(|c| c[1].to_owned());
            let persistent = source
                .get("persistentId")
                .and_then(Value::as_str)
                .and_then(|id| doi.captures(id))
                .map(|c| c[1].to_owned());
            let url = match (cited, persistent) {
                (Some(cited), _) => {
                    Some(format!("file:///{}", regex(r"^\.?/").replace(&cited, "")))
                }
                (None, Some(doi)) => Some(format!("https://doi.org/{doi}")),
                _ => None,
            };
            let mut source = source.clone();
            if let Some(url) = url {
                source["canonicalUrl"] = json!(url);
            }
            source
        })
        .collect();
    let mut record = record;
    record["sources"] = Value::Array(sources);
    record
}

const KIND_RANK: [&str; 4] = [
    "artifact_file",
    "artifact_source",
    "artifact_report",
    "artifact_bibliography",
];

fn kind_rank(kind: Option<&str>) -> i64 {
    kind.and_then(|kind| KIND_RANK.iter().position(|ranked| *ranked == kind))
        .map_or(-1, |rank| rank as i64)
}

fn citation_kind(key: &str, schema: &str) -> &'static str {
    match key {
        "bibliographyArtifactId" => "artifact_bibliography",
        "reportArtifactId" => "artifact_report",
        _ if schema == schema_uri("librarian-completion") => "artifact_source",
        _ => "artifact_file",
    }
}

/// Every whole-string "artifact:<path>" (no spaces) with the most specific kind any of its citing
/// fields implies, in first-citation order.
fn collect_artifact_citations(
    value: &Value,
    schema: &str,
    key: &str,
    kinds: &mut Vec<(String, String)>,
) {
    match value {
        Value::String(text) => {
            let Some(path) = regex(r"^artifact:(\S+)$")
                .captures(text)
                .map(|c| c[1].to_owned())
            else {
                return;
            };
            let kind = citation_kind(key, schema);
            match kinds.iter_mut().find(|(cited, _)| *cited == path) {
                Some(entry) if kind_rank(Some(kind)) > kind_rank(Some(&entry.1)) => {
                    entry.1 = kind.to_owned()
                }
                Some(_) => {}
                None => kinds.push((path, kind.to_owned())),
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_artifact_citations(item, schema, key, kinds)),
        Value::Object(fields) => fields
            .iter()
            .for_each(|(field, item)| collect_artifact_citations(item, schema, field, kinds)),
        _ => {}
    }
}

/// Replaces each citation with the art_ ID of its snapshotted worktree file.
fn replace_artifact_citations(value: &Value, resolved: &HashMap<String, String>) -> Value {
    match value {
        Value::String(text) => regex(r"^artifact:(\S+)$")
            .captures(text)
            .and_then(|c| resolved.get(&c[1]))
            .map_or_else(|| value.clone(), |id| json!(id)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace_artifact_citations(item, resolved))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(field, item)| (field.clone(), replace_artifact_citations(item, resolved)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// The progress-update for a task's `done -> next` note (the `ordinal`th of its attempt).
fn progress_update(
    task_id: &str,
    attempt: &Value,
    agent_id: &str,
    note: &str,
    ordinal: usize,
) -> Value {
    let arrow = regex(r"\s*->\s*");
    let mut parts = arrow.split(note);
    let summary = parts.next().unwrap_or_default();
    let next = parts.next().unwrap_or_default();
    json!({ "$schema": schema_uri("progress-update"), "schemaVersion": 1, "progressId": format!("progress_{}_{ordinal}", &task_id[4..]), "taskId": task_id, "attempt": attempt, "agentId": agent_id, "kind": "milestone", "summary": slice_to(summary, 2_000), "goalStackIds": [task_id], "durableDelta": { "commitIds": [], "artifactIds": [], "evidenceIds": [], "graphNodeStateChanges": [], "closedDefectIds": [] }, "validation": [], "currentOperation": slice_to(summary, 1_000), "nextOperation": slice_to(next, 1_000), "estimatedRemainingSeconds": null, "newRisk": null, "blockerId": null, "attemptFingerprint": sha256(&json!({ "taskId": task_id, "attempt": attempt })), "emittedAt": now_utc() })
}

/// Runs `write` against a file `name` in a fresh temporary directory, removed afterwards.
fn with_temporary_file<T>(
    prefix: &str,
    name: &str,
    content: &str,
    use_it: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let directory = tempfile::Builder::new().prefix(prefix).tempdir()?;
    let file = directory.path().join(name);
    std::fs::write(&file, content)?;
    use_it(&file)
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

impl NoshDaemon {
    fn task_agent(&self, project_id: &str, agent_id: Option<&str>) -> Option<Inspection> {
        let agent_id = agent_id.filter(|id| !id.is_empty())?;
        self.agents
            .inspect()
            .into_iter()
            .find(|agent| agent.agent_id == agent_id && agent.project_id == project_id)
    }

    fn task_events(&self, project_id: &str, task_id: &str) -> Result<Vec<Value>> {
        Ok(task_runs(
            &self.store_for(project_id)?.replay(project_id, 0)?,
            task_id,
        ))
    }

    /// Daemon-owned facts a model would otherwise spend tool calls on, filled into a task's
    /// terminal records before validation: commands[] (its nosh_run Jobs), "artifact:<path>"
    /// citations (snapshotted as Artifacts), codeChanges (the daemon commits the worktree on the
    /// worker's behalf), and the episode's changedFiles. A bad citation is a correctable reason;
    /// a failed commit is a "Terminal policy:" reason.
    pub fn with_daemon_facts(
        &self,
        context: &TerminalContext,
        submitted: Vec<Value>,
    ) -> Result<Vec<Value>> {
        let mut records: Vec<Value> = Vec::new();
        for record in submitted {
            records.push(with_source_urls(
                self.with_daemon_commands(context, record)?,
            ));
        }
        let agent = match context.task_id.as_deref().filter(|task| !task.is_empty()) {
            Some(task) => self.agents.inspect().into_iter().find(|agent| {
                agent.agent_id == context.agent_id
                    && agent.project_id == context.project_id
                    && agent.task_id.as_deref() == Some(task)
            }),
            None => None,
        };
        let Some(agent) = agent else {
            return Ok(records);
        };
        let task_id = agent.task_id.clone().unwrap_or_default();
        let project_id = context.project_id.as_str();
        let Some(packet) = self.task_packet_facts(project_id, &task_id)? else {
            return Ok(records);
        };
        let workspace = &packet["workspace"];
        if !workspace.is_object() {
            return Ok(records);
        }
        records = records
            .into_iter()
            .map(|record| self.with_evidence_sources(project_id, &agent, record))
            .collect::<Result<_>>()?;
        // One Artifact per cited path per envelope, with its most specific kind (a report cited
        // again in the episode is the same report).
        let mut kinds: Vec<(String, String)> = Vec::new();
        for record in &records {
            collect_artifact_citations(record, &string(record.get("$schema")), "", &mut kinds);
        }
        let mut resolved = HashMap::new();
        for (path, kind) in kinds {
            let artifact_id =
                self.snapshot_task_artifact(project_id, &agent, &packet, &path, &kind)?;
            resolved.insert(path, artifact_id);
        }
        records = records
            .iter()
            .map(|record| replace_artifact_citations(record, &resolved))
            .collect();
        records = records
            .into_iter()
            .map(|record| self.with_run_artifacts(project_id, &agent, record))
            .collect::<Result<_>>()?;
        let Some(index) = records
            .iter()
            .position(|record| has_schema(record, "general-worker-completion"))
        else {
            return Ok(records);
        };
        if packet["permissions"]["gitCommit"] != true {
            return Ok(records);
        }
        if let Err(error) = self.commit_task_worktree(
            project_id,
            &agent,
            &packet,
            &format!("nosh: {task_id} completion"),
        ) {
            return rejected(format!(
                "Terminal policy: daemon commit of your worktree failed: {}",
                error_text(&error, "unknown error")
            ));
        }
        let worktree = self.task_worktree(project_id, workspace)?;
        let starting = string(workspace.get("startingCommit"));
        let (ending, _) = git_worktree_state(&worktree)?;
        let changed = git_changed_paths(&worktree, &starting, &ending)?;
        let diff = if changed.is_empty() {
            None
        } else {
            Some(self.snapshot_task_diff(project_id, &agent, &worktree, &starting, &ending)?)
        };
        let completion = &records[index];
        let mut code_changes = completion
            .get("codeChanges")
            .filter(|changes| changes.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        code_changes["startingCommit"] = json!(starting);
        code_changes["endingCommit"] = json!(ending);
        code_changes["changedPaths"] = json!(changed);
        code_changes["diffArtifactId"] = json!(diff);
        code_changes["branch"] = workspace["branch"].clone();
        // Cited runs are checked now, while the worker can still correct a claim. Ids that are not
        // this task's nosh_runs (the daemon's own jobs, such as a Direction baseline) are left to
        // postflight, which knows them.
        let runs: Vec<Value> = task_command_runs(&self.research, project_id)?
            .into_iter()
            .filter(|run| run["taskId"] == task_id.as_str())
            .collect();
        let cited: Vec<String> = completion
            .get("criteria")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|criterion| criterion.get("validatorRunIds").and_then(Value::as_array))
            .flatten()
            .map(|id| string(Some(id)))
            .collect();
        let foreign: Vec<String> = cited
            .into_iter()
            .filter(|id| !runs.iter().any(|run| run["jobId"] == id.as_str()))
            .collect();
        let mut checked = completion.clone();
        checked["codeChanges"] = code_changes.clone();
        let repository_root =
            PathBuf::from(string(self.core.project(project_id)?.get("repositoryRoot")));
        let write_up = write_up_only(&repository_root);
        let issues = completion_run_issues(&runs, &task_id, &checked, &foreign, Some(&write_up));
        if !issues.is_empty() {
            return rejected(format!(
                "Cited runs: {}. Cite only passing runs made after your last edit; where your instructions allow it, claim a comparison across configurations deferred_to_review.",
                issues.join("; ")
            ));
        }
        Ok(records
            .into_iter()
            .enumerate()
            .map(|(position, mut record)| {
                if position == index {
                    record["codeChanges"] = code_changes.clone();
                } else if has_schema(&record, "episode-draft") {
                    record["changedFiles"] = json!(changed);
                }
                record
            })
            .collect())
    }

    /// The task's latest durable Task Packet.
    fn task_packet_facts(&self, project_id: &str, task_id: &str) -> Result<Option<Value>> {
        Ok(self
            .research
            .records(project_id, Some("task-packet"))?
            .into_iter()
            .rfind(|entry| entry["taskId"] == task_id))
    }

    fn task_worktree(&self, project_id: &str, workspace: &Value) -> Result<PathBuf> {
        let repository_root =
            PathBuf::from(string(self.core.project(project_id)?.get("repositoryRoot")));
        Ok(task_worktree_path(
            &repository_root,
            &string(workspace.get("worktreeId")),
        ))
    }

    /// Workers have no commit tool: the daemon commits every worktree change under the task's
    /// commit authority. `None` when unauthorized or clean.
    fn commit_task_worktree(
        &self,
        project_id: &str,
        agent: &Inspection,
        packet: &Value,
        message: &str,
    ) -> Result<Option<Commit>> {
        if packet["permissions"]["gitCommit"] != true || !packet["workspace"].is_object() {
            return Ok(None);
        }
        fn typed<T: serde::de::DeserializeOwned>(packet: &Value, field: &str) -> Result<T> {
            serde_json::from_value(packet[field].clone()).map_err(|error| {
                Error::Rejected(format!("Task Packet {field} is invalid: {error}"))
            })
        }
        let options = SessionOptions {
            project_id: project_id.to_owned(),
            mission_id: agent.mission_id.clone(),
            direction_id: agent.direction_id.clone(),
            autoresearch_id: agent.autoresearch_id.clone(),
            experiment_id: agent.experiment_id.clone(),
            run_id: None,
            job_id: None,
            task_id: agent.task_id.clone(),
            agent_id: agent.agent_id.clone(),
            role: agent.role,
            cwd: self.task_worktree(project_id, &packet["workspace"])?,
            package_path: PathBuf::new(),
            tools: None,
            runtime_scoped: false,
            task_permissions: Some(typed(packet, "permissions")?),
            task_workspace: Some(typed(packet, "workspace")?),
            thinking_level: None,
            model: None,
        };
        commit_all_workspace_changes(&options, message).map_err(Error::Rejected)
    }

    /// A `PROGRESS: done -> next` line in a live task's assistant text, recorded as a daemon-built
    /// progress-update (no model tool call).
    pub(super) fn record_progress_note(
        &self,
        project_id: &str,
        attempt_key: &str,
        agent_id: Option<&str>,
        record: &Value,
    ) -> Result<Value> {
        let agent = self.task_agent(project_id, agent_id);
        let task_id = agent
            .as_ref()
            .and_then(|agent| agent.task_id.clone())
            .filter(|task| !task.is_empty());
        let packet = match &task_id {
            Some(task_id) => self.task_packet_facts(project_id, task_id)?,
            None => None,
        };
        let note = match record.get("note") {
            None | Some(Value::Null) => String::new(),
            note => super::js_trim(&string(note)).to_owned(),
        };
        let (Some(agent), Some(task_id), Some(packet)) = (agent, task_id, packet) else {
            return Ok(
                json!({ "accepted": false, "error": "Progress notes require an active daemon-bound task session" }),
            );
        };
        if attempt_key != format!("task:{task_id}") || note.is_empty() {
            return Ok(
                json!({ "accepted": false, "error": "Progress notes require an active daemon-bound task session" }),
            );
        }
        let ordinal = self
            .research
            .submitted(project_id, Some(attempt_key))?
            .iter()
            .filter(|entry| has_schema(&entry["payload"], "progress-update"))
            .count()
            + 1;
        let attempt = packet.get("attempt").cloned().unwrap_or(Value::Null);
        let progress = progress_update(&task_id, &attempt, &agent.agent_id, &note, ordinal);
        self.submit_record(
            "nosh_progress_emit",
            project_id,
            attempt_key,
            &progress,
            Some(&agent.agent_id),
        )
    }

    /// Each nosh_run of the task becomes a hash-checked Artifact (command, commit, exit code,
    /// output tail): commands[] cite it, and so does every criterion whose validator is that Job.
    /// Downstream reviewers, who cannot run commands, verify a run from that Artifact set.
    fn with_run_artifacts(
        &self,
        project_id: &str,
        agent: &Inspection,
        record: Value,
    ) -> Result<Value> {
        if !has_schema(&record, "general-worker-completion") {
            return Ok(record);
        }
        let task_id = agent.task_id.clone().unwrap_or_default();
        let runs = self.task_events(project_id, &task_id)?;
        if runs.is_empty() {
            return Ok(record);
        }
        let mut by_job: HashMap<String, String> = HashMap::new();
        for run in &runs {
            let artifact_id = format!(
                "art_{}",
                &sha256(&json!({ "taskId": task_id, "jobId": run["jobId"] }))[7..39]
            );
            let recorded = with_temporary_file(
                "nosh-run-",
                "run.json",
                &format!("{}\n", canonical_json(run)),
                |file| {
                    self.record_task_artifact(
                        project_id,
                        agent,
                        file,
                        &artifact_id,
                        "artifact_run-result",
                        "application/json",
                        json!({ "type": "noshd", "taskId": task_id, "jobId": run["jobId"] }),
                    )
                },
            )?;
            by_job.insert(string(run.get("jobId")), recorded);
        }
        let latest: HashMap<String, String> = latest_run_per_command(&runs)
            .iter()
            .map(|run| (string(run.get("commandId")), string(run.get("jobId"))))
            .collect();
        let mut record = record;
        if let Some(commands) = record.get("commands").and_then(Value::as_array) {
            let commands: Vec<Value> = commands
                .iter()
                .map(|command| {
                    if !command.is_object() {
                        return command.clone();
                    }
                    let job = latest
                        .get(&string(command.get("commandId")))
                        .cloned()
                        .unwrap_or_default();
                    let mut command = command.clone();
                    command["resultArtifactId"] = json!(by_job.get(&job));
                    command
                })
                .collect();
            record["commands"] = Value::Array(commands);
        }
        if let Some(criteria) = record.get("criteria").and_then(Value::as_array) {
            let criteria: Vec<Value> = criteria
                .iter()
                .map(|criterion| {
                    let Some(ids) = criterion
                        .get("validatorRunIds")
                        .and_then(Value::as_array)
                        .filter(|_| criterion.is_object())
                    else {
                        return criterion.clone();
                    };
                    let mut artifact_ids: Vec<String> = criterion
                        .get("artifactIds")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|id| string(Some(id)))
                        .collect();
                    for id in ids.iter().filter_map(|id| by_job.get(&string(Some(id)))) {
                        artifact_ids.push(id.clone());
                    }
                    let mut seen = std::collections::HashSet::new();
                    artifact_ids.retain(|id| seen.insert(id.clone()));
                    let mut criterion = criterion.clone();
                    criterion["artifactIds"] = json!(artifact_ids);
                    criterion
                })
                .collect();
            record["criteria"] = Value::Array(criteria);
        }
        Ok(record)
    }

    /// commands[] in a worker completion is factual and daemon-owned: when the task ran nosh_run,
    /// it is exactly those runs (models kept inventing its shape).
    pub fn with_daemon_commands(&self, context: &TerminalContext, record: Value) -> Result<Value> {
        let Some(task_id) = context.task_id.as_deref().filter(|task| !task.is_empty()) else {
            return Ok(record);
        };
        if !has_schema(&record, "general-worker-completion") {
            return Ok(record);
        }
        let runs = latest_run_per_command(&self.task_events(&context.project_id, task_id)?);
        if runs.is_empty() {
            return Ok(record);
        }
        let commands: Vec<Value> = runs
            .iter()
            .map(|run| {
                let argv: Vec<String> = run.get("argv").and_then(Value::as_array).into_iter().flatten().map(|arg| string(Some(arg))).collect();
                let exit_code = if run["exitCode"].is_number() { run["exitCode"].clone() } else { Value::Null };
                json!({ "commandId": string(run.get("commandId")), "displayCommand": slice_to(&argv.join(" "), 2_000), "exitCode": exit_code, "resultArtifactId": null })
            })
            .collect();
        let mut record = record;
        record["commands"] = Value::Array(commands);
        Ok(record)
    }

    /// Runs one command declared in the approved Project contract for a live task session, as a
    /// supervised Job in its worktree. The agent never spawns processes.
    pub(super) async fn run_task_command(
        self: &Rc<Self>,
        project_id: &str,
        agent_id: Option<&str>,
        record: &Value,
    ) -> Value {
        match self
            .try_run_task_command(project_id, agent_id, record)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                json!({ "accepted": false, "error": error_text(&error, "nosh_run failed") })
            }
        }
    }

    async fn try_run_task_command(
        self: &Rc<Self>,
        project_id: &str,
        agent_id: Option<&str>,
        record: &Value,
    ) -> Result<Value> {
        self.assert_project_writable(project_id)?;
        let agent = self.task_agent(project_id, agent_id).filter(|agent| {
            agent
                .task_id
                .as_deref()
                .is_some_and(|task| !task.is_empty())
        });
        let Some(agent) = agent else {
            return rejected("nosh_run requires an active daemon-bound task session");
        };
        let task_id = agent.task_id.clone().unwrap_or_default();
        let packet = self
            .task_packet_facts(project_id, &task_id)?
            .unwrap_or(Value::Null);
        let permissions = &packet["permissions"];
        let authorized = permissions["subprocess"] == "allowlisted"
            && permissions["allowedToolIds"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool == "tool_nosh.run"))
            && packet["workspace"]["worktreeId"]
                .as_str()
                .is_some_and(|id| !id.is_empty());
        if !authorized {
            return rejected("This task is not authorized to run commands");
        }
        let repository_root =
            PathBuf::from(string(self.core.project(project_id)?.get("repositoryRoot")));
        let execution = read_project_contract(&repository_root)?
            .get("execution")
            .cloned()
            .filter(Value::is_object);
        let commands = execution
            .as_ref()
            .and_then(|execution| execution["commands"].as_array().cloned())
            .unwrap_or_default();
        let command = execution.as_ref().and_then(|_| {
            commands
                .iter()
                .find(|entry| {
                    record.is_object() && entry.get("commandId") == record.get("commandId")
                })
                .cloned()
        });
        let (Some(execution), Some(command)) = (execution, command) else {
            let declared: Vec<String> = commands
                .iter()
                .map(|entry| string(entry.get("commandId")))
                .collect();
            let declared = if declared.is_empty() {
                "none".to_owned()
            } else {
                declared.join(", ")
            };
            return rejected(format!(
                "Unknown commandId; the approved Project contract declares: {declared}"
            ));
        };
        let command_id = string(command.get("commandId"));
        let argv: Vec<String> = command["argv"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|arg| string(Some(arg)))
            .collect();
        if packet["taskType"] == "task_experiment.implementation"
            && let Some(autoresearch_id) = agent.autoresearch_id.as_deref()
        {
            let frozen = self
                .research
                .autoresearch_execution(project_id, autoresearch_id)?;
            let frozen: Option<Vec<String>> =
                frozen["value"]["evaluationContract"]["execution"]["command"]
                    .as_array()
                    .map(|command| command.iter().map(|arg| string(Some(arg))).collect());
            if frozen.is_some_and(|frozen| is_evaluation_variant(&argv, &frozen)) {
                return rejected(format!(
                    "{command_id} runs the frozen evaluation program. The daemon evaluates this variant with the frozen command after your final answer, and comparisons happen across rounds; run only the other declared commands (such as tests)."
                ));
            }
        }
        let worktree = task_worktree_path(
            &repository_root,
            &string(packet["workspace"].get("worktreeId")),
        );
        // Each run is tied to an exact commit: the daemon commits the worker's current edits first
        // (workers have no commit tool).
        if let Err(error) = self.commit_task_worktree(
            project_id,
            &agent,
            &packet,
            &format!("nosh: {task_id} edits before {command_id}"),
        ) {
            return rejected(format!(
                "Could not commit your edits before the run: {}",
                error_text(&error, "unknown error")
            ));
        }
        let (head, dirty) = git_worktree_state(&worktree)?;
        // The same contract command on the same clean commit has already been measured: answer
        // from the record at once instead of re-running it.
        let prior = if dirty {
            None
        } else {
            self.task_events(project_id, &task_id)?
                .into_iter()
                .rfind(|run| {
                    run["commandId"] == command_id.as_str()
                        && run["commit"] == head.as_str()
                        && run["dirty"] == false
                })
        };
        if let Some(prior) = prior {
            let failed = if prior["exitCode"] == 0 {
                String::new()
            } else {
                tap_failures(
                    &self
                        .jobs
                        .tail(&string(prior.get("jobId")), Stream::Stdout, 256 * 1024)?,
                    4_000,
                )
            };
            let mut answer = prior.clone();
            answer["accepted"] = json!(true);
            answer["cached"] = json!(true);
            answer["citable"] = json!(prior["state"] == "completed" && prior["exitCode"] == 0);
            answer["note"] = json!(format!(
                "Already ran {command_id} on this exact commit; returning that recorded result (cite {}).",
                string(prior.get("jobId"))
            ));
            if !failed.is_empty() {
                answer["failedTests"] = json!(failed);
            }
            return Ok(answer);
        }
        let job_id = create_id(IdPrefix::Job);
        let timeout_seconds = command["timeoutSeconds"].as_f64().unwrap_or(f64::NAN);
        self.start_job(&JobSpec {
            job_id: job_id.clone(),
            project_id: project_id.to_owned(),
            mission_id: agent.mission_id.clone(),
            direction_id: agent.direction_id.clone(),
            autoresearch_id: agent.autoresearch_id.clone(),
            run_id: create_id(IdPrefix::Run),
            experiment_id: agent.experiment_id.clone(),
            commit_sha: head.clone(),
            working_directory: worktree.to_string_lossy().into_owned(),
            runner: serde_json::from_value(execution["runner"].clone()).map_err(|error| {
                Error::Rejected(format!("Project contract runner is invalid: {error}"))
            })?,
            distribution: None,
            command: argv.clone(),
            checkpoint_command: None,
            environment_lock_hash: environment_lock_hash(),
            evaluation_contract_hash: sha256(&execution),
            timeout_seconds,
            uses_gpu: false,
        })?;
        let deadline = Instant::now() + Duration::from_secs_f64((timeout_seconds + 60.0).max(0.0));
        let mut job = self.jobs.get(&job_id)?;
        while !job.state.is_terminal() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            job = self.jobs.get(&job_id)?;
        }
        // The record keeps a bounded stdout tail so reviewers can verify what a run printed
        // without the logs.
        let result = json!({ "taskId": task_id, "jobId": job_id, "commandId": command_id, "description": command["description"], "argv": argv, "commit": head, "dirty": dirty, "state": job.state, "exitCode": job.exit_code, "failureReason": job.failure_reason, "stdoutTail": self.jobs.tail(&job_id, Stream::Stdout, 1_500)? });
        self.append_draft(&super::event_draft(
            "task.command_run",
            "noshd",
            json!({ "projectId": project_id, "missionId": agent.mission_id, "directionId": agent.direction_id, "autoresearchId": agent.autoresearch_id, "experimentId": agent.experiment_id, "runId": null, "jobId": job_id, "agentId": agent.agent_id }),
            json!(format!("task:{task_id}")),
            Value::Null,
            result.clone(),
        ))?;
        // A failed test run's tail holds only the counts (TAP reports each failure where it
        // happens), so the worker also gets the failures.
        let failed = if job.exit_code == Some(0) {
            String::new()
        } else {
            tap_failures(&self.jobs.tail(&job_id, Stream::Stdout, 256 * 1024)?, 4_000)
        };
        let mut answer = result;
        answer["accepted"] = json!(true);
        answer["citable"] = json!(
            job.state == nosh_sys::jobs::JobState::Completed && job.exit_code == Some(0) && !dirty
        );
        answer["stderrTail"] = json!(self.jobs.tail(&job_id, Stream::Stderr, 2_000)?);
        if !failed.is_empty() {
            answer["failedTests"] = json!(failed);
        }
        Ok(answer)
    }

    /// Read-only view of one Project Artifact (or recorded Evidence) for a live task session
    /// (reviewers verify what workers registered). Hash-checked; text only, bounded.
    pub(super) fn read_task_artifact(
        &self,
        project_id: &str,
        agent_id: Option<&str>,
        record: &Value,
    ) -> Value {
        match self.try_read_task_artifact(project_id, agent_id, record) {
            Ok(result) => result,
            Err(error) => {
                json!({ "accepted": false, "error": error_text(&error, "nosh_artifact_read failed") })
            }
        }
    }

    fn try_read_task_artifact(
        &self,
        project_id: &str,
        agent_id: Option<&str>,
        record: &Value,
    ) -> Result<Value> {
        if self
            .task_agent(project_id, agent_id)
            .and_then(|agent| agent.task_id)
            .is_none_or(|task| task.is_empty())
        {
            return rejected("nosh_artifact_read requires an active daemon-bound task session");
        }
        let offset = record
            .get("offset")
            .and_then(Value::as_f64)
            .filter(|offset| *offset > 0.0)
            .map_or(0, |offset| offset.floor() as usize);
        let artifact_id = record.get("artifactId").and_then(Value::as_str);
        let window = |text: &str| {
            let content = slice_units(text, offset, 12_000);
            let remaining = length(text).saturating_sub(offset + length(&content));
            (content, remaining)
        };
        if let Some(evidence_id) = artifact_id.filter(|id| regex(r"^evd_[0-9a-f]+$").is_match(id)) {
            // Review Requests cite Evidence by evd_ id; return the record so the reviewer can
            // follow it to its art_ Artifacts.
            let recorded = self
                .research
                .records(project_id, Some("evidence"))?
                .into_iter()
                .find(|entry| entry["evidenceId"] == evidence_id);
            // A candidate frozen for an open Review is readable too; it is recorded with that
            // Review's disposition.
            let kind = if recorded.is_some() {
                "evidence"
            } else {
                "evidence_candidate"
            };
            let Some(evidence) =
                recorded.or_else(|| self.research.candidate_evidence(project_id, evidence_id))
            else {
                return rejected(format!(
                    "Evidence {evidence_id} is not recorded in this Project"
                ));
            };
            let (content, remaining) = window(&pretty(&evidence));
            return Ok(
                json!({ "accepted": true, "evidenceId": evidence_id, "kind": kind, "mediaType": "application/json", "offset": offset, "content": content, "remainingCharacters": remaining }),
            );
        }
        let Some(artifact_id) = artifact_id.filter(|id| regex(r"^art_[0-9a-f]+$").is_match(id))
        else {
            return rejected("artifactId (art_... or an evd_... Evidence id) is required");
        };
        let repository_root =
            PathBuf::from(string(self.core.project(project_id)?.get("repositoryRoot")));
        let artifact = ArtifactStore::open(&repository_root.join(".nosh").join("artifacts"))?
            .resolve(artifact_id, None)?;
        if artifact["projectId"] != project_id {
            return rejected("Artifact belongs to another Project");
        }
        let text = String::from_utf8_lossy(&std::fs::read(string(artifact.get("storedPath")))?)
            .into_owned();
        let (content, remaining) = window(&text);
        Ok(
            json!({ "accepted": true, "artifactId": artifact["artifactId"], "kind": artifact["kind"], "mediaType": artifact["mediaType"], "contentHash": artifact["contentHash"], "sizeBytes": artifact["sizeBytes"], "offset": offset, "content": content, "remainingCharacters": remaining }),
        )
    }

    /// Snapshots one regular file from a live task's own worktree into the Artifact store and
    /// records it. The ID is derived from task, path, and content, so a corrected or replayed
    /// citation of the same file resolves to the same Artifact.
    fn snapshot_task_artifact(
        &self,
        project_id: &str,
        agent: &Inspection,
        packet: &Value,
        path: &str,
        kind: &str,
    ) -> Result<String> {
        self.assert_project_writable(project_id)?;
        let worktree = self.task_worktree(project_id, &packet["workspace"])?;
        let target = nosh_sys::paths::resolve(&worktree.join(path))?;
        let inside = nosh_sys::paths::relative_inside(&worktree, &target)?
            .map(|inside| nosh_sys::paths::forward_slashes(&inside))
            .filter(|inside| !inside.is_empty() && !inside.split('/').any(|part| part == ".git"));
        let Some(inside) = inside else {
            return rejected(format!(
                "Artifact citation artifact:{path} must name a relative file inside your worktree, outside .git"
            ));
        };
        let Ok(stat) = std::fs::symlink_metadata(&target) else {
            return rejected(format!(
                "Artifact citation artifact:{path} names no file in your worktree; cite a file you wrote or read"
            ));
        };
        if !stat.is_file() || nosh_sys::fs::hard_link_count(&target).is_ok_and(|links| links != 1) {
            return rejected(format!(
                "Artifact citation artifact:{path} must be a regular, unlinked file"
            ));
        }
        if stat.len() > 5_000_000 {
            return rejected(format!("Artifact citation artifact:{path} exceeds 5 MB"));
        }
        let lower = inside.to_lowercase();
        let media_type = if lower.ends_with(".md") {
            "text/markdown"
        } else if lower.ends_with(".json") {
            "application/json"
        } else if lower.ends_with(".bib") {
            "application/x-bibtex"
        } else {
            "text/plain"
        };
        let digest = format!("sha256:{}", hex_digest(&std::fs::read(&target)?));
        let task_id = agent.task_id.clone().unwrap_or_default();
        let artifact_id = format!(
            "art_{}",
            &sha256(&json!({ "taskId": task_id, "path": inside, "digest": digest, "kind": kind }))
                [7..39]
        );
        self.record_task_artifact(project_id, agent, &target, &artifact_id, kind, media_type, json!({ "type": "agent", "agentId": agent.agent_id, "taskId": task_id, "path": inside }))
    }

    /// The exact code change of a worker's completion (starting..ending commit) as a
    /// daemon-produced Artifact, so reviewers judge the change itself.
    fn snapshot_task_diff(
        &self,
        project_id: &str,
        agent: &Inspection,
        worktree: &Path,
        starting: &str,
        ending: &str,
    ) -> Result<String> {
        let (status, stdout, stderr) = git_output(
            worktree,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--find-renames",
                starting,
                ending,
            ],
        );
        if status != Some(0) {
            let message = super::js_trim(&stderr);
            return rejected(if message.is_empty() {
                "git diff failed"
            } else {
                message
            });
        }
        let limit = 1_000_000;
        let content = if length(&stdout) > limit {
            format!(
                "{}\n[diff truncated by NOSH at {limit} characters]\n",
                slice_to(&stdout, limit)
            )
        } else {
            stdout
        };
        let task_id = agent.task_id.clone().unwrap_or_default();
        let artifact_id = format!(
            "art_{}",
            &sha256(
                &json!({ "taskId": task_id, "startingCommit": starting, "endingCommit": ending })
            )[7..39]
        );
        with_temporary_file("nosh-diff-", "change.diff", &content, |file| {
            self.record_task_artifact(project_id, agent, file, &artifact_id, "artifact_diff", "text/x-diff", json!({ "type": "noshd", "taskId": task_id, "startingCommit": starting, "endingCommit": ending }))
        })
    }

    /// A librarian source may cite recorded Evidence (evd_): it becomes a snapshot Artifact of
    /// that record, at nosh://evidence/<id>.
    fn with_evidence_sources(
        &self,
        project_id: &str,
        agent: &Inspection,
        record: Value,
    ) -> Result<Value> {
        let Some(sources) = record
            .get("sources")
            .and_then(Value::as_array)
            .filter(|_| has_schema(&record, "librarian-completion"))
            .cloned()
        else {
            return Ok(record);
        };
        let evidence_pattern = regex(r"^evd_[0-9a-f]+$");
        let task_id = agent.task_id.clone().unwrap_or_default();
        let mut replaced = Vec::with_capacity(sources.len());
        for source in sources {
            let Some(evidence_id) = source
                .get("artifactId")
                .and_then(Value::as_str)
                .filter(|id| source.is_object() && evidence_pattern.is_match(id))
                .map(str::to_owned)
            else {
                replaced.push(source);
                continue;
            };
            let Some(evidence) = self
                .research
                .records(project_id, Some("evidence"))?
                .into_iter()
                .find(|entry| entry["evidenceId"] == evidence_id.as_str())
            else {
                return rejected(format!(
                    "Source cites {evidence_id}, which is not recorded Evidence in this Project"
                ));
            };
            let artifact_id = format!(
                "art_{}",
                &sha256(&json!({ "evidenceId": evidence_id, "taskId": task_id }))[7..39]
            );
            let recorded = with_temporary_file(
                "nosh-evidence-",
                &format!("{evidence_id}.json"),
                &format!("{}\n", pretty(&evidence)),
                |file| {
                    self.record_task_artifact(
                        project_id,
                        agent,
                        file,
                        &artifact_id,
                        "artifact_source",
                        "application/json",
                        json!({ "type": "noshd", "taskId": task_id, "evidenceId": evidence_id }),
                    )
                },
            )?;
            let canonical = source
                .get("canonicalUrl")
                .and_then(Value::as_str)
                .filter(|url| url.starts_with("http"))
                .map_or_else(|| format!("nosh://evidence/{evidence_id}"), str::to_owned);
            let mut source = source;
            source["artifactId"] = json!(recorded);
            source["canonicalUrl"] = json!(canonical);
            replaced.push(source);
        }
        let mut record = record;
        record["sources"] = Value::Array(replaced);
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_task_artifact(
        &self,
        project_id: &str,
        agent: &Inspection,
        source_path: &Path,
        artifact_id: &str,
        kind: &str,
        media_type: &str,
        producer: Value,
    ) -> Result<String> {
        let repository_root =
            PathBuf::from(string(self.core.project(project_id)?.get("repositoryRoot")));
        let input: ArtifactInput = Map::from_iter(BTreeMap::from([
            ("artifactId".to_owned(), json!(artifact_id)),
            ("projectId".to_owned(), json!(project_id)),
            ("kind".to_owned(), json!(kind)),
            ("mediaType".to_owned(), json!(media_type)),
            (
                "sourcePath".to_owned(),
                json!(source_path.to_string_lossy()),
            ),
            ("retentionClass".to_owned(), json!("accepted_evidence")),
        ]));
        let artifact =
            ArtifactStore::open(&repository_root.join(".nosh").join("artifacts"))?.add(&input)?;
        let scope = json!({ "missionId": agent.mission_id, "directionId": agent.direction_id, "autoresearchId": agent.autoresearch_id, "experimentId": agent.experiment_id });
        let record = json!({ "$schema": schema_uri("artifact"), "schemaVersion": 1, "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": media_type, "contentHash": artifact["contentHash"], "sizeBytes": artifact["sizeBytes"], "version": artifact["version"], "producer": producer, "scope": scope, "git": null, "evaluationContractHash": null, "retentionClass": "accepted_evidence", "remotePreviewPolicy": "encrypted_allowed", "redactionStatus": "checked", "createdAt": artifact["createdAt"] });
        let event_scope = json!({ "projectId": project_id, "missionId": agent.mission_id, "directionId": agent.direction_id, "autoresearchId": agent.autoresearch_id, "experimentId": agent.experiment_id, "runId": null, "jobId": null, "agentId": agent.agent_id });
        self.research.submit_daemon_record(
            project_id,
            &event_scope,
            &format!("artifact:{artifact_id}"),
            &record,
            &format!("task-artifact:{artifact_id}"),
        )?;
        Ok(artifact_id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_failures_keeps_each_not_ok_block() {
        let output = "TAP version 13\n# Subtest: a\nok 1 - a\n# Subtest: b\nnot ok 2 - b\n  ---\n  error: 'boom'\n  ...\n  after the block\nok 3 - c\nnot ok 4 - d\n  ---\n# Subtest: e\n";
        assert_eq!(
            tap_failures(output, 4_000),
            "not ok 2 - b\n  ---\n  error: 'boom'\n  ...\nnot ok 4 - d\n  ---"
        );
        assert_eq!(tap_failures(output, 6), "not ok");
    }

    #[test]
    fn latest_run_per_command_keeps_the_last_run_in_last_position() {
        let runs = [
            json!({ "commandId": "a", "n": 1 }),
            json!({ "commandId": "b", "n": 2 }),
            json!({ "commandId": "a", "n": 3 }),
        ];
        assert_eq!(
            latest_run_per_command(&runs),
            vec![runs[1].clone(), runs[2].clone()]
        );
    }

    #[test]
    fn citations_take_the_most_specific_kind_and_are_replaced() {
        let record = json!({ "$schema": schema_uri("general-worker-completion"), "artifactIds": ["artifact:out/report.md"], "reportArtifactId": "artifact:out/report.md", "note": "artifact:has space" });
        let mut kinds = Vec::new();
        collect_artifact_citations(&record, &string(record.get("$schema")), "", &mut kinds);
        assert_eq!(
            kinds,
            vec![("out/report.md".to_owned(), "artifact_report".to_owned())]
        );
        let replaced = replace_artifact_citations(
            &record,
            &HashMap::from([("out/report.md".to_owned(), "art_1".to_owned())]),
        );
        assert_eq!(replaced["reportArtifactId"], "art_1");
        assert_eq!(replaced["artifactIds"], json!(["art_1"]));
        assert_eq!(replaced["note"], "artifact:has space");
    }

    #[test]
    fn source_urls_are_derived_from_citations_and_dois() {
        let record = json!({ "$schema": schema_uri("librarian-completion"), "sources": [
            { "artifactId": "artifact:./notes/a.md" },
            { "persistentId": "https://dx.doi.org/10.1000/xyz" },
            { "persistentId": "doi:10.5/q", "canonicalUrl": "https://kept" },
            { "artifactId": "art_1" }
        ] });
        let sources = with_source_urls(record)["sources"].clone();
        assert_eq!(sources[0]["canonicalUrl"], "file:///notes/a.md");
        assert_eq!(sources[1]["canonicalUrl"], "https://doi.org/10.1000/xyz");
        assert_eq!(sources[2]["canonicalUrl"], "https://kept");
        assert!(sources[3].get("canonicalUrl").is_none());
    }

    #[test]
    fn a_progress_note_becomes_a_schema_valid_progress_update() {
        let task_id = format!("tsk_{}", "a".repeat(32));
        let agent_id = format!("agt_{}", "b".repeat(32));
        let progress = progress_update(
            &task_id,
            &json!(1),
            &agent_id,
            "tests pass -> write the report -> ship",
            2,
        );
        assert_eq!(
            progress["progressId"],
            format!("progress_{}_2", "a".repeat(32))
        );
        assert_eq!(
            (
                &progress["taskId"],
                &progress["agentId"],
                &progress["attempt"]
            ),
            (&json!(task_id), &json!(agent_id), &json!(1))
        );
        assert_eq!(
            (&progress["summary"], &progress["nextOperation"]),
            (&json!("tests pass"), &json!("write the report"))
        );
        assert!(nosh_wire::validate_record(&schema_uri("progress-update"), &progress).is_ok());
        assert_eq!(
            progress_update(&task_id, &json!(1), &agent_id, "no arrow", 1)["nextOperation"],
            ""
        );
    }

    #[test]
    fn evaluation_variants_ignore_flags() {
        let strings = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>()
        };
        assert!(is_evaluation_variant(
            &strings(&["node", "eval.js", "--seed=1"]),
            &strings(&["node", "--quiet", "eval.js"])
        ));
        assert!(!is_evaluation_variant(
            &strings(&["node", "test.js"]),
            &strings(&["node", "eval.js"])
        ));
    }
}
