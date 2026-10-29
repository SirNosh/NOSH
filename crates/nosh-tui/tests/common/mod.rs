//! Shared integration-test support: the scripted `DaemonApi` mock of tui-controller.md §15.1
//! (calls recorded at invocation as `(path, body, timeout_ms)`, FIFO one-shot responses, persistent
//! rules, controllable pending responses, scripted `subscribe`) and a `settle()` helper that yields
//! until the `LocalSet` is idle.
//!
//! Owned by Track B, which writes it with `tests/controller.rs`; `tests/smoke.rs` (Track F) may
//! reuse it. Test files include it with `mod common;`. Not every test file uses every item, hence
//! the allow below.

#![allow(dead_code)]
