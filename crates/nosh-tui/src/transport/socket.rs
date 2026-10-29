//! The client WebSocket seam: the `WebSocketFactory` / `EventSocket` types of `apps/tui/src/stream.ts`
//! as a Rust event stream, and the tokio-tungstenite implementation.
//!
//! Specified by tui-transport.md §6.1, §9.4 and §11.2 (event model, close semantics, handshake
//! errors), §11.4 (tokio-tungstenite 0.30 with `connect` and rustls; subprotocol
//! `auth.<token>` checked by tungstenite) and DECISIONS.md item 9 (ring provider, installed by
//! `super::install_crypto_provider`). The trait is not named `EventSocket`: that is the server-side
//! trait in `nosh_server::events`.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::rc::Rc;

use futures_util::future::LocalBoxFuture;

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
        todo!()
    }
}
