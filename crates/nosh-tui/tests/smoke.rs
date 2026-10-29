//! The workspace smoke test, ported one for one from `apps/tui/src/smoke.ts` on a ratatui
//! `TestBackend` of 100x30 (then 60x20).
//!
//! Specified by tui-workspace.md §9.1 (steps 0-19, `checkMapMouse` a-g, the map fixture, the
//! verbatim failure messages; CA-6 and DV-4: this is the only smoke test) and §9.3 (harness:
//! `render::frame_text`, crossterm events fed to `Workspace::handle_event`, "wait 50 ms" = yield to
//! the `LocalSet` and drain `Completion`s), with the fixture and input-helper notes of
//! tui-dialogs-visual.md §8.2. The client is a real `DaemonClient` for `http://127.0.0.1` with
//! session token `test`; no request is made. Owned by Track F.
