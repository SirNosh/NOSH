//! Transport errors: `ClientError`, the `requestError` formatter of `client.ts`, and the verbatim
//! error texts of the client and the stream.
//!
//! Specified by tui-transport.md §5.10 (`requestError` and `compact`), §8 (string catalogue) and
//! addenda TA-1, TA-3 (the Node 22 family, DECISIONS.md item 1 and INDEX.md §2 item 7). V8
//! `JSON.parse` texts come from `crate::js::js_json_parse`, which also yields
//! `Unexpected end of JSON input` for an empty 2xx body.

// Remove when implemented: the todo!() body below leaves its parameter unused.
#![allow(unused_variables)]

use std::fmt;

use crate::js::JsValue;

/// A transport failure; the field is the exact JS `error.message` (TA-1). The controller shows it
/// through `safe_text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientError(pub String);

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ClientError {}

impl From<String> for ClientError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for ClientError {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

/// Node 22 `fetch` failure (connection refused, DNS, TLS, reset).
pub const FETCH_FAILED: &str = "fetch failed";
/// Node 22 `AbortSignal.timeout` rejection.
pub const TIMED_OUT: &str = "The operation was aborted due to timeout";
/// Node 22 abort of a closed client (`throwIfAborted`, aborted fetches).
pub const ABORTED: &str = "This operation was aborted";
/// A 3xx answer: `redirect: 'error'` (same text in either runtime family, TA-3).
pub const REDIRECTS_NOT_ALLOWED: &str = "Redirects are not allowed";
/// `new URL(baseUrl)` failure.
pub const INVALID_URL: &str = "Invalid URL";
/// Constructor: a URL with another scheme, credentials, a query or a fragment.
pub const INVALID_DAEMON_URL: &str = "Invalid daemon URL";
/// Constructor and stream: plain HTTP to a host other than `localhost`, `127.0.0.1` or `[::1]`.
pub const NON_LOOPBACK: &str = "Non-loopback daemon connections require HTTPS";
/// `authenticate` with neither token.
pub const TOKEN_REQUIRED: &str = "A bootstrap token or session token is required";
/// `renew` without a bootstrap token (a 401 with a session-only config).
pub const SESSION_EXPIRED: &str = "Session expired. Reopen NOSH with a bootstrap token.";
/// `renew`: a parseable `/session` body that is not an object or has no non-empty `token`.
pub const NO_SESSION_TOKEN: &str = "Daemon returned no session token";
/// `request` with a path that does not start with exactly one `/`.
pub const INVALID_API_PATH: &str = "Invalid API path";
/// The unreachable arm after `request`'s two attempts.
pub const AUTHENTICATION_FAILED: &str = "Authentication failed";
/// `requestError`'s fallback.
pub const DAEMON_REQUEST_FAILED: &str = "Daemon request failed";
/// `subscribeEvents`: empty or longer than 1024 UTF-16 units.
pub const INVALID_STREAM_PROJECT: &str = "Invalid stream project";
/// `subscribeEvents`: `after` negative or not a safe integer.
pub const INVALID_STREAM_CURSOR: &str = "Invalid stream cursor";
/// `subscribeEvents`: the `/events` URL has another scheme, credentials, a query or a fragment.
pub const INVALID_STREAM_URL: &str = "Invalid stream URL";
/// The close reason sent with code 1000 on every client-side close.
pub const STREAM_DETACHED: &str = "Stream detached";
/// The close code sent on every client-side close.
pub const STREAM_DETACHED_CODE: u16 = 1000;

/// `requestError(record(data).error)`: the daemon's error turned into one line. Zod issue lists
/// (as an array, an `{issues}` object, or JSON text of either) show up to three `path: message`
/// items; plain messages are compacted to 320 units; anything else is
/// [`DAEMON_REQUEST_FAILED`] (transport §5.10, branch table included).
pub fn request_error(error: Option<&JsValue>) -> String {
    todo!()
}
