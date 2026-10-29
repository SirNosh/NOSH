//! Port of `apps/noshd/src/autoresearch-supervisor.ts`: runs each running Autoresearch as a greedy
//! climb. One dedicated autoresearcher proposes a bounded variant on the current best and
//! implements it in its own worktree; the daemon freezes the variant, evaluates it as a supervised
//! Job, records Evidence and the result, has an independent reviewer judge only promotion
//! candidates, promotes each round's winner, and closes the run through an independent closure
//! Review.
//!
//! Divergences: prompt-embedded JSON (metrics, contracts, packets) has sorted keys, and the
//! environment manifest describes the Rust host (`{ runtime, platform, arch }`) where TypeScript
//! records its Node version.

use super::{
    Busy, Dependencies, from_packet, git, number, repository_root, strings, unique, version,
};
use crate::daemon::{error_text, event_draft};
use crate::research_spawner::{JobRunner, frozen_evaluation_files, is_evaluation_variant};
use crate::runtime::parse_ms;
use crate::task_postflight::{completion_run_issues, validate_git_completion, write_up_only};
use crate::task_templates::{
    completion_instructions, general_worker_completion_template, reviewer_instructions, stringify,
    terminal_envelope,
};
use crate::{Error, Result, rejected};
use nosh_agent::policy::{Role, SessionOptions, TaskPermissions, TaskWorkspace};
use nosh_research::artifacts::ArtifactStore;
use nosh_research::graph::{Experiment, ExperimentReview, ExperimentTree};
use nosh_research::project::read_project_contract;
use nosh_research::research_control::{
    Host, declared_task_commands, task_command_instructions, task_command_runs,
};
use nosh_sys::git::{FreezeInput, GitWorkspaceManager};
use nosh_sys::jobs::{JobRecord, JobSpec, JobState, Runner, load_native_evaluation};
use nosh_sys::paths::{realpath, relative_inside, resolve};
use nosh_wire::js::{
    length, max, min, number_string, slice_to, sort_by_number, strict_equal, string, truthy,
};
use nosh_wire::{
    IdPrefix, canonical_json, create_id, episode_type_for_role, normalize_numbers, now_utc,
    schema_document_path, schema_uri, sha256, validate_record,
};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Consecutive proposal failures (no proposal accepted in between) before a run blocks for
/// attention. A proposer may only return one experiment-proposal; it has no file, shell, or Job
/// tools.
const MAXIMUM_PROPOSAL_FAILURES: usize = 3;

/// One task session's identity and authority.
struct Session<'a> {
    agent_id: &'a str,
    task_id: &'a str,
    experiment_id: Option<&'a str>,
    role: Role,
    cwd: PathBuf,
    permissions: Option<TaskPermissions>,
    workspace: Option<TaskWorkspace>,
}

/// What an independent Review must judge.
struct ReviewSpec<'a> {
    reviewer_agent_id: &'a str,
    producers: Vec<String>,
    review_type: &'a str,
    target_type: &'a str,
    target_id: String,
    target_version: i64,
    criterion_id: String,
    statement: String,
    artifacts: Vec<String>,
    evidence: Vec<String>,
}

/// The validated evaluation contract.
struct Contract {
    metric: String,
    maximize: bool,
    minimum_effect: f64,
    baseline: Value,
    guardrails: Vec<Value>,
    round_widths: Vec<usize>,
    target: Option<f64>,
    no_progress_rounds: Option<f64>,
    runner: Runner,
    distribution: Option<String>,
    command: Vec<String>,
    result_path: String,
    timeout_seconds: f64,
    checkpoint_command: Option<Vec<String>>,
    uses_gpu: bool,
}

/// The proposal brief's inputs.
struct Brief<'a> {
    project: &'a Value,
    execution: &'a Value,
    records: &'a [Value],
    experiment_id: &'a str,
    parent_experiment_id: &'a str,
    parent_commit: &'a str,
    round: i64,
    agent_id: &'a str,
    crashes: &'a [(String, String)],
    stop_after: f64,
    schema_dir: &'a Path,
}

pub struct AutoresearchSupervisor<H: Host> {
    deps: Dependencies<H>,
    busy: RefCell<HashSet<String>>,
    timer: RefCell<Option<JoinHandle<()>>>,
}

impl<H: Host + 'static> AutoresearchSupervisor<H> {
    pub fn new(deps: Dependencies<H>) -> Rc<Self> {
        Rc::new(AutoresearchSupervisor {
            deps,
            busy: RefCell::default(),
            timer: RefCell::default(),
        })
    }

    /// Ticks once a second until [`stop`](Self::stop), without waiting for the previous tick (the
    /// busy set keeps cycles apart). Each cycle records its own failures as events; only failures
    /// after shutdown (closed stores) reach here, and they are dropped.
    pub fn start(self: &Rc<Self>) {
        let supervisor = Rc::downgrade(self);
        let timer = tokio::task::spawn_local(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(supervisor) = supervisor.upgrade() else {
                    break;
                };
                tokio::task::spawn_local(async move {
                    let _ = supervisor.tick().await;
                });
            }
        });
        if let Some(previous) = self.timer.replace(Some(timer)) {
            previous.abort();
        }
    }

    pub fn stop(&self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }

    /// Runs one cycle of every running Autoresearch, in turn.
    pub async fn tick(&self) -> Result<()> {
        for project in (self.deps.projects)()? {
            let project_id = string(project.get("projectId"));
            for execution in self.deps.research.autoresearch(&project_id)? {
                if execution["state"] == "running" {
                    self.cycle(&project, &string(execution.get("entityId")))
                        .await?;
                }
            }
        }
        Ok(())
    }

    fn emit(&self, draft: &Value) -> Result<()> {
        (self.deps.emit)(draft)
    }

    fn jobs(&self) -> Result<&Rc<dyn JobRunner>> {
        self.deps
            .job_runner
            .as_ref()
            .ok_or_else(|| Error::Rejected("Autoresearch requires a Job runner".into()))
    }

    async fn cycle(&self, project: &Value, autoresearch_id: &str) -> Result<()> {
        if self.busy.borrow().contains(autoresearch_id)
            || self
                .deps
                .agents
                .inspect()
                .iter()
                .any(|agent| agent.autoresearch_id.as_deref() == Some(autoresearch_id))
        {
            return Ok(());
        }
        let project_id = string(project.get("projectId"));
        let current = self
            .deps
            .research
            .autoresearch_execution(&project_id, autoresearch_id)?;
        let value = &current["value"];
        if let Some(runtime) = &self.deps.runtime
            && runtime.has_active_fork(
                &project_id,
                value["missionId"].as_str(),
                value["directionId"].as_str(),
            )?
        {
            return Ok(());
        }
        if !self.parent_running(&project_id, value)? {
            return Ok(());
        }
        let _busy = Busy::hold(&self.busy, autoresearch_id);
        match self.advance(project, autoresearch_id).await {
            Ok(()) => Ok(()),
            Err(error) => self.cycle_failed(&project_id, autoresearch_id, &error),
        }
    }

    fn cycle_failed(&self, project_id: &str, autoresearch_id: &str, error: &Error) -> Result<()> {
        let research = &self.deps.research;
        let execution = research.autoresearch_execution(project_id, autoresearch_id)?;
        self.emit(&event(
            &execution["value"],
            "autoresearch.supervisor_error",
            json!({ "message": error_text(error, "Autoresearch supervisor failed") }),
        ))?;
        if execution["state"] == "running" {
            research.transition_autoresearch(
                project_id,
                autoresearch_id,
                version(&execution),
                "blocked",
                &format!("autoresearch-blocked:{}", create_id(IdPrefix::Cmd)),
            )?;
        }
        Ok(())
    }

    async fn advance(&self, project: &Value, autoresearch_id: &str) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let research = &self.deps.research;
        let mut execution = research.autoresearch_execution(&project_id, autoresearch_id)?;
        let contract = parse_contract(&execution["value"]["evaluationContract"])?;
        let records = self.records(&project_id, autoresearch_id)?;
        if records
            .iter()
            .any(|record| is_schema(record, "autoresearch-completion-packet"))
        {
            research.transition_autoresearch(
                &project_id,
                autoresearch_id,
                version(&execution),
                "completed",
                &format!("autoresearch-completed:{autoresearch_id}"),
            )?;
            return Ok(());
        }
        let round = records.iter().rev().find(|record| {
            is_schema(record, "autoresearch-round")
                && validate_record(&schema_uri("autoresearch-round"), record).is_ok()
        });
        if let Some(round) = round
            && let Some(number) = integer(&round["round"])
        {
            let promoted: Vec<String> = round["promotedExperimentIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let stop_requested = round["focusCheck"]["action"] == "stop" && promoted.is_empty();
            if number > current_round(&execution) {
                let frontier = if promoted.is_empty() {
                    frontier(&execution)
                } else {
                    promoted
                };
                execution = research.advance_autoresearch(
                    &project_id,
                    autoresearch_id,
                    version(&execution),
                    number as f64,
                    &frontier,
                    &format!("autoresearch-round-projection:{autoresearch_id}:{number}"),
                )?;
            }
            if stop_requested && number == current_round(&execution) {
                let reason = if frontier(&execution).is_empty() {
                    "all_hypotheses_rejected"
                } else {
                    "evidence_saturation"
                };
                let records = self.records(&project_id, autoresearch_id)?;
                return self
                    .close(project, &execution, &records, reason, None)
                    .await;
            }
        }
        let pending_run = records
            .iter()
            .filter(|record| is_schema(record, "run-manifest"))
            .find(|run| {
                !records.iter().any(|record| {
                    is_schema(record, "job-result")
                        && strict_equal(record.get("runId"), run.get("runId"))
                })
            });
        if let Some(run) = pending_run {
            let value = &execution["value"];
            let jobs = self.jobs()?;
            let job = match jobs.get(&string(run.get("jobId"))) {
                Ok(job) => job,
                Err(_) => {
                    let environment = ArtifactStore::open(&artifacts_root(project))?
                        .resolve(&string(run.get("environmentManifestArtifactId")), None)?;
                    jobs.start(&JobSpec {
                        job_id: string(run.get("jobId")),
                        project_id: project_id.clone(),
                        mission_id: optional(&value["missionId"]),
                        direction_id: optional(&value["directionId"]),
                        autoresearch_id: Some(autoresearch_id.to_owned()),
                        run_id: string(run.get("runId")),
                        experiment_id: Some(string(run.get("experimentId"))),
                        commit_sha: string(run.get("evaluatedCommit")),
                        working_directory: string(run.get("workingDirectory")),
                        runner: contract.runner,
                        distribution: contract.distribution.clone(),
                        command: strings(&run["resolvedCommand"]),
                        checkpoint_command: contract.checkpoint_command.clone(),
                        environment_lock_hash: string(environment.get("contentHash")),
                        evaluation_contract_hash: string(value.get("evaluationContractHash")),
                        timeout_seconds: contract.timeout_seconds,
                        uses_gpu: contract.uses_gpu,
                    })?
                }
            };
            if job.state.is_terminal() {
                self.finish_run(project, &execution, &contract, run, &job)
                    .await?;
            }
            return Ok(());
        }
        // A round whose results are all in but whose round record is missing was interrupted
        // (e.g. its promotion failed): finish it before proposing, so the next round starts from
        // its frontier.
        let finished = unique_results(&records);
        let mut total = 0;
        for (index, width) in round_widths(&execution["value"], &contract)
            .iter()
            .enumerate()
        {
            total += width;
            if total > finished.len() {
                break;
            }
            let round = index as i64 + 1;
            if !records
                .iter()
                .any(|record| is_schema(record, "autoresearch-round") && record["round"] == round)
            {
                return self
                    .finish_round(project, &execution, &contract, round, &finished)
                    .await;
            }
        }
        let value = &execution["value"];
        let usage = research.autoresearch_budget_use(&project_id, autoresearch_id)?;
        let used = |key: &str| number(&usage[key]);
        let limit = |key: &str| number(&value[key]);
        let exhausted: Vec<&str> = [
            (used("experiments") >= limit("maximumExperiments")).then_some("experiments"),
            (used("rounds") >= limit("maximumRounds")).then_some("rounds"),
            (used("modelTokens") >= limit("maximumModelTokens")).then_some("model_tokens"),
            (used("wallClockSeconds") >= limit("maximumWallClockSeconds")).then_some("wall_clock"),
            (used("gpuSeconds") > limit("maximumGpuSeconds")
                || contract.uses_gpu && used("gpuSeconds") >= limit("maximumGpuSeconds"))
            .then_some("gpu_seconds"),
            (used("diskBytes") >= limit("maximumDiskBytes")).then_some("disk_bytes"),
            (used("implementationFailures") >= limit("maximumExperiments"))
                .then_some("implementation_failures"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !exhausted.is_empty() {
            self.emit(&event(
                value,
                "autoresearch.budget_exhausted",
                json!({ "exhausted": exhausted, "use": usage }),
            ))?;
            if finished.is_empty() {
                research.transition_autoresearch(
                    &project_id,
                    autoresearch_id,
                    version(&execution),
                    "blocked",
                    &format!(
                        "autoresearch-budget-blocked:{autoresearch_id}:{}",
                        version(&execution)
                    ),
                )?;
            } else {
                self.close(
                    project,
                    &execution,
                    &records,
                    "budget_exhausted",
                    Some(&usage),
                )
                .await?;
            }
            return Ok(());
        }
        self.start_experiment(project, &execution, &contract, &records)
            .await
    }

    async fn start_experiment(
        &self,
        project: &Value,
        execution: &Value,
        contract: &Contract,
        records: &[Value],
    ) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let root = repository_root(project);
        let value = &execution["value"];
        let autoresearch_id = string(execution.get("entityId"));
        let contract_hash = string(value.get("evaluationContractHash"));
        let root_experiment = string(value.get("rootExperimentId"));
        let frontier = frontier(execution);
        let results = unique_results(records);
        // Only identity-valid proposals count; a mis-copied one stays on record but is never
        // implemented.
        let proposals: Vec<&Value> = records
            .iter()
            .filter(|record| {
                is_schema(record, "experiment-proposal")
                    && record["autoresearchId"] == autoresearch_id.as_str()
                    && record["evaluationContractHash"] == contract_hash.as_str()
            })
            .collect();
        let failures: Vec<Value> = research
            .events_after(&project_id, 0)?
            .into_iter()
            .filter(|entry| {
                entry["type"] == "autoresearch.experiment_failed"
                    && entry["scope"]["autoresearchId"] == autoresearch_id.as_str()
            })
            .collect();
        let retired = unique(
            failures
                .iter()
                .map(|entry| string(entry["payload"].get("experimentId"))),
        );
        let pending = proposals
            .iter()
            .find(|proposal| {
                !retired.contains(&string(proposal.get("experimentId")))
                    && !results
                        .iter()
                        .any(|result| result["experimentId"] == proposal["experimentId"])
                    && !records.iter().any(|record| {
                        is_schema(record, "run-manifest")
                            && record["experimentId"] == proposal["experimentId"]
                    })
            })
            .map(|proposal| (*proposal).clone());
        let ordinal = results.len() + 1;
        let widths = round_widths(value, contract);
        let round = match &pending {
            Some(proposal) => integer(&proposal["round"]).unwrap_or_default(),
            None => round_at(ordinal, &widths) as i64,
        };
        let parent_experiment_id = match &pending {
            Some(proposal) => string(proposal.get("parentExperimentId")),
            None if round == 1 => root_experiment.clone(),
            None => frontier
                .first()
                .cloned()
                .unwrap_or_else(|| root_experiment.clone()),
        };
        if let Some(proposal) = &pending {
            let allowed = if proposal["round"] == 1 || frontier.is_empty() {
                vec![root_experiment.clone()]
            } else {
                frontier.clone()
            };
            if !allowed.contains(&string(proposal.get("parentExperimentId"))) {
                return self.emit(&event(
                    value,
                    "autoresearch.experiment_failed",
                    json!({ "experimentId": proposal["experimentId"], "phase": "proposal", "issues": [format!("stale proposal: round {} parent {} is not on the current frontier", string(proposal.get("round")), string(proposal.get("parentExperimentId")))] }),
                ));
            }
        }
        if parent_experiment_id.is_empty() {
            return rejected("Autoresearch has no accepted parent frontier");
        }
        let parent_commit = if parent_experiment_id == root_experiment {
            Some(self.root_commit(project, value)?)
        } else {
            results
                .iter()
                .find(|result| result["experimentId"] == parent_experiment_id.as_str())
                .and_then(|result| result["evaluatedCommit"].as_str())
                .filter(|commit| !commit.is_empty())
                .map(str::to_owned)
        };
        let Some(parent_commit) = parent_commit else {
            return rejected("Experiment parent commit is unresolved");
        };
        let proposal = match pending {
            Some(proposal) => proposal,
            None => {
                let experiment_id = create_id(IdPrefix::Exp);
                let agent_id = autoresearcher_id(&autoresearch_id);
                let task_id = create_id(IdPrefix::Tsk);
                let crashes: Vec<(String, String)> = retired
                    .iter()
                    .map(|id| {
                        let issues: Vec<String> = failures
                            .iter()
                            .filter(|entry| entry["payload"]["experimentId"] == id.as_str())
                            .map(|entry| strings(&entry["payload"]["issues"]).join("; "))
                            .collect();
                        (id.clone(), issues.join("; "))
                    })
                    .collect();
                let brief = proposal_brief(&Brief {
                    project,
                    execution,
                    records,
                    experiment_id: &experiment_id,
                    parent_experiment_id: &parent_experiment_id,
                    parent_commit: &parent_commit,
                    round,
                    agent_id: &agent_id,
                    crashes: &crashes,
                    stop_after: contract.no_progress_rounds.unwrap_or(1.0),
                    schema_dir: &self.deps.schema_dir,
                });
                let session = Session {
                    agent_id: &agent_id,
                    task_id: &task_id,
                    experiment_id: Some(&experiment_id),
                    role: Role::GeneralWorker,
                    cwd: root.clone(),
                    permissions: Some(proposal_permissions()),
                    workspace: None,
                };
                let mut failure = String::new();
                if let Err(error) = self.session(project, value, session, &brief).await {
                    let message = error.to_string();
                    if !message.starts_with("Terminal output rejected") {
                        return Err(error);
                    }
                    failure = message;
                }
                let proposal = self
                    .task_record(&project_id, &task_id, "experiment-proposal")?
                    .filter(|proposal| {
                        validate_record(&schema_uri("experiment-proposal"), proposal).is_ok()
                            && proposal["autoresearchId"] == autoresearch_id.as_str()
                            && proposal["experimentId"] == experiment_id.as_str()
                            && proposal["parentExperimentId"] == parent_experiment_id.as_str()
                            && proposal["round"] == round
                            && proposal["proposedByAgentId"] == agent_id.as_str()
                            && proposal["evaluationContractHash"] == contract_hash.as_str()
                    });
                let Some(proposal) = proposal else {
                    // A malformed proposal is one model slip, not a reason to stop the search:
                    // record it and propose afresh next tick, until MAXIMUM_PROPOSAL_FAILURES
                    // consecutive failures block for attention.
                    if failure.is_empty() {
                        failure = "Worker did not submit the bounded experiment proposal".into();
                    }
                    self.emit(&event(
                        value,
                        "autoresearch.proposal_failed",
                        json!({ "experimentId": experiment_id, "taskId": task_id, "message": slice_to(&failure, 2_000) }),
                    ))?;
                    if consecutive_proposal_failures(
                        &research.events_after(&project_id, 0)?,
                        &autoresearch_id,
                    ) >= MAXIMUM_PROPOSAL_FAILURES
                    {
                        return rejected(format!(
                            "No valid experiment proposal after {MAXIMUM_PROPOSAL_FAILURES} consecutive attempts"
                        ));
                    }
                    return Ok(());
                };
                proposal
            }
        };
        let experiment_id = string(proposal.get("experimentId"));
        let proposal_parent = string(proposal.get("parentExperimentId"));
        // Depth follows the parent, not the round: a round with no accepted frontier branches
        // again from the root.
        let mut tree = self.rehydrate_tree(value, records, &parent_commit)?;
        let depth = tree.get(&proposal_parent)?.depth + 1.0;
        tree.add(Experiment {
            experiment_id: experiment_id.clone(),
            parent_experiment_id: Some(proposal_parent.clone()),
            idea_fingerprint: string(proposal["novelty"].get("ideaFingerprint")),
            attempt_fingerprint: sha256(
                &json!({ "experimentId": experiment_id, "proposalId": proposal["proposalId"] }),
            ),
            evaluation_contract_hash: contract_hash.clone(),
            branch: branch_name(&autoresearch_id, &experiment_id),
            evaluated_commit: None,
            depth,
            state: "proposed".into(),
            review_id: None,
            reviewer_agent_id: None,
            guardrails_pass: None,
            extra: Map::new(),
        })?;
        let manager = manager_for(project)?;
        let branch = branch_name(&autoresearch_id, &experiment_id);
        let worktree_id = format!("wt_{}", from(&experiment_id, 4));
        let create = research.begin_external_operation(
            &project_id,
            "git.worktree.create",
            &format!("git-worktree:{experiment_id}"),
            &json!({ "branch": branch, "baseCommit": parent_commit, "worktreeId": worktree_id }),
        )?;
        let worktree = manager.create(&branch, &parent_commit, &worktree_id)?;
        research.complete_external_operation(
            &project_id,
            &string(create.get("intentId")),
            &json!({ "branch": branch, "worktree": worktree.to_string_lossy() }),
        )?;
        let prior_packet = records
            .iter()
            .find(|record| {
                is_schema(record, "task-packet")
                    && record["scope"]["experimentId"] == experiment_id.as_str()
            })
            .cloned();
        let task_id = prior_packet.as_ref().map_or_else(
            || create_id(IdPrefix::Tsk),
            |packet| string(packet.get("taskId")),
        );
        let worker_id = prior_packet.as_ref().map_or_else(
            || string(proposal.get("proposedByAgentId")),
            |packet| string(packet.get("assignedAgentId")),
        );
        let packet = match prior_packet {
            Some(packet) => packet,
            None => {
                let packet = self.task_packet(
                    project,
                    execution,
                    &proposal,
                    &task_id,
                    &worker_id,
                    &branch,
                    &parent_commit,
                )?;
                self.submit(
                    &project_id,
                    value,
                    &format!("experiment:{experiment_id}:implementation"),
                    &packet,
                    &format!("experiment-task-packet:{experiment_id}"),
                    Some(&worker_id),
                    Some(&experiment_id),
                    None,
                    None,
                )?;
                packet
            }
        };
        let mut completion =
            research.terminal_record(&project_id, &task_id, Some("general-worker-completion"))?;
        if completion.is_none() {
            let prompt = format!(
                "Task Packet: {}. Implement exactly one bounded variant for experiment {experiment_id}, variantOrdinal={ordinal}. {}",
                stringify(&packet),
                completion_instructions(
                    &general_worker_completion_template(&packet, None),
                    "general_worker",
                    &self.deps.schema_dir
                )
            );
            let session = Session {
                agent_id: &worker_id,
                task_id: &task_id,
                experiment_id: Some(&experiment_id),
                role: Role::GeneralWorker,
                cwd: worktree.clone(),
                permissions: Some(from_packet(&packet["permissions"])?),
                workspace: Some(from_packet(&packet["workspace"])?),
            };
            self.session(project, value, session, &prompt).await?;
            completion = research.terminal_record(
                &project_id,
                &task_id,
                Some("general-worker-completion"),
            )?;
        }
        let issues = match &completion {
            None => vec!["no completion record".to_owned()],
            Some(completion) => {
                let write_up = write_up_only(&root);
                let mut issues = implementation_issues(&worktree, &packet, completion);
                issues.extend(completion_run_issues(
                    &task_command_runs(research, &project_id)?,
                    &task_id,
                    completion,
                    &[],
                    Some(&write_up),
                ));
                issues
            }
        };
        // A failed implementation is a recorded negative outcome, not a reason to block the
        // search: the experiment is retired (it still counts against the experiment budget) and
        // the next tick proposes anew.
        if !issues.is_empty() {
            return self.emit(&event(
                value,
                "autoresearch.experiment_failed",
                json!({ "experimentId": experiment_id, "taskId": task_id, "phase": "implementation", "issues": issues }),
            ));
        }
        let environment = json!({ "runtime": "nosh-rust", "platform": std::env::consts::OS, "arch": std::env::consts::ARCH });
        let freeze = research.begin_external_operation(
            &project_id,
            "git.experiment.freeze",
            &format!("git-freeze:{experiment_id}"),
            &json!({ "experimentId": experiment_id, "branch": branch, "parentCommit": parent_commit, "evaluationContractHash": contract_hash, "command": contract.command }),
        )?;
        let frozen = manager.freeze(&FreezeInput {
            experiment_id: &experiment_id,
            parent_experiment_id: Some(&proposal_parent),
            branch: &branch,
            worktree: &worktree,
            parent_commit: &parent_commit,
            evaluation_contract_hash: &contract_hash,
            environment_manifest: &environment,
            run_command: &contract.command,
        })?;
        research.complete_external_operation(
            &project_id,
            &string(freeze.get("intentId")),
            &serde_json::to_value(&frozen).map_err(|error| Error::Rejected(error.to_string()))?,
        )?;
        let state_root = root
            .join(".nosh")
            .join("autoresearch")
            .join(&autoresearch_id)
            .join(&experiment_id);
        std::fs::create_dir_all(&state_root)?;
        let configuration_path = self.write(
            &project_id,
            &format!("configuration:{experiment_id}"),
            &state_root.join("configuration.json"),
            &value["evaluationContract"],
        )?;
        let environment_path = self.write(
            &project_id,
            &format!("environment:{experiment_id}"),
            &state_root.join("environment.json"),
            &environment,
        )?;
        let configuration = self.artifact(
            project,
            value,
            Some(&experiment_id),
            "artifact_configuration",
            "application/json",
            &configuration_path,
            "accepted_evidence",
        )?;
        let environment_artifact = self.artifact(
            project,
            value,
            Some(&experiment_id),
            "artifact_environment",
            "application/json",
            &environment_path,
            "accepted_evidence",
        )?;
        let run_id = create_id(IdPrefix::Run);
        let job_id = create_id(IdPrefix::Job);
        let run = json!({
            "$schema": schema_uri("run-manifest"), "schemaVersion": 1, "runId": run_id, "experimentId": experiment_id, "evaluationContractHash": contract_hash, "evaluatedCommit": frozen.evaluated_commit, "seed": 0,
            "resolvedCommand": contract.command, "workingDirectory": worktree.to_string_lossy(), "inputArtifactIds": [], "configurationArtifactId": configuration["artifactId"], "environmentManifestArtifactId": environment_artifact["artifactId"],
            "hardware": environment, "jobId": job_id, "state": "queued", "metricArtifactId": null, "requiredOutputArtifactIds": [], "startedAt": now_utc(), "finishedAt": null,
        });
        self.submit(
            &project_id,
            value,
            &format!("run:{run_id}"),
            &run,
            &format!("run-manifest:{run_id}"),
            None,
            Some(&experiment_id),
            Some(&run_id),
            Some(&job_id),
        )?;
        self.jobs()?.start(&JobSpec {
            job_id,
            project_id,
            mission_id: optional(&value["missionId"]),
            direction_id: optional(&value["directionId"]),
            autoresearch_id: Some(autoresearch_id),
            run_id,
            experiment_id: Some(experiment_id),
            commit_sha: frozen.evaluated_commit,
            working_directory: worktree.to_string_lossy().into_owned(),
            runner: contract.runner,
            distribution: contract.distribution.clone(),
            command: contract.command.clone(),
            checkpoint_command: contract.checkpoint_command.clone(),
            environment_lock_hash: string(environment_artifact.get("contentHash")),
            evaluation_contract_hash: contract_hash,
            timeout_seconds: contract.timeout_seconds,
            uses_gpu: contract.uses_gpu,
        })?;
        Ok(())
    }

    async fn finish_run(
        &self,
        project: &Value,
        execution: &Value,
        contract: &Contract,
        run: &Value,
        job: &JobRecord,
    ) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let value = &execution["value"];
        let autoresearch_id = string(execution.get("entityId"));
        let contract_hash = string(value.get("evaluationContractHash"));
        let records = self.records(&project_id, &autoresearch_id)?;
        let experiment_id = string(run.get("experimentId"));
        let run_id = string(run.get("runId"));
        let job_id = job.spec.job_id.clone();
        let Some(proposal) = records
            .iter()
            .find(|record| {
                is_schema(record, "experiment-proposal")
                    && record["experimentId"] == run["experimentId"]
            })
            .cloned()
        else {
            return rejected("Run has no experiment proposal");
        };
        let proposed_by = string(proposal.get("proposedByAgentId"));
        let mut artifacts: Vec<Value> = Vec::new();
        for (kind, path) in [
            ("artifact_stdout.log", &job.stdout_path),
            ("artifact_stderr.log", &job.stderr_path),
        ] {
            let path = Path::new(path);
            if !path.as_os_str().is_empty() && path.exists() {
                artifacts.push(self.artifact(
                    project,
                    value,
                    Some(&experiment_id),
                    kind,
                    "text/plain",
                    path,
                    "negative_evidence",
                )?);
            }
        }
        let worktree = realpath(Path::new(&string(run.get("workingDirectory"))))?;
        let result_path = resolve(&worktree.join(&contract.result_path))?;
        if relative_inside(&worktree, &result_path)?.is_none() {
            return rejected("Evaluation result path escapes the worktree");
        }
        let job_state = match job.state {
            JobState::Completed => "succeeded",
            JobState::Cancelled => "cancelled",
            JobState::Lost => "lost",
            _ => "failed",
        };
        let mut metrics =
            json!({ "error": job.failure_reason.clone().unwrap_or_else(|| job_state.to_owned()) });
        let mut metric: Option<Value> = None;
        let mut metric_failure = Some(if job_state == "succeeded" {
            "evaluation result file is absent".to_owned()
        } else {
            format!("job {job_state}")
        });
        if job_state == "succeeded" && result_path.exists() {
            metric_failure = match load_native_evaluation(&result_path) {
                Ok(loaded) if !loaded.is_object() => {
                    Some("evaluation result is not a JSON object".to_owned())
                }
                Ok(loaded) => {
                    metrics = loaded;
                    primary_metric(&metrics, &contract.metric).1
                }
                Err(error) => Some(error.to_string()),
            };
            if let Some(failure) = &metric_failure {
                metrics = json!({ "error": failure });
            }
            let retention = if metric_failure.is_some() {
                "negative_evidence"
            } else {
                "accepted_evidence"
            };
            let artifact = self.artifact(
                project,
                value,
                Some(&experiment_id),
                "artifact_metrics",
                "application/json",
                &result_path,
                retention,
            )?;
            artifacts.push(artifact.clone());
            metric = Some(artifact);
        }
        let environment = ArtifactStore::open(&artifacts_root(project))?
            .resolve(&string(run.get("environmentManifestArtifactId")), None)?;
        let finished_at = job.finished_at.clone().unwrap_or_else(now_utc);
        let result_digest = sha256(
            &json!({ "state": job_state, "metrics": metrics, "exitCode": job.exit_code, "metricFailure": metric_failure }),
        );
        let started_at = job.started_at.clone().unwrap_or_default();
        let duration = max(
            0.0,
            ((parse_ms(&finished_at) - parse_ms(&started_at)) / 1000.0).floor(),
        );
        let log_artifacts: Vec<Value> = artifacts
            .iter()
            .filter(|artifact| string(artifact.get("kind")).contains("log"))
            .map(|artifact| artifact["artifactId"].clone())
            .collect();
        let job_result = json!({
            "$schema": schema_uri("job-result"), "schemaVersion": 1, "jobId": job_id, "runId": job.spec.run_id, "experimentId": job.spec.experiment_id, "state": job_state,
            "commandDigest": job.command_digest, "evaluatedCommit": job.spec.commit_sha, "environmentManifestArtifactId": environment["artifactId"], "startedAt": job.started_at, "finishedAt": finished_at,
            "durationSeconds": num(duration), "exit": { "exitCode": job.exit_code, "failureReason": job.failure_reason.clone().or_else(|| metric_failure.clone()) },
            "resourceUse": { "gpuSeconds": if job.spec.uses_gpu { num(duration) } else { json!(0) } }, "metricArtifactId": metric.as_ref().map(|metric| metric["artifactId"].clone()),
            "logArtifactIds": log_artifacts, "checkpointArtifactIds": [], "recovery": { "recoveredAfterRestart": job.recovered_after_restart }, "resultDigest": result_digest,
        });
        self.submit(
            &project_id,
            value,
            &format!("run:{run_id}"),
            &job_result,
            &format!("job-result:{job_id}"),
            None,
            Some(&experiment_id),
            Some(&run_id),
            Some(&job_id),
        )?;
        let parent_score = self.parent_score(
            value,
            contract,
            &string(proposal.get("parentExperimentId")),
            &records,
        )?;
        let (candidate_score, primary_failure) = primary_metric(&metrics, &contract.metric);
        let improvement = candidate_score.map(|score| {
            if contract.maximize {
                score - parent_score
            } else {
                parent_score - score
            }
        });
        let deterministic =
            job_state == "succeeded" && metric.is_some() && primary_failure.is_none();
        let checks = guardrail_checks(&metrics, contract);
        let violated: Vec<String> = checks
            .iter()
            .filter(|check| check["passed"] != true)
            .map(|check| text(&check["failure"]))
            .collect();
        let keep = deterministic
            && violated.is_empty()
            && improvement.is_some_and(|improvement| improvement >= contract.minimum_effect);
        let evidence_id = create_id(IdPrefix::Evd);
        let failure = primary_failure.or_else(|| metric_failure.clone());
        // Freeze the measured statement before Review. Only its review disposition may change.
        // Self-describing for later readers (e.g. a write-up): what was measured, and the metrics
        // Artifact by id.
        let subject = format!(
            "{experiment_id} (daemon-measured {})",
            slice_to(&stringify(&metrics), 600)
        );
        let statement = if keep {
            format!(
                "{subject} improved {} by {}.",
                contract.metric,
                optional_number(improvement)
            )
        } else if let Some(failure) = failure.filter(|failure| !failure.is_empty()) {
            format!("{subject} produced an invalid evaluation result: {failure}.")
        } else if !violated.is_empty() {
            format!(
                "{subject} violated contract guardrails: {}.",
                violated.join("; ")
            )
        } else {
            format!("{subject} did not meet the {} keep rule.", contract.metric)
        };
        let mut source_refs = vec![
            json!({ "refType": "source_experiment", "refId": experiment_id, "locator": metric.as_ref().map_or_else(|| json!(result_digest), |metric| metric["contentHash"].clone()) }),
        ];
        if let Some(metric) = &metric {
            source_refs.push(json!({ "refType": "source_artifact", "refId": metric["artifactId"], "locator": metric["contentHash"] }));
        }
        let candidate = json!({
            "$schema": schema_uri("evidence"), "schemaVersion": 1, "evidenceId": evidence_id, "projectId": project_id,
            "evidenceType": if keep { "evidence_positive.result" } else { "evidence_negative.result" }, "statement": statement, "polarity": if keep { "supports" } else { "contradicts" },
            "sourceRefs": source_refs, "evaluationContractHash": contract_hash, "scopeLimitations": ["Applies only to the frozen evaluation contract"],
            "quality": { "status": "unreviewed", "reviewId": null, "confidence": if deterministic { "high" } else { "low" } },
            "createdBy": "agent", "createdByAgentId": proposed_by, "createdAt": now_utc(),
        });
        let mut request: Option<Value> = None;
        let mut verdict: Option<Value> = None;
        let mut passed = false;
        // Only a promotion candidate gets an experiment Review: a discard is a measured negative
        // the closure Review judges.
        if let (true, Some(metric)) = (keep, &metric) {
            let reviewer_id = create_id(IdPrefix::Agt);
            let branch = branch_name(&autoresearch_id, &experiment_id);
            // The keep Review judges the change itself, so it gets the variant's code diff (the
            // worker's diff Artifact and inline).
            let packet = records.iter().find(|record| {
                is_schema(record, "task-packet")
                    && record["scope"]["experimentId"] == run["experimentId"]
            });
            let completion = records.iter().find(|record| {
                is_schema(record, "general-worker-completion")
                    && record["codeChanges"]["branch"] == branch.as_str()
            });
            let diff = packet.map_or_else(String::new, |packet| {
                git_stdout(
                    &repository_root(project),
                    &[
                        "diff",
                        "--no-color",
                        &string(packet["workspace"].get("startingCommit")),
                        &string(run.get("evaluatedCommit")),
                        "--",
                        ".",
                        ":(exclude).nosh",
                    ],
                )
            });
            let mut reviewed_artifacts = vec![string(metric.get("artifactId"))];
            if let Some(diff_artifact) = completion
                .and_then(|completion| completion["codeChanges"]["diffArtifactId"].as_str())
                .filter(|id| !id.is_empty())
            {
                reviewed_artifacts.push(diff_artifact.to_owned());
            }
            let built = self.review_request(&project_id, value, ReviewSpec {
                reviewer_agent_id: &reviewer_id,
                producers: vec![proposed_by.clone()],
                review_type: "experiment",
                target_type: "target_experiment",
                target_id: experiment_id.clone(),
                target_version: 1,
                criterion_id: format!("criterion_{}", from(&experiment_id, 4)),
                statement: format!("Confirm this kept variant is a legitimate improvement. From its code diff (in the review context): the change does what it describes, within the declared files, and nothing fits on, reads, or encodes the evaluation split's labels or the evaluation program's results (the frozen evaluation program is in the repository at your working directory). From the metrics Artifact: the daemon's parent comparison and guardrail checks hold. The hypothesis is context; judge the diff and the measurement, not its wording or claims about other variants: {}", slice_to(&string(proposal.get("hypothesis")), 1_200)),
                artifacts: reviewed_artifacts,
                evidence: vec![evidence_id.clone()],
            })?;
            research.offer_candidate_evidence(&project_id, candidate.clone());
            let diff = slice_to(&diff, 8_000);
            let context = format!(
                "Review experiment {experiment_id}. Change under test: {} Evaluated commit {}. Code diff from the parent commit:\n{}\n Daemon-measured metrics from the frozen evaluation Job (authoritative; the metrics Artifact holds these exact values): {}. Daemon guardrail checks: {}. Parent score={}; candidate score={}; improvement={}. Review this candidate Evidence for publication with your exact disposition: {}",
                string(proposal["primaryChange"].get("description")),
                string(run.get("evaluatedCommit")),
                if diff.is_empty() {
                    "(no diff available)"
                } else {
                    &diff
                },
                stringify(&metrics),
                stringify(&json!(checks)),
                number_string(parent_score),
                optional_number(candidate_score),
                optional_number(improvement),
                stringify(&candidate)
            );
            let reviewed = self
                .review_session(
                    project,
                    value,
                    Some(&experiment_id),
                    &reviewer_id,
                    &built,
                    &context,
                )
                .await;
            research.withdraw_candidate_evidence(&project_id, &evidence_id);
            (verdict, passed) = reviewed?;
            request = Some(built);
        }
        let accepted = keep && passed;
        let mut evidence = candidate.clone();
        evidence["quality"]["status"] = json!(if passed { "reviewed" } else { "unreviewed" });
        evidence["quality"]["reviewId"] = match (&request, passed) {
            (Some(request), true) => request["reviewId"].clone(),
            _ => Value::Null,
        };
        self.submit(
            &project_id,
            value,
            &format!("experiment:{experiment_id}"),
            &evidence,
            &format!("experiment-evidence:{experiment_id}"),
            Some(&proposed_by),
            Some(&experiment_id),
            Some(&run_id),
            Some(&job_id),
        )?;
        let artifact_ids: Vec<Value> = artifacts
            .iter()
            .map(|artifact| artifact["artifactId"].clone())
            .collect();
        let mut result = json!({
            "$schema": schema_uri("experiment-result"), "schemaVersion": 1, "autoresearchId": autoresearch_id, "experimentId": experiment_id, "parentExperimentId": proposal["parentExperimentId"],
            "branch": branch_name(&autoresearch_id, &experiment_id), "evaluatedCommit": run["evaluatedCommit"], "evaluationContractHash": contract_hash, "runIds": [run_id],
            "runSetStatus": if deterministic { "complete" } else { "invalid" }, "metrics": [metrics], "guardrails": checks,
            "comparison": { "parentScore": num(parent_score), "candidateScore": candidate_score.map(num), "improvement": improvement.map(num), "minimumEffect": num(contract.minimum_effect) },
            "deterministicValidation": if deterministic { "pass" } else { "fail" }, "reviewId": request.as_ref().map(|request| request["reviewId"].clone()), "reviewVerdict": verdict.as_ref().map(|verdict| verdict["verdict"].clone()),
            "promotionRecommendation": if accepted { "promote" } else { "reject" }, "promotionDecision": if accepted { "held" } else { "rejected" }, "artifactIds": artifact_ids,
            "attemptFingerprint": sha256(&json!({ "proposalId": proposal["proposalId"], "evaluatedCommit": run["evaluatedCommit"], "contract": contract_hash })), "completedAt": now_utc(),
        });
        normalize_numbers(&mut result);
        self.submit(
            &project_id,
            value,
            &format!("experiment:{experiment_id}"),
            &result,
            &format!("experiment-result:{experiment_id}"),
            Some(&proposed_by),
            Some(&experiment_id),
            Some(&run_id),
            Some(&job_id),
        )?;
        let mut all = records;
        all.push(result);
        let all = unique_results(&all);
        if round_boundary(all.len(), &round_widths(value, contract)) {
            let current = research.autoresearch_execution(&project_id, &autoresearch_id)?;
            self.finish_round(
                project,
                &current,
                contract,
                integer(&proposal["round"]).unwrap_or_default(),
                &all,
            )
            .await?;
        }
        Ok(())
    }

    async fn finish_round(
        &self,
        project: &Value,
        execution: &Value,
        contract: &Contract,
        round: i64,
        results: &[Value],
    ) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let value = &execution["value"];
        let autoresearch_id = string(execution.get("entityId"));
        let contract_hash = string(value.get("evaluationContractHash"));
        let records = self.records(&project_id, &autoresearch_id)?;
        let proposals: Vec<&Value> = records
            .iter()
            .filter(|record| is_schema(record, "experiment-proposal") && record["round"] == round)
            .collect();
        let round_results: Vec<Value> = results
            .iter()
            .filter(|result| {
                proposals
                    .iter()
                    .any(|proposal| proposal["experimentId"] == result["experimentId"])
            })
            .cloned()
            .collect();
        let mut candidates: Vec<&Value> = round_results
            .iter()
            .filter(|result| {
                result["promotionRecommendation"] == "promote"
                    && result["deterministicValidation"] == "pass"
            })
            .collect();
        sort_by_number(&mut candidates, |left, right| score(right) - score(left));
        let winner = candidates.first().map(|winner| (*winner).clone());
        let reviewer_id = create_id(IdPrefix::Agt);
        let evidence_ids: Vec<String> = records
            .iter()
            .filter(|record| {
                is_schema(record, "evidence")
                    && record["sourceRefs"].as_array().is_some_and(|sources| {
                        sources.iter().any(|source| {
                            source["refId"].as_str().is_some_and(|id| {
                                proposals
                                    .iter()
                                    .any(|proposal| proposal["experimentId"] == id)
                            })
                        })
                    })
            })
            .filter_map(|record| record["evidenceId"].as_str().map(str::to_owned))
            .collect();
        // One evaluated experiment per round (the greedy climb; a crashed attempt is not one): its
        // own experiment Review already decided promotion, so no round Review.
        let request = if round_results.len() == 1 {
            None
        } else {
            Some(
                self.review_request(
                    &project_id,
                    value,
                    ReviewSpec {
                        reviewer_agent_id: &reviewer_id,
                        producers: proposals
                            .iter()
                            .map(|proposal| string(proposal.get("proposedByAgentId")))
                            .collect(),
                        review_type: "experiment_round",
                        target_type: "target_experiment.round",
                        target_id: format!("round_{}.{round}", from(&autoresearch_id, 3)),
                        target_version: round,
                        criterion_id: format!("criterion_round.{round}"),
                        statement: format!(
                            "Validate round {round} sibling comparison and promotion"
                        ),
                        artifacts: round_results
                            .iter()
                            .flat_map(|result| strings(&result["artifactIds"]))
                            .collect(),
                        evidence: evidence_ids.clone(),
                    },
                )?,
            )
        };
        let passed = match &request {
            None => winner.is_some(),
            Some(request) => {
                let selected = match &winner {
                    Some(winner) => format!(
                        "{} (improvement {})",
                        string(winner.get("experimentId")),
                        string(winner["comparison"].get("improvement"))
                    ),
                    None => "none (no result qualifies for promotion)".to_owned(),
                };
                let context = [
                    format!("Review Autoresearch round {round}. Daemon-selected winner: {selected}."),
                    "A promotionDecision of \"held\" means the result awaits THIS round Review: your PASS authorizes the daemon to promote the winner; any other verdict leaves it unpromoted. Judge whether the sibling comparison is valid and the winner is the right choice under the frozen contract and its guardrails.".to_owned(),
                    format!("Round results (daemon-measured; the listed Artifacts hold these exact values): {}", digest_results(&round_results)),
                    format!("Evidence: {}", digest_evidence(&records, &evidence_ids)),
                ]
                .join(" ");
                self.review_session(project, value, None, &reviewer_id, request, &context)
                    .await?
                    .1
            }
        };
        let mut promoted: Vec<String> = Vec::new();
        if let (Some(winner), true) = (&winner, passed) {
            let manager = manager_for(project)?;
            let base = self.root_commit(project, value)?;
            let integration_branch = format!("ar/{}/integration", slice(&autoresearch_id, 3, 11));
            let integration_id = format!("integration_{}", from(&autoresearch_id, 3));
            let create = research.begin_external_operation(
                &project_id,
                "git.worktree.create",
                &format!("git-integration:{autoresearch_id}"),
                &json!({ "branch": integration_branch, "baseCommit": base, "worktreeId": integration_id }),
            )?;
            let integration = manager.create(&integration_branch, &base, &integration_id)?;
            research.complete_external_operation(
                &project_id,
                &string(create.get("intentId")),
                &json!({ "branch": integration_branch, "worktree": integration.to_string_lossy() }),
            )?;
            let winner_id = string(winner.get("experimentId"));
            let promote = research.begin_external_operation(
                &project_id,
                "git.experiment.promote",
                &format!("git-promote:{winner_id}"),
                &json!({ "experimentId": winner_id, "integration": integration.to_string_lossy(), "evaluationContractHash": contract_hash }),
            )?;
            let head = manager.promote(&winner_id, &integration, "PASS", true)?;
            research.complete_external_operation(
                &project_id,
                &string(promote.get("intentId")),
                &json!({ "experimentId": winner_id, "integrationHead": head }),
            )?;
            let mut promoted_result = winner.clone();
            promoted_result["promotionDecision"] = json!("promoted");
            self.submit(
                &project_id,
                value,
                &format!("experiment:{winner_id}"),
                &promoted_result,
                &format!("experiment-result-promoted:{winner_id}"),
                None,
                Some(&winner_id),
                None,
                None,
            )?;
            promoted = vec![winner_id];
        }
        let manager = manager_for(project)?;
        let current_records = research.submitted(&project_id, None)?;
        for result in &round_results {
            let result_id = string(result.get("experimentId"));
            let Some(proposal) = proposals
                .iter()
                .find(|proposal| proposal["experimentId"] == result["experimentId"])
            else {
                return rejected("Round result has no proposal");
            };
            let frozen = manager.assert_frozen(
                &result_id,
                &string(result.get("evaluatedCommit")),
                &contract_hash,
            )?;
            let sequence = current_records
                .iter()
                .rev()
                .find(|entry| {
                    entry["scope"]["autoresearchId"] == autoresearch_id.as_str()
                        && is_schema(&entry["payload"], "experiment-result")
                        && entry["payload"]["experimentId"] == result["experimentId"]
                })
                .map_or_else(|| json!(0), |entry| entry["sequence"].clone());
            let accepted = promoted.contains(&result_id);
            let review_id = match (&request, accepted) {
                (Some(request), true) => request["reviewId"].clone(),
                _ => result["reviewId"].clone(),
            };
            let manifest = json!({
                "$schema": schema_uri("experiment-manifest"), "schemaVersion": 1, "experimentId": result_id, "autoresearchId": autoresearch_id, "proposalId": proposal["proposalId"],
                "parentExperimentId": result["parentExperimentId"], "treeDepth": round, "round": round, "hypothesis": proposal["hypothesis"], "primaryChange": proposal["primaryChange"],
                "ideaFingerprint": proposal["novelty"]["ideaFingerprint"], "attemptFingerprint": result["attemptFingerprint"], "branch": result["branch"], "worktreeId": format!("wt_{}", from(&result_id, 4)),
                "parentCommit": frozen.parent_commit, "implementationCommit": frozen.evaluated_commit, "evaluatedCommit": frozen.evaluated_commit, "evaluationContractHash": contract_hash,
                "state": if accepted { "accepted" } else { "rejected" }, "runIds": result["runIds"], "resultRecordSequence": sequence, "reviewId": review_id,
                "promotionDecision": if accepted { "promoted" } else { "rejected" }, "artifactIds": result["artifactIds"], "createdAt": proposal["submittedAt"], "evaluatedAt": result["completedAt"],
            });
            self.submit(
                &project_id,
                value,
                &format!("experiment:{result_id}"),
                &manifest,
                &format!("experiment-manifest:{result_id}"),
                None,
                Some(&result_id),
                None,
                None,
            )?;
        }
        let no_progress = if promoted.is_empty() {
            no_progress_rounds(&records) + 1
        } else {
            0
        };
        let stop =
            promoted.is_empty() && no_progress as f64 >= contract.no_progress_rounds.unwrap_or(1.0);
        let fingerprints: HashSet<String> = proposals
            .iter()
            .map(|proposal| string(proposal["novelty"].get("ideaFingerprint")))
            .collect();
        let identifiers = |items: &[Value]| -> Vec<Value> {
            items
                .iter()
                .map(|item| item["experimentId"].clone())
                .collect()
        };
        let proposed: Vec<Value> = proposals
            .iter()
            .map(|proposal| proposal["experimentId"].clone())
            .collect();
        let rejected_ids: Vec<Value> = round_results
            .iter()
            .filter(|result| !promoted.contains(&string(result.get("experimentId"))))
            .map(|result| result["experimentId"].clone())
            .collect();
        let input_frontier = if round == 1 {
            vec![string(value.get("rootExperimentId"))]
        } else {
            frontier(execution)
        };
        let round_review = match (&request, &winner) {
            (Some(request), _) => request["reviewId"].clone(),
            (None, Some(winner)) => winner["reviewId"].clone(),
            (None, None) => Value::Null,
        };
        let purpose = if stop {
            "Stop because no candidate passed"
        } else if !promoted.is_empty() {
            "Descend from the reviewed winner"
        } else {
            "Continue bounded search after no promotion"
        };
        let round_record = json!({
            "$schema": schema_uri("autoresearch-round"), "schemaVersion": 1, "autoresearchId": autoresearch_id, "round": round, "inputFrontierExperimentIds": input_frontier,
            "proposedExperimentIds": proposed, "evaluatedExperimentIds": identifiers(&round_results), "promotedExperimentIds": promoted, "rejectedExperimentIds": rejected_ids,
            "progressDelta": { "reviewedExperiments": round_results.len(), "promoted": promoted.len() },
            "focusCheck": { "distinctIdeaFamiliesInWindow": fingerprints.len(), "repeatedFingerprintCount": proposals.len() - fingerprints.len(), "noProgressCount": no_progress, "action": if stop { "stop" } else { "continue" } },
            "reviewId": round_review, "nextRoundPurpose": purpose, "completedAt": now_utc(),
        });
        self.submit(
            &project_id,
            value,
            &format!("autoresearch:{autoresearch_id}"),
            &round_record,
            &format!("autoresearch-round:{autoresearch_id}:{round}"),
            request.as_ref().map(|_| reviewer_id.as_str()),
            None,
            None,
            None,
        )?;
        let frontier = if promoted.is_empty() {
            frontier(execution)
        } else {
            promoted.clone()
        };
        let advanced = research.advance_autoresearch(
            &project_id,
            &autoresearch_id,
            version(execution),
            round as f64,
            &frontier,
            &format!("autoresearch-round-projection:{autoresearch_id}:{round}"),
        )?;
        if stop {
            let reason = if frontier.is_empty() {
                "all_hypotheses_rejected"
            } else {
                "evidence_saturation"
            };
            let records = self.records(&project_id, &autoresearch_id)?;
            return self.close(project, &advanced, &records, reason, None).await;
        }
        let at_target = winner
            .as_ref()
            .and_then(|winner| winner["comparison"]["candidateScore"].as_f64());
        let target_reached = match (contract.target, at_target) {
            (Some(target), Some(score)) if contract.maximize => score >= target,
            (Some(target), Some(score)) => score <= target,
            _ => false,
        };
        if !promoted.is_empty() && target_reached {
            let records = self.records(&project_id, &autoresearch_id)?;
            self.close(project, &advanced, &records, "target_reached", None)
                .await?;
        }
        Ok(())
    }

    async fn close(
        &self,
        project: &Value,
        execution: &Value,
        records: &[Value],
        terminal_reason: &str,
        budget_use: Option<&Value>,
    ) -> Result<()> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let value = &execution["value"];
        let autoresearch_id = string(execution.get("entityId"));
        let results = unique_results(records);
        let evidence_ids: Vec<String> = records
            .iter()
            .filter(|record| is_schema(record, "evidence"))
            .filter_map(|record| record["evidenceId"].as_str().map(str::to_owned))
            .collect();
        if results.is_empty() || evidence_ids.is_empty() {
            return rejected(
                "Autoresearch cannot close without evaluated experiments and Evidence",
            );
        }
        let synthesis_path = self.write(
            &project_id,
            &format!("synthesis:{autoresearch_id}"),
            &repository_root(project)
                .join(".nosh")
                .join("autoresearch")
                .join(&autoresearch_id)
                .join("synthesis.json"),
            &json!({ "decisionQuestion": value["decisionQuestion"], "results": results, "frontier": value["acceptedFrontierExperimentIds"], "terminalReason": terminal_reason }),
        )?;
        let synthesis = self.artifact(
            project,
            value,
            None,
            "artifact_synthesis",
            "application/json",
            &synthesis_path,
            "accepted_evidence",
        )?;
        let synthesis_id = string(synthesis.get("artifactId"));
        let reviewer_id = create_id(IdPrefix::Agt);
        let question = string(value.get("decisionQuestion"));
        let request = self.review_request(
            &project_id,
            value,
            ReviewSpec {
                reviewer_agent_id: &reviewer_id,
                producers: records
                    .iter()
                    .filter(|record| is_schema(record, "experiment-proposal"))
                    .filter_map(|record| record["proposedByAgentId"].as_str().map(str::to_owned))
                    .collect(),
                review_type: "autoresearch_closure",
                target_type: "target_autoresearch",
                target_id: autoresearch_id.clone(),
                target_version: version(execution),
                criterion_id: format!("criterion_{}", from(&autoresearch_id, 3)),
                statement: question.clone(),
                artifacts: vec![synthesis_id.clone()],
                evidence: evidence_ids.clone(),
            },
        )?;
        let frontier = frontier(execution);
        let context = [
            format!(
                "Review Autoresearch closure for {question}. Terminal reason: {terminal_reason}. Accepted frontier: {}.",
                if frontier.is_empty() { "none".to_owned() } else { frontier.join(", ") }
            ),
            format!("The synthesis Artifact {synthesis_id} holds every result below (daemon-measured): {}", digest_results(&results)),
            format!("Evidence: {}", digest_evidence(records, &evidence_ids)),
            "PASS means the terminal reason, frontier, and answer to the decision question follow from these results under the frozen contract and its guardrails.".to_owned(),
        ]
        .join(" ");
        let (_, passed) = self
            .review_session(project, value, None, &reviewer_id, &request, &context)
            .await?;
        if !passed {
            return rejected("Autoresearch closure Review did not pass");
        }
        let best = frontier
            .iter()
            .find(|id| {
                results.iter().any(|result| {
                    result["experimentId"] == id.as_str()
                        && result["promotionDecision"] == "promoted"
                })
            })
            .cloned();
        let unpromoted: Vec<String> = results
            .iter()
            .filter(|result| {
                result["promotionRecommendation"] == "promote"
                    && result["promotionDecision"] != "promoted"
            })
            .map(|result| string(result.get("experimentId")))
            .collect();
        // Met the keep rule on daemon measurements but stopped by experiment Review: say so rather
        // than "nothing met the keep rule".
        let review_stopped: Vec<String> = results
            .iter()
            .filter(|result| {
                let comparison = &result["comparison"];
                !unpromoted.contains(&string(result.get("experimentId")))
                    && result["promotionDecision"] != "promoted"
                    && result["deterministicValidation"] == "pass"
                    && comparison["improvement"]
                        .as_f64()
                        .is_some_and(|improvement| {
                            improvement >= number(&comparison["minimumEffect"])
                        })
                    && result["guardrails"].as_array().is_none_or(|guardrails| {
                        guardrails
                            .iter()
                            .all(|guardrail| guardrail["passed"] == true)
                    })
                    && !result["reviewVerdict"].is_null()
                    && result["reviewVerdict"] != "PASS"
            })
            .map(|result| {
                format!(
                    "{} (experiment Review {})",
                    string(result.get("experimentId")),
                    string(result.get("reviewVerdict"))
                )
            })
            .collect();
        let runs: Vec<String> = records
            .iter()
            .filter(|record| is_schema(record, "run-manifest"))
            .filter_map(|record| record["runId"].as_str().map(str::to_owned))
            .collect();
        let budget = match budget_use {
            Some(budget) => budget.clone(),
            None => research.autoresearch_budget_use(&project_id, &autoresearch_id)?,
        };
        let answer = if let Some(best) = &best {
            format!("Reviewed experiment {best} is the accepted frontier.")
        } else if !unpromoted.is_empty() {
            format!(
                "Experiments {} met the keep rule and passed experiment Review, but no round Review promoted them; there is no accepted frontier.",
                unpromoted.join(", ")
            )
        } else if !review_stopped.is_empty() {
            format!(
                "Experiments {} met the keep rule on daemon-measured metrics, but their experiment Review did not pass, so none was promoted; there is no accepted frontier.",
                review_stopped.join(", ")
            )
        } else {
            "No tested experiment met the keep rule.".to_owned()
        };
        let evaluated: Vec<Value> = results
            .iter()
            .map(|result| result["experimentId"].clone())
            .collect();
        let failed: Vec<Value> = results
            .iter()
            .filter(|result| result["promotionRecommendation"] == "reject")
            .map(|result| result["experimentId"].clone())
            .collect();
        let completion = json!({
            "$schema": schema_uri("autoresearch-completion-packet"), "schemaVersion": 1, "completionPacketId": format!("completion_{}", from(&autoresearch_id, 3)), "autoresearchId": autoresearch_id,
            "directionId": value["directionId"], "evaluationContractHash": value["evaluationContractHash"], "terminalReason": terminal_reason, "budgetUse": budget,
            "tree": { "rootExperimentId": value["rootExperimentId"], "evaluatedExperimentIds": evaluated, "acceptedFrontierExperimentIds": value["acceptedFrontierExperimentIds"] },
            "decisionAnswer": answer, "bestExperimentId": best, "evidenceIds": evidence_ids, "failedHypothesisIds": failed, "unresolvedQuestions": [], "synthesisArtifactId": synthesis_id,
            "deterministicValidation": { "status": "pass", "validatorRunIds": runs }, "closureReviewId": request["reviewId"], "completedAt": now_utc(),
        });
        self.submit(
            &project_id,
            value,
            &format!("autoresearch:{autoresearch_id}"),
            &completion,
            &format!("autoresearch-completion:{autoresearch_id}"),
            Some(&reviewer_id),
            None,
            None,
            None,
        )?;
        research.transition_autoresearch(
            &project_id,
            &autoresearch_id,
            version(execution),
            "completed",
            &format!("autoresearch-completed:{autoresearch_id}"),
        )?;
        Ok(())
    }

    fn rehydrate_tree(
        &self,
        value: &Value,
        records: &[Value],
        root_commit: &str,
    ) -> Result<ExperimentTree> {
        let contract_hash = string(value.get("evaluationContractHash"));
        let mut tree = ExperimentTree::new(
            &contract_hash,
            number(&value["maximumDepth"]),
            number(&value["maximumChildrenPerParent"]),
        );
        let root = string(value.get("rootExperimentId"));
        tree.add(Experiment {
            experiment_id: root.clone(),
            parent_experiment_id: None,
            idea_fingerprint: sha256(&json!({ "root": root })),
            attempt_fingerprint: sha256(&json!({ "commit": root_commit })),
            evaluation_contract_hash: contract_hash.clone(),
            branch: "baseline".into(),
            evaluated_commit: Some(root_commit.to_owned()),
            depth: 0.0,
            state: "accepted".into(),
            review_id: None,
            reviewer_agent_id: None,
            guardrails_pass: Some(true),
            extra: Map::new(),
        })?;
        let proposals: Vec<&Value> = records
            .iter()
            .filter(|record| is_schema(record, "experiment-proposal"))
            .collect();
        let proposal_of = |result: &Value| {
            proposals
                .iter()
                .find(|proposal| proposal["experimentId"] == result["experimentId"])
                .copied()
        };
        let round_of = |result: &Value| {
            proposal_of(result)
                .and_then(|proposal| proposal["round"].as_f64())
                .unwrap_or(0.0)
        };
        let mut results = unique_results(records);
        sort_by_number(&mut results, |left, right| round_of(left) - round_of(right));
        for result in &results {
            let Some(proposal) = proposal_of(result) else {
                continue;
            };
            let experiment_id = string(result.get("experimentId"));
            let parent = string(result.get("parentExperimentId"));
            let depth = tree.get(&parent)?.depth + 1.0;
            tree.add(Experiment {
                experiment_id: experiment_id.clone(),
                parent_experiment_id: Some(parent),
                idea_fingerprint: string(proposal["novelty"].get("ideaFingerprint")),
                attempt_fingerprint: string(result.get("attemptFingerprint")),
                evaluation_contract_hash: contract_hash.clone(),
                branch: string(result.get("branch")),
                evaluated_commit: None,
                depth,
                state: "proposed".into(),
                review_id: None,
                reviewer_agent_id: None,
                guardrails_pass: None,
                extra: Map::new(),
            })?;
            let review = records.iter().find(|record| {
                is_schema(record, "review-verdict") && record["reviewId"] == result["reviewId"]
            });
            let review = match (
                review
                    .and_then(|review| review["reviewerAgentId"].as_str())
                    .filter(|id| !id.is_empty()),
                result["reviewId"].as_str().filter(|id| !id.is_empty()),
            ) {
                (Some(reviewer), Some(review_id)) => Some(ExperimentReview {
                    review_id: review_id.to_owned(),
                    reviewer_agent_id: reviewer.to_owned(),
                    verdict: review
                        .and_then(|review| review["verdict"].as_str())
                        .unwrap_or("BLOCKED")
                        .to_owned(),
                }),
                _ => None,
            };
            tree.evaluate(
                &experiment_id,
                &string(result.get("evaluatedCommit")),
                if result["promotionRecommendation"] == "promote" {
                    "accepted"
                } else {
                    "rejected"
                },
                review.as_ref(),
                result["deterministicValidation"] == "pass",
            )?;
        }
        Ok(tree)
    }

    /// A spawned Autoresearch runs only while what spawned it is running; user-started ones have
    /// no parent gate.
    fn parent_running(&self, project_id: &str, value: &Value) -> Result<bool> {
        let research = &self.deps.research;
        let origin = &value["origin"];
        if !truthy(Some(origin)) {
            return Ok(true);
        }
        if origin["scopeType"] == "mission" {
            return Ok(
                research.mission(project_id, &string(origin.get("scopeId")))?["state"] == "running",
            );
        }
        let direction = research.direction(project_id, &string(origin.get("scopeId")))?;
        if direction["state"] != "active" {
            return Ok(false);
        }
        let parent = &direction["value"]["origin"];
        Ok(parent["scopeType"] != "mission"
            || research.mission(project_id, &string(parent.get("scopeId")))?["state"] == "running")
    }

    fn root_commit(&self, project: &Value, value: &Value) -> Result<String> {
        let root = repository_root(project);
        if let Some(direction_id) = value["directionId"].as_str().filter(|id| !id.is_empty()) {
            let direction = self
                .deps
                .research
                .direction(&string(project.get("projectId")), direction_id)?;
            let Some(baseline) = direction["value"]["acceptedBaseline"]["commit"]
                .as_str()
                .filter(|commit| !commit.is_empty())
            else {
                return rejected("Direction baseline is unresolved");
            };
            return git(&root, &["rev-parse", &format!("{baseline}^{{commit}}")]);
        }
        match value["baseCommit"].as_str() {
            Some(commit) => Ok(commit.to_owned()),
            None => git(&root, &["rev-parse", "HEAD"]),
        }
    }

    fn parent_score(
        &self,
        value: &Value,
        contract: &Contract,
        parent_id: &str,
        records: &[Value],
    ) -> Result<f64> {
        if value["rootExperimentId"] == parent_id {
            // `Number(baselineMetrics[name] ?? 0)`.
            return Ok(match contract.baseline.get(&contract.metric) {
                None | Some(Value::Null) => 0.0,
                Some(baseline) => baseline.as_f64().unwrap_or(f64::NAN),
            });
        }
        let Some(parent) = unique_results(records)
            .into_iter()
            .find(|result| result["experimentId"] == parent_id)
        else {
            return rejected("Parent result is unresolved");
        };
        match primary_metric(&parent["metrics"][0], &contract.metric) {
            (Some(score), _) => Ok(score),
            (None, failure) => Err(Error::Rejected(failure.unwrap_or_default())),
        }
    }

    fn records(&self, project_id: &str, autoresearch_id: &str) -> Result<Vec<Value>> {
        Ok(self
            .deps
            .research
            .submitted(project_id, None)?
            .into_iter()
            .filter(|entry| entry["scope"]["autoresearchId"] == autoresearch_id)
            .map(|mut entry| entry["payload"].take())
            .collect())
    }

    fn task_record(&self, project_id: &str, task_id: &str, schema: &str) -> Result<Option<Value>> {
        Ok(self
            .deps
            .research
            .submitted(project_id, Some(&format!("task:{task_id}")))?
            .into_iter()
            .find(|entry| is_schema(&entry["payload"], schema))
            .map(|mut entry| entry["payload"].take()))
    }

    #[allow(clippy::too_many_arguments)]
    fn submit(
        &self,
        project_id: &str,
        value: &Value,
        correlation_id: &str,
        record: &Value,
        idempotency_key: &str,
        agent_id: Option<&str>,
        experiment_id: Option<&str>,
        run_id: Option<&str>,
        job_id: Option<&str>,
    ) -> Result<Value> {
        let mut record = record.clone();
        normalize_numbers(&mut record);
        Ok(self.deps.research.submit_daemon_record(
            project_id,
            &json!({ "projectId": project_id, "missionId": value["missionId"], "directionId": value["directionId"], "autoresearchId": value["autoresearchId"], "experimentId": experiment_id, "runId": run_id, "jobId": job_id, "agentId": agent_id }),
            correlation_id,
            &record,
            idempotency_key,
        )?)
    }

    #[allow(clippy::too_many_arguments)]
    fn task_packet(
        &self,
        project: &Value,
        execution: &Value,
        proposal: &Value,
        task_id: &str,
        agent_id: &str,
        branch: &str,
        starting_commit: &str,
    ) -> Result<Value> {
        let project_id = string(project.get("projectId"));
        let value = &execution["value"];
        let experiment_id = string(proposal.get("experimentId"));
        let suffix = from(&experiment_id, 4);
        // The frozen evaluation and its other splits are the daemon's to run; the worker only
        // implements and tests. Its files (program and result) are protected too, so a variant
        // cannot change how it is measured.
        let evaluation = &value["evaluationContract"]["execution"];
        let frozen = strings(&evaluation["command"]);
        let commands: Vec<Value> = declared_task_commands(project)
            .into_iter()
            .filter(|command| !is_evaluation_variant(&strings(&command["argv"]), &frozen))
            .collect();
        let mut protected = vec![".nosh/**".to_owned()];
        if truthy(evaluation.get("command")) && truthy(evaluation.get("resultPath")) {
            protected.extend(frozen_evaluation_files(
                &repository_root(project),
                &frozen,
                &string(evaluation.get("resultPath")),
            ));
        }
        let scope = strings(&value["scope"]);
        let scope = if scope.is_empty() {
            vec!["**".to_owned()]
        } else {
            scope
        };
        let description = string(proposal["primaryChange"].get("description"));
        let mut instructions = vec![
            description.clone(),
            "Implement only this one variant. Do not run evaluations or compare methods: after your final answer the daemon evaluates the variant with the frozen evaluation command, and later rounds compare variants.".to_owned(),
        ];
        instructions.extend(task_command_instructions(&commands, false));
        let mut tools = vec![
            "tool_nosh.response.submit",
            "tool_nosh.blocker.submit",
            "tool_nosh.delegation.request",
            "tool_pi.read",
            "tool_pi.edit",
            "tool_pi.write",
            "tool_nosh.git.commit",
        ];
        if !commands.is_empty() {
            tools.push("tool_nosh.run");
        }
        let mission_id = &value["missionId"];
        let direction_id = &value["directionId"];
        let question = &value["decisionQuestion"];
        let (direction_question, direction_summary) = if truthy(Some(direction_id)) {
            (
                json!(format!("question_{}", from(&string(Some(direction_id)), 4))),
                question.clone(),
            )
        } else {
            (Value::Null, Value::Null)
        };
        let deadline = super::iso_in(7_200_000.0)?;
        let mut packet = json!({
            "$schema": schema_uri("task-packet"), "schemaVersion": 1, "templateVersion": "1.0.0",
            "taskId": task_id, "attempt": 1, "taskType": "task_experiment.implementation", "assignedRole": "general_worker", "assignedAgentId": agent_id,
            "scope": { "projectId": project_id, "missionId": mission_id, "directionId": direction_id, "autoresearchId": execution["entityId"], "experimentId": experiment_id, "graphNodeId": format!("experiment_{suffix}") },
            "goalStack": { "projectGoalId": project_id, "projectGoalSummary": question, "missionCriterionIds": if truthy(Some(mission_id)) { json!([mission_id]) } else { json!([]) }, "missionCriterionSummary": question, "directionQuestionId": direction_question, "directionQuestionSummary": direction_summary, "nodeObjective": proposal["hypothesis"] },
            "whyNow": format!("Round {} selected a distinct bounded hypothesis.", string(proposal.get("round"))),
            "instructions": instructions,
            "inScope": scope, "outOfScope": [".nosh/**"], "inputArtifactIds": [], "inputEvidenceIds": [],
            "requiredOutputs": [{ "outputId": format!("output_{suffix}"), "kind": "output_code.commit", "required": true }],
            "acceptanceCriteria": [{ "criterionId": format!("criterion_{suffix}"), "statement": format!("The proposed change is implemented exactly and committed within scope: {} (the hypothesis itself is judged afterwards by the frozen evaluation Job, not by this task)", slice_to(&description, 1_500)), "validatorIds": ["validator_git.postflight"], "reviewRubricIds": ["rubric_experiment.correctness"], "required": true }],
            "workspace": { "worktreeId": format!("wt_{suffix}"), "branch": branch, "startingCommit": starting_commit, "writeScopes": scope, "protectedScopes": protected },
            "permissions": { "network": "disabled", "subprocess": if commands.is_empty() { "disabled" } else { "allowlisted" }, "gitCommit": true, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": tools },
            "budget": { "deadline": deadline, "maximumWallClockSeconds": 7_200, "maximumModelTokens": num(min(60_000.0, number(&value["maximumModelTokens"]))), "maximumToolCalls": 100, "maximumRepairAttempts": 1 },
            "progressPolicy": { "milestoneIntervalSeconds": 900, "emitOnFirstDurableDelta": true, "emitOnBlocker": true, "emitOnAnomaly": true, "maximumSilentSeconds": 1_200 },
            "responseSchema": schema_uri("general-worker-completion"), "observedVersions": { "autoresearchProjectionVersion": execution["version"], "round": proposal["round"] },
            "lease": { "leaseId": task_id, "expiresAt": deadline, "heartbeatSeconds": 15 }, "issuedBy": "research_director", "issuedAt": now_utc(),
        });
        normalize_numbers(&mut packet);
        Ok(packet)
    }

    fn review_request(
        &self,
        project_id: &str,
        value: &Value,
        spec: ReviewSpec<'_>,
    ) -> Result<Value> {
        let producers = unique(spec.producers);
        if producers.is_empty() || producers.iter().any(|id| id == spec.reviewer_agent_id) {
            return rejected("Review requires an independent producer");
        }
        let review_id = create_id(IdPrefix::Rev);
        let request = json!({
            "$schema": schema_uri("review-request"), "schemaVersion": 1, "reviewRequestId": format!("request_{}", from(&review_id, 4)), "reviewId": review_id, "reviewType": spec.review_type,
            "target": { "targetType": spec.target_type, "targetId": spec.target_id, "targetVersion": spec.target_version },
            "scope": { "projectId": project_id, "missionId": value["missionId"], "directionId": value["directionId"], "autoresearchId": value["autoresearchId"] },
            "producerAgentIds": producers, "reviewerAgentId": spec.reviewer_agent_id, "independenceCheck": "pass",
            "contractRefs": [{ "kind": "contract_evaluation", "id": format!("contract_{}", from(&string(value.get("autoresearchId")), 3)), "hash": value["evaluationContractHash"] }],
            "requiredArtifactIds": unique(spec.artifacts), "requiredEvidenceIds": unique(spec.evidence),
            "deterministicPostflight": { "status": "pass", "validatorRunIds": ["validator_experiment.postflight"] },
            "criteria": [{ "criterionId": spec.criterion_id, "statement": spec.statement, "required": true, "severityIfFailed": "blocking" }],
            "allowedVerdicts": ["PASS", "REVISE", "REDESIGN", "BLOCKED"], "issuedAt": now_utc(),
        });
        self.submit(
            project_id,
            value,
            &format!("review:{review_id}"),
            &request,
            &format!("review-request:{review_id}"),
            Some(spec.reviewer_agent_id),
            None,
            None,
            None,
        )?;
        Ok(request)
    }

    /// Returns the reviewer's matching verdict whatever it decided; only the pass flag may
    /// authorize acceptance or promotion.
    async fn review_session(
        &self,
        project: &Value,
        value: &Value,
        experiment_id: Option<&str>,
        reviewer_id: &str,
        request: &Value,
        context: &str,
    ) -> Result<(Option<Value>, bool)> {
        let task_id = create_id(IdPrefix::Tsk);
        let prompt = format!(
            "{context} Review Request: {}. {}",
            stringify(request),
            reviewer_instructions(request, &self.deps.schema_dir)
        );
        let session = Session {
            agent_id: reviewer_id,
            task_id: &task_id,
            experiment_id,
            role: Role::Reviewer,
            cwd: repository_root(project),
            permissions: None,
            workspace: None,
        };
        if let Err(error) = self.session(project, value, session, &prompt).await {
            // A reviewer that never produced a valid verdict leaves the target unreviewed (so
            // never accepted or promoted); it does not halt the search.
            let message = error.to_string();
            if !message.starts_with("Terminal output rejected") {
                return Err(error);
            }
            self.emit(&event(
                value,
                "autoresearch.review_failed",
                json!({ "reviewId": request["reviewId"], "reviewType": request["reviewType"], "message": slice_to(&message, 2_000) }),
            ))?;
            return Ok((None, false));
        }
        let review = self
            .deps
            .research
            .terminal_record(
                &string(project.get("projectId")),
                &task_id,
                Some("review-verdict"),
            )?
            .filter(|review| review_matches(review, request, reviewer_id));
        let passed = review
            .as_ref()
            .is_some_and(|review| super::review_passes_request(review, request, reviewer_id));
        Ok((review, passed))
    }

    async fn session(
        &self,
        project: &Value,
        value: &Value,
        session: Session<'_>,
        prompt: &str,
    ) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let mission_id = optional(&value["missionId"]);
        let direction_id = optional(&value["directionId"]);
        let autoresearch_id = optional(&value["autoresearchId"]);
        let Some(runtime) = &self.deps.runtime else {
            let options = SessionOptions {
                project_id,
                mission_id,
                direction_id,
                autoresearch_id,
                experiment_id: session.experiment_id.map(str::to_owned),
                run_id: None,
                job_id: None,
                task_id: Some(session.task_id.to_owned()),
                agent_id: session.agent_id.to_owned(),
                role: session.role,
                cwd: session.cwd,
                package_path: self.deps.package_path.clone(),
                tools: None,
                runtime_scoped: false,
                task_permissions: session.permissions,
                task_workspace: session.workspace,
                thinking_level: None,
                model: None,
            };
            self.deps
                .agents
                .start(options)
                .await
                .map_err(Error::Rejected)?;
            let prompted = self
                .deps
                .agents
                .prompt(session.agent_id, prompt, None)
                .await;
            self.deps.agents.stop(session.agent_id);
            return prompted.map_err(Error::Rejected);
        };
        let thread_id = create_id(IdPrefix::Thr);
        let issued_at = now_utc();
        let key = format!("runtime:{}:{thread_id}", session.task_id);
        let instruction = |operation: Value| {
            let mut instruction = json!({ "$schema": schema_uri("runtime-instruction"), "schemaVersion": 1, "instructionId": create_id(IdPrefix::Ins), "projectId": project_id, "proposedByAgentId": null, "issuedAt": issued_at, "threadId": thread_id });
            if let (Some(target), Value::Object(fields)) = (instruction.as_object_mut(), operation)
            {
                target.extend(fields);
            }
            instruction
        };
        runtime
            .execute(&instruction(json!({
                "idempotencyKey": format!("{key}:open"), "operation": "THREAD_OPEN", "taskId": session.task_id, "initialAgentId": session.agent_id,
                "ownerScope": { "missionId": mission_id, "directionId": direction_id, "autoresearchId": autoresearch_id, "experimentId": session.experiment_id, "graphNodeId": null },
                "role": session.role.as_str(), "purpose": format!("Execute Autoresearch task {}", session.task_id), "executionMode": "background", "parentThreadId": null, "inputRefs": [], "skillIds": [], "capabilities": [],
                "taskPermissions": session.permissions, "taskWorkspace": session.workspace,
                "budget": { "maximumToolCalls": 200, "maximumModelTokens": 100_000, "maximumWallClockSeconds": 7_200 },
            })))
            .await?;
        let stepped = runtime
            .execute(&instruction(json!({ "idempotencyKey": format!("{key}:step"), "operation": "THREAD_STEP", "objective": prompt, "expectedEpisodeType": episode_type_for_role(session.role.as_str()), "inputRefs": [], "skillIds": [] })))
            .await;
        runtime
            .execute(&instruction(json!({ "idempotencyKey": format!("{key}:stop"), "issuedAt": now_utc(), "operation": "STOP", "programId": null, "reason": "Task session reached its boundary" })))
            .await?;
        stepped.map(drop)
    }

    /// Writes a daemon-authored file under a durable intent, verified by digest.
    fn write(&self, project_id: &str, key: &str, path: &Path, value: &Value) -> Result<PathBuf> {
        let research = &self.deps.research;
        let path = resolve(path)?;
        let path_text = path.to_string_lossy().into_owned();
        let content = format!("{}\n", canonical_json(value));
        let digest = sha256(&json!(content));
        let file_digest =
            |path: &Path| -> Result<String> { Ok(sha256(&json!(std::fs::read_to_string(path)?))) };
        let intent = research.begin_external_operation(
            project_id,
            "filesystem.write",
            &format!("filesystem-write:{key}"),
            &json!({ "path": path_text, "digest": digest, "content": content }),
        )?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !path.exists() || file_digest(&path)? != digest {
            let temporary = PathBuf::from(format!(
                "{path_text}.{}.tmp",
                string(intent.get("intentId"))
            ));
            std::fs::write(&temporary, &content)?;
            std::fs::rename(&temporary, &path)?;
        }
        if file_digest(&path)? != digest {
            return rejected("Durable file write verification failed");
        }
        research.complete_external_operation(
            project_id,
            &string(intent.get("intentId")),
            &json!({ "path": path_text, "digest": digest }),
        )?;
        Ok(path)
    }

    /// Copies a file into the Project's Artifact store under a durable intent and records it.
    #[allow(clippy::too_many_arguments)]
    fn artifact(
        &self,
        project: &Value,
        value: &Value,
        experiment_id: Option<&str>,
        kind: &str,
        media_type: &str,
        source_path: &Path,
        retention_class: &str,
    ) -> Result<Value> {
        let research = &self.deps.research;
        let project_id = string(project.get("projectId"));
        let source = resolve(source_path)?.to_string_lossy().into_owned();
        let artifact_id = format!(
            "art_{}",
            &sha256(
                &json!({ "autoresearchId": value["autoresearchId"], "experimentId": experiment_id, "kind": kind, "sourcePath": source })
            )[7..39]
        );
        let intent = research.begin_external_operation(
            &project_id,
            "artifact.copy",
            &format!("artifact-copy:{artifact_id}"),
            &json!({ "artifactId": artifact_id, "sourcePath": source, "kind": kind, "mediaType": media_type, "retentionClass": retention_class }),
        )?;
        let store = ArtifactStore::open(&artifacts_root(project))?;
        let input = json!({ "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": media_type, "sourcePath": source_path.to_string_lossy(), "retentionClass": retention_class });
        let artifact = store.add(input.as_object().expect("an Artifact input is an object"))?;
        store.resolve(&artifact_id, artifact["version"].as_i64())?;
        research.complete_external_operation(
            &project_id,
            &string(intent.get("intentId")),
            &json!({ "artifactId": artifact_id, "contentHash": artifact["contentHash"], "version": artifact["version"] }),
        )?;
        let record = json!({
            "$schema": schema_uri("artifact"), "schemaVersion": 1, "artifactId": artifact_id, "projectId": project_id, "kind": kind, "mediaType": media_type,
            "contentHash": artifact["contentHash"], "sizeBytes": artifact["sizeBytes"], "version": artifact["version"], "producer": { "type": "noshd" },
            "scope": { "missionId": value["missionId"], "directionId": value["directionId"], "autoresearchId": value["autoresearchId"], "experimentId": experiment_id },
            "git": null, "evaluationContractHash": value["evaluationContractHash"], "retentionClass": retention_class, "remotePreviewPolicy": "encrypted_allowed", "redactionStatus": "checked", "createdAt": artifact["createdAt"],
        });
        self.submit(
            &project_id,
            value,
            &format!("artifact:{artifact_id}"),
            &record,
            &format!("artifact-record:{artifact_id}"),
            None,
            experiment_id,
            None,
            None,
        )?;
        Ok(artifact)
    }
}

/// The one autoresearcher identity that proposes and implements every experiment of a run; never
/// the agent that created it.
pub fn autoresearcher_id(autoresearch_id: &str) -> String {
    format!(
        "agt_{}",
        &sha256(&json!({ "autoresearcher": autoresearch_id }))[7..39]
    )
}

/// The parent commit's in-scope files. A proposer cannot read files, so without them it guesses
/// what exists (live: it proposed config-only switches to calibrators the parent did not
/// implement, and every worker refused).
pub fn scope_snapshot(
    root: &Path,
    commit: &str,
    scope: &[String],
    maximum_files: usize,
    maximum_characters: usize,
) -> Vec<String> {
    let matches = |path: &str, entry: &str| {
        let mut scope_path = entry.replace('\\', "/");
        while let Some(rest) = scope_path.strip_prefix("./") {
            scope_path = rest.to_owned();
        }
        let scope_path = scope_path.trim_end_matches('/');
        if matches!(scope_path, "**" | "." | "") {
            return true;
        }
        let prefix = scope_path.strip_suffix("/**").unwrap_or(scope_path);
        path == prefix || path.starts_with(&format!("{prefix}/"))
    };
    git_stdout(root, &["ls-tree", "-r", "--name-only", commit])
        .lines()
        .filter(|path| !path.is_empty())
        .filter(|path| scope.iter().any(|entry| matches(path, entry)))
        .take(maximum_files)
        .map(|path| {
            let content = git_stdout(root, &["show", &format!("{commit}:{path}")]);
            let clipped = if length(&content) > maximum_characters {
                format!(" (first {maximum_characters} characters)")
            } else {
                String::new()
            };
            format!(
                "--- {path}{clipped}\n{}",
                slice_to(&content, maximum_characters)
            )
        })
        .collect()
}

/// Deterministic proposal brief: what may change, the frozen contract and baseline, the Project's
/// scope constraints, and every prior proposal with its measured result, so proposals climb and do
/// not repeat. Ends with a schema-valid template whose identity fields the daemon owns.
fn proposal_brief(input: &Brief<'_>) -> String {
    let value = &input.execution["value"];
    let autoresearch_id = string(input.execution.get("entityId"));
    let root = repository_root(input.project);
    let project_scope = match read_project_contract(&root) {
        Ok(contract) => strings(&contract["scope"]["included"]),
        Err(_) => vec!["unavailable: the active Project contract could not be read".to_owned()],
    };
    // The experiment log (keep / discard / crash, in order) is the autoresearcher's memory across
    // fresh sessions.
    let results = unique_results(input.records);
    let result_of = |id: &str| results.iter().find(|result| result["experimentId"] == id);
    let proposals: Vec<&Value> = input
        .records
        .iter()
        .filter(|record| is_schema(record, "experiment-proposal"))
        .collect();
    let prior: Vec<String> = proposals
        .iter()
        .map(|proposal| {
            let id = string(proposal.get("experimentId"));
            let result = result_of(&id);
            let change = slice_to(&text(&proposal["primaryChange"]["description"]), 200);
            let failed_guardrails = result
                .map(|result| {
                    result["guardrails"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|check| !truthy(check.get("passed")))
                        .map(|check| text(&check["failure"]))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();
            let crash = input.crashes.iter().find(|(crashed, _)| *crashed == id);
            let status = match (crash, result) {
                (Some((_, issues)), _) => format!("crash ({})", slice_to(issues, 160)),
                (None, None) => "pending".to_owned(),
                (None, Some(result)) if result["promotionDecision"] == "promoted" => {
                    "keep".to_owned()
                }
                (None, Some(result)) if result["promotionDecision"] == "held" => {
                    "keep candidate (awaiting review)".to_owned()
                }
                (None, Some(result)) if result["deterministicValidation"] != "pass" => {
                    "discard (invalid evaluation)".to_owned()
                }
                (None, Some(_)) if !failed_guardrails.is_empty() => {
                    format!("discard (guardrail: {})", slice_to(&failed_guardrails, 160))
                }
                (None, Some(result)) if truthy(result.get("reviewId")) => format!(
                    "discard (review {})",
                    if result["reviewVerdict"].is_null() {
                        "did not pass".to_owned()
                    } else {
                        string(result.get("reviewVerdict"))
                    }
                ),
                (None, Some(_)) => "discard (no improvement)".to_owned(),
            };
            let measured = match result.map(|result| &result["comparison"]) {
                Some(comparison) if !comparison["candidateScore"].is_null() => {
                    let improvement = &comparison["improvement"];
                    let sign = if !improvement.is_null() && number(improvement) >= 0.0 {
                        "+"
                    } else {
                        ""
                    };
                    format!(
                        " score {} ({sign}{} vs parent {})",
                        string(comparison.get("candidateScore")),
                        string(Some(improvement)),
                        string(comparison.get("parentScore"))
                    )
                }
                _ => String::new(),
            };
            format!(
                "- {id} round {}, parent {}: {status}{measured} | change: {change}",
                string(proposal.get("round")),
                string(proposal.get("parentExperimentId"))
            )
        })
        .collect();
    let last_keep = results
        .iter()
        .rposition(|result| result["promotionDecision"] == "promoted");
    let since_keep = results.len() - last_keep.map_or(0, |index| index + 1);
    let frontier = strings(&value["acceptedFrontierExperimentIds"]);
    let best = frontier
        .first()
        .filter(|best| !best.is_empty())
        .and_then(|best| result_of(best).map(|result| (best, result)));
    let current_best = match best {
        Some((best, result)) => format!(
            "{best} (score {})",
            string(result["comparison"].get("candidateScore"))
        ),
        None => {
            let contract = &value["evaluationContract"];
            let name = string(contract["primaryMetric"].get("name"));
            let baseline = contract["baselineMetrics"]
                .get(&name)
                .cloned()
                .unwrap_or(Value::Null);
            format!("the baseline (score {})", stringify(&baseline))
        }
    };
    // Fingerprints are daemon-computed (the model cannot hash); semantic novelty is judged from
    // the prior list above and by Review.
    let prior_fingerprint = proposals
        .last()
        .map(|proposal| proposal["novelty"]["ideaFingerprint"].clone())
        .unwrap_or(Value::Null);
    let scope = strings(&value["scope"]);
    let family_tags = strings(&value["familyTags"]);
    let declared = |items: &[String]| {
        if items.is_empty() {
            "none declared".to_owned()
        } else {
            items.join(", ")
        }
    };
    let expected_files = if scope.is_empty() {
        vec!["EDIT: repository-relative file".to_owned()]
    } else {
        scope.clone()
    };
    let template = json!({
        "$schema": schema_uri("experiment-proposal"), "schemaVersion": 1, "proposalId": format!("proposal_{}", from(input.experiment_id, 4)), "autoresearchId": autoresearch_id, "experimentId": input.experiment_id,
        "parentExperimentId": input.parent_experiment_id, "round": input.round,
        "hypothesis": "EDIT: the falsifiable hypothesis this one variant tests against its parent under the frozen contract (e.g. the change improves the primary metric by at least the minimum effect while guardrails pass). Never claim this variant is the best of several: one evaluation cannot show that, and comparing variants is the job of rounds and closure.",
        "rationale": "EDIT: why, citing the contract, baseline, and prior results above.",
        "primaryChange": { "category": "change_configuration", "description": "EDIT: the exact code or configuration change of this one variant, e.g. the new values in each file. Only the change: the daemon runs the frozen evaluation afterwards, and rounds compare variants.", "expectedFiles": expected_files },
        "expectedEffect": { "primaryMetric": "EDIT: expected direction and size of the change" }, "guardrailRisks": ["EDIT: a guardrail this change could violate, and why (add strings for more)"],
        "novelty": { "ideaFingerprint": sha256(&json!({ "autoresearchId": autoresearch_id, "experimentId": input.experiment_id })), "nearestAttemptFingerprint": prior_fingerprint, "difference": "EDIT: how this differs from every prior proposal above.", "duplicateStatus": "distinct" },
        "evaluationContractHash": value["evaluationContractHash"], "estimatedGpuSeconds": 0, "proposedByAgentId": input.agent_id, "submittedAt": now_utc(),
    });
    let mut lines = vec![
        format!("AUTORESEARCH PROPOSAL BRIEF (daemon-rendered; authoritative). You are the autoresearcher for {autoresearch_id}: a fresh agent dedicated to this run (you did not create it). Question: {}", string(value.get("decisionQuestion"))),
        "The loop: you propose one change on top of the current best; the daemon implements it, scores it with the frozen evaluation, and keeps it only if the primary metric improves by at least the minimum effect without breaking a guardrail (otherwise it is discarded); then you propose the next. The run never pauses for permission; it stops at its budget, its target, or a run of non-improving experiments.".to_owned(),
        format!("Current best: {current_best}; this experiment starts from parent {}, round {}.", input.parent_experiment_id, input.round),
        format!("Budget: {} of {} evaluated experiments used; {since_keep} consecutive without improvement; the run stops after {}.", results.len(), string(value.get("maximumExperiments")), number_string(input.stop_after)),
        "How to choose: refine the current best first (its parameters and close variants of what worked), one change per experiment. After 2 consecutive discards, combine near-misses, re-read the code below for new angles, or try a bolder change. Prefer simpler code: an equal result with simpler code is good, and removing code is welcome. After a crash, simplify. Never change the evaluation: it is protected.".to_owned(),
        format!("Idea families: {}. Files an experiment may change: {}.", declared(&family_tags), declared(&scope)),
        format!("Frozen evaluation contract {}: {}", string(value.get("evaluationContractHash")), slice_to(&stringify(&value["evaluationContract"]), 1_500)),
        "Project scope constraints:".to_owned(),
    ];
    lines.extend(
        project_scope
            .iter()
            .take(30)
            .map(|line| format!("- {}", slice_to(line, 300))),
    );
    lines.push("Experiment log (oldest first; keep / discard / crash):".to_owned());
    if prior.is_empty() {
        lines.push("- none yet".to_owned());
    } else {
        lines.extend(prior);
    }
    lines.push(format!("Files in scope at the parent commit {} (what this variant starts from: a method that is not implemented here must be implemented by the change, not only selected):", slice_to(input.parent_commit, 12)));
    if scope.is_empty() {
        lines.push("- the scope is the whole repository; no files are shown".to_owned());
    } else {
        lines.extend(scope_snapshot(&root, input.parent_commit, &scope, 8, 6_000));
    }
    lines.push("You cannot read files or run commands; the daemon implements the change in an isolated worktree, runs the frozen evaluation as a Job, and reviews the result. Choose a change inside every bound above that is likely to improve the primary metric without violating a guardrail, and that differs from every prior proposal.".to_owned());
    lines.push(format!("Your final answer must be exactly this JSON envelope: the {} (schema: {}) and your episode-draft. Copy it, keep every key, bracket, and non-EDIT value, and replace each \"EDIT: ...\" string; guardrailRisks and unresolvedQuestions are lists of plain strings; leave the episode's recommendedNextActions empty (the director plans next actions).", schema_uri("experiment-proposal"), schema_document_path(input.schema_dir, "experiment-proposal").display()));
    lines.push(terminal_envelope(&template, "general_worker"));
    lines.join("\n")
}

fn parse_contract(value: &Value) -> Result<Contract> {
    let metric = &value["primaryMetric"];
    let execution = &value["execution"];
    let stops = &value["stopConditions"];
    let finite = |value: &Value| value.as_f64().is_some_and(f64::is_finite);
    let whole = |value: &Value| integer(value).is_some();
    let objective = metric["objective"].as_str();
    let runner = execution["runner"].as_str();
    let valid = truthy(metric.get("name"))
        && matches!(objective, Some("maximize" | "minimize"))
        && finite(&metric["minimumEffect"])
        && matches!(runner, Some("native" | "wsl2"))
        && execution["command"]
            .as_array()
            .is_some_and(|command| !command.is_empty())
        && truthy(execution.get("resultPath"))
        && whole(&execution["timeoutSeconds"])
        && number(&execution["timeoutSeconds"]) > 0.0
        && stops.get("target").is_none_or(finite)
        && stops
            .get("maximumNoProgressRounds")
            .is_none_or(|rounds| whole(rounds) && number(rounds) >= 1.0);
    if !valid {
        return rejected(
            "Autoresearch evaluation contract lacks a runnable primary metric, execution command, or valid stop condition",
        );
    }
    Ok(Contract {
        metric: string(metric.get("name")),
        maximize: objective == Some("maximize"),
        minimum_effect: number(&metric["minimumEffect"]),
        baseline: value["baselineMetrics"].clone(),
        guardrails: value["guardrails"].as_array().cloned().unwrap_or_default(),
        round_widths: value["roundWidths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(integer)
            .filter(|width| *width > 0)
            .map(|width| width as usize)
            .collect(),
        target: stops.get("target").and_then(Value::as_f64),
        no_progress_rounds: stops.get("maximumNoProgressRounds").and_then(Value::as_f64),
        runner: if runner == Some("wsl2") {
            Runner::Wsl2
        } else {
            Runner::Native
        },
        distribution: optional(&execution["distribution"]),
        command: strings(&execution["command"]),
        result_path: string(execution.get("resultPath")),
        timeout_seconds: number(&execution["timeoutSeconds"]),
        checkpoint_command: execution["checkpointCommand"]
            .is_array()
            .then(|| strings(&execution["checkpointCommand"])),
        uses_gpu: truthy(execution.get("usesGpu")),
    })
}

/// Proposal failures since the last accepted proposal (a later accepted proposal resets the
/// count).
fn consecutive_proposal_failures(events: &[Value], autoresearch_id: &str) -> usize {
    let mut count = 0;
    for entry in events {
        if entry["scope"]["autoresearchId"] != autoresearch_id {
            continue;
        }
        if entry["type"] == "autoresearch.proposal_failed" {
            count += 1;
        } else if entry["type"] == "record.submitted"
            && is_schema(&entry["payload"], "experiment-proposal")
        {
            count = 0;
        }
    }
    count
}

fn proposal_permissions() -> TaskPermissions {
    TaskPermissions {
        network: "disabled".into(),
        subprocess: "disabled".into(),
        git_commit: false,
        git_push: false,
        delegation: "request_only".into(),
        network_allowlist: Vec::new(),
        allowed_tool_ids: vec!["tool_nosh.experiment.propose".into()],
    }
}

/// The worker's completion against its Task Packet: a postflight-ready record, its Git change,
/// passing validators, and every acceptance criterion satisfied.
fn implementation_issues(worktree: &Path, packet: &Value, record: &Value) -> Vec<String> {
    let value = match validate_record(&schema_uri("general-worker-completion"), record) {
        Ok(value) => value,
        Err(issues) => {
            return issues
                .iter()
                .map(|issue| format!("{} {}", issue.pointer, issue.message))
                .collect();
        }
    };
    let mut issues: Vec<String> = Vec::new();
    if value["taskOutcome"] != "completed"
        || !truthy(value.get("readyForReview"))
        || !truthy(value.get("readyForDeterministicPostflight"))
    {
        issues.push("completion is not postflight-ready".into());
    }
    issues.extend(validate_git_completion(
        worktree,
        &packet["workspace"],
        &value["codeChanges"],
    ));
    if value["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|command| !strict_equal(command.get("exitCode"), Some(&json!(0))))
    {
        issues.push("a declared validator failed".into());
    }
    for criterion in packet["acceptanceCriteria"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let satisfied = value["criteria"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| {
                strict_equal(entry.get("criterionId"), criterion.get("criterionId"))
                    && entry["workerClaim"] == "satisfied"
                    && entry["validatorRunIds"]
                        .as_array()
                        .is_some_and(|ids| !ids.is_empty())
            });
        if !satisfied {
            issues.push(format!(
                "criterion {} lacks a passing validator",
                string(criterion.get("criterionId"))
            ));
        }
    }
    unique(issues)
}

/// A verdict that answers exactly this Review Request, whatever it decided.
fn review_matches(review: &Value, request: &Value, reviewer_id: &str) -> bool {
    let same = |pointer: &str| strict_equal(review.pointer(pointer), request.pointer(pointer));
    validate_record(&schema_uri("review-verdict"), review).is_ok()
        && same("/reviewId")
        && same("/reviewRequestId")
        && same("/reviewType")
        && review["reviewerAgentId"] == reviewer_id
        && same("/target/targetType")
        && same("/target/targetId")
        && same("/target/targetVersion")
}

/// Contract guardrails are judged by the daemon from measured metrics (booleans count as 0/1); a
/// missing value or malformed declaration fails closed.
fn guardrail_checks(metrics: &Value, contract: &Contract) -> Vec<Value> {
    contract
        .guardrails
        .iter()
        .map(|guardrail| {
            let name = string(guardrail.get("name"));
            let raw = metrics.get(&name).cloned().unwrap_or(Value::Null);
            let measured = match &raw {
                Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
                other => other.as_f64(),
            };
            let threshold = guardrail["threshold"].as_f64().filter(|threshold| threshold.is_finite());
            let direction = guardrail["direction"].as_str();
            let (passed, failure) = match (threshold, direction, measured) {
                (Some(threshold), Some(direction @ ("maximize" | "minimize")), Some(measured)) => {
                    let (operator, passed) = if direction == "minimize" {
                        ("<=", measured <= threshold)
                    } else {
                        (">=", measured >= threshold)
                    };
                    let failure = (!passed).then(|| {
                        format!(
                            "{name}={} violates {operator} {}",
                            stringify(&raw),
                            number_string(threshold)
                        )
                    });
                    (passed, failure)
                }
                (Some(_), Some("maximize" | "minimize"), None) => (
                    false,
                    Some(format!("Guardrail metric {name} is missing or non-finite")),
                ),
                _ => (
                    false,
                    Some(format!(
                        "Guardrail {name} is malformed in the evaluation contract"
                    )),
                ),
            };
            json!({ "metric": name, "direction": string(guardrail.get("direction")), "threshold": threshold.map(num), "value": raw, "passed": passed, "failure": failure })
        })
        .collect()
}

/// The primary metric's finite value, or why there is none.
fn primary_metric(metrics: &Value, name: &str) -> (Option<f64>, Option<String>) {
    match metrics.get(name).and_then(Value::as_f64) {
        Some(value) if value.is_finite() => (Some(value), None),
        _ => (
            None,
            Some(format!("Metric {name} is missing or non-finite")),
        ),
    }
}

/// Reviewers judge contents, not IDs: a compact daemon-authored digest of results for review
/// prompts.
fn digest_results(results: &[Value]) -> String {
    let digests: Vec<Value> = results
        .iter()
        .map(|result| json!({ "experimentId": result["experimentId"], "evaluatedCommit": result["evaluatedCommit"], "metrics": result["metrics"], "guardrails": result["guardrails"], "comparison": result["comparison"], "deterministicValidation": result["deterministicValidation"], "reviewVerdict": result["reviewVerdict"], "promotionRecommendation": result["promotionRecommendation"], "promotionDecision": result["promotionDecision"] }))
        .collect();
    slice_to(&stringify(&json!(digests)), 8_000)
}

fn digest_evidence(records: &[Value], evidence_ids: &[String]) -> String {
    let digests: Vec<Value> = records
        .iter()
        .filter(|record| {
            is_schema(record, "evidence") && evidence_ids.contains(&string(record.get("evidenceId")))
        })
        .map(|record| json!({ "evidenceId": record["evidenceId"], "polarity": record["polarity"], "statement": record["statement"], "quality": record["quality"] }))
        .collect();
    slice_to(&stringify(&json!(digests)), 4_000)
}

/// Each experiment's latest result, in the order experiments were first recorded (a JavaScript
/// `Map`).
fn unique_results(records: &[Value]) -> Vec<Value> {
    let mut results: Vec<Value> = Vec::new();
    for record in records
        .iter()
        .filter(|record| is_schema(record, "experiment-result"))
    {
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

/// One experiment per round by default: a greedy climb where each experiment starts from the
/// current best.
fn round_widths(value: &Value, contract: &Contract) -> Vec<usize> {
    if !contract.round_widths.is_empty() {
        return contract.round_widths.clone();
    }
    let count = min(
        number(&value["maximumExperiments"]),
        number(&value["maximumRounds"]),
    );
    vec![1; count as usize]
}

fn round_at(ordinal: usize, widths: &[usize]) -> usize {
    let mut total = 0;
    for (index, width) in widths.iter().enumerate() {
        total += width;
        if ordinal <= total {
            return index + 1;
        }
    }
    widths.len()
}

fn round_boundary(count: usize, widths: &[usize]) -> bool {
    let mut total = 0;
    widths.iter().any(|width| {
        total += width;
        total == count
    })
}

fn no_progress_rounds(records: &[Value]) -> usize {
    records
        .iter()
        .rev()
        .filter(|record| is_schema(record, "autoresearch-round"))
        .take_while(|record| {
            !record["promotedExperimentIds"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty())
        })
        .count()
}

fn score(result: &Value) -> f64 {
    result["comparison"]["improvement"]
        .as_f64()
        .unwrap_or(f64::NEG_INFINITY)
}

fn branch_name(autoresearch_id: &str, experiment_id: &str) -> String {
    format!(
        "ar/{}/{}",
        slice(autoresearch_id, 3, 11),
        slice(experiment_id, 4, 12)
    )
}

fn manager_for(project: &Value) -> Result<GitWorkspaceManager> {
    let root = repository_root(project);
    Ok(GitWorkspaceManager::new(
        &root,
        &root.join(".nosh").join("worktrees"),
        &root.join(".nosh").join("git"),
    )?)
}

fn artifacts_root(project: &Value) -> PathBuf {
    repository_root(project).join(".nosh").join("artifacts")
}

/// `spawnSync("git", ["-C", root, ...args]).stdout ?? ""`: never fails.
fn git_stdout(root: &Path, args: &[&str]) -> String {
    nosh_sys::process::hidden("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

fn event(execution: &Value, kind: &str, payload: Value) -> Value {
    event_draft(
        kind,
        "autoresearch_supervisor",
        json!({ "projectId": execution["projectId"], "missionId": execution["missionId"], "directionId": execution["directionId"], "autoresearchId": execution["autoresearchId"], "experimentId": null, "runId": null, "jobId": null, "agentId": null }),
        execution["autoresearchId"].clone(),
        Value::Null,
        payload,
    )
}

fn is_schema(record: &Value, name: &str) -> bool {
    record["$schema"] == schema_uri(name)
}

fn frontier(execution: &Value) -> Vec<String> {
    strings(&execution["value"]["acceptedFrontierExperimentIds"])
}

fn current_round(execution: &Value) -> i64 {
    integer(&execution["value"]["currentRound"]).unwrap_or_default()
}

/// A JSON number that is an integer (`Number.isInteger`).
fn integer(value: &Value) -> Option<i64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && number.fract() == 0.0)
        .map(|number| number as i64)
}

/// A string field, or `None` for null or a non-string.
fn optional(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// A string field as `Array.prototype.join` renders it: null is empty.
fn text(value: &Value) -> String {
    if value.is_null() {
        String::new()
    } else {
        string(Some(value))
    }
}

/// A JSON number as JavaScript serializes it (non-finite is null).
fn num(value: f64) -> Value {
    let mut value = json!(value);
    normalize_numbers(&mut value);
    value
}

/// `${value}` for a number that may be null.
fn optional_number(value: Option<f64>) -> String {
    value.map_or_else(|| "null".to_owned(), number_string)
}

/// `text.slice(start, end)` for ASCII identifiers.
fn slice(text: &str, start: usize, end: usize) -> &str {
    let end = end.min(text.len());
    text.get(start.min(end)..end).unwrap_or_default()
}

/// `text.slice(start)` for ASCII identifiers.
fn from(text: &str, start: usize) -> &str {
    text.get(start..).unwrap_or_default()
}
