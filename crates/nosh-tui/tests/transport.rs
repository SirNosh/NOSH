//! Transport tests, ported one for one from `apps/tui/src/client.test.ts` ("authenticated daemon
//! client", "daemon live subscriptions") and `apps/tui/src/stream.test.ts` ("native live event
//! transport").
//!
//! Specified by tui-transport.md §10.1-§10.3 (the cases), §10.4 (the transport part of smoke.ts) and
//! §11.8 (test mapping: scripted `HttpFetcher`s for `vi.fn()` fetchers, fake `SocketFactory`s for
//! `FakeSocket`, `#[tokio::test(start_paused = true)]` inside a `LocalSet` for timer cases). Tests
//! that build a real `DaemonClient` call `nosh_tui::transport::install_crypto_provider()` first.
//! Owned by Track A.
