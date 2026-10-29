//! The NOSH daemon, as `NoshDaemon` in `apps/noshd/src/daemon.ts`: the host registry and
//! per-Project stores, Projects and their contracts, chat, typed submissions and terminal turns,
//! delegation, `nosh_run`, Artifacts, Jobs, backups, recovery, activity, and the research
//! supervisors.
//!
//! The daemon lives on one thread inside a `LocalSet`, as the TypeScript daemon lives on one event
//! loop: code between two awaits is atomic, so read-modify-write sequences need no locks. `Send`
//! callers reach it through [`Bridge`].
//!
//! Divergences: an event the Pi adapter emits while its Project is in backup maintenance is
//! dropped (TypeScript's `appendDraft` throws into the adapter), and a `nosh_run` Job's
//! `environmentLockHash` hashes the Rust daemon's platform, not a Node version.

mod bridge;
mod delegation;
mod facts;
pub(crate) use facts::environment_lock_hash;
mod intake;
mod submit;
pub mod terminal;

pub use bridge::{Bridge, Inbox, Job, bridge};
pub use intake::{contract_successor_template, intake_prompt};

use crate::runtime::{OrchestrationRuntime, RuntimeHost};
use crate::single_instance::SingleInstanceLock;
use crate::supervisors::autoresearch::AutoresearchSupervisor;
use crate::supervisors::direction::DirectionSupervisor;
use crate::supervisors::mission::MissionSupervisor;
use crate::supervisors::{Dependencies, SupervisorRuntime};
use crate::{Agents, Error, Result, rejected};
use nosh_agent::pi::{Inspection, ModelSelection};
use nosh_agent::policy::{ModelRef, Role, SessionOptions};
use nosh_agent::terminal::is_js_whitespace;
use nosh_research::agent_runtime::StructuredSubmissionGate;
use nosh_research::project::{
    ProjectInitialization, amend_project_contract, commit_project_contract, ensure_nosh_ignore,
    initialize_research_project, read_project_contract,
};
use nosh_research::research_control::{self, ResearchControl};
use nosh_store::backup::{RegisteredProject, create_project_backup, list_project_backups};
use nosh_store::restore::{apply_scheduled_restores, schedule_project_restore};
use nosh_store::{EventStore, HostRegistry};
use nosh_sys::jobs::{JobRecord, JobSpec, JobSupervisor};
use nosh_wire::js::string;
use nosh_wire::{EVENT_SCHEMA, canonical_json, sha256};
use nosh_wire::{IdPrefix, is_nosh_id};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

pub struct DaemonOptions {
    pub data_directory: PathBuf,
    pub bootstrap_token: String,
    /// The Pi package every session loads (`pi-package/`).
    pub package_path: PathBuf,
    /// Host-wide cap on live Pi sessions for delegation; 8 when unset.
    pub maximum_active_pi_sessions: Option<f64>,
    /// Model for every session without an explicit selection (from NOSH config).
    pub default_model: Option<ModelSelection>,
    /// The Job exit-wrapper command (`nosh job-wrapper`, or the `nosh-job-wrapper` binary).
    pub job_wrapper: Vec<OsString>,
    /// The shipped record JSON Schemas (`<name>.v1.schema.json`) prompts point at.
    pub schema_dir: PathBuf,
}

/// An event subscriber.
pub type Listener = Rc<dyn Fn(&Value)>;

/// What both the research control and the runtime reach the daemon through: the registry, the
/// lazily opened per-Project stores, event delivery, and backup maintenance.
pub struct Core {
    registry: RefCell<Option<HostRegistry>>,
    stores: RefCell<HashMap<String, Rc<EventStore>>>,
    listeners: RefCell<Vec<(u64, Listener)>>,
    next_listener: Cell<u64>,
    maintenance: RefCell<HashSet<String>>,
    inspection: RefCell<Option<String>>,
}

impl Core {
    fn registry<T>(
        &self,
        use_it: impl FnOnce(&HostRegistry) -> nosh_store::Result<T>,
    ) -> Result<T> {
        let registry = self.registry.borrow();
        let registry = registry
            .as_ref()
            .ok_or_else(|| Error::Rejected("NOSH daemon is stopped".into()))?;
        Ok(use_it(registry)?)
    }

    pub fn projects(&self) -> Result<Vec<Value>> {
        self.registry(HostRegistry::list)
    }

    /// The registered Project, or `Project <id> is not registered on this host`.
    pub fn project(&self, project_id: &str) -> Result<Value> {
        self.projects()?
            .into_iter()
            .find(|project| project["projectId"] == project_id)
            .ok_or_else(|| {
                Error::Rejected(format!(
                    "Project {project_id} is not registered on this host"
                ))
            })
    }

    pub fn store_for(&self, project_id: &str) -> Result<Rc<EventStore>> {
        if let Some(store) = self.stores.borrow().get(project_id) {
            return Ok(Rc::clone(store));
        }
        let project = self.project(project_id)?;
        let store = Rc::new(EventStore::open(Path::new(&string(
            project.get("databasePath"),
        )))?);
        self.stores
            .borrow_mut()
            .insert(project_id.to_owned(), Rc::clone(&store));
        Ok(store)
    }

    pub fn assert_writable(&self, project_id: &str) -> Result<()> {
        if self.maintenance.borrow().contains(project_id)
            && self.inspection.borrow().as_deref() != Some(project_id)
        {
            return rejected("Project is in backup maintenance mode");
        }
        Ok(())
    }

    /// Delivers a stored event to every subscriber, in subscription order.
    pub fn publish(&self, event: &Value) {
        // A listener may subscribe or unsubscribe while it runs.
        let listeners: Vec<Listener> = self
            .listeners
            .borrow()
            .iter()
            .map(|(_, listener)| Rc::clone(listener))
            .collect();
        for listener in listeners {
            listener(event);
        }
    }

    pub fn append_draft(&self, draft: &Value) -> Result<Value> {
        let project_id = string(draft.pointer("/scope/projectId"));
        self.assert_writable(&project_id)?;
        let event = self.store_for(&project_id)?.append(draft)?;
        self.publish(&event);
        Ok(event)
    }
}

fn research_error(error: Error) -> nosh_research::Error {
    match error {
        Error::Invalid(issues) => nosh_research::Error::Invalid(issues),
        Error::Rejected(message) | Error::Slip(message) => nosh_research::Error::Rejected(message),
    }
}

/// The research control's view of the daemon.
#[derive(Clone)]
pub struct ResearchHost(Rc<Core>);

impl research_control::Host for ResearchHost {
    fn store(&self, project_id: &str) -> nosh_research::Result<Rc<EventStore>> {
        self.0.store_for(project_id).map_err(research_error)
    }
    fn project(&self, project_id: &str) -> nosh_research::Result<Value> {
        self.0.project(project_id).map_err(research_error)
    }
    fn publish(&self, event: &Value) {
        self.0.publish(event);
    }
    fn assert_writable(&self, project_id: &str) -> nosh_research::Result<()> {
        self.0.assert_writable(project_id).map_err(research_error)
    }
}

/// The runtime's view of the daemon: its stores refuse Projects in backup maintenance.
#[derive(Clone)]
pub struct DaemonRuntimeHost {
    core: Rc<Core>,
    package_path: PathBuf,
}

impl RuntimeHost for DaemonRuntimeHost {
    fn store(&self, project_id: &str) -> Result<Rc<EventStore>> {
        self.core.assert_writable(project_id)?;
        self.core.store_for(project_id)
    }
    fn repository_root(&self, project_id: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(string(
            self.core.project(project_id)?.get("repositoryRoot"),
        )))
    }
    fn package_path(&self) -> PathBuf {
        self.package_path.clone()
    }
    fn publish(&self, event: &Value) {
        self.core.publish(event);
    }
}

pub type Research = ResearchControl<ResearchHost>;
pub type Runtime = OrchestrationRuntime<DaemonRuntimeHost>;

/// A chat message's receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatReceipt {
    pub event: Value,
    pub agent_id: String,
    pub replayed: bool,
}

pub struct NoshDaemon {
    options: DaemonOptions,
    core: Rc<Core>,
    lock: SingleInstanceLock,
    gate: RefCell<StructuredSubmissionGate>,
    started: Cell<bool>,
    delegation_runs: RefCell<HashSet<String>>,
    terminal_turns: terminal::TurnLocks,
    bridge: Bridge,
    pub agents: Rc<dyn Agents>,
    pub jobs: JobSupervisor,
    pub research: Rc<Research>,
    pub runtime: Rc<Runtime>,
    pub missions: Rc<MissionSupervisor<ResearchHost>>,
    pub directions: Rc<DirectionSupervisor<ResearchHost>>,
    pub experiments: Rc<AutoresearchSupervisor<ResearchHost>>,
}

/// Supervisors start Jobs through the daemon, so scope IDs are checked and the launch is durable.
impl crate::research_spawner::JobRunner for NoshDaemon {
    fn start(&self, spec: &JobSpec) -> Result<JobRecord> {
        self.start_job(spec)
    }
    fn get(&self, job_id: &str) -> Result<JobRecord> {
        Ok(self.jobs.get(job_id)?)
    }
}

/// The daemon as its own supervisors' Job runner: held weakly, since the daemon owns them.
struct DaemonJobs(Weak<NoshDaemon>);

impl crate::research_spawner::JobRunner for DaemonJobs {
    fn start(&self, spec: &JobSpec) -> Result<JobRecord> {
        self.daemon()?.start_job(spec)
    }
    fn get(&self, job_id: &str) -> Result<JobRecord> {
        Ok(self.daemon()?.jobs.get(job_id)?)
    }
}

impl DaemonJobs {
    fn daemon(&self) -> Result<Rc<NoshDaemon>> {
        self.0
            .upgrade()
            .ok_or_else(|| Error::Rejected("NOSH daemon is stopped".into()))
    }
}

impl Drop for NoshDaemon {
    fn drop(&mut self) {
        bridge::detach(self.bridge.key());
    }
}

/// `{ projectId, missionId: null, …, agentId: null }`.
pub(crate) fn project_scope(project_id: &str) -> Value {
    json!({ "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null })
}

/// A persistent event draft.
pub(crate) fn event_draft(
    kind: &str,
    source: &str,
    scope: Value,
    correlation_id: Value,
    causation_id: Value,
    payload: Value,
) -> Value {
    json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": kind, "source": source, "scope": scope, "correlationId": correlation_id, "causationId": causation_id, "payload": payload })
}

/// JavaScript's `String.prototype.trim`.
/// Starts `task` as JavaScript starts an un-awaited async call: it runs to its first pending
/// await before this returns (a chat prompt reaches Pi before `chat` answers), then continues on
/// the `LocalSet`.
pub(crate) fn spawn_eager(task: impl std::future::Future<Output = ()> + 'static) {
    use futures_util::FutureExt;
    let mut task = Box::pin(task);
    if (&mut task).now_or_never().is_none() {
        tokio::task::spawn_local(task);
    }
}

pub(crate) fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

pub(crate) fn error_text(error: &Error, fallback: &str) -> String {
    let message = error.to_string();
    if message.is_empty() {
        fallback.to_owned()
    } else {
        message
    }
}

fn deterministic_chat_agent_id(project_id: &str) -> String {
    let digest = sha256(&json!({ "projectId": project_id, "role": "nosh_chat" }));
    format!("agt_{}", &digest[7..39])
}

/// `{ provider, id, thinkingLevel: level ?? null }`, or null.
fn chat_selection(selection: Option<&Value>) -> Value {
    match selection {
        Some(selection) => json!({
            "provider": selection.get("provider").cloned().unwrap_or(Value::Null),
            "id": selection.get("id").cloned().unwrap_or(Value::Null),
            "thinkingLevel": match selection.get("thinkingLevel") { None => Value::Null, Some(level) => level.clone() },
        }),
        None => Value::Null,
    }
}

/// The scope a Job's events carry must be valid before a record or intent exists, otherwise the
/// saved Job stays "starting" forever when its first event is rejected (`scopeSchema.parse`).
fn assert_job_scope(spec: &JobSpec) -> Result<()> {
    let checks: [(&str, Option<&str>, IdPrefix, bool); 7] = [
        (
            "projectId",
            Some(spec.project_id.as_str()),
            IdPrefix::Prj,
            false,
        ),
        ("missionId", spec.mission_id.as_deref(), IdPrefix::Mis, true),
        (
            "directionId",
            spec.direction_id.as_deref(),
            IdPrefix::Dir,
            true,
        ),
        (
            "autoresearchId",
            spec.autoresearch_id.as_deref(),
            IdPrefix::Ar,
            true,
        ),
        (
            "experimentId",
            spec.experiment_id.as_deref(),
            IdPrefix::Exp,
            true,
        ),
        ("runId", Some(spec.run_id.as_str()), IdPrefix::Run, false),
        ("jobId", Some(spec.job_id.as_str()), IdPrefix::Job, false),
    ];
    for (field, value, prefix, nullable) in checks {
        let valid = match value {
            Some(value) => is_nosh_id(value, Some(prefix)),
            None => nullable,
        };
        if !valid {
            return rejected(format!(
                "Invalid scope: {field} must be a {} id",
                prefix.as_str()
            ));
        }
    }
    Ok(())
}

fn assert_job_identity(record: &JobRecord, spec: &JobSpec, context: &str) -> Result<()> {
    if record.spec.job_id != spec.job_id
        || record.spec.project_id != spec.project_id
        || record.command_digest != sha256(&json!(spec.command))
        || record.spec.commit_sha != spec.commit_sha
        || record.spec.evaluation_contract_hash != spec.evaluation_contract_hash
    {
        return rejected(format!(
            "{context} does not match its durable launch specification"
        ));
    }
    Ok(())
}

pub fn mission_job_matches(job: &JobRecord, project_id: &str, mission_id: &str) -> bool {
    job.spec.project_id == project_id && job.spec.mission_id.as_deref() == Some(mission_id)
}

impl NoshDaemon {
    /// Opens the host registry and wires the runtime, research control, Job supervisor, and the
    /// research supervisors. Needs a `LocalSet`: the daemon drains `inbox` on a local task.
    pub fn new(
        options: DaemonOptions,
        agents: Rc<dyn Agents>,
        (bridge, inbox): (Bridge, Inbox),
    ) -> Result<Rc<Self>> {
        std::fs::create_dir_all(&options.data_directory)?;
        let lock = SingleInstanceLock::new(&options.data_directory.join("noshd.lock"));
        let registry = HostRegistry::open(&options.data_directory.join("host.sqlite"))?;
        let core = Rc::new(Core {
            registry: RefCell::new(Some(registry)),
            stores: RefCell::new(HashMap::new()),
            listeners: RefCell::new(Vec::new()),
            next_listener: Cell::new(1),
            maintenance: RefCell::new(HashSet::new()),
            inspection: RefCell::new(None),
        });
        agents.set_default_model(options.default_model.clone());
        let research = Rc::new(ResearchControl::new(ResearchHost(Rc::clone(&core))));
        let runtime = OrchestrationRuntime::new(
            DaemonRuntimeHost {
                core: Rc::clone(&core),
                package_path: options.package_path.clone(),
            },
            Rc::clone(&agents),
        );
        let changes = bridge.clone();
        let jobs = JobSupervisor::new(
            &options.data_directory,
            options.job_wrapper.clone(),
            move |job: &JobRecord| {
                let job = job.clone();
                changes.run(move |daemon| daemon.on_job_change(&job));
            },
        )?;
        let daemon = Rc::new_cyclic(|daemon: &Weak<NoshDaemon>| {
            let (projects, emit) = (Rc::clone(&core), Rc::clone(&core));
            let deps = Dependencies {
                research: Rc::clone(&research),
                agents: Rc::clone(&agents),
                projects: Rc::new(move || projects.projects()),
                package_path: options.package_path.clone(),
                schema_dir: options.schema_dir.clone(),
                emit: Rc::new(move |draft: &Value| emit.append_draft(draft).map(|_| ())),
                runtime: Some(Rc::new(Rc::clone(&runtime)) as Rc<dyn SupervisorRuntime>),
                job_runner: Some(Rc::new(DaemonJobs(daemon.clone()))),
            };
            NoshDaemon {
                missions: MissionSupervisor::new(deps.clone()),
                directions: DirectionSupervisor::new(deps.clone()),
                experiments: AutoresearchSupervisor::new(deps),
                options,
                core,
                lock,
                gate: RefCell::new(StructuredSubmissionGate::default()),
                started: Cell::new(false),
                delegation_runs: RefCell::new(HashSet::new()),
                terminal_turns: terminal::TurnLocks::default(),
                bridge,
                agents,
                jobs,
                research,
                runtime,
            }
        });
        inbox.attach(&daemon);
        Ok(daemon)
    }

    pub fn bridge(&self) -> &Bridge {
        &self.bridge
    }

    pub fn data_directory(&self) -> &Path {
        &self.options.data_directory
    }

    pub fn package_path(&self) -> &Path {
        &self.options.package_path
    }

    pub fn schema_dir(&self) -> &Path {
        &self.options.schema_dir
    }

    /// Subscribes to every stored event; returns the id `unsubscribe` takes.
    pub fn subscribe(&self, listener: Listener) -> u64 {
        let id = self.core.next_listener.get();
        self.core.next_listener.set(id + 1);
        self.core.listeners.borrow_mut().push((id, listener));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.core
            .listeners
            .borrow_mut()
            .retain(|(listener, _)| *listener != id);
    }

    fn on_job_change(&self, job: &JobRecord) {
        let registered = self.projects().is_ok_and(|projects| {
            projects
                .iter()
                .any(|project| project["projectId"] == job.spec.project_id.as_str())
        });
        if !registered {
            return;
        }
        let spec = &job.spec;
        let scope = json!({ "projectId": spec.project_id, "missionId": spec.mission_id, "directionId": spec.direction_id, "autoresearchId": spec.autoresearch_id, "experimentId": spec.experiment_id, "runId": spec.run_id, "jobId": spec.job_id, "agentId": null });
        let payload = serde_json::to_value(job).unwrap_or(Value::Null);
        let draft = event_draft(
            "job.state_changed",
            "job_supervisor",
            scope,
            json!(spec.run_id),
            Value::Null,
            payload,
        );
        // A Job change for a Project that cannot accept it is reported by the next reconciliation.
        let _ = self.append_draft(&draft);
    }

    /// Takes the lock, applies scheduled restores, recovers Jobs, and reconciles every Project.
    pub fn start(self: &Rc<Self>) -> Result<()> {
        self.lock.acquire()?;
        let started = (|| -> Result<()> {
            apply_scheduled_restores(&self.options.data_directory)?;
            self.jobs.recover()?;
            for project in self.projects()? {
                // Projects opened before NOSH ignored its own runtime state get the ignore file
                // on the next start; an unreachable repository is reported by reconciliation.
                let _ = ensure_nosh_ignore(Path::new(&string(project.get("repositoryRoot"))));
                self.reconcile_project(&project)?;
            }
            self.start_supervisors();
            self.started.set(true);
            Ok(())
        })();
        if started.is_err() {
            self.lock.release();
        }
        started
    }

    pub fn stop(&self) {
        // Marks where downtime begins, so wall-clock budgets exclude the time until the next start.
        if self.started.get() {
            for project in self.projects().unwrap_or_default() {
                let project_id = string(project.get("projectId"));
                let draft = event_draft(
                    "daemon.shutdown",
                    "noshd",
                    project_scope(&project_id),
                    Value::Null,
                    Value::Null,
                    json!({}),
                );
                let _ = self.append_draft(&draft);
            }
        }
        self.started.set(false);
        self.stop_supervisors();
        for agent in self.agents.inspect() {
            self.agents.stop(&agent.agent_id);
        }
        self.jobs.close();
        self.core.stores.borrow_mut().clear();
        self.core.registry.borrow_mut().take();
        self.lock.release();
    }

    fn start_supervisors(&self) {
        self.missions.start();
        self.directions.start();
        self.experiments.start();
    }

    fn stop_supervisors(&self) {
        self.missions.stop();
        self.directions.stop();
        self.experiments.stop();
    }

    /// Constant-time comparison with the bootstrap token.
    pub fn authenticate(&self, token: Option<&str>) -> bool {
        let Some(token) = token.filter(|token| !token.is_empty()) else {
            return false;
        };
        let (provided, expected) = (token.as_bytes(), self.options.bootstrap_token.as_bytes());
        provided.len() == expected.len()
            && provided
                .iter()
                .zip(expected)
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0
    }

    pub fn register_project(
        self: &Rc<Self>,
        project_id: &str,
        repository_root: &Path,
        database_path: &Path,
    ) -> Result<Value> {
        self.assert_project_writable(project_id)?;
        let registered = self
            .core
            .registry(|registry| registry.register(project_id, repository_root, database_path))?;
        self.reconcile_project(&registered)?;
        Ok(registered)
    }

    pub fn initialize_project(
        self: &Rc<Self>,
        path: &Path,
        create_repository: bool,
        working_title: Option<&str>,
        github_repository_url: Option<&str>,
    ) -> Result<Value> {
        let metadata_path = nosh_sys::paths::resolve(path)?
            .join(".nosh")
            .join("project.json");
        if metadata_path.exists() {
            let metadata: Value = serde_json::from_str(&std::fs::read_to_string(&metadata_path)?)
                .map_err(|error| Error::Rejected(error.to_string()))?;
            if let Some(existing) = metadata.get("projectId").and_then(Value::as_str) {
                self.assert_project_writable(existing)?;
            }
        }
        let initialized = initialize_research_project(&ProjectInitialization {
            path,
            data_directory: &self.options.data_directory,
            create_repository,
            working_title,
            github_repository_url,
        })?;
        self.register_project(
            &initialized.project_id,
            &initialized.repository_root,
            &initialized.database_path,
        )
    }

    fn repository_root(&self, project_id: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(string(
            self.core.project(project_id)?.get("repositoryRoot"),
        )))
    }

    pub fn project_contract(&self, project_id: &str) -> Result<Value> {
        Ok(read_project_contract(&self.repository_root(project_id)?)?)
    }

    /// User-only: writes the next approved contract version (e.g. declaring `nosh_run`
    /// commands). Agents cannot reach this.
    pub fn amend_project_contract(
        &self,
        project_id: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        self.assert_project_writable(project_id)?;
        let repository_root = self.repository_root(project_id)?;
        let contract = amend_project_contract(&repository_root, input)?;
        self.record_contract_commit(
            project_id,
            &repository_root,
            &format!("{idempotency_key}:contract-commit"),
        )?;
        let command_ids: Vec<Value> = contract
            .pointer("/execution/commands")
            .and_then(Value::as_array)
            .map(|commands| {
                commands
                    .iter()
                    .map(|command| command["commandId"].clone())
                    .collect()
            })
            .unwrap_or_default();
        self.append_draft(&event_draft(
            "project.contract_amended",
            "user",
            project_scope(project_id),
            json!(idempotency_key),
            Value::Null,
            json!({ "contractVersion": contract["contractVersion"], "commandIds": command_ids }),
        ))?;
        Ok(contract)
    }

    /// Approved contract files are committed at once so the next task preflight sees a clean
    /// checkout; the outcome is an event either way.
    fn record_contract_commit(
        &self,
        project_id: &str,
        repository_root: &Path,
        correlation_id: &str,
    ) -> Result<()> {
        let outcome = commit_project_contract(repository_root)?;
        let kind = if outcome["committed"] == true {
            "project.contract_committed"
        } else {
            "project.contract_commit_skipped"
        };
        self.append_draft(&event_draft(
            kind,
            "noshd",
            project_scope(project_id),
            json!(correlation_id),
            Value::Null,
            outcome,
        ))?;
        Ok(())
    }

    pub fn notification_acks(&self, project_id: &str) -> Result<Vec<String>> {
        let mut acknowledged: Vec<String> = Vec::new();
        for event in self.store_for(project_id)?.replay(project_id, 0)? {
            if event["type"] != "notification.acknowledged" {
                continue;
            }
            for id in event
                .pointer("/payload/eventIds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = id.as_str()
                    && !acknowledged.iter().any(|known| known == id)
                {
                    acknowledged.push(id.to_owned());
                }
            }
        }
        Ok(acknowledged)
    }

    pub fn acknowledge_notifications(
        &self,
        project_id: &str,
        event_ids: &[String],
        idempotency_key: &str,
    ) -> Result<Value> {
        self.assert_project_writable(project_id)?;
        let unique: HashSet<&String> = event_ids.iter().collect();
        if event_ids.is_empty() || unique.len() != event_ids.len() {
            return rejected("Notification acknowledgement requires unique event IDs");
        }
        let mut canonical = event_ids.to_vec();
        canonical.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let store = self.store_for(project_id)?;
        if let Some(prior) = store.command_receipt(project_id, idempotency_key)? {
            let event = &prior["event"];
            let prior_ids = event
                .pointer("/payload/eventIds")
                .filter(|ids| ids.is_array());
            if event["type"] != "notification.acknowledged"
                || event["source"] != "noshd"
                || event.pointer("/scope/projectId").and_then(Value::as_str) != Some(project_id)
                || prior_ids != Some(&json!(canonical))
            {
                return rejected(
                    "Notification acknowledgement idempotency key was reused for a different Project or event set",
                );
            }
            return Ok(event.clone());
        }
        let known: HashSet<String> = store
            .replay(project_id, 0)?
            .iter()
            .map(|event| string(event.get("eventId")))
            .collect();
        if canonical.iter().any(|id| !known.contains(id)) {
            return rejected("Notification acknowledgement references an unknown Project event");
        }
        let stored = store.append_idempotent(
            idempotency_key,
            &event_draft(
                "notification.acknowledged",
                "noshd",
                project_scope(project_id),
                Value::Null,
                Value::Null,
                json!({ "eventIds": canonical }),
            ),
        )?;
        let event = stored.receipt["event"].clone();
        if !stored.replayed {
            self.core.publish(&event);
        }
        Ok(event)
    }

    fn session_options(
        &self,
        project_id: &str,
        agent_id: String,
        cwd: PathBuf,
        selection: Option<&Value>,
    ) -> SessionOptions {
        let text = |key: &str| {
            selection
                .and_then(|selection| selection.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        SessionOptions {
            project_id: project_id.to_owned(),
            mission_id: None,
            direction_id: None,
            autoresearch_id: None,
            experiment_id: None,
            run_id: None,
            job_id: None,
            task_id: None,
            agent_id,
            role: Role::Nosh,
            cwd,
            package_path: self.options.package_path.clone(),
            tools: None,
            runtime_scoped: false,
            task_permissions: None,
            task_workspace: None,
            thinking_level: selection.and(text("thinkingLevel")),
            model: selection.map(|_| ModelRef {
                provider: text("provider").unwrap_or_default(),
                id: text("id").unwrap_or_default(),
            }),
        }
    }

    pub async fn begin_project_intake(
        &self,
        project_id: &str,
        selection: Option<&Value>,
    ) -> Result<()> {
        let repository_root = self.repository_root(project_id)?;
        self.validate_model_selection(selection).await?;
        let contract = self.project_contract(project_id)?;
        if nosh_wire::js::truthy(contract.get("approvedAt")) {
            return Ok(());
        }
        let options = self.session_options(
            project_id,
            nosh_wire::create_id(IdPrefix::Agt),
            repository_root.clone(),
            selection,
        );
        let agent = self.start_agent(options).await?;
        self.agents
            .prompt(
                &agent.agent_id,
                &intake_prompt(&contract, &repository_root, &self.options.schema_dir),
                None,
            )
            .await
            .map_err(Error::Rejected)
    }

    pub fn projects(&self) -> Result<Vec<Value>> {
        self.core.projects()
    }

    pub fn backups(&self, project_id: &str) -> Result<Vec<Value>> {
        if !self
            .projects()?
            .iter()
            .any(|project| project["projectId"] == project_id)
        {
            return rejected("Project is not registered on this host");
        }
        let backups = list_project_backups(&self.options.data_directory, project_id)?;
        Ok(backups
            .iter()
            .map(|backup| serde_json::to_value(backup).unwrap_or(Value::Null))
            .collect())
    }

    pub fn assert_project_writable(&self, project_id: &str) -> Result<()> {
        self.core.assert_writable(project_id)
    }

    pub fn backup_maintenance_in_progress(&self) -> bool {
        !self.core.maintenance.borrow().is_empty()
    }

    fn registered(&self, project_id: &str) -> Result<RegisteredProject> {
        let project = self
            .projects()?
            .into_iter()
            .find(|project| project["projectId"] == project_id)
            .ok_or_else(|| Error::Rejected("Project is not registered on this host".into()))?;
        serde_json::from_value(project).map_err(|error| Error::Rejected(error.to_string()))
    }

    pub fn create_backup(&self, project_id: &str) -> Result<Value> {
        let project = self.registered(project_id)?;
        self.assert_project_writable(project_id)?;
        self.core
            .maintenance
            .borrow_mut()
            .insert(project_id.to_owned());
        self.stop_supervisors();
        let result = (|| -> Result<Value> {
            if let Some(active) = self.backup_activity_reason(project_id)? {
                return rejected(active);
            }
            let summary = create_project_backup(&self.options.data_directory, &project)?;
            Ok(serde_json::to_value(summary).unwrap_or(Value::Null))
        })();
        self.core.maintenance.borrow_mut().remove(project_id);
        if self.started.get() && self.core.maintenance.borrow().is_empty() {
            self.start_supervisors();
        }
        result
    }

    pub fn schedule_backup_restore(&self, project_id: &str, backup_id: &str) -> Result<()> {
        let project = self.registered(project_id)?;
        if let Some(active) = self.backup_activity_reason(project_id)? {
            return rejected(format!(
                "Restore rejected: {}",
                &active["Backup rejected: ".len()..]
            ));
        }
        self.assert_project_writable(project_id)?;
        Ok(schedule_project_restore(
            &self.options.data_directory,
            &project,
            backup_id,
        )?)
    }

    pub async fn start_agent(&self, options: SessionOptions) -> Result<Inspection> {
        if !self
            .projects()?
            .iter()
            .any(|project| project["projectId"] == options.project_id.as_str())
        {
            return rejected(format!(
                "Project {} is not registered on this host",
                options.project_id
            ));
        }
        self.assert_project_writable(&options.project_id)?;
        self.agents.start(options).await.map_err(Error::Rejected)
    }

    pub async fn validate_model_selection(&self, selection: Option<&Value>) -> Result<()> {
        self.agents
            .validate_model_selection(selection)
            .await
            .map_err(Error::Rejected)
    }

    pub async fn chat(
        self: &Rc<Self>,
        project_id: &str,
        message: &str,
        idempotency_key: &str,
        selection: Option<&Value>,
    ) -> Result<ChatReceipt> {
        let text = js_trim(message);
        if text.is_empty() || text.len() > 100_000 {
            return rejected("Chat message must contain 1 to 100000 UTF-8 bytes");
        }
        let repository_root = self.repository_root(project_id)?;
        self.assert_project_writable(project_id)?;
        let store = self.store_for(project_id)?;
        if let Some(prior) = store.command_receipt(project_id, idempotency_key)? {
            let event = &prior["event"];
            let payload = &event["payload"];
            let agent_id = payload.get("agentId").and_then(Value::as_str);
            let prior_selection = match payload.get("selection") {
                None => Value::Null,
                Some(selection) => selection.clone(),
            };
            if event["type"] != "chat.user_message"
                || event["source"] != "user"
                || event.pointer("/scope/projectId").and_then(Value::as_str) != Some(project_id)
                || !payload.is_object()
                || payload["message"] != text
                || agent_id.is_none()
                || canonical_json(&prior_selection) != canonical_json(&chat_selection(selection))
            {
                return rejected(
                    "Chat idempotency key was reused for a different Project, message, or model selection",
                );
            }
            return Ok(ChatReceipt {
                event: event.clone(),
                agent_id: agent_id.unwrap_or_default().to_owned(),
                replayed: true,
            });
        }
        // Reject an invalid active contract before changing a session or persisting chat.
        // Completed requests still replay above, even if the contract later changes.
        let intake = self.project_contract(project_id)?;
        self.validate_model_selection(selection).await?;
        let mut agent = self
            .agents
            .inspect()
            .into_iter()
            .find(|agent| agent.project_id == project_id && agent.role == Role::Nosh);
        // A session started before any model account was connected has no model; start afresh so
        // it picks up `nosh login`.
        if let Some(current) = &agent
            && current.status != "running"
            && current
                .model_id
                .as_deref()
                .is_none_or(|model| model.is_empty() || model == "unknown")
        {
            self.agents.stop(&current.agent_id);
            agent = None;
        }
        if let (Some(current), Some(selection)) = (&agent, selection) {
            let same_model = current.model_provider.as_deref()
                == selection.get("provider").and_then(Value::as_str)
                && current.model_id.as_deref() == selection.get("id").and_then(Value::as_str);
            if !same_model {
                if current.status == "running" {
                    return rejected("Stop the current Nosh turn before changing its Pi model");
                }
                self.agents.stop(&current.agent_id);
                agent = None;
            } else if let Some(level) = selection.get("thinkingLevel").and_then(Value::as_str)
                && level != current.thinking_level
            {
                self.agents
                    .set_thinking_level(&current.agent_id, level)
                    .await
                    .map_err(Error::Rejected)?;
            }
        }
        let fresh = agent.is_none();
        let agent = match agent {
            Some(agent) => agent,
            None => {
                let options = self.session_options(
                    project_id,
                    deterministic_chat_agent_id(project_id),
                    repository_root.clone(),
                    selection,
                );
                self.start_agent(options).await?
            }
        };
        let stored = store.append_idempotent(
            idempotency_key,
            &event_draft("chat.user_message", "user", project_scope(project_id), json!(idempotency_key), Value::Null, json!({ "message": text, "selection": chat_selection(selection), "agentId": agent.agent_id })),
        )?;
        let event = stored.receipt["event"].clone();
        if stored.replayed {
            return Ok(ChatReceipt {
                event,
                agent_id: agent.agent_id,
                replayed: true,
            });
        }
        self.core.publish(&event);
        // A fresh Pi session (daemon restart, model change) has no memory: restore the recent
        // conversation so discovery and chat continue where they left off.
        let history = if fresh {
            let before = event
                .get("sequence")
                .and_then(Value::as_f64)
                .unwrap_or(9_007_199_254_740_991.0);
            self.chat_history(project_id, &agent.agent_id, before, 12_000)?
        } else {
            String::new()
        };
        let restored = if history.is_empty() {
            String::new()
        } else {
            format!(
                "Conversation so far in this Project (restored after a session restart; most recent last):\n{history}\n\n"
            )
        };
        let prompt = if fresh && !nosh_wire::js::truthy(intake.get("approvedAt")) {
            format!(
                "{}\n\n{restored}The user's {} Project-discovery response is:\n{text}",
                intake_prompt(&intake, &repository_root, &self.options.schema_dir),
                if history.is_empty() { "first" } else { "next" }
            )
        } else {
            format!("{restored}{text}")
        };
        let daemon = Rc::clone(self);
        let (project, key, agent_id, cause, running) = (
            project_id.to_owned(),
            idempotency_key.to_owned(),
            agent.agent_id.clone(),
            event["eventId"].clone(),
            agent.status == "running",
        );
        spawn_eager(async move {
            let outcome = if running {
                daemon.agents.follow_up(&agent_id, &prompt).await
            } else {
                daemon.agents.prompt(&agent_id, &prompt, None).await
            };
            if let Err(error) = outcome {
                let mut scope = project_scope(&project);
                scope["agentId"] = json!(agent_id);
                let message = if error.is_empty() {
                    "Chat agent failed".to_owned()
                } else {
                    error
                };
                let _ = daemon.append_draft(&event_draft(
                    "agent.failed",
                    "pi",
                    scope,
                    json!(key),
                    cause,
                    json!({ "message": message }),
                ));
            }
        });
        Ok(ChatReceipt {
            event,
            agent_id: agent.agent_id,
            replayed: false,
        })
    }

    /// The Project chat before `before_sequence`, newest turns kept within a character budget.
    fn chat_history(
        &self,
        project_id: &str,
        agent_id: &str,
        before_sequence: f64,
        maximum_characters: usize,
    ) -> Result<String> {
        let turns: Vec<String> = self
            .store_for(project_id)?
            .replay(project_id, 0)?
            .iter()
            .filter(|event| {
                event.get("sequence").and_then(Value::as_f64).unwrap_or(0.0) < before_sequence
                    && event["payload"].is_object()
                    && (event["type"] == "chat.user_message"
                        || (event["type"] == "agent.completed"
                            && event.pointer("/scope/agentId").and_then(Value::as_str)
                                == Some(agent_id)
                            && !nosh_wire::js::truthy(event.get("correlationId"))))
            })
            .map(|event| {
                let speaker = if event["type"] == "chat.user_message" {
                    "User"
                } else {
                    "NOSH"
                };
                let message = match event.pointer("/payload/message") {
                    None | Some(Value::Null) => String::new(),
                    message => string(message),
                };
                format!(
                    "{speaker}: {}",
                    nosh_wire::js::slice_to(js_trim(&message), 2_000)
                )
            })
            .filter(|turn| !turn.ends_with(": "))
            .collect();
        let mut kept: Vec<String> = Vec::new();
        let mut size = 0;
        for turn in turns.into_iter().rev() {
            let length = nosh_wire::js::length(&turn);
            if size + length > maximum_characters {
                break;
            }
            size += length + 1;
            kept.insert(0, turn);
        }
        Ok(kept.join("\n"))
    }

    pub async fn steer_mission(
        &self,
        project_id: &str,
        mission_id: &str,
        expected_version: i64,
        message: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let mission = self.research.assert_inspected_mission_state(
            project_id,
            mission_id,
            expected_version,
        )?;
        let text = js_trim(message);
        if mission["state"] != "running" {
            return rejected("Only a running Mission may be steered");
        }
        if text.is_empty() {
            return rejected("Mission steering message is required");
        }
        // Steering is durable: it reaches any live Director now and every later Director cycle
        // through the Mission brief.
        let directors: Vec<Inspection> = self
            .agents
            .inspect()
            .into_iter()
            .filter(|agent| {
                agent.project_id == project_id
                    && agent.mission_id.as_deref() == Some(mission_id)
                    && matches!(agent.role, Role::MissionDirector | Role::ResearchDirector)
            })
            .collect();
        let steering = directors
            .iter()
            .map(|agent| self.agents.steer(&agent.agent_id, text));
        for result in futures_util::future::join_all(steering).await {
            result.map_err(Error::Rejected)?;
        }
        let mut scope = project_scope(project_id);
        scope["missionId"] = json!(mission_id);
        let ids: Vec<&str> = directors
            .iter()
            .map(|agent| agent.agent_id.as_str())
            .collect();
        self.append_draft(&event_draft(
            "mission.steering_applied",
            "user",
            scope,
            json!(idempotency_key),
            Value::Null,
            json!({ "message": text, "directorAgentIds": ids }),
        ))?;
        Ok(())
    }

    pub async fn control_mission(
        &self,
        project_id: &str,
        mission_id: &str,
        expected_version: i64,
        action: &str,
        mode: &str,
        idempotency_key: &str,
    ) -> Result<Value> {
        let mission = self.research.assert_inspected_mission_state(
            project_id,
            mission_id,
            expected_version,
        )?;
        let version = mission["version"].as_i64().unwrap_or(-1);
        let agents: Vec<Inspection> = self
            .agents
            .inspect()
            .into_iter()
            .filter(|agent| {
                agent.project_id == project_id && agent.mission_id.as_deref() == Some(mission_id)
            })
            .collect();
        let all = |results: Vec<std::result::Result<(), String>>| -> Result<()> {
            results
                .into_iter()
                .collect::<std::result::Result<(), String>>()
                .map_err(Error::Rejected)
        };
        if action == "resume" {
            return Ok(self.research.transition_mission(
                project_id,
                mission_id,
                version,
                "running",
                &format!("{idempotency_key}:running"),
            )?);
        }
        if action == "pause" {
            let pausing = self.research.transition_mission(
                project_id,
                mission_id,
                version,
                "pausing",
                &format!("{idempotency_key}:pausing"),
            )?;
            if mode == "immediate" {
                all(futures_util::future::join_all(
                    agents
                        .iter()
                        .map(|agent| self.agents.abort(&agent.agent_id)),
                )
                .await)?;
            } else {
                all(futures_util::future::join_all(agents.iter().map(|agent| {
                    self.agents.steer(
                        &agent.agent_id,
                        "Pause at the next safe boundary and persist a canonical handoff.",
                    )
                }))
                .await)?;
            }
            if mode == "checkpoint" {
                for job in self
                    .jobs
                    .list()?
                    .iter()
                    .filter(|job| mission_job_matches(job, project_id, mission_id))
                {
                    self.jobs.checkpoint(&job.spec.job_id)?;
                }
            }
            return if mode == "immediate" || agents.is_empty() {
                let pausing_version = pausing["version"].as_i64().unwrap_or(-1);
                Ok(self.research.transition_mission(
                    project_id,
                    mission_id,
                    pausing_version,
                    "paused",
                    &format!("{idempotency_key}:paused"),
                )?)
            } else {
                Ok(pausing)
            };
        }
        let stopping = self.research.transition_mission(
            project_id,
            mission_id,
            version,
            "stopping",
            &format!("{idempotency_key}:stopping"),
        )?;
        self.stop_mission_children(project_id, mission_id, idempotency_key)?;
        for job in self
            .jobs
            .list()?
            .iter()
            .filter(|job| mission_job_matches(job, project_id, mission_id))
        {
            self.jobs.cancel(&job.spec.job_id, "mission_stopped")?;
        }
        all(futures_util::future::join_all(
            agents
                .iter()
                .map(|agent| self.agents.abort(&agent.agent_id)),
        )
        .await)?;
        let stopping_version = stopping["version"].as_i64().unwrap_or(-1);
        Ok(self.research.transition_mission(
            project_id,
            mission_id,
            stopping_version,
            "stopped",
            &format!("{idempotency_key}:stopped"),
        )?)
    }

    /// Stopping a Mission stops the Directions and Autoresearch it spawned, and the Autoresearch
    /// those Directions started.
    fn stop_mission_children(
        &self,
        project_id: &str,
        mission_id: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let directions: Vec<Value> = self
            .research
            .directions(project_id)?
            .into_iter()
            .filter(|direction| {
                direction
                    .pointer("/value/origin/scopeType")
                    .and_then(Value::as_str)
                    == Some("mission")
                    && direction
                        .pointer("/value/origin/scopeId")
                        .and_then(Value::as_str)
                        == Some(mission_id)
            })
            .collect();
        let direction_ids: HashSet<String> = directions
            .iter()
            .map(|direction| string(direction.get("entityId")))
            .collect();
        for direction in &directions {
            if ["draft", "proposed", "active", "paused", "blocked"]
                .contains(&string(direction.get("state")).as_str())
            {
                let id = string(direction.get("entityId"));
                self.research.transition_direction(
                    project_id,
                    &id,
                    direction["version"].as_i64().unwrap_or(-1),
                    "stopped",
                    &format!("{idempotency_key}:child:{id}"),
                )?;
            }
        }
        for run in self.research.autoresearch(project_id)? {
            let value = &run["value"];
            let owned = value.pointer("/origin/scopeId").and_then(Value::as_str)
                == Some(mission_id)
                || value
                    .get("directionId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| direction_ids.contains(id));
            if owned
                && ["running", "paused", "blocked"].contains(&string(run.get("state")).as_str())
            {
                let id = string(run.get("entityId"));
                self.research.transition_autoresearch(
                    project_id,
                    &id,
                    run["version"].as_i64().unwrap_or(-1),
                    "stopped",
                    &format!("{idempotency_key}:child:{id}"),
                )?;
            }
        }
        Ok(())
    }

    pub fn control_job(
        &self,
        project_id: &str,
        job_id: &str,
        action: &str,
        idempotency_key: &str,
    ) -> Result<JobRecord> {
        self.assert_project_writable(project_id)?;
        let job = self.jobs.get(job_id)?;
        if job.spec.project_id != project_id {
            return rejected("Job is outside the requested Project");
        }
        let store = self.store_for(project_id)?;
        let key = format!("job-control:{idempotency_key}");
        let request = json!({ "jobId": job_id, "action": action });
        let prior = store
            .operation_intents(project_id, None)?
            .into_iter()
            .find(|intent| intent["idempotencyKey"] == key.as_str());
        let intent = store.begin_operation(project_id, "job.control", &key, &request)?;
        if intent["state"] == "completed" {
            return serde_json::from_value(intent["result"].clone())
                .map_err(|error| Error::Rejected(error.to_string()));
        }
        if intent["state"] == "failed" {
            return rejected(match intent.get("error") {
                Some(Value::String(error)) => error.clone(),
                _ => "Job control previously failed".to_owned(),
            });
        }
        if prior.is_some() {
            return rejected(
                "Job control outcome is unresolved after interruption; refusing to repeat the action",
            );
        }
        let intent_id = string(intent.get("intentId"));
        let outcome = (|| -> Result<JobRecord> {
            let result = if action == "checkpoint" {
                self.jobs.checkpoint(job_id)?
            } else {
                self.jobs.cancel(job_id, "cancelled_by_user")?
            };
            let spec = &result.spec;
            let scope = json!({ "projectId": project_id, "missionId": spec.mission_id, "directionId": spec.direction_id, "autoresearchId": spec.autoresearch_id, "experimentId": spec.experiment_id, "runId": spec.run_id, "jobId": job_id, "agentId": null });
            let record = serde_json::to_value(&result).unwrap_or(Value::Null);
            let stored = store.append_idempotent(
                &format!("job-control-event:{idempotency_key}"),
                &event_draft(
                    "job.control_completed",
                    "noshd",
                    scope,
                    json!(job_id),
                    Value::Null,
                    json!({ "action": action, "job": record }),
                ),
            )?;
            store.complete_operation(project_id, &intent_id, &record)?;
            if !stored.replayed {
                self.core.publish(&stored.receipt["event"]);
            }
            Ok(result)
        })();
        if let Err(error) = &outcome {
            store.fail_operation(
                project_id,
                &intent_id,
                &error_text(error, "Job control failed"),
            )?;
        }
        outcome
    }

    pub fn start_job(&self, spec: &JobSpec) -> Result<JobRecord> {
        if !self
            .projects()?
            .iter()
            .any(|project| project["projectId"] == spec.project_id.as_str())
        {
            return rejected(format!(
                "Project {} is not registered on this host",
                spec.project_id
            ));
        }
        self.assert_project_writable(&spec.project_id)?;
        assert_job_scope(spec)?;
        let store = self.store_for(&spec.project_id)?;
        let request = serde_json::to_value(spec).unwrap_or(Value::Null);
        let intent = store.begin_operation(
            &spec.project_id,
            "job.launch",
            &format!("job-launch:{}", spec.job_id),
            &request,
        )?;
        if intent["state"] == "completed" {
            let result: JobRecord = serde_json::from_value(intent["result"].clone())
                .map_err(|error| Error::Rejected(error.to_string()))?;
            assert_job_identity(&result, spec, "Stored Job launch result")?;
            return Ok(result);
        }
        if intent["state"] == "failed" {
            return rejected(match intent.get("error") {
                Some(Value::String(error)) => error.clone(),
                _ => "Job launch previously failed".to_owned(),
            });
        }
        self.jobs.start(spec)?;
        let verified = self.jobs.get(&spec.job_id)?;
        assert_job_identity(&verified, spec, "Job launch verification")?;
        store.complete_operation(
            &spec.project_id,
            &string(intent.get("intentId")),
            &serde_json::to_value(&verified).unwrap_or(Value::Null),
        )?;
        Ok(verified)
    }

    /// The default for every new session without an explicit selection; running sessions keep
    /// theirs.
    pub async fn set_default_model(&self, model: &str) -> Result<ModelSelection> {
        let selection = parse_model_selection(model)?;
        let value = serde_json::to_value(&selection).unwrap_or(Value::Null);
        self.validate_model_selection(Some(&value)).await?;
        self.agents.set_default_model(Some(selection.clone()));
        Ok(selection)
    }

    /// What the Project is doing right now, as one tree for visual clients.
    pub fn research_map(&self, project_id: &str) -> Result<Value> {
        if !self
            .projects()?
            .iter()
            .any(|project| project["projectId"] == project_id)
        {
            return rejected(format!(
                "Project {project_id} is not registered on this host"
            ));
        }
        let events = self.store_for(project_id)?.replay(project_id, 0)?;
        let submitted: Vec<(Value, Value)> = events
            .iter()
            .filter(|event| event["type"] == "record.submitted" && event["payload"].is_object())
            .map(|event| (event.clone(), event["payload"].clone()))
            .collect();
        let agents: Vec<Value> = self
            .agents
            .inspect()
            .iter()
            .filter(|agent| agent.project_id == project_id)
            .map(|agent| serde_json::to_value(agent).unwrap_or(Value::Null))
            .collect();
        let jobs: Vec<Value> = self
            .jobs
            .list()?
            .iter()
            .filter(|job| job.spec.project_id == project_id)
            .map(|job| serde_json::to_value(job).unwrap_or(Value::Null))
            .collect();
        let (missions, directions, autoresearch) = (
            self.research.missions(project_id)?,
            self.research.directions(project_id)?,
            self.research.autoresearch(project_id)?,
        );
        Ok(crate::research_map::build_research_map(
            &crate::research_map::ResearchMapInput {
                missions: &missions,
                directions: &directions,
                autoresearch: &autoresearch,
                submitted: &submitted,
                events: &events,
                agents: &agents,
                jobs: &jobs,
            },
        ))
    }

    pub fn replay(&self, project_id: &str, after_sequence: i64) -> Result<Vec<Value>> {
        Ok(self
            .store_for(project_id)?
            .replay(project_id, after_sequence)?)
    }

    pub fn replay_page(
        &self,
        project_id: &str,
        after_sequence: i64,
        limit: i64,
        recent: bool,
    ) -> Result<nosh_store::Page> {
        Ok(self
            .store_for(project_id)?
            .replay_page(project_id, after_sequence, limit, recent)?)
    }

    /// Stores an event draft and delivers it to subscribers.
    pub fn append_draft(&self, draft: &Value) -> Result<Value> {
        self.core.append_draft(draft)
    }

    pub fn store_for(&self, project_id: &str) -> Result<Rc<EventStore>> {
        self.core.store_for(project_id)
    }

    /// Projects with non-terminal agents, Jobs, threads, Missions, Directions, or Autoresearch;
    /// `nosh stop --if-idle` reads this before an update.
    pub fn activity(&self) -> Result<Vec<(String, Vec<String>)>> {
        let mut active = Vec::new();
        for project in self.projects()? {
            let project_id = string(project.get("projectId"));
            let state = self.active_project_state(&project_id)?;
            if !state.is_empty() {
                active.push((project_id, state));
            }
        }
        Ok(active)
    }

    fn backup_activity_reason(&self, project_id: &str) -> Result<Option<String>> {
        let active = self.active_project_state(project_id)?;
        Ok((!active.is_empty()).then(|| {
            format!(
                "Backup rejected: active Project state ({})",
                active.join(", ")
            )
        }))
    }

    fn active_project_state(&self, project_id: &str) -> Result<Vec<String>> {
        *self.core.inspection.borrow_mut() = Some(project_id.to_owned());
        let state = (|| -> Result<Vec<String>> {
            let mut active: Vec<String> = Vec::new();
            for agent in self
                .agents
                .inspect()
                .iter()
                .filter(|agent| agent.project_id == project_id && agent.status != "idle")
            {
                active.push(format!("agent {}:{}", agent.agent_id, agent.status));
            }
            for job in self
                .jobs
                .list()?
                .iter()
                .filter(|job| job.spec.project_id == project_id)
            {
                let state = serde_json::to_value(job.state).unwrap_or(Value::Null);
                if [
                    "queued",
                    "starting",
                    "running",
                    "checkpointing",
                    "finishing",
                ]
                .contains(&string(Some(&state)).as_str())
                {
                    active.push(format!("job {}:{}", job.spec.job_id, string(Some(&state))));
                }
            }
            for thread in self.runtime.threads(project_id)? {
                let state = string(thread.pointer("/value/state"));
                if ["open", "running", "awaiting", "awaiting_user", "paused"]
                    .contains(&state.as_str())
                {
                    active.push(format!(
                        "runtime thread {}:{state}",
                        string(thread.get("entityId"))
                    ));
                }
            }
            let entities: [(&str, Vec<Value>, &[&str]); 3] = [
                (
                    "Mission",
                    self.research.missions(project_id)?,
                    &["draft", "completed", "stopped", "failed"],
                ),
                (
                    "Direction",
                    self.research.directions(project_id)?,
                    &["draft", "closed", "rejected", "stopped"],
                ),
                (
                    "Autoresearch",
                    self.research.autoresearch(project_id)?,
                    &["draft", "completed", "stopped", "failed"],
                ),
            ];
            for (label, entries, idle) in entities {
                for entry in entries {
                    let state = string(entry.get("state"));
                    if !idle.contains(&state.as_str()) {
                        active.push(format!("{label} {}:{state}", string(entry.get("entityId"))));
                    }
                }
            }
            Ok(active)
        })();
        *self.core.inspection.borrow_mut() = None;
        state
    }

    fn reconcile_project(self: &Rc<Self>, project: &Value) -> Result<()> {
        let project_id = string(project.get("projectId"));
        let store = self.store_for(&project_id)?;
        let papers = self.research.reconcile_pending_operations(&project_id)?;
        let domain_effects = self
            .research
            .reconcile_accepted_domain_effects(&project_id)?;
        let (recovered, rejected_count) = self.recover_delegations(&project_id)?;
        self.reconcile_delegation_reservations(&project_id)?;
        let mut jobs = 0;
        for intent in store.operation_intents(&project_id, Some("pending"))? {
            if intent["operationType"] != "job.launch" {
                continue;
            }
            let spec: JobSpec = serde_json::from_value(intent["request"].clone())
                .map_err(|error| Error::Rejected(error.to_string()))?;
            self.jobs.start(&spec)?;
            let verified = self.jobs.get(&spec.job_id)?;
            assert_job_identity(&verified, &spec, "Recovered Job launch")?;
            store.complete_operation(
                &project_id,
                &string(intent.get("intentId")),
                &serde_json::to_value(&verified).unwrap_or(Value::Null),
            )?;
            jobs += 1;
        }
        let runtime: Vec<Value> = store
            .operation_intents(&project_id, Some("pending"))?
            .into_iter()
            .filter(|intent| intent["operationType"] == "runtime.instruction")
            .collect();
        for intent in &runtime {
            let daemon = Rc::clone(self);
            let (project, intent) = (project_id.clone(), intent.clone());
            let effect = self
                .execute_runtime_effect(&string(intent.get("operationType")), &intent["request"]);
            tokio::task::spawn_local(async move {
                let intent_id = string(intent.get("intentId"));
                let Ok(store) = daemon.store_for(&project) else {
                    return;
                };
                match effect.await {
                    Ok(result) => {
                        let _ = store.complete_operation(&project, &intent_id, &result);
                    }
                    Err(error) => {
                        let message = error_text(&error, "Runtime recovery failed");
                        let _ = store.fail_operation(&project, &intent_id, &message);
                        let _ = daemon.append_draft(&event_draft(
                            "recovery.runtime_failed",
                            "noshd",
                            project_scope(&project),
                            json!(intent_id),
                            Value::Null,
                            json!({ "operationType": intent["operationType"], "message": message }),
                        ));
                    }
                }
            });
        }
        let programs: Vec<String> = self
            .runtime
            .program_states(&project_id)?
            .iter()
            .filter(|state| {
                state.pointer("/value/status").and_then(Value::as_str) == Some("running")
            })
            .map(|state| string(state.get("entityId")))
            .collect();
        for program_id in &programs {
            let daemon = Rc::clone(self);
            let project = project_id.clone();
            let program_id = program_id.clone();
            let run = self.runtime.run_program(&project_id, &program_id);
            tokio::task::spawn_local(async move {
                if let Err(error) = run.await {
                    let message = error_text(&error, "Program recovery failed");
                    let _ = daemon.append_draft(&event_draft(
                        "recovery.program_failed",
                        "noshd",
                        project_scope(&project),
                        json!(program_id),
                        Value::Null,
                        json!({ "message": message }),
                    ));
                }
            });
        }
        let unresolved: Vec<Value> = store
            .operation_intents(&project_id, Some("pending"))?
            .iter()
            .map(|intent| json!({ "intentId": intent["intentId"], "operationType": intent["operationType"] }))
            .collect();
        self.append_draft(&event_draft(
            "recovery.report",
            "noshd",
            project_scope(&project_id),
            Value::Null,
            Value::Null,
            json!({ "reconciledPaperOperations": papers, "recoveredDomainEffects": domain_effects["recovered"], "failedDomainEffects": domain_effects["failed"], "recoveredDelegations": recovered, "rejectedDelegations": rejected_count, "reconciledJobLaunches": jobs, "resumingRuntimeInstructions": runtime.len(), "resumingPrograms": programs.len(), "unresolvedIntents": unresolved }),
        ))?;
        Ok(())
    }
}

/// `parseModelSelection`: `provider/id` or `provider/id:thinkingLevel`
/// (`/^([^/\s]+)\/([^:\s]+)(?::([a-z]+))?$/` on the trimmed text).
pub fn parse_model_selection(model: &str) -> Result<ModelSelection> {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^([^/\s]+)/([^:\s]+)(?::([a-z]+))?$").expect("regex")
    });
    let found = PATTERN.captures(js_trim(model)).ok_or_else(|| {
        Error::Rejected(format!(
            "Model must be provider/id or provider/id:thinkingLevel, got \"{model}\""
        ))
    })?;
    let thinking_level = found.get(3).map(|level| level.as_str().to_owned());
    if thinking_level.as_deref().is_some_and(|level| {
        !["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&level)
    }) {
        return rejected("Invalid thinking level");
    }
    Ok(ModelSelection {
        provider: found[1].to_owned(),
        id: found[2].to_owned(),
        thinking_level,
    })
}
