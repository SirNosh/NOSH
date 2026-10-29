//! Shared integration-test support: the scripted `DaemonApi` mock of tui-controller.md §15.1
//! (calls recorded at invocation as `(path, body, timeout_ms)`, FIFO one-shot responses, persistent
//! rules, controllable pending responses, scripted `subscribe`) and a `settle()` helper that yields
//! until the `LocalSet` is idle.
//!
//! Test files include it with `mod common;`. Not every test file uses every item, hence the allow
//! below.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, ready};
use nosh_tui::js::{JsObject, JsValue, js_json_parse};
use nosh_tui::transport::{
    ClientError, DaemonApi, EventCallback, StateCallback, StreamState, Subscription,
};
use tokio::sync::oneshot;

/// A JS value from JSON text.
pub fn json(text: &str) -> JsValue {
    js_json_parse(text).expect("fixture JSON")
}

/// An object literal from `(key, value)` pairs, in order.
pub fn obj(entries: Vec<(&str, JsValue)>) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in entries {
        object.set(key, value);
    }
    object.into()
}

/// `{...value, ...overrides}` for an object `value`.
pub fn with(value: &JsValue, overrides: Vec<(&str, JsValue)>) -> JsValue {
    let mut value = value.clone();
    if let JsValue::Object(object) = &mut value {
        for (key, item) in overrides {
            object.set(key, item);
        }
    }
    value
}

/// Runs a test body on a `current_thread` runtime inside a `LocalSet`, as the TUI does.
pub fn local<F: Future>(body: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    tokio::task::LocalSet::new().block_on(&runtime, body)
}

/// `await new Promise(resolve => setTimeout(resolve, 0))`: lets every detached task run until it
/// waits on something unresolved.
pub async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

type Reply = LocalBoxFuture<'static, Result<JsValue, String>>;
type Rule = Rc<dyn Fn(&str) -> Reply>;

/// Settles a pending response (`resolve` / `reject` of a hand-made promise).
pub struct Resolver(oneshot::Sender<Result<JsValue, String>>);

impl Resolver {
    pub fn resolve(self, value: JsValue) {
        let _ = self.0.send(Ok(value));
    }

    /// `reject(new Error(message))`.
    pub fn reject(self, message: &str) {
        let _ = self.0.send(Err(message.to_owned()));
    }
}

fn pending() -> (
    Resolver,
    impl Future<Output = Result<JsValue, String>> + 'static,
) {
    let (sender, receiver) = oneshot::channel();
    let reply = receiver.map(|settled| settled.unwrap_or_else(|_| Err("never settled".into())));
    (Resolver(sender), reply)
}

/// One `request` call as it was invoked.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub path: String,
    pub body: Option<JsValue>,
    pub timeout: Option<u64>,
}

/// The `vi.spyOn(client, 'request').mockResolvedValue({})` of the TS tests, plus scripted
/// `authenticate` and `subscribe`. One-shot replies are used first, then the persistent rule, then
/// `{}`.
#[derive(Default)]
pub struct MockApi {
    calls: RefCell<Vec<Call>>,
    once: RefCell<VecDeque<Reply>>,
    rule: RefCell<Option<Rule>>,
    subscriptions: RefCell<Vec<(String, i64)>>,
    on_event: RefCell<Option<EventCallback>>,
    on_state: RefCell<Option<StateCallback>>,
    closes: Rc<Cell<usize>>,
}

impl MockApi {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// `mockResolvedValueOnce(value)`.
    pub fn once(&self, value: JsValue) -> &Self {
        self.once
            .borrow_mut()
            .push_back(ready(Ok(value)).boxed_local());
        self
    }

    /// `mockRejectedValueOnce(new Error(message))`.
    pub fn once_error(&self, message: &str) -> &Self {
        let error = Err(message.to_owned());
        self.once.borrow_mut().push_back(ready(error).boxed_local());
        self
    }

    /// `mockReturnValueOnce(new Promise(...))`.
    pub fn once_pending(&self) -> Resolver {
        let (resolver, reply) = pending();
        self.once.borrow_mut().push_back(reply.boxed_local());
        resolver
    }

    /// `mockImplementation(async path => ...)`.
    pub fn implement(&self, reply: impl Fn(&str) -> JsValue + 'static) {
        *self.rule.borrow_mut() = Some(Rc::new(move |path| ready(Ok(reply(path))).boxed_local()));
    }

    /// `mockReturnValue(promise)`: every later call shares one pending response.
    pub fn always_pending(&self) -> Resolver {
        let (resolver, reply) = pending();
        let shared = reply.shared();
        *self.rule.borrow_mut() = Some(Rc::new(move |_| shared.clone().boxed_local()));
        resolver
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    pub fn last(&self) -> Call {
        self.calls.borrow().last().cloned().expect("a request")
    }

    /// `toHaveBeenCalledWith(path)`: a call with no body and the default timeout.
    pub fn called(&self, path: &str) -> bool {
        self.calls
            .borrow()
            .iter()
            .any(|call| call.path == path && call.body.is_none() && call.timeout.is_none())
    }

    pub fn subscriptions(&self) -> Vec<(String, i64)> {
        self.subscriptions.borrow().clone()
    }

    pub fn closes(&self) -> usize {
        self.closes.get()
    }

    /// Calls the stored `onEvent`.
    pub fn emit(&self, event: &JsValue) {
        let callback = self.on_event.borrow_mut().take();
        let mut callback = callback.expect("a subscription");
        let _ = callback(event);
        *self.on_event.borrow_mut() = Some(callback);
    }

    /// Calls the stored `onState`.
    pub fn emit_state(&self, state: StreamState) {
        let callback = self.on_state.borrow_mut().take();
        let mut callback = callback.expect("a subscription");
        callback(state);
        *self.on_state.borrow_mut() = Some(callback);
    }
}

impl DaemonApi for MockApi {
    fn authenticate(&self) -> LocalBoxFuture<'_, Result<(), ClientError>> {
        ready(Ok(())).boxed_local()
    }

    fn request(
        &self,
        path: &str,
        body: Option<JsValue>,
        timeout_ms: Option<u64>,
    ) -> LocalBoxFuture<'static, Result<JsValue, ClientError>> {
        self.calls.borrow_mut().push(Call {
            path: path.to_owned(),
            body,
            timeout: timeout_ms,
        });
        let once = self.once.borrow_mut().pop_front();
        let reply = once
            .or_else(|| self.rule.borrow().as_ref().map(|rule| rule(path)))
            .unwrap_or_else(|| ready(Ok(JsObject::new().into())).boxed_local());
        reply.map(|reply| reply.map_err(ClientError)).boxed_local()
    }

    fn subscribe(
        &self,
        project_id: &str,
        after: i64,
        on_event: EventCallback,
        on_state: StateCallback,
    ) -> Result<Subscription, ClientError> {
        self.subscriptions
            .borrow_mut()
            .push((project_id.to_owned(), after));
        *self.on_event.borrow_mut() = Some(on_event);
        *self.on_state.borrow_mut() = Some(on_state);
        let closes = self.closes.clone();
        Ok(Subscription::new(move || closes.set(closes.get() + 1)))
    }

    fn close(&self) {}
}
