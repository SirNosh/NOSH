//! `PiAdapter`, as `packages/pi-adapter/src/index.ts`: Pi agent sessions, run by the Node sidecar
//! `sidecar/pi-host/host.mjs` (which owns every Pi object), driven from here (which owns the
//! system prompt, tools, permissions, terminal turns and NOSH events).
//!
//! The sidecar speaks JSON lines on stdio; see the host's header. One reader task applies its
//! events and replies in stream order, so an `agent_end` is recorded before the reply of the
//! `prompt` that caused it; tool calls from the sidecar run on their own tasks.
//!
//! Divergences: `inspect()` reports the session state as of the sidecar's latest event or reply,
//! not as of the call; and model-selection checks run before the sidecar loads the session's
//! resources, where TypeScript runs them after.

use crate::async_calls::{ASYNC_HEARTBEAT, AsyncToolCalls, BoxFuture};
use crate::policy::{self, Role, SUBMISSION_TOOLS, SessionOptions};
use crate::prompt::{SessionFacts, nosh_system_prompt};
use crate::terminal::{
    PromptOutcome, TerminalContext, TerminalHost, is_terminal_tool, run_terminal_turn,
    terminal_prompt,
};
use crate::tools::{AgentHost, SessionTools, event_base};
use nosh_wire::{budget_tokens, normalize_numbers, now_utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Where the sidecar is and how to run it.
#[derive(Debug, Clone)]
pub struct PiConfig {
    /// The Node executable.
    pub node: PathBuf,
    /// `sidecar/pi-host/host.mjs`.
    pub host_script: PathBuf,
    /// Extra environment for the sidecar (`NOSH_PI_HOST_TEST=1` enables its test hooks).
    pub env: Vec<(String, String)>,
    /// The record JSON Schemas the submission tool descriptions point at.
    pub schema_dir: PathBuf,
    /// Echo the sidecar's stderr as `[pi-host] …` lines (the daemon log). The CLI turns this off,
    /// as Pi in-process printed nothing between its prompts; either way a sidecar that exits is
    /// reported with its last stderr line.
    pub forward_stderr: bool,
}

/// The Node that runs the sidecar: `NOSH_NODE` (set by the release's launcher stub to the Node
/// running it) when set and non-empty, else `node` on PATH.
pub fn node_executable() -> PathBuf {
    std::env::var_os("NOSH_NODE")
        .filter(|node| !node.is_empty())
        .map_or_else(|| "node".into(), PathBuf::from)
}

/// Records an event draft.
pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

/// `ModelSelection` from `@nosh/wire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

/// The step of a runtime terminal turn (`TerminalStep`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStep {
    pub instruction_id: Option<String>,
    pub thread_id: Option<String>,
    pub expected_version: Option<f64>,
    pub expected_episode_type: String,
}

/// `AgentInspection`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub project_id: String,
    pub mission_id: Option<String>,
    pub direction_id: Option<String>,
    pub autoresearch_id: Option<String>,
    pub experiment_id: Option<String>,
    pub run_id: Option<String>,
    pub job_id: Option<String>,
    pub task_id: Option<String>,
    pub agent_id: String,
    pub role: Role,
    pub pi_session_id: String,
    /// `idle`, `running`, `compacting` or `aborting`.
    pub status: String,
    pub current_tool: Option<String>,
    pub active_tool_ids: Vec<String>,
    pub started_at: String,
    pub last_event_at: String,
    pub model_provider: Option<String>,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub thinking_level: String,
    pub context_tokens: Option<f64>,
    pub context_window: Option<f64>,
    pub context_percent: Option<f64>,
}

/// The sidecar's session snapshot, attached to its persistent events and replies.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionState {
    active_tool_ids: Vec<String>,
    model_provider: Option<String>,
    model_id: Option<String>,
    model_name: Option<String>,
    thinking_level: String,
    context_tokens: Option<f64>,
    context_window: Option<f64>,
    context_percent: Option<f64>,
}

/// Cumulative token usage of a session's assistant messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub total_tokens: f64,
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cache_read_tokens: f64,
    pub cache_write_tokens: f64,
}

/// A login flow's prompts and progress, as Pi's `AuthInteraction`.
pub trait AuthInteraction: Send + Sync {
    /// Answers one `AuthPrompt`; `signal` is its `signal`, which cannot cross the sidecar protocol
    /// as an object.
    fn prompt(&self, prompt: Value, signal: AuthSignal) -> BoxFuture<Result<String, String>>;
    fn notify(&self, event: Value);
}

/// `AuthPrompt.signal`: aborted when Pi abandons the prompt (the sidecar's `auth.cancel`), as an
/// OAuth flow does to its manual-code prompt once the browser callback wins the race.
#[derive(Clone, Default)]
pub struct AuthSignal(Arc<AtomicBool>);

impl AuthSignal {
    pub fn aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `AbortController.abort()`.
    pub fn abort(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub struct PiAdapter {
    rpc: Arc<Rpc>,
    shared: Arc<Shared>,
}

struct Shared {
    emit: Emit,
    host: Option<Arc<dyn AgentHost>>,
    terminal: Option<Arc<dyn TerminalHost>>,
    schema_dir: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Managed>>>,
    model_usage: Mutex<HashMap<String, ModelUsage>>,
    /// NOSH-owned default for sessions without an explicit model; Pi's shared global settings can
    /// be rewritten by other Pi clients.
    default_model: Mutex<Option<ModelSelection>>,
    login: Mutex<Option<Arc<dyn AuthInteraction>>>,
    /// The sign-in prompts awaiting an answer, by the sidecar's request id.
    auth_prompts: Mutex<HashMap<String, AuthSignal>>,
}

struct Managed {
    options: Mutex<SessionOptions>,
    inspection: Mutex<Inspection>,
    state: Mutex<SessionState>,
    tools: Option<Arc<SessionTools>>,
    async_calls: Arc<AsyncToolCalls>,
    prompt_active: AtomicBool,
    cancelled: AtomicBool,
    terminal_turn: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PiAdapter {
    /// Starts the sidecar. `host` handles submissions (no session tools without it); `terminal`
    /// receives the records of scoped terminal turns.
    pub async fn spawn(
        config: PiConfig,
        emit: Emit,
        host: Option<Arc<dyn AgentHost>>,
        terminal: Option<Arc<dyn TerminalHost>>,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            emit,
            host,
            terminal,
            schema_dir: config.schema_dir.clone(),
            sessions: Mutex::new(HashMap::new()),
            model_usage: Mutex::new(HashMap::new()),
            default_model: Mutex::new(None),
            login: Mutex::new(None),
            auth_prompts: Mutex::new(HashMap::new()),
        });
        let rpc = Rpc::spawn(&config, Arc::downgrade(&shared))?;
        Ok(Self { rpc, shared })
    }

    pub fn default_model(&self) -> Option<ModelSelection> {
        lock(&self.shared.default_model).clone()
    }

    pub fn set_default_model(&self, selection: Option<ModelSelection>) {
        *lock(&self.shared.default_model) = selection;
    }

    /// `AvailableModel[]`: the models Pi can use with the connected accounts.
    pub async fn available_models(&self) -> Result<Value, String> {
        self.rpc.call("availableModels", json!({})).await
    }

    pub async fn validate_model_selection(&self, selection: Option<&Value>) -> Result<(), String> {
        let Some(selection) = selection else {
            return Ok(());
        };
        self.rpc
            .call(
                "resolveModel",
                json!({ "selection": checked_selection(selection)? }),
            )
            .await
            .map(drop)
    }

    pub async fn start(&self, options: SessionOptions) -> Result<Inspection, String> {
        policy::assert_enforceable_task_permissions(&options)?;
        if let Some(existing) = self.managed(&options.agent_id) {
            if *lock(&existing.options) != options {
                return Err(format!(
                    "Agent {} already has a different Pi session scope",
                    options.agent_id
                ));
            }
            return Ok(lock(&existing.inspection).clone());
        }
        let code_mode = policy::code_mode_enabled();
        let facts = SessionFacts {
            project_id: &options.project_id,
            agent_id: &options.agent_id,
            task_id: options.task_id.as_deref(),
        };
        let system_prompt = nosh_system_prompt(options.role, Some(&facts), code_mode);
        // An explicit selection wins; otherwise NOSH's own default; Pi's global settings are the last resort.
        let filled = |level: Option<String>| level.filter(|level| !level.is_empty());
        let selection = match (&options.model, self.default_model()) {
            (Some(model), _) => Some(ModelSelection {
                provider: model.provider.clone(),
                id: model.id.clone(),
                thinking_level: filled(options.thinking_level.clone()),
            }),
            (None, Some(default)) => Some(ModelSelection {
                thinking_level: filled(options.thinking_level.clone().or(default.thinking_level)),
                ..default
            }),
            (None, None) => None,
        };
        let selection = match &selection {
            Some(selection) => Some(checked_selection(&json!(selection))?),
            None => None,
        };
        let async_calls = Arc::new(AsyncToolCalls::default());
        let tools = self.shared.host.as_ref().map(|host| {
            let host: Arc<dyn AgentHost> = Arc::new(ToolHost {
                host: Arc::clone(host),
                emit: Arc::clone(&self.shared.emit),
            });
            SessionTools::new(
                options.clone(),
                host,
                Arc::clone(&async_calls),
                &self.shared.schema_dir,
                code_mode,
            )
        });
        let definitions: Vec<Value> = tools.as_ref().map_or_else(Vec::new, |tools| {
            tools
                .definitions()
                .iter()
                .map(|tool| json!({ "name": tool.name, "label": tool.label, "description": tool.description, "parameters": typebox_json(&tool.parameters) }))
                .collect()
        });
        let started = self
            .rpc
            .call(
                "start",
                json!({
                    "agentId": options.agent_id, "cwd": options.cwd, "packagePath": options.package_path,
                    "systemPrompt": system_prompt, "cacheKey": policy::nosh_prompt_cache_key(&options), "tools": definitions,
                    "activeToolIds": policy::execution_tool_ids(&options, code_mode), "selection": selection, "thinkingLevel": options.thinking_level,
                }),
            )
            .await?;
        let state: SessionState = parse(&started["state"])?;
        let timestamp = now_utc();
        let inspection = Inspection {
            project_id: options.project_id.clone(),
            mission_id: options.mission_id.clone(),
            direction_id: options.direction_id.clone(),
            autoresearch_id: options.autoresearch_id.clone(),
            experiment_id: options.experiment_id.clone(),
            run_id: options.run_id.clone(),
            job_id: options.job_id.clone(),
            task_id: options.task_id.clone(),
            agent_id: options.agent_id.clone(),
            role: options.role,
            pi_session_id: started["piSessionId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            status: "idle".into(),
            current_tool: None,
            active_tool_ids: state.active_tool_ids.clone(),
            started_at: timestamp.clone(),
            last_event_at: timestamp,
            model_provider: state.model_provider.clone(),
            model_id: state.model_id.clone(),
            model_name: state.model_name.clone(),
            thinking_level: state.thinking_level.clone(),
            context_tokens: None,
            context_window: started["contextWindow"].as_f64(),
            context_percent: None,
        };
        // An async result that lands mid-run is steered in before the model's next call; otherwise it waits for the wake loop.
        let (rpc, agent_id) = (Arc::downgrade(&self.rpc), options.agent_id.clone());
        async_calls.set_deliver(move |text| {
            let (rpc, agent_id) = (rpc.clone(), agent_id.clone());
            Box::pin(async move {
                let Some(rpc) = rpc.upgrade() else {
                    return false;
                };
                rpc.call(
                    "steerIfStreaming",
                    json!({ "agentId": agent_id, "text": text }),
                )
                .await
                .is_ok_and(|steered| steered == json!(true))
            })
        });
        let managed = Arc::new(Managed {
            options: Mutex::new(options.clone()),
            inspection: Mutex::new(inspection.clone()),
            state: Mutex::new(state),
            tools,
            async_calls,
            prompt_active: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            terminal_turn: Mutex::new(None),
        });
        lock(&self.shared.sessions).insert(options.agent_id, managed);
        Ok(inspection)
    }

    pub async fn prompt(
        &self,
        agent_id: &str,
        prompt: &str,
        step: Option<&TerminalStep>,
    ) -> Result<(), String> {
        let managed = self.required(agent_id)?;
        if managed.prompt_active.swap(true, Ordering::SeqCst) {
            return Err("A Pi prompt is already active for this agent".into());
        }
        managed.cancelled.store(false, Ordering::SeqCst);
        lock(&managed.inspection).status = "running".into();
        let result = self
            .terminal_or_plain_prompt(&managed, agent_id, prompt, step)
            .await;
        managed.prompt_active.store(false, Ordering::SeqCst);
        *lock(&managed.terminal_turn) = None;
        lock(&managed.inspection).status = "idle".into();
        result.map_err(|error| policy::user_facing_pi_error(&error))
    }

    async fn terminal_or_plain_prompt(
        &self,
        managed: &Arc<Managed>,
        agent_id: &str,
        prompt: &str,
        step: Option<&TerminalStep>,
    ) -> Result<(), String> {
        let options = lock(&managed.options).clone();
        if !options.has_task() && !options.runtime_scoped && step.is_none() {
            return self
                .session_prompt(managed, agent_id, prompt)
                .await
                .map(drop);
        }
        let Some(terminal) = &self.shared.terminal else {
            return Err("Structured terminal output requires a host receipt service".into());
        };
        let permitted = policy::session_tool_ids(&options).unwrap_or_else(|| {
            SUBMISSION_TOOLS
                .iter()
                .map(|tool| (*tool).to_owned())
                .collect()
        });
        let mut allowed_tools: Vec<String> = permitted
            .into_iter()
            .filter(|tool| {
                is_terminal_tool(tool)
                    && options
                        .tools
                        .as_ref()
                        .is_none_or(|tools| tools.contains(tool))
            })
            .collect();
        // A runtime step must return one episode draft; without this the terminal contract is unsatisfiable.
        if step.is_some()
            && !allowed_tools
                .iter()
                .any(|tool| tool == "nosh_episode_submit")
        {
            allowed_tools.push("nosh_episode_submit".into());
        }
        let task_label = options.task_id.as_deref().unwrap_or("null");
        let context = TerminalContext {
            project_id: options.project_id.clone(),
            agent_id: agent_id.to_owned(),
            task_id: options.task_id.clone(),
            instruction_id: step.and_then(|step| step.instruction_id.clone()),
            thread_id: step.and_then(|step| step.thread_id.clone()),
            expected_version: step.and_then(|step| step.expected_version),
            turn_id: step
                .and_then(|step| step.instruction_id.clone())
                .unwrap_or_else(|| format!("task:{task_label}")),
            allowed_tools,
            expected_episode_type: step.map(|step| step.expected_episode_type.clone()),
        };
        *lock(&managed.terminal_turn) = Some(context.turn_id.clone());
        let terminal_text = terminal_prompt(&context);
        let receipt = run_terminal_turn(&context, terminal.as_ref(), |correction| {
            let (managed, terminal_text) = (Arc::clone(managed), terminal_text.clone());
            let text = format!(
                "{}\n\n{terminal_text}",
                correction.as_deref().unwrap_or(prompt)
            );
            async move {
                self.terminal_run(&managed, agent_id, text, correction.is_some())
                    .await
            }
        })
        .await?;
        let mut event = event_base(&options);
        event.insert("correlationId".into(), json!(context.turn_id));
        event.insert("retention".into(), json!("persistent"));
        event.insert("type".into(), json!("agent.terminal_receipt"));
        event.insert("payload".into(), receipt.clone());
        (self.shared.emit)(Value::Object(event));
        if receipt.pointer("/effect/state") == Some(&json!("failed")) {
            return Err("Terminal records accepted but host effects failed".into());
        }
        Ok(())
    }

    /// One provider run of a terminal turn, through the wake loop: a run that ends while async
    /// calls are in flight sleeps until their results wake it; the turn's answer is the last run's.
    async fn terminal_run(
        &self,
        managed: &Arc<Managed>,
        agent_id: &str,
        text: String,
        correction: bool,
    ) -> Result<PromptOutcome, String> {
        let active = if correction {
            Some(
                self.rpc
                    .call(
                        "setActiveTools",
                        json!({ "agentId": agent_id, "names": [] }),
                    )
                    .await?["previous"]
                    .take(),
            )
        } else {
            None
        };
        let mut last = None;
        let outcome = async {
            if let Some(answer) = self.session_prompt(managed, agent_id, &text).await? {
                last = Some(answer);
            }
            while let Some(wake) = self.next_wake(managed, agent_id).await? {
                if managed.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                if !wake.is_empty()
                    && let Some(answer) = self.session_prompt(managed, agent_id, &wake).await?
                {
                    last = Some(answer);
                }
            }
            Ok::<_, String>(())
        }
        .await;
        // The restored tools reach inspect() now, not at the session's next event.
        if let Some(names) = active {
            let restored = self
                .rpc
                .call(
                    "setActiveTools",
                    json!({ "agentId": agent_id, "names": names }),
                )
                .await?;
            *lock(&managed.state) = parse(&restored["state"])?;
        }
        outcome?;
        // An agent_end with willRetry leaves no answer; "missing" then, as with no agent_end.
        let answer = last.unwrap_or(Value::Null);
        let current = self.managed(agent_id);
        Ok(PromptOutcome {
            text: answer["text"].as_str().unwrap_or_default().to_owned(),
            stop_reason: answer["stopReason"]
                .as_str()
                .unwrap_or("missing")
                .to_owned(),
            cancelled: managed.cancelled.load(Ordering::SeqCst)
                || !current.is_some_and(|current| Arc::ptr_eq(&current, managed)),
        })
    }

    /// One `session.prompt`; the final answer when it ended an agent run.
    async fn session_prompt(
        &self,
        managed: &Managed,
        agent_id: &str,
        text: &str,
    ) -> Result<Option<Value>, String> {
        let reply = self
            .rpc
            .call("prompt", json!({ "agentId": agent_id, "text": text }))
            .await?;
        if let Ok(state) = parse(&reply["state"]) {
            *lock(&managed.state) = state;
        }
        Ok(reply.get("final").cloned())
    }

    /// The next message that wakes a sleeping task run, recovering any result steered into a run
    /// that ended first.
    async fn next_wake(&self, managed: &Managed, agent_id: &str) -> Result<Option<String>, String> {
        let stranded = self
            .rpc
            .call("clearQueue", json!({ "agentId": agent_id }))
            .await?;
        let texts: Vec<String> = ["steering", "followUp"]
            .iter()
            .flat_map(|key| stranded[*key].as_array().cloned().unwrap_or_default())
            .filter_map(|text| text.as_str().map(str::to_owned))
            .collect();
        managed.async_calls.requeue(&texts);
        Ok(managed.async_calls.next(ASYNC_HEARTBEAT).await)
    }

    pub fn terminal_turn_active(&self, context: &TerminalContext) -> bool {
        self.managed(&context.agent_id).is_some_and(|managed| {
            managed.prompt_active.load(Ordering::SeqCst)
                && !managed.cancelled.load(Ordering::SeqCst)
                && lock(&managed.terminal_turn).as_deref() == Some(context.turn_id.as_str())
                && lock(&managed.options).project_id == context.project_id
        })
    }

    pub async fn steer(&self, agent_id: &str, message: &str) -> Result<(), String> {
        self.required(agent_id)?;
        self.rpc
            .call("steer", json!({ "agentId": agent_id, "text": message }))
            .await
            .map(drop)
    }

    pub async fn follow_up(&self, agent_id: &str, message: &str) -> Result<(), String> {
        let managed = self.required(agent_id)?;
        let scoped = {
            let options = lock(&managed.options);
            options.has_task() || options.runtime_scoped
        };
        if scoped {
            return Err("Scoped terminal turns cannot queue Pi follow-ups; issue a separate host instruction".into());
        }
        self.rpc
            .call("followUp", json!({ "agentId": agent_id, "text": message }))
            .await
            .map(drop)
    }

    pub async fn set_thinking_level(
        &self,
        agent_id: &str,
        level: &str,
    ) -> Result<Inspection, String> {
        let managed = self.required(agent_id)?;
        let running = lock(&managed.inspection).status == "running";
        let state: SessionState = parse(
            &self
                .rpc
                .call(
                    "setThinkingLevel",
                    json!({ "agentId": agent_id, "level": level, "running": running }),
                )
                .await?,
        )?;
        lock(&managed.options).thinking_level = Some(level.to_owned());
        lock(&managed.inspection).thinking_level = state.thinking_level.clone();
        *lock(&managed.state) = state;
        Ok(lock(&managed.inspection).clone())
    }

    pub async fn compact(&self, agent_id: &str, instructions: Option<&str>) -> Result<(), String> {
        let managed = self.required(agent_id)?;
        lock(&managed.inspection).status = "compacting".into();
        let result = self
            .rpc
            .call(
                "compact",
                json!({ "agentId": agent_id, "instructions": instructions }),
            )
            .await;
        lock(&managed.inspection).status = "idle".into();
        *lock(&managed.state) = parse(&result?)?;
        Ok(())
    }

    pub async fn abort(&self, agent_id: &str) -> Result<(), String> {
        let managed = self.required(agent_id)?;
        managed.cancelled.store(true, Ordering::SeqCst);
        managed.async_calls.cancel();
        lock(&managed.inspection).status = "aborting".into();
        let result = self.rpc.call("abort", json!({ "agentId": agent_id })).await;
        {
            let mut inspection = lock(&managed.inspection);
            inspection.status = "idle".into();
            inspection.current_tool = None;
        }
        *lock(&managed.state) = parse(&result?)?;
        Ok(())
    }

    /// Ends a session at once; events it still produces are dropped.
    pub fn stop(&self, agent_id: &str) {
        let Some(managed) = lock(&self.shared.sessions).remove(agent_id) else {
            return;
        };
        managed.cancelled.store(true, Ordering::SeqCst);
        managed.async_calls.cancel();
        lock(&self.shared.model_usage).remove(agent_id);
        self.rpc
            .send_request("stop", json!({ "agentId": agent_id }));
    }

    pub fn inspect(&self) -> Vec<Inspection> {
        let sessions: Vec<Arc<Managed>> = lock(&self.shared.sessions).values().cloned().collect();
        sessions
            .iter()
            .map(|managed| {
                let mut inspection = lock(&managed.inspection).clone();
                let state = lock(&managed.state);
                inspection
                    .active_tool_ids
                    .clone_from(&state.active_tool_ids);
                inspection.model_provider.clone_from(&state.model_provider);
                inspection.model_id.clone_from(&state.model_id);
                inspection.model_name.clone_from(&state.model_name);
                inspection.thinking_level.clone_from(&state.thinking_level);
                inspection.context_tokens = state.context_tokens;
                inspection.context_window = state.context_window;
                inspection.context_percent = state.context_percent;
                inspection
            })
            .collect()
    }

    /// `AuthProvider[]`: Pi's providers with their sign-in options and whether one is connected.
    pub async fn auth_providers(&self) -> Result<Value, String> {
        self.rpc.call("authProviders", json!({})).await
    }

    /// Runs the provider's own sign-in flow (`type` is `oauth` or `api_key`).
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: &str,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Result<(), String> {
        *lock(&self.shared.login) = Some(interaction);
        let result = self
            .rpc
            .call(
                "login",
                json!({ "providerId": provider_id, "type": auth_type }),
            )
            .await;
        *lock(&self.shared.login) = None;
        // A prompt still open when the flow has ended (or the sidecar died) is abandoned too.
        for (_, signal) in lock(&self.shared.auth_prompts).drain() {
            signal.abort();
        }
        result.map(drop)
    }

    pub async fn logout(&self, provider_id: &str) -> Result<(), String> {
        self.rpc
            .call("logout", json!({ "providerId": provider_id }))
            .await
            .map(drop)
    }

    /// Calls a sidecar method directly (its `test.*` hooks).
    #[doc(hidden)]
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc.call(method, params).await
    }

    fn managed(&self, agent_id: &str) -> Option<Arc<Managed>> {
        lock(&self.shared.sessions).get(agent_id).cloned()
    }

    fn required(&self, agent_id: &str) -> Result<Arc<Managed>, String> {
        self.managed(agent_id)
            .ok_or_else(|| format!("Unknown Pi agent {agent_id}"))
    }
}

/// `modelSelectionSchema.safeParse`: a strict object of trimmed, nonempty provider and id and an
/// optional known thinking level. Returns the parsed (trimmed) selection.
fn checked_selection(value: &Value) -> Result<Value, String> {
    let invalid = || "Pi model provider, id, and thinking level are invalid".to_owned();
    let object = value.as_object().ok_or_else(invalid)?;
    if object
        .keys()
        .any(|key| !["provider", "id", "thinkingLevel"].contains(&key.as_str()))
    {
        return Err(invalid());
    }
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(trim_js)
            .filter(|text| !text.is_empty())
            .ok_or_else(invalid)
    };
    let mut parsed = json!({ "provider": text("provider")?, "id": text("id")? });
    match object.get("thinkingLevel") {
        None => {}
        Some(Value::String(level)) if THINKING_LEVELS.contains(&level.as_str()) => {
            parsed["thinkingLevel"] = json!(level);
        }
        Some(_) => return Err(invalid()),
    }
    Ok(parsed)
}

/// A tool's parameter schema as JSON text in TypeBox's key order, which serde's sorted maps lose:
/// an object's `type`, `required`, then `properties` (required ones in order, then the rest); a
/// leaf's `type`, then its options as the TypeScript definitions write them. Pi's validation
/// messages and the schema the provider sees follow this order.
pub fn typebox_json(schema: &Value) -> String {
    const ORDER: [&str; 8] = [
        "type",
        "required",
        "properties",
        "description",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
    ];
    let Value::Object(object) = schema else {
        return schema.to_string();
    };
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort_by_key(|key| {
        ORDER
            .iter()
            .position(|known| known == key)
            .unwrap_or(ORDER.len())
    });
    let entries: Vec<String> = keys
        .into_iter()
        .map(|key| {
            let value = match (key.as_str(), &object[key]) {
                ("properties", Value::Object(properties)) => {
                    let required: Vec<&str> = object
                        .get("required")
                        .and_then(Value::as_array)
                        .map(|names| names.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let mut names: Vec<&String> = properties.keys().collect();
                    names.sort_by_key(|name| {
                        required
                            .iter()
                            .position(|known| known == name)
                            .unwrap_or(required.len())
                    });
                    let fields: Vec<String> = names
                        .into_iter()
                        .map(|name| format!("{}:{}", json!(name), typebox_json(&properties[name])))
                        .collect();
                    format!("{{{}}}", fields.join(","))
                }
                (_, value) => typebox_json(value),
            };
            format!("{}:{value}", json!(key))
        })
        .collect();
    format!("{{{}}}", entries.join(","))
}

/// `String.prototype.trim`: JavaScript's whitespace and line terminators, which add U+FEFF to
/// Unicode's White_Space and leave out U+0085.
fn trim_js(text: &str) -> &str {
    text.trim_matches(|character: char| {
        (character.is_whitespace() && character != '\u{85}') || character == '\u{FEFF}'
    })
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("Invalid Pi host reply: {error}"))
}

fn delta(current: f64, previous: f64) -> f64 {
    if current >= previous {
        current - previous
    } else {
        current
    }
}

/// Session tools submit through the daemon and record their events through the adapter.
struct ToolHost {
    host: Arc<dyn AgentHost>,
    emit: Emit,
}

impl AgentHost for ToolHost {
    fn submit(
        &self,
        tool: &str,
        project_id: &str,
        attempt_key: &str,
        record: Value,
        agent_id: &str,
    ) -> BoxFuture<Result<Value, String>> {
        self.host
            .submit(tool, project_id, attempt_key, record, agent_id)
    }

    fn emit(&self, event: Value) {
        (self.emit)(event);
    }
}

impl Shared {
    /// One sidecar event, as `onEvent` and `mapPiEvent`; events of a stopped session are dropped.
    fn on_event(&self, params: &Value) {
        let agent_id = params["agentId"].as_str().unwrap_or_default();
        let Some(managed) = lock(&self.sessions).get(agent_id).cloned() else {
            return;
        };
        lock(&managed.inspection).last_event_at = now_utc();
        if let Ok(state) = parse::<SessionState>(&params["state"]) {
            *lock(&managed.state) = state;
        }
        let options = lock(&managed.options).clone();
        let kind = params["kind"].as_str().unwrap_or_default();
        let previous = lock(&self.model_usage)
            .get(agent_id)
            .copied()
            .unwrap_or_default();
        let draft = map_event(&options, params, previous);
        if kind == "agent_end" {
            let usage = parse(&params["usage"]).unwrap_or_default();
            lock(&self.model_usage).insert(agent_id.to_owned(), usage);
        }
        let Some(draft) = draft else {
            return;
        };
        {
            let mut inspection = lock(&managed.inspection);
            match kind {
                "agent_start" => inspection.status = "running".into(),
                "agent_end" => {
                    inspection.status = if managed.prompt_active.load(Ordering::SeqCst) {
                        "running".into()
                    } else {
                        "idle".into()
                    };
                    inspection.current_tool = None;
                }
                "tool_start" => {
                    inspection.current_tool = params["toolName"].as_str().map(str::to_owned)
                }
                "tool_end" => inspection.current_tool = None,
                _ => {}
            }
        }
        (self.emit)(draft);
        // Progress is a line of ordinary assistant text, not a tool call; the daemon builds and records the progress-update.
        if kind == "turn_end"
            && let (Some(task_id), Some(host), Some(_)) =
                (&options.task_id, &self.host, &options.task_permissions)
            && !task_id.is_empty()
        {
            for note in policy::progress_notes(params["allText"].as_str().unwrap_or_default()) {
                let submitted = host.submit(
                    "nosh_progress_note",
                    &options.project_id,
                    &format!("task:{task_id}"),
                    json!({ "note": note }),
                    agent_id,
                );
                tokio::spawn(submitted);
            }
        }
    }

    /// Runs a tool call from the sidecar; its reply is the result or the error message.
    async fn run_tool(&self, params: &Value) -> Result<Value, String> {
        let agent_id = params["agentId"].as_str().unwrap_or_default();
        let tools = lock(&self.sessions)
            .get(agent_id)
            .and_then(|managed| managed.tools.clone())
            .ok_or_else(|| format!("Unknown Pi agent {agent_id}"))?;
        let name = params["name"].as_str().unwrap_or_default();
        let call_id = params["toolCallId"].as_str().unwrap_or_default();
        let result = tools
            .execute(name, call_id, &params["params"])
            .wait()
            .await?;
        Ok(json!({ "content": result.content, "details": result.details }))
    }
}

/// `mapPiEvent`: the event draft of one sidecar event (`{ kind, ... }`), `None` for kinds NOSH
/// does not record. `previous` is the session's usage at its last `agent_end`, so an `agent_end`
/// records the usage of its own run only (messages accumulate across runs and compactions).
pub fn map_event(options: &SessionOptions, event: &Value, previous: ModelUsage) -> Option<Value> {
    let field = |key: &str| event.get(key).cloned().unwrap_or(Value::Null);
    let (retention, kind, payload) = match event["kind"].as_str()? {
        "agent_start" => ("persistent", "agent.started", json!({})),
        "agent_end" => {
            let current: ModelUsage = parse(&event["usage"]).unwrap_or_default();
            let (kind, payload) = agent_end(event, current, previous);
            ("persistent", kind, payload)
        }
        "turn_start" => ("persistent", "agent.turn_started", json!({})),
        "turn_end" => (
            "persistent",
            "agent.turn_completed",
            json!({ "toolResultCount": field("toolResultCount") }),
        ),
        "tool_start" => (
            "persistent",
            "agent.tool_started",
            json!({ "toolCallId": field("toolCallId"), "toolName": field("toolName"), "args": field("args") }),
        ),
        "tool_update" => (
            "ephemeral",
            "agent.tool_update",
            json!({ "toolCallId": field("toolCallId"), "toolName": field("toolName"), "partialResult": field("partialResult") }),
        ),
        "tool_end" => (
            "persistent",
            "agent.tool_completed",
            json!({ "toolCallId": field("toolCallId"), "toolName": field("toolName"), "isError": field("isError"), "result": field("result") }),
        ),
        "text_delta" => (
            "ephemeral",
            "agent.text_delta",
            json!({ "delta": field("delta") }),
        ),
        "compaction_start" => (
            "persistent",
            "agent.compaction_started",
            json!({ "reason": field("reason") }),
        ),
        "compaction_end" => (
            "persistent",
            "agent.compaction_completed",
            json!({ "reason": field("reason"), "aborted": field("aborted"), "willRetry": field("willRetry"), "errorMessage": field("errorMessage") }),
        ),
        _ => return None,
    };
    let mut draft = event_base(options);
    draft.insert("retention".into(), json!(retention));
    draft.insert("type".into(), json!(kind));
    draft.insert("payload".into(), payload);
    Some(Value::Object(draft))
}

/// `agent.completed`, or `agent.retrying`/`agent.failed` for a provider failure, with this run's usage.
fn agent_end(event: &Value, current: ModelUsage, previous: ModelUsage) -> (&'static str, Value) {
    let total = delta(current.total_tokens, previous.total_tokens);
    let input = delta(current.input_tokens, previous.input_tokens);
    let output = delta(current.output_tokens, previous.output_tokens);
    let cache_read = delta(current.cache_read_tokens, previous.cache_read_tokens);
    let cache_write = delta(current.cache_write_tokens, previous.cache_write_tokens);
    let mut payload = Map::new();
    let will_retry = event["willRetry"].as_bool();
    let kind = match event.get("failure").filter(|failure| failure.is_object()) {
        Some(failure) => {
            payload.insert("reason".into(), failure["reason"].clone());
            payload.insert("message".into(), failure["message"].clone());
            if will_retry == Some(true) {
                "agent.retrying"
            } else {
                "agent.failed"
            }
        }
        None => {
            payload.insert("message".into(), event["message"].clone());
            "agent.completed"
        }
    };
    payload.insert(
        "modelTokens".into(),
        json!(budget_tokens(total, input, output, cache_read, cache_write)),
    );
    payload.insert("providerTotalTokens".into(), json!(total));
    payload.insert("inputTokens".into(), json!(input));
    payload.insert("outputTokens".into(), json!(output));
    payload.insert("cacheReadTokens".into(), json!(cache_read));
    payload.insert("cacheWriteTokens".into(), json!(cache_write));
    if let Some(will_retry) = will_retry {
        payload.insert("willRetry".into(), json!(will_retry));
    }
    let mut payload = Value::Object(payload);
    normalize_numbers(&mut payload);
    (kind, payload)
}

/// The JSON-lines connection to the sidecar process.
struct Rpc {
    outgoing: mpsc::UnboundedSender<String>,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    /// Set when the sidecar's output ends: its last line on stderr.
    exited: Mutex<Option<String>>,
    _child: Mutex<tokio::process::Child>,
}

impl Rpc {
    fn spawn(config: &PiConfig, shared: Weak<Shared>) -> Result<Arc<Self>, String> {
        let mut command = tokio::process::Command::new(&config.node);
        command
            .arg(&config.host_script)
            .envs(config.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("Pi host failed to start: {error}"))?;
        let (stdin, stdout, stderr) = (
            child.stdin.take().expect("piped stdin"),
            child.stdout.take().expect("piped stdout"),
            child.stderr.take().expect("piped stderr"),
        );
        let (outgoing, mut lines) = mpsc::unbounded_channel::<String>();
        let rpc = Arc::new(Self {
            outgoing,
            waiting: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            exited: Mutex::new(None),
            _child: Mutex::new(child),
        });
        // Closing stdin (when the adapter is dropped) ends the sidecar.
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = lines.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let last_error = Arc::new(Mutex::new(String::new()));
        {
            let (last_error, forward) = (Arc::clone(&last_error), config.forward_stderr);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if forward {
                        eprintln!("[pi-host] {line}");
                    }
                    if !line.trim().is_empty() {
                        *lock(&last_error) = line;
                    }
                }
            });
        }
        let weak = Arc::downgrade(&rpc);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let (Some(rpc), Ok(message)) =
                    (weak.upgrade(), serde_json::from_str::<Value>(&line))
                else {
                    continue;
                };
                rpc.dispatch(message, &shared);
            }
            if let Some(rpc) = weak.upgrade() {
                // Let the stderr reader catch up so the reason is reported.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let reason = lock(&last_error).clone();
                rpc.fail_all(if reason.is_empty() {
                    "Pi host exited".to_owned()
                } else {
                    format!("Pi host exited: {reason}")
                });
            }
        });
        Ok(rpc)
    }

    fn dispatch(self: &Arc<Self>, message: Value, shared: &Weak<Shared>) {
        let Some(method) = message["method"].as_str() else {
            let id = message["id"].as_u64().unwrap_or_default();
            if let Some(waiter) = lock(&self.waiting).remove(&id) {
                let result = match message.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap_or_default().to_owned()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(result);
            }
            return;
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "event" => shared.on_event(&params),
            "auth.notify" => {
                if let Some(interaction) = lock(&shared.login).clone() {
                    interaction.notify(params["event"].clone());
                }
            }
            "auth.cancel" => {
                if let Some(signal) = lock(&shared.auth_prompts).get(&params["id"].to_string()) {
                    signal.abort();
                }
            }
            "auth.prompt" => {
                // Registered before the prompt's task runs, so an `auth.cancel` read next finds it.
                let (rpc, id, signal) = (
                    Arc::clone(self),
                    message["id"].clone(),
                    AuthSignal::default(),
                );
                lock(&shared.auth_prompts).insert(id.to_string(), signal.clone());
                tokio::spawn(async move {
                    let interaction = lock(&shared.login).clone();
                    let result = match interaction {
                        Some(interaction) => interaction
                            .prompt(params["prompt"].clone(), signal)
                            .await
                            .map(Value::String),
                        None => Err("No sign-in is in progress".into()),
                    };
                    lock(&shared.auth_prompts).remove(&id.to_string());
                    rpc.reply(&id, result);
                });
            }
            _ => {
                let (rpc, id, method) =
                    (Arc::clone(self), message["id"].clone(), method.to_owned());
                tokio::spawn(async move {
                    let result = match method.as_str() {
                        "tool" => shared.run_tool(&params).await,
                        _ => Err(format!("Unknown NOSH method {method}")),
                    };
                    rpc.reply(&id, result);
                });
            }
        }
    }

    fn reply(&self, id: &Value, result: Result<Value, String>) {
        self.send(&match result {
            Ok(result) => json!({ "id": id, "result": result }),
            Err(message) => json!({ "id": id, "error": { "message": message } }),
        });
    }

    fn send(&self, message: &Value) {
        let _ = self.outgoing.send(format!("{message}\n"));
    }

    fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Result<Value, String>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = oneshot::channel();
        if let Some(reason) = lock(&self.exited).clone() {
            let _ = sender.send(Err(reason));
            return receiver;
        }
        lock(&self.waiting).insert(id, sender);
        self.send(&json!({ "id": id, "method": method, "params": params }));
        receiver
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request(method, params)
            .await
            .unwrap_or_else(|_| Err("Pi host exited".into()))
    }

    /// A request whose reply nobody waits for.
    fn send_request(&self, method: &str, params: Value) {
        drop(self.request(method, params));
    }

    fn fail_all(&self, reason: String) {
        *lock(&self.exited) = Some(reason.clone());
        for (_, waiter) in lock(&self.waiting).drain() {
            let _ = waiter.send(Err(reason.clone()));
        }
    }
}
