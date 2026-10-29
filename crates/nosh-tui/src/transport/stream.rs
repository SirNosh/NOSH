//! The live event stream: a port of `apps/tui/src/stream.ts` (`subscribeEvents`, `isStreamEvent`).
//!
//! Specified by tui-transport.md §6 (validation order §6.3.1, state §6.3.2, connect §6.3.7, frames
//! §6.3.8, cleanup §6.3.9, state sequences §6.3.10), §10.3 (tests) and §11.3 (one `spawn_local` task
//! per subscription, generation checks after every await, `tokio::time::Instant` for the 5 s
//! stability rule so paused-clock tests see it). Frames parse with
//! `serde_json::from_str::<JsValue>` (TA-2); parse errors are never shown.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::rc::Rc;

use futures_util::future::LocalBoxFuture;

use super::socket::SocketFactory;
use super::{ClientError, EventCallback, StateCallback, Subscription};
use crate::js::JsValue;

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

/// `subscribeEvents`: validates synchronously (§6.3.1, throwing in that order), then spawns the
/// connect/retry task on the current `LocalSet` and returns its cleanup handle. The returned
/// [`Subscription`] emits the final `Disconnected` from `cleanup` and stops silently on drop.
pub fn subscribe_events(options: SubscribeOptions) -> Result<Subscription, ClientError> {
    todo!()
}

/// `isStreamEvent`: the envelope check applied to every parsed frame (transport §6.5).
pub fn is_stream_event(value: &JsValue) -> bool {
    todo!()
}
