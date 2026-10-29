//! The client WebSocket seam: the `WebSocketFactory` / `EventSocket` types of `apps/tui/src/stream.ts`
//! as a Rust event stream, and the tokio-tungstenite implementation.
//!
//! Specified by tui-transport.md §6.1, §9.4 and §11.2 (event model, close semantics, handshake
//! errors), §11.4 (tokio-tungstenite 0.30 with `connect` and rustls; subprotocol
//! `auth.<token>` checked by tungstenite) and DECISIONS.md item 9 (ring provider, installed by
//! `super::install_crypto_provider`). The trait is not named `EventSocket`: that is the server-side
//! trait in `nosh_server::events`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use futures_util::future::LocalBoxFuture;
use futures_util::{FutureExt, StreamExt};
use tokio::sync::{Notify, oneshot};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// One event of a client socket, mirroring the JS event handlers one for one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketEvent {
    /// `onopen`: the 101 handshake completed.
    Open,
    /// `onmessage` with a text frame.
    Text(String),
    /// `onmessage` with a binary frame (the stream treats it as an invalid frame).
    Binary,
    /// `onerror`: the handshake failed (including the server destroying the socket) or the
    /// connection broke.
    Error,
    /// `onclose`; every later `next` also yields `Close`.
    Close,
}

/// A client socket as the stream uses it. Methods take `&self` because the stream task and the
/// synchronous cleanup share the socket.
pub trait ClientSocket {
    /// The next event. Pings are answered while this is being polled.
    fn next(&self) -> LocalBoxFuture<'static, SocketEvent>;

    /// `close(code, reason)`: on an open socket sends the close frame from a detached task; while
    /// connecting aborts the handshake without a frame; on a closed socket does nothing.
    fn close(&self, code: u16, reason: &str);
}

/// `WebSocketFactory`: creates a socket for `url` offering `protocols`. `Err` is the JS factory
/// throwing synchronously (the stream counts it as a failed connect).
pub trait SocketFactory {
    fn create(&self, url: &str, protocols: &[String]) -> Result<Rc<dyn ClientSocket>, String>;
}

/// The real factory: `tokio_tungstenite::connect_async` with the request header
/// `Sec-WebSocket-Protocol` set from `protocols`, spawned on the current `LocalSet`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TungsteniteSockets;

impl SocketFactory for TungsteniteSockets {
    fn create(&self, url: &str, protocols: &[String]) -> Result<Rc<dyn ClientSocket>, String> {
        let mut request = url
            .into_client_request()
            .map_err(|error| error.to_string())?;
        let offered =
            HeaderValue::from_str(&protocols.join(", ")).map_err(|error| error.to_string())?;
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, offered);
        let shared = Rc::new(Shared::default());
        let task = tokio::task::spawn_local(drive(request, shared.clone())).abort_handle();
        *shared.task.borrow_mut() = Some(task);
        Ok(Rc::new(TungsteniteSocket { shared }))
    }
}

/// The socket state shared by the handle and its driver task.
#[derive(Default)]
struct Shared {
    events: RefCell<VecDeque<SocketEvent>>,
    notify: Notify,
    /// The last event was queued; `next` yields `Close` once the queue is drained.
    ended: Cell<bool>,
    /// `true` once the handshake completed.
    open: Cell<bool>,
    /// `true` once `close` was called.
    closed: Cell<bool>,
    /// Hands the close frame to the driver of an open socket.
    closing: RefCell<Option<oneshot::Sender<CloseFrame>>>,
    task: RefCell<Option<AbortHandle>>,
}

impl Shared {
    fn push(&self, event: SocketEvent) {
        self.events.borrow_mut().push_back(event);
        self.notify.notify_one();
    }

    fn end(&self, event: SocketEvent) {
        self.push(event);
        self.ended.set(true);
    }
}

/// Connects, then forwards frames until the connection ends or the stream releases the socket.
async fn drive(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    shared: Rc<Shared>,
) {
    let Ok((mut socket, _)) = tokio_tungstenite::connect_async(request).await else {
        shared.end(SocketEvent::Error);
        return;
    };
    let (sender, mut closing) = oneshot::channel();
    *shared.closing.borrow_mut() = Some(sender);
    shared.open.set(true);
    shared.push(SocketEvent::Open);
    loop {
        tokio::select! {
            frame = &mut closing => {
                // Released by the stream: nothing reads this socket any more.
                if let Ok(frame) = frame {
                    let _ = socket.close(Some(frame)).await;
                }
                return;
            }
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => shared.push(SocketEvent::Text(text.as_str().to_owned())),
                Some(Ok(Message::Binary(_))) => shared.push(SocketEvent::Binary),
                Some(Ok(Message::Close(_))) | None => {
                    shared.end(SocketEvent::Close);
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    shared.end(SocketEvent::Error);
                    return;
                }
            },
        }
    }
}

/// A tokio-tungstenite client socket.
struct TungsteniteSocket {
    shared: Rc<Shared>,
}

impl ClientSocket for TungsteniteSocket {
    fn next(&self) -> LocalBoxFuture<'static, SocketEvent> {
        let shared = self.shared.clone();
        async move {
            loop {
                let event = shared.events.borrow_mut().pop_front();
                if let Some(event) = event {
                    return event;
                }
                if shared.ended.get() {
                    return SocketEvent::Close;
                }
                shared.notify.notified().await;
            }
        }
        .boxed_local()
    }

    fn close(&self, code: u16, reason: &str) {
        if self.shared.closed.replace(true) {
            return;
        }
        if self.shared.open.get() {
            let closing = self.shared.closing.borrow_mut().take();
            if let Some(closing) = closing {
                let _ = closing.send(CloseFrame {
                    code: code.into(),
                    reason: reason.into(),
                });
            }
        } else if let Some(task) = self.shared.task.borrow().as_ref() {
            // A JS socket closed while connecting sends no frame.
            task.abort();
        }
    }
}
