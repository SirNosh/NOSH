//! Black-box conformance of the local API. One scenario runs against `nosh daemon` and, when the
//! TypeScript tree is built (`NOSH_TS_ROOT`, else this worktree's parent), against
//! `node apps/noshd/dist/main.js`; their responses must agree once IDs, times, hashes, and the
//! scratch directory are masked. The scenario needs no model account: it stays clear of running
//! Missions and active Directions, whose supervisors would start sessions.
//!
//! Needs `npm ci` in `sidecar/pi-host`.

use futures_util::StreamExt;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BOOTSTRAP: &str = "conformance-bootstrap-token";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Implementation {
    Rust,
    TypeScript,
}

fn ts_root() -> PathBuf {
    std::env::var_os("NOSH_TS_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
        Into::into,
    )
}

fn ts_main() -> PathBuf {
    ts_root().join("apps/noshd/dist/main.js")
}

/// Whether the TypeScript build `file` is present. CI always builds it, so there a missing build
/// fails instead of silently skipping the comparison.
fn ts_built(file: &Path) -> bool {
    let built = file.is_file();
    assert!(
        built || std::env::var_os("CI").is_none(),
        "CI builds the TypeScript reference, but {} is missing",
        file.display()
    );
    if !built {
        eprintln!("TypeScript build missing at {}; skipped", file.display());
    }
    built
}

/// A daemon process on a free loopback port, with its stdout log.
struct Daemon {
    child: Child,
    base: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn spawn(implementation: Implementation, data: &Path) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = match implementation {
            Implementation::Rust => {
                let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
                command.arg("daemon");
                command
            }
            Implementation::TypeScript => {
                let mut command = Command::new("node");
                command.arg(ts_main());
                command
            }
        };
        let mut child = command
            .arg(format!("--data-dir={}", data.display()))
            .arg(format!("--port={port}"))
            .arg(format!("--bootstrap-token={BOOTSTRAP}"))
            .env_remove("NOSH_DEFAULT_MODEL")
            .env_remove("NOSH_BOOTSTRAP_TOKEN")
            .current_dir(data.parent().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (lines, stdout) = (Arc::clone(&log), child.stdout.take().unwrap());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        });
        let started = Instant::now();
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("noshd listening on"))
        {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("{implementation:?} daemon exited with {status} before listening");
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "{implementation:?} daemon did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            log,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Masks what legitimately differs between two runs: IDs (numbered by first appearance), times,
/// digests, commit SHAs, and the scratch directory.
struct Masks {
    root: Vec<String>,
    ids: HashMap<String, String>,
}

static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-z]+)_[0-9a-f]{32}\b").unwrap());
static TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap()
});
static DIGEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"sha256:[0-9a-f]{64}").unwrap());
static COMMIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{40}\b").unwrap());

impl Masks {
    fn new(root: &Path) -> Self {
        let canonical = nosh_like_realpath(root);
        let mut forms = vec![root.display().to_string(), canonical];
        forms.extend(
            forms
                .clone()
                .iter()
                .map(|form| form.replace('\\', "/"))
                .collect::<Vec<_>>(),
        );
        forms.extend(
            forms
                .clone()
                .iter()
                .map(|form| form.replace('\\', "\\\\"))
                .collect::<Vec<_>>(),
        );
        forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
        forms.dedup();
        Self {
            root: forms,
            ids: HashMap::new(),
        }
    }

    fn text(&mut self, text: &str) -> String {
        let mut text = text.to_owned();
        for form in &self.root {
            text = text.replace(form.as_str(), "<root>");
        }
        let ids = &mut self.ids;
        let text = ID
            .replace_all(&text, |found: &regex::Captures| {
                let next = ids.len();
                ids.entry(found[0].to_owned())
                    .or_insert_with(|| format!("<{}#{next}>", &found[1]))
                    .clone()
            })
            .into_owned();
        let text = TIME.replace_all(&text, "<time>");
        let text = DIGEST.replace_all(&text, "<sha256>");
        COMMIT.replace_all(&text, "<commit>").into_owned()
    }

    fn value(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (self.text(key), self.value(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// The scratch directory as both daemons resolve it (short names expanded, verbatim prefix
/// dropped).
fn nosh_like_realpath(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap();
    let text = canonical.display().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// What one run of the scenario observed, step by step.
struct Run {
    client: reqwest::Client,
    base: String,
    masks: Masks,
    steps: Vec<(String, Value)>,
}

impl Run {
    async fn call(
        &mut self,
        label: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Value {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_owned());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        let observed = json!({ "status": status, "body": self.masks.value(&body) });
        self.steps.push((label.to_owned(), observed));
        body
    }

    async fn get(&mut self, label: &str, path: &str) -> Value {
        self.call(label, reqwest::Method::GET, path, None, &[])
            .await
    }

    async fn post(&mut self, label: &str, path: &str, body: &Value) -> Value {
        let body = body.to_string();
        self.call(label, reqwest::Method::POST, path, Some(&body), &[])
            .await
    }

    async fn put(&mut self, label: &str, path: &str, body: &Value) -> Value {
        let body = body.to_string();
        self.call(label, reqwest::Method::PUT, path, Some(&body), &[])
            .await
    }

    fn note(&mut self, label: &str, observed: Value) {
        let observed = self.masks.value(&observed);
        self.steps.push((label.to_owned(), observed));
    }
}

/// Opens a WebSocket on `/events`; `Err` when the daemon refuses it.
async fn socket(
    base: &str,
    path: &str,
    headers: &[(&'static str, &str)],
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::Error,
> {
    let address = base.trim_start_matches("http://");
    let mut request = format!("ws://{address}{path}")
        .into_client_request()
        .unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(*name, value.parse().unwrap());
    }
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    tokio_tungstenite::client_async(request, stream)
        .await
        .map(|(socket, _)| socket)
}

async fn scenario(implementation: Implementation, scratch: &Path) -> Vec<(String, Value)> {
    let repository = scratch.join("repository");
    std::fs::create_dir_all(repository.join("docs")).unwrap();
    std::fs::write(repository.join("docs/paper.md"), "# Conformance\n").unwrap();
    std::fs::write(repository.join("docs/paper.bib"), "").unwrap();
    let daemon = Daemon::spawn(implementation, &scratch.join("data"));
    let mut run = Run {
        client: reqwest::Client::new(),
        base: daemon.base.clone(),
        masks: Masks::new(scratch),
        steps: Vec::new(),
    };
    let get = reqwest::Method::GET;
    let post = reqwest::Method::POST;
    let attacker = ("origin", "http://attacker.invalid");

    // Authentication.
    run.get("health", "/health").await;
    run.get("health under /api", "/api/health").await;
    run.call("cross-origin", get.clone(), "/projects", None, &[attacker])
        .await;
    run.call(
        "session without bootstrap",
        post.clone(),
        "/session",
        None,
        &[],
    )
    .await;
    run.call(
        "session with a wrong bootstrap",
        post.clone(),
        "/session",
        None,
        &[("authorization", "Bearer wrong")],
    )
    .await;
    let bootstrap = format!("Bearer {BOOTSTRAP}");
    let session = run
        .call(
            "session",
            post.clone(),
            "/session",
            None,
            &[("authorization", &bootstrap)],
        )
        .await;
    let token = session["token"].as_str().unwrap_or_default().to_owned();
    // The token is random: the step keeps its shape, compared below.
    if let Some((_, observed)) = run.steps.last_mut() {
        observed["body"]["token"] = json!("<token>");
    }
    run.note("session token shape", json!({ "length": token.len(), "base64url": token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') }));
    let bearer = format!("Bearer {token}");
    run.call(
        "cross-origin with a session",
        get.clone(),
        "/projects",
        None,
        &[attacker, ("authorization", &bearer)],
    )
    .await;
    run.get("unknown route", "/no-such-route").await;
    run.call("method mismatch", post.clone(), "/health", None, &[])
        .await;

    // Projects.
    let project_id = format!("prj_{}", "c".repeat(32));
    let database = scratch
        .join("data/projects")
        .join(&project_id)
        .join("nosh.sqlite");
    run.post(
        "register",
        "/projects",
        &json!({ "projectId": project_id, "repositoryRoot": repository, "databasePath": database }),
    )
    .await;
    run.get("projects", "/projects").await;
    run.post(
        "register without a root",
        "/projects",
        &json!({ "projectId": project_id }),
    )
    .await;
    run.post("open without a path", "/projects/open", &json!({}))
        .await;
    run.post(
        "open without a repository mode",
        "/projects/open",
        &json!({ "path": repository, "workingTitle": "T" }),
    )
    .await;
    run.post("open with a bad model", "/projects/open", &json!({ "path": repository, "workingTitle": "T", "createRepository": false, "model": { "provider": "p" } })).await;

    // Request validation.
    run.call("malformed JSON", post.clone(), "/chat", Some("{"), &[])
        .await;
    run.call("empty body", post.clone(), "/chat", Some(""), &[])
        .await;
    run.post("null body", "/chat", &Value::Null).await;
    run.post(
        "short idempotency key",
        "/chat",
        &json!({ "projectId": project_id, "message": "hi", "idempotencyKey": "short" }),
    )
    .await;
    run.post("blank message", "/chat", &json!({ "projectId": project_id, "message": "  ", "idempotencyKey": "chat-blank-message-0001" })).await;
    run.post("thinking level without a model", "/chat", &json!({ "projectId": project_id, "message": "hi", "idempotencyKey": "chat-level-only-0001", "thinkingLevel": "high" })).await;
    run.post("mismatched thinking levels", "/chat", &json!({ "projectId": project_id, "message": "hi", "idempotencyKey": "chat-level-mismatch-01", "model": { "provider": "p", "id": "m", "thinkingLevel": "low" }, "thinkingLevel": "high" })).await;
    run.get("threads without a project", "/threads").await;
    run.get(
        "negative cursor",
        &format!("/events?projectId={project_id}&after=-1"),
    )
    .await;
    run.get(
        "limit too large",
        &format!("/events?projectId={project_id}&limit=1001"),
    )
    .await;
    run.get(
        "limit not a number",
        &format!("/events?projectId={project_id}&limit=abc"),
    )
    .await;
    run.get(
        "recent invalid",
        &format!("/events?projectId={project_id}&recent=maybe"),
    )
    .await;
    run.get(
        "recent after a cursor",
        &format!("/events?projectId={project_id}&recent=true&after=1"),
    )
    .await;
    run.put("default model missing", "/default-model", &json!({}))
        .await;
    run.put(
        "default model malformed",
        "/default-model",
        &json!({ "model": "malformed" }),
    )
    .await;
    run.post(
        "unknown agent",
        "/agents/agt_missing/prompt",
        &json!({ "message": "hi" }),
    )
    .await;
    run.post("agent without action", "/agents/x", &json!({}))
        .await;
    run.post("job action without keys", "/jobs/job_x/cancel", &json!({}))
        .await;
    run.post(
        "job action unsupported",
        "/jobs/job_x/pause",
        &json!({ "projectId": project_id, "idempotencyKey": "job-pause-unsupported-01" }),
    )
    .await;
    run.get("job without a project", "/jobs/job_x").await;
    run.post(
        "restore with a malformed id",
        "/backups/%ZZ/restore",
        &json!({ "projectId": project_id }),
    )
    .await;
    run.post("evidence not an object", "/evidence", &json!({ "projectId": project_id, "idempotencyKey": "evidence-not-object-01", "evidence": [1] })).await;
    run.post("claim not an object", "/claims", &json!({ "projectId": project_id, "idempotencyKey": "claim-not-object-0001", "claim": "x" })).await;
    run.post("acknowledge without ids", "/notifications/acknowledge", &json!({ "projectId": project_id, "idempotencyKey": "acknowledge-no-ids-01", "eventIds": [1] })).await;
    run.put("paper without hashes", "/paper", &json!({ "projectId": project_id, "idempotencyKey": "paper-no-hashes-0001", "markdown": "x", "bibliography": "" })).await;

    // Missions and Directions, short of running.
    let missions_invalid = json!({ "projectId": project_id, "title": "M", "objective": "O", "idempotencyKey": "mission-invalid-arrays", "deliverables": "x" });
    run.post("mission without arrays", "/missions", &missions_invalid)
        .await;
    let mission = run.post("mission", "/missions", &json!({ "projectId": project_id, "title": "Conformance Mission", "objective": "Compare the daemons", "deliverables": ["A comparison"], "successCriteria": ["Both agree"], "idempotencyKey": "conformance-mission-0001" })).await;
    let mission_id = mission["mission"]["entityId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    run.post("mission replay", "/missions", &json!({ "projectId": project_id, "title": "Conformance Mission", "objective": "Compare the daemons", "deliverables": ["A comparison"], "successCriteria": ["Both agree"], "idempotencyKey": "conformance-mission-0001" })).await;
    run.get(
        "mission",
        &format!("/missions/{mission_id}?projectId={project_id}"),
    )
    .await;
    run.post("mission to planning", &format!("/missions/{mission_id}/transition"), &json!({ "projectId": project_id, "expectedVersion": 1, "next": "planning", "idempotencyKey": "conformance-planning-01" })).await;
    run.post("mission stale version", &format!("/missions/{mission_id}/transition"), &json!({ "projectId": project_id, "expectedVersion": 1, "next": "awaiting_approval", "idempotencyKey": "conformance-stale-0001" })).await;
    run.post("mission bare stop", &format!("/missions/{mission_id}/transition"), &json!({ "projectId": project_id, "expectedVersion": 2, "next": "stopping", "idempotencyKey": "conformance-stopping-01" })).await;
    run.post("mission leased node", &format!("/missions/{mission_id}/nodes/n/transition"), &json!({ "projectId": project_id, "expectedVersion": 2, "next": "leased", "idempotencyKey": "conformance-leased-0001" })).await;
    run.post("mission control invalid", &format!("/missions/{mission_id}/control"), &json!({ "projectId": project_id, "expectedVersion": 2, "action": "explode", "mode": "safe", "idempotencyKey": "conformance-control-01" })).await;
    run.get(
        "mission completion",
        &format!("/missions/{mission_id}/completion?projectId={project_id}"),
    )
    .await;
    run.get("missions", &format!("/missions?projectId={project_id}"))
        .await;
    run.get(
        "graph proposals",
        &format!("/graph-proposals?projectId={project_id}"),
    )
    .await;
    let direction = run.post("direction", "/directions", &json!({ "projectId": project_id, "question": "Do they agree?", "decisionUse": "Ship the port", "idempotencyKey": "conformance-direction-01" })).await;
    let direction_id = direction["direction"]["entityId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    run.post("direction proposed", &format!("/directions/{direction_id}/transition"), &json!({ "projectId": project_id, "expectedVersion": 1, "next": "proposed", "idempotencyKey": "conformance-proposed-01" })).await;
    run.get("directions", &format!("/directions?projectId={project_id}"))
        .await;
    run.post("autoresearch without a baseline", "/autoresearch", &json!({ "projectId": project_id, "directionId": direction_id, "decisionQuestion": "Is it faster?", "idempotencyKey": "conformance-autoresearch-1" })).await;
    run.post("autoresearch tags not arrays", "/autoresearch", &json!({ "projectId": project_id, "decisionQuestion": "Is it faster?", "idempotencyKey": "conformance-autoresearch-2", "familyTags": "x" })).await;
    run.get(
        "autoresearch",
        &format!("/autoresearch?projectId={project_id}"),
    )
    .await;

    // Records, paper, notifications.
    let paper = run
        .get("paper", &format!("/paper?projectId={project_id}"))
        .await;
    let expected = json!({ "markdownHash": paper["markdownHash"], "bibliographyHash": paper["bibliographyHash"], "version": paper["version"] });
    run.put("paper save", "/paper", &json!({ "projectId": project_id, "markdown": "# Revised\n", "bibliography": paper["bibliography"], "expected": expected, "idempotencyKey": "conformance-paper-0001" })).await;
    run.put("paper stale", "/paper", &json!({ "projectId": project_id, "markdown": "# Stale\n", "bibliography": paper["bibliography"], "expected": expected, "idempotencyKey": "conformance-paper-0002" })).await;
    run.get("paper revised", &format!("/paper?projectId={project_id}"))
        .await;
    for (label, path) in [
        ("records", "/records"),
        ("reviews", "/reviews"),
        ("evidence", "/evidence"),
        ("claims", "/claims"),
        ("notification acks", "/notification-acks"),
        ("threads", "/threads"),
        ("episodes", "/episodes"),
        ("skills", "/skills"),
        ("programs", "/programs"),
        ("program states", "/programs/states"),
        ("research map", "/research-map"),
        ("backups", "/backups"),
        ("agents", "/agents"),
        ("jobs", "/jobs"),
    ] {
        run.get(label, &format!("{path}?projectId={project_id}"))
            .await;
    }
    run.get("activity", "/activity").await;
    let page = run
        .get("events", &format!("/events?projectId={project_id}&after=0"))
        .await;
    let first = page["events"][0]["eventId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    run.post("acknowledge", "/notifications/acknowledge", &json!({ "projectId": project_id, "idempotencyKey": "conformance-acknowledge", "eventIds": [first] })).await;
    run.get(
        "notification acks after",
        &format!("/notification-acks?projectId={project_id}"),
    )
    .await;
    run.get(
        "recent events",
        &format!("/events?projectId={project_id}&recent=true&limit=3"),
    )
    .await;

    // The event stream.
    let history = page["events"].as_array().cloned().unwrap_or_default();
    let mut stream = socket(
        &daemon.base,
        &format!("/events?projectId={project_id}&after=0"),
        &[],
    )
    .await
    .unwrap();
    let mut streamed = Vec::new();
    while streamed.len() < history.len() {
        let message = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        streamed.push(json!([event["sequence"], event["type"]]));
    }
    run.note("streamed history", Value::Array(streamed));
    let refused = socket(
        &daemon.base,
        &format!("/events?projectId={project_id}&after=0"),
        &[attacker],
    )
    .await
    .is_err();
    let offered = socket(
        &daemon.base,
        &format!("/events?projectId={project_id}&after=0"),
        &[
            attacker,
            ("sec-websocket-protocol", &format!("auth.{token}")),
        ],
    )
    .await
    .is_ok();
    run.note(
        "socket authorization",
        json!({ "crossOriginRefused": refused, "sessionSubprotocolAccepted": offered }),
    );

    // Shutdown, with the event socket still open and answering as a live client's does: a
    // daemon finishes closing only once the socket's closing handshake does.
    let client = tokio::spawn(async move {
        let mut close = json!("no Close frame");
        while let Some(Ok(message)) = stream.next().await {
            if let tokio_tungstenite::tungstenite::Message::Close(frame) = message {
                close = json!(frame.map_or("no status code".to_owned(), |frame| {
                    u16::from(frame.code).to_string()
                }));
            }
        }
        close
    });
    run.post("shutdown", "/shutdown", &json!({})).await;
    let mut daemon = daemon;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{implementation:?} daemon did not stop"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let close = tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .expect("the socket ends with the daemon")
        .unwrap();
    // The log thread drains stdout once the process has gone.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = daemon.log.lock().unwrap().clone();
    let shape = |line: &str| TIME.replace(line, "<time>").into_owned();
    let lifecycle: Vec<String> = log
        .iter()
        .filter(|line| line.contains("noshd "))
        .map(|line| {
            run.masks
                .text(&shape(line))
                .replace(&daemon.base["http://127.0.0.1:".len()..], "<port>")
        })
        .collect();
    run.note(
        "process",
        json!({ "exitCode": status.code(), "lifecycle": lifecycle, "socketClose": close }),
    );
    run.steps
}

#[test]
fn rust_and_typescript_daemons_answer_the_api_alike() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let rust_dir = tempfile::tempdir().unwrap();
    let ours = runtime.block_on(scenario(Implementation::Rust, rust_dir.path()));
    let failures: Vec<&String> = ours
        .iter()
        .filter(|(_, observed)| {
            observed["status"]
                .as_u64()
                .is_some_and(|status| status >= 500)
        })
        .map(|(label, _)| label)
        .collect();
    assert!(failures.is_empty(), "server errors: {failures:?}");
    if !ts_built(&ts_main()) {
        return;
    }
    let ts_dir = tempfile::tempdir().unwrap();
    let theirs = runtime.block_on(scenario(Implementation::TypeScript, ts_dir.path()));
    let mut differences = Vec::new();
    for ((label, rust), (ts_label, ts)) in ours.iter().zip(&theirs) {
        assert_eq!(label, ts_label);
        if rust != ts {
            differences.push(format!(
                "{label}:\n  rust:       {rust}\n  typescript: {ts}"
            ));
        }
    }
    assert_eq!(ours.len(), theirs.len());
    assert!(
        differences.is_empty(),
        "{} of {} steps differ:\n{}",
        differences.len(),
        ours.len(),
        differences.join("\n")
    );
}

/// Reads `path` from a daemon without a session (loopback requests are trusted).
async fn read(base: &str, path: &str) -> Value {
    let response = reqwest::get(format!("{base}{path}")).await.unwrap();
    assert_eq!(response.status(), 200, "{path}");
    response.json().await.unwrap()
}

async fn write(base: &str, path: &str, body: &Value) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

/// The JSON paths at which `left` and `right` differ, for a readable failure.
fn differences(left: &Value, right: &Value, path: &str, found: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys().filter(|key| !a.contains_key(*key))) {
                let (x, y) = (
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                );
                differences(x, y, &format!("{path}/{key}"), found);
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                differences(x, y, &format!("{path}/{index}"), found);
            }
        }
        _ if left != right => found.push(format!("{path}: {left} != {right}")),
        _ => {}
    }
}

fn assert_same(left: &Value, right: &Value, what: &str) {
    let mut found = Vec::new();
    differences(left, right, "", &mut found);
    assert!(
        found.is_empty(),
        "{what}:
{}",
        found.join(
            "
"
        )
    );
}

/// Removes from `views` the two events a handover itself appends, after checking they are
/// exactly those: the previous daemon's `daemon.shutdown` and this daemon's startup
/// `recovery.report`.
fn strip_handover(views: &mut Value) {
    let page = &mut views["/events"];
    let events = page["events"].as_array_mut().unwrap();
    let handover = events.split_off(events.len().saturating_sub(2));
    let kinds: Vec<&str> = handover
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, ["daemon.shutdown", "recovery.report"]);
    page["nextCursor"] = events.last().unwrap()["sequence"].clone();
}

/// Every view of a Project the other daemon must read back unchanged.
async fn project_views(base: &str, project_id: &str) -> Value {
    let mut views = serde_json::Map::new();
    for path in [
        "/missions",
        "/directions",
        "/autoresearch",
        "/graph-proposals",
        "/threads",
        "/notification-acks",
        "/research-map",
        "/paper",
    ] {
        let mut view = read(base, &format!("{path}?projectId={project_id}")).await;
        // The map is stamped when it is read.
        if let Some(map) = view.as_object_mut() {
            map.remove("generatedAt");
        }
        views.insert(path.to_owned(), view);
    }
    views.insert(
        "/events".to_owned(),
        read(
            base,
            &format!("/events?projectId={project_id}&after=0&limit=1000"),
        )
        .await,
    );
    views.insert("/projects".to_owned(), read(base, "/projects").await);
    Value::Object(views)
}

/// Writes to a Project through `base`: a Mission taken to planning, a proposed Direction, an
/// acknowledged event, and a paper revision.
async fn continue_project(base: &str, project_id: &str, round: &str) {
    let mission = write(base, "/missions", &json!({ "projectId": project_id, "title": format!("Mission {round}"), "objective": "Survive the handover", "deliverables": ["Rows"], "successCriteria": ["Read back"], "idempotencyKey": format!("handover-mission-{round}") })).await;
    let mission_id = mission["mission"]["entityId"].as_str().unwrap().to_owned();
    write(base, &format!("/missions/{mission_id}/transition"), &json!({ "projectId": project_id, "expectedVersion": 1, "next": "planning", "idempotencyKey": format!("handover-planning-{round}") })).await;
    let direction = write(base, "/directions", &json!({ "projectId": project_id, "question": format!("Question {round}"), "decisionUse": "Handover", "idempotencyKey": format!("handover-direction-{round}") })).await;
    let direction_id = direction["direction"]["entityId"]
        .as_str()
        .unwrap()
        .to_owned();
    write(base, &format!("/directions/{direction_id}/transition"), &json!({ "projectId": project_id, "expectedVersion": 1, "next": "proposed", "idempotencyKey": format!("handover-proposed-{round}") })).await;
    let page = read(
        base,
        &format!("/events?projectId={project_id}&recent=true&limit=1"),
    )
    .await;
    write(base, "/notifications/acknowledge", &json!({ "projectId": project_id, "idempotencyKey": format!("handover-acknowledge-{round}"), "eventIds": [page["events"][0]["eventId"]] })).await;
    let paper = read(base, &format!("/paper?projectId={project_id}")).await;
    let response = reqwest::Client::new()
        .put(format!("{base}/paper"))
        .json(&json!({ "projectId": project_id, "markdown": format!("# Revised in {round}\n"), "bibliography": paper["bibliography"], "expected": { "markdownHash": paper["markdownHash"], "bibliographyHash": paper["bibliographyHash"], "version": paper["version"] }, "idempotencyKey": format!("handover-paper-{round}") }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202, "{}", response.text().await.unwrap());
}

async fn stop(mut daemon: Daemon) {
    let response = reqwest::Client::new()
        .post(format!("{}/shutdown", daemon.base))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let started = Instant::now();
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the daemon did not stop"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A Project one implementation created is continued by the other and handed back: each daemon
/// reads every view of the other's data exactly as the writer last served it (plus the two
/// events the handover appends), appends to it, and the writer then reads those appends back the
/// same way.
#[test]
fn each_daemon_continues_a_project_the_other_created() {
    if !ts_built(&ts_main()) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (first, second) in [
        (Implementation::TypeScript, Implementation::Rust),
        (Implementation::Rust, Implementation::TypeScript),
    ] {
        runtime.block_on(async {
            let scratch = tempfile::tempdir().unwrap();
            let repository = scratch.path().join("repository");
            std::fs::create_dir_all(repository.join("docs")).unwrap();
            std::fs::write(repository.join("docs/paper.md"), "# Handover\n").unwrap();
            std::fs::write(repository.join("docs/paper.bib"), "").unwrap();
            let data = scratch.path().join("data");
            let project_id = format!("prj_{}", "e".repeat(32));
            let database = data.join("projects").join(&project_id).join("nosh.sqlite");

            let writer = Daemon::spawn(first, &data);
            write(&writer.base, "/projects", &json!({ "projectId": project_id, "repositoryRoot": repository, "databasePath": database })).await;
            continue_project(&writer.base, &project_id, "one").await;
            let written = project_views(&writer.base, &project_id).await;
            stop(writer).await;

            let reader = Daemon::spawn(second, &data);
            let mut read_back = project_views(&reader.base, &project_id).await;
            strip_handover(&mut read_back);
            assert_same(&read_back, &written, &format!("{second:?} reads {first:?} data differently"));
            continue_project(&reader.base, &project_id, "two").await;
            let appended = project_views(&reader.base, &project_id).await;
            stop(reader).await;

            let writer = Daemon::spawn(first, &data);
            let mut returned = project_views(&writer.base, &project_id).await;
            strip_handover(&mut returned);
            assert_same(&returned, &appended, &format!("{first:?} reads {second:?} appends differently"));
            let events = returned["/events"]["events"].as_array().unwrap();
            assert!(
                events
                    .windows(2)
                    .all(|pair| pair[0]["sequence"].as_i64() < pair[1]["sequence"].as_i64()),
                "sequences continue across the handover"
            );
            stop(writer).await;
        });
    }
}

/// The TypeScript TUI (`apps/tui/dist`: its Controller and DaemonClient) drives `nosh daemon`:
/// sign-in, the event page and live stream, the read-only slash commands, and a Job in
/// `/activity` and `/tail`.
#[test]
fn the_typescript_tui_drives_the_rust_daemon() {
    if !ts_built(&ts_root().join("apps/tui/dist/controller.js")) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("data");
    // A Project as `nosh project open` makes one: an initialized repository, then registered.
    let project = nosh_research::project::initialize_research_project(
        &nosh_research::project::ProjectInitialization {
            path: &scratch.path().join("repository"),
            data_directory: &data,
            create_repository: true,
            working_title: Some("TUI handover"),
            github_repository_url: None,
        },
    )
    .unwrap();
    let daemon = Daemon::spawn(Implementation::Rust, &data);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(write(&daemon.base, "/projects", &json!({ "projectId": project.project_id, "repositoryRoot": project.repository_root, "databasePath": project.database_path })));
    let output = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tui_controller.mjs"))
        .arg(ts_root())
        .arg(&daemon.base)
        .arg(BOOTSTRAP)
        .arg(&project.project_id)
        .arg(&project.repository_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    runtime.block_on(stop(daemon));
}

/// The Rust TUI (`nosh-tui`: its DaemonClient, Controller and Workspace) drives `nosh daemon`
/// through the script the TypeScript TUI runs above, then draws the session: sign-in, the event
/// page and live stream, the read-only slash commands, a Job in `/activity` and `/tail`.
#[test]
fn the_rust_tui_drives_the_rust_daemon() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("data");
    let project = nosh_research::project::initialize_research_project(
        &nosh_research::project::ProjectInitialization {
            path: &scratch.path().join("repository"),
            data_directory: &data,
            create_repository: true,
            working_title: Some("TUI handover"),
            github_repository_url: None,
        },
    )
    .unwrap();
    let daemon = Daemon::spawn(Implementation::Rust, &data);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(write(&daemon.base, "/projects", &json!({ "projectId": project.project_id, "repositoryRoot": project.repository_root, "databasePath": project.database_path })));
    tokio::task::LocalSet::new().block_on(
        &runtime,
        rust_tui(&daemon.base, &project.project_id, &project.repository_root),
    );
    runtime.block_on(stop(daemon));
}

async fn rust_tui(base: &str, project_id: &str, repository: &Path) {
    use nosh_tui::controller::Controller;
    use nosh_tui::js::js_json_parse;
    use nosh_tui::transport::{DaemonApi, DaemonClient, install_crypto_provider};
    use std::rc::Rc;

    install_crypto_provider();
    let client = Rc::new(
        DaemonClient::new(&nosh_tui::TuiConfig {
            base_url: base.into(),
            bootstrap_token: Some(BOOTSTRAP.into()),
            ..nosh_tui::TuiConfig::default()
        })
        .unwrap(),
    );
    let request = |path: &'static str, body: Value| {
        let body = js_json_parse(&body.to_string()).unwrap();
        let client = client.clone();
        async move { serde_json::Value::from(client.request(path, Some(body), None).await.unwrap()) }
    };
    let controller = Controller::new(client.clone(), None);
    controller.initialize().await.unwrap();
    {
        let state = controller.state();
        assert_eq!(
            state.connection, "connected",
            "initialize left the connection"
        );
        assert_eq!(
            state.project_id, project_id,
            "initialize chose another project"
        );
    }
    controller.poll().await;
    let polled = {
        let state = controller.state();
        assert_eq!(state.connection, "connected", "poll failed");
        assert!(
            !state.events.is_empty() && state.cursor > 0,
            "poll loaded no events"
        );
        state.events.len()
    };
    // A Mission created elsewhere reaches the TUI over the WebSocket, without another poll.
    request("/missions", json!({ "projectId": project_id, "title": "TUI Mission", "objective": "Reach the TUI", "deliverables": ["A row"], "successCriteria": ["It shows"], "idempotencyKey": "tui-mission-rust" })).await;
    let started = Instant::now();
    while controller.state().events.len() <= polled {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the live stream delivered nothing"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let job_id = format!("job_{}", "7".repeat(32));
    let started = request("/jobs", json!({
        "jobId": job_id, "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null,
        "runId": format!("run_{}", "8".repeat(32)), "experimentId": null, "commitSha": "0".repeat(40), "workingDirectory": repository,
        "runner": "native", "distribution": null, "command": ["node", "-e", "console.log('tui job'); setTimeout(() => {}, 4000)"],
        "checkpointCommand": null, "environmentLockHash": format!("sha256:{}", "0".repeat(64)), "evaluationContractHash": format!("sha256:{}", "0".repeat(64)),
        "timeoutSeconds": 60, "usesGpu": false,
    })).await;
    assert_eq!(
        started["job"]["jobId"],
        json!(job_id),
        "the Job did not start"
    );
    let job = format!("/job {job_id}");
    for (command, view) in [
        ("/projects", "projects"),
        ("/status", "status"),
        ("/jobs", "jobs"),
        (job.as_str(), "detail"),
        ("/map", "map"),
        ("/approvals", "approvals"),
        ("/help", "help"),
    ] {
        controller.execute(command).await.unwrap();
        let state = controller.state();
        assert!(
            state.view == view && state.error.is_empty(),
            "{command} showed {} ({})",
            state.view,
            state.error
        );
    }
    assert!(
        controller
            .sidebar_rows()
            .iter()
            .any(|row| format!("{row:?}").contains("TUI Mission")),
        "the sidebar does not list the Mission"
    );
    let mut tail = String::new();
    for _ in 0..60 {
        controller
            .execute(&format!("/tail {job_id}"))
            .await
            .unwrap();
        tail = format!("{:?}{}", controller.state().rows, controller.state().detail);
        if tail.contains("tui job") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        tail.contains("tui job"),
        "the Job's output never reached /tail: {tail}"
    );
    // The workspace draws the session: the status cards, the sidebar and the footer.
    controller.execute("/status").await.unwrap();
    let (sender, _completions) = tokio::sync::mpsc::unbounded_channel();
    let mut workspace = nosh_tui::workspace::Workspace::new(controller.clone(), sender);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| workspace.draw(frame)).unwrap();
    let frame = nosh_tui::render::frame_text(terminal.backend().buffer());
    for needle in ["NOSH / STATUS", "TUI Mission", "● connected", "ctrl+b hide"] {
        assert!(
            frame.contains(needle),
            "the drawn session lacks {needle}:\n{frame}"
        );
    }
    workspace.dispose();
    controller.dispose();
    client.close();
}
