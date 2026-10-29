//! The TUI's state and command interpreter: a port of `apps/tui/src/controller.ts`.
//!
//! Specified by tui-controller.md §1-§14 (types, constants, `HELP`, `acceptPage`, fields, every
//! method, the command table §8, generations §9, strings §10, HTTP catalogue §11), §15 (tests T1-T53
//! and goldens), §16 (Rust mapping) and addenda CC-1 to CC-7; DECISIONS.md item 3 (controller quirks
//! are reproduced) and item 4 (`sidebar_rows` keeps its once-per-project lazy section load).
//!
//! Concurrency (controller §16.3): the controller lives in an `Rc` on one `current_thread` runtime
//! inside a `LocalSet`. Fire-and-forget loads use `tokio::task::spawn_local`, so `initialize`, `poll`,
//! `execute` and `sidebar_rows` must run inside `LocalSet::run_until` (tests included). No `RefCell`
//! borrow is held across an `.await` or a call back into the API; every continuation re-reads state
//! and compares its captured generation.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::js::JsValue;
use crate::transport::DaemonApi;
use crate::view_model::{ActivityState, PendingAction, TranscriptEntry, ViewRow};

/// `MAP_REFRESH_MS`: the minimum interval between automatic `/research-map` fetches.
pub const MAP_REFRESH_MS: f64 = 2500.0;

/// `CHAT_TIMEOUT`: the request timeout of `POST /chat` and `POST /projects/open`.
pub const CHAT_TIMEOUT: u64 = 120_000;

/// `HELP`: the command reference, byte-exact (57 lines, no trailing newline; controller §4). It is
/// the initial `detail` and the inspector text of the `help` view.
pub const HELP: &str = r#"NOSH / research workspace
Type a message to chat with the project's Nosh agent.

Keys
  enter                 send
  shift+enter           newline (also alt+enter)
  ctrl+p                commands
  ctrl+o                switch project
  f2                    select model
  ctrl+t                next thinking level
  ctrl+b                toggle research sidebar
  ctrl+g                research map (what is running, live)
  esc                   back to conversation
  esc esc               clear the draft
  up / down             input history
  pgup / pgdn           scroll
  ctrl+home / ctrl+end  top / bottom
  ctrl+c                detach (daemon and jobs keep running)

Project and model
  /projects                       list registered projects
  /project <id>                   select project
  /open | /new                    open or create a repository (form)
  /open <JSON>                    {"path":"/repo","workingTitle":"Study","createRepository":false}
  /models                         list authenticated models
  /model <provider> <id> [thinking] | /model default
  /thinking                       choose a thinking level for the selected model

Research state
  /chat                           return to conversation
  /map                            research map: missions, directions, autoresearch, workers, jobs
  /map mermaid [file]             write the map as a Mermaid chart (default research-map.md)
  /status [missions|directions|autoresearch|agents]
  /jobs                           supervised jobs
  /job <id>                       details + resources
  /tail <id> [stdout|stderr]      bounded job output
  /approvals                      graph proposals and project contract
  /paths                          repository, paper and contract paths
  /refresh                        reload current view

Staged actions (nothing is applied until /confirm)
  /cancel <job-id>                /checkpoint <job-id>
  /export-paper                   render docs/paper.md and paper.bib into .nosh/paper-export
  /approve <proposalId> <version>
  /start <missions|directions|autoresearch> <id> <version>   approve and start a new one (one confirmation)
  /transition <missions|directions|autoresearch> <id> <version> <state>
  /retry <missions|directions> <id> <version> <nodeId>   return a blocked/failed node to ready
  /control <missionId> <version> <pause|resume|stop> [safe|checkpoint|immediate]
  /steer <missionId> <version> <message>   bounded Mission Director steer
  /create <missions|directions|autoresearch> <JSON file path>
  /amend-contract <JSON file path>   next approved Project contract version (e.g. runnable commands)
  /confirm                        apply staged action once
  /discard                        discard staged action

Ids and versions for staged actions are shown on each card in /status, /jobs and /approvals.
Mutations are never retried after network failures. Inspect state before retrying.
/quit detaches; the daemon and supervised jobs keep running."#;

/// `Project`: one registered repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub project_id: String,
    pub repository_root: String,
}

/// `Model`: one authenticated model and the thinking levels it advertises.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub thinking_levels: Vec<String>,
}

/// The `{provider, id}` of an explicit model selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// `selection`: the explicit model and thinking level sent with chat and `/open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub model: ModelRef,
    pub thinking_level: String,
}

/// `defaultModel`: the daemon default as `provider/id` and its thinking level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultModel {
    pub name: String,
    pub thinking_level: String,
}

/// The controller fields that are public in TS (read by the renderer and dialogs, written by the
/// workspace and by tests), plus `rows` (read by the conformance driver) and `mapRequestedAt`
/// (written by test T4). Field semantics: controller §6.
#[derive(Clone, Debug)]
pub struct ControllerState {
    /// `projects`: the project catalog (latest `/projects`, at most 1000).
    pub projects: Vec<Project>,
    /// `models`: the model catalog (latest `/models`, at most 1000).
    pub models: Vec<Model>,
    /// `projectId`: the selected project; `""` is none.
    pub project_id: String,
    /// `events`: the durable event window (at most 400, ascending sequence), each a `NoshEvent`.
    pub events: Vec<JsValue>,
    /// `cursor`: the highest accepted durable sequence of the selected project.
    pub cursor: i64,
    /// `view`: `chat`, `help`, `map`, `paths`, `projects`, `models`, `status`, `jobs`, `detail`,
    /// `approvals`, `action` or `result`.
    pub view: String,
    /// `detail`: the text payload of the view (initially [`HELP`]).
    pub detail: String,
    /// `connection`: `connecting`, `connected` or `reconnecting: <message>`.
    pub connection: String,
    /// `error`: the last command error, sanitized; the workspace clears it on typing and Esc.
    pub error: String,
    /// `busy`: a command (`execute`) is in flight.
    pub busy: bool,
    pub selection: Option<Selection>,
    /// `modelsChecked`: the model catalog was read successfully at least once.
    pub models_checked: bool,
    pub default_model: Option<DefaultModel>,
    /// `researchMap`: the latest `/research-map` response; `None` is `undefined`.
    pub research_map: Option<JsValue>,
    /// `rows`: the rows of the current inspector view.
    pub rows: Vec<ViewRow>,
    /// `mapRequestedAt`: ms epoch of the last map request (initially the construction time).
    pub map_requested_at: f64,
}

/// `class Controller`. Private TS fields (generation, pending, sections, drafts, listeners, stream
/// handle, catalog tokens, agent metadata, …) are Track B's private fields.
pub struct Controller {
    state: RefCell<ControllerState>,
    api: Rc<dyn DaemonApi>,
    now_ms: Box<dyn Fn() -> f64>,
}

/// `acceptPage`: merges one page of durable events into the window (controller §5.1). Keeps safe
/// integer sequences above `after` for `project_id`, sorted stably, deduplicated by `eventId`, the
/// last 400 overall; returns the window and the new cursor.
pub fn accept_page(
    current: &[JsValue],
    page: &[JsValue],
    project_id: &str,
    after: i64,
) -> (Vec<JsValue>, i64) {
    todo!()
}

impl Controller {
    /// `new Controller(client, projectId)` with the system clock (`Date.now()`).
    pub fn new(api: Rc<dyn DaemonApi>, project_id: Option<String>) -> Rc<Self> {
        todo!()
    }

    /// As [`Controller::new`] with an injected ms-epoch clock (tests: `mapRequestedAt`, the 5000 ms
    /// agent throttle).
    pub fn with_clock(
        api: Rc<dyn DaemonApi>,
        project_id: Option<String>,
        now_ms: Box<dyn Fn() -> f64>,
    ) -> Rc<Self> {
        todo!()
    }

    /// Reads the public fields. Never hold the guard across a controller call or an `.await`.
    pub fn state(&self) -> Ref<'_, ControllerState> {
        self.state.borrow()
    }

    /// Writes the public fields (the workspace clears `error`; tests set `view`, `detail`,
    /// `projects`, `events`, …). Never hold the guard across a controller call or an `.await`.
    pub fn state_mut(&self) -> RefMut<'_, ControllerState> {
        self.state.borrow_mut()
    }

    /// `initialize`: authenticate, load projects, check or pick the project, enable live streaming,
    /// try the model catalog (controller §7.1). Errors propagate to `run`.
    pub async fn initialize(self: &Rc<Self>) -> Result<(), String> {
        todo!()
    }

    /// `poll`: fetches one bounded `/events` page for the current generation; errors become
    /// `connection = 'reconnecting: …'` and never propagate (controller §7.2).
    pub async fn poll(self: &Rc<Self>) {
        todo!()
    }

    /// `loadProjects`: the catalog, latest request wins (controller §7.7).
    pub async fn load_projects(&self) -> Result<Vec<Project>, String> {
        todo!()
    }

    /// `loadModels`: the catalog and the daemon default, latest request wins; returns deep copies
    /// (controller §7.8).
    pub async fn load_models(&self) -> Result<Vec<Model>, String> {
        todo!()
    }

    /// `selectProject`: resets every project-scoped field and bumps the generation; fails with
    /// `Unknown project. Run /projects first.` (controller §7.9). Closing the old stream emits its
    /// final `Disconnected` synchronously, which the bumped generation ignores.
    pub fn select_project(&self, project_id: &str, preserve_model: bool) -> Result<(), String> {
        todo!()
    }

    /// `pendingAction`: the staged action for review, or `None` (controller §7.10).
    pub fn pending_action(&self) -> Option<PendingAction> {
        todo!()
    }

    /// `inspectorRows`: the rows of the current view (controller §7.11).
    pub fn inspector_rows(&self) -> Vec<ViewRow> {
        todo!()
    }

    /// `sidebarRows`: the sidebar families plus durable job/proposal updates, last 40; the first call
    /// after bootstrap spawns the once-per-project section load (controller §7.13).
    pub fn sidebar_rows(self: &Rc<Self>) -> Vec<ViewRow> {
        todo!()
    }

    /// `activity`: who is working and with which model (controller §7.14).
    pub fn activity(&self) -> ActivityState {
        todo!()
    }

    /// `transcriptEntries`: durable entries with live drafts overlaid (controller §7.15).
    pub fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        todo!()
    }

    /// `subscribe(listener)`: registers a change listener and returns its id for
    /// [`Controller::unsubscribe`] (controller §7.16). Listeners must be infallible and must not call
    /// back into the controller synchronously (the renderer's listener only schedules a frame).
    pub fn subscribe(&self, listener: Rc<dyn Fn()>) -> u64 {
        todo!()
    }

    /// The unsubscribe function `subscribe` returns in TS.
    pub fn unsubscribe(&self, id: u64) {
        todo!()
    }

    /// `clearLiveDrafts`: drops every draft and live id, then notifies (controller §7.18).
    pub fn clear_live_drafts(&self) {
        todo!()
    }

    /// `dispose`: disables streaming, bumps the generation, closes the stream, clears drafts and
    /// listeners; no notify (controller §7.19).
    pub fn dispose(&self) {
        todo!()
    }

    /// `ingestLiveEvent`: applies one stream event, durable or ephemeral (controller §7.22).
    pub fn ingest_live_event(&self, event: &JsValue) {
        todo!()
    }

    /// `execute(input)`: runs one command or chat message; `Wait for the current command` while
    /// busy; clears `busy` and notifies on every exit path (controller §7.29, §8).
    pub async fn execute(self: &Rc<Self>, input: &str) -> Result<(), String> {
        todo!()
    }

    /// `transcript`: the conversation as plain text (controller §7.31).
    pub fn transcript(&self) -> String {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_is_verbatim() {
        assert_eq!(HELP.split('\n').count(), 57);
        assert!(HELP.starts_with("NOSH / research workspace\nType a message"));
        assert!(HELP.ends_with("/quit detaches; the daemon and supervised jobs keep running."));
        assert!(HELP.is_ascii() && !HELP.contains('\t') && !HELP.contains('\r'));
        assert!(HELP.contains(
            "\n  /open <JSON>                    {\"path\":\"/repo\",\"workingTitle\":\"Study\",\"createRepository\":false}\n"
        ));
    }
}
