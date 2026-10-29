//! The terminal session: `runTui` in `apps/tui/src/ui.ts` (with `index.ts` `launchTui`) as a
//! `current_thread` runtime, a `LocalSet` and one event loop.
//!
//! Specified by tui-workspace.md §1.5 (startup sequence, cleanup, handoff write), §1.6 (timers),
//! §7.7 (terminal setup and teardown: alternate screen, raw mode, mouse capture, bracketed paste,
//! keyboard enhancement flags, cursor style and OSC 12 color), §10.4 (the `select!` loop over
//! crossterm's `EventStream`, the poll timer, the 50 ms render coalescer, the spinner, completions
//! and Ctrl+C/Ctrl+Break) and CA-4 (in-process, restores the terminal before returning or unwinding);
//! tui-controller.md §14 (driver integration). Private: only `crate::run` calls it. Track F.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::path::Path;

use crate::config::TuiConfig;

/// The poll delay while `connection === 'connected'`, measured from the end of the previous poll.
pub(crate) const POLL_CONNECTED_MS: u64 = 1000;
/// The poll delay otherwise.
pub(crate) const POLL_DISCONNECTED_MS: u64 = 5000;
/// The trailing render coalescing of controller notifications.
pub(crate) const RENDER_COALESCE_MS: u64 = 50;
/// The terminal resize debounce.
pub(crate) const RESIZE_DEBOUNCE_MS: u64 = 100;
/// The frame cap (OpenTUI `maxFps`).
pub(crate) const MAX_FPS: u64 = 30;
/// `runTui`'s error when stdin or stdout is not a terminal.
pub(crate) const NEEDS_TERMINAL: &str = "NOSH TUI needs an interactive terminal";

/// `runTui(config)`: TTY check, client, `controller.initialize()`, terminal setup, the event loop
/// until quit, then teardown and one handoff write (workspace §1.5).
pub(crate) fn run(config: TuiConfig) -> Result<(), String> {
    todo!()
}

/// `recordProject()`: writes `{"currentProjectId":"<id>"}` (compact, no newline) when both a path
/// and a project exist; write errors are ignored.
pub(crate) fn write_handoff(path: &Path, project_id: &str) {
    todo!()
}
