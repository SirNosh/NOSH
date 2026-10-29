//! Modal dialogs: a port of `apps/tui/src/dialog.ts` (palette, slash menu, project/model/thinking
//! pickers, project form, staged-action confirmation, failure dialog) as a retained state machine
//! drawn into a ratatui buffer.
//!
//! Specified by tui-workspace.md §5 (5.1-5.20) = tui-dialogs-visual.md §3 (3.1-3.29) and §4.1-§4.4,
//! with the runtime design of dialogs-visual §10.3: actions never run inline. Asynchronous work is
//! returned as [`DialogEffect`]s tagged with the generation captured when it started; the workspace
//! runs them and reports back through [`DialogManager::action_finished`],
//! [`DialogManager::projects_loaded`] and [`DialogManager::models_loaded`], which apply the
//! `run`/`load`/`failed` generation rules. Synchronous controller access goes through the
//! [`DialogController`] trait, the port of TS `interface DialogController`. `Controller` implements it
//! here, and tests implement it with a fake, so the dialogs can be tested without a daemon mock.
//!
//! The overlay dim pass (every underlying cell's fg and bg × 95/255, dialogs-visual §3.18 and
//! workspace §5.3) is part of [`DialogManager::draw`], so the dialog owner tests it with the panel.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::rc::Rc;

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use crate::controller::{Controller, Model, Project, Selection};
use crate::view_model::{ActivityState, PendingAction};

/// Work a dialog hands to the workspace (`options.submit`, `controller.loadProjects`,
/// `controller.loadModels`, `options.setComposer`, `options.quit`, `options.onClose`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogEffect {
    /// Run `command` through the workspace's `submit(command, true)`, then call
    /// [`DialogManager::action_finished`] with `generation` and the outcome.
    Submit { generation: u64, command: String },
    /// Run `controller.load_projects()`, then call [`DialogManager::projects_loaded`].
    LoadProjects { generation: u64 },
    /// Run `controller.load_models()`, then call [`DialogManager::models_loaded`].
    LoadModels { generation: u64 },
    /// Set the composer (a compose command, or `""` before a slash choice runs).
    SetComposer(String),
    /// Detach (the palette's "Quit NOSH").
    Quit,
    /// The dialog closed (`onClose`): the workspace refocuses the composer and recomputes
    /// `slashDismissed`.
    Closed,
}

/// `DialogController` (workspace §5.1): the synchronous part of the controller the dialogs use.
/// `execute`, `loadProjects` and `loadModels` are [`DialogEffect`]s instead.
pub trait DialogController {
    /// `controller.projects`.
    fn projects(&self) -> Vec<Project>;
    /// `controller.projectId` (`""` when none).
    fn project_id(&self) -> String;
    /// `controller.busy`.
    fn busy(&self) -> bool;
    /// `controller.selection`.
    fn selection(&self) -> Option<Selection>;
    /// `controller.selectProject(projectId)` (with `preserveModel` false).
    fn select_project(&self, project_id: &str) -> Result<(), String>;
    /// `controller.pendingAction()`.
    fn pending_action(&self) -> Option<PendingAction>;
    /// `controller.activity()` (the model picker reads `activity().model`).
    fn activity(&self) -> ActivityState;
}

impl DialogController for Controller {
    fn projects(&self) -> Vec<Project> {
        self.state().projects.clone()
    }

    fn project_id(&self) -> String {
        self.state().project_id.clone()
    }

    fn busy(&self) -> bool {
        self.state().busy
    }

    fn selection(&self) -> Option<Selection> {
        self.state().selection.clone()
    }

    fn select_project(&self, project_id: &str) -> Result<(), String> {
        Controller::select_project(self, project_id, false)
    }

    fn pending_action(&self) -> Option<PendingAction> {
        Controller::pending_action(self)
    }

    fn activity(&self) -> ActivityState {
        Controller::activity(self)
    }
}

/// `DialogManager`: at most one open dialog, its generation and lock (dialogs-visual §3.2).
pub struct DialogManager {
    controller: Rc<dyn DialogController>,
    generation: u64,
    locked: bool,
    effects: Vec<DialogEffect>,
}

impl DialogManager {
    /// `createDialogs(renderer, controller, options)`. The workspace passes its `Rc<Controller>`;
    /// tests pass a fake.
    pub fn new(controller: Rc<dyn DialogController>) -> Self {
        todo!()
    }

    /// `isOpen`.
    pub fn is_open(&self) -> bool {
        todo!()
    }

    /// An action or load is in flight: every key but Esc and Ctrl+C is swallowed.
    pub fn is_locked(&self) -> bool {
        todo!()
    }

    /// The current generation (bumped by every open and close).
    pub fn generation(&self) -> u64 {
        todo!()
    }

    /// `openPalette(query)` (Ctrl+P): `Commands` over all 29 commands.
    pub fn open_palette(&mut self, query: &str) {
        todo!()
    }

    /// `openSlash(query)` (typing `/`): `Slash commands`, titles `command  title`; choosing clears
    /// the composer first.
    pub fn open_slash(&mut self, query: &str) {
        todo!()
    }

    /// `openProjects()`: a `Loading…` dialog and a [`DialogEffect::LoadProjects`].
    pub fn open_projects(&mut self) {
        todo!()
    }

    /// `openModels()`: a `Loading…` dialog and a [`DialogEffect::LoadModels`].
    pub fn open_models(&mut self) {
        todo!()
    }

    /// `openThinking()`: the model picker without a selection, else the levels of the selected
    /// model (after a [`DialogEffect::LoadModels`]).
    pub fn open_thinking(&mut self) {
        todo!()
    }

    /// `openProjectForm(createRepository)`.
    pub fn open_project_form(&mut self, create_repository: bool) {
        todo!()
    }

    /// `openConfirmation()`: reviews `controller.pending_action()` with a fingerprint check before
    /// `/discard` or `/confirm`.
    pub fn open_confirmation(&mut self) {
        todo!()
    }

    /// `close()`: bumps the generation and queues [`DialogEffect::Closed`] when a dialog was open.
    pub fn close(&mut self) {
        todo!()
    }

    /// `destroy()`: closes without notifying (the workspace is being disposed).
    pub fn destroy(&mut self) {
        todo!()
    }

    /// `handleKey(key)`: true when the key was consumed; false with no dialog open and for Ctrl+C
    /// (workspace §5.18). Keys the dialog does not consume go to the focused input inside it.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        todo!()
    }

    /// Bracketed paste into the focused input; true when a dialog is open.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        todo!()
    }

    /// Mouse on the overlay and choice rows (workspace §5.19): true when a dialog is open (the
    /// overlay swallows every event). `screen` is the full terminal area.
    pub fn handle_mouse(&mut self, event: &MouseEvent, screen: Rect) -> bool {
        todo!()
    }

    /// The outcome of a [`DialogEffect::Submit`] (or of a synchronous action): closes on success
    /// when the generation still matches; opens `Action did not complete` on failure.
    pub fn action_finished(&mut self, generation: u64, result: Result<(), String>) {
        todo!()
    }

    /// The outcome of a [`DialogEffect::LoadProjects`]; stale generations are discarded.
    pub fn projects_loaded(&mut self, generation: u64, result: Result<Vec<Project>, String>) {
        todo!()
    }

    /// The outcome of a [`DialogEffect::LoadModels`]; stale generations are discarded.
    pub fn models_loaded(&mut self, generation: u64, result: Result<Vec<Model>, String>) {
        todo!()
    }

    /// Drains the queued effects, in the order they arose.
    pub fn take_effects(&mut self) -> Vec<DialogEffect> {
        todo!()
    }

    /// Dims the already drawn workspace and draws the open dialog (geometry of workspace §5.3);
    /// returns the cursor cell of the focused input. Does nothing when no dialog is open.
    pub fn draw(&mut self, buf: &mut Buffer, screen: Rect) -> Option<Position> {
        todo!()
    }
}
