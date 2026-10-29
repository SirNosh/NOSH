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

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::rc::Rc;

use futures_util::future::try_join_all;
use serde::{Deserialize, Serialize};

use crate::js::{
    JsObject, JsValue, date_now, encode_uri_component, is_safe_integer, js_json_parse, js_number,
    js_split_ws, js_trim, length, number_string, path_resolve, read_text_lossy, strict_equal,
    string, truthy,
};
use crate::transport::{DaemonApi, StreamState, Subscription};
use crate::view_model::{
    ActivityState, BOUND_ROWS_MAXIMUM, COLLAPSE_CHARACTERS, COLLAPSE_LINES, EntryKind,
    FIELDS_MAXIMUM, PendingAction, TEXT_LIMIT, TranscriptEntry, ViewField, ViewRow, bound_entries,
    bound_rows, collapse_lines, entity_row, event_entries, fields, format_fields, model_name,
    raw_detail, record, research_map_mermaid, response_rows, safe_text_str, text,
};

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

/// `class Controller`. The public TS fields are [`ControllerState`]; the private ones are
/// [`Private`]. Neither `RefCell` is ever borrowed across an `.await`, a [`DaemonApi`] call that may
/// call back, a listener or a stream cleanup.
pub struct Controller {
    state: RefCell<ControllerState>,
    private: RefCell<Private>,
    /// `listeners`, in insertion order (a `Set` in TS).
    listeners: RefCell<Vec<(u64, Listener)>>,
    next_listener: Cell<u64>,
    api: Rc<dyn DaemonApi>,
    now_ms: Box<dyn Fn() -> f64>,
}

/// A change listener (`() => void`).
type Listener = Rc<dyn Fn()>;

/// A staged action (`pending`).
#[derive(Clone)]
struct Pending {
    path: String,
    body: JsObject,
    from_map: bool,
}

/// The private TS fields.
#[derive(Default)]
struct Private {
    generation: u64,
    polling_generation: Option<u64>,
    bootstrapped: bool,
    pending: Option<Pending>,
    /// `sections`: rows per family, in family insertion order (a `Map` in TS).
    sections: Vec<(String, Vec<ViewRow>)>,
    section_cursors: HashMap<String, i64>,
    /// `liveIds`, oldest first.
    live_ids: VecDeque<String>,
    /// `drafts`, in insertion order (a `Map` in TS).
    drafts: Vec<(String, TranscriptEntry)>,
    live_enabled: bool,
    stream_project: String,
    stream: Option<Subscription>,
    catalog_projects: u64,
    catalog_models: u64,
    agent_metadata: Vec<JsObject>,
    agents_requested_at: f64,
    sections_loaded_for: String,
}

impl Private {
    fn section(&self, family: &str) -> Option<&Vec<ViewRow>> {
        self.sections
            .iter()
            .find(|(name, _)| name == family)
            .map(|(_, rows)| rows)
    }

    /// `sections.set(family, rows)`: an existing family keeps its position.
    fn set_section(&mut self, family: &str, rows: Vec<ViewRow>) {
        match self.sections.iter_mut().find(|(name, _)| name == family) {
            Some((_, slot)) => *slot = rows,
            None => self.sections.push((family.to_owned(), rows)),
        }
    }

    fn section_cursor(&self, family: &str) -> i64 {
        self.section_cursors.get(family).copied().unwrap_or(0)
    }

    fn draft(&self, key: &str) -> Option<&TranscriptEntry> {
        self.drafts
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, draft)| draft)
    }

    /// `drafts.set(key, draft)`: an existing key keeps its position.
    fn set_draft(&mut self, key: String, draft: TranscriptEntry) {
        match self.drafts.iter_mut().find(|(name, _)| *name == key) {
            Some((_, slot)) => *slot = draft,
            None => self.drafts.push((key, draft)),
        }
    }
}

/// An event's `sequence` when it is a number.
fn sequence(event: &JsValue) -> Option<f64> {
    event.get("sequence").and_then(JsValue::as_f64)
}

/// `e.sequence ?? 0` for the section-cursor comparisons.
fn sequence_or_zero(event: &JsValue) -> f64 {
    sequence(event).unwrap_or(0.0)
}

fn event_type(event: &JsValue) -> &str {
    event.get("type").and_then(JsValue::as_str).unwrap_or("")
}

fn scope(event: &JsValue) -> &JsObject {
    record(event.get("scope"))
}

/// `e.scope.projectId === projectId`.
fn in_project(event: &JsValue, project_id: &str) -> bool {
    scope(event).get("projectId").and_then(JsValue::as_str) == Some(project_id)
}

/// `text(e.scope.agentId ?? record(e.payload).agentId)`.
fn agent_of(event: &JsValue) -> String {
    let agent = scope(event)
        .get("agentId")
        .filter(|value| !value.is_null())
        .or_else(|| record(event.get("payload")).get("agentId"));
    text(agent, TEXT_LIMIT)
}

/// The sidebar family of an event type's first dot-separated segment.
fn section_of(event_type: &str) -> &'static str {
    match event_type.split('.').next() {
        Some("mission") => "missions",
        Some("direction") => "directions",
        Some("autoresearch") => "autoresearch",
        _ => "",
    }
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
    let mut ids: HashSet<String> = current
        .iter()
        .map(|event| string(event.get("eventId")))
        .collect();
    let mut accepted: Vec<&JsValue> = page
        .iter()
        .filter(|event| {
            in_project(event, project_id)
                && sequence(event)
                    .is_some_and(|number| is_safe_integer(number) && number > after as f64)
        })
        .collect();
    accepted.sort_by(|a, b| {
        sequence(a)
            .unwrap_or(0.0)
            .total_cmp(&sequence(b).unwrap_or(0.0))
    });
    accepted.retain(|event| ids.insert(string(event.get("eventId"))));
    let cursor = accepted
        .iter()
        .filter_map(|event| sequence(event))
        .fold(after, |cursor, number| cursor.max(number as i64));
    let mut events: Vec<JsValue> = current.to_vec();
    events.extend(accepted.into_iter().cloned());
    let start = events.len().saturating_sub(400);
    (events.split_off(start), cursor)
}

/// `version`: a positive safe-integer inspected version (`Number(value)`), else
/// `A positive inspected version is required` (controller §5.2).
fn version(value: Option<&str>) -> Result<f64, String> {
    let number = value.map_or(f64::NAN, js_number);
    match value {
        Some(text) if !text.is_empty() && is_safe_integer(number) && number >= 1.0 => Ok(number),
        _ => Err("A positive inspected version is required".into()),
    }
}

/// The V8 `TypeError` for reading a property of a non-object (controller §10.3, §16.6).
fn property_error(value: Option<&JsValue>, property: &str) -> String {
    let base = if value.is_some_and(JsValue::is_null) {
        "null"
    } else {
        "undefined"
    };
    format!("Cannot read properties of {base} (reading '{property}')")
}

/// `result.<key>.slice(0, 1000)` of a catalog array, with V8's `TypeError` texts otherwise. A string
/// slices in TS and then fails on `.map`; here it fails with that text without being assigned.
fn catalog<'a>(result: &'a JsValue, key: &str) -> Result<&'a [JsValue], String> {
    if result.is_null() {
        return Err(property_error(Some(result), key));
    }
    match result.get(key) {
        Some(JsValue::Array(items)) => Ok(&items[..items.len().min(1000)]),
        missing @ (None | Some(JsValue::Null)) => Err(property_error(missing, "slice")),
        Some(JsValue::String(_)) => Err(format!("this.{key}.map is not a function")),
        Some(_) => Err(format!("result.{key}.slice is not a function")),
    }
}

/// `String(value)` of a field that should be a string (`undefined` when absent, as TS shows it).
fn string_field(object: &JsObject, key: &str) -> String {
    string(object.get(key))
}

fn project_from(value: &JsValue) -> Project {
    let object = record(Some(value));
    Project {
        project_id: string_field(object, "projectId"),
        repository_root: string_field(object, "repositoryRoot"),
    }
}

fn model_from(value: &JsValue) -> Model {
    let object = record(Some(value));
    Model {
        provider: string_field(object, "provider"),
        id: string_field(object, "id"),
        name: string_field(object, "name"),
        thinking_levels: object
            .get("thinkingLevels")
            .and_then(JsValue::as_array)
            .map_or_else(Vec::new, |levels| {
                levels.iter().map(|level| string(Some(level))).collect()
            }),
    }
}

impl From<&Project> for JsValue {
    fn from(project: &Project) -> Self {
        let mut object = JsObject::new();
        object.set("projectId", project.project_id.as_str().into());
        object.set("repositoryRoot", project.repository_root.as_str().into());
        object.into()
    }
}

impl From<&Model> for JsValue {
    fn from(model: &Model) -> Self {
        let mut object = JsObject::new();
        object.set("provider", model.provider.as_str().into());
        object.set("id", model.id.as_str().into());
        object.set("name", model.name.as_str().into());
        object.set(
            "thinkingLevels",
            JsValue::Array(
                model
                    .thinking_levels
                    .iter()
                    .map(|level| level.as_str().into())
                    .collect(),
            ),
        );
        object.into()
    }
}

impl From<&ModelRef> for JsValue {
    fn from(model: &ModelRef) -> Self {
        let mut object = JsObject::new();
        object.set("provider", model.provider.as_str().into());
        object.set("id", model.id.as_str().into());
        object.into()
    }
}

impl Selection {
    /// `...this.selection`: `model` then `thinkingLevel`.
    fn spread_into(&self, body: &mut JsObject) {
        body.set("model", (&self.model).into());
        body.set("thinkingLevel", self.thinking_level.as_str().into());
    }
}

/// Node's `fs` error message for a failed `readFile` (`write == false`) or `writeFile`.
fn fs_error(error: &io::Error, path: &str, write: bool) -> String {
    let shown = if path.is_empty() {
        String::new()
    } else {
        path_resolve(path).display().to_string()
    };
    if !path.is_empty() && Path::new(path).is_dir() {
        return if write {
            format!("EISDIR: illegal operation on a directory, open '{shown}'")
        } else {
            "EISDIR: illegal operation on a directory, read".into()
        };
    }
    match error.kind() {
        io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, open '{shown}'"),
        io::ErrorKind::PermissionDenied if cfg!(windows) => {
            format!("EPERM: operation not permitted, open '{shown}'")
        }
        io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, open '{shown}'"),
        _ => error.to_string(),
    }
}

/// `readFile(path, 'utf8')` of a staged JSON object: at most 256000 UTF-16 units, a JSON object.
fn read_json_object(path: &str) -> Result<JsObject, String> {
    let text = read_text_lossy(Path::new(path)).map_err(|error| fs_error(&error, path, false))?;
    if length(&text) > 256_000 {
        return Err("Input file exceeds 256 KB".into());
    }
    match js_json_parse(&text)? {
        JsValue::Object(object) => Ok(object),
        _ => Err("Expected a JSON object".into()),
    }
}

const FAMILIES: [&str; 3] = ["missions", "directions", "autoresearch"];
const STATUS_FAMILIES: [&str; 4] = ["missions", "directions", "autoresearch", "agents"];
const PRESENTED_SECTIONS: [&str; 7] = [
    "missions",
    "directions",
    "autoresearch",
    "agents",
    "jobs",
    "approvals",
    "contract",
];
const OPEN_USAGE: &str =
    r#"Use /open {"path":"/repo","workingTitle":"Study","createRepository":false}"#;

/// `randomUUID()`.
fn uuid() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

fn text_row(
    id: &str,
    section: &str,
    title: String,
    subtitle: Option<String>,
    fields: Vec<ViewField>,
) -> ViewRow {
    ViewRow {
        id: id.to_owned(),
        section: section.to_owned(),
        title,
        state: None,
        version: None,
        subtitle,
        fields,
    }
}

impl Controller {
    /// `new Controller(client, projectId)` with the system clock (`Date.now()`).
    pub fn new(api: Rc<dyn DaemonApi>, project_id: Option<String>) -> Rc<Self> {
        Self::with_clock(api, project_id, Box::new(date_now))
    }

    /// As [`Controller::new`] with an injected ms-epoch clock (tests: `mapRequestedAt`, the 5000 ms
    /// agent throttle).
    pub fn with_clock(
        api: Rc<dyn DaemonApi>,
        project_id: Option<String>,
        now_ms: Box<dyn Fn() -> f64>,
    ) -> Rc<Self> {
        let state = ControllerState {
            projects: Vec::new(),
            models: Vec::new(),
            project_id: project_id.unwrap_or_default(),
            events: Vec::new(),
            cursor: 0,
            view: "chat".into(),
            detail: HELP.into(),
            connection: "connecting".into(),
            error: String::new(),
            busy: false,
            selection: None,
            models_checked: false,
            default_model: None,
            research_map: None,
            rows: Vec::new(),
            // The first fetch waits one interval, so the bootstrap page of events comes first.
            map_requested_at: now_ms(),
        };
        Rc::new(Self {
            state: RefCell::new(state),
            private: RefCell::new(Private::default()),
            listeners: RefCell::new(Vec::new()),
            next_listener: Cell::new(0),
            api,
            now_ms,
        })
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

    fn generation(&self) -> u64 {
        self.private.borrow().generation
    }

    /// `initialize`: authenticate, load projects, check or pick the project, enable live streaming,
    /// try the model catalog (controller §7.1). Errors propagate to `run`.
    pub async fn initialize(self: &Rc<Self>) -> Result<(), String> {
        self.api
            .authenticate()
            .await
            .map_err(|error| error.to_string())?;
        self.load_projects().await?;
        {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            if !state.project_id.is_empty()
                && !state
                    .projects
                    .iter()
                    .any(|project| project.project_id == state.project_id)
            {
                return Err("Selected project is not registered".into());
            }
            if state.project_id.is_empty() {
                state.project_id = state
                    .projects
                    .first()
                    .map(|project| project.project_id.clone())
                    .unwrap_or_default();
            }
            state.connection = "connected".into();
        }
        self.private.borrow_mut().live_enabled = true;
        // A first-run user may have skipped sign-in; knowing it lets the workspace say so before a
        // turn fails. The model picker retries on demand.
        if self.load_models().await.is_ok() {
            self.state.borrow_mut().models_checked = true;
        }
        Ok(())
    }

    /// `poll`: fetches one bounded `/events` page for the current generation; errors become
    /// `connection = 'reconnecting: …'` and never propagate (controller §7.2).
    pub async fn poll(self: &Rc<Self>) {
        let (project, generation, path) = {
            let private = self.private.borrow();
            let state = self.state.borrow();
            // A switched project polls immediately; the stale request is discarded by generation.
            if private.polling_generation == Some(private.generation) || state.project_id.is_empty()
            {
                return;
            }
            let recent = if !private.bootstrapped && state.cursor == 0 {
                "&recent=true"
            } else {
                ""
            };
            (
                state.project_id.clone(),
                private.generation,
                format!(
                    "/events?projectId={}&after={}&limit=200{recent}",
                    encode_uri_component(&state.project_id),
                    state.cursor
                ),
            )
        };
        self.private.borrow_mut().polling_generation = Some(generation);
        let outcome = match self.api.request(&path, None, None).await {
            Ok(page) => self.accept_poll(&page, &project, generation),
            Err(error) => Err(error.to_string()),
        };
        if let Err(message) = outcome
            && generation == self.generation()
        {
            self.state.borrow_mut().connection =
                format!("reconnecting: {}", safe_text_str(&message));
        }
        let mut private = self.private.borrow_mut();
        if private.polling_generation == Some(generation) {
            private.polling_generation = None;
        }
    }

    /// The `try` body of `poll` after the page arrived.
    fn accept_poll(
        self: &Rc<Self>,
        page: &JsValue,
        project: &str,
        generation: u64,
    ) -> Result<(), String> {
        if generation != self.generation() {
            return Ok(());
        }
        let events: &[JsValue] = match page {
            JsValue::Null => return Err(property_error(Some(page), "events")),
            JsValue::Object(object) => match object.get("events") {
                Some(JsValue::Array(events)) => events,
                Some(
                    JsValue::Object(_) | JsValue::String(_) | JsValue::Number(_) | JsValue::Bool(_),
                ) => {
                    return Err("page.filter is not a function".into());
                }
                missing => return Err(property_error(missing, "filter")),
            },
            _ => return Err(property_error(None, "filter")),
        };
        let prior_cursor = {
            let mut state = self.state.borrow_mut();
            let prior = state.cursor;
            let (window, cursor) = accept_page(&state.events, events, project, prior);
            state.events = window;
            state.cursor = cursor;
            state.connection = "connected".into();
            prior
        };
        self.private.borrow_mut().bootstrapped = true;
        let fresh: Vec<JsValue> = events
            .iter()
            .filter(|event| sequence(event).is_some_and(|number| number > prior_cursor as f64))
            .cloned()
            .collect();
        self.fold_sections(&fresh);
        self.reconcile_drafts(&fresh);
        self.ensure_stream()?;
        self.notify();
        self.refresh_agent_metadata(generation);
        let due = (self.now_ms)() - self.state.borrow().map_requested_at >= MAP_REFRESH_MS;
        if due {
            self.refresh_map(generation);
        }
        Ok(())
    }

    /// `loadSections`: every Mission, Direction and Autoresearch for the sidebar, not only those in
    /// the recent event window (controller §7.3). The three requests are issued now; the rest runs
    /// detached.
    fn load_sections(self: &Rc<Self>, generation: u64) {
        let Ok(query) = self.query() else { return };
        let cursor = self.state.borrow().cursor;
        let requests: Vec<_> = FAMILIES
            .iter()
            .map(|family| self.api.request(&format!("/{family}{query}"), None, None))
            .collect();
        let controller = self.clone();
        tokio::task::spawn_local(async move {
            // Live events still fill the sidebar; /status retries.
            let Ok(loaded) = try_join_all(requests).await else {
                return;
            };
            if generation != controller.generation() {
                return;
            }
            {
                let mut private = controller.private.borrow_mut();
                for (family, data) in FAMILIES.iter().zip(&loaded) {
                    let mut rows = response_rows(family, Some(data));
                    rows.truncate(80);
                    private.set_section(family, rows);
                    private.section_cursors.insert((*family).to_owned(), cursor);
                }
            }
            let events = controller.state.borrow().events.clone();
            controller.fold_sections(&events);
            controller.notify();
        });
    }

    /// `refreshAgentMetadata`: agent events carry no role or model, so `/agents` is loaded once per
    /// unknown agent, at most every 5 s (controller §7.4).
    fn refresh_agent_metadata(self: &Rc<Self>, generation: u64) {
        {
            let private = self.private.borrow();
            let known: HashSet<String> = private
                .agent_metadata
                .iter()
                .map(|agent| text(agent.get("agentId"), TEXT_LIMIT))
                .collect();
            let unknown = self.state.borrow().events.iter().any(|event| {
                let id = text(scope(event).get("agentId"), TEXT_LIMIT);
                !id.is_empty() && event_type(event).starts_with("agent.") && !known.contains(&id)
            });
            if !unknown || (self.now_ms)() - private.agents_requested_at < 5000.0 {
                return;
            }
        }
        self.private.borrow_mut().agents_requested_at = (self.now_ms)();
        let Ok(query) = self.query() else { return };
        let request = self.api.request(&format!("/agents{query}"), None, None);
        let controller = self.clone();
        tokio::task::spawn_local(async move {
            // Metadata is presentation only; the next poll retries.
            let Ok(data) = request.await else { return };
            // A null reply throws on `data.agents` inside the `try`.
            if data.is_null() || generation != controller.generation() {
                return;
            }
            controller.private.borrow_mut().agent_metadata = agent_records(&data);
            controller.notify();
        });
    }

    /// `refreshMap`: the research map for the map view and the sidebar's climb (controller §7.5).
    fn refresh_map(self: &Rc<Self>, generation: u64) {
        self.state.borrow_mut().map_requested_at = (self.now_ms)();
        let Ok(query) = self.query() else { return };
        let request = self
            .api
            .request(&format!("/research-map{query}"), None, None);
        let controller = self.clone();
        tokio::task::spawn_local(async move {
            // The map is presentation only; the next poll retries.
            let Ok(data) = request.await else { return };
            if generation == controller.generation() {
                controller.state.borrow_mut().research_map = Some(data);
                controller.notify();
            }
        });
    }

    /// `loadProjects`: the catalog, latest request wins (controller §7.7). Catalog reads do not
    /// navigate or stage an action.
    pub async fn load_projects(&self) -> Result<Vec<Project>, String> {
        let token = {
            let mut private = self.private.borrow_mut();
            private.catalog_projects += 1;
            private.catalog_projects
        };
        let result = self
            .api
            .request("/projects", None, None)
            .await
            .map_err(|error| error.to_string())?;
        if token == self.private.borrow().catalog_projects {
            let projects = catalog(&result, "projects")?
                .iter()
                .map(project_from)
                .collect();
            self.state.borrow_mut().projects = projects;
        }
        Ok(self.state.borrow().projects.clone())
    }

    /// `loadModels`: the catalog and the daemon default, latest request wins; returns deep copies
    /// (controller §7.8).
    pub async fn load_models(&self) -> Result<Vec<Model>, String> {
        let token = {
            let mut private = self.private.borrow_mut();
            private.catalog_models += 1;
            private.catalog_models
        };
        let result = self
            .api
            .request("/models", None, None)
            .await
            .map_err(|error| error.to_string())?;
        if token == self.private.borrow().catalog_models {
            let models = catalog(&result, "models")?.iter().map(model_from).collect();
            let preset = record(result.get("defaultModel"));
            let default_model =
                (truthy(preset.get("provider")) && truthy(preset.get("id"))).then(|| {
                    DefaultModel {
                        name: format!(
                            "{}/{}",
                            string(preset.get("provider")),
                            string(preset.get("id"))
                        ),
                        thinking_level: match preset.get("thinkingLevel") {
                            None | Some(JsValue::Null) => String::new(),
                            level => string(level),
                        },
                    }
                });
            let mut state = self.state.borrow_mut();
            state.models = models;
            state.models_checked = true;
            state.default_model = default_model;
        }
        Ok(self.state.borrow().models.clone())
    }

    /// Takes the stream handle out (so no borrow is held) and cleans it up; its final
    /// `Disconnected` is ignored by the already bumped generation.
    fn close_stream(&self) {
        let stream = {
            let mut private = self.private.borrow_mut();
            private.stream_project.clear();
            private.stream.take()
        };
        if let Some(stream) = stream {
            stream.cleanup();
        }
    }

    /// `selectProject`: resets every project-scoped field and bumps the generation; fails with
    /// `Unknown project. Run /projects first.` (controller §7.9). `sectionsLoadedFor` is not reset,
    /// as in TS (DECISIONS.md item 3).
    pub fn select_project(&self, project_id: &str, preserve_model: bool) -> Result<(), String> {
        let project = self
            .state
            .borrow()
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
            .map(|project| project.project_id.clone())
            .ok_or("Unknown project. Run /projects first.")?;
        self.private.borrow_mut().generation += 1;
        self.close_stream();
        {
            let mut private = self.private.borrow_mut();
            private.bootstrapped = false;
            private.pending = None;
            private.sections.clear();
            private.section_cursors.clear();
            private.agent_metadata.clear();
            private.agents_requested_at = 0.0;
            private.drafts.clear();
            private.live_ids.clear();
        }
        {
            let mut state = self.state.borrow_mut();
            state.project_id = project;
            state.events.clear();
            state.cursor = 0;
            if !preserve_model {
                state.selection = None;
            }
            state.rows.clear();
            state.research_map = None;
            state.view = "chat".into();
            state.detail.clear();
            state.error.clear();
        }
        self.notify();
        Ok(())
    }

    /// `pendingAction`: a detached copy of the staged action for review, or `None` (controller
    /// §7.10).
    pub fn pending_action(&self) -> Option<PendingAction> {
        let pending = self.private.borrow().pending.clone()?;
        let body = JsValue::Object(pending.body.clone());
        let segment = pending.path.split('/').rfind(|part| !part.is_empty());
        Some(PendingAction {
            title: format!("Review {}", segment.unwrap_or("action")),
            project_id: string(pending.body.get("projectId")),
            fields: fields(Some(&body), FIELDS_MAXIMUM),
            path: pending.path,
            body: pending.body,
        })
    }

    /// `inspectorRows`: the rows of the current view (controller §7.11).
    pub fn inspector_rows(&self) -> Vec<ViewRow> {
        bound_rows(&self.state.borrow().rows, BOUND_ROWS_MAXIMUM)
    }

    /// `foldSections`: entity state changes are folded into the sidebar as they arrive, because the
    /// 400-event window loses them within minutes of busy research (controller §7.12).
    fn fold_sections(&self, events: &[JsValue]) {
        let project_id = self.state.borrow().project_id.clone();
        let mut private = self.private.borrow_mut();
        for event in events {
            let p = record(event.get("payload"));
            let section = section_of(event_type(event));
            if section.is_empty()
                || !truthy(p.get("entityId"))
                || !truthy(p.get("state"))
                || !in_project(event, &project_id)
                || sequence_or_zero(event) <= private.section_cursor(section) as f64
            {
                continue;
            }
            let id = text(p.get("entityId"), TEXT_LIMIT);
            let mut rows = private.section(section).cloned().unwrap_or_default();
            let prior = rows
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.title.clone());
            let row = ViewRow {
                title: prior
                    .clone()
                    .unwrap_or_else(|| non_empty_or(text(p.get("title"), TEXT_LIMIT), &id)),
                id: id.clone(),
                section: section.to_owned(),
                state: Some(text(p.get("state"), TEXT_LIMIT)),
                version: None,
                subtitle: None,
                fields: fields(event.get("payload"), FIELDS_MAXIMUM),
            };
            if prior.is_some() {
                for entry in rows.iter_mut().filter(|entry| entry.id == id) {
                    *entry = row.clone();
                }
            } else {
                rows.push(row);
            }
            private.set_section(section, rows);
        }
    }

    /// `sidebarRows`: the sidebar families plus durable job and proposal updates, last 40; the first
    /// call after bootstrap starts the once-per-project section load (controller §7.13,
    /// DECISIONS.md item 4).
    pub fn sidebar_rows(self: &Rc<Self>) -> Vec<ViewRow> {
        let load = {
            let private = self.private.borrow();
            let state = self.state.borrow();
            (!state.project_id.is_empty()
                && private.bootstrapped
                && private.sections_loaded_for != state.project_id)
                .then(|| (state.project_id.clone(), private.generation))
        };
        if let Some((project_id, generation)) = load {
            self.private.borrow_mut().sections_loaded_for = project_id;
            self.load_sections(generation);
        }
        let private = self.private.borrow();
        let state = self.state.borrow();
        let mut rows: Vec<(String, ViewRow)> = Vec::new();
        let set = |rows: &mut Vec<(String, ViewRow)>, key: String, row: ViewRow| match rows
            .iter_mut()
            .find(|(name, _)| *name == key)
        {
            Some((_, slot)) => *slot = row,
            None => rows.push((key, row)),
        };
        for (_, entries) in &private.sections {
            for row in entries {
                set(
                    &mut rows,
                    format!("{}:{}", row.section, row.id),
                    row.clone(),
                );
            }
        }
        // Durable job and proposal updates fill the sidebar without fetching every panel.
        for event in &state.events {
            if !in_project(event, &state.project_id) {
                continue;
            }
            let p = record(event.get("payload"));
            let kind = event_type(event);
            let section = section_of(kind);
            let number = sequence_or_zero(event);
            let prior_title = |rows: &[(String, ViewRow)], key: &str| {
                rows.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, row)| row.clone())
            };
            if !section.is_empty()
                && truthy(p.get("entityId"))
                && truthy(p.get("state"))
                && number > private.section_cursor(section) as f64
            {
                let id = text(p.get("entityId"), TEXT_LIMIT);
                let key = format!("{section}:{id}");
                let prior = prior_title(&rows, &key);
                let row = ViewRow {
                    title: prior.map_or_else(
                        || non_empty_or(text(p.get("title"), TEXT_LIMIT), &id),
                        |row| row.title,
                    ),
                    id,
                    section: section.to_owned(),
                    state: Some(text(p.get("state"), TEXT_LIMIT)),
                    version: None,
                    subtitle: None,
                    fields: fields(event.get("payload"), FIELDS_MAXIMUM),
                };
                set(&mut rows, key, row);
            }
            if kind == "job.state_changed" && number > private.section_cursor("jobs") as f64 {
                let mut job = p.clone();
                let job_id = [p.get("jobId"), scope(event).get("jobId")]
                    .into_iter()
                    .flatten()
                    .find(|value| !value.is_null());
                job.set("jobId", job_id.cloned().unwrap_or(JsValue::Null));
                let row = entity_row("jobs", Some(&JsValue::Object(job)), 0);
                set(&mut rows, format!("jobs:{}", row.id), row);
            } else if matches!(kind, "graph.proposal_pending" | "graph.proposal_approved")
                && number > private.section_cursor("approvals") as f64
            {
                let id = text(p.get("proposalId"), TEXT_LIMIT);
                if id.is_empty() {
                    continue;
                }
                let key = format!("approvals:{id}");
                let prior = prior_title(&rows, &key);
                let row = ViewRow {
                    title: prior
                        .as_ref()
                        .map_or_else(|| "Graph proposal".to_owned(), |row| row.title.clone()),
                    state: Some(
                        if kind.ends_with("_approved") {
                            "approved"
                        } else {
                            "pending"
                        }
                        .into(),
                    ),
                    fields: prior.map_or_else(
                        || fields(event.get("payload"), FIELDS_MAXIMUM),
                        |row| row.fields,
                    ),
                    id,
                    section: "approvals".into(),
                    version: None,
                    subtitle: None,
                };
                set(&mut rows, key, row);
            }
        }
        let rows: Vec<ViewRow> = rows.into_iter().map(|(_, row)| row).collect();
        bound_rows(&rows[rows.len().saturating_sub(40)..], 40)
    }

    /// `activity`: who is working and with which model (controller §7.14).
    pub fn activity(&self) -> ActivityState {
        let private = self.private.borrow();
        let state = self.state.borrow();
        let mut active: Vec<(String, String)> = Vec::new();
        let mut selections: HashMap<String, JsObject> = HashMap::new();
        let (mut latest_agent, mut latest_chat) = (String::new(), String::new());
        for event in &state.events {
            if !in_project(event, &state.project_id) {
                continue;
            }
            let p = record(event.get("payload"));
            let agent = agent_of(event);
            let kind = event_type(event);
            if agent.is_empty() || !kind.starts_with("agent.") && kind != "chat.user_message" {
                continue;
            }
            latest_agent.clone_from(&agent);
            if kind == "chat.user_message" {
                selections.insert(agent.clone(), record(p.get("selection")).clone());
                latest_chat.clone_from(&agent);
            }
            if matches!(
                kind,
                "chat.user_message"
                    | "agent.started"
                    | "agent.turn_started"
                    | "agent.retrying"
                    | "agent.tool_started"
            ) {
                let label = match kind {
                    "agent.tool_started" => format!(
                        "Running {}",
                        non_empty_or(text(p.get("toolName"), TEXT_LIMIT), "tool")
                    ),
                    "agent.retrying" => "Retrying provider turn".into(),
                    _ => "Working".into(),
                };
                ordered_set(&mut active, agent.clone(), label);
            }
            if kind == "agent.completed"
                || kind == "agent.failed"
                || kind == "agent.terminal_receipt"
                    && p.get("retryAllowed").and_then(JsValue::as_bool) != Some(true)
            {
                active.retain(|(name, _)| *name != agent);
            }
        }
        for (_, draft) in &private.drafts {
            if let Some(agent) = &draft.agent_id {
                ordered_set(&mut active, agent.clone(), "Writing".into());
            }
        }
        // Idle, the composer names who the user is talking to (the chat agent), not the last
        // background task agent.
        let agent_id = active.last().map_or_else(
            || {
                if latest_chat.is_empty() {
                    latest_agent
                } else {
                    latest_chat
                }
            },
            |(name, _)| name.clone(),
        );
        let inspection = private.agent_metadata.iter().find(|agent| {
            agent.get("agentId").and_then(JsValue::as_str) == Some(agent_id.as_str())
        });
        let explicit_model = state
            .selection
            .as_ref()
            .map_or_else(String::new, |selection| {
                model_name(Some(&JsValue::from(&selection.model)))
            });
        let durable = selections.get(&agent_id);
        let observed_model = non_empty(inspection.map_or_else(String::new, |agent| {
            model_name(Some(&JsValue::Object(agent.clone())))
        }))
        .unwrap_or_else(|| model_name(durable.and_then(|selection| selection.get("model"))));
        let model = if active.is_empty() {
            non_empty(explicit_model).unwrap_or(observed_model)
        } else {
            observed_model
        };
        let model = non_empty(model)
            .or_else(|| {
                state
                    .default_model
                    .as_ref()
                    .map(|preset| preset.name.clone())
            })
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| "Default model".into());
        let inspected_level = || {
            [
                inspection.and_then(|agent| agent.get("thinkingLevel")),
                durable.and_then(|selection| selection.get("thinkingLevel")),
            ]
            .into_iter()
            .flatten()
            .find(|level| !level.is_null())
            .cloned()
        };
        let level = if active.is_empty() {
            match (&state.selection, inspected_level()) {
                (Some(selection), _) => Some(JsValue::from(selection.thinking_level.as_str())),
                (None, Some(level)) => Some(level),
                (None, None) => state
                    .default_model
                    .as_ref()
                    .map(|preset| JsValue::from(preset.thinking_level.as_str())),
            }
        } else {
            inspected_level()
        };
        let role = non_empty(text(
            inspection.and_then(|agent| agent.get("role")),
            TEXT_LIMIT,
        ))
        .unwrap_or_else(|| {
            if selections.contains_key(&agent_id) {
                "nosh".into()
            } else if agent_id.is_empty() {
                String::new()
            } else {
                "agent".into()
            }
        });
        let label = active
            .iter()
            .find(|(name, _)| *name == agent_id)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| {
                if state.busy {
                    "Sending command"
                } else {
                    "Ready"
                }
                .into()
            });
        ActivityState {
            working: !active.is_empty(),
            label,
            model,
            thinking_level: text(level.as_ref(), TEXT_LIMIT),
            role,
            agent_id: non_empty(agent_id),
        }
    }

    /// `transcriptEntries`: durable entries with live drafts overlaid (controller §7.15).
    pub fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        let mut entries = {
            let state = self.state.borrow();
            let events: Vec<JsValue> = state
                .events
                .iter()
                .filter(|event| in_project(event, &state.project_id))
                .cloned()
                .collect();
            event_entries(&events)
        };
        for (_, draft) in &self.private.borrow().drafts {
            if draft.kind == EntryKind::Tool
                && let Some(tool) = entries.iter_mut().find(|entry| entry.id == draft.id)
            {
                let title = tool.title.clone();
                tool.assign(TranscriptEntry {
                    title,
                    ..draft.clone()
                });
                continue;
            }
            entries.push(draft.clone());
        }
        bound_entries(&entries)
    }

    /// `subscribe(listener)`: registers a change listener and returns its id for
    /// [`Controller::unsubscribe`] (controller §7.16). Listeners must be infallible and must not call
    /// back into the controller synchronously (the renderer's listener only schedules a frame).
    pub fn subscribe(&self, listener: Rc<dyn Fn()>) -> u64 {
        let id = self.next_listener.get();
        self.next_listener.set(id + 1);
        self.listeners.borrow_mut().push((id, listener));
        id
    }

    /// The unsubscribe function `subscribe` returns in TS.
    pub fn unsubscribe(&self, id: u64) {
        self.listeners
            .borrow_mut()
            .retain(|(listener, _)| *listener != id);
    }

    /// `notify`: calls every listener; the list is copied first, so a listener may unsubscribe.
    fn notify(&self) {
        let listeners: Vec<Listener> = self
            .listeners
            .borrow()
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener();
        }
    }

    /// `clearLiveDrafts`: drops every draft and live id, then notifies (controller §7.18).
    pub fn clear_live_drafts(&self) {
        {
            let mut private = self.private.borrow_mut();
            private.drafts.clear();
            private.live_ids.clear();
        }
        self.notify();
    }

    /// `dispose`: disables streaming, bumps the generation, closes the stream, clears drafts and
    /// listeners; no notify (controller §7.19).
    pub fn dispose(&self) {
        {
            let mut private = self.private.borrow_mut();
            private.live_enabled = false;
            private.generation += 1;
        }
        self.close_stream();
        self.private.borrow_mut().drafts.clear();
        self.listeners.borrow_mut().clear();
    }

    /// `ensureStream`: opens the live stream once per project and generation, after the HTTP
    /// bootstrap (controller §7.21). A synchronous `subscribe` failure propagates to `poll` and,
    /// as in TS, is not retried for the same project (DECISIONS.md item 3).
    fn ensure_stream(self: &Rc<Self>) -> Result<(), String> {
        let (project, generation, after) = {
            let private = self.private.borrow();
            let state = self.state.borrow();
            if !private.live_enabled
                || !private.bootstrapped
                || private.stream_project == state.project_id
            {
                return Ok(());
            }
            (state.project_id.clone(), private.generation, state.cursor)
        };
        let previous = self.private.borrow_mut().stream.take();
        if let Some(previous) = previous {
            previous.cleanup();
        }
        self.private
            .borrow_mut()
            .stream_project
            .clone_from(&project);
        let (events, states) = (Rc::downgrade(self), Rc::downgrade(self));
        let stream_project = project.clone();
        let stream = self.api.subscribe(
            &project,
            after,
            Box::new(move |event| {
                if let Some(controller) = events.upgrade()
                    && generation == controller.generation()
                    && controller.state.borrow().project_id == stream_project
                {
                    controller.ingest_live_event(event);
                }
                Ok(())
            }),
            Box::new(move |stream_state| {
                let Some(controller) = states.upgrade() else {
                    return;
                };
                if generation != controller.generation() {
                    return;
                }
                if stream_state == StreamState::Disconnected {
                    controller.clear_live_drafts();
                }
                controller.notify();
            }),
        );
        self.private.borrow_mut().stream = Some(stream.map_err(|error| error.to_string())?);
        Ok(())
    }

    /// `ingestLiveEvent`: applies one stream event: a durable one through `acceptPage`, an ephemeral
    /// one (no sequence) as a bounded draft (controller §7.22).
    pub fn ingest_live_event(&self, event: &JsValue) {
        let project_id = self.state.borrow().project_id.clone();
        if !in_project(event, &project_id) {
            return;
        }
        if !event.get("sequence").is_some_and(JsValue::is_null) {
            {
                let mut state = self.state.borrow_mut();
                let (window, cursor) = accept_page(
                    &state.events,
                    std::slice::from_ref(event),
                    &project_id,
                    state.cursor,
                );
                if cursor == state.cursor {
                    return;
                }
                state.events = window;
                state.cursor = cursor;
            }
            self.fold_sections(std::slice::from_ref(event));
            self.reconcile_drafts(std::slice::from_ref(event));
            self.notify();
            return;
        }
        let agent_id = text(scope(event).get("agentId"), TEXT_LIMIT);
        let event_id = string(event.get("eventId"));
        let p = record(event.get("payload"));
        {
            let mut private = self.private.borrow_mut();
            if agent_id.is_empty() || private.live_ids.contains(&event_id) {
                return;
            }
            let state = self.state.borrow();
            let latest = state.events.iter().rev().find(|prior| {
                agent_of(prior) == agent_id
                    && matches!(
                        event_type(prior),
                        "agent.started"
                            | "agent.turn_started"
                            | "agent.completed"
                            | "agent.failed"
                            | "agent.retrying"
                            | "agent.terminal_receipt"
                            | "chat.user_message"
                    )
            });
            if latest.is_some_and(|latest| {
                matches!(
                    event_type(latest),
                    "agent.completed" | "agent.failed" | "agent.terminal_receipt"
                )
            }) {
                return;
            }
            private.live_ids.push_back(event_id.clone());
            if private.live_ids.len() > 512 {
                private.live_ids.pop_front();
            }
            let draft =
                |kind, key: String, title: String, body: String, status: &str| TranscriptEntry {
                    id: key,
                    kind,
                    title,
                    text: body,
                    sequence: None,
                    agent_id: Some(agent_id.clone()),
                    model: None,
                    thinking_level: None,
                    status: Some(status.to_owned()),
                };
            match (event_type(event), p.get("delta")) {
                ("agent.text_delta", Some(JsValue::String(delta))) => {
                    let key = format!("draft:{project_id}:{agent_id}");
                    let prior = private.draft(&key).map_or("", |prior| prior.text.as_str());
                    let body = safe_text_str(&format!("{prior}{delta}"));
                    let entry = draft(
                        EntryKind::Assistant,
                        key.clone(),
                        "NOSH".into(),
                        body,
                        "streaming",
                    );
                    private.set_draft(key, entry);
                }
                ("agent.tool_update", _) => {
                    let call = p.get("toolCallId");
                    let completed = state.events.iter().any(|prior| {
                        event_type(prior) == "agent.tool_completed"
                            && text(scope(prior).get("agentId"), TEXT_LIMIT) == agent_id
                            && strict_equal(record(prior.get("payload")).get("toolCallId"), call)
                    });
                    if completed {
                        return;
                    }
                    let key = format!(
                        "tool:{project_id}:{agent_id}:{}",
                        non_empty_or(text(call, TEXT_LIMIT), &event_id)
                    );
                    let partial = p.get("partialResult");
                    let body = match record(partial).get("content") {
                        Some(JsValue::Array(content)) => content
                            .iter()
                            .take(8)
                            .map(|item| record(Some(item)))
                            .filter(|item| {
                                item.get("type").and_then(JsValue::as_str) == Some("text")
                            })
                            .map(|item| text(item.get("text"), 2000))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => format_fields(&fields(partial, 8)),
                    };
                    let title = non_empty_or(text(p.get("toolName"), TEXT_LIMIT), "Tool");
                    let body =
                        collapse_lines(&safe_text_str(&body), COLLAPSE_LINES, COLLAPSE_CHARACTERS);
                    let entry = draft(EntryKind::Tool, key.clone(), title, body, "running");
                    private.set_draft(key, entry);
                }
                _ => return,
            }
            // Both active agents and output bytes are bounded independently.
            let excess = private.drafts.len().saturating_sub(16);
            private.drafts.drain(..excess);
        }
        self.notify();
    }

    /// `reconcileDrafts`: a turn's end drops its agent's drafts; a tool's completion drops its tool
    /// draft (controller §7.23).
    fn reconcile_drafts(&self, events: &[JsValue]) {
        let project_id = self.state.borrow().project_id.clone();
        let mut private = self.private.borrow_mut();
        for event in events {
            if !in_project(event, &project_id) {
                continue;
            }
            let agent_id = agent_of(event);
            let kind = event_type(event);
            if matches!(
                kind,
                "agent.completed" | "agent.failed" | "agent.retrying" | "agent.terminal_receipt"
            ) {
                private
                    .drafts
                    .retain(|(_, draft)| draft.agent_id.as_deref() != Some(agent_id.as_str()));
            }
            if kind == "agent.tool_completed" {
                let call = text(record(event.get("payload")).get("toolCallId"), TEXT_LIMIT);
                let key = format!(
                    "tool:{project_id}:{agent_id}:{}",
                    non_empty_or(call, &string(event.get("eventId")))
                );
                private.drafts.retain(|(name, _)| *name != key);
            }
        }
    }

    /// `present`: shows rows and raw detail for a view and refreshes the sidebar families they
    /// belong to (controller §7.24).
    fn present(&self, view: &str, rows: Vec<ViewRow>, raw: &JsValue) {
        let cursor = {
            let mut state = self.state.borrow_mut();
            state.rows = bound_rows(&rows, BOUND_ROWS_MAXIMUM);
            state.view = view.to_owned();
            state.detail = raw_detail(Some(raw));
            state.cursor
        };
        let mut sections: Vec<&str> = Vec::new();
        for row in &rows {
            if !sections.contains(&row.section.as_str()) {
                sections.push(&row.section);
            }
        }
        let mut private = self.private.borrow_mut();
        for section in sections {
            if !PRESENTED_SECTIONS.contains(&section) {
                continue;
            }
            let incoming: Vec<ViewRow> = rows
                .iter()
                .filter(|row| row.section == section)
                .take(80)
                .cloned()
                .collect();
            let mut merged = if view == "detail" {
                let mut merged: Vec<ViewRow> = private
                    .section(section)
                    .map(|rows| {
                        rows.iter()
                            .filter(|row| !incoming.iter().any(|next| next.id == row.id))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                merged.extend(incoming);
                merged
            } else {
                incoming
            };
            let merged = merged.split_off(merged.len().saturating_sub(80));
            private.set_section(section, merged);
            private.section_cursors.insert(section.to_owned(), cursor);
        }
    }

    /// `requireProject`.
    fn require_project(&self) -> Result<(), String> {
        if self.state.borrow().project_id.is_empty() {
            Err("Select or open a project first".into())
        } else {
            Ok(())
        }
    }

    /// `query`: `?projectId=<encoded>`.
    fn query(&self) -> Result<String, String> {
        self.require_project()?;
        Ok(format!(
            "?projectId={}",
            encode_uri_component(&self.state.borrow().project_id)
        ))
    }

    /// `stage`: holds an action for review until `/confirm` (controller §7.28). The map draws from
    /// `researchMap`, not rows or detail, so a discard can return to it.
    fn stage(&self, path: &str, body: JsObject) {
        let (from_map, project_id) = {
            let private = self.private.borrow();
            let state = self.state.borrow();
            let from_map = state.view == "map"
                || state.view == "action"
                    && private
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.from_map);
            (from_map, state.project_id.clone())
        };
        let mut staged = body;
        staged.set("projectId", project_id.as_str().into());
        staged.set("idempotencyKey", format!("tui-{}", uuid()).into());
        let staged_value = JsValue::Object(staged.clone());
        self.private.borrow_mut().pending = Some(Pending {
            path: path.to_owned(),
            body: staged,
            from_map,
        });
        let title = self
            .pending_action()
            .map(|action| action.title)
            .unwrap_or_default();
        let mut state = self.state.borrow_mut();
        state.detail = format!(
            "REVIEW ACTION\n{}\n{}\n\n/confirm to apply · /discard to dismiss",
            safe_text_str(path),
            raw_detail(Some(&staged_value))
        );
        state.rows = vec![text_row(
            "pending",
            "action",
            title,
            Some(safe_text_str(path)),
            fields(Some(&staged_value), FIELDS_MAXIMUM),
        )];
        state.view = "action".into();
    }

    /// `execute(input)`: runs one command or chat message; `Wait for the current command` while
    /// busy; clears `busy` and notifies on every exit path (controller §7.29, §8).
    pub async fn execute(self: &Rc<Self>, input: &str) -> Result<(), String> {
        let (generation, previous) = {
            let mut state = self.state.borrow_mut();
            if state.busy {
                return Err("Wait for the current command".into());
            }
            state.busy = true;
            state.error.clear();
            (
                self.generation(),
                (state.view.clone(), state.detail.clone(), state.rows.clone()),
            )
        };
        let result = self.run(js_trim(input)).await;
        if let Err(error) = &result
            && generation == self.generation()
        {
            let mut state = self.state.borrow_mut();
            (state.view, state.detail, state.rows) = previous;
            state.error = safe_text_str(error);
        }
        self.state.borrow_mut().busy = false;
        self.notify();
        result
    }

    /// `run(input)`: the command table (controller §8).
    async fn run(self: &Rc<Self>, input: &str) -> Result<(), String> {
        if input.is_empty() {
            return Ok(());
        }
        let generation = self.generation();
        let current = || generation == self.generation();
        let request = |path: &str, body: Option<JsObject>, timeout: Option<u64>| {
            let future = self.api.request(path, body.map(JsValue::Object), timeout);
            async move { future.await.map_err(|error| error.to_string()) }
        };
        if !input.starts_with('/') {
            self.require_project()?;
            let mut body = JsObject::new();
            body.set("projectId", self.state.borrow().project_id.as_str().into());
            body.set("message", input.into());
            body.set("idempotencyKey", format!("tui-chat-{}", uuid()).into());
            if let Some(selection) = &self.state.borrow().selection {
                selection.spread_into(&mut body);
            }
            request("/chat", Some(body), Some(CHAT_TIMEOUT)).await?;
            if current() {
                self.state.borrow_mut().view = "chat".into();
            }
            return Ok(());
        }
        let words = js_split_ws(input);
        let command = words[0];
        let args = &words[1..];
        let arg = |index: usize| args.get(index).copied();
        let rest = js_trim(&input[command.len()..]);
        match command {
            "/help" => {
                let mut state = self.state.borrow_mut();
                state.view = "help".into();
                state.detail = HELP.into();
                state.rows.clear();
            }
            "/chat" => self.state.borrow_mut().view = "chat".into(),
            "/map" => {
                let data = request(&format!("/research-map{}", self.query()?), None, None).await?;
                if !current() {
                    return Ok(());
                }
                {
                    let mut state = self.state.borrow_mut();
                    state.research_map = Some(data.clone());
                    state.map_requested_at = (self.now_ms)();
                }
                if arg(0) == Some("mermaid") {
                    let name = args[1..].join(" ");
                    let name = if name.is_empty() {
                        "research-map.md".into()
                    } else {
                        name
                    };
                    let file = path_resolve(&name);
                    let chart = research_map_mermaid(Some(&data));
                    std::fs::write(
                        &file,
                        format!("# NOSH research map\n\n```mermaid\n{chart}\n```\n"),
                    )
                    .map_err(|error| fs_error(&error, &file.display().to_string(), true))?;
                    let mut state = self.state.borrow_mut();
                    state.view = "paths".into();
                    state.rows.clear();
                    state.detail = format!(
                        "Wrote the research map as a Mermaid chart:\n{}\n\nOpen it in a Markdown preview (VS Code, GitHub) or paste the block into mermaid.live.",
                        file.display()
                    );
                    return Ok(());
                }
                let mut state = self.state.borrow_mut();
                state.view = "map".into();
                state.rows.clear();
                state.detail.clear();
            }
            "/projects" => {
                self.load_projects().await?;
                if !current() {
                    return Ok(());
                }
                let projects = self.state.borrow().projects.clone();
                let rows = projects
                    .iter()
                    .map(|project| {
                        text_row(
                            &project.project_id,
                            "projects",
                            project.project_id.clone(),
                            None,
                            vec![ViewField {
                                label: "repository".into(),
                                value: safe_text_str(&project.repository_root),
                            }],
                        )
                    })
                    .collect();
                let mut raw = JsObject::new();
                raw.set(
                    "projects",
                    JsValue::Array(projects.iter().map(JsValue::from).collect()),
                );
                self.present("projects", rows, &raw.into());
            }
            "/project" => self.select_project(rest, false)?,
            "/open" => {
                let body = match js_json_parse(rest) {
                    Ok(JsValue::Object(body))
                        if body.get("path").is_some_and(|v| v.as_str().is_some())
                            && body
                                .get("workingTitle")
                                .is_some_and(|v| v.as_str().is_some())
                            && body
                                .get("createRepository")
                                .and_then(JsValue::as_bool)
                                .is_some() =>
                    {
                        body
                    }
                    _ => return Err(OPEN_USAGE.into()),
                };
                let selection = self.state.borrow().selection.clone();
                let mut sent = body;
                if let Some(selection) = &selection {
                    selection.spread_into(&mut sent);
                }
                let reply = request("/projects/open", Some(sent), Some(CHAT_TIMEOUT)).await?;
                if reply.is_null() {
                    return Err(
                        "Cannot destructure property 'project' of '(intermediate value)' as it is null."
                            .into(),
                    );
                }
                if !current() {
                    return Ok(());
                }
                let project = match reply.get("project") {
                    missing @ (None | Some(JsValue::Null)) => {
                        return Err(property_error(missing, "projectId"));
                    }
                    Some(project) => project,
                };
                // TS appends a project without a string `projectId`, then cannot select it.
                if project.get("projectId").and_then(JsValue::as_str).is_none() {
                    return Err("Unknown project. Run /projects first.".into());
                }
                let project = project_from(project);
                {
                    let mut state = self.state.borrow_mut();
                    state
                        .projects
                        .retain(|known| known.project_id != project.project_id);
                    state.projects.push(project.clone());
                }
                self.select_project(&project.project_id, true)?;
                self.state.borrow_mut().selection = selection;
            }
            "/models" => {
                self.load_models().await?;
                if !current() {
                    return Ok(());
                }
                let models = self.state.borrow().models.clone();
                let rows = models
                    .iter()
                    .map(|model| {
                        let name = format!("{}/{}", model.provider, model.id);
                        text_row(
                            &name,
                            "models",
                            safe_text_str(&model.name),
                            Some(safe_text_str(&name)),
                            vec![ViewField {
                                label: "thinking".into(),
                                value: model
                                    .thinking_levels
                                    .iter()
                                    .map(|level| safe_text_str(level))
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            }],
                        )
                    })
                    .collect();
                // Deviation: the raw detail lists the four catalog fields the controller keeps;
                // TS lists every field the daemon sent. `detail` is only shown for help and paths.
                let mut raw = JsObject::new();
                raw.set(
                    "models",
                    JsValue::Array(models.iter().map(JsValue::from).collect()),
                );
                self.present("models", rows, &raw.into());
            }
            "/model" => {
                if rest == "default" {
                    self.state.borrow_mut().selection = None;
                    return Ok(());
                }
                if self.state.borrow().models.is_empty() {
                    self.load_models().await?;
                }
                if !current() {
                    return Ok(());
                }
                let model = self
                    .state
                    .borrow()
                    .models
                    .iter()
                    .find(|model| {
                        Some(model.provider.as_str()) == arg(0) && Some(model.id.as_str()) == arg(1)
                    })
                    .cloned()
                    .ok_or("Unknown model. Use /models.")?;
                // Omitted thinking uses "off" only when the provider advertises it.
                let levels = &model.thinking_levels;
                let level = arg(2).map_or_else(
                    || {
                        if levels.iter().any(|level| level == "off") {
                            "off".to_owned()
                        } else {
                            levels.first().cloned().unwrap_or_default()
                        }
                    },
                    str::to_owned,
                );
                if !levels.contains(&level) {
                    return Err(format!("Thinking must be one of: {}", levels.join(", ")));
                }
                self.state.borrow_mut().selection = Some(Selection {
                    model: ModelRef {
                        provider: model.provider.clone(),
                        id: model.id.clone(),
                    },
                    thinking_level: level,
                });
            }
            "/status" => {
                let query = self.query()?;
                if !rest.is_empty() && !STATUS_FAMILIES.contains(&rest) {
                    return Err("Use /status [missions|directions|autoresearch|agents]".into());
                }
                let families: Vec<&str> = if rest.is_empty() {
                    STATUS_FAMILIES.to_vec()
                } else {
                    vec![rest]
                };
                let requests: Vec<_> = families
                    .iter()
                    .map(|family| request(&format!("/{family}{query}"), None, None))
                    .collect();
                let loaded = try_join_all(requests).await?;
                if !current() {
                    return Ok(());
                }
                let mut rows = Vec::new();
                let mut raw = JsObject::new();
                for (family, data) in families.iter().zip(loaded) {
                    let family_rows = response_rows(family, Some(&data));
                    let mut private = self.private.borrow_mut();
                    private.set_section(family, family_rows.clone());
                    if *family == "agents" {
                        private.agent_metadata = agent_records(&data);
                    }
                    rows.extend(family_rows);
                    raw.set(*family, data);
                }
                self.present("status", rows, &raw.into());
            }
            "/jobs" => {
                let data = request(&format!("/jobs{}", self.query()?), None, None).await?;
                if !current() {
                    return Ok(());
                }
                let rows = response_rows("jobs", Some(&data));
                self.private.borrow_mut().set_section("jobs", rows.clone());
                self.present("jobs", rows, &data);
            }
            "/job" | "/tail" => {
                let id = arg(0).ok_or("Job ID required")?;
                let tail = command == "/tail";
                if tail && arg(1).is_some_and(|stream| stream != "stdout" && stream != "stderr") {
                    return Err("Stream must be stdout or stderr".into());
                }
                let path = format!(
                    "/jobs/{}{}{}{}",
                    encode_uri_component(id),
                    if tail { "/tail" } else { "" },
                    self.query()?,
                    if tail {
                        format!("&stream={}", arg(1).unwrap_or("stdout"))
                    } else {
                        String::new()
                    }
                );
                let data = request(&path, None, None).await?;
                if !current() {
                    return Ok(());
                }
                self.present("detail", response_rows("jobs", Some(&data)), &data);
            }
            "/export-paper" => {
                self.require_project()?;
                self.stage("/paper/export", JsObject::new());
            }
            "/cancel" | "/checkpoint" => {
                self.require_project()?;
                if rest.is_empty() || args.len() != 1 {
                    return Err("Job ID required".into());
                }
                self.stage(
                    &format!("/jobs/{}/{}", encode_uri_component(rest), &command[1..]),
                    JsObject::new(),
                );
            }
            "/approvals" => {
                let query = self.query()?;
                let project = self.state.borrow().project_id.clone();
                let contract = request(
                    &format!("/projects/{}/contract", encode_uri_component(&project)),
                    None,
                    None,
                );
                let proposals = request(&format!("/graph-proposals{query}"), None, None);
                let (contract, proposals) =
                    futures_util::future::try_join(contract, proposals).await?;
                if !current() {
                    return Ok(());
                }
                let approvals = response_rows("approvals", Some(&proposals));
                self.private
                    .borrow_mut()
                    .set_section("approvals", approvals.clone());
                let mut rows = response_rows("contract", Some(&contract));
                rows.extend(approvals);
                let mut raw = JsObject::new();
                raw.set("contract", contract);
                raw.set("proposals", proposals);
                self.present("approvals", rows, &raw.into());
            }
            "/approve" => {
                self.require_project()?;
                if args.len() != 2 {
                    return Err("Proposal ID and inspected version required".into());
                }
                let mut body = JsObject::new();
                body.set("expectedProposalVersion", version(arg(1))?.into());
                self.stage(
                    &format!("/graph-proposals/{}/approve", encode_uri_component(args[0])),
                    body,
                );
            }
            "/transition" => {
                self.require_project()?;
                if args.len() != 4 || !FAMILIES.contains(&args[0]) {
                    return Err("Use /transition <family> <id> <inspected-version> <state>".into());
                }
                let mut body = JsObject::new();
                body.set("expectedVersion", version(arg(2))?.into());
                body.set("next", args[3].into());
                self.stage(
                    &format!("/{}/{}/transition", args[0], encode_uri_component(args[1])),
                    body,
                );
            }
            "/start" => {
                self.require_project()?;
                if args.len() != 3 || !FAMILIES.contains(&args[0]) {
                    return Err(
                        "Use /start <missions|directions|autoresearch> <id> <inspected-version>"
                            .into(),
                    );
                }
                let mut body = JsObject::new();
                body.set("expectedVersion", version(arg(2))?.into());
                self.stage(
                    &format!("/{}/{}/start", args[0], encode_uri_component(args[1])),
                    body,
                );
            }
            "/retry" => {
                self.require_project()?;
                if args.len() != 4 || !["missions", "directions"].contains(&args[0]) {
                    return Err(
                        "Use /retry <missions|directions> <id> <inspected-version> <nodeId>".into(),
                    );
                }
                let mut body = JsObject::new();
                body.set("expectedVersion", version(arg(2))?.into());
                body.set("next", "ready".into());
                self.stage(
                    &format!(
                        "/{}/{}/nodes/{}/transition",
                        args[0],
                        encode_uri_component(args[1]),
                        encode_uri_component(args[3])
                    ),
                    body,
                );
            }
            "/steer" => {
                self.require_project()?;
                let message = js_split_ws(rest)
                    .get(2..)
                    .map_or_else(String::new, |words| words.join(" "));
                let message = js_trim(&message);
                if args.len() < 3 || message.is_empty() {
                    return Err("Use /steer <missionId> <inspected-version> <message>".into());
                }
                let mut body = JsObject::new();
                body.set("expectedVersion", version(arg(1))?.into());
                body.set("message", message.into());
                self.stage(
                    &format!("/missions/{}/steer", encode_uri_component(args[0])),
                    body,
                );
            }
            "/control" => {
                self.require_project()?;
                let mode = arg(3).unwrap_or("safe");
                if args.len() < 3
                    || args.len() > 4
                    || !["pause", "resume", "stop"].contains(&args[2])
                    || !["safe", "checkpoint", "immediate"].contains(&mode)
                {
                    return Err(
                        "Use /control <missionId> <version> <pause|resume|stop> [mode]".into(),
                    );
                }
                let mut body = JsObject::new();
                body.set("expectedVersion", version(arg(1))?.into());
                body.set("action", args[2].into());
                body.set("mode", mode.into());
                self.stage(
                    &format!("/missions/{}/control", encode_uri_component(args[0])),
                    body,
                );
            }
            "/create" => {
                self.require_project()?;
                let family = arg(0)
                    .filter(|family| FAMILIES.contains(family))
                    .ok_or("Use /create <missions|directions|autoresearch> <JSON file path>")?;
                let body = read_json_object(js_trim(&rest[family.len()..]))?;
                if current() {
                    self.stage(&format!("/{family}"), body);
                }
            }
            "/amend-contract" => {
                self.require_project()?;
                if rest.is_empty() {
                    return Err("Use /amend-contract <JSON file path>".into());
                }
                let contract = read_json_object(rest)?;
                if current() {
                    let project = self.state.borrow().project_id.clone();
                    let mut body = JsObject::new();
                    body.set("contract", contract.into());
                    self.stage(
                        &format!(
                            "/projects/{}/contract/amend",
                            encode_uri_component(&project)
                        ),
                        body,
                    );
                }
            }
            "/confirm" => {
                let pending = self
                    .private
                    .borrow_mut()
                    .pending
                    .take()
                    .ok_or("No staged action")?;
                if pending.body.get("projectId").and_then(JsValue::as_str)
                    != Some(self.state.borrow().project_id.as_str())
                {
                    return Err("Staged action belongs to another project".into());
                }
                let data = request(&pending.path, Some(pending.body), None).await?;
                if !current() {
                    return Ok(());
                }
                self.present("result", response_rows("result", Some(&data)), &data);
            }
            "/discard" => {
                let pending = self.private.borrow_mut().pending.take();
                self.state.borrow_mut().view = if pending.is_some_and(|pending| pending.from_map) {
                    "map"
                } else {
                    "chat"
                }
                .into();
            }
            "/paths" => {
                self.require_project()?;
                let mut state = self.state.borrow_mut();
                let root = state
                    .projects
                    .iter()
                    .find(|project| project.project_id == state.project_id)
                    .map_or_else(
                        || "undefined".to_owned(),
                        |project| project.repository_root.clone(),
                    );
                state.view = "paths".into();
                state.rows.clear();
                state.detail = format!(
                    "{root}\n\nOpen this repository in your external editor.\nPaper: docs/paper.md\nContract: .nosh/contracts/project.v<N>.json (active version in .nosh/project.json)\nUse /status and /job for daemon-owned artifact references.\nNo shell is embedded. Contract approval remains an explicit intake conversation."
                );
            }
            "/refresh" => {
                let view = self.state.borrow().view.clone();
                if ["status", "jobs", "approvals", "projects", "models", "map"]
                    .contains(&view.as_str())
                {
                    Box::pin(self.run(&format!("/{view}"))).await?;
                } else {
                    self.poll().await;
                    let _ = self.load_models().await;
                }
            }
            _ => return Err("Unknown command. Use /help.".into()),
        }
        Ok(())
    }

    /// `transcript`: the conversation as plain text (controller §7.31).
    pub fn transcript(&self) -> String {
        let text = self
            .transcript_entries()
            .iter()
            .map(|entry| {
                format!(
                    "{}  ·  {}{}\n{}",
                    entry.title,
                    entry.sequence.map_or_else(
                        || "live".to_owned(),
                        |number| format!("#{}", number_string(number))
                    ),
                    entry
                        .status
                        .as_ref()
                        .filter(|status| !status.is_empty())
                        .map_or_else(String::new, |status| format!(" · {status}")),
                    entry.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        non_empty_or(
            text,
            "What should we investigate?\n\nOpen a project with /open or choose one with /projects.\nType a message to continue project intake and research.",
        )
    }
}

/// `(Array.isArray(data.agents) ? data.agents : []).slice(0, 80).map(record)`.
fn agent_records(data: &JsValue) -> Vec<JsObject> {
    record(Some(data))
        .get("agents")
        .and_then(JsValue::as_array)
        .map_or_else(Vec::new, |agents| {
            agents
                .iter()
                .take(80)
                .map(|agent| record(Some(agent)).clone())
                .collect()
        })
}

/// `map.set(key, value)` on an insertion-ordered list: an existing key keeps its position.
fn ordered_set(entries: &mut Vec<(String, String)>, key: String, value: String) {
    match entries.iter_mut().find(|(name, _)| *name == key) {
        Some((_, slot)) => *slot = value,
        None => entries.push((key, value)),
    }
}

/// `value || fallback` for strings.
fn non_empty_or(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

/// `value || ''` kept as an option.
fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
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
