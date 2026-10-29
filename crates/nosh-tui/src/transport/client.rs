//! The authenticated daemon client: a port of `class DaemonClient` in `apps/tui/src/client.ts`.
//!
//! Specified by tui-transport.md §3 (server contract), §5.2-§5.8 (state, constructor URL rules,
//! `close`, `subscribe`, `authenticate`, the shared renewal, `request` with one 401 retry), §10.1-
//! §10.2 (tests), §11.2-§11.6 and addenda TA-1 to TA-4, TA-6: reqwest with `no_proxy()` and no
//! redirects, bodies parsed with `crate::js::js_json_parse` into `JsValue`, request bodies written
//! with `crate::js::json_stringify`, `closed` as a `tokio::sync::watch` channel, Node 22 error texts.
//! "Tokens stay in memory. Redirects are forbidden so credentials cannot leak."

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::rc::Rc;

use futures_util::future::LocalBoxFuture;
use tokio::sync::watch;

use super::socket::SocketFactory;
use super::{ClientError, DaemonApi, EventCallback, StateCallback, Subscription};
use crate::config::TuiConfig;
use crate::js::JsValue;

/// `request`'s default `timeoutMs`, also the `/session` timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// The client's abort signal as one HTTP exchange sees it: set by `close()`. The per-attempt timeout
/// is [`HttpRequest::timeout_ms`], applied by the fetcher.
#[derive(Clone, Debug)]
pub struct AbortSignal(watch::Receiver<bool>);

impl AbortSignal {
    /// Wraps the receiving end of the client's `closed` channel.
    pub fn new(receiver: watch::Receiver<bool>) -> Self {
        Self(receiver)
    }

    /// `signal.aborted`.
    pub fn is_aborted(&self) -> bool {
        todo!()
    }

    /// Resolves once the client is closed (or immediately when it already is).
    pub fn aborted(&self) -> LocalBoxFuture<'static, ()> {
        todo!()
    }
}

/// One HTTP exchange as the client issues it (`fetch(url, init)` in TS). `method` is `GET` or
/// `POST`; headers are lowercase names in the TS order.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: u64,
    pub signal: AbortSignal,
}

/// The status and the complete body of a response. A fetcher never follows redirects: a 3xx comes
/// back as is and the client fails it with `Redirects are not allowed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The injectable HTTP transport (`fetcher` in TS), so tests can script responses. Failures carry
/// the Node texts (`fetch failed`, `The operation was aborted due to timeout`,
/// `This operation was aborted`).
pub trait HttpFetcher {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> LocalBoxFuture<'static, Result<HttpResponse, ClientError>>;
}

/// The real fetcher: one reqwest client built with `no_proxy()` and `redirect::Policy::none()`,
/// rustls with the ring provider, and a per-request timeout covering connect through body end.
#[derive(Clone, Debug)]
pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    /// Builds the reqwest client. Requires [`super::install_crypto_provider`] first.
    pub fn new() -> Result<Self, ClientError> {
        todo!()
    }
}

impl HttpFetcher for ReqwestFetcher {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> LocalBoxFuture<'static, Result<HttpResponse, ClientError>> {
        todo!()
    }
}

/// `DaemonClient`: cheap to clone (shared state behind an `Rc`), so `request` futures are
/// `'static` and start the exchange at call time.
#[derive(Clone)]
pub struct DaemonClient {
    inner: Rc<Inner>,
}

/// The shared client state of transport §5.2 / §11.3 (token, single in-flight renewal, `closed`
/// channel, live subscriptions). Private: Track A shapes it.
struct Inner {
    base_url: String,
    config: TuiConfig,
    fetcher: Rc<dyn HttpFetcher>,
    sockets: Rc<dyn SocketFactory>,
}

impl DaemonClient {
    /// `new DaemonClient(config)` with the real fetcher and socket factory. Fails with
    /// `Invalid URL`, `Invalid daemon URL` or `Non-loopback daemon connections require HTTPS`
    /// (transport §5.3). Requires [`super::install_crypto_provider`] first.
    pub fn new(config: &TuiConfig) -> Result<Self, ClientError> {
        todo!()
    }

    /// `new DaemonClient(config, fetcher, createSocket)`: the same URL rules with injected
    /// transports (tests).
    pub fn with_transport(
        config: &TuiConfig,
        fetcher: Rc<dyn HttpFetcher>,
        sockets: Rc<dyn SocketFactory>,
    ) -> Result<Self, ClientError> {
        todo!()
    }

    /// `baseUrl`: the normalized API base, always ending in `/api` (transport §5.3 step 4).
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }
}

impl DaemonApi for DaemonClient {
    fn authenticate(&self) -> LocalBoxFuture<'_, Result<(), ClientError>> {
        todo!()
    }

    fn request(
        &self,
        path: &str,
        body: Option<JsValue>,
        timeout_ms: Option<u64>,
    ) -> LocalBoxFuture<'static, Result<JsValue, ClientError>> {
        todo!()
    }

    fn subscribe(
        &self,
        project_id: &str,
        after: i64,
        on_event: EventCallback,
        on_state: StateCallback,
    ) -> Result<Subscription, ClientError> {
        todo!()
    }

    fn close(&self) {
        todo!()
    }
}
