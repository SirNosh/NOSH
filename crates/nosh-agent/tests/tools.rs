//! Session tools, tool execution, and terminal turns against the built TypeScript pi-adapter
//! (`scripts/pi-adapter-ts.mjs`): tool definitions, one `execute` per case in its own workspace
//! (results, daemon requests, events, and the files left behind), and scripted `runTerminalTurn`
//! scenarios must all match.
//!
//! Model-facing JSON text is compared as parsed JSON, because Rust serializes it with sorted keys.

use nosh_agent::async_calls::{AsyncToolCalls, BoxFuture};
use nosh_agent::policy::SessionOptions;
use nosh_agent::terminal::{PromptOutcome, TerminalContext, TerminalHost, run_terminal_turn};
use nosh_agent::tools::{AgentHost, SessionTools};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TASK: &str = "tsk_0123456789abcdef0123456789abcdef";
const OUTPUT: &str = "https://nosh.dev/schemas/terminal-output/v1";

fn ts_root() -> PathBuf {
    let root = std::env::var_os("NOSH_TS_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
        PathBuf::from,
    );
    nosh_sys::paths::resolve(&root).unwrap()
}

fn run_typescript(ops: &[Value]) -> Vec<Value> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pi-adapter-ts.mjs");
    let mut child = Command::new("node")
        .arg(script)
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

fn options(extra: &Value) -> Value {
    let mut base = json!({
        "projectId": "prj_0123456789abcdef0123456789abcdef", "missionId": "mis_1", "directionId": null,
        "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "taskId": TASK,
        "agentId": "agt_0123456789abcdef0123456789abcdef", "role": "general_worker", "cwd": ".", "packagePath": "pi-package",
        "taskPermissions": { "network": "allowlisted", "subprocess": "allowlisted", "gitCommit": false, "gitPush": false, "delegation": "request_only", "networkAllowlist": ["api.crossref.org"],
            "allowedToolIds": ["tool_pi.read", "tool_pi.edit", "tool_pi.write", "tool_nosh.run", "tool_nosh.network.read", "tool_nosh.blocker.submit"] },
        "taskWorkspace": { "worktreeId": "wt_1", "branch": "nosh/task", "startingCommit": "0123456789abcdef0123456789abcdef01234567", "writeScopes": ["src/**", "notes.md"], "protectedScopes": ["src/secret/**"] },
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn code_mode(env: Option<&Value>) -> bool {
    env.and_then(|env| env.get("NOSH_CODE_MODE"))
        .and_then(Value::as_str)
        != Some("off")
}

// ---- The Rust side of each op.

struct Daemon {
    receipts: Mutex<VecDeque<Value>>,
    submits: Mutex<Vec<Value>>,
    events: Mutex<Vec<Value>>,
}

async fn reply(next: Value) -> Result<Value, String> {
    if let Some(message) = next.get("throw") {
        return Err(message.as_str().unwrap().to_owned());
    }
    if let Some(delay) = next.get("delay").and_then(Value::as_u64) {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    Ok(next["value"].clone())
}

fn next_reply(script: &Mutex<VecDeque<Value>>) -> Value {
    script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| json!({ "value": { "accepted": true } }))
}

impl AgentHost for Daemon {
    fn submit(
        &self,
        tool: &str,
        project_id: &str,
        attempt_key: &str,
        record: Value,
        agent_id: &str,
    ) -> BoxFuture<Result<Value, String>> {
        self.submits
            .lock()
            .unwrap()
            .push(json!([tool, project_id, attempt_key, record, agent_id]));
        Box::pin(reply(next_reply(&self.receipts)))
    }
    fn emit(&self, event: Value) {
        self.events.lock().unwrap().push(event);
    }
}

fn schema_dir() -> PathBuf {
    ts_root()
        .join("packages")
        .join("wire")
        .join("src")
        .join("schemas")
}

fn definitions(options: &Value, code_mode: bool) -> Value {
    let options: SessionOptions = serde_json::from_value(options.clone()).unwrap();
    let daemon = Arc::new(Daemon {
        receipts: Mutex::default(),
        submits: Mutex::default(),
        events: Mutex::default(),
    });
    let tools = SessionTools::new(
        options,
        daemon,
        Arc::new(AsyncToolCalls::default()),
        &schema_dir(),
        code_mode,
    );
    tools.definitions().iter().map(|tool| json!({ "name": tool.name, "label": tool.label, "description": tool.description, "parameters": tool.parameters })).collect()
}

async fn exec(op: &Value, code_mode: bool) -> Value {
    let options: SessionOptions = serde_json::from_value(op["options"].clone()).unwrap();
    let receipts = op
        .get("receipts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let daemon = Arc::new(Daemon {
        receipts: Mutex::new(receipts.into()),
        submits: Mutex::default(),
        events: Mutex::default(),
    });
    let calls = Arc::new(AsyncToolCalls::new(Duration::from_millis(
        op.get("grace").and_then(Value::as_u64).unwrap_or(2_000),
    )));
    let tools = SessionTools::new(
        options,
        Arc::clone(&daemon) as Arc<dyn AgentHost>,
        Arc::clone(&calls),
        &schema_dir(),
        code_mode,
    );
    let mut outcome = Map::new();
    match tools
        .execute(
            op["name"].as_str().unwrap(),
            op["callId"].as_str().unwrap(),
            &op["params"],
        )
        .wait()
        .await
    {
        Ok(result) => outcome.insert(
            "result".into(),
            json!({ "content": result.content, "details": result.details }),
        ),
        Err(error) => outcome.insert("error".into(), json!(error)),
    };
    if op.get("wake") == Some(&json!(true)) {
        outcome.insert(
            "wake".into(),
            json!(calls.next(Duration::from_secs(10)).await),
        );
    }
    outcome.insert("submits".into(), json!(*daemon.submits.lock().unwrap()));
    outcome.insert("events".into(), json!(*daemon.events.lock().unwrap()));
    Value::Object(outcome)
}

struct Scripted {
    admits: Option<Mutex<VecDeque<Value>>>,
    submits: Mutex<VecDeque<Value>>,
    rejects: Mutex<VecDeque<Value>>,
    trace: Arc<Mutex<Vec<Value>>>,
}

impl TerminalHost for Scripted {
    fn admit(&self, _context: &TerminalContext) -> BoxFuture<Result<Option<Value>, String>> {
        let Some(admits) = &self.admits else {
            return Box::pin(async { Ok(None) });
        };
        self.trace.lock().unwrap().push(json!(["admit"]));
        let next = admits.lock().unwrap().pop_front().unwrap_or(Value::Null);
        Box::pin(async move {
            if next.is_null() {
                Ok(None)
            } else {
                reply(next).await.map(Some)
            }
        })
    }
    fn submit(
        &self,
        _context: &TerminalContext,
        records: Vec<Value>,
    ) -> BoxFuture<Result<Value, String>> {
        self.trace.lock().unwrap().push(json!(["submit", records]));
        Box::pin(reply(next_reply(&self.submits)))
    }
    fn reject(
        &self,
        _context: &TerminalContext,
        reason: String,
    ) -> BoxFuture<Result<Value, String>> {
        self.trace.lock().unwrap().push(json!(["reject", reason]));
        Box::pin(reply(next_reply(&self.rejects)))
    }
}

async fn turn(op: &Value) -> Value {
    let context: TerminalContext = serde_json::from_value(op["context"].clone()).unwrap();
    let script = |key: &str| {
        Mutex::new(
            op.get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into(),
        )
    };
    let trace = Arc::new(Mutex::new(Vec::new()));
    let host = Scripted {
        admits: op.get("admits").map(|_| script("admits")),
        submits: script("submits"),
        rejects: script("rejects"),
        trace: Arc::clone(&trace),
    };
    let prompts: Mutex<VecDeque<Value>> = script("prompts");
    let result = run_terminal_turn(&context, &host, |correction| {
        trace.lock().unwrap().push(json!(["prompt", correction]));
        let next = next_reply(&prompts);
        async move {
            let value = reply(next).await?;
            Ok(PromptOutcome {
                text: value["text"].as_str().unwrap_or_default().to_owned(),
                stop_reason: value["stopReason"].as_str().unwrap_or_default().to_owned(),
                cancelled: value["cancelled"] == json!(true),
            })
        }
    })
    .await;
    let trace = trace.lock().unwrap().clone();
    match result {
        Ok(receipt) => json!({ "result": receipt, "trace": trace }),
        Err(error) => json!({ "error": error, "trace": trace }),
    }
}

// ---- Normalization: workspace roots, and JSON text compared as values.

fn replace_root(value: &mut Value, root: &str) {
    match value {
        Value::String(text) => *text = text.replace(root, "<ROOT>"),
        Value::Array(items) => items.iter_mut().for_each(|item| replace_root(item, root)),
        Value::Object(map) => map.values_mut().for_each(|item| replace_root(item, root)),
        _ => {}
    }
}

/// `prefix` + JSON text becomes `{ prefix, json }`.
fn as_json(text: &str) -> Value {
    for prefix in ["Receipt: ", ""] {
        if let Some(rest) = text.strip_prefix(prefix)
            && let Ok(parsed) = serde_json::from_str::<Value>(rest)
            && (rest.starts_with('{') || rest.starts_with('['))
        {
            return json!({ "prefix": prefix, "json": parsed });
        }
    }
    if let Some((head, rest)) = text.split_once(":\n")
        && head.starts_with("Async result of ")
        && let Ok(parsed) = serde_json::from_str::<Value>(rest)
    {
        return json!({ "prefix": head, "json": parsed });
    }
    json!(text)
}

fn normalize(mut outcome: Value) -> Value {
    if let Some(content) = outcome
        .pointer_mut("/result/content")
        .and_then(Value::as_array_mut)
    {
        for block in content {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                block["text"] = as_json(text);
            }
        }
    }
    if let Some(wake) = outcome.get("wake").and_then(Value::as_str) {
        outcome["wake"] = as_json(wake);
    }
    outcome
}

/// Every file under `root`, by relative path, as lossy text.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(
                    relative,
                    String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned(),
                );
            }
        }
    }
    files
}

// ---- Cases.

struct ExecCase {
    files: Vec<(&'static str, Vec<u8>)>,
    hardlink: Option<(&'static str, &'static str)>,
    op: Value,
    /// The error text is the engine's (quickjs-ng differs): compare up to the error name.
    engine_text: bool,
}

fn case(files: &[(&'static str, &[u8])], name: &str, params: Value) -> ExecCase {
    ExecCase {
        files: files
            .iter()
            .map(|(path, body)| (*path, body.to_vec()))
            .collect(),
        hardlink: None,
        op: json!({ "name": name, "callId": "call_1", "params": params }),
        engine_text: false,
    }
}

fn with(mut case: ExecCase, extra: Value) -> ExecCase {
    case.op
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    case
}

const BASE: &[(&str, &[u8])] = &[
    ("src/a.ts", b"export const a = 1;\n// TODO a\n"),
    ("src/b.ts", b"export const b = 2;\n// TODO b\n"),
    ("src/crlf.json", b"{\r\n  \"threshold\": 0.5\r\n}\r\n"),
    ("src/bom.txt", b"\xEF\xBB\xBFhello"),
    ("src/bad.txt", b"ok \xFF\xFE end"),
    ("src/twice.txt", b"x y x"),
    ("notes.md", b"# notes\n"),
    ("src/secret/key.txt", b"secret"),
];

fn code(program: &str) -> Value {
    json!({ "code": program })
}

#[allow(clippy::too_many_lines)]
fn exec_cases() -> Vec<ExecCase> {
    let big = vec![b'a'; 1_000_001];
    let big: &'static [u8] = Box::leak(big.into_boxed_slice());
    let cases = vec![
        // Reads.
        case(BASE, "nosh_workspace_read", json!({ "path": "src/a.ts" })),
        case(
            BASE,
            "nosh_workspace_read",
            json!({ "path": "src/bom.txt" }),
        ),
        case(
            BASE,
            "nosh_workspace_read",
            json!({ "path": "src/bad.txt" }),
        ),
        case(
            BASE,
            "nosh_workspace_read",
            json!({ "path": "src/none.ts" }),
        ),
        case(BASE, "nosh_workspace_read", json!({ "path": "src" })),
        case(
            BASE,
            "nosh_workspace_read",
            json!({ "path": "../outside.txt" }),
        ),
        case(
            BASE,
            "nosh_workspace_read",
            json!({ "path": ".git/config" }),
        ),
        case(
            BASE,
            "nosh_workspace_read",
            json!({ "path": "src/secret/key.txt" }),
        ),
        case(
            &[("src/big.txt", big)],
            "nosh_workspace_read",
            json!({ "path": "src/big.txt" }),
        ),
        ExecCase {
            hardlink: Some(("src/a.ts", "src/linked.ts")),
            ..case(
                BASE,
                "nosh_workspace_read",
                json!({ "path": "src/linked.ts" }),
            )
        },
        // Writes.
        case(
            BASE,
            "nosh_workspace_write",
            json!({ "path": "src/new/deep.ts", "content": "fresh\n" }),
        ),
        case(
            BASE,
            "nosh_workspace_write",
            json!({ "path": "notes.md", "content": "é 😀\n" }),
        ),
        case(
            BASE,
            "nosh_workspace_write",
            json!({ "path": "outside.md", "content": "x" }),
        ),
        case(
            BASE,
            "nosh_workspace_write",
            json!({ "path": "src/secret/key.txt", "content": "x" }),
        ),
        case(
            BASE,
            "nosh_workspace_write",
            json!({ "path": "src/a.ts", "content": "é".repeat(1_000_001) }),
        ),
        case(
            BASE,
            "nosh_workspace_write",
            json!({ "path": "SRC/A.ts", "content": "case" }),
        ),
        // Edits.
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/crlf.json", "oldText": "  \"threshold\": 0.5\n}", "newText": "  \"threshold\": 0.6\n}" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/crlf.json", "oldText": "0.5\r\n}", "newText": "0.7\n}" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/a.ts", "oldText": "a = 1", "newText": "a = 10" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/a.ts", "oldText": "missing", "newText": "x" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/twice.txt", "oldText": "x", "newText": "z" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/twice.txt", "oldText": "", "newText": "z" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/none.ts", "oldText": "a", "newText": "b" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/bom.txt", "oldText": "hello", "newText": "bye" }),
        ),
        case(
            BASE,
            "nosh_workspace_edit",
            json!({ "path": "src/a.ts", "oldText": "a", "newText": "é".repeat(1_000_000) }),
        ),
        ExecCase {
            hardlink: Some(("src/a.ts", "src/linked.ts")),
            ..case(
                BASE,
                "nosh_workspace_edit",
                json!({ "path": "src/linked.ts", "oldText": "a", "newText": "b" }),
            )
        },
        // Typed submissions and daemon reads.
        with(
            case(
                BASE,
                "nosh_blocker_submit",
                json!({ "record": { "answer": 1 } }),
            ),
            json!({ "receipts": [{ "value": { "accepted": true, "veryLargeProjection": "x".repeat(10_000) } }] }),
        ),
        with(
            case(
                BASE,
                "nosh_blocker_submit",
                json!({ "record": { "answer": 1 } }),
            ),
            json!({ "receipts": [{ "value": { "accepted": 0, "issues": ["bad"] } }] }),
        ),
        with(
            case(BASE, "nosh_blocker_submit", json!({ "record": {} })),
            json!({ "receipts": [{ "throw": "daemon unavailable" }] }),
        ),
        with(
            case(
                BASE,
                "nosh_artifact_read",
                json!({ "artifactId": "art_1", "offset": 3 }),
            ),
            json!({ "receipts": [{ "value": { "artifactId": "art_1", "text": "body" } }] }),
        ),
        case(BASE, "nosh_artifact_read", json!({ "artifactId": "art_1" })),
        with(
            case(BASE, "nosh_run", json!({ "commandId": "command_test" })),
            json!({ "receipts": [{ "value": { "accepted": true, "exitCode": 0, "stdoutTail": "1 passed" } }] }),
        ),
        with(
            case(BASE, "nosh_run", json!({ "commandId": "command_test" })),
            json!({ "receipts": [{ "throw": "job failed" }] }),
        ),
        with(
            case(BASE, "nosh_run", json!({ "commandId": "command_slow" })),
            json!({ "grace": 50, "wake": true, "receipts": [{ "delay": 300, "value": { "accepted": true, "exitCode": 1 } }] }),
        ),
        // The network reader's refusals (no request is made).
        case(
            BASE,
            "nosh_network_read",
            json!({ "url": "http://api.crossref.org/works" }),
        ),
        case(
            BASE,
            "nosh_network_read",
            json!({ "url": "https://example.invalid/works" }),
        ),
        case(
            BASE,
            "nosh_network_read",
            json!({ "url": "https://api.crossref.org:444/works" }),
        ),
        case(
            BASE,
            "nosh_network_read",
            json!({ "url": "https://user:pass@api.crossref.org/works" }),
        ),
        case(BASE, "nosh_network_read", json!({ "url": "not a url" })),
        // nosh_code.
        case(
            BASE,
            "nosh_code",
            code(
                r#"const files = await Promise.all(["src/a.ts", "src/b.ts"].map((path) => tools.read(path)));
            let escaped; try { await tools.read("../outside.txt"); } catch (error) { escaped = error.message; }
            await tools.edit("src/a.ts", "a = 1", "a = 10");
            return { todos: files.map((text) => text.split(String.fromCharCode(10)).filter((line) => line.includes("TODO"))), escaped, functions: Object.keys(tools) };"#,
            ),
        ),
        case(BASE, "nosh_code", code(r#"throw new Error("nope")"#)),
        case(
            BASE,
            "nosh_code",
            code(r#"console.log("a", 1); console.log({ b: [2] }); throw "plain""#),
        ),
        case(
            BASE,
            "nosh_code",
            code(
                r#"const out = []; for (const call of [() => tools.read(5), () => tools.read(""), () => tools.write("src/x.ts", 5), () => tools.edit("src/a.ts", "a"), () => tools.artifact("art_1", -1), () => tools.artifact("art_1", 1.5), () => tools.artifact("art_1", null), () => tools.artifact("art", 0), () => tools.run(""), () => tools.fetch("http://x")]) { try { out.push(await call()); } catch (error) { out.push(error.message); } } return out;"#,
            ),
        ),
        with(
            case(
                BASE,
                "nosh_code",
                code(
                    r#"const [artifact, run] = await Promise.all([tools.artifact("art_12345", 2), tools.run("command_test")]); await tools.write("src/w.ts", "w"); return { artifact, run, written: await tools.read("src/w.ts") };"#,
                ),
            ),
            json!({ "receipts": [{ "value": { "artifactId": "art_12345" } }, { "value": { "accepted": true, "exitCode": 0 } }] }),
        ),
        with(
            case(
                BASE,
                "nosh_code",
                code(
                    r#"const run = tools.run("command_test");
            let refused; try { await tools.edit("src/a.ts", "a = 1", "a = 2"); } catch (error) { refused = error.message; }
            const result = await run; return { exitCode: result.exitCode, refused };"#,
                ),
            ),
            json!({ "grace": 50, "wake": true, "receipts": [{ "delay": 300, "value": { "accepted": true, "exitCode": 0, "stdoutTail": "1 passed" } }] }),
        ),
        case(
            BASE,
            "nosh_code",
            code(r#"return await tools.fetch("https://api.crossref.org:444/x")"#),
        ),
        ExecCase {
            engine_text: true,
            ..case(BASE, "nosh_code", code("return 1n"))
        },
    ];
    cases
}

fn turn_cases() -> Vec<Value> {
    let context = json!({ "projectId": "prj_1", "agentId": "agt_1", "taskId": TASK, "instructionId": null, "threadId": null, "expectedVersion": null, "turnId": "turn_1", "allowedTools": ["nosh_response_submit"] });
    let record =
        json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 });
    let good = serde_json::to_string(
        &json!({ "$schema": OUTPUT, "schemaVersion": 1, "records": [record] }),
    )
    .unwrap();
    // One stray closer after the record: the parser repairs it.
    let repaired = good.replacen("1}]", "1}}]", 1);
    assert_ne!(repaired, good);
    let texts = [
        good.clone(),
        format!("```json\n{good}\n```"),
        "bad".to_owned(),
        format!("{good}]}}"),
        repaired,
        "{\"a\":1}".to_owned(),
    ];
    let prompt_choices: Vec<Value> = texts
        .iter()
        .map(|text| json!({ "value": { "text": text, "stopReason": "stop" } }))
        .chain([
            json!({ "value": { "text": good, "stopReason": "length" } }),
            json!({ "value": { "text": good, "stopReason": "stop", "cancelled": true } }),
            json!({ "throw": "provider down" }),
        ])
        .collect();
    let receipt_choices = [
        json!({ "value": { "accepted": true, "retryAllowed": false, "effect": { "state": "completed" } } }),
        json!({ "value": { "accepted": false, "retryAllowed": true, "issues": [{ "path": "records/0", "message": "bad" }] } }),
        json!({ "value": { "accepted": false, "retryAllowed": false } }),
        json!({ "value": { "accepted": false, "retryAllowed": true, "note": "é😀".repeat(1_500) } }),
        json!({ "throw": "host failed" }),
    ];
    let admit_choices = [
        None,
        Some(json!([null, null])),
        Some(json!([{ "value": { "accepted": true, "replayed": true } }])),
        Some(json!([null, { "value": { "accepted": false, "state": "closed" } }])),
    ];
    let mut random = 0x9E37_79B9_7F4A_7C15_u64;
    let mut pick = |limit: usize| {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        usize::try_from(random % limit as u64).unwrap()
    };
    let mut cases = Vec::new();
    for _ in 0..400 {
        let mut op = json!({
            "context": context,
            "prompts": [prompt_choices[pick(prompt_choices.len())], prompt_choices[pick(prompt_choices.len())]],
            "submits": [receipt_choices[pick(receipt_choices.len())], receipt_choices[pick(receipt_choices.len())]],
            "rejects": [receipt_choices[pick(receipt_choices.len())], receipt_choices[pick(receipt_choices.len())]],
        });
        if let Some(admits) = &admit_choices[pick(admit_choices.len())] {
            op["admits"] = admits.clone();
        }
        cases.push(op);
    }
    cases
}

fn definition_variants() -> Vec<(Value, Option<Value>)> {
    let read_only = json!({ "taskPermissions": { "network": "disabled", "subprocess": "disabled", "gitCommit": false, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": ["tool_pi.read"] } });
    let off = Some(json!({ "NOSH_CODE_MODE": "off" }));
    vec![
        (options(&json!({})), None),
        (options(&json!({})), off.clone()),
        (options(&read_only), None),
        (options(&json!({ "taskWorkspace": null })), None),
        (options(&json!({ "taskId": null })), None),
        (
            options(
                &json!({ "taskId": null, "taskWorkspace": null, "taskPermissions": null, "role": "nosh" }),
            ),
            None,
        ),
        (
            options(
                &json!({ "taskId": null, "runtimeScoped": true, "taskWorkspace": null, "taskPermissions": null, "role": "research_director" }),
            ),
            None,
        ),
        (
            options(&json!({ "taskPermissions": null, "taskWorkspace": null, "role": "reviewer" })),
            None,
        ),
        (
            options(&json!({ "taskPermissions": null, "taskWorkspace": null, "role": "reviewer" })),
            off,
        ),
        (
            options(&json!({ "tools": ["nosh_workspace_read", "nosh_run"] })),
            None,
        ),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn session_tools_match_typescript() {
    let directory = tempfile::tempdir().unwrap();
    let cases = exec_cases();
    let roots: Vec<(PathBuf, PathBuf)> = (0..cases.len())
        .map(|index| {
            (
                directory.path().join(format!("ts{index}")),
                directory.path().join(format!("rs{index}")),
            )
        })
        .collect();
    for (case, (ts, rs)) in cases.iter().zip(&roots) {
        for root in [ts, rs] {
            for (path, body) in &case.files {
                let file = root.join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, body).unwrap();
            }
            if let Some((target, link)) = case.hardlink {
                std::fs::hard_link(root.join(target), root.join(link)).unwrap();
            }
        }
    }
    let exec_op = |case: &ExecCase, root: &Path| {
        let mut op = case.op.clone();
        op["options"] = options(&json!({ "cwd": root.to_str().unwrap() }));
        op
    };
    let definitions_ops: Vec<Value> = definition_variants()
        .into_iter()
        .map(|(options, env)| json!({ "tools": options, "env": env }))
        .collect();
    let mut ops = definitions_ops.clone();
    ops.extend(
        cases
            .iter()
            .zip(&roots)
            .map(|(case, (ts, _))| json!({ "exec": exec_op(case, ts) })),
    );
    let turns = turn_cases();
    ops.extend(turns.iter().map(|turn| json!({ "turn": turn })));
    let expected = run_typescript(&ops);

    let mut mismatches = Vec::new();
    let mut compared = 0;
    for (index, op) in definitions_ops.iter().enumerate() {
        let actual = definitions(&op["tools"], code_mode(op.get("env")));
        compared += 1;
        if actual != expected[index] {
            mismatches.push(format!(
                "definitions {index}:\n  rust: {actual}\n  ts:   {}",
                expected[index]
            ));
        }
    }
    for (offset, (case, (ts, rs))) in cases.iter().zip(&roots).enumerate() {
        let mut ts_outcome = expected[definitions_ops.len() + offset].clone();
        let mut rs_outcome = exec(&exec_op(case, rs), true).await;
        replace_root(&mut ts_outcome, ts.to_str().unwrap());
        replace_root(&mut rs_outcome, rs.to_str().unwrap());
        let (mut ts_outcome, mut rs_outcome) = (normalize(ts_outcome), normalize(rs_outcome));
        if case.engine_text {
            for outcome in [&mut ts_outcome, &mut rs_outcome] {
                let error = outcome["error"]
                    .as_str()
                    .unwrap()
                    .split(": ")
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(": ");
                outcome["error"] = json!(error);
            }
        }
        compared += 1;
        if ts_outcome != rs_outcome || snapshot(ts) != snapshot(rs) {
            mismatches.push(format!(
                "exec {offset} {}:\n  rust: {rs_outcome}\n  ts:   {ts_outcome}\n  files equal: {}",
                case.op["name"],
                snapshot(ts) == snapshot(rs)
            ));
        }
    }
    let turn_offset = definitions_ops.len() + cases.len();
    for (offset, op) in turns.iter().enumerate() {
        let actual = normalize(turn(op).await);
        compared += 1;
        if actual != normalize(expected[turn_offset + offset].clone()) {
            mismatches.push(format!(
                "turn {offset} {op}:\n  rust: {actual}\n  ts:   {}",
                expected[turn_offset + offset]
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {compared} differ:\n{}",
        mismatches.len(),
        mismatches
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(compared > 450, "{compared}");
    assert!(
        expected[turn_offset..]
            .iter()
            .any(|outcome| outcome.pointer("/result/repaired").is_some()),
        "some accepted turn was repaired"
    );
}
