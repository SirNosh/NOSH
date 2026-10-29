//! Port of `apps/noshd/src/server.test.ts`, plus Rust-only cases for the event pump and the
//! WebSocket upgrade.
//!
//! Not ported here: "loads the NOSH Pi package through Pi's resource loader", which
//! `nosh-agent`'s `tests/pi.rs` covers (every role's session is supplied `nosh-control`).

use futures_util::future::LocalBoxFuture;
use futures_util::{FutureExt, StreamExt};
use nosh_agent::pi::Inspection;
use nosh_agent::policy::SessionOptions;
use nosh_orchestration::Agents;
use nosh_orchestration::daemon::{
    DaemonOptions, Listener, NoshDaemon, bridge, mission_job_matches,
};
use nosh_server::events::{EventSocket, EventSource, subscribe};
use nosh_server::{LocalApiServer, is_loopback_address};
use nosh_store::{EventStore, ProjectionInput};
use nosh_wire::{EVENT_SCHEMA, IdPrefix, create_id, schema_uri};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, future)
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---- The event pump over a fake socket and event source ----

/// A daemon stand-in: a fixed history and the event fan-out.
#[derive(Default)]
struct FakeSource {
    history: Vec<Value>,
    listeners: RefCell<Vec<(u64, Listener)>>,
    next: Cell<u64>,
    fail_replay: Cell<bool>,
}

impl FakeSource {
    fn with_history(count: i64) -> Rc<Self> {
        Rc::new(Self {
            history: (1..=count)
                .map(|sequence| json!({ "scope": { "projectId": "prj_test" }, "sequence": sequence }))
                .collect(),
            ..Self::default()
        })
    }

    fn emit(&self, event: &Value) {
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

    fn listener_count(&self) -> usize {
        self.listeners.borrow().len()
    }
}

impl EventSource for FakeSource {
    fn replay_one(&self, _project_id: &str, after: i64) -> Result<Option<Value>, String> {
        if self.fail_replay.get() {
            return Err("database is locked".into());
        }
        Ok(self
            .history
            .iter()
            .find(|event| event["sequence"].as_i64().unwrap() > after)
            .cloned())
    }
    fn subscribe(&self, listener: Listener) -> u64 {
        let id = self.next.get() + 1;
        self.next.set(id);
        self.listeners.borrow_mut().push((id, listener));
        id
    }
    fn unsubscribe(&self, id: u64) {
        self.listeners
            .borrow_mut()
            .retain(|(known, _)| *known != id);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Sends {
    Deliver,
    /// The send callback never runs: a blocked consumer.
    Hold,
    Fail,
}

struct FakeSocket {
    sends: Sends,
    /// The client closes after receiving this sequence.
    close_after: Option<i64>,
    open: Cell<bool>,
    ended: Cell<bool>,
    end: Notify,
    received: RefCell<Vec<Value>>,
    code: Cell<u16>,
    reason: RefCell<String>,
    terminated: Cell<bool>,
}

impl FakeSocket {
    fn new(sends: Sends, close_after: Option<i64>) -> Rc<Self> {
        Rc::new(Self {
            sends,
            close_after,
            open: Cell::new(true),
            ended: Cell::new(false),
            end: Notify::new(),
            received: RefCell::new(Vec::new()),
            code: Cell::new(0),
            reason: RefCell::new(String::new()),
            terminated: Cell::new(false),
        })
    }

    fn finish(&self) {
        self.open.set(false);
        self.ended.set(true);
        self.end.notify_waiters();
    }

    fn sequences(&self) -> Vec<Value> {
        self.received
            .borrow()
            .iter()
            .map(|event| event["sequence"].clone())
            .collect()
    }
}

impl EventSocket for FakeSocket {
    fn send(&self, message: String) -> LocalBoxFuture<'_, Result<(), String>> {
        match self.sends {
            Sends::Hold => return std::future::pending().boxed_local(),
            Sends::Fail => return async { Err("write EPIPE".to_owned()) }.boxed_local(),
            Sends::Deliver => {}
        }
        let event: Value = serde_json::from_str(&message).unwrap();
        let last = self.close_after.is_some() && event["sequence"].as_i64() == self.close_after;
        self.received.borrow_mut().push(event);
        if last {
            self.finish();
        }
        async { Ok(()) }.boxed_local()
    }
    fn close(&self, code: u16, reason: &str) {
        self.code.set(code);
        *self.reason.borrow_mut() = reason.to_owned();
        self.open.set(false);
    }
    fn terminate(&self) {
        self.terminated.set(true);
        self.finish();
    }
    fn buffered_amount(&self) -> usize {
        0
    }
    fn is_open(&self) -> bool {
        self.open.get()
    }
    fn closed(&self) -> LocalBoxFuture<'_, Result<(), String>> {
        async move {
            while !self.ended.get() {
                self.end.notified().await;
            }
            Ok(())
        }
        .boxed_local()
    }
}

async fn settle(until: impl Fn() -> bool) {
    for _ in 0..200 {
        if until() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn streams_ordered_durable_pages_and_resumes_strictly_after_the_received_cursor() {
    local(async {
        let source = FakeSource::with_history(5);
        for (after, expected) in [(0, json!([1, 2, 3, 4, 5])), (3, json!([4, 5]))] {
            let socket = FakeSocket::new(Sends::Deliver, Some(5));
            subscribe(Rc::clone(&socket), Rc::clone(&source), "prj_test", after);
            tokio::time::timeout(Duration::from_secs(5), socket.closed())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Value::Array(socket.sequences()), expected);
        }
        settle(|| source.listener_count() == 0).await;
        assert_eq!(source.listener_count(), 0);
    });
}

#[test]
fn closes_overflowing_transient_queues_explicitly_and_removes_the_listener() {
    local(async {
        let source = FakeSource::with_history(0);
        let socket = FakeSocket::new(Sends::Hold, None);
        subscribe(Rc::clone(&socket), Rc::clone(&source), "prj_test", 0);
        for _ in 0..305 {
            source.emit(
                &json!({ "scope": { "projectId": "prj_test" }, "sequence": null, "payload": "x" }),
            );
        }
        assert_eq!(socket.code.get(), 1013);
        assert!(socket.reason.borrow().contains("last received cursor"));
        assert_eq!(source.listener_count(), 0);
        // The closing handshake never completes, so the connection is dropped a second later.
        tokio::time::timeout(Duration::from_secs(3), socket.closed())
            .await
            .unwrap()
            .unwrap();
        assert!(socket.terminated.get());
    });
}

#[test]
fn sends_transient_events_after_durable_ones_and_ignores_other_projects() {
    local(async {
        let source = FakeSource::with_history(2);
        let socket = FakeSocket::new(Sends::Deliver, None);
        subscribe(Rc::clone(&socket), Rc::clone(&source), "prj_test", 0);
        source.emit(
            &json!({ "scope": { "projectId": "prj_other" }, "sequence": null, "payload": "other" }),
        );
        source.emit(
            &json!({ "scope": { "projectId": "prj_test" }, "sequence": null, "payload": "live" }),
        );
        settle(|| socket.received.borrow().len() == 3).await;
        assert_eq!(
            Value::Array(socket.sequences()),
            json!([1, 2, null]),
            "durable history first, then the queued transient event"
        );
        assert_eq!(socket.received.borrow()[2]["payload"], "live");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            socket.received.borrow().len(),
            3,
            "nothing for other Projects"
        );
        assert_eq!(source.listener_count(), 1);
    });
}

#[test]
fn refuses_events_beyond_one_megabyte_and_stops_on_send_and_replay_failures() {
    local(async {
        let source = FakeSource::with_history(0);
        let socket = FakeSocket::new(Sends::Hold, None);
        subscribe(Rc::clone(&socket), Rc::clone(&source), "prj_test", 0);
        let big = "x".repeat(400 * 1024);
        for _ in 0..3 {
            source.emit(
                &json!({ "scope": { "projectId": "prj_test" }, "sequence": null, "payload": big }),
            );
        }
        assert_eq!(socket.code.get(), 1013, "three 400 KB events exceed 1 MB");
        assert_eq!(source.listener_count(), 0);

        let failing = FakeSocket::new(Sends::Fail, None);
        let history = FakeSource::with_history(1);
        subscribe(Rc::clone(&failing), Rc::clone(&history), "prj_test", 0);
        settle(|| failing.code.get() != 0).await;
        assert_eq!(
            (failing.code.get(), failing.reason.borrow().as_str()),
            (1011, "Send failed; reconnect from last received cursor")
        );

        let broken = FakeSource::with_history(1);
        broken.fail_replay.set(true);
        let socket = FakeSocket::new(Sends::Deliver, None);
        subscribe(Rc::clone(&socket), Rc::clone(&broken), "prj_test", 0);
        settle(|| socket.code.get() != 0).await;
        assert_eq!(
            (socket.code.get(), socket.reason.borrow().as_str()),
            (1011, "Replay failed; reconnect from last received cursor")
        );
        assert_eq!(broken.listener_count(), 0);

        // A stored event that alone exceeds 1 MB is refused rather than sent.
        let oversized = Rc::new(FakeSource {
            history: vec![
                json!({ "scope": { "projectId": "prj_test" }, "sequence": 1, "payload": "x".repeat(1024 * 1024) }),
            ],
            ..FakeSource::default()
        });
        let socket = FakeSocket::new(Sends::Deliver, None);
        subscribe(Rc::clone(&socket), Rc::clone(&oversized), "prj_test", 0);
        settle(|| socket.code.get() != 0).await;
        assert_eq!(
            (socket.code.get(), socket.reason.borrow().as_str()),
            (1013, nosh_server::events::SLOW_CONSUMER)
        );
        assert!(socket.received.borrow().is_empty());
    });
}

#[test]
fn disconnects_a_consumer_whose_send_does_not_complete_within_five_seconds() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, async {
        let source = FakeSource::with_history(1);
        let socket = FakeSocket::new(Sends::Hold, None);
        subscribe(Rc::clone(&socket), Rc::clone(&source), "prj_test", 0);
        tokio::time::sleep(Duration::from_millis(4900)).await;
        assert_eq!(socket.code.get(), 0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            (socket.code.get(), socket.reason.borrow().as_str()),
            (1013, nosh_server::events::SLOW_CONSUMER)
        );
        assert_eq!(source.listener_count(), 0);
    });
}

// ---- The HTTP API over a real daemon ----

/// Sessions that cannot start: these tests drive no model.
struct NoSessions;

impl Agents for NoSessions {
    fn start(&self, _options: SessionOptions) -> LocalBoxFuture<'_, Result<Inspection, String>> {
        async { Err("No Pi model is configured in this test".to_owned()) }.boxed_local()
    }
    fn prompt<'a>(
        &'a self,
        _agent_id: &'a str,
        _prompt: &'a str,
        _step: Option<&'a nosh_agent::pi::TerminalStep>,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        async { Err("unknown agent".to_owned()) }.boxed_local()
    }
    fn steer<'a>(
        &'a self,
        _agent_id: &'a str,
        _message: &'a str,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        async { Err("unknown agent".to_owned()) }.boxed_local()
    }
    fn abort<'a>(&'a self, _agent_id: &'a str) -> LocalBoxFuture<'a, Result<(), String>> {
        async { Err("unknown agent".to_owned()) }.boxed_local()
    }
    fn stop(&self, _agent_id: &str) {}
    fn inspect(&self) -> Vec<Inspection> {
        Vec::new()
    }
}

fn daemon(data_directory: &Path, token: &str) -> Rc<NoshDaemon> {
    NoshDaemon::new(
        DaemonOptions {
            data_directory: data_directory.to_path_buf(),
            bootstrap_token: token.into(),
            package_path: repository_root().join("pi-package"),
            maximum_active_pi_sessions: None,
            default_model: None,
            // No Job runs in these tests.
            job_wrapper: vec!["nosh-job-wrapper".into()],
            schema_dir: repository_root().join("conformance/schemas/json"),
        },
        Rc::new(NoSessions),
        bridge(),
    )
    .unwrap()
}

/// An HTTP/1.1 request written by hand, for headers `reqwest` derives itself (Host).
async fn raw_status(base: &str, request: &str) -> u16 {
    let address = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("the server ends the response")
        .unwrap();
    let response = String::from_utf8_lossy(&response);
    response
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0)
}

struct Api {
    client: reqwest::Client,
    base: String,
}

impl Api {
    async fn status(&self, method: reqwest::Method, path: &str, headers: &[(&str, &str)]) -> u16 {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap().status().as_u16()
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> (u16, Value) {
        let response = self
            .client
            .request(method, format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }
}

#[test]
fn auto_trusts_local_origins_while_preserving_bearer_fallback_and_event_replay() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir_all(repository.join("docs")).unwrap();
        std::fs::write(repository.join("docs/paper.md"), "# Test\n").unwrap();
        std::fs::write(repository.join("docs/paper.bib"), "").unwrap();
        let token = "test-bootstrap-token";
        let project_id = create_id(IdPrefix::Prj);
        let database_path = directory
            .path()
            .join("data/projects")
            .join(&project_id)
            .join("nosh.sqlite");
        let daemon = daemon(&directory.path().join("data"), token);
        let server = LocalApiServer::new(Rc::clone(&daemon));
        daemon.start().unwrap();
        // This test drives Mission and Direction versions through the API; a background
        // supervisor tick would race it.
        daemon.missions.stop();
        daemon.directions.stop();
        daemon.experiments.stop();
        server.listen("127.0.0.1", 0).await.unwrap();
        let base = server.address().unwrap();
        let api = Api {
            client: reqwest::Client::new(),
            base: base.clone(),
        };
        let get = reqwest::Method::GET;

        assert_eq!(api.status(get.clone(), "/projects", &[]).await, 200);
        assert_eq!(
            api.status(get.clone(), "/projects", &[("origin", &base)])
                .await,
            200
        );
        assert_eq!(
            api.status(
                get.clone(),
                "/projects",
                &[("origin", "http://attacker.invalid")]
            )
            .await,
            401
        );
        assert_eq!(
            api.status(
                get.clone(),
                "/projects",
                &[("sec-fetch-site", "cross-site")]
            )
            .await,
            401
        );
        // DNS rebinding: attacker Host and matching Origin arrive from loopback.
        let port = base.rsplit(':').next().unwrap();
        let rebound = |headers: String| {
            let base = base.clone();
            async move {
                raw_status(
                    &base,
                    &format!("GET /projects HTTP/1.1\r\n{headers}Connection: close\r\n\r\n"),
                )
                .await
            }
        };
        assert_eq!(
            rebound(format!(
                "Host: attacker.invalid:{port}\r\nOrigin: http://attacker.invalid:{port}\r\n"
            ))
            .await,
            401
        );
        assert_eq!(
            rebound(format!("Host: attacker.invalid:{port}\r\n")).await,
            401
        );
        assert_eq!(rebound(format!("Host: localhost:{port}\r\n")).await, 200);
        assert_eq!(
            api.status(reqwest::Method::POST, "/session", &[]).await,
            401
        );
        let session = api
            .client
            .post(format!("{base}/session"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(session.status().as_u16(), 201);
        let session: Value = session.json().await.unwrap();
        let session_token = session["token"].as_str().unwrap().to_owned();
        assert_eq!(session_token.len(), 43, "32 random bytes in base64url");
        let expires = time::OffsetDateTime::parse(
            session["expiresAt"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert!(expires > time::OffsetDateTime::now_utc());
        let bearer = format!("Bearer {session_token}");
        assert_eq!(
            api.status(
                get.clone(),
                "/projects",
                &[
                    ("origin", "http://attacker.invalid"),
                    ("authorization", &bearer)
                ]
            )
            .await,
            200
        );

        let created_root = directory.path().join("created-project");
        let (status, created) = api
            .post(
                "/projects/open",
                &json!({ "path": created_root, "createRepository": true, "workingTitle": "Created Project" }),
            )
            .await;
        assert_eq!(status, 201, "{created}");
        assert!(
            created_root
                .join(".nosh/contracts/project.v1.json")
                .exists()
        );
        assert!(created_root.join("docs/paper.md").exists());
        let created_id = created["project"]["projectId"].as_str().unwrap();
        let (_, draft) = api.get(&format!("/projects/{created_id}/contract")).await;
        assert_eq!(draft["contract"]["approvedAt"], Value::Null);
        assert_eq!(
            draft["contract"]["northStar"]["contributionType"],
            "contribution_pending"
        );
        assert_eq!(draft["contract"]["computeEnvelope"]["maximumDiskBytes"], 0);

        let (status, registered) = api
            .post(
                "/projects",
                &json!({ "projectId": project_id, "repositoryRoot": repository, "databasePath": database_path }),
            )
            .await;
        assert_eq!(status, 201, "{registered}");
        assert_eq!(
            api.post("/terminals", &json!({ "projectId": project_id }))
                .await
                .0,
            404
        );
        assert_eq!(api.get("/remote").await.0, 404);
        assert_eq!(
            api.status(reqwest::Method::POST, "/directories/select", &[])
                .await,
            404
        );
        assert_eq!(api.get("/").await.0, 404);
        let (_, threads) = api.get(&format!("/threads?projectId={project_id}")).await;
        assert_eq!(threads["threads"], json!([]));
        let skill_id = create_id(IdPrefix::Skl);
        let (status, skill) = api
            .post(
                "/skills",
                &json!({ "projectId": project_id, "idempotencyKey": "register-runtime-skill-0001", "manifest": {
                    "$schema": schema_uri("skill-manifest"), "schemaVersion": 1, "skillId": skill_id, "name": "runtime_test", "version": "1.0.0",
                    "description": "API integration fixture", "activation": { "roles": ["general_worker"], "requiredCapabilities": [], "episodeTypes": [] },
                    "promptFragment": "Return a bounded result.", "permittedTools": ["nosh_episode_submit"], "inputEpisodeTypes": [], "outputEpisodeType": "episode_test",
                    "executionMode": "prompt", "programId": null, "preflightChecks": [], "postflightChecks": ["check_episode"],
                } }),
            )
            .await;
        assert_eq!(status, 201, "{skill}");
        let (_, skills) = api.get(&format!("/skills?projectId={project_id}")).await;
        assert!(
            skills["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["entityId"] == skill_id.as_str())
        );

        assert_eq!(api.post("/commands", &json!({})).await.0, 404);
        let (_, replayed) = api
            .get(&format!("/events?projectId={project_id}&after=0"))
            .await;
        assert!(
            !replayed["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["type"] == "agent.started")
        );
        let (_, page) = api
            .get(&format!("/events?projectId={project_id}&after=0&limit=1"))
            .await;
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(page["nextCursor"], page["events"][0]["sequence"]);
        assert_eq!(page["hasMore"], true);
        assert_eq!(
            api.get(&format!("/events?projectId={project_id}&limit=1001"))
                .await
                .0,
            400
        );
        let cursor = page["nextCursor"].as_i64().unwrap();
        let (_, next) = api
            .get(&format!(
                "/events?projectId={project_id}&after={cursor}&limit=1"
            ))
            .await;
        assert!(next["events"][0]["sequence"].as_i64().unwrap() > cursor);
        let (_, recent) = api
            .get(&format!(
                "/events?projectId={project_id}&recent=true&limit=2"
            ))
            .await;
        let history = daemon.replay(&project_id, 0).unwrap();
        let last_two: Vec<&Value> = history[history.len() - 2..]
            .iter()
            .map(|event| &event["sequence"])
            .collect();
        assert_eq!(
            recent["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| &event["sequence"])
                .collect::<Vec<_>>(),
            last_two
        );
        assert_eq!(recent["nextCursor"], history.last().unwrap()["sequence"]);
        assert_eq!(recent["hasMore"], false);
        assert_eq!(
            api.get(&format!(
                "/events?projectId={project_id}&recent=true&after=1"
            ))
            .await
            .0,
            400
        );
        assert_eq!(
            api.get(&format!("/events?projectId={project_id}&recent=invalid"))
                .await
                .0,
            400
        );
        let delta = EventStore::open(&database_path)
            .unwrap()
            .append(&json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "agent.started", "source": "test", "scope": { "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null }, "correlationId": null, "causationId": null, "payload": {} }))
            .unwrap();
        let recent_cursor = recent["nextCursor"].as_i64().unwrap();
        let (_, delta_page) = api
            .get(&format!(
                "/events?projectId={project_id}&after={recent_cursor}&limit=2"
            ))
            .await;
        assert_eq!(
            delta_page["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| &event["sequence"])
                .collect::<Vec<_>>(),
            [&delta["sequence"]]
        );
        assert_eq!(delta_page["nextCursor"], delta["sequence"]);
        let socket_url = format!(
            "{}/events?projectId={project_id}&after=0",
            base.replacen("http", "ws", 1)
        );
        let mut cross_origin = socket_url.as_str().into_client_request().unwrap();
        cross_origin
            .headers_mut()
            .insert("origin", "http://attacker.invalid".parse().unwrap());
        let stream = tokio::net::TcpStream::connect(base.trim_start_matches("http://"))
            .await
            .unwrap();
        assert!(
            tokio_tungstenite::client_async(cross_origin, stream)
                .await
                .is_err(),
            "a cross-origin socket without a session is refused"
        );

        let (status, mission) = api
            .post(
                "/missions",
                &json!({ "projectId": project_id, "title": "Test Mission", "objective": "Produce a reviewed result", "deliverables": ["Reviewed result"], "successCriteria": ["All required nodes pass"], "idempotencyKey": "create-mission-0001" }),
            )
            .await;
        assert_eq!(status, 201, "{mission}");
        let mission = &mission["mission"];
        assert_eq!(mission["value"]["graphVersion"], 1);
        let mission_id = mission["entityId"].as_str().unwrap();
        let (status, _) = api
            .post(
                &format!("/missions/{mission_id}/nodes/internal-lease/transition"),
                &json!({ "projectId": project_id, "expectedVersion": mission["version"], "next": "leased", "lease": {}, "idempotencyKey": "public-lease-rejected-0001" }),
            )
            .await;
        assert_eq!(status, 400);
        // Rust-only: an internal state is refused without a lease too.
        let (status, refused) = api
            .post(
                &format!("/missions/{mission_id}/nodes/internal-lease/transition"),
                &json!({ "projectId": project_id, "expectedVersion": mission["version"], "next": "leased", "idempotencyKey": "public-lease-rejected-0002" }),
            )
            .await;
        assert_eq!(
            (status, refused["error"].as_str().unwrap()),
            (
                400,
                "Public graph transitions allow only ready, blocked, or cancelled without a lease"
            )
        );
        for (next, expected) in [("planning", 1), ("awaiting_approval", 2), ("running", 3)] {
            let (status, changed) = api
                .post(
                    &format!("/missions/{mission_id}/transition"),
                    &json!({ "projectId": project_id, "expectedVersion": expected, "next": next, "idempotencyKey": format!("mission-transition-{next}-0001") }),
                )
                .await;
            assert_eq!(status, 202, "{changed}");
        }
        let (_, listed) = api.get(&format!("/missions?projectId={project_id}")).await;
        assert_eq!(listed["missions"][0]["state"], "running");
        let (status, bare_stop) = api
            .post(
                &format!("/missions/{mission_id}/transition"),
                &json!({ "projectId": project_id, "expectedVersion": 4, "next": "stopping", "idempotencyKey": "mission-transition-stopping-0001" }),
            )
            .await;
        assert_eq!(status, 400);
        assert!(
            bare_stop["error"]
                .as_str()
                .unwrap()
                .contains("/control <missionId> <version> stop")
        );
        let (status, paused) = api
            .post(
                &format!("/missions/{mission_id}/control"),
                &json!({ "projectId": project_id, "expectedVersion": 4, "action": "pause", "mode": "safe", "idempotencyKey": "mission-control-pause-0001" }),
            )
            .await;
        assert_eq!(status, 202, "{paused}");
        assert_eq!(
            (&paused["mission"]["state"], &paused["mission"]["version"]),
            (&json!("paused"), &json!(6))
        );
        let (status, resumed) = api
            .post(
                &format!("/missions/{mission_id}/control"),
                &json!({ "projectId": project_id, "expectedVersion": 6, "action": "resume", "mode": "safe", "idempotencyKey": "mission-control-resume-0001" }),
            )
            .await;
        assert_eq!(status, 202, "{resumed}");
        assert_eq!(
            (&resumed["mission"]["state"], &resumed["mission"]["version"]),
            (&json!("running"), &json!(7))
        );
        let (status, direction) = api
            .post(
                "/directions",
                &json!({ "projectId": project_id, "question": "Does the mechanism work?", "decisionUse": "Decide the paper claim", "missionId": mission_id, "idempotencyKey": "create-direction-0001" }),
            )
            .await;
        assert_eq!(status, 201, "{direction}");
        let direction_id = direction["direction"]["entityId"].as_str().unwrap();
        for (next, expected) in [("proposed", 1), ("active", 2)] {
            let (status, changed) = api
                .post(
                    &format!("/directions/{direction_id}/transition"),
                    &json!({ "projectId": project_id, "expectedVersion": expected, "next": next, "idempotencyKey": format!("direction-transition-{next}-0001") }),
                )
                .await;
            assert_eq!(status, 202, "{changed}");
        }
        let (status, premature) = api
            .post(
                "/autoresearch",
                &json!({ "projectId": project_id, "directionId": direction_id, "decisionQuestion": "Does an unreviewed baseline suffice?", "idempotencyKey": "premature-ar-0001" }),
            )
            .await;
        assert_eq!(status, 400);
        assert!(
            premature["error"]
                .as_str()
                .unwrap()
                .contains("reviewed immutable baseline"),
            "{premature}"
        );
        let (_, original) = api.get(&format!("/paper?projectId={project_id}")).await;
        let expected = json!({ "markdownHash": original["markdownHash"], "bibliographyHash": original["bibliographyHash"], "version": original["version"] });
        let (status, saved) = api
            .send(
                reqwest::Method::PUT,
                "/paper",
                &json!({ "projectId": project_id, "markdown": "# Revised\n", "bibliography": original["bibliography"], "expected": expected, "idempotencyKey": "paper-save-command-0001" }),
            )
            .await;
        assert_eq!(status, 202, "{saved}");
        let (_, read) = api.get(&format!("/paper?projectId={project_id}")).await;
        assert_eq!(read["markdown"], "# Revised\n");
        let (status, _) = api
            .send(
                reqwest::Method::PUT,
                "/paper",
                &json!({ "projectId": project_id, "markdown": "# Stale overwrite\n", "bibliography": original["bibliography"], "expected": expected, "idempotencyKey": "paper-save-command-stale-0001" }),
            )
            .await;
        assert_eq!(status, 400);

        server.close().await.unwrap();
        daemon.stop();
    });
}

/// The sequence of the next event a client socket receives.
async fn next_sequence<S>(socket: &mut S) -> Value
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    event["sequence"].clone()
}

/// Rust-only: an authorized subscriber receives the stored history and then live events in
/// order, a session token in the subprotocol authorizes a cross-origin socket, and malformed
/// subscriptions lose their connection.
#[test]
fn streams_events_over_a_websocket_and_authorizes_sockets_by_subprotocol() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir_all(&repository).unwrap();
        let project_id = create_id(IdPrefix::Prj);
        let daemon = daemon(&directory.path().join("data"), "socket-bootstrap-token");
        daemon
            .register_project(
                &project_id,
                &repository,
                &directory
                    .path()
                    .join("data/projects")
                    .join(&project_id)
                    .join("nosh.sqlite"),
            )
            .unwrap();
        let server = LocalApiServer::new(Rc::clone(&daemon));
        server.listen("127.0.0.1", 0).await.unwrap();
        let base = server.address().unwrap();
        let address = base.trim_start_matches("http://").to_owned();
        let connect = |path: String, headers: Vec<(&'static str, String)>| {
            let address = address.clone();
            async move {
                let mut request = format!("ws://{address}{path}")
                    .into_client_request()
                    .unwrap();
                for (name, value) in headers {
                    request.headers_mut().insert(name, value.parse().unwrap());
                }
                let stream = tokio::net::TcpStream::connect(&address).await.unwrap();
                tokio_tungstenite::client_async(request, stream).await
            }
        };
        let history = daemon.replay(&project_id, 0).unwrap();
        assert!(!history.is_empty());
        let (mut socket, response) = connect(
            format!("/events?projectId={project_id}&after=0"),
            vec![("sec-websocket-protocol", "nosh.events, auth.ignored".into())],
        )
        .await
        .unwrap();
        assert_eq!(
            response.headers()["sec-websocket-protocol"],
            "nosh.events",
            "the first offered subprotocol is accepted"
        );
        for event in &history {
            assert_eq!(next_sequence(&mut socket).await, event["sequence"]);
        }
        let live = daemon
            .append_draft(&json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "test.live", "source": "test", "scope": { "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null }, "correlationId": null, "causationId": null, "payload": {} }))
            .unwrap();
        assert_eq!(next_sequence(&mut socket).await, live["sequence"]);
        // A resumed subscription starts strictly after its cursor.
        let cursor = live["sequence"].as_i64().unwrap() - 1;
        let (mut resumed, _) = connect(
            format!("/events?projectId={project_id}&after={cursor}"),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(next_sequence(&mut resumed).await, live["sequence"]);

        // A session token in the subprotocol stands in for the bearer header.
        let api = reqwest::Client::new();
        let session: Value = api
            .post(format!("{base}/session"))
            .bearer_auth("socket-bootstrap-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let attacker = ("origin", "http://attacker.invalid".to_owned());
        let authorized = connect(
            format!("/events?projectId={project_id}&after=0"),
            vec![
                attacker.clone(),
                (
                    "sec-websocket-protocol",
                    format!("auth.{}", session["token"].as_str().unwrap()),
                ),
            ],
        )
        .await;
        assert!(authorized.is_ok(), "{:?}", authorized.err());
        for (path, headers) in [
            (
                format!("/events?projectId={project_id}&after=0"),
                vec![
                    attacker.clone(),
                    ("sec-websocket-protocol", "auth.forged".into()),
                ],
            ),
            ("/events?after=0".to_owned(), vec![]),
            (format!("/events?projectId={project_id}&after=-1"), vec![]),
            (format!("/events?projectId={project_id}&after=1.5"), vec![]),
            (format!("/projects?projectId={project_id}"), vec![]),
        ] {
            assert!(connect(path.clone(), headers).await.is_err(), "{path}");
        }
        // `/api` prefixes reach the same stream.
        assert!(
            connect(
                format!("/api/events?projectId={project_id}&after=0"),
                vec![]
            )
            .await
            .is_ok()
        );
        // An unauthorized upgrade loses its connection without a response (`socket.destroy()`);
        // an authorized malformed handshake gets `ws`'s short refusal.
        let raw = |headers: &str| {
            let request = format!(
                "GET /events?projectId={project_id}&after=0 HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\n{headers}\r\n"
            );
            let address = address.clone();
            async move {
                let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut response = Vec::new();
                let read =
                    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
                        .await;
                assert!(read.is_ok(), "the server ends the connection");
                String::from_utf8_lossy(&response).into_owned()
            }
        };
        let key = "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";
        assert_eq!(
            raw(&format!(
                "Upgrade: websocket\r\nOrigin: http://attacker.invalid\r\n{key}Sec-WebSocket-Version: 13\r\n"
            ))
            .await,
            ""
        );
        for (headers, status, message) in [
            (
                "Upgrade: h2c\r\n".to_owned(),
                "400 Bad Request",
                "Invalid Upgrade header",
            ),
            (
                "Upgrade: websocket\r\nSec-WebSocket-Key: short\r\nSec-WebSocket-Version: 13\r\n"
                    .to_owned(),
                "400 Bad Request",
                "Missing or invalid Sec-WebSocket-Key header",
            ),
            (
                format!("Upgrade: websocket\r\n{key}Sec-WebSocket-Version: 12\r\n"),
                "400 Bad Request",
                "Missing or invalid Sec-WebSocket-Version header",
            ),
            (
                format!(
                    "Upgrade: websocket\r\n{key}Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: a,a\r\n"
                ),
                "400 Bad Request",
                "Invalid Sec-WebSocket-Protocol header",
            ),
        ] {
            let response = raw(&headers).await;
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}\r\n"))
                    && response.ends_with(message),
                "{response}"
            );
            assert!(
                response.to_ascii_lowercase().contains("connection: close"),
                "{response}"
            );
        }
        assert!(
            raw(&format!(
                "Upgrade: websocket\r\n{key}Sec-WebSocket-Version: 12\r\n"
            ))
            .await
            .to_ascii_lowercase()
            .contains("sec-websocket-version: 13, 8")
        );
        // The clients leave first: an open socket holds `close` for its closing handshake.
        drop((socket, resumed, authorized));
        server.close().await.unwrap();
        daemon.stop();
    });
}

/// Rust-only: closing the server sends each WebSocket a Close frame without a status code (`ws`'s
/// `client.close()`) and, like Node's `server.close()` with an upgraded socket, finishes only
/// once that socket's closing handshake does.
#[test]
fn closing_the_server_waits_for_each_websocket_closing_handshake() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir_all(&repository).unwrap();
        let project_id = create_id(IdPrefix::Prj);
        let daemon = daemon(&directory.path().join("data"), "closing-bootstrap-token");
        daemon
            .register_project(
                &project_id,
                &repository,
                &directory
                    .path()
                    .join("data/projects")
                    .join(&project_id)
                    .join("nosh.sqlite"),
            )
            .unwrap();
        let server = LocalApiServer::new(Rc::clone(&daemon));
        server.listen("127.0.0.1", 0).await.unwrap();
        let address = server
            .address()
            .unwrap()
            .trim_start_matches("http://")
            .to_owned();
        let request = format!("ws://{address}/events?projectId={project_id}&after=0")
            .into_client_request()
            .unwrap();
        let stream = tokio::net::TcpStream::connect(&address).await.unwrap();
        let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        next_sequence(&mut socket).await;

        let closing = server.close();
        tokio::pin!(closing);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), closing.as_mut())
                .await
                .is_err(),
            "an unanswered closing handshake keeps the server open"
        );
        let frame = loop {
            match tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
            {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => break frame,
                Some(Ok(_)) => {}
                other => panic!("expected a Close frame, got {other:?}"),
            }
        };
        assert_eq!(frame, None, "a Close frame without a status code");
        // Reading on sends tungstenite's reply and completes the handshake.
        while let Some(Ok(_)) = socket.next().await {}
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("close finishes with the handshake")
            .unwrap();
        daemon.stop();
    });
}

/// Rust-only: `/health` needs no session, unknown routes answer 401 before 404, body and query
/// errors are 400s with TypeScript's messages, and `POST /shutdown` requests a stop after its
/// response.
#[test]
fn answers_errors_with_typescript_statuses_and_requests_shutdown_after_responding() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let daemon = daemon(&directory.path().join("data"), "shutdown-bootstrap-token");
        let server = LocalApiServer::new(Rc::clone(&daemon));
        server.listen("127.0.0.1", 0).await.unwrap();
        let base = server.address().unwrap();
        let api = Api {
            client: reqwest::Client::new(),
            base: base.clone(),
        };
        let attacker = [("origin", "http://attacker.invalid")];
        assert_eq!(
            api.status(reqwest::Method::GET, "/health", &attacker).await,
            200
        );
        assert_eq!(
            api.status(reqwest::Method::GET, "/no-such-route", &attacker)
                .await,
            401
        );
        assert_eq!(
            api.get("/no-such-route").await,
            (404, json!({ "error": "not_found" }))
        );
        assert_eq!(
            api.get("/api/health").await,
            (200, json!({ "status": "ok" }))
        );
        let response = api
            .client
            .get(format!("{base}/projects"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(
            api.get("/threads").await,
            (400, json!({ "error": "projectId is required" }))
        );
        assert_eq!(
            api.get("/events?projectId=prj_x&after=abc").await,
            (
                400,
                json!({ "error": "projectId and a non-negative after cursor are required" })
            )
        );
        let malformed = api
            .client
            .post(format!("{base}/chat"))
            .body("{")
            .send()
            .await
            .unwrap();
        assert_eq!(malformed.status().as_u16(), 400);
        let error: Value = malformed.json().await.unwrap();
        assert!(error["error"].as_str().unwrap().contains("JSON"), "{error}");
        assert_eq!(
            api.post(
                "/chat",
                &json!({ "projectId": "prj_x", "message": "hi", "idempotencyKey": "short" })
            )
            .await,
            (
                400,
                json!({ "error": "idempotencyKey must be at least 16 characters" })
            )
        );
        assert_eq!(
            api.post("/chat", &Value::Null).await,
            (
                400,
                json!({ "error": "Cannot read properties of null (reading 'projectId')" })
            )
        );
        let oversized = api
            .client
            .post(format!("{base}/chat"))
            .body(vec![b' '; 3_000_001])
            .send()
            .await
            .unwrap();
        assert_eq!(oversized.status().as_u16(), 400);
        assert_eq!(
            oversized.json::<Value>().await.unwrap(),
            json!({ "error": "request body exceeds 3 MB" })
        );
        assert_eq!(
            api.get("/models").await,
            (200, json!({ "models": [], "defaultModel": null }))
        );

        let mut requested = std::pin::pin!(server.shutdown_requested());
        assert!(requested.as_mut().now_or_never().is_none());
        assert_eq!(
            api.post("/shutdown", &json!({})).await,
            (202, json!({ "stopping": true }))
        );
        tokio::time::timeout(Duration::from_secs(5), requested)
            .await
            .unwrap();
        server.close().await.unwrap();
        assert!(
            server.close().await.is_err(),
            "a closed server is not running"
        );
        daemon.stop();
    });
}

#[test]
fn retains_accepted_runtime_instructions_when_their_durable_effect_fails() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir_all(&repository).unwrap();
        let project_id = create_id(IdPrefix::Prj);
        let daemon = daemon(&directory.path().join("data"), "test-bootstrap-token");
        daemon
            .register_project(
                &project_id,
                &repository,
                &directory
                    .path()
                    .join("data/projects")
                    .join(&project_id)
                    .join("nosh.sqlite"),
            )
            .unwrap();
        let instruction_id = create_id(IdPrefix::Ins);
        let result = daemon
            .submit_tool(
                "nosh_runtime_instruct",
                &project_id,
                "runtime-effect-failure-0001",
                &json!({
                    "$schema": schema_uri("runtime-instruction"), "schemaVersion": 1, "instructionId": instruction_id, "projectId": project_id,
                    "idempotencyKey": "runtime-effect-failure-0001", "proposedByAgentId": null, "issuedAt": nosh_wire::now_utc(),
                    "operation": "STOP", "threadId": create_id(IdPrefix::Thr), "programId": null, "reason": "Exercise durable failed-effect recovery",
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result["accepted"], true, "{result}");
        assert_eq!(result["effect"]["state"], "failed", "{result}");
        assert!(
            daemon
                .research
                .records(&project_id, Some("runtime-instruction"))
                .unwrap()
                .iter()
                .any(|record| record["instructionId"] == instruction_id.as_str())
        );
        daemon.stop();
    });
}

#[test]
fn binds_graph_proposal_approval_replays_to_the_expected_proposal_version() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let project_id = create_id(IdPrefix::Prj);
        let database_path = directory.path().join("project.sqlite");
        let daemon = daemon(&directory.path().join("data"), "test-bootstrap-token");
        daemon
            .register_project(&project_id, directory.path(), &database_path)
            .unwrap();
        let research = &daemon.research;
        let direction = research
            .create_direction(
                &project_id,
                &serde_json::from_value(json!({ "question": "Does the replay bind its intent?", "decisionUse": "Exercise approval idempotency", "idempotencyKey": "create-direction-approval-replay" })).unwrap(),
                None,
            )
            .unwrap();
        let direction_id = direction["entityId"].as_str().unwrap();
        let proposal_id = "proposal_replay";
        let scope = json!({ "projectId": project_id, "missionId": null, "directionId": direction_id, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null });
        let submitted_at = nosh_wire::now_utc();
        let record = json!({
            "$schema": schema_uri("graph-change-proposal"), "schemaVersion": 1, "proposalId": proposal_id, "scopeType": "direction", "scopeId": direction_id,
            "baseGraphVersion": direction["value"]["graphVersion"], "operations": [{ "type": "add_node", "node": { "id": "dnode_approval_replay", "type": "general_worker", "title": "Verify idempotent approval binding", "required": false, "criterionIds": [], "hardDependencies": [], "softDependencies": [], "state": "pending", "attempt": 0, "maximumAttempts": 1, "priority": 1, "criticalWeight": 1, "createdAt": submitted_at, "lease": null } }],
            "rationale": "Verify that approval replays bind the expected proposal version.", "evidenceIds": [], "proposerRole": "user", "approvalRequired": true, "contractImpact": "none", "reasonCode": "reason_replay", "expectedEffect": "Reject a changed expected version for the same idempotency key.", "budgetImpact": { "modelTokensDelta": 0, "gpuSecondsDelta": 0 }, "submittedAt": submitted_at,
        });
        let pending = daemon
            .store_for(&project_id)
            .unwrap()
            .mutate_projection(
                &format!("domain-graph:{proposal_id}"),
                Some(0),
                &json!({ "$schema": EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": "graph.proposal_pending", "source": "noshd", "scope": scope, "correlationId": proposal_id, "causationId": null, "payload": { "proposalId": proposal_id, "contractImpact": "none" } }),
                &ProjectionInput {
                    entity_type: "graph_proposal".into(),
                    entity_id: proposal_id.into(),
                    state: "pending".into(),
                    value: json!({ "record": record, "scope": scope, "submittedAt": submitted_at }),
                    graph: None,
                    records: Vec::new(),
                    intent: None,
                },
            )
            .unwrap();
        let version = pending.projection["version"].as_i64().unwrap();
        research
            .approve_graph_proposal(
                &project_id,
                proposal_id,
                version,
                "approve-graph-proposal-replay",
            )
            .unwrap();
        let replayed = research
            .approve_graph_proposal(
                &project_id,
                proposal_id,
                version + 1,
                "approve-graph-proposal-replay",
            )
            .unwrap_err();
        assert!(
            replayed
                .to_string()
                .contains("different graph-proposal approval"),
            "{replayed}"
        );
        daemon.stop();
    });
}

#[test]
fn recognizes_ipv4_ipv6_and_ipv4_mapped_loopback_addresses() {
    assert!(is_loopback_address("127.0.0.1"));
    assert!(is_loopback_address("127.42.0.9"));
    assert!(is_loopback_address("::1"));
    assert!(is_loopback_address("::ffff:127.0.0.1"));
    assert!(is_loopback_address("::ffff:7f00:1"));
    assert!(!is_loopback_address("192.168.1.10"));
    assert!(!is_loopback_address("::2"));
}

#[test]
fn scopes_mission_job_control_to_both_project_and_mission() {
    let job: nosh_sys::jobs::JobRecord = serde_json::from_value(json!({
        "jobId": "job_a", "projectId": "project-a", "missionId": "mission-a", "directionId": null, "autoresearchId": null, "runId": "run_a", "experimentId": null,
        "commitSha": "0".repeat(40), "workingDirectory": "/w", "runner": "native", "distribution": null, "command": ["true"], "checkpointCommand": null,
        "environmentLockHash": "e", "evaluationContractHash": "c", "timeoutSeconds": 0, "usesGpu": false,
        "state": "running", "commandDigest": "d", "windowsPid": null, "processFingerprint": null, "wslPidFile": null, "stdoutPath": "o", "stderrPath": "e",
        "startedAt": "2026-01-01T00:00:00.000Z", "finishedAt": null, "exitCode": null, "failureReason": null, "recoveredAfterRestart": false,
    }))
    .unwrap();
    assert!(mission_job_matches(&job, "project-a", "mission-a"));
    assert!(!mission_job_matches(&job, "project-a", "mission-b"));
    assert!(!mission_job_matches(&job, "project-b", "mission-a"));
}
