//! Transport tests ported one for one from `apps/tui/src/client.test.ts` ("authenticated daemon
//! client", "daemon live subscriptions") and `apps/tui/src/stream.test.ts` ("native live event
//! transport").
//!
//! `vi.useFakeTimers()` is a paused tokio clock moved with `advance` (each followed by `settle()`, as
//! `advanceTimersByTimeAsync` flushes promises). A fake socket event is queued and the stream task
//! reads it at the next `settle()`. Not representable in Rust: a fetch's `redirect: 'error'` field
//! (the fetcher contract never follows redirects), cursors `1.5`, `Infinity` and `NaN` (the cursor is
//! an `i64`), and non-string frame data other than binary.

mod common;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use common::{local, local_timed, obj, settle, with};
use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared, ready};
use nosh_tui::TuiConfig;
use nosh_tui::js::{JsValue, json_stringify, json_stringify_pretty, length};
use nosh_tui::transport::client::{HttpFetcher, HttpRequest, HttpResponse};
use nosh_tui::transport::socket::{ClientSocket, SocketEvent, SocketFactory};
use nosh_tui::transport::stream::{
    MAX_EVENT_BYTES, SubscribeOptions, TokenSource, subscribe_events,
};
use nosh_tui::transport::{
    ClientError, DaemonApi, DaemonClient, EventCallback, StreamState, Subscription,
};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------------------------
// fakes
// ---------------------------------------------------------------------------------------------

/// `new Response(JSON.stringify(data), {status})`.
fn response(data: &JsValue, status: u16) -> HttpResponse {
    HttpResponse {
        status,
        body: json_stringify(data).into_bytes(),
    }
}

fn parse(text: &str) -> JsValue {
    common::json(text)
}

type Pending = Shared<LocalBoxFuture<'static, Result<HttpResponse, ClientError>>>;

/// Completes a pending fetch (`resolve` of a hand-made `Promise<Response>`).
struct Completer(oneshot::Sender<HttpResponse>);

impl Completer {
    fn complete(self, response: HttpResponse) {
        let _ = self.0.send(response);
    }
}

fn pending_response() -> (Completer, Pending) {
    let (sender, receiver) = oneshot::channel();
    let reply = receiver
        .map(|settled| settled.map_err(|_| ClientError::from("never settled")))
        .boxed_local()
        .shared();
    (Completer(sender), reply)
}

/// `vi.fn()` as a fetcher: one-shot replies first, then the persistent reply. It ignores the abort
/// signal, as the TS fakes do; with neither reply it fails the test.
#[derive(Default)]
struct FakeFetcher {
    calls: RefCell<Vec<HttpRequest>>,
    once: RefCell<VecDeque<Pending>>,
    always: RefCell<Option<Pending>>,
}

impl FakeFetcher {
    fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// `mockResolvedValueOnce`.
    fn once(&self, response: HttpResponse) -> &Self {
        self.once
            .borrow_mut()
            .push_back(ready(Ok(response)).boxed_local().shared());
        self
    }

    /// `mockReturnValueOnce(new Promise(...))`.
    fn once_pending(&self) -> Completer {
        let (completer, reply) = pending_response();
        self.once.borrow_mut().push_back(reply);
        completer
    }

    /// `mockResolvedValue`.
    fn always(&self, response: HttpResponse) {
        *self.always.borrow_mut() = Some(ready(Ok(response)).boxed_local().shared());
    }

    /// `mockReturnValue(new Promise(...))`: every call shares one pending response.
    fn always_pending(&self) -> Completer {
        let (completer, reply) = pending_response();
        *self.always.borrow_mut() = Some(reply);
        completer
    }

    fn calls(&self) -> Vec<HttpRequest> {
        self.calls.borrow().clone()
    }
}

impl HttpFetcher for FakeFetcher {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> LocalBoxFuture<'static, Result<HttpResponse, ClientError>> {
        self.calls.borrow_mut().push(request);
        let reply = self.once.borrow_mut().pop_front();
        let reply = reply
            .or_else(|| self.always.borrow().clone())
            .expect("an unexpected fetch");
        reply.boxed_local()
    }
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> &'a str {
    request
        .headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .unwrap()
}

#[derive(Default)]
struct FakeSocketState {
    events: RefCell<VecDeque<SocketEvent>>,
    notify: Notify,
    closes: Cell<usize>,
}

/// `FakeSocket`: the test queues the events its JS handlers would receive; `close` only counts.
#[derive(Clone, Default)]
struct FakeSocket(Rc<FakeSocketState>);

impl FakeSocket {
    fn push(&self, event: SocketEvent) {
        self.0.events.borrow_mut().push_back(event);
        self.0.notify.notify_one();
    }

    fn open(&self) {
        self.push(SocketEvent::Open);
    }

    fn message(&self, text: &str) {
        self.push(SocketEvent::Text(text.to_owned()));
    }

    fn end(&self) {
        self.push(SocketEvent::Close);
    }

    fn error(&self) {
        self.push(SocketEvent::Error);
    }

    fn closes(&self) -> usize {
        self.0.closes.get()
    }
}

impl ClientSocket for FakeSocket {
    fn next(&self) -> LocalBoxFuture<'static, SocketEvent> {
        let state = self.0.clone();
        async move {
            loop {
                let event = state.events.borrow_mut().pop_front();
                if let Some(event) = event {
                    return event;
                }
                state.notify.notified().await;
            }
        }
        .boxed_local()
    }

    fn close(&self, _code: u16, _reason: &str) {
        self.0.closes.set(self.0.closes.get() + 1);
    }
}

/// `createSocket = vi.fn(() => new FakeSocket())`.
#[derive(Default)]
struct FakeSockets {
    calls: RefCell<Vec<(String, Vec<String>)>>,
    sockets: RefCell<Vec<FakeSocket>>,
}

impl FakeSockets {
    fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.borrow().clone()
    }

    fn count(&self) -> usize {
        self.sockets.borrow().len()
    }

    fn socket(&self, index: usize) -> FakeSocket {
        self.sockets.borrow()[index].clone()
    }

    fn last(&self) -> FakeSocket {
        self.sockets.borrow().last().unwrap().clone()
    }
}

impl SocketFactory for FakeSockets {
    fn create(&self, url: &str, protocols: &[String]) -> Result<Rc<dyn ClientSocket>, String> {
        self.calls
            .borrow_mut()
            .push((url.to_owned(), protocols.to_vec()));
        let socket = FakeSocket::default();
        self.sockets.borrow_mut().push(socket.clone());
        Ok(Rc::new(socket))
    }
}

fn config(base_url: &str, session: Option<&str>, bootstrap: Option<&str>) -> TuiConfig {
    TuiConfig {
        base_url: base_url.into(),
        session_token: session.map(Into::into),
        bootstrap_token: bootstrap.map(Into::into),
        ..TuiConfig::default()
    }
}

fn client(
    config: &TuiConfig,
    fetcher: &Rc<FakeFetcher>,
    sockets: &Rc<FakeSockets>,
) -> DaemonClient {
    DaemonClient::with_transport(config, fetcher.clone(), sockets.clone()).unwrap()
}

/// `vi.fn()` as `onEvent`.
fn ignore_events() -> EventCallback {
    Box::new(|_| Ok(()))
}

fn ignore_states() -> Box<dyn FnMut(StreamState)> {
    Box::new(|_| {})
}

/// `vi.advanceTimersByTimeAsync(ms)`. Queued socket events are read first: their JS handlers ran
/// synchronously, before any timer.
async fn advance(milliseconds: u64) {
    settle().await;
    tokio::time::advance(Duration::from_millis(milliseconds)).await;
    settle().await;
}

async fn failure(future: LocalBoxFuture<'static, Result<JsValue, ClientError>>) -> String {
    future.await.expect_err("the request fails").0
}

// ---------------------------------------------------------------------------------------------
// authenticated daemon client
// ---------------------------------------------------------------------------------------------

#[test]
fn requires_credentials_rather_than_relying_on_anonymous_loopback() {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        let client = client(
            &config("http://127.0.0.1:4317", None, None),
            &fetcher,
            &sockets,
        );
        assert!(
            failure(client.request("/projects", None, None))
                .await
                .contains("token")
        );
        assert!(fetcher.calls().is_empty());
    });
}

#[test]
fn exchanges_bootstrap_and_renews_401_preserving_the_mutation_body() {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher
            .once(response(&parse(r#"{"token":"session1"}"#), 200))
            .once(response(&parse(r#"{"error":"expired"}"#), 401))
            .once(response(&parse(r#"{"token":"session2"}"#), 200))
            .once(response(&parse(r#"{"ok":true}"#), 200));
        let client = client(
            &config("http://127.0.0.1:4317/api", None, Some("bootstrap")),
            &fetcher,
            &sockets,
        );
        client
            .request(
                "/chat",
                Some(parse(r#"{"projectId":"p","idempotencyKey":"stable"}"#)),
                None,
            )
            .await
            .unwrap();
        let calls = fetcher.calls();
        assert_eq!(calls[0].url, "http://127.0.0.1:4317/api/session");
        assert_eq!(header(&calls[0], "authorization"), "Bearer bootstrap");
        assert_eq!(calls[1].body, calls[3].body);
        assert_eq!(header(&calls[3], "authorization"), "Bearer session2");
    });
}

#[test]
fn does_not_retry_ambiguous_mutation_errors_or_stale_versions() {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&parse(r#"{"error":"stale version"}"#), 409));
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message =
            failure(client.request("/missions/m/transition", Some(parse("{}")), None)).await;
        assert!(message.contains("409: stale version"), "{message}");
        assert_eq!(fetcher.calls().len(), 1);
    });
}

#[test]
fn summarizes_serialized_validation_issues_instead_of_hiding_the_field_behind_json_syntax() {
    local(async {
        let issues = parse(
            r#"[{"code":"invalid_type","expected":"string","received":"undefined","path":["templateVersion"],"message":"Required"},{"code":"invalid_string","path":["northStar","contributionType"],"message":"Invalid"},{"code":"invalid_type","expected":"object","received":"undefined","path":["paper"],"message":"Required"},{"code":"unrecognized_keys","path":[],"message":"Unrecognized keys"}]"#,
        );
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        let error = obj(vec![("error", json_stringify_pretty(&issues).into())]);
        fetcher.always(response(&error, 400));
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message =
            failure(client.request("/chat", Some(parse(r#"{"message":"hello"}"#)), None)).await;
        assert!(
            message.contains(
                "400: templateVersion: Required; northStar.contributionType: Invalid; paper: Required (+1 more)"
            ),
            "{message}"
        );
        assert_eq!(fetcher.calls().len(), 1);
    });
}

/// `it.each([...])('accepts structured validation errors with array field paths')`.
fn accepts_structured_validation_errors_with_array_field_paths(error: JsValue) {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&obj(vec![("error", error)]), 400));
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message = failure(client.request("/chat", Some(parse("{}")), None)).await;
        assert!(
            message.contains("400: datasets.0.name: Required"),
            "{message}"
        );
    });
}

#[test]
fn accepts_structured_validation_errors_as_an_array() {
    accepts_structured_validation_errors_with_array_field_paths(parse(
        r#"[{"path":["datasets",0,"name"],"message":"Required"}]"#,
    ));
}

#[test]
fn accepts_structured_validation_errors_as_an_issues_object() {
    accepts_structured_validation_errors_with_array_field_paths(parse(
        r#"{"issues":[{"path":["datasets",0,"name"],"message":"Required"}]}"#,
    ));
}

#[test]
fn accepts_structured_validation_errors_as_json_text() {
    accepts_structured_validation_errors_with_array_field_paths(
        r#"{"issues":[{"path":["datasets",0,"name"],"message":"Required"}]}"#.into(),
    );
}

#[test]
fn bounds_and_sanitizes_validation_paths_and_messages_before_displaying_them() {
    local(async {
        let issue = obj(vec![
            (
                "path",
                JsValue::Array(vec![
                    "paper\x1b]52;c;secret\x07".into(),
                    "name\u{202e}".into(),
                ]),
            ),
            (
                "message",
                format!("Required\n\t{}", "x".repeat(2000)).into(),
            ),
        ]);
        let error = json_stringify(&JsValue::Array(vec![issue]));
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&obj(vec![("error", error.into())]), 400));
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message = failure(client.request("/chat", Some(parse("{}")), None)).await;
        assert!(message.contains("400: paper.name: Required "), "{message}");
        assert!(!message.contains(['\x1b', '\u{202e}', '\n', '\t']));
        assert!(!message.contains("secret"));
        assert!(length(&message) <= 325);
    });
}

/// `it.each([null, {}, {error:null}, {error:{code:'invalid_type'}}])`.
fn handles_an_unusable_error_response_without_masking_its_http_status(body: &str) {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&parse(body), 400));
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message = failure(client.request("/chat", Some(parse("{}")), None)).await;
        assert!(message.contains("400: Daemon request failed"), "{message}");
    });
}

#[test]
fn handles_a_null_error_response() {
    handles_an_unusable_error_response_without_masking_its_http_status("null");
}

#[test]
fn handles_an_empty_error_response() {
    handles_an_unusable_error_response_without_masking_its_http_status("{}");
}

#[test]
fn handles_a_null_error_field() {
    handles_an_unusable_error_response_without_masking_its_http_status(r#"{"error":null}"#);
}

#[test]
fn handles_an_error_object_without_a_message() {
    handles_an_unusable_error_response_without_masking_its_http_status(
        r#"{"error":{"code":"invalid_type"}}"#,
    );
}

#[test]
fn keeps_the_http_status_when_an_error_body_is_not_json() {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(HttpResponse {
            status: 502,
            body: b"<html>Bad gateway</html>".to_vec(),
        });
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message = failure(client.request("/projects", None, None)).await;
        assert!(message.contains("502: Daemon request failed"), "{message}");
    });
}

#[test]
fn keeps_ordinary_errors_readable_and_bounded_without_terminal_controls_or_newlines() {
    local(async {
        let error = format!(
            "Invalid contract\n\x1b[31mpaper\x1b[0m: {}",
            "x".repeat(2000)
        );
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&obj(vec![("error", error.into())]), 400));
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let message = failure(client.request("/chat", Some(parse("{}")), None)).await;
        assert!(
            message.starts_with("400: Invalid contract paper: "),
            "{message}"
        );
        assert!(length(&message) <= 325);
        assert!(!message.contains(['\n', '\x1b']));
    });
}

#[test]
fn rejects_credential_bearing_urls_and_plaintext_remote_hosts() {
    let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
    let error = |base_url: &str| {
        DaemonClient::with_transport(
            &config(base_url, None, None),
            fetcher.clone(),
            sockets.clone(),
        )
        .err()
        .unwrap()
        .0
    };
    assert!(error("http://remote.invalid").contains("HTTPS"));
    assert!(error("https://user:pass@remote.invalid").contains("Invalid"));
}

// ---------------------------------------------------------------------------------------------
// daemon live subscriptions
// ---------------------------------------------------------------------------------------------

#[test]
fn uses_the_http_bootstrap_cursor_and_session_subprotocol_without_adding_credential_urls() {
    local_timed(false, async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&parse(r#"{"events":[{"sequence":91}]}"#), 200));
        let client = client(
            &config("http://localhost:4317", Some("private-session"), None),
            &fetcher,
            &sockets,
        );
        let page = client
            .request("/events?projectId=p&after=0&recent=true", None, None)
            .await
            .unwrap();
        let after = page.get("events").unwrap().as_array().unwrap()[0]
            .get("sequence")
            .and_then(JsValue::as_f64)
            .unwrap();
        let stop = client
            .subscribe("p", after as i64, ignore_events(), ignore_states())
            .unwrap();
        settle().await;
        assert_eq!(fetcher.calls().len(), 1);
        assert_eq!(
            sockets.calls(),
            vec![(
                "ws://localhost:4317/api/events?projectId=p&after=91".to_owned(),
                vec!["auth.private-session".to_owned()]
            )]
        );
        assert_eq!(
            header(&fetcher.calls()[0], "authorization"),
            "Bearer private-session"
        );
        stop.cleanup();
        client.close();
        assert_eq!(sockets.socket(0).closes(), 1);
    });
}

#[test]
fn refreshes_bootstrap_authentication_before_reconnect_preserving_the_last_delivered_cursor() {
    local_timed(true, async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.always(response(&parse(r#"{"token":"renewed-session"}"#), 200));
        let client = client(
            &config(
                "http://localhost:4317",
                Some("old-session"),
                Some("bootstrap-secret"),
            ),
            &fetcher,
            &sockets,
        );
        let _subscription = client
            .subscribe("p", 5, ignore_events(), ignore_states())
            .unwrap();
        settle().await;
        let first = sockets.socket(0);
        first.message(r#"{"eventId":"evt_1","sequence":9,"type":"agent.started","scope":{"projectId":"p"},"payload":{}}"#);
        first.end();
        advance(250).await;
        let calls = fetcher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].url, "http://localhost:4317/api/session");
        assert_eq!(
            header(&calls[0], "authorization"),
            "Bearer bootstrap-secret"
        );
        assert_eq!(
            sockets.calls()[1],
            (
                "ws://localhost:4317/api/events?projectId=p&after=9".to_owned(),
                vec!["auth.renewed-session".to_owned()]
            )
        );
        client.close();
        advance(60_000).await;
        assert_eq!(sockets.count(), 2);
    });
}

#[test]
fn shares_an_in_flight_http_token_renewal_with_a_reconnect_attempt() {
    local_timed(true, async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        fetcher.once(response(&parse("{}"), 401));
        let finish_renew = fetcher.once_pending();
        fetcher.always(response(&parse(r#"{"projects":[]}"#), 200));
        let client = client(
            &config("http://localhost:4317", Some("expired-session"), Some("b")),
            &fetcher,
            &sockets,
        );
        let _subscription = client
            .subscribe("p", 0, ignore_events(), ignore_states())
            .unwrap();
        settle().await;
        let request = client.request("/projects", None, None);
        settle().await;
        assert_eq!(fetcher.calls().len(), 2);
        sockets.socket(0).end();
        advance(250).await;
        assert_eq!(fetcher.calls().len(), 2);
        finish_renew.complete(response(&parse(r#"{"token":"fresh-session"}"#), 200));
        request.await.unwrap();
        settle().await;
        assert_eq!(
            sockets.calls()[1],
            (
                "ws://localhost:4317/api/events?projectId=p&after=0".to_owned(),
                vec!["auth.fresh-session".to_owned()]
            )
        );
        let renewals = fetcher
            .calls()
            .iter()
            .filter(|call| call.url.ends_with("/session"))
            .count();
        assert_eq!(renewals, 1);
        client.close();
    });
}

#[test]
fn aborts_pending_auth_and_all_subscriptions_on_close_and_cannot_resurrect_credentials() {
    local_timed(false, async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        let complete = fetcher.always_pending();
        let client = client(
            &config("http://localhost:4317", None, Some("b")),
            &fetcher,
            &sockets,
        );
        let _subscription = client
            .subscribe("p", 0, ignore_events(), ignore_states())
            .unwrap();
        settle().await;
        let signal = fetcher.calls()[0].signal.clone();
        client.close();
        assert!(signal.is_aborted());
        // Even a fetch implementation that ignores abort cannot repopulate the token.
        complete.complete(response(&parse(r#"{"token":"late-session"}"#), 200));
        settle().await;
        assert_eq!(sockets.count(), 0);
        assert!(client.authenticate().await.is_err());
        assert!(client.request("/projects", None, None).await.is_err());
        assert!(
            client
                .subscribe("p", 0, ignore_events(), ignore_states())
                .is_err()
        );
        assert_eq!(fetcher.calls().len(), 1);
    });
}

#[test]
fn closes_each_active_subscription_exactly_once_and_cancels_pending_retries() {
    local_timed(true, async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        let first = client
            .subscribe("p", 0, ignore_events(), ignore_states())
            .unwrap();
        let second = client
            .subscribe("q", 0, ignore_events(), ignore_states())
            .unwrap();
        settle().await;
        sockets.socket(0).end();
        settle().await;
        client.close();
        first.cleanup();
        second.cleanup();
        client.close();
        advance(60_000).await;
        assert_eq!(sockets.count(), 2);
        assert_eq!(
            [sockets.socket(0).closes(), sockets.socket(1).closes()],
            [1, 1]
        );
    });
}

#[test]
fn does_not_accept_invalid_subscription_projects_or_cursors() {
    local(async {
        let (fetcher, sockets) = (FakeFetcher::new(), FakeSockets::new());
        let client = client(
            &config("http://localhost:4317", Some("s"), None),
            &fetcher,
            &sockets,
        );
        for after in [-1, 9_007_199_254_740_992] {
            let error = client
                .subscribe("p", after, ignore_events(), ignore_states())
                .err()
                .unwrap();
            assert!(error.0.contains("cursor"), "{after}");
        }
        let error = client
            .subscribe("", 0, ignore_events(), ignore_states())
            .err()
            .unwrap();
        assert!(error.0.contains("project"));
        client.close();
    });
}

// ---------------------------------------------------------------------------------------------
// native live event transport
// ---------------------------------------------------------------------------------------------

/// `getToken = vi.fn().mockResolvedValue('secret-session')` with its call log.
struct Tokens {
    calls: RefCell<Vec<bool>>,
    once: RefCell<VecDeque<Result<String, String>>>,
    value: RefCell<Shared<LocalBoxFuture<'static, Result<String, String>>>>,
}

impl Tokens {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            calls: RefCell::new(Vec::new()),
            once: RefCell::new(VecDeque::new()),
            value: RefCell::new(
                ready(Ok("secret-session".to_owned()))
                    .boxed_local()
                    .shared(),
            ),
        })
    }

    /// `mockResolvedValue(token)`.
    fn resolve_always(&self, token: &str) {
        *self.value.borrow_mut() = ready(Ok(token.to_owned())).boxed_local().shared();
    }

    /// `mockReturnValue(new Promise(...))`.
    fn pending_always(&self) -> oneshot::Sender<String> {
        let (sender, receiver) = oneshot::channel::<String>();
        *self.value.borrow_mut() = receiver
            .map(|token| token.map_err(|_| "never settled".to_owned()))
            .boxed_local()
            .shared();
        sender
    }

    /// `mockRejectedValueOnce(new Error(message))`.
    fn reject_once(&self, message: &str) {
        self.once.borrow_mut().push_back(Err(message.to_owned()));
    }

    fn source(self: &Rc<Self>) -> TokenSource {
        let tokens = self.clone();
        Rc::new(move |refresh| {
            tokens.calls.borrow_mut().push(refresh);
            let once = tokens.once.borrow_mut().pop_front();
            let reply = once.map_or_else(
                || tokens.value.borrow().clone().boxed_local(),
                |reply| ready(reply).boxed_local(),
            );
            reply.map(|reply| reply.map_err(ClientError)).boxed_local()
        })
    }
}

/// The fakes and recorders of `setup(after)` in stream.test.ts.
#[derive(Clone)]
struct Parts {
    sockets: Rc<FakeSockets>,
    tokens: Rc<Tokens>,
    events: Rc<RefCell<Vec<JsValue>>>,
    states: Rc<RefCell<Vec<StreamState>>>,
    /// `onEvent.mockImplementationOnce(() => { throw ... })`.
    throw_next: Rc<Cell<bool>>,
}

/// `setup(after)`: the parts plus the subscription's cleanup.
struct Setup {
    parts: Parts,
    stop: Subscription,
}

impl std::ops::Deref for Setup {
    type Target = Parts;

    fn deref(&self) -> &Parts {
        &self.parts
    }
}

fn subscribe(base_url: &str, project_id: &str, after: i64, parts: &Parts) -> Subscription {
    let Parts {
        events,
        states,
        throw_next,
        ..
    } = parts.clone();
    subscribe_events(SubscribeOptions {
        base_url: base_url.into(),
        project_id: project_id.into(),
        after,
        get_token: parts.tokens.source(),
        create_socket: parts.sockets.clone(),
        on_event: Box::new(move |event| {
            events.borrow_mut().push(event.clone());
            if throw_next.replace(false) {
                Err(())
            } else {
                Ok(())
            }
        }),
        on_state: Box::new(move |state| states.borrow_mut().push(state)),
    })
    .unwrap()
}

fn setup(after: i64) -> Setup {
    let parts = Parts {
        sockets: FakeSockets::new(),
        tokens: Tokens::new(),
        events: Rc::new(RefCell::new(Vec::new())),
        states: Rc::new(RefCell::new(Vec::new())),
        throw_next: Rc::new(Cell::new(false)),
    };
    let stop = subscribe("http://127.0.0.1:4317/api", "prj_test", after, &parts);
    Setup { parts, stop }
}

impl Setup {
    fn last_state(&self) -> Option<StreamState> {
        self.states.borrow().last().copied()
    }
}

/// `event(eventId, sequence, extra)` of stream.test.ts.
fn stream_event(event_id: &str, sequence: Option<f64>, extra: Vec<(&str, JsValue)>) -> JsValue {
    let durable = sequence.is_some();
    let base = obj(vec![
        ("eventId", event_id.into()),
        ("sequence", sequence.map_or(JsValue::Null, JsValue::from)),
        (
            "type",
            if durable {
                "agent.started"
            } else {
                "agent.text_delta"
            }
            .into(),
        ),
        (
            "payload",
            if durable {
                obj(vec![])
            } else {
                obj(vec![("delta", "hello".into())])
            },
        ),
        (
            "scope",
            obj(vec![
                ("projectId", "prj_test".into()),
                ("agentId", "agt_test".into()),
            ]),
        ),
        ("correlationId", "task_test".into()),
        (
            "retention",
            if durable { "persistent" } else { "ephemeral" }.into(),
        ),
    ]);
    with(&base, extra)
}

#[test]
fn resumes_the_bootstrap_cursor_with_auth_only_in_the_websocket_subprotocol() {
    local_timed(true, async {
        let s = setup(12);
        assert!(s.sockets.calls().is_empty());
        settle().await;
        let calls = s.sockets.calls();
        assert_eq!(
            calls,
            vec![(
                "ws://127.0.0.1:4317/api/events?projectId=prj_test&after=12".to_owned(),
                vec!["auth.secret-session".to_owned()]
            )]
        );
        assert!(!calls[0].0.contains("secret-session"));
        assert_eq!(*s.tokens.calls.borrow(), [false]);
        s.sockets.socket(0).open();
        settle().await;
        assert_eq!(
            *s.states.borrow(),
            [StreamState::Connecting, StreamState::Connected]
        );
        s.stop.cleanup();
    });
}

#[test]
fn delivers_exact_pi_text_tool_deltas_scopes_projects_and_deduplicates_without_advancing_null_sequences()
 {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        let socket = s.sockets.socket(0);
        socket.open();
        let first = stream_event(
            "evt_a",
            None,
            vec![("payload", obj(vec![("delta", "hello ".into())]))],
        );
        let second = stream_event(
            "evt_b",
            None,
            vec![("payload", obj(vec![("delta", "世界".into())]))],
        );
        let tool = stream_event(
            "evt_c",
            None,
            vec![
                ("type", "agent.tool_update".into()),
                (
                    "payload",
                    parse(
                        r#"{"toolCallId":"call_1","toolName":"read","partialResult":{"content":[]}}"#,
                    ),
                ),
            ],
        );
        let frames = [
            first.clone(),
            first.clone(),
            second.clone(),
            tool.clone(),
            stream_event(
                "evt_wrong",
                Some(500.0),
                vec![("scope", obj(vec![("projectId", "other".into())]))],
            ),
            stream_event("evt_old", Some(12.0), vec![]),
            stream_event("evt_d", Some(16.0), vec![]),
            stream_event("evt_dup", Some(16.0), vec![]),
            stream_event("evt_outoforder", Some(15.0), vec![]),
        ];
        for frame in &frames {
            socket.message(&json_stringify(frame));
        }
        settle().await;
        assert_eq!(
            *s.events.borrow(),
            vec![
                first,
                second,
                tool,
                stream_event("evt_d", Some(16.0), vec![])
            ]
        );
        socket.end();
        settle().await;
        assert_eq!(s.last_state(), Some(StreamState::Disconnected));
        advance(250).await;
        assert_eq!(*s.tokens.calls.borrow(), [false, true]);
        assert!(s.sockets.calls()[1].0.contains("after=16"));
        s.stop.cleanup();
    });
}

#[test]
fn acks_only_ingested_events_so_consumer_errors_replay_from_the_previous_durable_cursor() {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        s.throw_next.set(true);
        s.sockets.socket(0).message(&json_stringify(&stream_event(
            "evt_failed",
            Some(20.0),
            vec![],
        )));
        settle().await;
        assert_eq!(s.last_state(), Some(StreamState::Disconnected));
        advance(250).await;
        assert!(s.sockets.calls()[1].0.contains("after=12"));
        s.sockets.socket(1).message(&json_stringify(&stream_event(
            "evt_failed",
            Some(20.0),
            vec![],
        )));
        settle().await;
        assert_eq!(s.events.borrow().len(), 2);
        s.stop.cleanup();
    });
}

#[test]
fn closes_and_detaches_once_late_socket_callbacks_cannot_replay_into_a_new_subscription() {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        let socket = s.sockets.socket(0);
        s.stop.cleanup();
        s.stop.cleanup();
        socket.message(&json_stringify(&stream_event("evt_late", None, vec![])));
        socket.end();
        socket.open();
        advance(60_000).await;
        assert!(s.events.borrow().is_empty());
        assert_eq!(s.sockets.count(), 1);
        assert_eq!(socket.closes(), 1);
        assert_eq!(
            *s.states.borrow(),
            [StreamState::Connecting, StreamState::Disconnected]
        );
    });
}

#[test]
fn cleanup_before_admission_or_while_authentication_is_pending_cannot_create_a_socket() {
    local_timed(true, async {
        let first = setup(12);
        first.stop.cleanup();
        settle().await;
        assert!(first.tokens.calls.borrow().is_empty());
        let s = setup(12);
        let resolve = s.tokens.pending_always();
        settle().await;
        s.stop.cleanup();
        let _ = resolve.send("late-secret".into());
        settle().await;
        assert_eq!(s.sockets.count(), 0);
    });
}

#[test]
fn backs_off_repeated_failures_with_a_ten_second_cap_including_short_lived_opens() {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        for delay in [250, 500, 1000, 2000, 4000, 8000, 10_000, 10_000] {
            let count = s.sockets.count();
            s.sockets.last().open();
            s.sockets.last().error();
            settle().await;
            advance(delay - 1).await;
            assert_eq!(s.sockets.count(), count, "{delay}");
            advance(1).await;
            assert_eq!(s.sockets.count(), count + 1, "{delay}");
        }
        // A stable connection resets the delay, not every successful handshake.
        s.sockets.last().open();
        settle().await;
        advance(5000).await;
        s.sockets.last().end();
        settle().await;
        let count = s.sockets.count();
        advance(250).await;
        assert_eq!(s.sockets.count(), count + 1);
        s.stop.cleanup();
    });
}

#[test]
fn times_out_a_stalled_handshake_and_cancels_reconnect_timers_on_cleanup() {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        advance(15_000).await;
        assert_eq!(s.last_state(), Some(StreamState::Disconnected));
        assert_eq!(s.sockets.socket(0).closes(), 1);
        s.stop.cleanup();
        advance(60_000).await;
        assert_eq!(s.sockets.count(), 1);
    });
}

#[test]
fn retries_authentication_failures_without_exposing_their_error_or_token() {
    local_timed(true, async {
        let s = setup(12);
        s.tokens.reject_once("credential secret-session");
        settle().await;
        assert_eq!(s.sockets.count(), 0);
        assert_eq!(s.last_state(), Some(StreamState::Disconnected));
        advance(250).await;
        assert_eq!(*s.tokens.calls.borrow(), [false, true]);
        assert!(!format!("{:?}", s.states.borrow()).contains("secret-session"));
        assert_eq!(s.sockets.count(), 1);
        s.stop.cleanup();
    });
}

/// `it.each(['bad token', 'secret\nheader', '', 'x'.repeat(8193)])`.
fn rejects_invalid_protocol_credentials_safely(token: &str) {
    local_timed(true, async {
        let s = setup(12);
        s.tokens.resolve_always(token);
        settle().await;
        assert_eq!(s.sockets.count(), 0);
        assert_eq!(s.last_state(), Some(StreamState::Disconnected));
        s.stop.cleanup();
    });
}

#[test]
fn rejects_a_token_with_a_space() {
    rejects_invalid_protocol_credentials_safely("bad token");
}

#[test]
fn rejects_a_token_with_a_newline() {
    rejects_invalid_protocol_credentials_safely("secret\nheader");
}

#[test]
fn rejects_an_empty_token() {
    rejects_invalid_protocol_credentials_safely("");
}

#[test]
fn rejects_an_oversized_token() {
    rejects_invalid_protocol_credentials_safely(&"x".repeat(8193));
}

/// `it.each([...])('rejects %s frames, preserving the durable replay cursor')`.
fn rejects_frames_preserving_the_durable_replay_cursor(frame: SocketEvent) {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        s.sockets.socket(0).push(frame);
        settle().await;
        assert!(s.events.borrow().is_empty());
        assert_eq!(s.sockets.socket(0).closes(), 1);
        advance(250).await;
        assert!(s.sockets.calls()[1].0.contains("after=12"));
        s.stop.cleanup();
    });
}

fn text_frame(text: String) -> SocketEvent {
    SocketEvent::Text(text)
}

#[test]
fn rejects_invalid_json_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame("{".into()));
}

#[test]
fn rejects_array_envelope_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame("[]".into()));
}

#[test]
fn rejects_missing_sequence_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame(
        r#"{"eventId":"evt_bad","type":"agent.started","scope":{"projectId":"prj_test"},"payload":{}}"#
            .into(),
    ));
}

#[test]
fn rejects_invalid_sequence_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame(json_stringify(&stream_event(
        "evt_bad",
        Some(-1.0),
        vec![],
    ))));
}

#[test]
fn rejects_fractional_sequence_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame(json_stringify(&stream_event(
        "evt_bad",
        Some(1.5),
        vec![],
    ))));
}

#[test]
fn rejects_retention_mismatch_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame(json_stringify(&stream_event(
        "evt_bad",
        Some(13.0),
        vec![("retention", "ephemeral".into())],
    ))));
}

#[test]
fn rejects_oversized_ascii_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame(
        "x".repeat(MAX_EVENT_BYTES + 1),
    ));
}

#[test]
fn rejects_oversized_utf8_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(text_frame(
        "界".repeat(MAX_EVENT_BYTES / 2),
    ));
}

#[test]
fn rejects_binary_frames() {
    rejects_frames_preserving_the_durable_replay_cursor(SocketEvent::Binary);
}

#[test]
fn accepts_a_bounded_large_text_frame_and_never_queues_frames_for_rendering() {
    local_timed(true, async {
        let s = setup(12);
        settle().await;
        let socket = s.sockets.socket(0);
        let bounded = stream_event(
            "evt_large",
            None,
            vec![(
                "payload",
                obj(vec![("delta", "a".repeat(MAX_EVENT_BYTES - 512).into())]),
            )],
        );
        socket.message(&json_stringify(&bounded));
        settle().await;
        assert_eq!(s.events.borrow()[0], bounded);
        for index in 0..2000 {
            socket.message(&json_stringify(&stream_event(
                &format!("evt_{index}"),
                Some(13.0 + f64::from(index)),
                vec![],
            )));
        }
        settle().await;
        assert_eq!(s.events.borrow().len(), 2001);
        socket.end();
        advance(250).await;
        assert!(s.sockets.calls()[1].0.contains("after=2012"));
        s.stop.cleanup();
    });
}

#[test]
fn uses_wss_for_https_and_url_encodes_project_ids_without_mixing_them_with_credentials() {
    local_timed(true, async {
        let s = setup(12);
        s.stop.cleanup();
        let stop = subscribe(
            "https://example.test/api",
            "project & after=0",
            42,
            &s.parts,
        );
        settle().await;
        let url = url::Url::parse(&s.sockets.calls()[0].0).unwrap();
        assert_eq!(url.scheme(), "wss");
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            vec![
                ("projectId".to_owned(), "project & after=0".to_owned()),
                ("after".to_owned(), "42".to_owned())
            ]
        );
        stop.cleanup();
    });
}
