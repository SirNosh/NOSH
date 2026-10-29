//! The authorized routes of the local API, in the order `LocalApiServer.handle` tests them in
//! `apps/noshd/src/server.ts`: the order decides which route answers, and an error in any of them
//! is a 400 with its message.

use crate::LocalApiServer;
use http_body_util::BodyExt;
use hyper::Method;
use hyper::body::Incoming;
use nosh_orchestration::daemon::NoshDaemon;
use nosh_research::research_control::{
    AutoresearchInput, BaselineInput, DirectionInput, MissionInput,
};
use nosh_sys::jobs::{JobSpec, Stream};
use nosh_wire::js::{is_js_whitespace, js_number};
use serde_json::{Map, Value, json};
use std::fmt::Display;
use std::rc::Rc;

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// What a route answered with, and whether the daemon should stop once it is sent.
pub(crate) struct Reply {
    pub status: u16,
    pub body: Value,
    pub shutdown: bool,
}

fn reply(status: u16, body: Value) -> Result<Reply, String> {
    Ok(Reply {
        status,
        body,
        shutdown: false,
    })
}

/// A thrown error's message.
trait Thrown<T> {
    fn thrown(self) -> Result<T, String>;
}

impl<T, E: Display> Thrown<T> for Result<T, E> {
    fn thrown(self) -> Result<T, String> {
        self.map_err(|error| error.to_string())
    }
}

/// A request's parsed URL path (after `/api`), query, and unread body.
pub(crate) struct Request {
    pub method: Method,
    pub path: String,
    pub url: url::Url,
    pub body: Option<Incoming>,
}

impl Request {
    /// `url.searchParams.get(name)`.
    fn query(&self, name: &str) -> Option<String> {
        self.url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    /// `requiredQuery`: an empty value is as missing as an absent one.
    fn required_query(&self, name: &str) -> Result<String, String> {
        self.query(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{name} is required"))
    }

    /// `readJson`: the body as `JSON.parse` reads it, refused beyond 3 MB.
    async fn json(&mut self) -> Result<Value, String> {
        let Some(mut body) = self.body.take() else {
            return Ok(Value::Null);
        };
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.thrown()?;
            if let Ok(data) = frame.into_data() {
                if bytes.len() + data.len() > 3_000_000 {
                    return Err("request body exceeds 3 MB".into());
                }
                bytes.extend_from_slice(&data);
            }
        }
        nosh_agent::js_json::parse(&String::from_utf8_lossy(&bytes)).thrown()
    }

    /// The segments of a path matching `pattern`, where `*` stands for one non-empty segment
    /// (TypeScript's `[^/]+`).
    fn path_matches(&self, pattern: &str) -> Option<Vec<String>> {
        let (path, pattern): (Vec<&str>, Vec<&str>) =
            (self.path.split('/').collect(), pattern.split('/').collect());
        (path.len() == pattern.len()
            && path.iter().zip(&pattern).all(|(part, want)| {
                if *want == "*" {
                    !part.is_empty()
                } else {
                    part == want
                }
            }))
        .then(|| path.iter().map(|part| (*part).to_owned()).collect())
    }
}

/// `body.name`: JSON `null` has no properties, so reading one throws as JavaScript does.
fn field<'a>(body: &'a Value, name: &str) -> Result<Option<&'a Value>, String> {
    match body {
        Value::Null => Err(format!("Cannot read properties of null (reading '{name}')")),
        Value::Object(map) => Ok(map.get(name)),
        _ => Ok(None),
    }
}

/// A string field `requiredStrings` has already checked.
fn text(body: &Value, name: &str) -> String {
    body[name].as_str().unwrap_or_default().to_owned()
}

/// `requiredStrings`: each name is a non-blank string; idempotency keys have at least 16 UTF-16
/// units.
fn required_strings(body: &Value, names: &[&str]) -> Result<(), String> {
    for name in names {
        let value = field(body, name)?.and_then(Value::as_str);
        let blank = value.is_none_or(|value| value.trim_matches(is_js_whitespace).is_empty());
        let short = *name == "idempotencyKey"
            && value.is_some_and(|value| value.encode_utf16().count() < 16);
        if blank || short {
            return Err(if *name == "idempotencyKey" && !blank {
                "idempotencyKey must be at least 16 characters".into()
            } else {
                format!("{name} is required")
            });
        }
    }
    Ok(())
}

/// `Number.isInteger(body.name)`, as the integer.
fn integer(body: &Value, name: &str) -> Result<Option<i64>, String> {
    #[allow(clippy::cast_possible_truncation)]
    Ok(field(body, name)?
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && number.fract() == 0.0)
        .map(|number| number as i64))
}

fn required_integer(body: &Value, name: &str) -> Result<i64, String> {
    integer(body, name)?.ok_or_else(|| format!("{name} is required"))
}

fn is_object(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_object)
}

fn is_array(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_array)
}

/// `Number.isSafeInteger`.
pub(crate) fn is_safe_integer(number: f64) -> bool {
    number.is_finite() && number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0
}

/// `parseModelSelection`: a request's model selection, with a top-level `thinkingLevel` that
/// must agree with a nested one.
pub fn parse_model_selection(
    value: Option<&Value>,
    top_level_thinking_level: Option<&Value>,
) -> Result<Option<Value>, String> {
    let Some(value) = value else {
        if top_level_thinking_level.is_some() {
            return Err("thinkingLevel requires a selected Pi model".into());
        }
        return Ok(None);
    };
    let levels = THINKING_LEVELS.join(", ");
    let level = |value: &Value| {
        value
            .as_str()
            .filter(|level| THINKING_LEVELS.contains(level))
            .map(str::to_owned)
    };
    let trimmed = |name: &str| {
        value[name]
            .as_str()
            .map(|text| text.trim_matches(is_js_whitespace))
            .filter(|text| !text.is_empty())
    };
    let parsed = value
        .as_object()
        .filter(|map| {
            map.keys()
                .all(|key| ["provider", "id", "thinkingLevel"].contains(&key.as_str()))
        })
        .and_then(|map| {
            let nested = match map.get("thinkingLevel") {
                None => None,
                Some(value) => Some(level(value)?),
            };
            Some((trimmed("provider")?, trimmed("id")?, nested))
        });
    let Some((provider, id, nested)) = parsed else {
        return Err(format!(
            "Pi model selection must include provider and id, with thinkingLevel one of {levels}"
        ));
    };
    let mut selection = json!({ "provider": provider, "id": id });
    let thinking_level = match top_level_thinking_level {
        None => nested,
        Some(top) => {
            let top = level(top).ok_or_else(|| format!("thinkingLevel must be one of {levels}"))?;
            if nested.as_ref().is_some_and(|nested| *nested != top) {
                return Err("Nested and top-level thinkingLevel selections must match".into());
            }
            Some(top)
        }
    };
    if let Some(level) = thinking_level {
        selection["thinkingLevel"] = json!(level);
    }
    Ok(Some(selection))
}

/// `publicGraphNodeState`.
fn public_graph_node_state(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|state| matches!(state, "ready" | "blocked" | "cancelled"))
}

/// `decodeURIComponent`.
fn decode_uri_component(text: &str) -> Result<String, String> {
    let malformed = || "URI malformed".to_owned();
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = tail.get(..2).ok_or_else(malformed)?;
            let hex = std::str::from_utf8(hex).map_err(|_| malformed())?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|_| malformed())?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).map_err(|_| malformed())
}

/// A typed input built from request fields; a mismatched type is refused with serde's message.
fn input<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).thrown()
}

/// Copies the fields of `body` that are present (TypeScript's `...(x === undefined ? {} : { x })`).
fn present(body: &Value, names: &[&str], into: &mut Map<String, Value>) {
    for name in names {
        if let Some(value) = body.get(*name) {
            into.insert((*name).to_owned(), value.clone());
        }
    }
}

/// Fails as TypeScript does when a scope-checked call names an unregistered Project.
fn assert_registered(daemon: &NoshDaemon, project_id: Option<&Value>) -> Result<(), String> {
    let registered = daemon.projects().thrown()?;
    if !registered
        .iter()
        .any(|project| Some(&project["projectId"]) == project_id)
    {
        return Err(format!(
            "Project {} is not registered on this host",
            nosh_wire::js::string(project_id)
        ));
    }
    Ok(())
}

impl LocalApiServer {
    pub(crate) async fn route(&self, mut request: Request) -> Result<Reply, String> {
        let daemon = Rc::clone(&self.daemon);
        let research = &daemon.research;
        let (get, post, put) = (
            request.method == Method::GET,
            request.method == Method::POST,
            request.method == Method::PUT,
        );
        let path = request.path.clone();
        let path = path.as_str();

        if get && path == "/projects" {
            return reply(200, json!({ "projects": daemon.projects().thrown()? }));
        }
        if post && let Some(parts) = request.path_matches("/projects/*/contract/amend") {
            let body = request.json().await?;
            required_strings(&body, &["idempotencyKey"])?;
            let contract = daemon
                .amend_project_contract(
                    &parts[2],
                    body.get("contract").unwrap_or(&Value::Null),
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(200, json!({ "contract": contract }));
        }
        if get && let Some(parts) = request.path_matches("/projects/*/contract") {
            let contract = daemon.project_contract(&parts[2]).thrown()?;
            return reply(200, json!({ "contract": contract }));
        }
        if get && path == "/models" {
            let models = daemon.agents.available_models().await?;
            return reply(
                200,
                json!({ "models": models, "defaultModel": daemon.agents.default_model() }),
            );
        }
        if post && path == "/projects" {
            let project = request.json().await?;
            let path_argument = |index: usize, name: &str| -> Result<String, String> {
                match field(&project, name)? {
                    Some(Value::String(path)) => Ok(path.clone()),
                    other => Err(format!(
                        "The \"paths[{index}]\" argument must be of type string. Received {}",
                        received(other)
                    )),
                }
            };
            let project_id = field(&project, "projectId")?.and_then(Value::as_str);
            if let Some(project_id) = project_id {
                daemon.assert_project_writable(project_id).thrown()?;
            }
            let repository_root = path_argument(0, "repositoryRoot")?;
            let database_path = path_argument(0, "databasePath")?;
            let project_id = project_id.ok_or("projectId must be a string")?;
            let registered = daemon
                .register_project(project_id, repository_root.as_ref(), database_path.as_ref())
                .thrown()?;
            return reply(201, json!({ "project": registered }));
        }
        if post && path == "/projects/open" {
            let body = request.json().await?;
            required_strings(&body, &["path", "workingTitle"])?;
            let github = field(&body, "githubRepositoryUrl")?;
            if !field(&body, "createRepository")?.is_some_and(Value::is_boolean)
                || github.is_some_and(|url| !url.is_string())
            {
                return Err("repository mode or GitHub link is invalid".into());
            }
            let selection =
                parse_model_selection(field(&body, "model")?, field(&body, "thinkingLevel")?)?;
            daemon
                .validate_model_selection(selection.as_ref())
                .await
                .thrown()?;
            let github = github.and_then(Value::as_str).filter(|url| !url.is_empty());
            let project = daemon
                .initialize_project(
                    text(&body, "path").as_ref(),
                    body["createRepository"] == true,
                    Some(&text(&body, "workingTitle")),
                    github,
                )
                .thrown()?;
            let project_id = nosh_wire::js::string(project.get("projectId"));
            let intake = Rc::clone(&daemon);
            tokio::task::spawn_local(async move {
                let _ = intake
                    .begin_project_intake(&project_id, selection.as_ref())
                    .await;
            });
            return reply(201, json!({ "project": project }));
        }
        if get && path == "/events" {
            let project_id = request.query("projectId").filter(|id| !id.is_empty());
            let after = js_number(&request.query("after").unwrap_or_else(|| "0".into()));
            let Some(project_id) = project_id.filter(|_| is_safe_integer(after) && after >= 0.0)
            else {
                return Err("projectId and a non-negative after cursor are required".into());
            };
            let limit = js_number(&request.query("limit").unwrap_or_else(|| "300".into()));
            let recent = request.query("recent");
            if recent
                .as_deref()
                .is_some_and(|recent| recent != "true" && recent != "false")
            {
                return Err("recent must be true or false".into());
            }
            // A limit that is not a safe integer fails the store's range check, as in TypeScript.
            #[allow(clippy::cast_possible_truncation)]
            let limit = if is_safe_integer(limit) {
                limit as i64
            } else {
                0
            };
            #[allow(clippy::cast_possible_truncation)]
            let page = daemon
                .replay_page(
                    &project_id,
                    after as i64,
                    limit,
                    recent.as_deref() == Some("true"),
                )
                .thrown()?;
            return reply(
                200,
                json!({ "events": page.events, "nextCursor": page.next_cursor, "hasMore": page.has_more }),
            );
        }
        if post && path == "/chat" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "message", "idempotencyKey"])?;
            let selection =
                parse_model_selection(field(&body, "model")?, field(&body, "thinkingLevel")?)?;
            let receipt = daemon
                .chat(
                    &text(&body, "projectId"),
                    &text(&body, "message"),
                    &text(&body, "idempotencyKey"),
                    selection.as_ref(),
                )
                .await
                .thrown()?;
            return reply(
                202,
                json!({ "event": receipt.event, "agentId": receipt.agent_id, "replayed": receipt.replayed }),
            );
        }
        if post && path == "/runtime/instructions" {
            let body = request.json().await?;
            return reply(202, daemon.runtime.execute(&body).await.thrown()?);
        }
        if get && path == "/threads" {
            let threads = daemon
                .runtime
                .threads(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "threads": threads }));
        }
        if get && let Some(parts) = request.path_matches("/threads/*") {
            let thread = daemon
                .runtime
                .thread(&request.required_query("projectId")?, &parts[2])
                .thrown()?;
            return reply(200, json!({ "thread": thread }));
        }
        if post && let Some(parts) = request.path_matches("/threads/*/rotate") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let thread = daemon
                .runtime
                .rotate(
                    &text(&body, "projectId"),
                    &parts[2],
                    &text(&body, "idempotencyKey"),
                )
                .await
                .thrown()?;
            return reply(202, json!({ "thread": thread }));
        }
        if post && let Some(parts) = request.path_matches("/threads/*/messages") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "message", "idempotencyKey"])?;
            daemon
                .runtime
                .fork_message(
                    &text(&body, "projectId"),
                    &parts[2],
                    &text(&body, "message"),
                    &text(&body, "idempotencyKey"),
                )
                .await
                .thrown()?;
            return reply(202, json!({ "accepted": true }));
        }
        if get && path == "/episodes" {
            let episodes = daemon
                .runtime
                .episodes(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "episodes": episodes }));
        }
        if get && let Some(parts) = request.path_matches("/episodes/*/trace") {
            let events = daemon
                .runtime
                .trace(&request.required_query("projectId")?, &parts[2])
                .thrown()?;
            return reply(200, json!({ "events": events }));
        }
        if post && path == "/skills" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let skill = daemon
                .runtime
                .register_skill(
                    &text(&body, "projectId"),
                    body.get("manifest").unwrap_or(&Value::Null),
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(201, json!({ "skill": skill }));
        }
        if get && path == "/skills" {
            let skills = daemon
                .runtime
                .skills(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "skills": skills }));
        }
        if post && path == "/programs" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let program = daemon
                .runtime
                .register_program(
                    &text(&body, "projectId"),
                    body.get("program").unwrap_or(&Value::Null),
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(201, json!({ "program": program }));
        }
        if get && path == "/programs" {
            let programs = daemon
                .runtime
                .program_records(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "programs": programs }));
        }
        if get && path == "/programs/states" {
            let states = daemon
                .runtime
                .program_states(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "states": states }));
        }
        if post && let Some(parts) = request.path_matches("/programs/*/run") {
            let body = request.json().await?;
            required_strings(&body, &["projectId"])?;
            let results = daemon
                .runtime
                .run_program(&text(&body, "projectId"), &parts[2])
                .await
                .thrown()?;
            return reply(202, json!({ "results": results }));
        }
        if get && path == "/missions" {
            let missions = research
                .missions(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "missions": missions }));
        }
        if post && path == "/missions" {
            let body = request.json().await?;
            required_strings(
                &body,
                &["projectId", "title", "objective", "idempotencyKey"],
            )?;
            let optional_array = |name: &str| -> Result<bool, String> {
                Ok(field(&body, name)?
                    .is_some_and(|value| nosh_wire::js::truthy(Some(value)) && !value.is_array()))
            };
            if !is_array(field(&body, "deliverables")?)
                || !is_array(field(&body, "successCriteria")?)
                || optional_array("nonObjectives")?
                || optional_array("startingEvidence")?
            {
                return Err("Mission deliverables and success criteria are required arrays".into());
            }
            let or_empty = |name: &str| match body.get(name) {
                None | Some(Value::Null) => json!([]),
                Some(value) => value.clone(),
            };
            let mut mission = Map::new();
            present(
                &body,
                &[
                    "title",
                    "objective",
                    "deliverables",
                    "successCriteria",
                    "idempotencyKey",
                ],
                &mut mission,
            );
            mission.insert("nonObjectives".into(), or_empty("nonObjectives"));
            mission.insert("startingEvidence".into(), or_empty("startingEvidence"));
            present(&body, &["researchPlan", "budgets"], &mut mission);
            let mission: MissionInput = input(Value::Object(mission))?;
            let created = research
                .create_mission(&text(&body, "projectId"), &mission)
                .thrown()?;
            return reply(201, json!({ "mission": created }));
        }
        if get && let Some(parts) = request.path_matches("/missions/*/completion") {
            let project_id = request.required_query("projectId")?;
            let mission_id = &parts[2];
            let packet = research
                .submitted(&project_id, None)
                .thrown()?
                .into_iter()
                .rev()
                .find(|event| {
                    event["scope"]["missionId"] == mission_id.as_str()
                        && event["payload"]["$schema"]
                            == "https://nosh.dev/schemas/mission-completion-packet/v1"
                })
                .map_or(Value::Null, |event| event["payload"].clone());
            let basis = research
                .mission_completion_basis(&project_id, mission_id)
                .thrown()?;
            return reply(200, json!({ "basis": basis, "packet": packet }));
        }
        if get && let Some(parts) = request.path_matches("/missions/*") {
            let mission = research
                .mission(&request.required_query("projectId")?, &parts[2])
                .thrown()?;
            return reply(200, json!({ "mission": mission }));
        }
        if post && let Some(parts) = request.path_matches("/missions/*/steer") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "message", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            daemon
                .steer_mission(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &text(&body, "message"),
                    &text(&body, "idempotencyKey"),
                )
                .await
                .thrown()?;
            return reply(202, json!({ "accepted": true }));
        }
        if post && let Some(parts) = request.path_matches("/missions/*/control") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "action", "mode", "idempotencyKey"])?;
            let (action, mode) = (text(&body, "action"), text(&body, "mode"));
            let expected = integer(&body, "expectedVersion")?;
            let Some(expected) = expected.filter(|_| {
                ["pause", "resume", "stop"].contains(&action.as_str())
                    && ["safe", "checkpoint", "immediate"].contains(&mode.as_str())
            }) else {
                return Err("expectedVersion, action, and mode are required".into());
            };
            let mission = daemon
                .control_mission(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &action,
                    &mode,
                    &text(&body, "idempotencyKey"),
                )
                .await
                .thrown()?;
            return reply(202, json!({ "mission": mission }));
        }
        if post
            && let Some(parts) = ["missions", "directions", "autoresearch"]
                .iter()
                .find_map(|family| request.path_matches(&format!("/{family}/*/start")))
        {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            let family = parts[1].as_str();
            let started = research
                .start(
                    &text(&body, "projectId"),
                    family,
                    &parts[2],
                    expected,
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            let key = match family {
                "missions" => "mission",
                "directions" => "direction",
                _ => "execution",
            };
            return reply(202, json!({ key: started }));
        }
        if post && let Some(parts) = request.path_matches("/missions/*/transition") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "next", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            let next = text(&body, "next");
            if next == "stopping" || next == "pausing" {
                let verb = if next == "stopping" { "stop" } else { "pause" };
                return Err(format!(
                    "Use the Mission control action to {verb} it (TUI: /control <missionId> <version> {verb}); it also cancels Jobs and ends sessions, which a bare transition would leave running"
                ));
            }
            let mission = research
                .transition_mission(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &next,
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "mission": mission }));
        }
        if post && let Some(parts) = request.path_matches("/missions/*/graph") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "rationale", "idempotencyKey"])?;
            let (expected, base) = (
                integer(&body, "expectedVersion")?,
                integer(&body, "baseGraphVersion")?,
            );
            let (Some(expected), Some(base), true, true) = (
                expected,
                base,
                is_array(field(&body, "operations")?),
                is_array(field(&body, "evidenceIds")?),
            ) else {
                return Err(
                    "expectedVersion, baseGraphVersion, operations, and evidenceIds are required"
                        .into(),
                );
            };
            let evidence_ids: Vec<String> = input(body["evidenceIds"].clone())?;
            let mission = research
                .mutate_mission_graph(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    base,
                    &body["operations"],
                    &text(&body, "rationale"),
                    &evidence_ids,
                    &text(&body, "idempotencyKey"),
                    false,
                )
                .thrown()?;
            return reply(202, json!({ "mission": mission }));
        }
        if post && let Some(parts) = request.path_matches("/missions/*/nodes/*/transition") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "next", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            if !public_graph_node_state(field(&body, "next")?) || body.get("lease").is_some() {
                return Err(
                    "Public graph transitions allow only ready, blocked, or cancelled without a lease"
                        .into(),
                );
            }
            let mission = research
                .transition_mission_node(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &parts[4],
                    &text(&body, "next"),
                    &text(&body, "idempotencyKey"),
                    None,
                    true,
                )
                .thrown()?;
            return reply(202, json!({ "mission": mission }));
        }
        if post && let Some(parts) = request.path_matches("/graph-proposals/*/approve") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedProposalVersion")?;
            let proposal = research
                .approve_graph_proposal(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "proposal": proposal }));
        }
        if get && path == "/graph-proposals" {
            let proposals = research
                .graph_proposals(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "proposals": proposals }));
        }
        if put && path == "/default-model" {
            let body = request.json().await?;
            let Some(Value::String(model)) = field(&body, "model")? else {
                return Err("model (provider/id[:level]) is required".into());
            };
            let selection = daemon.set_default_model(model).await.thrown()?;
            return reply(200, json!({ "defaultModel": selection }));
        }
        if get && path == "/research-map" {
            let map = daemon
                .research_map(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, map);
        }
        if get && path == "/directions" {
            let directions = research
                .directions(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "directions": directions }));
        }
        if post && path == "/directions" {
            let body = request.json().await?;
            required_strings(
                &body,
                &["projectId", "question", "decisionUse", "idempotencyKey"],
            )?;
            let mut direction = Map::new();
            present(
                &body,
                &["question", "decisionUse", "idempotencyKey"],
                &mut direction,
            );
            direction.insert(
                "missionId".into(),
                body.get("missionId").cloned().unwrap_or(Value::Null),
            );
            direction.insert(
                "evaluationContract".into(),
                match body.get("evaluationContract") {
                    None | Some(Value::Null) => {
                        json!({ "metrics": [], "datasets": [], "seeds": [0] })
                    }
                    Some(contract) => contract.clone(),
                },
            );
            let direction: DirectionInput = input(Value::Object(direction))?;
            let created = research
                .create_direction(&text(&body, "projectId"), &direction, None)
                .thrown()?;
            return reply(201, json!({ "direction": created }));
        }
        if post && let Some(parts) = request.path_matches("/directions/*/transition") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "next", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            let direction = research
                .transition_direction(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &text(&body, "next"),
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "direction": direction }));
        }
        if post && let Some(parts) = request.path_matches("/directions/*/baseline") {
            let body = request.json().await?;
            required_strings(
                &body,
                &[
                    "projectId",
                    "commit",
                    "reviewId",
                    "evaluationContractHash",
                    "idempotencyKey",
                ],
            )?;
            let expected = required_integer(&body, "expectedVersion")?;
            let baseline = BaselineInput {
                commit: text(&body, "commit"),
                review_id: text(&body, "reviewId"),
                evaluation_contract_hash: text(&body, "evaluationContractHash"),
                idempotency_key: text(&body, "idempotencyKey"),
            };
            let direction = research
                .accept_direction_baseline(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &baseline,
                )
                .thrown()?;
            return reply(202, json!({ "direction": direction }));
        }
        if post && let Some(parts) = request.path_matches("/directions/*/nodes/*/transition") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "next", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            if !public_graph_node_state(field(&body, "next")?) || body.get("lease").is_some() {
                return Err(
                    "Public graph transitions allow only ready, blocked, or cancelled without a lease"
                        .into(),
                );
            }
            let direction = research
                .transition_direction_node(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &parts[4],
                    &text(&body, "next"),
                    &text(&body, "idempotencyKey"),
                    None,
                    true,
                )
                .thrown()?;
            return reply(202, json!({ "direction": direction }));
        }
        if get && path == "/autoresearch" {
            let executions = research
                .autoresearch(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "executions": executions }));
        }
        if post && path == "/autoresearch" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "decisionQuestion", "idempotencyKey"])?;
            let not_array = |name: &str| -> Result<bool, String> {
                Ok(field(&body, name)?
                    .is_some_and(|value| nosh_wire::js::truthy(Some(value)) && !value.is_array()))
            };
            if not_array("familyTags")? || not_array("scope")? {
                return Err("familyTags and scope must be arrays".into());
            }
            let mut execution = Map::new();
            present(
                &body,
                &["decisionQuestion", "idempotencyKey"],
                &mut execution,
            );
            for (name, fallback) in [
                ("directionId", Value::Null),
                ("missionId", Value::Null),
                ("familyTags", json!([])),
                ("scope", json!([])),
            ] {
                execution.insert(
                    name.into(),
                    match body.get(name) {
                        None | Some(Value::Null) => fallback,
                        Some(value) => value.clone(),
                    },
                );
            }
            present(
                &body,
                &[
                    "evaluationContract",
                    "maximumExperiments",
                    "maximumRounds",
                    "maximumWallClockSeconds",
                    "maximumModelTokens",
                    "maximumGpuSeconds",
                    "maximumDiskBytes",
                ],
                &mut execution,
            );
            let execution: AutoresearchInput = input(Value::Object(execution))?;
            let created = research
                .create_autoresearch(&text(&body, "projectId"), &execution, None)
                .thrown()?;
            return reply(201, json!({ "execution": created }));
        }
        if post && let Some(parts) = request.path_matches("/autoresearch/*/transition") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "next", "idempotencyKey"])?;
            let expected = required_integer(&body, "expectedVersion")?;
            let execution = research
                .transition_autoresearch(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &text(&body, "next"),
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "execution": execution }));
        }
        if get && path == "/records" {
            let project_id = request.required_query("projectId")?;
            let records = research
                .records(&project_id, request.query("schema").as_deref())
                .thrown()?;
            return reply(200, json!({ "records": records }));
        }
        if get && path == "/reviews" {
            let reviews = research
                .reviews(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "reviews": reviews }));
        }
        if get && path == "/evidence" {
            let evidence = research
                .records(&request.required_query("projectId")?, Some("evidence"))
                .thrown()?;
            return reply(200, json!({ "evidence": evidence }));
        }
        if post && path == "/evidence" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            if !is_object(field(&body, "evidence")?) {
                return Err("evidence must be an object".into());
            }
            let event = research
                .submit_user_evidence(
                    &text(&body, "projectId"),
                    &body["evidence"],
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(201, json!({ "event": event }));
        }
        if get && path == "/claims" {
            let claims = research
                .latest_claims(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "claims": claims }));
        }
        if get && path == "/notification-acks" {
            let event_ids = daemon
                .notification_acks(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "eventIds": event_ids }));
        }
        if post && path == "/notifications/acknowledge" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let event_ids: Option<Vec<String>> =
                field(&body, "eventIds")?.and_then(|ids| serde_json::from_value(ids.clone()).ok());
            let Some(event_ids) = event_ids else {
                return Err("eventIds must be an array of strings".into());
            };
            let event = daemon
                .acknowledge_notifications(
                    &text(&body, "projectId"),
                    &event_ids,
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "event": event }));
        }
        if post && path == "/claims" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            if !is_object(field(&body, "claim")?) {
                return Err("claim must be an object".into());
            }
            let event = research
                .submit_user_claim(
                    &text(&body, "projectId"),
                    &body["claim"],
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(201, json!({ "event": event }));
        }
        if put && let Some(parts) = request.path_matches("/claims/*") {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let expected = field(&body, "expectedClaimVersion")?
                .and_then(Value::as_f64)
                .filter(|number| number.is_finite() && number.fract() == 0.0);
            let (Some(expected), true) = (expected, is_object(field(&body, "changes")?)) else {
                return Err("expectedClaimVersion and changes object are required".into());
            };
            let event = research
                .edit_user_claim(
                    &text(&body, "projectId"),
                    &parts[2],
                    expected,
                    &body["changes"],
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "event": event }));
        }
        if get && path == "/paper" {
            let paper = research
                .read_paper(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, paper);
        }
        if put && path == "/paper" {
            let body = request.json().await?;
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            let expected =
                field(&body, "expected")?.filter(|expected| nosh_wire::js::truthy(Some(expected)));
            let hashes_present = expected.is_some_and(|expected| {
                ["markdownHash", "bibliographyHash", "version"]
                    .iter()
                    .all(|name| expected.get(*name).is_some_and(Value::is_string))
            });
            let (Some(Value::String(markdown)), Some(Value::String(bibliography)), true) = (
                field(&body, "markdown")?,
                field(&body, "bibliography")?,
                hashes_present,
            ) else {
                return Err(
                    "paper content and expected paper hashes and version are required".into(),
                );
            };
            let event = research
                .save_paper(
                    &text(&body, "projectId"),
                    markdown,
                    bibliography,
                    &body["expected"],
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "event": event }));
        }
        if post && path == "/paper/export" {
            let body = request.json().await?;
            required_strings(&body, &["projectId"])?;
            return reply(
                200,
                research.export_paper(&text(&body, "projectId")).thrown()?,
            );
        }
        if get && path == "/backups" {
            let backups = daemon
                .backups(&request.required_query("projectId")?)
                .thrown()?;
            return reply(200, json!({ "backups": backups }));
        }
        if post && path == "/backups" {
            let body = request.json().await?;
            required_strings(&body, &["projectId"])?;
            let backup = daemon.create_backup(&text(&body, "projectId")).thrown()?;
            return reply(201, json!({ "backup": backup }));
        }
        if post && let Some(parts) = request.path_matches("/backups/*/restore") {
            let body = request.json().await?;
            required_strings(&body, &["projectId"])?;
            daemon
                .schedule_backup_restore(
                    &text(&body, "projectId"),
                    &decode_uri_component(&parts[2])?,
                )
                .thrown()?;
            return Ok(Reply {
                status: 202,
                body: json!({ "scheduled": true, "stopping": true }),
                shutdown: true,
            });
        }
        if get && path == "/activity" {
            let active: Vec<Value> = daemon
                .activity()
                .thrown()?
                .into_iter()
                .map(|(project_id, active)| json!({ "projectId": project_id, "active": active }))
                .collect();
            return reply(200, json!({ "active": active }));
        }
        if post && path == "/shutdown" {
            if daemon.backup_maintenance_in_progress() {
                return Err("Shutdown is unavailable while a backup is in progress".into());
            }
            return Ok(Reply {
                status: 202,
                body: json!({ "stopping": true }),
                shutdown: true,
            });
        }
        if get && path == "/agents" {
            let project_id = request.query("projectId").filter(|id| !id.is_empty());
            let agents: Vec<_> = daemon
                .agents
                .inspect()
                .into_iter()
                .filter(|agent| project_id.as_ref().is_none_or(|id| agent.project_id == *id))
                .collect();
            return reply(200, json!({ "agents": agents }));
        }
        if post && path == "/agents" {
            let options = request.json().await?;
            assert_registered(&daemon, field(&options, "projectId")?)?;
            daemon
                .assert_project_writable(options["projectId"].as_str().unwrap_or_default())
                .thrown()?;
            let agent = daemon.start_agent(input(options)?).await.thrown()?;
            return reply(201, json!({ "agent": agent }));
        }
        if post && path.starts_with("/agents/") {
            let parts: Vec<&str> = path.split('/').collect();
            let (agent_id, action) = (
                parts.get(2).copied().unwrap_or_default(),
                parts.get(3).copied().unwrap_or_default(),
            );
            if agent_id.is_empty() || action.is_empty() {
                return Err("agent and action are required".into());
            }
            let agent = daemon
                .agents
                .inspect()
                .into_iter()
                .find(|agent| agent.agent_id == agent_id)
                .ok_or("unknown agent")?;
            daemon.assert_project_writable(&agent.project_id).thrown()?;
            let body = request.json().await?;
            let message = |body: &Value| -> Result<Option<String>, String> {
                Ok(field(body, "message")?
                    .filter(|message| nosh_wire::js::truthy(Some(message)))
                    .map(|message| nosh_wire::js::string(Some(message))))
            };
            let agents = &daemon.agents;
            if action == "prompt"
                && let Some(message) = message(&body)?
            {
                agents.prompt(agent_id, &message, None).await?;
            } else if action == "steer"
                && let Some(message) = message(&body)?
            {
                agents.steer(agent_id, &message).await?;
            } else if action == "compact" {
                let instructions = field(&body, "instructions")?
                    .filter(|instructions| !instructions.is_null())
                    .map(|instructions| nosh_wire::js::string(Some(instructions)));
                agents.compact(agent_id, instructions.as_deref()).await?;
            } else if action == "abort" {
                agents.abort(agent_id).await?;
            } else if action == "stop" {
                agents.stop(agent_id);
            } else {
                return Err("unsupported agent action".into());
            }
            return reply(202, json!({ "accepted": true }));
        }
        if get && path == "/jobs" {
            let project_id = request.query("projectId").filter(|id| !id.is_empty());
            let jobs: Vec<_> = daemon
                .jobs
                .list()
                .thrown()?
                .into_iter()
                .filter(|job| {
                    project_id
                        .as_ref()
                        .is_none_or(|id| job.spec.project_id == *id)
                })
                .collect();
            return reply(200, json!({ "jobs": jobs }));
        }
        if post && path == "/jobs" {
            let spec = request.json().await?;
            assert_registered(&daemon, field(&spec, "projectId")?)?;
            daemon
                .assert_project_writable(spec["projectId"].as_str().unwrap_or_default())
                .thrown()?;
            let spec: JobSpec = input(spec)?;
            return reply(201, json!({ "job": daemon.start_job(&spec).thrown()? }));
        }
        if get && path.starts_with("/jobs/") {
            let parts: Vec<&str> = path.split('/').collect();
            let (job_id, action) = (
                parts.get(2).copied().unwrap_or_default(),
                parts.get(3).copied(),
            );
            let project_id = request.required_query("projectId")?;
            if job_id.is_empty() {
                return Err("job is required".into());
            }
            let job = daemon.jobs.get(job_id).thrown()?;
            if job.spec.project_id != project_id {
                return Err("Job is outside the requested Project".into());
            }
            if action == Some("tail") {
                let stream = if request.query("stream").as_deref() == Some("stderr") {
                    Stream::Stderr
                } else {
                    Stream::Stdout
                };
                let text = daemon.jobs.tail(job_id, stream, 64 * 1024).thrown()?;
                return reply(200, json!({ "text": text }));
            }
            let snapshot = daemon.jobs.resource_snapshot_task(job_id);
            let resources = tokio::task::spawn_blocking(snapshot)
                .await
                .thrown()?
                .thrown()?;
            return reply(200, json!({ "job": job, "resources": resources }));
        }
        if post && path.starts_with("/jobs/") {
            let parts: Vec<&str> = path.split('/').collect();
            let (job_id, action) = (
                parts.get(2).copied().unwrap_or_default(),
                parts.get(3).copied().unwrap_or_default(),
            );
            let body = request.json().await?;
            if job_id.is_empty() || action.is_empty() {
                return Err("job and action are required".into());
            }
            required_strings(&body, &["projectId", "idempotencyKey"])?;
            if action != "checkpoint" && action != "cancel" {
                return Err("unsupported job action".into());
            }
            let job = daemon
                .control_job(
                    &text(&body, "projectId"),
                    job_id,
                    action,
                    &text(&body, "idempotencyKey"),
                )
                .thrown()?;
            return reply(202, json!({ "job": job }));
        }
        reply(404, json!({ "error": "not_found" }))
    }
}

/// How Node's argument type errors describe the value they received.
fn received(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(flag)) => format!("type boolean ({flag})"),
        Some(Value::Number(number)) => format!(
            "type number ({})",
            nosh_wire::js::string(Some(&Value::Number(number.clone())))
        ),
        Some(Value::String(_)) => unreachable!("strings are accepted"),
        Some(Value::Array(_)) => "an instance of Array".into(),
        Some(Value::Object(_)) => "an instance of Object".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_safe_integers() {
        assert!(is_safe_integer(9_007_199_254_740_991.0));
        assert!(!is_safe_integer(9_007_199_254_740_992.0));
        assert!(!is_safe_integer(1.5));
    }

    #[test]
    fn validates_model_selection_request_shapes_while_preserving_omitted_thinking_levels() {
        let selection = json!({ "provider": "provider-a", "id": "model-a" });
        assert_eq!(
            parse_model_selection(Some(&selection), None).unwrap(),
            Some(selection.clone())
        );
        assert_eq!(
            parse_model_selection(Some(&selection), Some(&json!("max"))).unwrap(),
            Some(json!({ "provider": "provider-a", "id": "model-a", "thinkingLevel": "max" }))
        );
        let invalid =
            json!({ "provider": "provider-a", "id": "model-a", "thinkingLevel": "invalid" });
        assert!(
            parse_model_selection(Some(&invalid), None)
                .unwrap_err()
                .contains("thinkingLevel one of")
        );
        assert!(
            parse_model_selection(None, Some(&json!("high")))
                .unwrap_err()
                .contains("requires a selected Pi model")
        );
        // zod's `.strict()` and `.trim().min(1)`.
        let padded = json!({ "provider": " p ", "id": "m" });
        assert_eq!(
            parse_model_selection(Some(&padded), None).unwrap(),
            Some(json!({ "provider": "p", "id": "m" }))
        );
        for bad in [
            json!({ "provider": "p", "id": "m", "extra": 1 }),
            json!({ "provider": "  ", "id": "m" }),
            json!({ "provider": "p", "id": "m", "thinkingLevel": null }),
            Value::Null,
        ] {
            assert!(parse_model_selection(Some(&bad), None).is_err(), "{bad}");
        }
        assert_eq!(
            parse_model_selection(Some(&selection), Some(&json!("low"))).unwrap(),
            Some(json!({ "provider": "provider-a", "id": "model-a", "thinkingLevel": "low" }))
        );
        let nested = json!({ "provider": "p", "id": "m", "thinkingLevel": "high" });
        assert_eq!(
            parse_model_selection(Some(&nested), Some(&json!("low"))).unwrap_err(),
            "Nested and top-level thinkingLevel selections must match"
        );
        assert_eq!(
            parse_model_selection(Some(&nested), Some(&json!("bogus"))).unwrap_err(),
            "thinkingLevel must be one of off, minimal, low, medium, high, xhigh, max"
        );
    }

    #[test]
    fn required_strings_reads_as_typescript_does() {
        let body = json!({ "projectId": "prj", "idempotencyKey": "short", "blank": " \u{FEFF} " });
        assert_eq!(
            required_strings(&body, &["idempotencyKey"]).unwrap_err(),
            "idempotencyKey must be at least 16 characters"
        );
        assert_eq!(
            required_strings(&body, &["blank"]).unwrap_err(),
            "blank is required"
        );
        assert_eq!(
            required_strings(&json!({ "idempotencyKey": "   " }), &["idempotencyKey"]).unwrap_err(),
            "idempotencyKey is required"
        );
        assert_eq!(
            required_strings(&Value::Null, &["projectId"]).unwrap_err(),
            "Cannot read properties of null (reading 'projectId')"
        );
        assert_eq!(
            required_strings(&json!([1]), &["projectId"]).unwrap_err(),
            "projectId is required"
        );
        assert!(required_strings(&body, &["projectId"]).is_ok());
    }

    #[test]
    fn decodes_uri_components_and_refuses_malformed_ones() {
        assert_eq!(decode_uri_component("a%20b%2Fc").unwrap(), "a b/c");
        assert_eq!(decode_uri_component("%E2%9C%93").unwrap(), "\u{2713}");
        for bad in ["%", "%2", "%zz", "%FF"] {
            assert_eq!(decode_uri_component(bad).unwrap_err(), "URI malformed");
        }
    }
}
