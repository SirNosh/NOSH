//! The authenticated daemon client and the live event stream: a port of `apps/tui/src/client.ts`
//! and `apps/tui/src/stream.ts`.
//!
//! Specified by tui-transport.md §2-§11 with addenda TA-1 (one `DaemonApi` seam), TA-2 (bodies and
//! frames parse into `JsValue`), TA-3 (Node 22 error texts, DECISIONS.md item 1), TA-4 (no proxies),
//! TA-5 (module layout) and TA-6 (no `tokio-util`, no `async-trait`); INDEX.md §2 items 3, 5 and 7;
//! DECISIONS.md item 9 (rustls with the ring provider for both reqwest and tokio-tungstenite).
//!
//! Concurrency (transport §11.3): everything runs on one `current_thread` runtime inside a
//! `LocalSet`. Callbacks may re-enter (`on_state` may call [`Subscription::cleanup`] or
//! [`DaemonApi::close`]), so no `RefCell` borrow is ever held across a callback.

use std::cell::RefCell;

use futures_util::future::LocalBoxFuture;

use crate::js::JsValue;

pub mod client;
pub mod errors;
pub mod socket;
pub mod stream;

pub use client::DaemonClient;
pub use errors::ClientError;
pub use stream::StreamState;

/// The stream's event callback: receives the whole parsed frame object. `Err(())` means "the
/// consumer threw": the event is not acknowledged and the connection fails (transport §11.3).
pub type EventCallback = Box<dyn FnMut(&JsValue) -> Result<(), ()>>;

/// The stream's state callback (`connecting`, `connected`, `disconnected`). It cannot fail.
pub type StateCallback = Box<dyn FnMut(StreamState)>;

/// The one seam between the controller and the daemon (TA-1, controller CC-1). [`DaemonClient`] is
/// the real implementation; controller tests implement it with a scripted mock.
pub trait DaemonApi {
    /// `authenticate`: obtains a session token through `POST /api/session` unless one is held
    /// (transport §5.6).
    fn authenticate(&self) -> LocalBoxFuture<'_, Result<(), ClientError>>;

    /// `request(path, body?, timeoutMs)`: `GET` without a body, `POST` of `json_stringify(body)`
    /// with one; resolves with the parsed JSON (transport §5.8). `timeout_ms: None` is the client
    /// default (15000); the controller passes `Some(120000)` only for chat and `/projects/open`.
    /// The exchange is issued (and a mock records the call) when `request` is called, not on first
    /// poll, so call order is invocation order.
    fn request(
        &self,
        path: &str,
        body: Option<JsValue>,
        timeout_ms: Option<u64>,
    ) -> LocalBoxFuture<'static, Result<JsValue, ClientError>>;

    /// `subscribe(projectId, after, onEvent, onState)`: opens the live stream for a project after
    /// the HTTP bootstrap (transport §5.5, §6.3). Validation errors and a closed client fail
    /// synchronously; nothing connects until the caller yields to the `LocalSet`.
    fn subscribe(
        &self,
        project_id: &str,
        after: i64,
        on_event: EventCallback,
        on_state: StateCallback,
    ) -> Result<Subscription, ClientError>;

    /// `close`: aborts in-flight requests, cleans up every live subscription and forgets the token.
    /// Idempotent (transport §5.4).
    fn close(&self);
}

/// The cleanup handle of a live subscription: the port of the function `subscribe` returns
/// (transport §11.2, TA-1). The controller keeps an `Option<Subscription>` where TS keeps
/// `streamClose`.
///
/// [`Subscription::cleanup`] is idempotent and runs the cleanup closure once; that closure emits the
/// final `on_state(Disconnected)`. `Drop` never invokes callbacks: it only runs the silent `stop`
/// given to [`Subscription::on_drop`] (stop the task, close the socket) if `cleanup` was never
/// called, because a drop can happen while the owner holds a `RefCell` borrow.
pub struct Subscription {
    cleanup: RefCell<Option<Box<dyn FnOnce()>>>,
    stop: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Subscription {
    /// A subscription whose explicit cleanup runs `cleanup` once.
    pub fn new(cleanup: impl FnOnce() + 'static) -> Self {
        Self {
            cleanup: RefCell::new(Some(Box::new(cleanup))),
            stop: RefCell::new(None),
        }
    }

    /// Adds the silent stop that `Drop` runs when [`Subscription::cleanup`] was never called. It must
    /// not call `on_event` or `on_state`.
    pub fn on_drop(self, stop: impl FnOnce() + 'static) -> Self {
        *self.stop.borrow_mut() = Some(Box::new(stop));
        self
    }

    /// Runs the cleanup closure the first time; later calls, including re-entrant ones from inside
    /// the closure, do nothing. The silent stop is discarded.
    pub fn cleanup(&self) {
        let cleanup = self.cleanup.borrow_mut().take();
        self.stop.borrow_mut().take();
        if let Some(cleanup) = cleanup {
            cleanup();
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.get_mut().take() {
            stop();
        }
    }
}

/// Installs rustls' ring provider as the process default (DECISIONS.md item 9). Idempotent: when a
/// provider is already installed it is kept. [`crate::run`] calls it before building a client, and so
/// must every test that builds a real [`DaemonClient`]: when nosh-agent is in the build, rustls has
/// both the ring and aws-lc-rs features, and tokio-tungstenite's `ClientConfig::builder()` panics
/// without an installed default.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn cleanup_runs_once_and_drop_stays_silent() {
        let cleaned = Rc::new(Cell::new(0));
        let stopped = Rc::new(Cell::new(0));
        let (c, s) = (cleaned.clone(), stopped.clone());
        let subscription =
            Subscription::new(move || c.set(c.get() + 1)).on_drop(move || s.set(s.get() + 1));
        subscription.cleanup();
        subscription.cleanup();
        drop(subscription);
        assert_eq!((cleaned.get(), stopped.get()), (1, 0));

        let (c, s) = (cleaned.clone(), stopped.clone());
        drop(Subscription::new(move || c.set(c.get() + 1)).on_drop(move || s.set(s.get() + 1)));
        assert_eq!((cleaned.get(), stopped.get()), (1, 1));
    }

    #[test]
    fn cleanup_may_reenter() {
        let slot: Rc<RefCell<Option<Rc<Subscription>>>> = Rc::new(RefCell::new(None));
        let count = Rc::new(Cell::new(0));
        let (inner, c) = (slot.clone(), count.clone());
        let subscription = Rc::new(Subscription::new(move || {
            c.set(c.get() + 1);
            let again = inner.borrow().clone();
            if let Some(again) = again {
                again.cleanup();
            }
        }));
        *slot.borrow_mut() = Some(subscription.clone());
        subscription.cleanup();
        slot.borrow_mut().take();
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn installs_the_ring_provider() {
        install_crypto_provider();
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
