//! The `/events` stream of one subscriber (`subscribe` in `apps/noshd/src/server.ts`).
//!
//! Durable events stay in SQLite until sent; only transient events enter memory. A slow consumer
//! is disconnected explicitly and must reconnect from its own last received durable cursor and
//! refresh its queries (transient events are not replayable).

use futures_util::future::LocalBoxFuture;
use nosh_orchestration::daemon::Listener;
use nosh_orchestration::daemon::NoshDaemon;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;
use tokio::sync::Notify;

pub const SLOW_CONSUMER: &str =
    "Slow consumer; reconnect from last received cursor; refresh queries";
const MAXIMUM_BYTES: usize = 1024 * 1024;
const MAXIMUM_TRANSIENT: usize = 300;

/// The subscriber's connection, as the pump uses a `ws` WebSocket.
pub trait EventSocket {
    /// Sends one text message; resolves once it is written (`send`'s callback).
    fn send(&self, message: String) -> LocalBoxFuture<'_, Result<(), String>>;
    /// Starts the closing handshake.
    fn close(&self, code: u16, reason: &str);
    /// Drops the connection without a handshake.
    fn terminate(&self);
    /// Bytes accepted by `send` but not yet written.
    fn buffered_amount(&self) -> usize;
    /// `readyState === OPEN`.
    fn is_open(&self) -> bool;
    /// Resolves once the connection has closed (`Ok`) or failed (`Err`).
    fn closed(&self) -> LocalBoxFuture<'_, Result<(), String>>;
}

/// Where the pump reads events: the daemon's store and its event fan-out.
pub trait EventSource {
    /// The first stored event after `after`, if any.
    fn replay_one(&self, project_id: &str, after: i64) -> Result<Option<Value>, String>;
    fn subscribe(&self, listener: Listener) -> u64;
    fn unsubscribe(&self, id: u64);
}

impl EventSource for NoshDaemon {
    fn replay_one(&self, project_id: &str, after: i64) -> Result<Option<Value>, String> {
        let page = self
            .replay_page(project_id, after, 1, false)
            .map_err(|error| error.to_string())?;
        Ok(page.events.into_iter().next())
    }
    fn subscribe(&self, listener: Listener) -> u64 {
        NoshDaemon::subscribe(self, listener)
    }
    fn unsubscribe(&self, id: u64) {
        NoshDaemon::unsubscribe(self, id);
    }
}

/// `JSON.stringify(event)`.
fn stringify(event: &Value) -> String {
    let mut event = event.clone();
    nosh_wire::normalize_numbers(&mut event);
    event.to_string()
}

struct Subscription<S, D> {
    socket: Rc<S>,
    source: Rc<D>,
    project_id: String,
    cursor: Cell<i64>,
    stopped: Cell<bool>,
    transient: RefCell<VecDeque<String>>,
    transient_bytes: Cell<usize>,
    wake: Notify,
    listener: Cell<Option<u64>>,
}

impl<S: EventSocket + 'static, D: EventSource + 'static> Subscription<S, D> {
    /// Leaves the fan-out and drops queued transient events.
    fn release(&self) {
        if let Some(id) = self.listener.take() {
            self.source.unsubscribe(id);
        }
        self.transient.borrow_mut().clear();
        self.wake.notify_one();
    }

    /// Closes with `code`, and drops the connection if the closing handshake has not finished a
    /// second later.
    fn stop(&self, code: u16, reason: &str) {
        if self.stopped.replace(true) {
            return;
        }
        self.release();
        self.socket.close(code, reason);
        let socket = Rc::clone(&self.socket);
        tokio::task::spawn_local(async move {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(1)) => socket.terminate(),
                _ = socket.closed() => {}
            }
        });
    }

    /// A stored event reached the daemon's fan-out.
    fn deliver(&self, event: &Value) {
        if self.stopped.get() || event["scope"]["projectId"] != self.project_id.as_str() {
            return;
        }
        if event.get("sequence") == Some(&Value::Null) {
            let message = stringify(event);
            let bytes = self.transient_bytes.get() + message.len();
            self.transient_bytes.set(bytes);
            if self.transient.borrow().len() >= MAXIMUM_TRANSIENT
                || bytes + self.socket.buffered_amount() > MAXIMUM_BYTES
            {
                self.stop(1013, SLOW_CONSUMER);
                return;
            }
            self.transient.borrow_mut().push_back(message);
        }
        self.wake.notify_one();
    }

    /// Sends durable events in cursor order, then queued transient ones, one at a time.
    async fn pump(self: Rc<Self>) {
        loop {
            if self.stopped.get() || !self.socket.is_open() {
                return;
            }
            let Ok(event) = self.source.replay_one(&self.project_id, self.cursor.get()) else {
                self.stop(1011, "Replay failed; reconnect from last received cursor");
                return;
            };
            let message = if let Some(event) = &event {
                stringify(event)
            } else {
                let queued = self.transient.borrow_mut().pop_front();
                let Some(message) = queued else {
                    self.wake.notified().await;
                    continue;
                };
                self.transient_bytes
                    .set(self.transient_bytes.get().saturating_sub(message.len()));
                message
            };
            if message.len() + self.socket.buffered_amount() + self.transient_bytes.get()
                > MAXIMUM_BYTES
            {
                self.stop(1013, SLOW_CONSUMER);
                return;
            }
            match tokio::time::timeout(Duration::from_secs(5), self.socket.send(message)).await {
                Err(_) => {
                    self.stop(1013, SLOW_CONSUMER);
                    return;
                }
                Ok(Err(_)) => {
                    self.stop(1011, "Send failed; reconnect from last received cursor");
                    return;
                }
                Ok(Ok(())) => {
                    if let Some(sequence) = event.and_then(|event| event["sequence"].as_i64()) {
                        self.cursor.set(sequence);
                    }
                }
            }
            // `setImmediate(pump)`: other work runs between two sends.
            tokio::task::yield_now().await;
        }
    }

    async fn watch(self: Rc<Self>) {
        match self.socket.closed().await {
            Ok(()) => {
                self.stopped.set(true);
                self.release();
            }
            Err(_) => self.stop(1011, "Socket failed; reconnect from last received cursor"),
        }
    }
}

/// Streams `project_id`'s events after `after` to `socket` until either side stops. Needs a
/// `LocalSet`.
pub fn subscribe<S: EventSocket + 'static, D: EventSource + 'static>(
    socket: Rc<S>,
    source: Rc<D>,
    project_id: &str,
    after: i64,
) {
    let subscription = Rc::new(Subscription {
        socket,
        source: Rc::clone(&source),
        project_id: project_id.to_owned(),
        cursor: Cell::new(after),
        stopped: Cell::new(false),
        transient: RefCell::new(VecDeque::new()),
        transient_bytes: Cell::new(0),
        wake: Notify::new(),
        listener: Cell::new(None),
    });
    let weak: Weak<Subscription<S, D>> = Rc::downgrade(&subscription);
    let id = source.subscribe(Rc::new(move |event: &Value| {
        if let Some(subscription) = weak.upgrade() {
            subscription.deliver(event);
        }
    }));
    subscription.listener.set(Some(id));
    tokio::task::spawn_local(Rc::clone(&subscription).watch());
    tokio::task::spawn_local(subscription.pump());
}
