//! The workspace: a port of `createWorkspace` in `apps/tui/src/ui.ts` (state, `submit`,
//! `setComposer`, key/mouse/paste handling, `cycleThinking`, `render`), plus `noshVersion`, `clip`,
//! `basename` and `EMPTY_VIEW`.
//!
//! Specified by tui-workspace.md §6 (6.1-6.15), §7 (OpenTUI defaults that leak into behaviour), §9.1
//! (smoke) and §9.2 (derived tests), with addenda CA-1 (only the exact `/new`, `/open`, `/thinking`
//! open forms; INDEX.md §2 item 6), CA-3 (`nosh_version` is `CARGO_PKG_VERSION`; DECISIONS.md item
//! 8); DECISIONS.md item 4 (click focus, mouse capture, Keypad Enter, lazy `sidebar_rows`);
//! tui-dialogs-visual.md §7 (integration points). Track F owns this file and may refine its API.
//!
//! Async work (`controller.execute`, catalog loads, dialog effects) is spawned with
//! `tokio::task::spawn_local` and reports back as a [`Completion`] on the channel given to
//! [`Workspace::new`]; the event loop (or a test, after yielding) feeds it to
//! [`Workspace::handle_completion`].

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::rc::Rc;

use crossterm::event::Event;
use ratatui::Frame;
use tokio::sync::mpsc::UnboundedSender;

use crate::controller::{Controller, Model, Project};
use crate::dialog::DialogManager;

/// The double-Esc window: a second Esc within it clears the draft.
pub const ESCAPE_WINDOW_MS: f64 = 1500.0;
/// The spinner interval while `busy || activity.working`.
pub const SPINNER_INTERVAL_MS: u64 = 180;
/// The spinner frames, indexed by `floor(now_ms / 180) % 3`.
pub const SPINNER_FRAMES: [&str; 3] = ["▪▫▫", "▫▪▫", "▫▫▪"];
/// The composer history cap.
pub const HISTORY_LIMIT: usize = 100;

/// `noshVersion`: the workspace version, `env!("CARGO_PKG_VERSION")` (CA-3); never empty, so the
/// footer always reads `nosh <version>`.
pub fn nosh_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `clip`: `safeText(value)` with every whitespace run collapsed to one space, cut to
/// `width − 1` units plus `…` when longer than `width` (workspace §6.2 vectors).
pub fn clip(value: &str, width: usize) -> String {
    todo!()
}

/// `basename`: the last `/`- or `\`-separated segment after stripping trailing separators; the
/// original value when that segment is empty (workspace §6.2 vectors).
pub fn basename(value: &str) -> String {
    todo!()
}

/// `EMPTY_VIEW[view]`: the inspector fallback of `status`, `jobs`, `approvals`, `projects`,
/// `models`, `result` and `detail`; `None` for every other view (workspace §6.2, CA-1).
pub fn empty_view(view: &str) -> Option<&'static str> {
    todo!()
}

/// The result of async work the workspace spawned.
#[derive(Debug)]
pub enum Completion {
    /// `submit(value, propagateError)` finished `controller.execute(value)`. `generation` is the
    /// dialog generation for dialog submissions (`propagateError = true`), `None` for the composer.
    Submitted {
        generation: Option<u64>,
        value: String,
        result: Result<(), String>,
    },
    /// A direct `controller.execute` (Esc → `/chat`, Ctrl+G, Ctrl+T): errors are ignored and nothing
    /// enters the history.
    Executed { result: Result<(), String> },
    /// A dialog's [`crate::dialog::DialogEffect::LoadProjects`] finished.
    ProjectsLoaded {
        generation: u64,
        result: Result<Vec<Project>, String>,
    },
    /// A dialog's [`crate::dialog::DialogEffect::LoadModels`] finished.
    ModelsLoaded {
        generation: u64,
        result: Result<Vec<Model>, String>,
    },
    /// `cycleThinking`'s `loadModels()` (only when the catalog was empty) finished.
    ThinkingCatalog { result: Result<Vec<Model>, String> },
}

/// The workspace state of workspace §6.3 and its widgets.
pub struct Workspace {
    controller: Rc<Controller>,
    completions: UnboundedSender<Completion>,
    dialogs: DialogManager,
    now_ms: Box<dyn Fn() -> f64>,
}

impl Workspace {
    /// `createWorkspace(renderer, controller, quit)` with the system clock: focuses the composer.
    pub fn new(controller: Rc<Controller>, completions: UnboundedSender<Completion>) -> Self {
        todo!()
    }

    /// As [`Workspace::new`] with an injected ms-epoch clock (Esc window, spinner).
    pub fn with_clock(
        controller: Rc<Controller>,
        completions: UnboundedSender<Completion>,
        now_ms: Box<dyn Fn() -> f64>,
    ) -> Self {
        todo!()
    }

    /// One terminal event: keys (workspace §6.12, then the dialog or the focused widget), mouse
    /// (§6.13), bracketed paste (§7.8) and resize.
    pub fn handle_event(&mut self, event: &Event) {
        todo!()
    }

    /// Applies the result of spawned work (history, composer clearing, confirmation dialog, dialog
    /// generation rules).
    pub fn handle_completion(&mut self, completion: Completion) {
        todo!()
    }

    /// `render()` (workspace §6.10) and drawing of the whole screen, dialogs included.
    pub fn draw(&mut self, frame: &mut Frame) {
        todo!()
    }

    /// `setComposer(value)`: a programmatic set that does not run the content-change handler
    /// (workspace §6.6).
    pub fn set_composer(&mut self, value: &str) {
        todo!()
    }

    /// Replaces the composer text as a user edit (runs the content-change handler, §6.9): smoke
    /// step 6's "set the composer to `''` directly".
    pub fn edit_composer(&mut self, value: &str) {
        todo!()
    }

    /// The composer text.
    pub fn composer_value(&self) -> String {
        todo!()
    }

    /// The dialog manager.
    pub fn dialogs(&self) -> &DialogManager {
        &self.dialogs
    }

    /// The dialog manager, for opening dialogs directly (smoke step 12).
    pub fn dialogs_mut(&mut self) -> &mut DialogManager {
        &mut self.dialogs
    }

    /// True once `quit()` was requested (Ctrl+C, `/quit`, palette "Quit NOSH").
    pub fn quit_requested(&self) -> bool {
        todo!()
    }

    /// `busy || activity.working`: the event loop runs the 180 ms spinner timer while true.
    pub fn working(&self) -> bool {
        todo!()
    }

    /// Wheel events that reached the conversation scroll area (smoke `checkMapMouse` `leaked`).
    pub fn conversation_wheel_events(&self) -> usize {
        todo!()
    }

    /// `dispose()`: idempotent; destroys the dialogs without notifying.
    pub fn dispose(&mut self) {
        todo!()
    }
}
