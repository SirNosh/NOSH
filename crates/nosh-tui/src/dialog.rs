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
//!
//! Rows are recomputed on every draw, which is `paintChoices` run per frame; the one repaint quirk
//! that needs state is the failure dialog's error text (§3.12), kept until the next selection move
//! or choice. Choice rows fill the whole inner width when selected (§3.18's recommendation).

use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};

use crate::controller::{Controller, Model, Project, Selection};
use crate::editor::{EditOutcome, Input, PATH_MAX_LENGTH, SEARCH_MAX_LENGTH, TITLE_MAX_LENGTH};
use crate::layout::{round_half_up, wrap_words};
use crate::palette::{
    COMMANDS, CommandItem, CommandMode, DIALOG_TEXT_MAX, PaletteItem, ProjectFormValues,
    action_fingerprint, dialog_text, filter_palette, move_selection, project_form_command,
    validate_project_form,
};
use crate::theme::DIALOG_THEME as THEME;
use crate::view_model::{ActivityState, PendingAction};

const HINT: &str = "↑↓ select   Enter choose   Esc close";
const NO_MATCHES: &str = "No matches. Try another search.";
const CHANGED: &str =
    "The staged action changed. Close this dialog and inspect the current action.";

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

/// What choosing a row does (the TS `Choice.run` closures).
#[derive(Clone, Debug)]
enum Action {
    /// `command(c)`; a slash-menu row clears the composer first.
    Command {
        command: CommandItem,
        slash: bool,
    },
    SelectProject(String),
    ProjectForm(bool),
    Execute(String),
    PickThinking(Model),
    Close,
    /// `check(); return execute(command)` of the confirmation.
    Checked {
        command: &'static str,
        fingerprint: String,
    },
}

#[derive(Clone, Debug)]
struct Choice {
    item: PaletteItem,
    action: Action,
}

impl AsRef<PaletteItem> for Choice {
    fn as_ref(&self) -> &PaletteItem {
        &self.item
    }
}

/// The `select(...)` settings.
#[derive(Default)]
struct Settings<'a> {
    query: &'a str,
    hint: Option<&'a str>,
    empty: Option<&'a str>,
    current: Option<&'a str>,
    searchable: bool,
}

struct SelectState {
    title: String,
    choices: Vec<Choice>,
    /// Positions in `choices`, best match first.
    filtered: Vec<usize>,
    index: i64,
    search: Input,
    hint: String,
    empty: String,
    searchable: bool,
    /// The confirmation's action summary, between the search box and the list.
    summary: Vec<String>,
    /// The failure dialog's error text, shown until the next repaint (§3.12).
    detail_override: Option<String>,
    /// The detail is drawn in the error color for the life of the failure dialog.
    detail_error: bool,
}

struct FormState {
    title: &'static str,
    path: Input,
    working_title: Input,
    create_repository: bool,
    focus: i64,
    submitted: bool,
    error: String,
    error_muted: bool,
}

enum DialogState {
    Select(SelectState),
    Form(FormState),
}

/// What a finished load feeds (`load`'s `ready`).
enum Ready {
    Projects,
    Models,
    Thinking(Selection),
}

/// The panel and its inner content area for the current dialog.
struct Frame {
    panel: Rect,
    inner: Rect,
}

/// `DialogManager`: at most one open dialog, its generation and lock (dialogs-visual §3.2).
pub struct DialogManager {
    controller: Rc<dyn DialogController>,
    generation: u64,
    locked: bool,
    effects: Vec<DialogEffect>,
    state: Option<DialogState>,
    ready: Option<Ready>,
    /// The generation and row of the last mouse-down on a choice row (OpenTUI's drag capture).
    pressed: Option<(u64, i64)>,
    /// The terminal size of the last draw (`renderer.width`/`height`).
    screen: (u16, u16),
}

impl DialogManager {
    /// `createDialogs(renderer, controller, options)`. The workspace passes its `Rc<Controller>`;
    /// tests pass a fake.
    pub fn new(controller: Rc<dyn DialogController>) -> Self {
        DialogManager {
            controller,
            generation: 0,
            locked: false,
            effects: Vec::new(),
            state: None,
            ready: None,
            pressed: None,
            screen: (80, 24),
        }
    }

    /// `isOpen`.
    pub fn is_open(&self) -> bool {
        self.state.is_some()
    }

    /// An action or load is in flight: every key but Esc and Ctrl+C is swallowed.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// The current generation (bumped by every open and close).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// `begin(title)`: a new dialog replaces any open one, unlocked, in a new generation.
    fn begin(&mut self, state: DialogState) {
        self.generation += 1;
        self.locked = false;
        self.ready = None;
        self.state = Some(state);
    }

    fn select(&mut self, title: &str, choices: Vec<Choice>, settings: Settings) {
        let mut search = Input::new(SEARCH_MAX_LENGTH);
        search.set_value(settings.query);
        let filtered = filter_palette(&choices, settings.query);
        let current = filtered
            .iter()
            .position(|&choice| Some(choices[choice].item.id.as_str()) == settings.current);
        let index = if filtered.is_empty() {
            -1
        } else {
            current.unwrap_or(0) as i64
        };
        self.begin(DialogState::Select(SelectState {
            title: title.to_owned(),
            choices,
            filtered,
            index,
            search,
            hint: settings.hint.unwrap_or(HINT).to_owned(),
            empty: settings.empty.unwrap_or(NO_MATCHES).to_owned(),
            searchable: settings.searchable,
            summary: Vec::new(),
            detail_override: None,
            detail_error: false,
        }));
    }

    fn select_state(&mut self) -> Option<&mut SelectState> {
        match &mut self.state {
            Some(DialogState::Select(state)) => Some(state),
            _ => None,
        }
    }

    /// `paintChoices` for the state it changes: the failure text gives way to the description.
    fn repaint(&mut self) {
        if let Some(state) = self.select_state() {
            state.detail_override = None;
        }
    }

    /// `openPalette(query)` (Ctrl+P): `Commands` over all 29 commands.
    pub fn open_palette(&mut self, query: &str) {
        let choices = COMMANDS
            .iter()
            .map(|command| Choice {
                item: command.into(),
                action: Action::Command {
                    command: *command,
                    slash: false,
                },
            })
            .collect();
        let settings = Settings {
            query,
            searchable: true,
            ..Settings::default()
        };
        self.select("Commands", choices, settings);
    }

    /// `openSlash(query)` (typing `/`): `Slash commands`, titles `command  title`; choosing clears
    /// the composer first.
    pub fn open_slash(&mut self, query: &str) {
        let choices = COMMANDS
            .iter()
            .map(|command| Choice {
                item: PaletteItem {
                    title: format!("{}  {}", command.command.trim(), command.title),
                    ..command.into()
                },
                action: Action::Command {
                    command: *command,
                    slash: true,
                },
            })
            .collect();
        let query = query.strip_prefix('/').unwrap_or(query);
        let settings = Settings {
            query,
            hint: Some(HINT),
            searchable: true,
            ..Settings::default()
        };
        self.select("Slash commands", choices, settings);
    }

    /// `load(title, request, ready)`: a locked `Loading…` dialog until the workspace reports back.
    fn load(&mut self, title: &str, ready: Ready) {
        let settings = Settings {
            empty: Some("Loading…"),
            ..Settings::default()
        };
        self.select(title, Vec::new(), settings);
        self.locked = true;
        let generation = self.generation;
        self.effects.push(match ready {
            Ready::Projects => DialogEffect::LoadProjects { generation },
            Ready::Models | Ready::Thinking(_) => DialogEffect::LoadModels { generation },
        });
        self.ready = Some(ready);
    }

    /// `openProjects()`: a `Loading…` dialog and a [`DialogEffect::LoadProjects`].
    pub fn open_projects(&mut self) {
        self.load("Switch project", Ready::Projects);
    }

    /// `openModels()`: a `Loading…` dialog and a [`DialogEffect::LoadModels`].
    pub fn open_models(&mut self) {
        self.load("Select model", Ready::Models);
    }

    /// `openThinking()`: the model picker without a selection, else the levels of the selected
    /// model (after a [`DialogEffect::LoadModels`]).
    pub fn open_thinking(&mut self) {
        match self.controller.selection() {
            None => self.open_models(),
            Some(selection) => self.load("Thinking level", Ready::Thinking(selection)),
        }
    }

    fn pick_thinking(&mut self, model: Model) {
        let choices = model
            .thinking_levels
            .iter()
            .map(|level| Choice {
                item: PaletteItem {
                    id: level.clone(),
                    title: level.clone(),
                    category: model.provider.clone(),
                    description: Some(format!("{}/{} · {level}", model.provider, model.id)),
                    ..PaletteItem::default()
                },
                action: Action::Execute(format!("/model {} {} {level}", model.provider, model.id)),
            })
            .collect();
        let current = self
            .controller
            .selection()
            .map(|selection| selection.thinking_level);
        let name = if model.name.is_empty() {
            &model.id
        } else {
            &model.name
        };
        let settings = Settings {
            current: current.as_deref(),
            empty: Some("This model advertises no supported thinking levels. Esc to close."),
            searchable: true,
            ..Settings::default()
        };
        self.select(&format!("Thinking · {name}"), choices, settings);
    }

    /// `openProjectForm(createRepository)`.
    pub fn open_project_form(&mut self, create_repository: bool) {
        self.begin(DialogState::Form(FormState {
            title: if create_repository {
                "New project"
            } else {
                "Open project"
            },
            path: Input::new(PATH_MAX_LENGTH),
            working_title: Input::new(TITLE_MAX_LENGTH),
            create_repository,
            focus: 0,
            submitted: false,
            error: String::new(),
            error_muted: false,
        }));
    }

    /// `openConfirmation()`: reviews `controller.pending_action()` with a fingerprint check before
    /// `/discard` or `/confirm`.
    pub fn open_confirmation(&mut self) {
        let Some(pending) = self.controller.pending_action() else {
            let settings = Settings {
                empty: Some("No staged action. Inspect state and stage an action first."),
                ..Settings::default()
            };
            return self.select("Review staged action", Vec::new(), settings);
        };
        let fingerprint = action_fingerprint(&pending);
        let choice = |id: &str, title: &str, category: &str, description: &str, action| Choice {
            item: PaletteItem {
                id: id.into(),
                title: title.into(),
                category: category.into(),
                description: Some(description.into()),
                ..PaletteItem::default()
            },
            action,
        };
        let choices = vec![
            choice(
                "keep",
                "Go back — keep staged",
                "Safe default",
                "Close this review. The action stays staged; nothing is applied.",
                Action::Close,
            ),
            choice(
                "discard",
                "Discard staged action",
                "No mutation",
                "Remove this staged action without applying it.",
                Action::Checked {
                    command: "/discard",
                    fingerprint: fingerprint.clone(),
                },
            ),
            choice(
                "apply",
                "Apply this action once",
                "Explicit confirmation",
                "Send the request above exactly once. It is not retried after a network failure.",
                Action::Checked {
                    command: "/confirm",
                    fingerprint,
                },
            ),
        ];
        let title = pending
            .title
            .strip_prefix("Review ")
            .unwrap_or(&pending.title);
        let settings = Settings {
            hint: Some("↑↓ review   Enter choose   Esc keep staged"),
            ..Settings::default()
        };
        self.select(&format!("Confirm · {title}"), choices, settings);
        let (width, height) = self.screen;
        let summary: Vec<String> = std::iter::once(pending.path.clone())
            .chain(
                pending
                    .fields
                    .iter()
                    .map(|field| format!("{}: {}", field.label, field.value)),
            )
            .collect();
        let limit = (i64::from(height) - 17).clamp(1, 8) as usize;
        let budget = (i64::from(width) - 8).clamp(10, 70) as usize;
        let mut lines: Vec<String> = summary
            .iter()
            .take(limit)
            .map(|line| dialog_text(line, budget))
            .collect();
        if summary.len() > limit {
            lines.push("More fields in the staged-action inspector. Esc to review.".into());
        }
        if let Some(state) = self.select_state() {
            state.summary = lines;
        }
    }

    /// `close()`: bumps the generation and queues [`DialogEffect::Closed`] when a dialog was open.
    pub fn close(&mut self) {
        if self.dismiss() {
            self.effects.push(DialogEffect::Closed);
        }
    }

    fn dismiss(&mut self) -> bool {
        if self.state.is_none() {
            return false;
        }
        self.generation += 1;
        self.locked = false;
        self.ready = None;
        self.state = None;
        true
    }

    /// `destroy()`: closes without notifying (the workspace is being disposed).
    pub fn destroy(&mut self) {
        self.dismiss();
    }

    /// `run(action)`: locks, performs the synchronous part, and settles synchronous outcomes at
    /// once (after the action's own opens and closes, as the TS microtask does); an `execute` is
    /// settled later through [`DialogManager::action_finished`].
    fn run(&mut self, action: Action) {
        if self.locked {
            return;
        }
        let generation = self.generation;
        self.locked = true;
        self.repaint();
        match self.perform(action) {
            Ok(Some(command)) => self.effects.push(DialogEffect::Submit {
                generation,
                command,
            }),
            Ok(None) => self.action_finished(generation, Ok(())),
            Err(error) => self.failed(&error, generation),
        }
    }

    fn perform(&mut self, action: Action) -> Result<Option<String>, String> {
        match action {
            Action::Command { command, slash } => {
                if slash {
                    self.effects.push(DialogEffect::SetComposer(String::new()));
                }
                Ok(self.command(&command))
            }
            Action::SelectProject(project_id) => {
                self.controller.select_project(&project_id).map(|()| None)
            }
            Action::ProjectForm(create) => {
                self.open_project_form(create);
                Ok(None)
            }
            Action::Execute(command) => Ok(Some(command)),
            Action::PickThinking(model) => {
                self.pick_thinking(model);
                Ok(None)
            }
            Action::Close => {
                self.close();
                Ok(None)
            }
            Action::Checked {
                command,
                fingerprint,
            } => {
                let current = self.controller.pending_action();
                let unchanged = current.is_some_and(|current| {
                    current.project_id == self.controller.project_id()
                        && action_fingerprint(&current) == fingerprint
                });
                if unchanged {
                    Ok(Some(command.into()))
                } else {
                    Err(CHANGED.into())
                }
            }
        }
    }

    /// `command(c)`: the command to execute, or `None` when it ran synchronously.
    fn command(&mut self, command: &CommandItem) -> Option<String> {
        match command.mode {
            CommandMode::Quit => {
                self.close();
                self.effects.push(DialogEffect::Quit);
            }
            CommandMode::Compose => {
                self.close();
                self.effects
                    .push(DialogEffect::SetComposer(command.command.into()));
            }
            CommandMode::Execute => return Some(command.command.into()),
            CommandMode::Dialog => match command.id {
                "projects" => self.open_projects(),
                "models" => self.open_models(),
                "thinking" => self.open_thinking(),
                "open" => self.open_project_form(false),
                "new" => self.open_project_form(true),
                "confirm" => self.open_confirmation(),
                _ => {}
            },
        }
        None
    }

    /// `failed(error, generation)`: the failure dialog, unless the generation moved.
    fn failed(&mut self, error: &str, generation: u64) {
        if self.generation != generation {
            return;
        }
        let close = Choice {
            item: PaletteItem {
                id: "close".into(),
                title: "Close".into(),
                description: Some(
                    "No automatic retry. Inspect current state before trying again.".into(),
                ),
                ..PaletteItem::default()
            },
            action: Action::Close,
        };
        self.select("Action did not complete", vec![close], Settings::default());
        if let Some(state) = self.select_state() {
            state.detail_override = Some(dialog_text(error, 220));
            state.detail_error = true;
        }
    }

    fn choose(&mut self) {
        let Some(state) = self.select_state() else {
            return;
        };
        let choice = usize::try_from(state.index)
            .ok()
            .and_then(|index| state.filtered.get(index))
            .map(|&choice| state.choices[choice].clone());
        if self.locked {
            return;
        }
        if let Some(choice) = choice.filter(|choice| !choice.item.disabled) {
            self.run(choice.action);
        }
    }

    fn submit_form(&mut self) {
        let locked = self.locked;
        let busy = self.controller.busy();
        let Some(DialogState::Form(state)) = &mut self.state else {
            return;
        };
        if locked || state.submitted {
            return;
        }
        let values = ProjectFormValues {
            path: state.path.value().into(),
            working_title: state.working_title.value().into(),
            create_repository: state.create_repository,
        };
        if let Some(error) = validate_project_form(&values) {
            state.error = error.into();
            return;
        }
        if busy {
            state.error = "Wait for the current command.".into();
            return;
        }
        state.submitted = true;
        state.error_muted = true;
        state.error = "Starting intake… Do not retry after an uncertain result.".into();
        match project_form_command(&values) {
            Ok(command) => self.run(Action::Execute(command)),
            Err(error) => self.failed(&error, self.generation),
        }
    }

    /// `handleKey(key)`: true when the key was consumed; false with no dialog open and for Ctrl+C
    /// (workspace §5.18). Keys the dialog does not consume go to the focused input inside it.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if self.state.is_none() {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT) || key.code == KeyCode::BackTab;
        if control && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return false;
        }
        if key.code == KeyCode::Esc {
            self.close();
            return true;
        }
        if self.locked {
            return true;
        }
        let shortcut = control && matches!(key.code, KeyCode::Char('p' | 'o' | 't'))
            || key.code == KeyCode::F(2);
        match &mut self.state {
            Some(DialogState::Select(state)) => match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Tab
                | KeyCode::BackTab => {
                    let delta = match key.code {
                        KeyCode::Up | KeyCode::BackTab => -1,
                        KeyCode::Tab if shift => -1,
                        KeyCode::PageUp => -8,
                        KeyCode::PageDown => 8,
                        _ => 1,
                    };
                    state.index = move_selection(state.index, delta, state.filtered.len() as i64);
                    self.repaint();
                }
                KeyCode::Enter => self.choose(),
                _ if !state.searchable || shortcut => {}
                _ => {
                    if state.search.handle_key(key) == EditOutcome::Changed {
                        state.filtered = filter_palette(&state.choices, state.search.value());
                        state.index = if state.filtered.is_empty() { -1 } else { 0 };
                    }
                }
            },
            Some(DialogState::Form(state)) => match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    let delta = if key.code == KeyCode::Up || shift {
                        -1
                    } else {
                        1
                    };
                    state.focus = move_selection(state.focus, delta, 5);
                }
                KeyCode::Enter => match state.focus {
                    0 | 1 => state.focus += 1,
                    2 => state.create_repository = !state.create_repository,
                    3 => self.submit_form(),
                    _ => self.close(),
                },
                KeyCode::Char(' ') if state.focus == 2 => {
                    state.create_repository = !state.create_repository
                }
                _ if state.focus >= 2 || shortcut => {}
                _ => {
                    let input = if state.focus == 0 {
                        &mut state.path
                    } else {
                        &mut state.working_title
                    };
                    if input.handle_key(key) == EditOutcome::Changed {
                        state.error.clear();
                    }
                }
            },
            None => {}
        }
        true
    }

    /// Bracketed paste into the focused input; true when a dialog is open.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        let locked = self.locked;
        match &mut self.state {
            Some(DialogState::Select(state)) => {
                state.search.paste(text);
                if state.searchable && !locked {
                    state.filtered = filter_palette(&state.choices, state.search.value());
                    state.index = if state.filtered.is_empty() { -1 } else { 0 };
                }
            }
            Some(DialogState::Form(state)) if state.focus < 2 => {
                let input = if state.focus == 0 {
                    &mut state.path
                } else {
                    &mut state.working_title
                };
                input.paste(text);
                state.error.clear();
            }
            Some(DialogState::Form(_)) => {}
            None => return false,
        }
        true
    }

    /// Mouse on the overlay and choice rows (workspace §5.19): true when a dialog is open (the
    /// overlay swallows every event). `screen` is the full terminal area.
    pub fn handle_mouse(&mut self, event: &MouseEvent, screen: Rect) -> bool {
        if self.state.is_none() {
            return false;
        }
        self.screen = (screen.width, screen.height);
        match event.kind {
            MouseEventKind::Down(_) => {
                self.pressed = self
                    .row_at(event.column, event.row)
                    .map(|row| (self.generation, row));
                if let Some((_, row)) = self.pressed
                    && let Some(state) = self.select_state()
                {
                    state.index = row;
                    state.detail_override = None;
                }
            }
            MouseEventKind::Up(_) => {
                if let Some((generation, _)) = self.pressed.take()
                    && generation == self.generation
                    && !self.locked
                {
                    self.choose();
                }
            }
            _ => {}
        }
        true
    }

    /// The choice index of the row drawn at `(x, y)`, if any.
    fn row_at(&self, x: u16, y: u16) -> Option<i64> {
        let Some(DialogState::Select(state)) = &self.state else {
            return None;
        };
        let frame = self.frame();
        let (start, rows) = self.window(state);
        let top = frame.inner.y + 3 + state.summary.len() as u16;
        let inside =
            x >= frame.inner.x && x < frame.inner.right() && y >= top && y < frame.panel.bottom();
        let offset = i64::from(y.checked_sub(top)?);
        (inside && offset < rows as i64).then_some(start as i64 + offset)
    }

    /// The outcome of a [`DialogEffect::Submit`] (or of a synchronous action): closes on success
    /// when the generation still matches; opens `Action did not complete` on failure.
    pub fn action_finished(&mut self, generation: u64, result: Result<(), String>) {
        match result {
            Ok(()) if self.generation == generation => self.close(),
            Ok(()) => {}
            Err(error) => self.failed(&error, generation),
        }
    }

    /// Whether a load reported for `generation` still feeds the open dialog.
    fn loading(&self, generation: u64) -> bool {
        self.generation == generation && self.state.is_some()
    }

    /// The outcome of a [`DialogEffect::LoadProjects`]; stale generations are discarded.
    pub fn projects_loaded(&mut self, generation: u64, result: Result<Vec<Project>, String>) {
        let projects = match result {
            Err(error) => return self.failed(&error, generation),
            Ok(_) if !self.loading(generation) => return,
            Ok(projects) => projects,
        };
        self.locked = false;
        let current = self.controller.project_id();
        let project = |project: Project| {
            let title = project
                .repository_root
                .split(['\\', '/'])
                .rfind(|segment| !segment.is_empty())
                .unwrap_or(&project.project_id)
                .to_owned();
            Choice {
                item: PaletteItem {
                    title,
                    category: if project.project_id == current {
                        "Current project"
                    } else {
                        "Project"
                    }
                    .into(),
                    description: Some(project.repository_root.clone()),
                    keywords: Some(project.project_id.clone()),
                    id: project.project_id.clone(),
                    ..PaletteItem::default()
                },
                action: Action::SelectProject(project.project_id),
            }
        };
        let form = |id: &str, title: &str, description: &str, create| Choice {
            item: PaletteItem {
                id: id.into(),
                title: title.into(),
                category: "Project".into(),
                description: Some(description.into()),
                ..PaletteItem::default()
            },
            action: Action::ProjectForm(create),
        };
        let mut choices: Vec<Choice> = projects.into_iter().map(project).collect();
        choices.push(form(
            "__open",
            "Open an existing project…",
            "Enter a repository path and working title",
            false,
        ));
        choices.push(form(
            "__new",
            "Create a new project…",
            "Create a repository and start project intake",
            true,
        ));
        let settings = Settings {
            current: Some(&current),
            searchable: true,
            ..Settings::default()
        };
        self.select("Switch project", choices, settings);
    }

    /// The outcome of a [`DialogEffect::LoadModels`]; stale generations are discarded.
    pub fn models_loaded(&mut self, generation: u64, result: Result<Vec<Model>, String>) {
        let models = match result {
            Err(error) => return self.failed(&error, generation),
            Ok(_) if !self.loading(generation) => return,
            Ok(models) => models,
        };
        self.locked = false;
        match self.ready.take() {
            Some(Ready::Thinking(selected)) => {
                let model = models.into_iter().find(|model| {
                    model.provider == selected.model.provider && model.id == selected.model.id
                });
                match model {
                    Some(model) => self.pick_thinking(model),
                    None => self.failed(
                        "Selected model is no longer available. Choose a model again.",
                        generation,
                    ),
                }
            }
            _ => self.show_models(models),
        }
    }

    fn show_models(&mut self, models: Vec<Model>) {
        let hint = if models.is_empty() {
            "No authenticated models · Esc close"
        } else {
            "↑↓ select   Enter thinking levels   Esc close"
        };
        let mut choices = vec![Choice {
            item: PaletteItem {
                id: "__default".into(),
                title: "Use daemon default".into(),
                category: "Model".into(),
                description: Some("Clear the explicit model and thinking selection".into()),
                ..PaletteItem::default()
            },
            action: Action::Execute("/model default".into()),
        }];
        choices.extend(models.into_iter().map(|model| {
            let levels = model.thinking_levels.join(", ");
            let levels = if levels.is_empty() {
                "none advertised"
            } else {
                &levels
            };
            Choice {
                item: PaletteItem {
                    id: format!("{}/{}", model.provider, model.id),
                    title: if model.name.is_empty() {
                        model.id.clone()
                    } else {
                        model.name.clone()
                    },
                    category: model.provider.clone(),
                    description: Some(format!(
                        "{}/{} · thinking: {levels}",
                        model.provider, model.id
                    )),
                    keywords: Some(model.id.clone()),
                    disabled: model.thinking_levels.is_empty(),
                    keybind: None,
                },
                action: Action::PickThinking(model),
            }
        }));
        let running = self.controller.activity().model;
        let current = match self.controller.selection() {
            Some(selection) => format!("{}/{}", selection.model.provider, selection.model.id),
            None if choices.iter().any(|choice| choice.item.id == running) => running,
            None => "__default".into(),
        };
        let settings = Settings {
            current: Some(&current),
            hint: Some(hint),
            searchable: true,
            ..Settings::default()
        };
        self.select("Select model", choices, settings);
    }

    /// Drains the queued effects, in the order they arose.
    pub fn take_effects(&mut self) -> Vec<DialogEffect> {
        std::mem::take(&mut self.effects)
    }

    /// The panel geometry of §3.18 for the open dialog at the last screen size.
    fn frame(&self) -> Frame {
        let (width, height) = (i64::from(self.screen.0), i64::from(self.screen.1));
        let top = ((height - 12).div_euclid(3)).clamp(0, 4);
        let panel_width = (width - 2).clamp(8, 76);
        let content = match &self.state {
            Some(DialogState::Select(state)) => {
                let (_, rows) = self.window(state);
                6 + state.summary.len() as i64 + if rows == 0 { 2 } else { rows as i64 }
            }
            _ => 13,
        };
        let panel_height = (content + 4).min((height - 1).max(4));
        let left = i64::from(round_half_up((width - panel_width) as f64 / 2.0));
        let panel = Rect::new(
            left as u16,
            top as u16,
            panel_width as u16,
            panel_height as u16,
        );
        let inner = Rect::new(
            panel.x + 2,
            panel.y + 2,
            panel.width.saturating_sub(4),
            panel.height.saturating_sub(4),
        );
        Frame { panel, inner }
    }

    /// `paintChoices`' window: the first visible position and the number of rows shown.
    fn window(&self, state: &SelectState) -> (usize, usize) {
        let visible = (i64::from(self.screen.1) - 12).clamp(1, 9);
        let count = state.filtered.len() as i64;
        let start = (state.index - visible / 2).min(count - visible).max(0);
        (start as usize, (count - start).clamp(0, visible) as usize)
    }

    /// Dims the already drawn workspace and draws the open dialog (geometry of workspace §5.3);
    /// returns the cursor cell of the focused input. Does nothing when no dialog is open.
    pub fn draw(&mut self, buf: &mut Buffer, screen: Rect) -> Option<Position> {
        self.state.as_ref()?;
        self.screen = (screen.width, screen.height);
        for cell in &mut buf.content {
            cell.fg = dim(cell.fg);
            cell.bg = dim(cell.bg);
        }
        let frame = self.frame();
        let panel = frame.panel.intersection(*buf.area());
        let style = Style::new().fg(THEME.text).bg(THEME.panel);
        buf.set_style(panel, style);
        ratatui::widgets::Widget::render(
            ratatui::widgets::Block::bordered()
                .border_style(Style::new().fg(THEME.border).bg(THEME.panel)),
            panel,
            buf,
        );
        let inner = frame.inner.intersection(*buf.area());
        let mut rows = Rows {
            buf,
            inner,
            y: inner.y,
        };
        let row_width = (i64::from(screen.width) - 8).clamp(10, 70) as usize;
        let locked = self.locked;
        let window = match &self.state {
            Some(DialogState::Select(state)) => self.window(state),
            _ => (0, 0),
        };
        match &mut self.state {
            Some(DialogState::Select(state)) => {
                rows.text(
                    &dialog_text(&state.title, DIALOG_TEXT_MAX),
                    1,
                    THEME.accent,
                    None,
                );
                let placeholder = if state.searchable {
                    "Type to search…"
                } else {
                    "Use ↑ / ↓ to choose"
                };
                let cursor = rows.input(&mut state.search, placeholder, THEME.panel);
                rows.text(
                    &state.summary.join("\n"),
                    state.summary.len() as u16,
                    THEME.text,
                    None,
                );
                rows.y += 1;
                let (start, count) = window;
                if count == 0 {
                    rows.text(&dialog_text(&state.empty, 200), 2, THEME.muted, None);
                }
                for position in start..start + count {
                    let item = &state.choices[state.filtered[position]].item;
                    let selected = position as i64 == state.index;
                    let label = format!("{} {}", if selected { '›' } else { ' ' }, item.title);
                    let tag = item.keybind.as_deref().unwrap_or(&item.category);
                    let content = if tag.is_empty() {
                        label
                    } else {
                        let gap = row_width
                            .saturating_sub(crate::js::length(&label) + crate::js::length(tag))
                            .max(2);
                        format!("{label}{}{tag}", " ".repeat(gap))
                    };
                    let (fg, bg) = if selected {
                        (THEME.accent, Some(THEME.selected))
                    } else {
                        (THEME.text, None)
                    };
                    rows.text(&dialog_text(&content, row_width), 1, fg, bg);
                }
                let current = usize::try_from(state.index)
                    .ok()
                    .and_then(|index| state.filtered.get(index));
                let description =
                    current.and_then(|&choice| state.choices[choice].item.description.as_deref());
                let detail = match &state.detail_override {
                    Some(text) => text.clone(),
                    None => dialog_text(description.unwrap_or(""), (row_width * 2).max(40)),
                };
                let color = if state.detail_error {
                    THEME.error
                } else {
                    THEME.muted
                };
                rows.text(&detail, 2, color, None);
                let counter = current.map_or(String::new(), |_| {
                    format!("{}/{}", state.index + 1, state.filtered.len())
                });
                let hint = if locked {
                    "Working…  Esc dismiss"
                } else {
                    &state.hint
                };
                rows.text(
                    &dialog_text(&format!("{hint}  {counter}"), row_width),
                    1,
                    THEME.muted,
                    None,
                );
                cursor
            }
            Some(DialogState::Form(state)) => {
                rows.text(state.title, 1, THEME.accent, None);
                rows.text(
                    "Enter a local repository path and working title.\nOpening starts project intake; no contract is approved.",
                    2,
                    THEME.muted,
                    None,
                );
                rows.text("Repository path", 1, THEME.muted, None);
                let path = rows.input(
                    &mut state.path,
                    "C:\\research\\project or /home/me/project",
                    THEME.selected,
                );
                rows.text("Working title", 1, THEME.muted, None);
                let title = rows.input(
                    &mut state.working_title,
                    "What are we investigating?",
                    THEME.selected,
                );
                let marker = |focus: i64| if state.focus == focus { '›' } else { ' ' };
                let checked = if state.create_repository { 'x' } else { ' ' };
                let mode_color = if state.focus == 2 {
                    THEME.accent
                } else {
                    THEME.text
                };
                rows.text(
                    &format!("{} [{checked}] Create a new repository", marker(2)),
                    1,
                    mode_color,
                    None,
                );
                let verb = if state.create_repository {
                    "Create project"
                } else {
                    "Open project"
                };
                let submit_bg = (state.focus == 3).then_some(THEME.selected);
                rows.text(
                    &format!("{} {verb} and start intake", marker(3)),
                    1,
                    THEME.accent,
                    submit_bg,
                );
                let cancel_color = if state.focus == 4 {
                    THEME.accent
                } else {
                    THEME.muted
                };
                rows.text(&format!("{} Cancel", marker(4)), 1, cancel_color, None);
                let error_color = if state.error_muted {
                    THEME.muted
                } else {
                    THEME.error
                };
                rows.text(&state.error, 2, error_color, None);
                rows.text(
                    "Tab / ↑↓ focus   Enter select   Esc cancel",
                    1,
                    THEME.muted,
                    None,
                );
                match state.focus {
                    0 => path,
                    1 => title,
                    _ => None,
                }
            }
            None => None,
        }
    }
}

/// The overlay's `RGBA(0,0,0,160)` over a drawn cell color: `round(c × 95 / 255)` per channel.
fn dim(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let scale = |channel: u8| ((f64::from(channel) * 95.0 / 255.0).round()) as u8;
            Color::Rgb(scale(r), scale(g), scale(b))
        }
        other => other,
    }
}

/// Stacks the panel's text widgets top to bottom inside the inner area, clipping at its bottom.
struct Rows<'a> {
    buf: &'a mut Buffer,
    inner: Rect,
    y: u16,
}

impl Rows<'_> {
    /// A word-wrapped text widget of fixed `height` (only its first rows show); `bg` fills the full
    /// inner width of each row.
    fn text(&mut self, content: &str, height: u16, fg: Color, bg: Option<Color>) {
        let lines = wrap_words(content, self.inner.width);
        for line in lines.iter().take(usize::from(height)) {
            if self.y < self.inner.bottom() {
                let row = Rect::new(self.inner.x, self.y, self.inner.width, 1);
                let style = Style::new().fg(fg).bg(bg.unwrap_or(THEME.panel));
                self.buf.set_style(row, style);
                self.buf
                    .set_stringn(row.x, row.y, line, usize::from(row.width), style);
            }
            self.y += 1;
        }
        self.y += height.saturating_sub(lines.len() as u16);
    }

    fn input(
        &mut self,
        input: &mut Input,
        placeholder: &str,
        background: Color,
    ) -> Option<Position> {
        let y = self.y;
        self.y += 1;
        (y < self.inner.bottom()).then(|| {
            input.render(
                Rect::new(self.inner.x, y, self.inner.width, 1),
                self.buf,
                placeholder,
                background,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Fake {
        project_id: RefCell<String>,
        busy: bool,
        pending: RefCell<Option<PendingAction>>,
    }

    impl DialogController for Fake {
        fn projects(&self) -> Vec<Project> {
            Vec::new()
        }
        fn project_id(&self) -> String {
            self.project_id.borrow().clone()
        }
        fn busy(&self) -> bool {
            self.busy
        }
        fn selection(&self) -> Option<Selection> {
            None
        }
        fn select_project(&self, project_id: &str) -> Result<(), String> {
            if project_id == "missing" {
                return Err("Unknown project. Run /projects first.".into());
            }
            *self.project_id.borrow_mut() = project_id.into();
            Ok(())
        }
        fn pending_action(&self) -> Option<PendingAction> {
            self.pending.borrow().clone()
        }
        fn activity(&self) -> ActivityState {
            ActivityState {
                working: false,
                label: String::new(),
                model: String::new(),
                thinking_level: String::new(),
                agent_id: None,
                role: "nosh".into(),
            }
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn screen(manager: &mut DialogManager, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        manager.draw(&mut buf, area);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// The palette at 100×30 (dialogs-visual §3.18's worked example).
    #[test]
    fn palette_geometry() {
        let mut manager = DialogManager::new(Rc::new(Fake::default()));
        manager.open_palette("");
        let rows = screen(&mut manager, 100, 30);
        let cells = |y: usize, from: usize, to: usize| -> String {
            rows[y].chars().skip(from).take(to - from).collect()
        };
        assert_eq!(cells(4, 12, 13), "┌");
        assert_eq!(cells(6, 14, 22), "Commands");
        assert_eq!(cells(7, 14, 29), "Type to search…");
        assert_eq!(
            cells(9, 14, 84),
            format!("› Switch project{}ctrl+o", " ".repeat(48))
        );
        assert_eq!(
            cells(10, 14, 84),
            format!("  Open project{}Project", " ".repeat(49))
        );
        assert_eq!(cells(18, 14, 44), "Choose a registered repository");
        assert_eq!(
            cells(20, 14, 56),
            "↑↓ select   Enter choose   Esc close  1/29"
        );
        assert_eq!(cells(22, 12, 13), "└");
        assert_eq!(cells(22, 87, 88), "┘");
    }

    /// `run`: a synchronous action closes in its generation; an action that opens a dialog keeps
    /// the new one; a failure opens the failure dialog whose error gives way on the next move.
    #[test]
    fn run_generation_rules() {
        let fake = Rc::new(Fake::default());
        let mut manager = DialogManager::new(fake.clone());
        manager.open_projects();
        let generation = manager.generation();
        assert_eq!(
            manager.take_effects(),
            [DialogEffect::LoadProjects { generation }]
        );
        assert!(manager.is_locked());
        let project = |id: &str| Project {
            project_id: id.into(),
            repository_root: format!("C:\\r\\{id}"),
        };
        manager.projects_loaded(generation, Ok(vec![project("p1"), project("missing")]));
        assert!(!manager.is_locked());
        manager.handle_key(&key(KeyCode::Enter));
        assert!(!manager.is_open());
        assert_eq!(fake.project_id(), "p1");
        assert_eq!(manager.take_effects(), [DialogEffect::Closed]);

        manager.open_projects();
        let generation = manager.generation();
        manager.take_effects();
        manager.projects_loaded(generation, Ok(vec![project("missing")]));
        manager.handle_key(&key(KeyCode::Enter));
        let rows = screen(&mut manager, 100, 30);
        assert!(
            rows.iter()
                .any(|row| row.contains("Action did not complete"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Unknown project. Run /projects first."))
        );
        manager.handle_key(&key(KeyCode::Down));
        let rows = screen(&mut manager, 100, 30);
        assert!(rows.iter().any(|row| row.contains("No automatic retry.")));

        // A stale load is discarded; an execute settles through action_finished.
        manager.open_palette("status");
        manager.projects_loaded(generation, Ok(Vec::new()));
        let rows = screen(&mut manager, 100, 30);
        assert!(rows.iter().any(|row| row.contains("Commands")));
        manager.handle_key(&key(KeyCode::Enter));
        let generation = manager.generation();
        assert_eq!(
            manager.take_effects(),
            [DialogEffect::Submit {
                generation,
                command: "/status".into()
            }]
        );
        assert!(manager.is_locked());
        manager.action_finished(generation, Err("Wait for the current command".into()));
        let rows = screen(&mut manager, 100, 30);
        assert!(
            rows.iter()
                .any(|row| row.contains("Wait for the current command"))
        );
    }

    /// The confirmation refuses to apply an action that changed since it was reviewed.
    #[test]
    fn confirmation_checks_the_fingerprint() {
        let fake = Rc::new(Fake::default());
        *fake.project_id.borrow_mut() = "p".into();
        let action = |path: &str| PendingAction {
            path: path.into(),
            body: crate::js::JsObject::new(),
            project_id: "p".into(),
            title: "Review cancel".into(),
            fields: Vec::new(),
        };
        *fake.pending.borrow_mut() = Some(action("/jobs/j1/cancel"));
        let mut manager = DialogManager::new(fake.clone());
        manager.open_confirmation();
        assert!(
            screen(&mut manager, 100, 30)
                .iter()
                .any(|row| row.contains("Confirm · cancel"))
        );
        *fake.pending.borrow_mut() = Some(action("/jobs/j2/cancel"));
        manager.handle_key(&key(KeyCode::Down));
        manager.handle_key(&key(KeyCode::Down));
        manager.handle_key(&key(KeyCode::Enter));
        assert!(manager.take_effects().is_empty());
        assert!(
            screen(&mut manager, 100, 30)
                .iter()
                .any(|row| row.contains("The staged action changed."))
        );
    }
}
