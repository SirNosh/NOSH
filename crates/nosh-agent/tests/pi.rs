//! `PiAdapter` against the real sidecar (`sidecar/pi-host`, with `NOSH_PI_HOST_TEST=1`), as
//! `pi-adapter.test.ts`: real Pi sessions on static and faux model runtimes. Pi's tool-argument
//! verdicts and the event mapping are also compared with the built TypeScript pi-adapter.
//!
//! Needs `npm ci` in `sidecar/pi-host` and the TypeScript tree built (`NOSH_TS_ROOT`).

use nosh_agent::async_calls::{AsyncToolCalls, BoxFuture};
use nosh_agent::pi::{
    Emit, ModelSelection, ModelUsage, PiAdapter, PiConfig, map_event, typebox_json,
};
use nosh_agent::policy::SessionOptions;
use nosh_agent::terminal::{TerminalContext, TerminalHost};
use nosh_agent::tools::{AgentHost, SessionTools};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROJECT: &str = "prj_0123456789abcdef0123456789abcdef";
const TASK: &str = "tsk_0123456789abcdef0123456789abcdef";
const AGENT: &str = "agt_0123456789abcdef0123456789abcdef";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run_typescript(ops: &[Value]) -> Vec<Value> {
    let mut child = Command::new("node")
        .arg(root().join("scripts/pi-adapter-ts.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(ops).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "the TypeScript side ran");
    serde_json::from_slice::<Vec<Value>>(&output.stdout)
        .unwrap()
        .into_iter()
        .map(|reply| reply.get("ok").cloned().unwrap_or(reply))
        .collect()
}

fn schema_dir() -> PathBuf {
    root().join("conformance/schemas/json")
}

/// A daemon that accepts every submission.
struct Accepting;

impl AgentHost for Accepting {
    fn submit(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Value,
        _: &str,
    ) -> BoxFuture<Result<Value, String>> {
        Box::pin(async { Ok(json!({ "accepted": true })) })
    }
    fn emit(&self, _: Value) {}
}

struct Recorded {
    adapter: Arc<PiAdapter>,
    events: Arc<Mutex<Vec<Value>>>,
}

impl Recorded {
    fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }
    fn types(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_owned())
            .collect()
    }
}

async fn adapter(
    host: Option<Arc<dyn AgentHost>>,
    terminal: Option<Arc<dyn TerminalHost>>,
) -> Recorded {
    let events = Arc::new(Mutex::new(Vec::new()));
    let emit: Emit = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let config = PiConfig {
        node: "node".into(),
        host_script: root().join("sidecar/pi-host/host.mjs"),
        env: vec![("NOSH_PI_HOST_TEST".into(), "1".into())],
        schema_dir: schema_dir(),
    };
    let adapter = PiAdapter::spawn(config, emit, host, terminal)
        .await
        .unwrap();
    Recorded {
        adapter: Arc::new(adapter),
        events,
    }
}

fn reasoning_model() -> Value {
    json!({
        "provider": "provider-a", "id": "reasoning-model", "name": "Reasoning Model", "api": "openai-responses", "baseUrl": "https://provider.invalid/v1", "reasoning": true,
        "thinkingLevelMap": { "xhigh": "xhigh", "max": "max" }, "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 10_000, "maxTokens": 1_000,
    })
}

/// `testRuntime()`: two models of one provider, for sessions that never prompt.
async fn static_runtime(adapter: &PiAdapter) {
    let mut plain = reasoning_model();
    plain["id"] = json!("plain-model");
    plain["name"] = json!("Plain Model");
    plain["reasoning"] = json!(false);
    adapter
        .call("test.runtime", json!({ "models": [reasoning_model(), plain], "providers": [{ "id": "provider-a", "name": "Provider A" }] }))
        .await
        .unwrap();
}

async fn faux_runtime(adapter: &PiAdapter, faux: Value, steps: Value) {
    adapter
        .call("test.runtime", json!({ "faux": faux }))
        .await
        .unwrap();
    adapter
        .call("test.respond", json!({ "steps": steps }))
        .await
        .unwrap();
}

fn options(extra: Value) -> SessionOptions {
    let mut base = json!({
        "projectId": PROJECT, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null,
        "taskId": TASK, "agentId": AGENT, "role": "general_worker", "cwd": ".", "packagePath": root().join("pi-package"),
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(base).unwrap()
}

fn permissions(allowed: &[&str], subprocess: &str) -> Value {
    json!({ "network": "disabled", "subprocess": subprocess, "gitCommit": false, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": allowed })
}

fn workspace(write_scopes: &[&str]) -> Value {
    json!({ "worktreeId": "wt_adapter", "branch": "main", "startingCommit": "a".repeat(40), "writeScopes": write_scopes, "protectedScopes": [".nosh/**"] })
}

fn has(tools: &[String], name: &str) -> bool {
    tools.iter().any(|tool| tool == name)
}

#[tokio::test(flavor = "multi_thread")]
async fn activates_only_task_packet_authorized_tools_in_a_real_pi_session() {
    let recorded = adapter(Some(Arc::new(Accepting)), None).await;
    let adapter = &recorded.adapter;
    static_runtime(adapter).await;
    let directory = tempfile::tempdir().unwrap();
    let mut packet = permissions(
        &[
            "tool_pi.read",
            "tool_nosh.git.commit",
            "tool_nosh.response.submit",
        ],
        "disabled",
    );
    packet["gitCommit"] = json!(true);
    let base = json!({ "cwd": directory.path(), "model": { "provider": "provider-a", "id": "reasoning-model" }, "taskPermissions": packet, "taskWorkspace": workspace(&["src/**"]) });
    let started = adapter.start(options(base.clone())).await.unwrap();
    assert!(
        has(&started.active_tool_ids, "nosh_workspace_read"),
        "{:?}",
        started.active_tool_ids
    );
    // Commit authority is exercised by the daemon; no bookkeeping tool reaches the model.
    assert!(!has(&started.active_tool_ids, "nosh_git_commit"));
    for builtin in ["bash", "read", "edit", "write", "nosh_subprocess"] {
        assert!(!has(&started.active_tool_ids, builtin), "{builtin}");
    }
    let other = |change: Value| {
        let mut options = base.clone();
        options["agentId"] = json!("agt_ffffffffffffffffffffffffffffffff");
        for (key, value) in change.as_object().unwrap() {
            options["taskPermissions"][key] = value.clone();
        }
        self::options(options)
    };
    let error = adapter
        .start(other(json!({ "subprocess": "allowlisted" })))
        .await
        .unwrap_err();
    assert!(error.contains("subprocess authority"), "{error}");
    let error = adapter
        .start(other(json!({ "gitCommit": false })))
        .await
        .unwrap_err();
    assert!(error.contains("gitCommit authority"), "{error}");
    let error = adapter
        .start(other(
            json!({ "subprocess": "enabled", "allowedToolIds": ["tool_pi.read", "tool_nosh.run"] }),
        ))
        .await
        .unwrap_err();
    assert!(error.contains("subprocess authority=enabled"), "{error}");
    let error = adapter
        .start(other(
            json!({ "subprocess": "allowlisted", "allowedToolIds": ["tool_nosh.subprocess"] }),
        ))
        .await
        .unwrap_err();
    assert!(error.contains("subprocess authority"), "{error}");
    // The same scope again is the same session; a different one is refused.
    assert_eq!(
        adapter
            .start(options(base.clone()))
            .await
            .unwrap()
            .pi_session_id,
        started.pi_session_id
    );
    let mut changed = base;
    changed["role"] = json!("reviewer");
    let error = adapter.start(options(changed)).await.unwrap_err();
    assert_eq!(
        error,
        format!("Agent {AGENT} already has a different Pi session scope")
    );
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_the_nosh_default_model_and_thinking_level_when_a_session_selects_none() {
    let recorded = adapter(Some(Arc::new(Accepting)), None).await;
    let adapter = &recorded.adapter;
    static_runtime(adapter).await;
    adapter.set_default_model(Some(ModelSelection {
        provider: "provider-a".into(),
        id: "reasoning-model".into(),
        thinking_level: Some("low".into()),
    }));
    let started = adapter.start(options(json!({}))).await.unwrap();
    assert_eq!(
        (
            started.model_provider.as_deref(),
            started.model_id.as_deref(),
            started.thinking_level.as_str()
        ),
        (Some("provider-a"), Some("reasoning-model"), "low")
    );
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn projects_models_and_rejects_invalid_and_unsupported_thinking_levels() {
    let recorded = adapter(None, None).await;
    let adapter = &recorded.adapter;
    static_runtime(adapter).await;
    let models = adapter.available_models().await.unwrap();
    assert_eq!(
        models[0],
        json!({ "provider": "provider-a", "providerName": "Provider A", "id": "reasoning-model", "name": "Reasoning Model", "reasoning": true, "thinkingLevels": ["off", "minimal", "low", "medium", "high", "xhigh", "max"], "contextWindow": 10_000, "maxTokens": 1_000 })
    );
    assert_eq!(
        (
            models[1]["reasoning"].clone(),
            models[1]["thinkingLevels"].clone()
        ),
        (json!(false), json!(["off"]))
    );
    let invalid = adapter.validate_model_selection(Some(&json!({ "provider": "provider-a", "id": "reasoning-model", "thinkingLevel": "invalid" }))).await;
    assert_eq!(
        invalid.unwrap_err(),
        "Pi model provider, id, and thinking level are invalid"
    );
    let unsupported = adapter
        .validate_model_selection(Some(
            &json!({ "provider": "provider-a", "id": "plain-model", "thinkingLevel": "high" }),
        ))
        .await
        .unwrap_err();
    assert!(
        unsupported.ends_with("supported levels: off"),
        "{unsupported}"
    );
    // zod's schema trims provider and id, and refuses unknown keys.
    adapter
        .validate_model_selection(Some(
            &json!({ "provider": " provider-a\u{3000}", "id": "\treasoning-model" }),
        ))
        .await
        .unwrap();
    assert!(
        adapter
            .validate_model_selection(Some(
                &json!({ "provider": "provider-a", "id": "reasoning-model", "extra": 1 })
            ))
            .await
            .is_err()
    );
    let missing = adapter
        .validate_model_selection(Some(&json!({ "provider": "provider-a", "id": "none" })))
        .await
        .unwrap_err();
    assert_eq!(missing, "Unavailable Pi model provider-a/none");
    adapter.validate_model_selection(None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn propagates_the_selected_level_into_a_new_session_and_changes_it_in_place_when_idle() {
    let recorded = adapter(None, None).await;
    let adapter = &recorded.adapter;
    static_runtime(adapter).await;
    let directory = tempfile::tempdir().unwrap();
    let started = adapter
        .start(options(json!({ "role": "nosh", "cwd": directory.path(), "model": { "provider": "provider-a", "id": "reasoning-model" }, "thinkingLevel": "max" })))
        .await
        .unwrap();
    assert_eq!(
        (started.model_id.as_deref(), started.thinking_level.as_str()),
        (Some("reasoning-model"), "max")
    );
    let changed = adapter.set_thinking_level(AGENT, "medium").await.unwrap();
    assert_eq!(
        (
            changed.pi_session_id.as_str(),
            changed.thinking_level.as_str()
        ),
        (started.pi_session_id.as_str(), "medium")
    );
    let inspected = adapter.inspect();
    assert_eq!(
        (
            inspected[0].pi_session_id.as_str(),
            inspected[0].thinking_level.as_str()
        ),
        (started.pi_session_id.as_str(), "medium")
    );
    let error = adapter
        .set_thinking_level(AGENT, "turbo")
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "Thinking level \"turbo\" is not supported by provider-a/reasoning-model; supported levels: off, minimal, low, medium, high, xhigh, max"
    );
    adapter.stop(AGENT);
    assert!(adapter.inspect().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_a_real_agent_session_streams_a_turn_reuses_scope_and_returns_idle() {
    let recorded = adapter(None, None).await;
    let adapter = &recorded.adapter;
    faux_runtime(adapter, json!({ "provider": "test-provider", "api": "test-api", "models": [{ "id": "deterministic", "name": "Deterministic", "reasoning": false }] }), json!([{ "text": "deterministic result" }])).await;
    let directory = tempfile::tempdir().unwrap();
    let options = options(
        json!({ "taskId": null, "cwd": directory.path(), "model": { "provider": "test-provider", "id": "deterministic" } }),
    );
    let started = adapter.start(options.clone()).await.unwrap();
    let again = adapter.start(options).await.unwrap();
    assert_eq!(
        (again.pi_session_id.as_str(), again.status.as_str()),
        (started.pi_session_id.as_str(), "idle")
    );
    adapter
        .prompt(AGENT, "Say one deterministic sentence.", None)
        .await
        .unwrap();
    let types = recorded.types();
    for kind in ["agent.started", "agent.text_delta", "agent.completed"] {
        assert!(types.iter().any(|event| event == kind), "{kind}: {types:?}");
    }
    let completed = recorded
        .events()
        .into_iter()
        .find(|event| event["type"] == "agent.completed")
        .unwrap();
    assert_eq!(completed["payload"]["message"], "deterministic result");
    assert_eq!(completed["scope"]["agentId"], AGENT);
    let inspected = adapter.inspect();
    assert_eq!(
        (inspected[0].agent_id.as_str(), inspected[0].status.as_str()),
        (AGENT, "idle")
    );
    assert!(
        inspected[0]
            .context_tokens
            .is_some_and(|tokens| tokens > 0.0)
    );
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_faux_provider_rejection_to_a_safe_failure_and_idle_status() {
    let recorded = adapter(None, None).await;
    let adapter = &recorded.adapter;
    faux_runtime(adapter, json!({ "provider": "reject-provider", "api": "reject-api", "models": [{ "id": "rejecting", "reasoning": false }] }), json!([{ "text": "", "stopReason": "error", "errorMessage": "provider secret: rejected" }])).await;
    let directory = tempfile::tempdir().unwrap();
    adapter.start(options(json!({ "taskId": null, "cwd": directory.path(), "model": { "provider": "reject-provider", "id": "rejecting" } }))).await.unwrap();
    adapter.prompt(AGENT, "reject", None).await.unwrap();
    let failure = recorded
        .events()
        .into_iter()
        .find(|event| event["type"] == "agent.failed")
        .unwrap();
    assert_eq!(failure["payload"]["reason"], "provider_error");
    assert!(!failure.to_string().contains("provider secret"));
    assert_eq!(adapter.inspect()[0].status, "idle");
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn aborts_a_controlled_streaming_turn_without_post_stop_event_leakage() {
    let recorded = adapter(None, None).await;
    let adapter = Arc::clone(&recorded.adapter);
    faux_runtime(&adapter, json!({ "provider": "stop-provider", "api": "stop-api", "models": [{ "id": "controlled", "reasoning": false }], "tokensPerSecond": 1 }), json!([{ "hold": "response" }])).await;
    let directory = tempfile::tempdir().unwrap();
    adapter.start(options(json!({ "taskId": null, "cwd": directory.path(), "model": { "provider": "stop-provider", "id": "controlled" } }))).await.unwrap();
    let turn = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.prompt(AGENT, "hold", None).await })
    };
    for _ in 0..200 {
        if recorded.types().iter().any(|kind| kind == "agent.started") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(recorded.types().iter().any(|kind| kind == "agent.started"));
    adapter.follow_up(AGENT, "queued").await.unwrap();
    let aborting = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.abort(AGENT).await })
    };
    // The held response does not observe the abort signal; releasing it lets Pi finish cancelling.
    adapter
        .call(
            "test.release",
            json!({ "hold": "response", "text": "released after abort" }),
        )
        .await
        .unwrap();
    aborting.await.unwrap().unwrap();
    let _ = turn.await.unwrap();
    assert_eq!(adapter.inspect()[0].status, "idle");
    let count = recorded.events().len();
    adapter.stop(AGENT);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(recorded.events().len(), count);
    assert!(adapter.inspect().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn offers_nosh_code_only_to_sessions_that_already_hold_a_tool_it_can_call() {
    let recorded = adapter(Some(Arc::new(Accepting)), None).await;
    let adapter = &recorded.adapter;
    static_runtime(adapter).await;
    let directory = tempfile::tempdir().unwrap();
    let active = async |extra: Value| {
        let mut base = json!({ "cwd": directory.path(), "model": { "provider": "provider-a", "id": "reasoning-model" } });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let started = adapter.start(options(base)).await.unwrap();
        adapter.stop(AGENT);
        started.active_tool_ids
    };
    let worker = active(json!({ "taskPermissions": permissions(&["tool_pi.read", "tool_pi.edit"], "disabled"), "taskWorkspace": workspace(&["src/**"]) })).await;
    for tool in ["nosh_code", "nosh_workspace_read", "nosh_workspace_edit"] {
        assert!(has(&worker, tool), "{tool}: {worker:?}");
    }
    // Reviewers batch-read Artifacts; Directors have no tools; unscoped Project chat never gets programs.
    let reviewer = active(json!({ "role": "reviewer" })).await;
    assert!(
        has(&reviewer, "nosh_code") && has(&reviewer, "nosh_artifact_read"),
        "{reviewer:?}"
    );
    assert!(!has(
        &active(json!({ "role": "mission_director" })).await,
        "nosh_code"
    ));
    assert!(!has(
        &active(json!({ "role": "nosh", "taskId": null })).await,
        "nosh_code"
    ));
}

/// A terminal host that accepts the records of the first envelope.
struct Receipts {
    accepted: Mutex<Vec<Value>>,
}

impl TerminalHost for Receipts {
    fn submit(&self, _: &TerminalContext, records: Vec<Value>) -> BoxFuture<Result<Value, String>> {
        self.accepted.lock().unwrap().push(Value::Array(records));
        Box::pin(async {
            Ok(
                json!({ "accepted": true, "retryAllowed": false, "effect": { "state": "completed" } }),
            )
        })
    }
    fn reject(&self, _: &TerminalContext, reason: String) -> BoxFuture<Result<Value, String>> {
        Box::pin(async move { Err(format!("unexpected rejection: {reason}")) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_nosh_code_in_a_real_pi_turn_and_answers_with_the_filtered_result() {
    let receipts = Arc::new(Receipts {
        accepted: Mutex::new(Vec::new()),
    });
    let recorded = adapter(
        Some(Arc::new(Accepting)),
        Some(Arc::clone(&receipts) as Arc<dyn TerminalHost>),
    )
    .await;
    let adapter = &recorded.adapter;
    let record =
        json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 });
    let envelope = json!({ "$schema": "https://nosh.dev/schemas/terminal-output/v1", "schemaVersion": 1, "records": [record] }).to_string();
    let program = "const [a, b] = await Promise.all([tools.read('a.txt'), tools.read('b.txt')]); return (a + b).trim().split(String.fromCharCode(10)).length;";
    faux_runtime(
        adapter,
        json!({ "provider": "code-proof", "api": "code-proof-api", "models": [{ "id": "deterministic", "reasoning": false }] }),
        json!([{ "toolCalls": [{ "name": "nosh_code", "arguments": { "code": program }, "id": "call_code" }] }, { "capture": true, "text": envelope }]),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "three\nfour\n").unwrap();
    adapter
        .start(options(json!({ "cwd": directory.path(), "model": { "provider": "code-proof", "id": "deterministic" }, "taskPermissions": permissions(&["tool_pi.read", "tool_nosh.response.submit"], "disabled"), "taskWorkspace": workspace(&["**"]) })))
        .await
        .unwrap();
    adapter
        .prompt(AGENT, "Count the lines of a.txt and b.txt.", None)
        .await
        .unwrap();
    let captured = adapter.call("test.captured", json!({})).await.unwrap();
    assert!(
        captured[0].as_str().unwrap().contains(r#"\"value\":4"#),
        "{captured}"
    );
    assert_eq!(*receipts.accepted.lock().unwrap(), vec![json!([record])]);
    let completed: Vec<Value> = recorded
        .events()
        .iter()
        .filter(|event| event["type"] == "agent.tool_completed")
        .map(|event| {
            json!([
                event["payload"]["toolName"],
                event["payload"]
                    .get("parentToolCallId")
                    .cloned()
                    .unwrap_or(Value::Null)
            ])
        })
        .collect();
    assert_eq!(
        completed,
        vec![
            json!(["nosh_workspace_read", "call_code"]),
            json!(["nosh_workspace_read", "call_code"]),
            json!(["nosh_code", null])
        ]
    );
    let receipt = recorded
        .events()
        .into_iter()
        .find(|event| event["type"] == "agent.terminal_receipt")
        .unwrap();
    assert_eq!(
        (
            receipt["correlationId"].clone(),
            receipt["payload"]["accepted"].clone()
        ),
        (json!(format!("task:{TASK}")), json!(true))
    );
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn offers_chatgpt_and_claude_subscriptions_and_api_keys_from_pis_providers() {
    let recorded = adapter(None, None).await;
    let providers = recorded.adapter.auth_providers().await.unwrap();
    let find = |id: &str| {
        providers
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert!(
        find("openai-codex")["subscription"]
            .as_str()
            .unwrap()
            .contains("ChatGPT")
    );
    assert!(
        find("anthropic")["subscription"]
            .as_str()
            .unwrap()
            .contains("Claude")
    );
    assert!(
        find("openai")["apiKey"]
            .as_str()
            .unwrap()
            .contains("API key")
    );
    assert!(
        providers
            .as_array()
            .unwrap()
            .iter()
            .all(|provider| provider["connected"].is_boolean())
    );
}

/// Arguments for one tool: each value under each parameter, alone and next to valid others.
fn vectors(name: &str, parameters: &Value) -> Vec<Value> {
    let keys: Vec<String> = parameters["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    let values = [
        Value::Null,
        json!(true),
        json!(false),
        json!(0),
        json!(1),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!("-1"),
        json!("1.5"),
        json!(""),
        json!(" "),
        json!("x"),
        json!("true"),
        json!("null"),
        json!([]),
        json!(["x"]),
        json!([1]),
        json!({}),
        json!({ "a": 1 }),
        json!("x".repeat(1_001)),
        json!("x".repeat(2_049)),
        json!("x".repeat(20_001)),
    ];
    let mut out = vec![
        json!({}),
        Value::Null,
        json!("x"),
        json!([]),
        json!({ "extra": 1 }),
    ];
    let base: serde_json::Map<String, Value> = keys
        .iter()
        .map(|key| (key.clone(), json!("src/a.txt")))
        .collect();
    out.push(Value::Object(base.clone()));
    for key in &keys {
        for value in &values {
            out.push(json!({ key: value }));
            let mut next = base.clone();
            next.insert(key.clone(), value.clone());
            out.push(Value::Object(next));
        }
    }
    out.into_iter()
        .map(|arguments| json!({ "name": name, "arguments": arguments }))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_validates_tool_arguments_against_revived_definitions_as_against_typebox() {
    let recorded = adapter(None, None).await;
    let mut packet = permissions(
        &[
            "tool_pi.read",
            "tool_pi.edit",
            "tool_pi.write",
            "tool_nosh.run",
            "tool_nosh.network.read",
            "tool_nosh.blocker.submit",
        ],
        "allowlisted",
    );
    packet["network"] = json!("allowlisted");
    packet["networkAllowlist"] = json!(["example.com"]);
    let variants = [
        json!({ "taskPermissions": packet, "taskWorkspace": workspace(&["**"]) }),
        json!({ "taskId": null, "role": "nosh" }),
        json!({ "role": "reviewer" }),
    ];
    let mut compared = 0;
    for extra in variants {
        let options = options(extra);
        let tools = SessionTools::new(
            options.clone(),
            Arc::new(Accepting),
            Arc::new(AsyncToolCalls::default()),
            &schema_dir(),
            true,
        );
        let definitions: Vec<Value> = tools
            .definitions()
            .iter()
            .map(|tool| json!({ "name": tool.name, "parameters": typebox_json(&tool.parameters) }))
            .collect();
        let calls: Vec<Value> = tools
            .definitions()
            .iter()
            .flat_map(|tool| vectors(&tool.name, &tool.parameters))
            .collect();
        let ours = recorded
            .adapter
            .call(
                "test.validate",
                json!({ "tools": definitions, "calls": calls }),
            )
            .await
            .unwrap();
        let theirs =
            run_typescript(&[json!({ "validate": { "options": options, "calls": calls } })])
                .remove(0);
        // The provider sees the same schema text, key order included.
        assert_eq!(ours["parameters"], theirs["parameters"]);
        let (ours, theirs) = (
            ours["verdicts"].as_array().unwrap(),
            theirs["verdicts"].as_array().unwrap(),
        );
        assert_eq!(ours.len(), calls.len());
        for ((call, a), b) in calls.iter().zip(ours).zip(theirs) {
            assert_eq!(a, b, "{call}");
        }
        compared += calls.len();
    }
    assert!(compared > 1_000, "{compared}");
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_pi_events_as_typescript_does() {
    let recorded = adapter(None, None).await;
    let options = options(json!({ "missionId": "mis_1", "runId": "run_1" }));
    let scope = json!({ "projectId": PROJECT, "missionId": "mis_1", "directionId": null, "autoresearchId": null, "experimentId": null, "runId": "run_1", "jobId": null, "taskId": TASK, "agentId": AGENT });
    let block = |text: &str, phase: Option<&str>| match phase {
        Some(phase) => {
            json!({ "type": "text", "text": text, "textSignature": json!({ "v": 1, "id": format!("msg_{text}"), "phase": phase }).to_string() })
        }
        None => json!({ "type": "text", "text": text }),
    };
    let end = |message: Value, will_retry: Value| json!({ "type": "agent_end", "willRetry": will_retry, "messages": [{ "role": "user", "content": [] }, message] });
    let events = [
        json!({ "type": "message_update", "message": {}, "assistantMessageEvent": { "type": "text_delta", "delta": "hello", "contentIndex": 0, "partial": {} } }),
        json!({ "type": "message_update", "message": {}, "assistantMessageEvent": { "type": "thinking_delta", "delta": "hidden", "contentIndex": 0, "partial": {} } }),
        json!({ "type": "tool_execution_start", "toolCallId": "call_1", "toolName": "read", "args": { "path": "README.md" } }),
        json!({ "type": "tool_execution_update", "toolCallId": "call_1", "toolName": "read", "args": {}, "partialResult": { "content": [] } }),
        json!({ "type": "tool_execution_end", "toolCallId": "call_1", "toolName": "read", "isError": true, "result": { "content": [{ "type": "text", "text": "no" }] } }),
        json!({ "type": "agent_start" }),
        json!({ "type": "turn_start" }),
        json!({ "type": "turn_end", "message": { "role": "assistant", "content": [block("PROGRESS: x", None)] }, "toolResults": [{}, {}] }),
        json!({ "type": "compaction_start", "reason": "threshold" }),
        json!({ "type": "compaction_end", "reason": "overflow", "aborted": false, "willRetry": true }),
        json!({ "type": "compaction_end", "reason": "manual", "aborted": true, "willRetry": false, "errorMessage": "stopped" }),
        json!({ "type": "message_start", "message": {} }),
        end(
            json!({ "role": "assistant", "stopReason": "stop", "content": [{ "type": "thinking", "thinking": "hidden" }, block("result", None)], "usage": { "totalTokens": 42 } }),
            json!(false),
        ),
        end(
            json!({ "role": "assistant", "stopReason": "error", "errorMessage": "Temporary provider rejection", "content": [], "usage": { "totalTokens": 12 } }),
            json!(true),
        ),
        end(
            json!({ "role": "assistant", "stopReason": "error", "errorMessage": "CreditsError: Insufficient balance", "content": [], "usage": { "totalTokens": 42, "input": 20, "output": 10, "cacheRead": 8, "cacheWrite": 4 } }),
            json!(false),
        ),
        end(
            json!({ "role": "assistant", "stopReason": "error", "errorMessage": "401 Incorrect API key provided", "content": [] }),
            json!(false),
        ),
        end(
            json!({ "role": "assistant", "stopReason": "error", "errorMessage": "invalid_api_key", "content": [] }),
            Value::Null,
        ),
        end(
            json!({ "role": "assistant", "stopReason": "aborted", "errorMessage": "Request aborted by user", "content": [], "usage": { "totalTokens": 7 } }),
            json!(false),
        ),
        end(
            json!({ "role": "assistant", "content": [], "usage": { "totalTokens": 100, "input": 20, "output": 10, "cacheRead": 60, "cacheWrite": 10 } }),
            json!(false),
        ),
        end(
            json!({ "role": "assistant", "content": [block("thinking aloud", Some("commentary")), { "type": "toolCall" }, block("the answer", Some("final_answer"))] }),
            json!(false),
        ),
        end(
            json!({ "role": "assistant", "content": [block("only commentary", Some("commentary"))], "usage": { "totalTokens": -5, "input": "9" } }),
            json!(false),
        ),
    ];
    let previous = [
        json!(0),
        json!(12),
        json!(30),
        json!(42),
        json!({ "totalTokens": 50, "inputTokens": 30, "outputTokens": 5, "cacheReadTokens": 70, "cacheWriteTokens": 1 }),
    ];
    let mut ops = Vec::new();
    let mut cases = Vec::new();
    for event in &events {
        for previous in &previous {
            ops.push(json!({ "fn": "mapPiEvent", "args": [scope, event, previous] }));
            cases.push((event, previous));
        }
    }
    let expected = run_typescript(&ops);
    for ((event, previous), expected) in cases.into_iter().zip(expected) {
        let summary = recorded
            .adapter
            .call("test.summarize", json!({ "event": event }))
            .await
            .unwrap();
        let usage = match previous {
            Value::Number(total) => ModelUsage {
                total_tokens: total.as_f64().unwrap(),
                ..ModelUsage::default()
            },
            usage => ModelUsage {
                total_tokens: usage["totalTokens"].as_f64().unwrap(),
                input_tokens: usage["inputTokens"].as_f64().unwrap(),
                output_tokens: usage["outputTokens"].as_f64().unwrap(),
                cache_read_tokens: usage["cacheReadTokens"].as_f64().unwrap(),
                cache_write_tokens: usage["cacheWriteTokens"].as_f64().unwrap(),
            },
        };
        let actual = map_event(&options, &summary, usage).unwrap_or(Value::Null);
        assert_eq!(actual, expected, "{event} after {previous}");
    }
}

/// A daemon whose nosh_run takes `run` to finish; everything else is accepted at once.
struct SlowRun {
    run: Duration,
}

impl AgentHost for SlowRun {
    fn submit(
        &self,
        tool: &str,
        _: &str,
        _: &str,
        _: Value,
        _: &str,
    ) -> BoxFuture<Result<Value, String>> {
        let delay = if tool == "nosh_run" {
            self.run
        } else {
            Duration::ZERO
        };
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(json!({ "accepted": true, "exitCode": 0, "jobId": "job_1" }))
        })
    }
    fn emit(&self, _: Value) {}
}

fn captured(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| serde_json::from_str(entry.as_str().unwrap()).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_the_real_pi_loop_placeholder_refused_edits_sleep_wake_and_final_answer() {
    let receipts = Arc::new(Receipts {
        accepted: Mutex::new(Vec::new()),
    });
    // Longer than the 2 s grace, so the run answers with a placeholder, and done after the run that waits for it.
    let host = Arc::new(SlowRun {
        run: Duration::from_millis(2_600),
    });
    let recorded = adapter(
        Some(host),
        Some(Arc::clone(&receipts) as Arc<dyn TerminalHost>),
    )
    .await;
    let adapter = &recorded.adapter;
    let record =
        json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 });
    let envelope = json!({ "$schema": "https://nosh.dev/schemas/terminal-output/v1", "schemaVersion": 1, "records": [record] }).to_string();
    faux_runtime(
        adapter,
        json!({ "provider": "async-proof", "api": "async-proof-api", "models": [{ "id": "deterministic", "reasoning": false }] }),
        json!([
            { "toolCalls": [{ "name": "nosh_run", "arguments": { "commandId": "command_test" }, "id": "call_run" }] },
            { "capture": true, "toolCalls": [{ "name": "nosh_workspace_edit", "arguments": { "path": "a.txt", "oldText": "x", "newText": "y" }, "id": "call_edit" }] },
            { "capture": true, "text": "Waiting for the test run." },
            { "capture": true, "text": envelope },
        ]),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "x\n").unwrap();
    adapter
        .start(options(json!({
            "cwd": directory.path(), "model": { "provider": "async-proof", "id": "deterministic" },
            "taskPermissions": permissions(&["tool_pi.read", "tool_pi.edit", "tool_nosh.run", "tool_nosh.response.submit"], "allowlisted"), "taskWorkspace": workspace(&["**"]),
        })))
        .await
        .unwrap();
    adapter
        .prompt(AGENT, "Run the tests, then report.", None)
        .await
        .unwrap();
    let seen = captured(&adapter.call("test.captured", json!({})).await.unwrap());
    let text = |index: usize| seen[index]["messages"].to_string();
    assert!(
        text(0).contains("nosh_run command_test is still running in the background"),
        "{}",
        text(0)
    );
    assert!(
        text(1).contains("A nosh_run is running in this worktree, so edits are refused"),
        "{}",
        text(1)
    );
    assert!(
        text(2).contains("Async result of nosh_run command_test (call call_run)"),
        "{}",
        text(2)
    );
    assert!(text(2).contains(r#"\"exitCode\":0"#), "{}", text(2));
    assert_eq!(*receipts.accepted.lock().unwrap(), vec![json!([record])]);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "x\n"
    );
    assert_eq!(adapter.inspect()[0].status, "idle");
    adapter.stop(AGENT);
}

/// Rejects the first envelope (a retry is allowed), then accepts while the turn is still active.
struct Correcting {
    adapter: Mutex<std::sync::Weak<PiAdapter>>,
    trace: Mutex<Vec<String>>,
}

impl TerminalHost for Correcting {
    fn submit(
        &self,
        context: &TerminalContext,
        records: Vec<Value>,
    ) -> BoxFuture<Result<Value, String>> {
        assert_eq!(context.task_id.as_deref(), Some(TASK));
        assert_eq!(
            records,
            vec![
                json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 })
            ]
        );
        let adapter = self.adapter.lock().unwrap().upgrade().unwrap();
        assert_eq!(adapter.inspect()[0].status, "running");
        self.trace
            .lock()
            .unwrap()
            .push("accepted-while-active".into());
        Box::pin(async {
            Ok(
                json!({ "accepted": true, "retryAllowed": false, "effect": { "state": "completed" } }),
            )
        })
    }
    fn reject(&self, _: &TerminalContext, reason: String) -> BoxFuture<Result<Value, String>> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("rejected:{reason}"));
        Box::pin(async { Ok(json!({ "accepted": false, "retryAllowed": true })) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_the_real_pi_inner_loop_and_corrects_final_json_before_active_session_cleanup() {
    let host = Arc::new(Correcting {
        adapter: Mutex::new(std::sync::Weak::new()),
        trace: Mutex::new(Vec::new()),
    });
    let recorded = adapter(
        Some(Arc::new(Accepting)),
        Some(Arc::clone(&host) as Arc<dyn TerminalHost>),
    )
    .await;
    *host.adapter.lock().unwrap() = Arc::downgrade(&recorded.adapter);
    let adapter = &recorded.adapter;
    let record =
        json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 });
    let envelope = json!({ "$schema": "https://nosh.dev/schemas/terminal-output/v1", "schemaVersion": 1, "records": [record] }).to_string();
    faux_runtime(
        adapter,
        json!({ "provider": "terminal-proof", "api": "terminal-proof-api", "models": [{ "id": "deterministic", "reasoning": false }] }),
        json!([{ "text": format!("```json\n{envelope}\n```") }, { "capture": true, "text": envelope }]),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    // Unlike the TypeScript test, the session holds a tool, so the correction has one to disable and restore.
    let started = adapter.start(options(json!({
        "cwd": directory.path(), "model": { "provider": "terminal-proof", "id": "deterministic" },
        "taskPermissions": permissions(&["tool_pi.read", "tool_nosh.response.submit"], "disabled"), "taskWorkspace": workspace(&["**"]),
    }))).await.unwrap();
    assert!(!has(&started.active_tool_ids, "nosh_response_submit"));
    assert!(
        has(&started.active_tool_ids, "nosh_workspace_read"),
        "{:?}",
        started.active_tool_ids
    );
    adapter
        .prompt(AGENT, "Return a bounded task failure record.", None)
        .await
        .unwrap();
    let trace = host.trace.lock().unwrap().clone();
    assert_eq!(trace.len(), 2, "{trace:?}");
    assert!(trace[0].starts_with("rejected:"), "{trace:?}");
    assert_eq!(trace[1], "accepted-while-active");
    assert!(
        recorded
            .types()
            .iter()
            .any(|kind| kind == "agent.terminal_receipt")
    );
    // The correction runs with no tools, and says what to correct.
    let seen = captured(&adapter.call("test.captured", json!({})).await.unwrap());
    assert_eq!(seen[0]["tools"], json!([]));
    assert!(
        seen[0]["messages"]
            .to_string()
            .contains("The host rejected the final JSON")
    );
    let after = adapter.inspect()[0].clone();
    assert_eq!(after.status, "idle");
    assert_eq!(after.active_tool_ids, started.active_tool_ids);
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_sleeping_on_an_async_call_refuses_thinking_changes_and_an_abort_ends_its_wake_loop()
{
    let receipts = Arc::new(Receipts {
        accepted: Mutex::new(Vec::new()),
    });
    let run = Duration::from_millis(5_000);
    let recorded = adapter(
        Some(Arc::new(SlowRun { run })),
        Some(Arc::clone(&receipts) as Arc<dyn TerminalHost>),
    )
    .await;
    let adapter = Arc::clone(&recorded.adapter);
    let envelope = json!({ "$schema": "https://nosh.dev/schemas/terminal-output/v1", "schemaVersion": 1, "records": [{ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 }] }).to_string();
    faux_runtime(
        &adapter,
        json!({ "provider": "sleep-proof", "api": "sleep-proof-api", "models": [{ "id": "deterministic", "reasoning": true }] }),
        json!([
            { "toolCalls": [{ "name": "nosh_run", "arguments": { "commandId": "command_test" }, "id": "call_run" }] },
            { "text": "Waiting for the test run." },
            { "capture": true, "text": envelope },
        ]),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    adapter.start(options(json!({
        "cwd": directory.path(), "model": { "provider": "sleep-proof", "id": "deterministic" },
        "taskPermissions": permissions(&["tool_nosh.run", "tool_nosh.response.submit"], "allowlisted"), "taskWorkspace": workspace(&["**"]),
    }))).await.unwrap();
    let began = std::time::Instant::now();
    let turn = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move {
            adapter
                .prompt(AGENT, "Run the tests, then report.", None)
                .await
        })
    };
    // The first run ends on "Waiting…" once the placeholder is back; the turn then sleeps on the run.
    while !recorded
        .types()
        .iter()
        .any(|kind| kind == "agent.completed")
    {
        assert!(
            began.elapsed() < run,
            "the first run did not end before the async call"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(adapter.inspect()[0].status, "running");
    // Pi is not streaming, but the NOSH turn is active.
    let current = adapter.inspect()[0].thinking_level.clone();
    let other = if current == "high" { "low" } else { "high" };
    let refused = adapter.set_thinking_level(AGENT, other).await.unwrap_err();
    assert!(refused.contains("Stop the current Nosh turn"), "{refused}");
    adapter.abort(AGENT).await.unwrap();
    let _ = turn.await.unwrap();
    assert!(
        began.elapsed() < run,
        "the turn waited for the async call after the abort"
    );
    // The run's late result must not wake the model.
    tokio::time::sleep(run - began.elapsed() + Duration::from_millis(1_000)).await;
    let seen = adapter.call("test.captured", json!({})).await.unwrap();
    assert_eq!(seen, json!([]));
    assert!(receipts.accepted.lock().unwrap().is_empty());
    assert_eq!(adapter.inspect()[0].status, "idle");
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_follows_the_live_session_state_during_a_turn() {
    let recorded = adapter(Some(Arc::new(Accepting)), None).await;
    let adapter = Arc::clone(&recorded.adapter);
    faux_runtime(
        &adapter,
        json!({ "provider": "live-proof", "api": "live-proof-api", "models": [{ "id": "deterministic", "reasoning": false }] }),
        json!([
            { "toolCalls": [{ "name": "nosh_workspace_read", "arguments": { "path": "a.txt" }, "id": "call_read" }] },
            { "hold": "response" },
        ]),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "x\n".repeat(400)).unwrap();
    adapter.start(options(json!({
        "taskId": null, "cwd": directory.path(), "model": { "provider": "live-proof", "id": "deterministic" },
        "taskPermissions": permissions(&["tool_pi.read"], "disabled"), "taskWorkspace": workspace(&["**"]),
    }))).await.unwrap();
    let before = adapter.inspect()[0].context_tokens;
    let turn = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.prompt(AGENT, "Read a.txt.", None).await })
    };
    let mut during = before;
    for _ in 0..300 {
        during = adapter.inspect()[0].context_tokens;
        if during != before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Only the session's events can carry this: the prompt has not returned.
    assert!(!turn.is_finished());
    assert_ne!(during, before, "context usage did not follow the turn");
    adapter
        .call(
            "test.release",
            json!({ "hold": "response", "text": "done" }),
        )
        .await
        .unwrap();
    turn.await.unwrap().unwrap();
    adapter.stop(AGENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_the_actual_base_prompt_through_the_resource_loader_to_pi() {
    let recorded = adapter(None, None).await;
    let adapter = &recorded.adapter;
    let directory = tempfile::tempdir().unwrap();
    adapter.call("test.runtime", json!({ "faux": { "provider": "prompt-provider", "api": "prompt-api", "models": [{ "id": "deterministic", "reasoning": false }] } })).await.unwrap();
    for (index, role) in ["nosh", "mission_director", "general_worker"]
        .into_iter()
        .enumerate()
    {
        adapter.call("test.respond", json!({ "steps": [{ "capture": true, "text": "What research question should we study?" }] })).await.unwrap();
        let options = options(
            json!({ "taskId": null, "role": role, "cwd": directory.path(), "model": { "provider": "prompt-provider", "id": "deterministic" } }),
        );
        adapter.start(options.clone()).await.unwrap();
        adapter
            .prompt(AGENT, "Ask a short question in normal chat.", None)
            .await
            .unwrap();
        let seen = captured(&adapter.call("test.captured", json!({})).await.unwrap());
        let supplied = seen[index]["systemPrompt"].as_str().unwrap().to_owned();
        let facts = nosh_agent::prompt::SessionFacts {
            project_id: PROJECT,
            agent_id: AGENT,
            task_id: None,
        };
        assert!(
            supplied.contains(&nosh_agent::prompt::nosh_system_prompt(
                options.role,
                Some(&facts),
                true
            )),
            "{role}"
        );
        assert!(
            supplied.contains(&format!(
                "Session facts: projectId {PROJECT}; your agentId {AGENT}"
            )),
            "{role}"
        );
        assert!(supplied.contains("nosh-control"), "{role}");
        assert!(!supplied.contains("TERMINAL OUTPUT CONTRACT"), "{role}");
        adapter.stop(AGENT);
    }
}
