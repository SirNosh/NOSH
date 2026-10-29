//! The live event stream: a port of `apps/tui/src/stream.ts` (`subscribeEvents`, `isStreamEvent`).
//!
//! Specified by tui-transport.md §6 (validation order §6.3.1, state §6.3.2, connect §6.3.7, frames
//! §6.3.8, cleanup §6.3.9, state sequences §6.3.10), §10.3 (tests) and §11.3 (one `spawn_local` task
//! per subscription, generation checks after every await, `tokio::time::Instant` for the 5 s
//! stability rule so paused-clock tests see it). Frames parse with `crate::js::js_json_parse`, which
//! has no nesting limit (TA-2); parse errors are never shown.
//!
//! Subscribe only after the recent HTTP bootstrap. The server attaches its live listener before
//! replaying durable events after this cursor. Never infer gaps from nonconsecutive sequences: they
//! are global, not project-local. No frame queue is kept here: consumers ingest synchronously and
//! coalesce their own rendering. Keep HTTP polling as the durable fallback.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use futures_util::future::LocalBoxFuture;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use url::Url;

use super::client::check_daemon_url;
use super::errors::{
    INVALID_STREAM_CURSOR, INVALID_STREAM_PROJECT, INVALID_STREAM_URL, INVALID_URL,
    STREAM_DETACHED, STREAM_DETACHED_CODE,
};
use super::socket::{ClientSocket, SocketEvent, SocketFactory};
use super::{ClientError, EventCallback, StateCallback, Subscription};
use crate::js::{JsValue, MAX_SAFE_INTEGER, is_safe_integer, js_json_parse, length};

/// `MAX_EVENT_BYTES`: a text frame longer than this fails the connection. TS checks the UTF-16
/// length and then the UTF-8 byte length; UTF-8 bytes are never fewer than UTF-16 units, so the
/// Rust check is `text.len() > MAX_EVENT_BYTES` (transport §6.3.8). Exactly 1 MiB is accepted.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// `MAX_SEEN_IDS`: delivered event ids remembered for dedupe.
pub const MAX_SEEN_IDS: usize = 512;
/// `CONNECT_TIMEOUT_MS`: the handshake deadline, disarmed by `Open`.
pub const CONNECT_TIMEOUT_MS: u64 = 15_000;
/// `RETRY_BASE_MS`: the first reconnect delay.
pub const RETRY_BASE_MS: u64 = 250;
/// `RETRY_MAX_MS`: the reconnect delay cap.
pub const RETRY_MAX_MS: u64 = 10_000;
/// `STABLE_CONNECTION_MS`: a connection open this long resets the failure counter.
pub const STABLE_CONNECTION_MS: u64 = 5_000;

/// `StreamState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamState {
    Connecting,
    Connected,
    Disconnected,
}

impl StreamState {
    /// The TS string: `connecting`, `connected` or `disconnected`.
    pub fn as_str(self) -> &'static str {
        match self {
            StreamState::Connecting => "connecting",
            StreamState::Connected => "connected",
            StreamState::Disconnected => "disconnected",
        }
    }
}

/// `getToken(refresh)`: resolves with the token for the next connect; `refresh` is true for every
/// connect after the first (transport §5.5 step 3).
pub type TokenSource = Rc<dyn Fn(bool) -> LocalBoxFuture<'static, Result<String, ClientError>>>;

/// The options of `subscribeEvents` (transport §6.3).
pub struct SubscribeOptions {
    /// The client's normalized API base, ending in `/api`.
    pub base_url: String,
    pub project_id: String,
    /// The last durable sequence already applied; validated as a non-negative safe integer.
    pub after: i64,
    pub get_token: TokenSource,
    pub create_socket: Rc<dyn SocketFactory>,
    pub on_event: EventCallback,
    pub on_state: StateCallback,
}

/// The closure variables of `subscribeEvents`. The retry and connect timers live in the task.
struct Stream {
    url: RefCell<Url>,
    project_id: String,
    cursor: Cell<i64>,
    stopped: Cell<bool>,
    generation: Cell<u64>,
    attempted: Cell<bool>,
    failures: Cell<u32>,
    opened_at: Cell<Option<Instant>>,
    socket: RefCell<Option<Rc<dyn ClientSocket>>>,
    seen: RefCell<Seen>,
    get_token: TokenSource,
    create_socket: Rc<dyn SocketFactory>,
    on_event: RefCell<Option<EventCallback>>,
    on_state: RefCell<Option<StateCallback>>,
}

/// `seen`: an insertion-ordered set of delivered event ids.
#[derive(Default)]
struct Seen {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl Seen {
    fn insert(&mut self, id: &str) {
        if self.ids.insert(id.to_owned()) {
            self.order.push_back(id.to_owned());
        }
        if self.order.len() > MAX_SEEN_IDS
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
    }

    fn clear(&mut self) {
        self.order.clear();
        self.ids.clear();
    }
}

/// RFC 6455 subprotocol token syntax. Never put credentials in URLs or errors.
fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 8192
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&byte))
}

impl Stream {
    /// `active(id)`.
    fn active(&self, id: u64) -> bool {
        !self.stopped.get() && id == self.generation.get()
    }

    fn bump(&self) -> u64 {
        self.generation.set(self.generation.get() + 1);
        self.generation.get()
    }

    /// `state(value)`. The callback is taken out while it runs, so a state change it causes
    /// re-entrantly (its own cleanup) is not reported again.
    fn state(&self, value: StreamState) {
        let callback = self.on_state.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(value);
            *self.on_state.borrow_mut() = Some(callback);
        }
    }

    /// `releaseSocket()`: the socket is never read again.
    fn release_socket(&self) {
        let previous = self.socket.borrow_mut().take();
        if let Some(previous) = previous {
            previous.close(STREAM_DETACHED_CODE, STREAM_DETACHED);
        }
    }

    /// `failed(id)`: the retry delay, or `None` when there is nothing to retry.
    fn failed(&self, id: u64) -> Option<Duration> {
        if !self.active(id) {
            return None;
        }
        self.bump();
        self.release_socket();
        // All ephemeral drafts must be discarded, not replayed.
        self.state(StreamState::Disconnected);
        if self.stopped.get() {
            return None;
        }
        if self
            .opened_at
            .take()
            .is_some_and(|opened| opened.elapsed() >= Duration::from_millis(STABLE_CONNECTION_MS))
        {
            self.failures.set(0);
        }
        let failures = self.failures.get();
        self.failures.set(failures + 1);
        Some(Duration::from_millis(
            (RETRY_BASE_MS << failures.min(6)).min(RETRY_MAX_MS),
        ))
    }

    /// `onmessage` for a text frame; `Err` fails the connection.
    fn message(&self, id: u64, text: &str) -> Result<(), ()> {
        // Daemon frames are text JSON. Reject oversized frames without copying or queuing.
        if text.len() > MAX_EVENT_BYTES {
            return Err(());
        }
        let event = js_json_parse(text).map_err(|_| ())?;
        if !is_stream_event(&event) {
            return Err(());
        }
        let event_id = event.get("eventId").and_then(JsValue::as_str).unwrap_or("");
        let project = event
            .get("scope")
            .and_then(|scope| scope.get("projectId"))
            .and_then(JsValue::as_str);
        if project != Some(self.project_id.as_str()) || self.seen.borrow().ids.contains(event_id) {
            return Ok(());
        }
        let sequence = event.get("sequence").and_then(JsValue::as_f64);
        if sequence.is_some_and(|sequence| sequence <= self.cursor.get() as f64) {
            return Ok(());
        }
        let callback = self.on_event.borrow_mut().take();
        if let Some(mut callback) = callback {
            let delivered = callback(&event);
            *self.on_event.borrow_mut() = Some(callback);
            delivered?;
        }
        if !self.active(id) {
            return Ok(());
        }
        // Ack only after successful synchronous ingestion; a failed callback replays.
        if let Some(sequence) = sequence {
            self.cursor.set(sequence as i64);
        }
        self.seen.borrow_mut().insert(event_id);
        Ok(())
    }

    /// `connect()` and the socket's handlers for one generation; returns the retry delay, or
    /// `None` when the subscription ended.
    async fn connect(&self) -> Option<Duration> {
        if self.stopped.get() {
            return None;
        }
        let id = self.bump();
        self.state(StreamState::Connecting);
        if !self.active(id) {
            return None;
        }
        let refresh = self.attempted.replace(true);
        let token = (self.get_token)(refresh).await;
        if !self.active(id) {
            return None;
        }
        let token = match token {
            Ok(token) if valid_token(&token) => token,
            _ => return self.failed(id),
        };
        let url = {
            let mut url = self.url.borrow_mut();
            url.query_pairs_mut()
                .clear()
                .append_pair("projectId", &self.project_id)
                .append_pair("after", &self.cursor.get().to_string());
            url.to_string()
        };
        let Ok(socket) = self.create_socket.create(&url, &[format!("auth.{token}")]) else {
            return self.failed(id);
        };
        *self.socket.borrow_mut() = Some(socket.clone());
        let deadline = Instant::now() + Duration::from_millis(CONNECT_TIMEOUT_MS);
        let mut opened = false;
        loop {
            let event = if opened {
                socket.next().await
            } else {
                tokio::select! {
                    event = socket.next() => event,
                    () = tokio::time::sleep_until(deadline) => return self.failed(id),
                }
            };
            if !self.active(id) {
                return None;
            }
            match event {
                SocketEvent::Open => {
                    opened = true;
                    self.opened_at.set(Some(Instant::now()));
                    self.state(StreamState::Connected);
                }
                SocketEvent::Text(text) => {
                    if self.message(id, &text).is_err() {
                        return self.failed(id);
                    }
                }
                SocketEvent::Binary | SocketEvent::Error | SocketEvent::Close => {
                    return self.failed(id);
                }
            }
            if !self.active(id) {
                return None;
            }
        }
    }

    /// The returned cleanup.
    fn stop(&self, task: &AbortHandle) {
        if self.stopped.replace(true) {
            return;
        }
        self.bump();
        task.abort();
        self.release_socket();
        self.seen.borrow_mut().clear();
        self.state(StreamState::Disconnected);
    }
}

/// `subscribeEvents`: validates synchronously (§6.3.1, throwing in that order), then spawns the
/// connect/retry task on the current `LocalSet` and returns its cleanup handle. The returned
/// [`Subscription`] emits the final `Disconnected` from `cleanup` and stops silently on drop.
pub fn subscribe_events(options: SubscribeOptions) -> Result<Subscription, ClientError> {
    if options.project_id.is_empty() || length(&options.project_id) > 1024 {
        return Err(INVALID_STREAM_PROJECT.into());
    }
    if options.after < 0 || options.after as f64 > MAX_SAFE_INTEGER {
        return Err(INVALID_STREAM_CURSOR.into());
    }
    let mut url = Url::parse(&format!("{}/events", options.base_url))
        .map_err(|_| ClientError::from(INVALID_URL))?;
    check_daemon_url(&url, INVALID_STREAM_URL)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| ClientError::from(INVALID_STREAM_URL))?;
    let stream = Rc::new(Stream {
        url: RefCell::new(url),
        project_id: options.project_id,
        cursor: Cell::new(options.after),
        stopped: Cell::new(false),
        generation: Cell::new(0),
        attempted: Cell::new(false),
        failures: Cell::new(0),
        opened_at: Cell::new(None),
        socket: RefCell::new(None),
        seen: RefCell::new(Seen::default()),
        get_token: options.get_token,
        create_socket: options.create_socket,
        on_event: RefCell::new(Some(options.on_event)),
        on_state: RefCell::new(Some(options.on_state)),
    });
    // Admission is deferred: nothing runs until the caller yields, so it can store the cleanup
    // before any state callback.
    let runner = stream.clone();
    let task = tokio::task::spawn_local(async move {
        while let Some(delay) = runner.connect().await {
            tokio::time::sleep(delay).await;
        }
    })
    .abort_handle();
    let (cleanup, silent) = (stream.clone(), stream);
    let silent_task = task.clone();
    Ok(
        Subscription::new(move || cleanup.stop(&task)).on_drop(move || {
            silent.stopped.set(true);
            silent_task.abort();
            silent.release_socket();
        }),
    )
}

/// `isStreamEvent`: the envelope check applied to every parsed frame (transport §6.5).
pub fn is_stream_event(value: &JsValue) -> bool {
    let (Some(scope), Some(JsValue::Object(_))) = (
        value.get("scope").filter(|scope| scope.is_object()),
        value.get("payload"),
    ) else {
        return false;
    };
    let text = |value: Option<&JsValue>, maximum: usize, non_empty: bool| {
        value
            .and_then(JsValue::as_str)
            .is_some_and(|text| (!non_empty || !text.is_empty()) && length(text) <= maximum)
    };
    if !text(value.get("eventId"), 256, true)
        || !text(value.get("type"), 128, true)
        || !text(scope.get("projectId"), 1024, false)
    {
        return false;
    }
    let durable = match value.get("sequence") {
        Some(JsValue::Null) => false,
        Some(JsValue::Number(number)) if is_safe_integer(*number) && *number >= 1.0 => true,
        _ => return false,
    };
    let retention = if durable { "persistent" } else { "ephemeral" };
    if value
        .get("retention")
        .is_some_and(|value| value.as_str() != Some(retention))
    {
        return false;
    }
    let nullable_text =
        |value: Option<&JsValue>| matches!(value, None | Some(JsValue::Null | JsValue::String(_)));
    nullable_text(scope.get("agentId")) && nullable_text(value.get("correlationId"))
}
