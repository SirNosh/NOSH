//! The workspace: a port of `createWorkspace` in `apps/tui/src/ui.ts` (state, `submit`,
//! `setComposer`, key/mouse/paste handling, `cycleThinking`, `render`), plus `noshVersion`, `clip`,
//! `basename` and `EMPTY_VIEW`.
//!
//! Specified by tui-workspace.md §6 (6.1-6.15), §7 (OpenTUI defaults that leak into behaviour), §9.1
//! (smoke) and §9.2 (derived tests), with addenda CA-1 (only the exact `/new`, `/open`, `/thinking`
//! open forms; INDEX.md §2 item 6), CA-3 (`nosh_version` is `CARGO_PKG_VERSION`; DECISIONS.md item
//! 8); DECISIONS.md item 4 (click focus, mouse capture, Keypad Enter, lazy `sidebar_rows`);
//! tui-dialogs-visual.md §7 (integration points).
//!
//! Async work (`controller.execute`, catalog loads, dialog effects) is spawned with
//! `tokio::task::spawn_local` and reports back as a [`Completion`] on the channel given to
//! [`Workspace::new`]; the event loop (or a test, after yielding) feeds it to
//! [`Workspace::handle_completion`]. Every entry point ends by running the dialog effects it queued,
//! so the TS callbacks (`onClose`, `submit`, `setComposer`, `quit`) run before the next frame.
//!
//! Focus follows OpenTUI (§7.3, DECISIONS.md item 4): a left click on the conversation or the sidebar
//! list focuses that scroll area, which then scrolls with Up/Down/`k`/`j` and drops other keys and
//! pastes; a click on the composer, or any dialog closing, focuses the composer again. The map canvas
//! keeps the focus where it is.

use std::rc::Rc;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use tokio::sync::mpsc::UnboundedSender;

use crate::controller::{Controller, HELP, Model, ModelRef, Project, Selection};
use crate::dialog::{DialogEffect, DialogManager};
use crate::editor::{EditOutcome, Editor};
use crate::js::{self, date_now, js_trim};
use crate::layout::{
    self, LayoutInput, cell_width, composer_max_height, footer_layout, model_line_layout,
    sidebar_visible, sidebar_width, wrap_ranges, wrap_words,
};
use crate::map_graph::GraphDirection;
use crate::palette::slash_query;
use crate::render::{fill, scrollbar, split_bar};
use crate::theme::{THEME, compact_logo, key_hints, logo};
use crate::view_model::{CLIMB_MAXIMUM, EntryKind, climb_lines, safe_text_str};
use crate::visual::{AttentionItem, Inspector, ResearchMap, ResearchSidebar, Transcript};

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
    let clean = js::js_collapse_ws(&safe_text_str(value));
    if js::length(&clean) > width {
        format!("{}…", js::slice_to(&clean, width.saturating_sub(1)))
    } else {
        clean
    }
}

/// `basename`: the last `/`- or `\`-separated segment after stripping trailing separators; the
/// original value when that segment is empty (workspace §6.2 vectors).
pub fn basename(value: &str) -> String {
    match value
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
    {
        Some(segment) if !segment.is_empty() => segment.to_owned(),
        _ => value.to_owned(),
    }
}

/// `EMPTY_VIEW[view]`: the inspector fallback of `status`, `jobs`, `approvals`, `projects`,
/// `models`, `result` and `detail`; `None` for every other view (workspace §6.2, CA-1).
pub fn empty_view(view: &str) -> Option<&'static str> {
    Some(match view {
        "status" => {
            "No missions, directions, autoresearch or agents yet.\nKeep talking in the conversation to shape the project; research work is proposed for your approval."
        }
        "jobs" => {
            "No supervised jobs for this project.\nJobs started by research work appear here with their resources and output."
        }
        "approvals" => "Nothing is waiting for approval.",
        "projects" => "No registered projects. Press ctrl+o or use /new to open one.",
        "models" => {
            "No authenticated models. Configure provider authentication in Pi, then /refresh."
        }
        "result" => "The action was applied. The daemon returned no records.",
        "detail" => "The daemon returned no details for this record.",
        _ => return None,
    })
}

/// Who called `submit`: the composer (`propagateError = false`) with the draft it had, or a dialog
/// (`propagateError = true`) with its generation.
#[derive(Debug)]
pub enum Origin {
    Composer { draft: String },
    Dialog(u64),
}

/// The result of async work the workspace spawned.
#[derive(Debug)]
pub enum Completion {
    /// `submit(value, propagateError)` finished `controller.execute(value)`.
    Submitted {
        origin: Origin,
        value: String,
        result: Result<(), String>,
    },
    /// A direct `controller.execute` (Esc → `/chat`, Ctrl+G, Ctrl+T): errors are ignored and nothing
    /// enters the history.
    Executed { result: Result<(), String> },
    /// A dialog's [`DialogEffect::LoadProjects`] finished.
    ProjectsLoaded {
        generation: u64,
        result: Result<Vec<Project>, String>,
    },
    /// A dialog's [`DialogEffect::LoadModels`] finished.
    ModelsLoaded {
        generation: u64,
        result: Result<Vec<Model>, String>,
    },
    /// `cycleThinking`'s `loadModels()` (only when the catalog was empty) finished; `selected` is
    /// the model it captured before loading.
    ThinkingCatalog {
        selected: Selection,
        result: Result<Vec<Model>, String>,
    },
}

/// The focusable elements outside dialogs (§7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Composer,
    Conversation,
    Sidebar,
}

/// A vertical scroll area (§7.4): the offset clamped to the content, and for the conversation in
/// `chat` the sticky bottom with its "manual" suspension.
#[derive(Debug, Default)]
struct Scroll {
    offset: usize,
    content: usize,
    viewport: usize,
    sticky: bool,
    manual: bool,
}

impl Scroll {
    fn max(&self) -> usize {
        self.content.saturating_sub(self.viewport)
    }

    fn scroll_to(&mut self, offset: usize) {
        self.offset = offset.min(self.max());
        if self.sticky {
            if self.offset >= self.max() {
                self.manual = false;
            } else if self.max() > 1 {
                self.manual = true;
            }
        }
    }

    fn scroll_by(&mut self, rows: i64) {
        self.scroll_to(self.offset.saturating_add_signed(rows as isize));
    }

    /// A new frame's content and viewport heights: follows the bottom unless the user scrolled
    /// away; a content change within one row of the bottom resumes following.
    fn layout(&mut self, content: usize, viewport: usize) {
        if self.sticky && content != self.content && self.offset + 1 >= self.max() {
            self.manual = false;
        }
        (self.content, self.viewport) = (content, viewport);
        self.offset = if self.sticky && !self.manual {
            self.max()
        } else {
            self.offset.min(self.max())
        };
    }
}

/// Where the last frame drew the mouse targets.
#[derive(Debug, Default)]
struct Targets {
    screen: Rect,
    conversation: Rect,
    /// The visible part of the map canvas, and the screen cell of canvas cell `(0, 0)`.
    canvas: Rect,
    canvas_origin: (i64, i64),
    sidebar_list: Rect,
    composer: Rect,
    input: Rect,
    model: Rect,
    /// Empty while the thinking label is.
    thinking: Rect,
    cwd: Rect,
}

/// The controller fields one render reads, copied so no borrow is held across a controller call.
struct Snapshot {
    view: String,
    project_id: String,
    /// The selected project's repository root; `None` when no registered project is selected.
    project: Option<String>,
    error: String,
    busy: bool,
    selection: Option<Selection>,
    no_model: bool,
    connection: String,
    detail: String,
}

/// The workspace state of workspace §6.3 and its widgets.
pub struct Workspace {
    controller: Rc<Controller>,
    completions: UnboundedSender<Completion>,
    dialogs: DialogManager,
    now_ms: Box<dyn Fn() -> f64>,
    composer: Editor,
    focus: Focus,
    disposed: bool,
    quit: bool,
    slash_dismissed: bool,
    previous_view: String,
    previous_project: String,
    show_sidebar: bool,
    escape_armed_at: f64,
    history: Vec<String>,
    history_index: usize,
    history_draft: String,
    transcript: Transcript,
    inspector: Inspector,
    map: ResearchMap,
    sidebar: ResearchSidebar,
    conversation: Scroll,
    side: Scroll,
    wheel_events: usize,
    targets: Targets,
}

impl Workspace {
    /// `createWorkspace(renderer, controller, quit)` with the system clock: focuses the composer.
    pub fn new(controller: Rc<Controller>, completions: UnboundedSender<Completion>) -> Self {
        Self::with_clock(controller, completions, Box::new(date_now))
    }

    /// As [`Workspace::new`] with an injected ms-epoch clock (Esc window, spinner).
    pub fn with_clock(
        controller: Rc<Controller>,
        completions: UnboundedSender<Completion>,
        now_ms: Box<dyn Fn() -> f64>,
    ) -> Self {
        Workspace {
            dialogs: DialogManager::new(controller.clone()),
            controller,
            completions,
            now_ms,
            composer: Editor::new(),
            focus: Focus::Composer,
            disposed: false,
            quit: false,
            slash_dismissed: false,
            previous_view: String::new(),
            previous_project: String::new(),
            show_sidebar: true,
            escape_armed_at: 0.0,
            history: Vec::new(),
            history_index: 0,
            history_draft: String::new(),
            transcript: Transcript::new(),
            inspector: Inspector::new(),
            map: ResearchMap::new(),
            sidebar: ResearchSidebar::new(),
            conversation: Scroll {
                sticky: true,
                ..Scroll::default()
            },
            side: Scroll::default(),
            wheel_events: 0,
            targets: Targets::default(),
        }
    }

    /// One terminal event: keys (workspace §6.12, then the dialog or the focused widget), mouse
    /// (§6.13) and bracketed paste (§7.8). Key releases are ignored; repeats count as presses.
    pub fn handle_event(&mut self, event: &Event) {
        if self.disposed {
            return;
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => self.handle_paste(text),
            _ => {}
        }
        self.run_effects();
    }

    /// Applies the result of spawned work (history, composer clearing, confirmation dialog, dialog
    /// generation rules).
    pub fn handle_completion(&mut self, completion: Completion) {
        if self.disposed {
            return;
        }
        match completion {
            Completion::Submitted {
                origin,
                value,
                result,
            } => {
                if result.is_ok() {
                    // Dialog submissions, and drafts edited meanwhile, are not erased.
                    if let Origin::Composer { draft } = &origin
                        && self.composer.value() == *draft
                    {
                        self.set_composer("");
                    }
                    if self.history.last() != Some(&value) {
                        self.history.push(value);
                    }
                    if self.history.len() > HISTORY_LIMIT {
                        self.history.remove(0);
                    }
                    self.history_index = self.history.len();
                    // Opening it bumps the generation, so the originating dialog stays open.
                    if self.controller.pending_action().is_some() {
                        self.dialogs.open_confirmation();
                    }
                }
                // A composer failure keeps the draft; `controller.error` is shown in the status.
                if let Origin::Dialog(generation) = origin {
                    self.dialogs.action_finished(generation, result);
                }
            }
            Completion::Executed { .. } => {}
            Completion::ProjectsLoaded { generation, result } => {
                self.dialogs.projects_loaded(generation, result)
            }
            Completion::ModelsLoaded { generation, result } => {
                self.dialogs.models_loaded(generation, result)
            }
            Completion::ThinkingCatalog { selected, result } => {
                if let Ok(models) = result {
                    self.cycle_with(&selected, &models);
                }
            }
        }
        self.run_effects();
    }

    /// `render()` (workspace §6.10) and drawing of the whole screen, dialogs included.
    pub fn draw(&mut self, frame: &mut Frame) {
        if self.disposed {
            return;
        }
        let area = frame.area();
        if let Some(cursor) = self.render(area, frame.buffer_mut()) {
            frame.set_cursor_position(cursor);
        }
    }

    /// `setComposer(value)`: a programmatic set that does not run the content-change handler
    /// (workspace §6.6); a recalled or programmatic `/command` must not reopen slash search.
    pub fn set_composer(&mut self, value: &str) {
        self.composer.set_value(value);
        self.slash_dismissed = value.starts_with('/');
    }

    /// Replaces the composer text as a user edit (runs the content-change handler, §6.9): smoke
    /// step 6's "set the composer to `''` directly".
    pub fn edit_composer(&mut self, value: &str) {
        self.composer.set_value(value);
        self.content_changed();
        self.run_effects();
    }

    /// The composer text.
    pub fn composer_value(&self) -> String {
        self.composer.value()
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
        self.quit
    }

    /// `busy || activity.working`: the event loop runs the 180 ms spinner timer while true.
    pub fn working(&self) -> bool {
        let busy = self.controller.state().busy;
        busy || self.controller.activity().working
    }

    /// Wheel events that reached the conversation scroll area (smoke `checkMapMouse` `leaked`).
    pub fn conversation_wheel_events(&self) -> usize {
        self.wheel_events
    }

    /// `dispose()`: idempotent; destroys the dialogs without notifying.
    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        self.dialogs.destroy();
        self.dialogs.take_effects();
    }

    /// Runs `work` on the `LocalSet` and reports its result on the completion channel.
    fn spawn(&self, work: impl Future<Output = Completion> + 'static) {
        let completions = self.completions.clone();
        tokio::task::spawn_local(async move {
            // The receiver is gone only after the event loop ended.
            let _ = completions.send(work.await);
        });
    }

    /// `controller.execute(command)` whose errors only show in the status line.
    fn execute(&self, command: &str) {
        let (controller, command) = (self.controller.clone(), command.to_owned());
        self.spawn(async move {
            Completion::Executed {
                result: controller.execute(&command).await,
            }
        });
    }

    /// Runs the queued dialog effects (the TS dialog callbacks) until none is left.
    fn run_effects(&mut self) {
        loop {
            let effects = self.dialogs.take_effects();
            if effects.is_empty() || self.disposed {
                return;
            }
            for effect in effects {
                match effect {
                    DialogEffect::Submit {
                        generation,
                        command,
                    } => self.submit(&command, Some(generation)),
                    DialogEffect::LoadProjects { generation } => {
                        let controller = self.controller.clone();
                        self.spawn(async move {
                            Completion::ProjectsLoaded {
                                generation,
                                result: controller.load_projects().await,
                            }
                        });
                    }
                    DialogEffect::LoadModels { generation } => {
                        let controller = self.controller.clone();
                        self.spawn(async move {
                            Completion::ModelsLoaded {
                                generation,
                                result: controller.load_models().await,
                            }
                        });
                    }
                    DialogEffect::SetComposer(value) => self.set_composer(&value),
                    DialogEffect::Quit => self.quit = true,
                    DialogEffect::Closed => {
                        self.slash_dismissed = self.composer.value().starts_with('/');
                        self.focus = Focus::Composer;
                    }
                }
            }
        }
    }

    /// `submit(value, propagateError)` (workspace §6.7); `generation` is set for dialog submissions,
    /// whose outcome goes back through `action_finished`.
    fn submit(&mut self, value: &str, generation: Option<u64>) {
        let finish = |dialogs: &mut DialogManager, result| {
            if let Some(generation) = generation {
                dialogs.action_finished(generation, result);
            }
        };
        let command = js_trim(value);
        if command.is_empty() {
            return finish(&mut self.dialogs, Ok(()));
        }
        if self.controller.state().busy {
            // Enter is ignored while a command runs; a dialog shows the error.
            return finish(
                &mut self.dialogs,
                Err("Wait for the current command".into()),
            );
        }
        if command == "/quit" {
            self.quit = true;
            return finish(&mut self.dialogs, Ok(()));
        }
        if generation.is_none() {
            // CA-1: only these exact texts open a form. TS looked them up in a plain object, so
            // `toString` and the other Object.prototype names cleared the draft and sent nothing.
            let create = match command {
                "/new" => Some(true),
                "/open" => Some(false),
                _ => None,
            };
            if let Some(create) = create {
                self.set_composer("");
                return self.dialogs.open_project_form(create);
            }
            if command == "/thinking" {
                self.set_composer("");
                return self.dialogs.open_thinking();
            }
            if !command.starts_with('/') && self.controller.state().project_id.is_empty() {
                return self.dialogs.open_projects();
            }
        }
        let origin = match generation {
            Some(generation) => Origin::Dialog(generation),
            None => Origin::Composer {
                draft: self.composer.value(),
            },
        };
        // The untrimmed value: the controller trims, and the history keeps what was typed.
        let (controller, value) = (self.controller.clone(), value.to_owned());
        self.spawn(async move {
            let result = controller.execute(&value).await;
            Completion::Submitted {
                origin,
                value,
                result,
            }
        });
    }

    /// The composer's content-change handler (§6.9): clears the error and opens slash search.
    fn content_changed(&mut self) {
        if self.dialogs.is_open() || self.disposed {
            return;
        }
        let value = self.composer.value();
        if value.is_empty() {
            self.slash_dismissed = false;
        }
        self.controller.state_mut().error.clear();
        if !self.slash_dismissed
            && let Some(query) = slash_query(&value)
        {
            self.dialogs.open_slash(query);
        }
    }

    /// `handleKey` (workspace §6.12), then the focused widget.
    fn handle_key(&mut self, key: &KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let ctrl = |letter: char| {
            control && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&letter))
        };
        if ctrl('c') {
            self.quit = true;
            return;
        }
        if self.dialogs.handle_key(key) {
            return;
        }
        let (view, busy) = {
            let state = self.controller.state();
            (state.view.clone(), state.busy)
        };
        if ctrl('p') {
            return self.dialogs.open_palette("");
        }
        if ctrl('o') {
            return self.dialogs.open_projects();
        }
        // Ctrl+M is Enter (CR) in most terminals, so the model picker uses F2.
        if key.code == KeyCode::F(2) {
            return self.dialogs.open_models();
        }
        if ctrl('t') {
            return self.cycle_thinking();
        }
        if ctrl('b') {
            self.show_sidebar = !self.show_sidebar;
            return;
        }
        if ctrl('g') {
            if !busy {
                self.execute(if view == "map" { "/chat" } else { "/map" });
            }
            return;
        }
        if key.code == KeyCode::Esc {
            return self.escape(&view, busy);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // The map takes navigation keys while the composer is empty, so a typed command still works.
        if view == "map" && self.composer.value().is_empty() {
            let direction = match key.code {
                KeyCode::Up => Some(GraphDirection::Up),
                KeyCode::Down => Some(GraphDirection::Down),
                KeyCode::Left => Some(GraphDirection::Left),
                KeyCode::Right => Some(GraphDirection::Right),
                _ => None,
            };
            if let Some(direction) = direction {
                return self.map.move_selection(direction);
            }
            match key.code {
                KeyCode::Tab => return self.map.cycle(if shift { -1 } else { 1 }),
                KeyCode::BackTab => return self.map.cycle(-1),
                KeyCode::Backspace if self.map.back() => return,
                // Keypad Enter is not `return` in TS: it falls through to the composer's newline.
                KeyCode::Enter
                    if !shift
                        && !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.state.contains(KeyEventState::KEYPAD) =>
                {
                    let command = self.map.open();
                    if !command.is_empty() {
                        self.set_composer(&command);
                    }
                    return;
                }
                KeyCode::PageUp => return self.map.pan(-10),
                KeyCode::PageDown => return self.map.pan(10),
                _ => {}
            }
        }
        let page = self.conversation.viewport.saturating_sub(3).max(3) as i64;
        match key.code {
            KeyCode::PageUp => return self.conversation.scroll_by(-page),
            KeyCode::PageDown => return self.conversation.scroll_by(page),
            KeyCode::Home if control => return self.conversation.scroll_to(0),
            KeyCode::End if control => return self.conversation.scroll_to(usize::MAX),
            KeyCode::Up if self.composer.logical_cursor_row() == 0 && !self.history.is_empty() => {
                if self.history_index == self.history.len() {
                    self.history_draft = self.composer.value();
                }
                self.history_index = self.history_index.saturating_sub(1);
                let entry = self.history[self.history_index].clone();
                return self.set_composer(&entry);
            }
            KeyCode::Down
                if self.history_index < self.history.len()
                    && self.composer.logical_cursor_row() + 1
                        >= self.composer.logical_line_count() =>
            {
                self.history_index += 1;
                let entry = self
                    .history
                    .get(self.history_index)
                    .unwrap_or(&self.history_draft)
                    .clone();
                return self.set_composer(&entry);
            }
            _ => {}
        }
        match self.focus {
            Focus::Composer => match self.composer.handle_key(key) {
                EditOutcome::Submit if !self.dialogs.is_open() => {
                    let value = self.composer.value();
                    self.submit(&value, None);
                }
                EditOutcome::Changed => self.content_changed(),
                _ => {}
            },
            // A focused scroll area scrolls by a fifth of its height and drops other keys.
            Focus::Conversation | Focus::Sidebar => {
                let scroll = if self.focus == Focus::Sidebar {
                    &mut self.side
                } else {
                    &mut self.conversation
                };
                let step = (scroll.viewport as f64 / 5.0).round().max(1.0) as i64;
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => scroll.scroll_by(-step),
                    KeyCode::Down | KeyCode::Char('j') => scroll.scroll_by(step),
                    _ => {}
                }
            }
        }
    }

    /// Esc outside dialogs: back to the conversation, or the double-Esc draft clear (§6.12 row 9).
    /// Navigation, not a typed command: it stays out of the history.
    fn escape(&mut self, view: &str, busy: bool) {
        if view != "chat" {
            if !busy {
                self.execute("/chat");
            }
            return;
        }
        let draft = !self.composer.value().is_empty();
        let now = (self.now_ms)();
        // A single Esc never discards a draft; a second one within 1.5 s clears it.
        if draft && now - self.escape_armed_at < ESCAPE_WINDOW_MS {
            self.set_composer("");
            self.escape_armed_at = 0.0;
        } else {
            if draft {
                self.escape_armed_at = now;
            }
            self.controller.state_mut().error.clear();
        }
    }

    /// OpenCode-style Ctrl+T: advance to the selected model's next advertised level (§6.11).
    fn cycle_thinking(&mut self) {
        let activity = self.controller.activity();
        let (selection, busy, models) = {
            let state = self.controller.state();
            (state.selection.clone(), state.busy, state.models.clone())
        };
        // Without an explicit selection, cycle the model the active agent is running.
        let selected = selection.or_else(|| {
            let slash = activity.model.find('/').filter(|&slash| slash > 0)?;
            Some(Selection {
                model: ModelRef {
                    provider: activity.model[..slash].to_owned(),
                    id: activity.model[slash + 1..].to_owned(),
                },
                thinking_level: activity.thinking_level.clone(),
            })
        });
        let Some(selected) = selected else {
            return self.dialogs.open_thinking();
        };
        if busy {
            return;
        }
        if models.is_empty() {
            let controller = self.controller.clone();
            self.spawn(async move {
                Completion::ThinkingCatalog {
                    result: controller.load_models().await,
                    selected,
                }
            });
        } else {
            self.cycle_with(&selected, &models);
        }
    }

    /// `cycleThinking` steps 5-7 with the catalog in hand.
    fn cycle_with(&mut self, selected: &Selection, models: &[Model]) {
        let model = models.iter().find(|model| {
            model.provider == selected.model.provider && model.id == selected.model.id
        });
        let Some(model) = model.filter(|model| !model.thinking_levels.is_empty()) else {
            return self.dialogs.open_models();
        };
        let levels = &model.thinking_levels;
        // An unknown current level gives the first one, as `indexOf` −1 + 1 does.
        let next = levels
            .iter()
            .position(|level| *level == selected.thinking_level)
            .map_or(0, |index| (index + 1) % levels.len());
        self.execute(&format!(
            "/model {} {} {}",
            model.provider, model.id, levels[next]
        ));
    }

    /// Mouse targets (§6.13): the map canvas captures its events, labels open dialogs, a left
    /// click moves the focus, the wheel scrolls the area under the pointer.
    fn handle_mouse(&mut self, event: &MouseEvent) {
        if self.dialogs.handle_mouse(event, self.targets.screen) {
            return;
        }
        let at = Position::new(event.column, event.row);
        let targets = &self.targets;
        let map = self.controller.state().view == "map";
        // The canvas stops propagation: the conversation neither scrolls nor takes focus.
        if map && (self.map.dragging() || targets.canvas.contains(at)) {
            let (x, y) = targets.canvas_origin;
            self.map.handle_mouse(
                event.kind,
                i64::from(event.column) - x,
                i64::from(event.row) - y,
            );
            return;
        }
        match event.kind {
            MouseEventKind::Down(button) => {
                if targets.model.contains(at) {
                    return self.dialogs.open_models();
                }
                if targets.thinking.contains(at) {
                    return self.dialogs.open_thinking();
                }
                if targets.cwd.contains(at) {
                    return self.dialogs.open_projects();
                }
                if button != MouseButton::Left {
                    return;
                }
                if targets.composer.contains(at) {
                    self.focus = Focus::Composer;
                } else if targets.conversation.contains(at) {
                    self.focus = Focus::Conversation;
                } else if targets.sidebar_list.contains(at) {
                    self.focus = Focus::Sidebar;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let rows = if event.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                };
                if targets.conversation.contains(at) {
                    self.wheel_events += 1;
                    self.conversation.scroll_by(rows);
                } else if targets.sidebar_list.contains(at) {
                    self.side.scroll_by(rows);
                } else if targets.input.contains(at) {
                    self.composer.scroll(rows as i32);
                }
            }
            _ => {}
        }
    }

    /// Bracketed paste goes only to the focused widget (§7.8).
    fn handle_paste(&mut self, text: &str) {
        if self.dialogs.handle_paste(text) {
            return;
        }
        if self.focus == Focus::Composer && self.composer.paste(text) == EditOutcome::Changed {
            self.content_changed();
        }
    }

    fn snapshot(&self) -> Snapshot {
        let state = self.controller.state();
        Snapshot {
            view: state.view.clone(),
            project_id: state.project_id.clone(),
            project: state
                .projects
                .iter()
                .find(|project| project.project_id == state.project_id)
                .map(|project| project.repository_root.clone()),
            error: state.error.clone(),
            busy: state.busy,
            selection: state.selection.clone(),
            no_model: state.models_checked && state.models.is_empty(),
            connection: state.connection.clone(),
            detail: state.detail.clone(),
        }
    }

    /// `render()` in the order of workspace §6.10, then the drawing of every region; returns the
    /// hardware cursor cell of the focused editor.
    fn render(&mut self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        let (width, height) = (area.width, area.height);
        let snapshot = self.snapshot();
        let entries = self.controller.transcript_entries();
        let activity = self.controller.activity();
        let view = snapshot.view.as_str();
        let home = view == "chat" && entries.is_empty() && !activity.working;
        let changed_view =
            self.previous_view != view || self.previous_project != snapshot.project_id;
        let sidebar = sidebar_visible(home, width, self.show_sidebar);
        let sidebar_cols = sidebar_width(width);
        let main_width = usize::from(layout::main_width(width, sidebar));
        let inner = width.saturating_sub(if sidebar { sidebar_cols } else { 0 } + 4);
        let prompt_width = if home { inner.min(75) } else { inner };
        let project = snapshot.project.as_deref();
        let root_or =
            |fallback: &'static str| project.filter(|root| !root.is_empty()).unwrap_or(fallback);

        let topic = entries
            .iter()
            .find(|entry| entry.kind == EntryKind::User)
            .map(|entry| entry.text.as_str())
            .filter(|text| !text.is_empty());
        let place = match view {
            "chat" => topic.map_or_else(|| basename(root_or("Research workspace")), str::to_owned),
            "map" => "RESEARCH MAP".into(),
            _ => view.to_uppercase(),
        };
        let heading = clip(&format!("NOSH  /  {place}"), main_width);
        let context = clip(
            &format!(
                "{}  ·  {}  ·  ctrl+p commands",
                basename(root_or("No project")),
                if view == "chat" {
                    "Research conversation"
                } else {
                    "Esc conversation"
                }
            ),
            main_width,
        );
        let role_name = if activity.role.is_empty() {
            "Research"
        } else {
            &activity.role
        }
        .replace('_', " ");
        let mut chars = role_name.chars();
        let role: String = chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        let model_name = match &snapshot.selection {
            Some(selection) => selection.model.id.clone(),
            None => activity.model.clone(),
        };
        let level = snapshot
            .selection
            .as_ref()
            .map(|selection| selection.thinking_level.as_str())
            .filter(|level| !level.is_empty())
            .unwrap_or(&activity.thinking_level);
        let model_label = if snapshot.no_model {
            "· no model connected — run nosh login in a terminal".to_owned()
        } else {
            let room = if home { 71 } else { main_width as i64 }
                - js::length(&role_name) as i64
                - if level.is_empty() {
                    6
                } else {
                    js::length(level) as i64 + 8
                };
            let name = if model_name.is_empty() {
                "Default model"
            } else {
                &model_name
            };
            clip(&format!("· {name}"), room.max(12) as usize)
        };
        let thinking = if !level.is_empty() && !snapshot.no_model {
            format!("· {level}")
        } else {
            String::new()
        };
        let working = snapshot.busy || activity.working;
        let bar = if !snapshot.error.is_empty() {
            THEME.error
        } else if working {
            THEME.secondary
        } else {
            THEME.accent
        };
        let hints = key_hints(if view == "map" {
            &[
                ("esc", "conversation"),
                ("↑↓←→", "select"),
                ("enter", "open / act"),
                ("scroll", "zoom"),
                ("drag", "move"),
                ("tab", "next"),
                ("ctrl+p", "commands"),
            ]
        } else if view != "chat" {
            &[
                ("esc", "conversation"),
                ("pgup/pgdn", "scroll"),
                ("ctrl+g", "map"),
                ("ctrl+p", "commands"),
            ]
        } else if project.is_none() {
            &[("ctrl+o", "choose a project"), ("ctrl+p", "commands")]
        } else if main_width < 72 {
            &[
                ("enter", "send"),
                ("⇧enter", "newline"),
                ("ctrl+p", "commands"),
            ]
        } else {
            &[
                ("enter", "send"),
                ("⇧enter", "newline"),
                ("ctrl+p", "commands"),
                ("ctrl+g", "map"),
                ("f2", "model"),
                ("ctrl+t", "thinking"),
            ]
        });
        let now = (self.now_ms)();
        let spinner = SPINNER_FRAMES[((now / 180.0).floor() as i64).rem_euclid(3) as usize];
        let status_color = if !snapshot.error.is_empty() {
            THEME.error
        } else if working {
            THEME.secondary
        } else {
            THEME.muted
        };
        let armed =
            !self.composer.value().is_empty() && now - self.escape_armed_at < ESCAPE_WINDOW_MS;
        let staged = self.controller.pending_action();
        let receipt = entries
            .last()
            .filter(|entry| entry.kind == EntryKind::Receipt);
        let status = safe_text_str(&if !snapshot.error.is_empty() {
            format!("! {}", snapshot.error)
        } else if armed {
            "esc again to clear the draft".into()
        } else if working {
            let label = if activity.label.is_empty() {
                "Working…"
            } else {
                &activity.label
            };
            format!("{spinner} {label}  ·  daemon continues if you detach")
        } else if snapshot.connection.starts_with("reconnecting") {
            format!("○ {} · retrying automatically", snapshot.connection)
        } else if staged.is_some() {
            "? Action staged · /confirm to review · /discard to dismiss".into()
        } else if let Some(receipt) = receipt {
            format!("✓ {}", clip(&receipt.title, main_width - 4))
        } else {
            String::new()
        });
        let style = |color: Color| Style::new().fg(color);
        let (muted, accent, text) = (style(THEME.muted), style(THEME.accent), style(THEME.text));
        let tip = if snapshot.no_model {
            Line::from(vec![
                Span::styled(" ! ", style(THEME.warning)),
                Span::styled("No model account connected. ", text),
                Span::styled("Run ", muted),
                Span::styled("nosh login", accent),
                Span::styled(" in a terminal, then ", muted),
                Span::styled("/refresh", accent),
                Span::styled(".", muted),
            ])
        } else if let Some(root) = project {
            Line::from(vec![
                Span::styled(" ◆ ", accent),
                Span::styled(basename(root), text),
                Span::styled("   ask a research question, or try ", muted),
                Span::styled("ctrl+g", accent),
                Span::styled(" map · ", muted),
                Span::styled("/help", accent),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " Open a repository you already have, or create a new one: ",
                    muted,
                ),
                Span::styled("/open", accent),
                Span::styled(" · ", muted),
                Span::styled("/new", accent),
            ])
        };
        let placeholder = if project.is_some() {
            "Ask NOSH… what should we investigate?"
        } else {
            "Choose a project first — press ctrl+o"
        };
        let (badge, badge_color) = match snapshot.connection.as_str() {
            "connected" => ("● connected", THEME.success),
            "connecting" => ("○ connecting", THEME.warning),
            _ => ("○ reconnecting", THEME.warning),
        };
        let cwd = clip(
            root_or("No project · ctrl+o to choose"),
            usize::from(width).saturating_sub(38).max(8),
        );
        let side_project = clip(
            &project.map_or_else(|| "No project selected".into(), basename),
            usize::from(sidebar_cols) - 4,
        );

        self.transcript.update(&entries);
        let inspector = view != "chat" && view != "map";
        if inspector {
            let fallback = match view {
                "help" => HELP,
                "paths" => snapshot.detail.as_str(),
                _ => empty_view(view).unwrap_or("Nothing to show."),
            };
            self.inspector
                .update(&self.controller.inspector_rows(), fallback);
        }
        // Called on every render, even with the sidebar hidden: it starts the lazy section load.
        let side_rows = self.controller.sidebar_rows();
        let mut attention = Vec::new();
        if let Some(staged) = &staged {
            attention.push(AttentionItem {
                title: staged.title.clone(),
                hint: "/confirm · /discard".into(),
            });
        }
        let proposals = side_rows
            .iter()
            .filter(|row| row.section == "approvals" && row.state.as_deref() == Some("pending"))
            .count();
        if proposals > 0 {
            attention.push(AttentionItem {
                title: format!(
                    "{proposals} graph proposal{} pending",
                    if proposals > 1 { "s" } else { "" }
                ),
                hint: "/approvals to inspect".into(),
            });
        }
        // Inside paddingX 2, less the terminal's last column (live, a width-4 line lost its last
        // character).
        let climb = climb_lines(
            self.controller.state().research_map.as_ref(),
            usize::from(sidebar_cols) - 5,
            CLIMB_MAXIMUM,
        );
        self.sidebar.update(&side_rows, &attention, &climb);

        self.composer.set_width(prompt_width.saturating_sub(5));
        self.composer.set_max_height(composer_max_height(height));
        let status_rows = if status.is_empty() {
            Vec::new()
        } else {
            let mut rows = wrap_words(&status, prompt_width);
            rows.truncate(3);
            rows
        };
        let regions = layout::compute(&LayoutInput {
            width,
            height,
            home,
            show_sidebar: self.show_sidebar,
            input_rows: self.composer.height(),
            status_rows: status_rows.len() as u16,
        });
        let conversation = regions.conversation.unwrap_or_default();
        if view == "map" {
            let rows = if conversation.height == 0 {
                height.saturating_sub(14)
            } else {
                conversation.height
            };
            let state = self.controller.state();
            self.map.update(
                state.research_map.as_ref(),
                main_width as u16,
                rows.saturating_sub(1).max(10),
            );
        }

        fill(buf, area, THEME.background);
        self.targets = Targets {
            screen: area,
            ..Targets::default()
        };
        if let Some(side) = regions.sidebar {
            fill(buf, side.area, THEME.panel);
            draw_line(buf, side.title, &compact_logo().patch_style(Modifier::BOLD));
            draw_line(
                buf,
                side.subtitle,
                &Line::styled("research workspace", muted),
            );
            draw_line(
                buf,
                side.project,
                &first_row(Line::styled(side_project, muted), side.project.width),
            );
            let items = self.sidebar.items(side.list.width);
            self.side.layout(items.len(), usize::from(side.list.height));
            for (row, (_, line)) in items
                .iter()
                .skip(self.side.offset)
                .take(usize::from(side.list.height))
                .enumerate()
            {
                draw_line(buf, row_of(side.list, row), line);
            }
            let footer = if sidebar_cols >= 38 {
                "ctrl+g map  ctrl+b hide  /jobs"
            } else {
                "ctrl+g map  ctrl+b hide"
            };
            draw_line(
                buf,
                side.footer,
                &first_row(Line::styled(footer, muted), side.footer.width),
            );
            self.targets.sidebar_list = side.list;
        }
        if let Some(area) = regions.logo {
            for (row, line) in logo().iter().enumerate() {
                draw_line(buf, row_of(area, row), line);
            }
        }
        if let Some(area) = regions.wordmark {
            draw_line(buf, area, &compact_logo());
        }
        if let Some(area) = regions.tagline {
            draw_line(buf, area, &Line::styled(TAGLINE, muted));
        }
        if let Some(area) = regions.heading {
            let line = Line::styled(heading, text.add_modifier(Modifier::BOLD));
            draw_line(buf, area, &first_row(line, area.width));
        }
        if let Some(area) = regions.context {
            draw_line(
                buf,
                area,
                &first_row(Line::styled(context, muted), area.width),
            );
        }
        if !home {
            self.draw_conversation(buf, conversation, view, height >= 20, changed_view);
        }

        fill(buf, regions.composer, THEME.element);
        split_bar(buf, regions.composer, bar, THEME.element);
        let cursor = self.composer.render(regions.input, buf, placeholder);
        let parts = model_line_layout(
            regions.model_line,
            cell_width(&role) as u16,
            cell_width(&model_label) as u16,
            cell_width(&thinking) as u16,
        );
        let model_color = if snapshot.no_model {
            THEME.warning
        } else {
            THEME.text
        };
        draw_line(buf, parts.role, &Line::styled(role, accent));
        draw_line(
            buf,
            parts.model,
            &first_row(
                Line::styled(model_label, style(model_color)),
                parts.model.width,
            ),
        );
        draw_line(
            buf,
            parts.thinking,
            &Line::styled(thinking.clone(), style(THEME.warning)),
        );
        draw_line(buf, regions.hints, &first_row(hints, regions.hints.width));
        if let Some(area) = regions.status {
            for (row, line) in status_rows.into_iter().enumerate() {
                draw_line(
                    buf,
                    row_of(area, row),
                    &Line::styled(line, style(status_color)),
                );
            }
        }
        if let Some(area) = regions.home_tip {
            draw_line(buf, area, &first_row(tip, area.width));
        }
        let version = format!("nosh {}", nosh_version());
        let footer = footer_layout(
            regions.footer,
            cell_width(badge) as u16,
            cell_width(&version) as u16,
        );
        draw_line(
            buf,
            footer.cwd,
            &first_row(Line::styled(cwd, muted), footer.cwd.width),
        );
        draw_line(
            buf,
            footer.connection,
            &Line::styled(badge, style(badge_color)),
        );
        draw_line(buf, footer.version, &Line::styled(version, muted));
        self.targets.conversation = conversation;
        self.targets.composer = regions.composer;
        self.targets.input = regions.input;
        self.targets.model = parts.model;
        if !thinking.is_empty() {
            self.targets.thinking = parts.thinking;
        }
        self.targets.cwd = footer.cwd;
        if changed_view {
            self.previous_view = snapshot.view.clone();
            self.previous_project = snapshot.project_id.clone();
        }

        let dialog_cursor = self.dialogs.draw(buf, area);
        if self.dialogs.is_open() {
            dialog_cursor
        } else {
            (self.focus == Focus::Composer).then_some(cursor)
        }
    }

    /// The conversation scroll area: a blank row, the banner (chat, `H ≥ 20`), then the transcript,
    /// inspector or research map; the scrollbar in non-chat views that overflow (§6.10 step 32).
    fn draw_conversation(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        view: &str,
        banner: bool,
        changed_view: bool,
    ) {
        let mut lines = vec![Line::default()];
        if view == "chat" && banner {
            let pad = |line: Line<'static>| {
                let mut spans = vec![Span::raw(" ")];
                spans.extend(line.spans);
                Line::from(spans)
            };
            lines.extend(logo().into_iter().map(pad));
            lines.push(Line::default());
            lines.push(pad(Line::styled(TAGLINE, Style::new().fg(THEME.muted))));
            lines.push(Line::default());
        }
        let lead = lines.len();
        let viewport = usize::from(area.height);
        let mut body = match view {
            "chat" => self.transcript.lines(area.width),
            "map" => self.map.lines(),
            _ => self.inspector.lines(area.width),
        };
        // Inspector views show a scrollbar only when content overflows; chat stays clean.
        let bar = view != "chat" && lead + body.len() > viewport;
        if bar && view != "map" {
            body = self.inspector.lines(area.width.saturating_sub(1));
        }
        lines.extend(body);
        if changed_view {
            self.conversation.sticky = view == "chat";
            self.conversation.manual = false;
        }
        self.conversation.layout(lines.len(), viewport);
        if changed_view {
            self.conversation.offset = if view == "chat" {
                self.conversation.max()
            } else {
                0
            };
        }
        let offset = self.conversation.offset;
        let content = Rect {
            width: area.width.saturating_sub(u16::from(bar)),
            ..area
        };
        for (row, line) in lines.iter().skip(offset).take(viewport).enumerate() {
            draw_line(buf, row_of(content, row), line);
        }
        if bar {
            let column = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
            scrollbar(buf, column, lines.len(), offset);
        }
        if view == "map" {
            let top = i64::from(area.y) + (lead + self.map.canvas_row()) as i64 - offset as i64;
            self.targets.canvas_origin = (i64::from(area.x), top);
            let bottom = (top + self.map.canvas_rows() as i64).clamp(0, i64::from(u16::MAX));
            let top = top.clamp(0, i64::from(u16::MAX));
            let canvas = Rect::new(area.x, top as u16, content.width, (bottom - top) as u16);
            self.targets.canvas = canvas.intersection(content);
        }
    }
}

/// The tagline under the logo (home and banner).
const TAGLINE: &str = "autonomous research you can verify";

/// Row `row` of `area`, empty past its bottom.
fn row_of(area: Rect, row: usize) -> Rect {
    let row = u16::try_from(row).unwrap_or(u16::MAX);
    Rect::new(area.x, area.y.saturating_add(row), area.width, 1).intersection(area)
}

/// Draws `line` into `area`, filling the area with the line's style first (panel backgrounds).
fn draw_line(buf: &mut Buffer, area: Rect, line: &Line) {
    let area = area.intersection(buf.area);
    if !area.is_empty() {
        line.render(area, buf);
    }
}

/// The first word-wrapped row of `line` at `width`: fixed one-row texts show only that (§6.4.1).
fn first_row(line: Line<'static>, width: u16) -> Line<'static> {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let hard = text.split('\n').next().unwrap_or("");
    let mut end = wrap_ranges(hard, width).first().map_or(0, |row| row.end);
    let mut spans = Vec::new();
    for span in line.spans {
        if end == 0 {
            break;
        }
        let cut = span.content.len().min(end);
        end -= cut;
        spans.push(Span::styled(span.content[..cut].to_owned(), span.style));
    }
    Line::from(spans).style(line.style)
}
