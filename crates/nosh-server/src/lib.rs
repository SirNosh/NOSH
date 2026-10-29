//! The daemon's local HTTP and WebSocket API (`apps/noshd/src/server.ts`) and the `noshd` process
//! (`main.ts`).
//!
//! Every connection is served on the daemon's thread (`spawn_local`), so handlers call the
//! `Rc<NoshDaemon>` directly, as TypeScript's handlers run on its one event loop.

mod auth;
pub mod events;
pub mod noshd;
mod routes;
mod socket;

pub use auth::is_loopback_address;
pub use routes::parse_model_selection;

use auth::{automatically_trusted, header};
use base64::Engine;
use bytes::Bytes;
use events::EventSocket;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use nosh_orchestration::daemon::NoshDaemon;
use routes::{Request, is_safe_integer, js_number};
use serde_json::{Value, json};
use socket::WebSocketClient;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;

const SESSION_MILLISECONDS: f64 = 15.0 * 60_000.0;

/// Ends a connection without a response (Node's `socket.destroy()`).
#[derive(Debug)]
struct Destroyed;

impl std::fmt::Display for Destroyed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("connection destroyed")
    }
}

impl std::error::Error for Destroyed {}

type Reply = Result<Response<Full<Bytes>>, Destroyed>;

/// The open connections, so `close` can wait for them.
#[derive(Default)]
struct Connections {
    open: Cell<usize>,
    drained: Notify,
}

impl Connections {
    async fn drained(&self) {
        while self.open.get() > 0 {
            self.drained.notified().await;
        }
    }
}

/// An upgraded connection, counted open until dropped: Node's `server.close()` waits for
/// upgraded sockets too, until their closing handshake completes or `ws` gives up after 30 s.
struct Upgraded(Rc<Connections>);

impl Upgraded {
    fn open(connections: &Rc<Connections>) -> Self {
        connections.open.set(connections.open.get() + 1);
        Self(Rc::clone(connections))
    }
}

impl Drop for Upgraded {
    fn drop(&mut self) {
        self.0.open.set(self.0.open.get() - 1);
        self.0.drained.notify_waiters();
    }
}

struct Listening {
    address: SocketAddr,
    stop: watch::Sender<bool>,
    accept: tokio::task::JoinHandle<()>,
    connections: Rc<Connections>,
}

pub struct LocalApiServer {
    daemon: Rc<NoshDaemon>,
    sessions: RefCell<HashMap<String, f64>>,
    shutdown: Notify,
    listening: RefCell<Option<Listening>>,
    clients: RefCell<Vec<Weak<WebSocketClient>>>,
}

/// `Date.now()`.
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// `JSON.stringify(value)`.
fn stringify(value: &Value) -> String {
    let mut value = value.clone();
    nosh_wire::normalize_numbers(&mut value);
    value.to_string()
}

fn json_response(status: u16, body: &Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(stringify(body))))
        .expect("a valid response")
}

/// `ws`'s `abortHandshake`: a short HTML response, then the connection closes.
fn abort_handshake(status: u16, message: &str, extra: Option<(&str, &str)>) -> Reply {
    let mut response = Response::builder()
        .status(status)
        .header("connection", "close")
        .header("content-type", "text/html");
    if let Some((name, value)) = extra {
        response = response.header(name, value);
    }
    Ok(response
        .body(Full::new(Bytes::from(message.to_owned())))
        .expect("a valid response"))
}

/// `apiPath`: the API also answers under `/api`.
fn api_path(pathname: &str) -> String {
    if pathname == "/api" {
        "/".into()
    } else if let Some(rest) = pathname.strip_prefix("/api/") {
        format!("/{rest}")
    } else {
        pathname.to_owned()
    }
}

/// The token of an `Authorization: Bearer <token>` header (`/^Bearer (.+)$/`).
fn bearer(headers: &hyper::HeaderMap) -> Option<String> {
    header(headers, "authorization")?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// `ws`'s `subprotocol.parse`: comma-separated tokens, optional spaces after commas, no repeats.
fn parse_subprotocols(header: &str) -> Result<Vec<String>, ()> {
    let token_char = |byte: u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte);
    let bytes = header.as_bytes();
    let mut protocols: Vec<String> = Vec::new();
    let (mut start, mut end) = (None::<usize>, None::<usize>);
    let add = |protocol: &str, protocols: &mut Vec<String>| {
        if protocols.iter().any(|seen| seen == protocol) {
            return Err(());
        }
        protocols.push(protocol.to_owned());
        Ok(())
    };
    for (index, &byte) in bytes.iter().enumerate() {
        if end.is_none() && token_char(byte) {
            start.get_or_insert(index);
        } else if index != 0 && (byte == b' ' || byte == b'\t') {
            if end.is_none() && start.is_some() {
                end = Some(index);
            }
        } else if byte == b',' {
            let begin = start.ok_or(())?;
            add(&header[begin..end.unwrap_or(index)], &mut protocols)?;
            (start, end) = (None, None);
        } else {
            return Err(());
        }
    }
    match (start, end) {
        (Some(begin), None) => add(&header[begin..], &mut protocols)?,
        _ => return Err(()),
    }
    Ok(protocols)
}

impl LocalApiServer {
    pub fn new(daemon: Rc<NoshDaemon>) -> Rc<Self> {
        Rc::new(Self {
            daemon,
            sessions: RefCell::new(HashMap::new()),
            shutdown: Notify::new(),
            listening: RefCell::new(None),
            clients: RefCell::new(Vec::new()),
        })
    }

    /// Starts accepting connections on `host:port` (port 0 picks a free one). Needs a `LocalSet`.
    pub async fn listen(self: &Rc<Self>, host: &str, port: u16) -> std::io::Result<()> {
        let listener = TcpListener::bind((host, port)).await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = watch::channel(false);
        let connections = Rc::new(Connections::default());
        let (server, open) = (Rc::clone(self), Rc::clone(&connections));
        let accept = tokio::task::spawn_local(async move {
            let mut until_stopped = stopped.clone();
            loop {
                tokio::select! {
                    _ = until_stopped.wait_for(|stopped| *stopped) => return,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, remote)) => {
                            open.open.set(open.open.get() + 1);
                            let connection = Rc::clone(&server).serve(stream, remote, stopped.clone(), Rc::clone(&open));
                            tokio::task::spawn_local(connection);
                        }
                        // A connection that failed before it was accepted (e.g. reset) affects no other.
                        Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                    },
                }
            }
        });
        *self.listening.borrow_mut() = Some(Listening {
            address,
            stop,
            accept,
            connections,
        });
        Ok(())
    }

    /// Closes every event stream, stops accepting, and waits for open requests to finish.
    pub async fn close(&self) -> Result<(), String> {
        let clients: Vec<_> = self.clients.borrow_mut().drain(..).collect();
        for client in clients.iter().filter_map(Weak::upgrade) {
            client.close_plain();
        }
        let listening = self.listening.borrow_mut().take();
        let Some(listening) = listening else {
            return Err("Server is not running.".into());
        };
        let _ = listening.stop.send(true);
        let _ = listening.accept.await;
        listening.connections.drained().await;
        Ok(())
    }

    /// `http://127.0.0.1:<port>` (IPv6 hosts in brackets).
    pub fn address(&self) -> Result<String, String> {
        let listening = self.listening.borrow();
        let address = listening.as_ref().ok_or("Server is not listening")?.address;
        Ok(match address {
            SocketAddr::V4(v4) => format!("http://{}:{}", v4.ip(), v4.port()),
            SocketAddr::V6(v6) => format!("http://[{}]:{}", v6.ip(), v6.port()),
        })
    }

    /// Resolves once a request asked the daemon to stop (`shutdown-request`).
    pub async fn shutdown_requested(&self) {
        self.shutdown.notified().await;
    }

    async fn serve(
        self: Rc<Self>,
        stream: TcpStream,
        remote: SocketAddr,
        mut stopped: watch::Receiver<bool>,
        connections: Rc<Connections>,
    ) {
        let server = Rc::clone(&self);
        let service = service_fn(move |request| Rc::clone(&server).handle(request, remote));
        let connection = http1::Builder::new()
            .timer(TokioTimer::new())
            // Node's `headersTimeout`.
            .header_read_timeout(Duration::from_secs(60))
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        tokio::pin!(connection);
        tokio::select! {
            _ = connection.as_mut() => {}
            _ = stopped.wait_for(|stopped| *stopped) => {
                connection.as_mut().graceful_shutdown();
                let _ = connection.as_mut().await;
            }
        }
        connections.open.set(connections.open.get() - 1);
        connections.drained.notify_waiters();
    }

    async fn handle(
        self: Rc<Self>,
        request: hyper::Request<Incoming>,
        remote: SocketAddr,
    ) -> Reply {
        let remote = remote.ip().to_string();
        let target = request.uri().to_string();
        let url = url::Url::parse("http://localhost")
            .and_then(|base| base.join(&target))
            .map_err(|_| Destroyed)?;
        let headers = request.headers();
        let upgrade = header(headers, "connection").is_some_and(|connection| {
            connection
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        }) && headers.contains_key("upgrade");
        if upgrade {
            return self.upgrade(request, &remote, &url);
        }
        let path = api_path(url.path());
        let method = request.method().clone();
        if method == Method::GET && path == "/health" {
            return Ok(json_response(200, &json!({ "status": "ok" })));
        }
        if method == Method::POST && path == "/session" {
            if !self.daemon.authenticate(bearer(headers).as_deref()) {
                return Ok(json_response(401, &json!({ "error": "unauthorized" })));
            }
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("the operating system provides randomness");
            let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            let now = now_ms();
            let expires_at = now + SESSION_MILLISECONDS;
            let mut sessions = self.sessions.borrow_mut();
            sessions.retain(|_, expiry| *expiry > now);
            sessions.insert(token.clone(), expires_at);
            return Ok(json_response(
                201,
                &json!({ "token": token, "expiresAt": nosh_wire::iso_from_ms(expires_at) }),
            ));
        }
        if !self.authorized(&remote, headers) {
            return Ok(json_response(401, &json!({ "error": "unauthorized" })));
        }
        let request = Request {
            method,
            path,
            url,
            body: Some(request.into_body()),
        };
        match self.route(request).await {
            Ok(reply) => {
                if reply.shutdown {
                    // `setImmediate(() => emit("shutdown-request"))`, after the response is queued.
                    let server = Rc::clone(&self);
                    tokio::task::spawn_local(async move {
                        tokio::task::yield_now().await;
                        server.shutdown.notify_one();
                    });
                }
                Ok(json_response(reply.status, &reply.body))
            }
            Err(message) => Ok(json_response(400, &json!({ "error": message }))),
        }
    }

    fn authorized(&self, remote: &str, headers: &hyper::HeaderMap) -> bool {
        if automatically_trusted(remote, headers) {
            return true;
        }
        let protocol = || {
            header(headers, "sec-websocket-protocol")?
                .split(',')
                .map(|value| value.trim_matches(nosh_agent::terminal::is_js_whitespace))
                .find(|value| value.starts_with("auth."))
                .map(|value| value["auth.".len()..].to_owned())
        };
        let Some(token) = bearer(headers)
            .or_else(protocol)
            .filter(|token| !token.is_empty())
        else {
            return false;
        };
        let mut sessions = self.sessions.borrow_mut();
        match sessions.get(&token) {
            None => false,
            Some(expiry) if *expiry <= now_ms() => {
                sessions.remove(&token);
                false
            }
            Some(_) => true,
        }
    }

    /// `handleUpgrade`: an authorized `/events` subscription becomes a WebSocket; anything else
    /// loses its connection.
    fn upgrade(
        self: Rc<Self>,
        mut request: hyper::Request<Incoming>,
        remote: &str,
        url: &url::Url,
    ) -> Reply {
        let headers = request.headers();
        if !self.authorized(remote, headers) {
            return Err(Destroyed);
        }
        let query = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let project_id = query("projectId").filter(|id| !id.is_empty());
        let after = js_number(&query("after").unwrap_or_else(|| "0".into()));
        let (Some(project_id), "/events", true) = (
            project_id,
            api_path(url.path()).as_str(),
            is_safe_integer(after) && after >= 0.0,
        ) else {
            return Err(Destroyed);
        };
        // The checks of `ws`'s `WebSocketServer.handleUpgrade`, in its order.
        if request.method() != Method::GET {
            return abort_handshake(405, "Invalid HTTP method", None);
        }
        if !header(headers, "upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        {
            return abort_handshake(400, "Invalid Upgrade header", None);
        }
        static KEY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^[+/0-9A-Za-z]{22}==$").expect("pattern compiles")
        });
        let Some(key) = header(headers, "sec-websocket-key").filter(|key| KEY.is_match(key)) else {
            return abort_handshake(400, "Missing or invalid Sec-WebSocket-Key header", None);
        };
        let version = header(headers, "sec-websocket-version").map_or(f64::NAN, |v| js_number(&v));
        if version != 13.0 && version != 8.0 {
            return abort_handshake(
                400,
                "Missing or invalid Sec-WebSocket-Version header",
                Some(("Sec-WebSocket-Version", "13, 8")),
            );
        }
        let protocols = match header(headers, "sec-websocket-protocol") {
            None => Vec::new(),
            Some(offered) => match parse_subprotocols(&offered) {
                Ok(protocols) => protocols,
                Err(()) => {
                    return abort_handshake(400, "Invalid Sec-WebSocket-Protocol header", None);
                }
            },
        };
        let mut response = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header(
                "sec-websocket-accept",
                tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()),
            );
        // Without a protocol handler, `ws` accepts the first offered subprotocol.
        if let Some(protocol) = protocols.first() {
            response = response.header("sec-websocket-protocol", protocol.as_str());
        }
        let upgraded = hyper::upgrade::on(&mut request);
        #[allow(clippy::cast_possible_truncation)]
        let after = after as i64;
        // Counted before the HTTP connection's own count is released at the upgrade.
        let open = self
            .listening
            .borrow()
            .as_ref()
            .map(|listening| Upgraded::open(&listening.connections));
        tokio::task::spawn_local(async move {
            let Ok(upgraded) = upgraded.await else {
                return;
            };
            let stream =
                WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
            let client = WebSocketClient::start(stream);
            {
                let mut clients = self.clients.borrow_mut();
                clients.retain(|client| client.strong_count() > 0);
                clients.push(Rc::downgrade(&client));
            }
            events::subscribe(
                Rc::clone(&client),
                Rc::clone(&self.daemon),
                &project_id,
                after,
            );
            let _ = client.closed().await;
            drop(open);
        });
        Ok(response
            .body(Full::new(Bytes::new()))
            .expect("a valid response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subprotocols_as_ws_does() {
        assert_eq!(
            parse_subprotocols("auth.abc, nosh").unwrap(),
            ["auth.abc", "nosh"]
        );
        assert_eq!(parse_subprotocols("a,b").unwrap(), ["a", "b"]);
        for bad in ["", " a", "a ", "a,,b", "a,a", "a b", "a;b", ","] {
            assert!(parse_subprotocols(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn strips_the_api_prefix() {
        assert_eq!(api_path("/api"), "/");
        assert_eq!(api_path("/api/projects"), "/projects");
        assert_eq!(api_path("/apix"), "/apix");
        assert_eq!(api_path("/projects"), "/projects");
    }
}
