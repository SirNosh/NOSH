//! The authenticated daemon client: a port of `class DaemonClient` in `apps/tui/src/client.ts`.
//!
//! Specified by tui-transport.md §3 (server contract), §5.2-§5.8 (state, constructor URL rules,
//! `close`, `subscribe`, `authenticate`, the shared renewal, `request` with one 401 retry), §10.1-
//! §10.2 (tests), §11.2-§11.6 and addenda TA-1 to TA-4, TA-6: reqwest with `no_proxy()` and no
//! redirects, bodies parsed with `crate::js::js_json_parse` into `JsValue`, request bodies written
//! with `crate::js::json_stringify`, `closed` as a `tokio::sync::watch` channel, Node 22 error texts.
//! "Tokens stay in memory. Redirects are forbidden so credentials cannot leak."

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared, ready};
use tokio::sync::watch;
use url::Url;

use super::errors::{
    ABORTED, AUTHENTICATION_FAILED, FETCH_FAILED, INVALID_API_PATH, INVALID_DAEMON_URL,
    INVALID_URL, NO_SESSION_TOKEN, NON_LOOPBACK, REDIRECTS_NOT_ALLOWED, SESSION_EXPIRED, TIMED_OUT,
    TOKEN_REQUIRED, request_error,
};
use super::socket::{SocketFactory, TungsteniteSockets};
use super::stream::{SubscribeOptions, TokenSource, subscribe_events};
use super::{ClientError, DaemonApi, EventCallback, StateCallback, Subscription};
use crate::config::TuiConfig;
use crate::js::{JsObject, JsValue, js_json_parse, json_stringify};
use crate::view_model::record;

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
        *self.0.borrow()
    }

    /// Resolves once the client is closed (or immediately when it already is).
    pub fn aborted(&self) -> LocalBoxFuture<'static, ()> {
        let mut receiver = self.0.clone();
        async move {
            // A dropped client can no longer be closed.
            if receiver.wait_for(|closed| *closed).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        .boxed_local()
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
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ClientError(error.to_string()))?;
        Ok(Self { client })
    }
}

impl HttpFetcher for ReqwestFetcher {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> LocalBoxFuture<'static, Result<HttpResponse, ClientError>> {
        let client = self.client.clone();
        async move {
            if request.signal.is_aborted() {
                return Err(ABORTED.into());
            }
            let method = if request.method == "POST" {
                reqwest::Method::POST
            } else {
                reqwest::Method::GET
            };
            let mut builder = client
                .request(method, &request.url)
                .timeout(Duration::from_millis(request.timeout_ms));
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let exchange = async {
                let response = builder.send().await?;
                let status = response.status().as_u16();
                let body = response.bytes().await?;
                Ok::<_, reqwest::Error>(HttpResponse {
                    status,
                    body: body.to_vec(),
                })
            };
            tokio::select! {
                biased;
                () = request.signal.aborted() => Err(ABORTED.into()),
                result = exchange => result.map_err(|error| {
                    ClientError::from(if error.is_timeout() { TIMED_OUT } else { FETCH_FAILED })
                }),
            }
        }
        .boxed_local()
    }
}

type Renewal = Shared<LocalBoxFuture<'static, Result<(), ClientError>>>;

/// `DaemonClient`: cheap to clone (shared state behind an `Rc`), so `request` futures are
/// `'static` and start the exchange at call time.
#[derive(Clone)]
pub struct DaemonClient {
    inner: Rc<Inner>,
}

/// The shared client state of transport §5.2 / §11.3.
struct Inner {
    base_url: String,
    config: TuiConfig,
    fetcher: Rc<dyn HttpFetcher>,
    sockets: Rc<dyn SocketFactory>,
    /// `token`: `""` is none.
    token: RefCell<String>,
    /// `renewal`: the single in-flight `POST /session`.
    renewal: RefCell<Option<Renewal>>,
    /// `abort`: `true` once `close()` ran.
    closed: watch::Sender<bool>,
    /// `subscriptions`: the stream of each live subscription, in insertion order.
    subscriptions: RefCell<Vec<(u64, Rc<Subscription>)>>,
    next_subscription: Cell<u64>,
}

/// The URL rules shared by the constructor and the stream (transport §5.3 steps 2-3).
pub(super) fn check_daemon_url(url: &Url, invalid: &str) -> Result<(), ClientError> {
    // WHATWG getters return '' for an empty query or fragment, so those pass.
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
        || url.query().is_some_and(|query| !query.is_empty())
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
    {
        return Err(invalid.into());
    }
    if url.scheme() != "https"
        && !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
    {
        return Err(NON_LOOPBACK.into());
    }
    Ok(())
}

/// `response.json()`: UTF-8 with a leading BOM dropped, then `JSON.parse`.
fn parse_body(body: &[u8]) -> Result<JsValue, ClientError> {
    let text = String::from_utf8_lossy(body);
    js_json_parse(text.strip_prefix('\u{feff}').unwrap_or(&text)).map_err(ClientError)
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

impl Inner {
    /// `abort.signal.throwIfAborted()`.
    fn check_open(&self) -> Result<(), ClientError> {
        if *self.closed.borrow() {
            Err(ABORTED.into())
        } else {
            Ok(())
        }
    }

    /// `config.bootstrapToken` when truthy.
    fn bootstrap(&self) -> Option<String> {
        self.config
            .bootstrap_token
            .clone()
            .filter(|token| !token.is_empty())
    }

    /// One exchange through the fetcher; a 3xx is refused, never followed.
    async fn fetch(&self, request: HttpRequest) -> Result<HttpResponse, ClientError> {
        let response = self.fetcher.fetch(request).await?;
        if (300..400).contains(&response.status) {
            return Err(REDIRECTS_NOT_ALLOWED.into());
        }
        Ok(response)
    }

    fn signal(&self) -> AbortSignal {
        AbortSignal::new(self.closed.subscribe())
    }
}

/// `authenticate()` (transport §5.6).
async fn authenticate(inner: Rc<Inner>) -> Result<(), ClientError> {
    inner.check_open()?;
    if !inner.token.borrow().is_empty() {
        return Ok(());
    }
    if inner.bootstrap().is_none() {
        return Err(TOKEN_REQUIRED.into());
    }
    renew(&inner).await
}

/// `renew()`: joins the in-flight `POST /session` or starts one; every caller gets the same outcome
/// (transport §5.7).
fn renew(inner: &Rc<Inner>) -> LocalBoxFuture<'static, Result<(), ClientError>> {
    if let Err(error) = inner.check_open() {
        return ready(Err(error)).boxed_local();
    }
    let Some(bootstrap) = inner.bootstrap() else {
        return ready(Err(SESSION_EXPIRED.into())).boxed_local();
    };
    let current = inner.renewal.borrow().clone();
    let renewal = current.unwrap_or_else(|| {
        let owner = inner.clone();
        let renewal = async move {
            let result = exchange_session(&owner, &bootstrap).await;
            owner.renewal.borrow_mut().take();
            result
        }
        .boxed_local()
        .shared();
        *inner.renewal.borrow_mut() = Some(renewal.clone());
        renewal
    });
    renewal.boxed_local()
}

/// The body of the renewal: no request body and no content-type; the abort is re-checked after
/// parsing and before the token is assigned.
async fn exchange_session(inner: &Inner, bootstrap: &str) -> Result<(), ClientError> {
    let response = inner
        .fetch(HttpRequest {
            method: "POST",
            url: format!("{}/session", inner.base_url),
            headers: vec![("authorization".into(), format!("Bearer {bootstrap}"))],
            body: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            signal: inner.signal(),
        })
        .await?;
    if !ok(response.status) {
        return Err(format!("Authentication failed ({})", response.status).into());
    }
    let data = parse_body(&response.body)?;
    let token = match data.get("token") {
        Some(JsValue::String(token)) if !token.is_empty() => token.clone(),
        _ => return Err(NO_SESSION_TOKEN.into()),
    };
    inner.check_open()?;
    *inner.token.borrow_mut() = token;
    Ok(())
}

/// `request(path, body?, timeoutMs)` (transport §5.8).
async fn request(
    inner: Rc<Inner>,
    path: String,
    body: Option<JsValue>,
    timeout_ms: u64,
) -> Result<JsValue, ClientError> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(INVALID_API_PATH.into());
    }
    authenticate(inner.clone()).await?;
    let encoded = body.as_ref().map(json_stringify);
    for attempt in 0..2 {
        let token = inner.token.borrow().clone();
        let response = inner
            .fetch(HttpRequest {
                method: if encoded.is_some() { "POST" } else { "GET" },
                url: format!("{}{path}", inner.base_url),
                headers: vec![
                    ("authorization".into(), format!("Bearer {token}")),
                    ("content-type".into(), "application/json".into()),
                ],
                body: encoded.clone(),
                timeout_ms,
                signal: inner.signal(),
            })
            .await?;
        // Retry only an explicit authentication rejection, with the exact same body/key.
        if response.status == 401 && attempt == 0 {
            renew(&inner).await?;
            continue;
        }
        if ok(response.status) {
            return parse_body(&response.body);
        }
        // Keep the HTTP status when an error body is empty or not JSON.
        let data = parse_body(&response.body).unwrap_or_else(|_| JsObject::new().into());
        let message = request_error(record(Some(&data)).get("error"));
        return Err(format!("{}: {message}", response.status).into());
    }
    Err(AUTHENTICATION_FAILED.into())
}

impl DaemonClient {
    /// `new DaemonClient(config)` with the real fetcher and socket factory. Fails with
    /// `Invalid URL`, `Invalid daemon URL` or `Non-loopback daemon connections require HTTPS`
    /// (transport §5.3). Requires [`super::install_crypto_provider`] first.
    pub fn new(config: &TuiConfig) -> Result<Self, ClientError> {
        Self::with_transport(
            config,
            Rc::new(ReqwestFetcher::new()?),
            Rc::new(TungsteniteSockets),
        )
    }

    /// `new DaemonClient(config, fetcher, createSocket)`: the same URL rules with injected
    /// transports (tests).
    pub fn with_transport(
        config: &TuiConfig,
        fetcher: Rc<dyn HttpFetcher>,
        sockets: Rc<dyn SocketFactory>,
    ) -> Result<Self, ClientError> {
        let url = Url::parse(&config.base_url).map_err(|_| ClientError::from(INVALID_URL))?;
        check_daemon_url(&url, INVALID_DAEMON_URL)?;
        let mut base_url = url.as_str().to_owned();
        if base_url.ends_with('/') {
            base_url.pop();
        }
        if base_url.ends_with("/api") {
            base_url.truncate(base_url.len() - 4);
        }
        base_url.push_str("/api");
        Ok(Self {
            inner: Rc::new(Inner {
                base_url,
                token: RefCell::new(config.session_token.clone().unwrap_or_default()),
                config: config.clone(),
                fetcher,
                sockets,
                renewal: RefCell::new(None),
                closed: watch::Sender::new(false),
                subscriptions: RefCell::new(Vec::new()),
                next_subscription: Cell::new(0),
            }),
        })
    }

    /// `baseUrl`: the normalized API base, always ending in `/api` (transport §5.3 step 4).
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }
}

/// Removes a subscription from the client's list; the entry is dropped after the borrow ends.
fn forget(inner: &Weak<Inner>, id: u64) -> Option<Rc<Subscription>> {
    let inner = inner.upgrade()?;
    let mut subscriptions = inner.subscriptions.borrow_mut();
    let index = subscriptions.iter().position(|(key, _)| *key == id)?;
    Some(subscriptions.remove(index).1)
}

impl DaemonApi for DaemonClient {
    fn authenticate(&self) -> LocalBoxFuture<'_, Result<(), ClientError>> {
        authenticate(self.inner.clone()).boxed_local()
    }

    fn request(
        &self,
        path: &str,
        body: Option<JsValue>,
        timeout_ms: Option<u64>,
    ) -> LocalBoxFuture<'static, Result<JsValue, ClientError>> {
        let exchange = request(
            self.inner.clone(),
            path.to_owned(),
            body,
            timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        );
        // Started now, as `fetch` is; the caller only awaits the outcome.
        let task = tokio::task::spawn_local(exchange);
        async move {
            match task.await {
                Ok(result) => result,
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(_) => Err(ABORTED.into()),
            }
        }
        .boxed_local()
    }

    fn subscribe(
        &self,
        project_id: &str,
        after: i64,
        on_event: EventCallback,
        on_state: StateCallback,
    ) -> Result<Subscription, ClientError> {
        self.inner.check_open()?;
        let owner = Rc::downgrade(&self.inner);
        let get_token: TokenSource = Rc::new(move |refresh| {
            let owner = owner.clone();
            async move {
                let inner = owner.upgrade().ok_or(ClientError::from(ABORTED))?;
                inner.check_open()?;
                // Native WebSocket hides handshake status. Refresh before every retry when
                // bootstrap credentials are available, not only after an observable 401.
                if refresh && inner.bootstrap().is_some() {
                    renew(&inner).await?;
                } else {
                    authenticate(inner.clone()).await?;
                }
                inner.check_open()?;
                Ok(inner.token.borrow().clone())
            }
            .boxed_local()
        });
        let stream = Rc::new(subscribe_events(SubscribeOptions {
            base_url: self.inner.base_url.clone(),
            project_id: project_id.to_owned(),
            after,
            get_token,
            create_socket: self.inner.sockets.clone(),
            on_event,
            on_state,
        })?);
        let id = self.inner.next_subscription.get();
        self.inner.next_subscription.set(id + 1);
        self.inner
            .subscriptions
            .borrow_mut()
            .push((id, stream.clone()));
        let (cleanup_owner, drop_owner) = (Rc::downgrade(&self.inner), Rc::downgrade(&self.inner));
        Ok(Subscription::new(move || {
            drop(forget(&cleanup_owner, id));
            stream.cleanup();
        })
        .on_drop(move || drop(forget(&drop_owner, id))))
    }

    fn close(&self) {
        if *self.inner.closed.borrow() {
            return;
        }
        self.inner.closed.send_replace(true);
        let subscriptions = std::mem::take(&mut *self.inner.subscriptions.borrow_mut());
        for (_, stream) in subscriptions {
            stream.cleanup();
        }
        self.inner.token.borrow_mut().clear();
    }
}
