//! Controller tests T1-T53, ported one for one from `apps/tui/src/controller.test.ts` ("thin
//! controller", "workspace presentation", "project-scoped concurrency"), including its tests of
//! `view-model.ts` functions.
//!
//! A TS call that runs synchronously up to its first `await` (`const stale = controller.poll()`) is
//! a `spawn_local` followed by `settle()`, so the request is issued before the next step.

mod common;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use common::{MockApi, Resolver, json, local, obj, settle, with};
use nosh_tui::controller::{Controller, Model, ModelRef, Project, Selection, accept_page};
use nosh_tui::js::{JsValue, json_stringify, length};
use nosh_tui::transport::StreamState;
use nosh_tui::view_model::{
    CLIMB_MAXIMUM, COLLAPSE_CHARACTERS, COLLAPSE_LINES, MapLine, PROGRESS_CELLS, ViewField,
    ViewRow, climb_lines, collapse_lines, event_entries, map_tone, progress_bar,
    research_map_mermaid, safe_text_str,
};

/// `event(sequence, projectId)`: an `agent.completed` with message `hello`.
fn event(sequence: i64, project_id: &str) -> JsValue {
    obj(vec![
        ("eventId", format!("e{sequence}").into()),
        ("sequence", (sequence as f64).into()),
        ("type", "agent.completed".into()),
        ("payload", obj(vec![("message", "hello".into())])),
        ("scope", obj(vec![("projectId", project_id.into())])),
    ])
}

/// `agentEvent(sequence, type, payload, agentId)` of "workspace presentation".
fn agent_event(sequence: Option<i64>, kind: &str, payload: JsValue, agent_id: &str) -> JsValue {
    let number = sequence.map_or_else(|| "null".to_owned(), |number| number.to_string());
    obj(vec![
        ("eventId", format!("{kind}-{number}").into()),
        (
            "sequence",
            sequence.map_or(JsValue::Null, |number| (number as f64).into()),
        ),
        ("type", kind.into()),
        ("payload", payload),
        (
            "scope",
            obj(vec![
                ("projectId", "p".into()),
                ("agentId", agent_id.into()),
            ]),
        ),
    ])
}

fn empty() -> JsValue {
    obj(vec![])
}

/// `setup()`: a controller for project `p` with projects `p` and `q` registered.
fn setup() -> (Rc<Controller>, Rc<MockApi>) {
    let api = MockApi::new();
    let controller = Controller::new(api.clone(), Some("p".into()));
    controller.state_mut().projects = vec![project("p"), project("q")];
    (controller, api)
}

fn project(id: &str) -> Project {
    Project {
        project_id: id.into(),
        repository_root: format!("/{id}"),
    }
}

fn model(provider: &str, id: &str, name: &str, levels: &[&str]) -> Model {
    Model {
        provider: provider.into(),
        id: id.into(),
        name: name.into(),
        thinking_levels: levels.iter().map(|level| (*level).to_owned()).collect(),
    }
}

fn selection(provider: &str, id: &str, level: &str) -> Selection {
    Selection {
        model: ModelRef {
            provider: provider.into(),
            id: id.into(),
        },
        thinking_level: level.into(),
    }
}

fn sequences(controller: &Controller) -> Vec<f64> {
    controller
        .state()
        .events
        .iter()
        .map(|event| event.get("sequence").and_then(JsValue::as_f64).unwrap())
        .collect()
}

fn error_of(result: Result<(), String>) -> String {
    result.expect_err("the command fails")
}

/// `controller.poll()` started without awaiting it.
async fn start_poll(controller: &Rc<Controller>) -> tokio::task::JoinHandle<()> {
    let controller = controller.clone();
    let handle = tokio::task::spawn_local(async move { controller.poll().await });
    settle().await;
    handle
}

/// `controller.execute(command)` started without awaiting it.
async fn start_execute(
    controller: &Rc<Controller>,
    command: &str,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let (controller, command) = (controller.clone(), command.to_owned());
    let handle = tokio::task::spawn_local(async move { controller.execute(&command).await });
    settle().await;
    handle
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The live climb's shape: baseline → ★ temperature → ★ beta, each with the experiments that tried
/// to beat it.
fn climb(state: &str, updated_at: &str) -> JsValue {
    json(&format!(
        r#"{{"kind":"autoresearch","id":"ar_c","title":"How low can ECE go?","state":"{state}","updatedAt":"{updated_at}","detail":"round 8/30 · 6/30 experiments · best 0.006006687176108325 · baseline 0.018660968449126965","progress":null,"children":[
          {{"kind":"experiment","id":"exp_t","title":"Temperature scaling fitted on held-out data","state":"promoted","detail":"round 1 · score 0.009670712709885499 (+0.009)","children":[
            {{"kind":"experiment","id":"exp_p","title":"Regularized Platt \"scaling\"","state":"rejected","detail":"round 2 · score 0.011475832864619858 (-0.0018)","children":[]}},
            {{"kind":"experiment","id":"exp_b","title":"Beta calibration","state":"promoted","detail":"round 3 · score 0.006006687176108325 (+0.0037)","children":[
              {{"kind":"experiment","id":"exp_l","title":"L2 1e-4","state":"rejected","detail":"round 4 · score 0.01596543812707744 (-0.01)","children":[]}},
              {{"kind":"experiment","id":"exp_n","title":"No intercept","state":"failed","detail":"round 7 · failed in implementation","children":[]}},
              {{"kind":"experiment","id":"exp_s","title":"Shrink toward the raw probability","state":"implementing","detail":"round 8","children":[]}}]}}]}}]}}"#
    ))
}

fn default_climb() -> JsValue {
    climb("completed", "2026-10-18T02:13:29.000Z")
}

fn row(line: &MapLine) -> String {
    let glyph = if line.glyph.is_empty() {
        String::new()
    } else {
        format!("{} ", line.glyph)
    };
    let label = if line.label.is_empty() {
        String::new()
    } else {
        format!("{} ", line.label)
    };
    format!("{}{glyph}{label}{}{}", line.prefix, line.title, line.state)
}

fn pad(left: &str, right: &str) -> String {
    format!(
        "{left}{}{right}",
        " ".repeat(30 - length(left) - length(right))
    )
}

fn row_matches(rows: &[ViewRow], test: impl Fn(&ViewRow) -> bool) -> bool {
    rows.iter().any(test)
}

// ---------------------------------------------------------------------------------------------
// thin controller
// ---------------------------------------------------------------------------------------------

#[test]
fn t01_keeps_the_prior_view_and_content_when_a_command_fails() {
    local(async {
        let (controller, _) = setup();
        {
            let mut state = controller.state_mut();
            state.project_id.clear();
            state.view = "help".into();
            state.detail = "help text".into();
        }
        assert!(error_of(controller.execute("/jobs").await).contains("Select or open"));
        assert_eq!(controller.state().view, "help");
        assert_eq!(controller.state().detail, "help text");
    });
}

#[test]
fn t02_keeps_the_explicitly_selected_intake_model_after_opening_a_project() {
    local(async {
        let (controller, api) = setup();
        controller.state_mut().selection = Some(selection("x", "model", "low"));
        api.once(json(
            r#"{"project":{"projectId":"new","repositoryRoot":"/new"}}"#,
        ));
        controller
            .execute(r#"/open {"path":"/new","workingTitle":"Study","createRepository":true}"#)
            .await
            .unwrap();
        assert_eq!(controller.state().project_id, "new");
        assert_eq!(
            controller.state().selection,
            Some(selection("x", "model", "low"))
        );
    });
}

#[test]
fn t03_labels_an_unapproved_active_contract_as_a_draft_never_approved() {
    local(async {
        let (controller, api) = setup();
        api.once(json(
            r#"{"contract":{"workingTitle":"Study","approvedAt":null}}"#,
        ))
        .once(json(r#"{"proposals":[]}"#));
        controller.execute("/approvals").await.unwrap();
        let rows = controller.inspector_rows();
        assert_eq!(rows[0].title, "Study");
        assert_eq!(rows[0].state.as_deref(), Some("draft"));
        api.once(json(
            r#"{"contract":{"workingTitle":"Study","approvedAt":"2026-10-01T00:00:00.000Z"}}"#,
        ))
        .once(json(r#"{"proposals":[]}"#));
        controller.execute("/approvals").await.unwrap();
        assert_eq!(
            controller.inspector_rows()[0].state.as_deref(),
            Some("approved")
        );
    });
}

#[test]
fn t04_opens_the_research_map_refreshes_it_while_open_and_clears_it_on_project_switch() {
    local(async {
        let (controller, api) = setup();
        let map = json(
            r#"{"generatedAt":"2026-10-03T12:00:00.000Z","roots":[{"kind":"mission","id":"mis_1","title":"Boundary tests","state":"running","detail":"v4","progress":{"done":1,"total":3},"children":[]}],"workers":[]}"#,
        );
        api.once(map.clone());
        controller.execute("/map").await.unwrap();
        assert!(api.called("/research-map?projectId=p"));
        assert_eq!(controller.state().view, "map");
        assert_eq!(controller.state().research_map, Some(map));
        assert_eq!(controller.inspector_rows(), vec![]);
        // A poll refreshes it (throttled) in any view: the sidebar's climb graph reads it too.
        controller.execute("/chat").await.unwrap();
        controller.state_mut().map_requested_at = 0.0;
        let next = json(
            r#"{"generatedAt":"2026-10-03T12:00:00.000Z","roots":[{"kind":"mission","id":"mis_1","title":"Boundary tests","state":"completed","detail":"v4","progress":{"done":1,"total":3},"children":[]}],"workers":[]}"#,
        );
        let reply = next.clone();
        api.implement(move |path| {
            if path.starts_with("/research-map") {
                reply.clone()
            } else {
                json(r#"{"events":[]}"#)
            }
        });
        controller.poll().await;
        settle().await;
        assert_eq!(controller.state().research_map, Some(next));
        controller.select_project("q", false).unwrap();
        assert_eq!(controller.state().research_map, None);
        assert_eq!(controller.state().view, "chat");
    });
}

#[test]
fn t05_maps_states_to_tones_and_draws_progress_bars() {
    assert_eq!(progress_bar(1.0, 3.0, PROGRESS_CELLS), "▰▰▰▱▱▱▱▱▱▱ 1/3");
    assert_eq!(progress_bar(0.0, 0.0, PROGRESS_CELLS), "");
    let tones: Vec<&str> = ["accepted", "working", "reviewing", "failed", "ready"]
        .iter()
        .map(|state| map_tone(state).as_str())
        .collect();
    assert_eq!(tones, ["success", "active", "waiting", "error", "idle"]);
}

#[test]
fn t06_draws_the_most_relevant_autoresearch_climb_as_a_narrow_chain_for_the_sidebar() {
    // A finished nested run of a blocked Mission sorts first in the map; the most recently updated
    // run wins.
    let older = with(
        &climb("completed", "2026-10-17T22:00:00.000Z"),
        vec![
            ("id", "ar_old".into()),
            ("title", "Old run".into()),
            ("children", JsValue::Array(vec![])),
        ],
    );
    let node = obj(vec![
        ("kind", "node".into()),
        ("id", "n1".into()),
        ("title", "Direction".into()),
        ("state", "accepted".into()),
        ("detail", "".into()),
        ("children", JsValue::Array(vec![older.clone()])),
    ]);
    let mission = obj(vec![
        ("kind", "mission".into()),
        ("id", "mis_1".into()),
        ("title", "Spam".into()),
        ("state", "blocked".into()),
        ("detail", "v93".into()),
        ("progress", JsValue::Null),
        ("children", JsValue::Array(vec![node])),
    ]);
    let map = obj(vec![(
        "roots",
        JsValue::Array(vec![mission, default_climb()]),
    )]);
    let lines = climb_lines(Some(&map), 30, CLIMB_MAXIMUM);
    assert_eq!(
        lines.iter().map(row).collect::<Vec<_>>(),
        vec![
            "CLIMB completed".to_owned(),
            "best 0.006007 · 6/30 exp".to_owned(),
            pad("◎ baseline", "0.01866"),
            pad("★ Temperature scalin…", "0.009671"),
            pad("│ ✗ Regularized Platt…", "0.01148"),
            pad("★ Beta calibration", "0.006007"),
            pad("  ✗ L2 1e-4", "0.01597"),
            pad("  ✗ No intercept", "crash"),
            pad("  ● Shrink towar…", "implementing"),
        ]
    );
    assert!(lines[2..].iter().all(|line| length(&row(line)) == 30));
    // Headers clip too (live, "best 0.006007 · 12/30 exp" overflowed a 23-column sidebar).
    assert!(
        climb_lines(Some(&map), 20, CLIMB_MAXIMUM)
            .iter()
            .all(|line| length(&row(line)) <= 20)
    );
    let tones: Vec<&str> = lines[2..5].iter().map(|line| line.tone.as_str()).collect();
    assert_eq!(tones, ["idle", "success", "error"]);
    // A running run anywhere beats a more recently updated finished one.
    let live = with(
        &older,
        vec![("state", "running".into()), ("title", "Live run".into())],
    );
    let running = obj(vec![("roots", JsValue::Array(vec![default_climb(), live]))]);
    assert_eq!(
        climb_lines(Some(&running), 30, CLIMB_MAXIMUM)[0].title,
        "running"
    );
    // Bounded, saying how much is hidden.
    let bounded = climb_lines(Some(&map), 30, 5);
    assert_eq!(bounded.len(), 5);
    assert_eq!(bounded.last().unwrap().title, "+5 more");
    assert_eq!(
        climb_lines(Some(&json(r#"{"roots":[]}"#)), 30, CLIMB_MAXIMUM),
        vec![]
    );
    assert_eq!(climb_lines(None, 30, CLIMB_MAXIMUM), vec![]);
}

#[test]
fn t07_exports_the_research_map_as_a_mermaid_flowchart_with_lineage_edges() {
    let map = obj(vec![("roots", JsValue::Array(vec![default_climb()]))]);
    let chart = research_map_mermaid(Some(&map));
    let lines: Vec<&str> = chart.split('\n').collect();
    assert_eq!(lines[0], "flowchart LR");
    for edge in [
        "n0 --> n0base",
        "n0base --> n1",
        "n1 --> n2",
        "n1 --> n3",
        "n3 --> n4",
    ] {
        assert!(lines.contains(&format!("  {edge}").as_str()), "{edge}");
    }
    assert!(chart.contains(
        r#"n1["Temperature scaling fitted on held-out data<br/><small>promoted · round 1 · score 0.00967 (+0.009)</small>"]:::keep"#
    ));
    assert!(chart.contains("Regularized Platt #quot;scaling#quot;"));
    assert!(chart.contains(":::crash"));
}

#[test]
fn t08_writes_the_mermaid_chart_to_a_file_on_map_mermaid() {
    local(async {
        let (controller, api) = setup();
        api.once(obj(vec![("roots", JsValue::Array(vec![default_climb()]))]));
        let dir = temp_dir("nosh-map-");
        let file = dir.join("map.md");
        let file_text = file.display().to_string();
        controller
            .execute(&format!("/map mermaid {file_text}"))
            .await
            .unwrap();
        assert!(api.called("/research-map?projectId=p"));
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("```mermaid\nflowchart LR\n"));
        assert!(written.contains("n0base --> n1"));
        assert!(controller.state().detail.contains(&file_text));
        std::fs::remove_dir_all(dir).unwrap();
    });
}

#[test]
fn t09_help_and_paths_do_not_keep_the_previous_view_rows() {
    local(async {
        let (controller, api) = setup();
        api.once(json(
            r#"{"contract":{"workingTitle":"Study","approvedAt":null}}"#,
        ))
        .once(json(r#"{"proposals":[]}"#));
        controller.execute("/approvals").await.unwrap();
        assert_ne!(controller.inspector_rows(), vec![]);
        controller.execute("/help").await.unwrap();
        assert_eq!(controller.inspector_rows(), vec![]);
        controller.execute("/paths").await.unwrap();
        assert_eq!(controller.inspector_rows(), vec![]);
    });
}

#[test]
fn t10_summarizes_a_terminal_envelope_the_daemon_accepted_even_with_stray_trailing_closers() {
    let envelope = json_stringify(&json(
        r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1,"records":[{"$schema":"https://nosh.dev/schemas/review-verdict/v1","summary":"PASS all criteria"}]}"#,
    ));
    let completed = |id: &str, sequence: f64, message: String| {
        obj(vec![
            ("eventId", id.into()),
            ("sequence", sequence.into()),
            ("type", "agent.completed".into()),
            ("payload", obj(vec![("message", message.into())])),
            (
                "scope",
                obj(vec![("projectId", "p".into()), ("agentId", "r".into())]),
            ),
        ])
    };
    for message in [
        envelope.clone(),
        format!("{envelope}]}}"),
        format!("{envelope}</|im_end|>"),
    ] {
        let entries = event_entries(&[completed("c1", 1.0, message)]);
        assert_eq!(
            entries[0].text,
            "PASS all criteria\nAwait the host receipt for validation."
        );
    }
    let prose = event_entries(&[completed("c2", 2.0, "{not json} and prose".into())]);
    assert_eq!(prose[0].text, "{not json} and prose");
}

#[test]
fn t11_titles_tools_by_their_target_and_collapses_long_output() {
    let scope = obj(vec![("projectId", "p".into()), ("agentId", "a".into())]);
    let output = (0..20)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let entries = event_entries(&[
        obj(vec![
            ("eventId", "t1".into()),
            ("sequence", 1.0.into()),
            ("type", "agent.tool_started".into()),
            (
                "payload",
                json(r#"{"toolName":"bash","toolCallId":"c","args":{"command":"ls -la .nosh"}}"#),
            ),
            ("scope", scope.clone()),
        ]),
        obj(vec![
            ("eventId", "t2".into()),
            ("sequence", 2.0.into()),
            ("type", "agent.tool_completed".into()),
            (
                "payload",
                obj(vec![
                    ("toolName", "bash".into()),
                    ("toolCallId", "c".into()),
                    (
                        "result",
                        obj(vec![(
                            "content",
                            JsValue::Array(vec![obj(vec![
                                ("type", "text".into()),
                                ("text", output.into()),
                            ])]),
                        )]),
                    ),
                ]),
            ),
            ("scope", scope),
        ]),
    ]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].title, "bash ls -la .nosh");
    assert_eq!(entries[0].status.as_deref(), Some("completed"));
    assert_eq!(entries[0].text.split('\n').count(), 7);
    assert!(entries[0].text.contains("… 14 more lines"));
    assert_eq!(
        length(&collapse_lines(
            &"x".repeat(2000),
            COLLAPSE_LINES,
            COLLAPSE_CHARACTERS
        )),
        360
    );
}

#[test]
fn t12_reports_open_usage_instead_of_a_json_parser_error() {
    local(async {
        let (controller, api) = setup();
        assert!(error_of(controller.execute("/open ").await).contains(r#"Use /open {"path""#));
        assert!(
            error_of(controller.execute("/open not-json").await).contains(r#"Use /open {"path""#)
        );
        assert_eq!(api.calls(), vec![]);
    });
}

#[test]
fn t13_omitted_thinking_uses_only_an_advertised_level() {
    local(async {
        let (controller, _) = setup();
        controller.state_mut().models = vec![
            model("x", "plain", "Plain", &["low", "high"]),
            model("x", "off", "Off", &["off", "low"]),
        ];
        controller.execute("/model x plain").await.unwrap();
        let level = |controller: &Controller| {
            controller
                .state()
                .selection
                .as_ref()
                .map(|selection| selection.thinking_level.clone())
        };
        assert_eq!(level(&controller).as_deref(), Some("low"));
        controller.execute("/model x off").await.unwrap();
        assert_eq!(level(&controller).as_deref(), Some("off"));
    });
}

#[test]
fn t14_polls_a_switched_project_immediately_while_a_stale_poll_is_in_flight() {
    local(async {
        let (controller, api) = setup();
        let release = api.once_pending();
        api.once(obj(vec![("events", JsValue::Array(vec![event(5, "q")]))]));
        let stale = start_poll(&controller).await;
        controller.select_project("q", false).unwrap();
        controller.poll().await;
        assert_eq!(
            api.last().path,
            "/events?projectId=q&after=0&limit=200&recent=true"
        );
        release.resolve(obj(vec![("events", JsValue::Array(vec![event(9, "p")]))]));
        stale.await.unwrap();
        assert_eq!(sequences(&controller), [5.0]);
    });
}

#[test]
fn t15_bootstraps_once_resumes_deltas_after_reconnect_and_resets_on_project_switch() {
    local(async {
        let (controller, api) = setup();
        let page = |events: Vec<JsValue>, next: f64| {
            obj(vec![
                ("events", JsValue::Array(events)),
                ("nextCursor", next.into()),
                ("hasMore", false.into()),
            ])
        };
        api.once(page(vec![event(999, "p"), event(1000, "p")], 1000.0))
            .once_error("offline")
            .once(page(vec![event(1001, "p"), event(1002, "p")], 1002.0))
            .once(page(vec![event(700, "q")], 700.0));
        controller.poll().await;
        assert_eq!(
            api.last().path,
            "/events?projectId=p&after=0&limit=200&recent=true"
        );
        assert_eq!(controller.state().cursor, 1000);
        controller.poll().await;
        assert_eq!(controller.state().cursor, 1000);
        controller.poll().await;
        assert_eq!(api.last().path, "/events?projectId=p&after=1000&limit=200");
        assert_eq!(sequences(&controller), [999.0, 1000.0, 1001.0, 1002.0]);
        controller.execute("/project q").await.unwrap();
        controller.poll().await;
        assert_eq!(
            api.last().path,
            "/events?projectId=q&after=0&limit=200&recent=true"
        );
        assert_eq!(sequences(&controller), [700.0]);
    });
}

#[test]
fn t16_does_not_repeat_recent_bootstrap_after_a_successful_empty_page() {
    local(async {
        let (controller, api) = setup();
        api.once(json(r#"{"events":[]}"#))
            .once(obj(vec![("events", JsValue::Array(vec![event(1, "p")]))]));
        controller.poll().await;
        controller.poll().await;
        assert_eq!(
            api.calls()[0].path,
            "/events?projectId=p&after=0&limit=200&recent=true"
        );
        assert_eq!(api.last().path, "/events?projectId=p&after=0&limit=200");
        assert_eq!(controller.state().cursor, 1);
    });
}

#[test]
fn t17_bounds_history_and_filters_duplicates_old_cursors_and_other_projects() {
    let mut page: Vec<JsValue> = (1..=600).map(|sequence| event(sequence, "p")).collect();
    page.push(event(601, "other"));
    page.push(event(600, "p"));
    let (events, after) = accept_page(&[], &page, "p", 0);
    assert_eq!(events.len(), 400);
    assert_eq!(after, 600);
    assert_eq!(
        accept_page(&events, &[event(3, "p")], "p", 600).0.len(),
        400
    );
}

#[test]
fn t18_requires_explicit_confirmation_and_preserves_inspected_version_and_idempotency() {
    local(async {
        let (controller, api) = setup();
        controller.execute("/approve proposal 7").await.unwrap();
        assert_eq!(api.calls(), vec![]);
        controller.execute("/confirm").await.unwrap();
        let call = api.last();
        assert_eq!(call.path, "/graph-proposals/proposal/approve");
        let body = call.body.unwrap();
        assert_eq!(body.get("projectId"), Some(&"p".into()));
        assert_eq!(body.get("expectedProposalVersion"), Some(&7.0.into()));
        let key = body
            .get("idempotencyKey")
            .and_then(JsValue::as_str)
            .unwrap();
        assert!(key.starts_with("tui-"));
        assert!(error_of(controller.execute("/confirm").await).contains("No staged"));
        assert_eq!(api.calls().len(), 1);
    });
}

#[test]
fn t19_clears_pending_actions_model_and_cursor_on_project_switch() {
    local(async {
        let (controller, _) = setup();
        {
            let mut state = controller.state_mut();
            state.cursor = 50;
            state.events = vec![event(50, "p")];
        }
        controller.execute("/cancel job").await.unwrap();
        controller.execute("/project q").await.unwrap();
        assert_eq!(controller.state().cursor, 0);
        assert_eq!(controller.state().events, vec![]);
        assert!(error_of(controller.execute("/confirm").await).contains("No staged"));
    });
}

#[test]
fn t20_rejects_stale_asynchronous_pages_after_project_changes() {
    local(async {
        let (controller, api) = setup();
        let resolve = api.always_pending();
        let poll = start_poll(&controller).await;
        controller.execute("/project q").await.unwrap();
        resolve.resolve(obj(vec![("events", JsValue::Array(vec![event(1, "p")]))]));
        poll.await.unwrap();
        assert_eq!(controller.state().events, vec![]);
        assert_eq!(controller.state().cursor, 0);
    });
}

#[test]
fn t21_validates_model_thinking_and_sends_explicit_selection_with_chat() {
    local(async {
        let (controller, api) = setup();
        controller.state_mut().models = vec![model("x", "model", "Model", &["off", "low"])];
        assert!(error_of(controller.execute("/model x model high").await).contains("Thinking"));
        controller.execute("/model x model low").await.unwrap();
        controller.execute("hello").await.unwrap();
        let call = api.last();
        assert_eq!((call.path.as_str(), call.timeout), ("/chat", Some(120_000)));
        let body = call.body.unwrap();
        assert_eq!(
            body.get("model"),
            Some(&json(r#"{"provider":"x","id":"model"}"#))
        );
        assert_eq!(body.get("thinkingLevel"), Some(&"low".into()));
        assert_eq!(body.get("message"), Some(&"hello".into()));
    });
}

/// `toHaveBeenLastCalledWith('/chat', expected, 120_000)` with `idempotencyKey:
/// expect.stringMatching(/^tui-chat-/)` in `expected` (written here without it).
fn assert_chat(api: &MockApi, expected: &str) {
    let call = api.last();
    assert_eq!((call.path.as_str(), call.timeout), ("/chat", Some(120_000)));
    let mut body = call.body.unwrap();
    let JsValue::Object(object) = &mut body else {
        panic!("an object body")
    };
    let key = object
        .get("idempotencyKey")
        .and_then(JsValue::as_str)
        .unwrap();
    assert!(key.starts_with("tui-chat-"), "{key}");
    let mut expected = json(expected);
    if let JsValue::Object(expected) = &mut expected {
        let keys: Vec<&str> = object.iter().map(|(key, _)| key).collect();
        assert!(keys.contains(&"idempotencyKey"));
        assert_eq!(keys.len(), expected.iter().count() + 1);
        for (key, value) in expected.iter() {
            assert_eq!(object.get(key), Some(value), "{key}");
        }
    }
}

#[test]
fn t22_sends_the_daemon_chat_shape_with_default_and_selected_muse_models() {
    local(async {
        let (controller, api) = setup();
        controller.state_mut().models = vec![model(
            "opencode",
            "muse-spark-1.3-contributor-free",
            "Muse",
            &["off", "high"],
        )];
        controller.execute("hello").await.unwrap();
        assert_chat(&api, r#"{"projectId":"p","message":"hello"}"#);
        controller
            .execute("/model opencode muse-spark-1.3-contributor-free high")
            .await
            .unwrap();
        controller.execute("hello").await.unwrap();
        assert_chat(
            &api,
            r#"{"projectId":"p","message":"hello","model":{"provider":"opencode","id":"muse-spark-1.3-contributor-free"},"thinkingLevel":"high"}"#,
        );
        controller.execute("/model default").await.unwrap();
        controller.execute("hello again").await.unwrap();
        assert_chat(&api, r#"{"projectId":"p","message":"hello again"}"#);
    });
}

#[test]
fn t23_asks_for_bounded_pages_and_reconnects_without_losing_cursor() {
    local(async {
        let (controller, api) = setup();
        controller.state_mut().cursor = 9;
        api.once_error("offline")
            .once(obj(vec![("events", JsValue::Array(vec![event(10, "p")]))]));
        controller.poll().await;
        assert!(controller.state().connection.contains("reconnecting"));
        assert_eq!(controller.state().cursor, 9);
        controller.poll().await;
        assert_eq!(controller.state().cursor, 10);
        assert_eq!(api.last().path, "/events?projectId=p&after=9&limit=200");
    });
}

#[test]
fn t24_renders_authoritative_assistant_completion_and_strips_terminal_controls() {
    let (controller, _) = setup();
    controller.state_mut().events = vec![event(1, "p")];
    assert!(controller.transcript().contains("NOSH"));
    assert!(controller.transcript().contains("hello"));
    assert!(!safe_text_str("hello\x1b]52;secret\x07").contains('\x1b'));
}

// ---------------------------------------------------------------------------------------------
// workspace presentation
// ---------------------------------------------------------------------------------------------

#[test]
fn t25_projects_durable_user_assistant_tool_failure_and_host_receipts_with_stable_ids() {
    let (controller, _) = setup();
    let start = agent_event(
        Some(2),
        "agent.tool_started",
        json(r#"{"toolCallId":"call","toolName":"read","args":{"path":"paper.md"}}"#),
        "a",
    );
    controller.state_mut().events = vec![
        agent_event(
            Some(1),
            "chat.user_message",
            json(
                r#"{"message":"Read the paper","agentId":"a","selection":{"model":{"provider":"x","id":"m"},"thinkingLevel":"low"}}"#,
            ),
            "a",
        ),
        start,
    ];
    let tool_id = controller
        .transcript_entries()
        .into_iter()
        .find(|entry| entry.kind.as_str() == "tool")
        .unwrap()
        .id;
    controller.state_mut().events.extend([
        agent_event(
            Some(3),
            "agent.tool_completed",
            json(
                r#"{"toolCallId":"call","toolName":"read","result":{"content":[{"type":"text","text":"Paper contents"},{"type":"thinking","text":"never display"}]}}"#,
            ),
            "a",
        ),
        agent_event(
            Some(4),
            "agent.completed",
            json(r#"{"message":"The paper is ready."}"#),
            "a",
        ),
        agent_event(
            Some(5),
            "agent.terminal_receipt",
            json(
                r#"{"accepted":true,"retryAllowed":false,"status":"completed","effect":{"state":"completed"}}"#,
            ),
            "a",
        ),
        agent_event(
            Some(6),
            "agent.failed",
            json(r#"{"message":"Provider access failed"}"#),
            "a",
        ),
    ]);
    let entries = controller.transcript_entries();
    let kinds: Vec<&str> = entries.iter().map(|entry| entry.kind.as_str()).collect();
    assert_eq!(kinds, ["user", "tool", "assistant", "receipt", "failure"]);
    assert_eq!(entries[1].id, tool_id);
    assert_eq!(entries[1].status.as_deref(), Some("completed"));
    assert_eq!(entries[1].text, "Paper contents");
    assert_eq!(entries[2].model.as_deref(), Some("x/m"));
    assert_eq!(entries[2].thinking_level.as_deref(), Some("low"));
    assert!(!format!("{entries:?}").contains("never display"));
    assert!(entries[3].text.contains("Accepted: true"));
}

#[test]
fn t26_shows_validation_receipts_separately_from_terminal_json_and_preserves_host_failure() {
    let (controller, _) = setup();
    let message = json_stringify(&json(
        r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1,"records":[{"summary":"Experiment finished"}]}"#,
    ));
    controller.state_mut().events = vec![
        agent_event(
            Some(1),
            "agent.completed",
            obj(vec![("message", message.into())]),
            "a",
        ),
        agent_event(
            Some(2),
            "agent.terminal_receipt",
            json(
                r#"{"accepted":true,"retryAllowed":false,"status":"completed","effect":{"state":"failed"}}"#,
            ),
            "a",
        ),
    ];
    let entries = controller.transcript_entries();
    assert!(entries[0].text.contains("Experiment finished"));
    assert!(!entries[0].text.contains("$schema"));
    assert_eq!(entries[1].status.as_deref(), Some("failed"));
}

#[test]
fn t27_streams_bounded_drafts_deduplicates_live_ids_and_replaces_them_with_authoritative_completion()
 {
    let (controller, _) = setup();
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    controller.subscribe(Rc::new(move || counter.set(counter.get() + 1)));
    controller.ingest_live_event(&agent_event(Some(1), "agent.started", empty(), "a"));
    let first = agent_event(None, "agent.text_delta", json(r#"{"delta":"Hello"}"#), "a");
    controller.ingest_live_event(&first);
    controller.ingest_live_event(&first);
    let draft = controller.transcript_entries()[0].clone();
    assert_eq!(
        (
            draft.kind.as_str(),
            draft.text.as_str(),
            draft.status.as_deref()
        ),
        ("assistant", "Hello", Some("streaming"))
    );
    controller.ingest_live_event(&with(
        &first,
        vec![
            ("eventId", "delta-2".into()),
            ("payload", json(r#"{"delta":" world"}"#)),
        ],
    ));
    let entry = controller.transcript_entries()[0].clone();
    assert_eq!((entry.id, entry.text.as_str()), (draft.id, "Hello world"));
    assert_eq!(controller.state().cursor, 1);
    assert!(controller.activity().working);
    controller.ingest_live_event(&agent_event(
        Some(2),
        "agent.completed",
        json(r#"{"message":"Authoritative result"}"#),
        "a",
    ));
    assert_eq!(controller.transcript_entries().len(), 1);
    assert_eq!(
        controller.transcript_entries()[0].text,
        "Authoritative result"
    );
    controller.ingest_live_event(&with(&first, vec![("eventId", "late".into())]));
    assert_eq!(controller.transcript_entries().len(), 1);
    assert!(!controller.activity().working);
    assert!(calls.get() > 0);
    controller.ingest_live_event(&agent_event(Some(3), "agent.started", empty(), "a"));
    controller.ingest_live_event(&with(
        &first,
        vec![
            ("eventId", "new-turn".into()),
            ("payload", json(r#"{"delta":"Next turn"}"#)),
        ],
    ));
    let last = controller.transcript_entries().pop().unwrap();
    assert_eq!(
        (last.text.as_str(), last.status.as_deref()),
        ("Next turn", Some("streaming"))
    );
}

#[test]
fn t28_clears_ephemeral_tool_output_on_completion_disconnect_and_project_switch() {
    let (controller, _) = setup();
    controller.ingest_live_event(&agent_event(
        Some(1),
        "agent.tool_started",
        json(r#"{"toolCallId":"call","toolName":"read","args":{"path":"file"}}"#),
        "a",
    ));
    let update = agent_event(
        None,
        "agent.tool_update",
        json(
            r#"{"toolCallId":"call","toolName":"read","partialResult":{"content":[{"type":"text","text":"partial"}]}}"#,
        ),
        "a",
    );
    controller.ingest_live_event(&update);
    assert_eq!(controller.transcript_entries().len(), 1);
    assert_eq!(controller.transcript_entries()[0].text, "partial");
    controller.ingest_live_event(&agent_event(
        Some(2),
        "agent.tool_completed",
        json(
            r#"{"toolCallId":"call","toolName":"read","result":{"content":[{"type":"text","text":"final"}]}}"#,
        ),
        "a",
    ));
    assert_eq!(controller.transcript_entries()[0].text, "final");
    controller.ingest_live_event(&with(&update, vec![("eventId", "late-tool-update".into())]));
    assert_eq!(controller.transcript_entries()[0].text, "final");
    controller.ingest_live_event(&agent_event(
        None,
        "agent.text_delta",
        json(r#"{"delta":"temporary"}"#),
        "a",
    ));
    controller.clear_live_drafts();
    assert!(
        controller
            .transcript_entries()
            .iter()
            .all(|entry| entry.sequence.is_some())
    );
    controller.select_project("q", false).unwrap();
    controller.ingest_live_event(&update);
    assert_eq!(controller.transcript_entries(), vec![]);
}

#[test]
fn t29_tracks_concurrent_agent_work_and_does_not_claim_new_composer_model_ran_the_active_turn() {
    let (controller, _) = setup();
    controller.state_mut().selection = Some(selection("next", "future", "high"));
    controller.state_mut().events = vec![
        agent_event(
            Some(1),
            "chat.user_message",
            json(
                r#"{"message":"hello","selection":{"model":{"provider":"current","id":"active"},"thinkingLevel":"low"}}"#,
            ),
            "a",
        ),
        agent_event(Some(2), "agent.started", empty(), "a"),
        agent_event(Some(3), "agent.started", empty(), "b"),
        agent_event(
            Some(4),
            "agent.completed",
            json(r#"{"message":"finished"}"#),
            "b",
        ),
    ];
    let activity = controller.activity();
    assert!(activity.working);
    assert_eq!(activity.agent_id.as_deref(), Some("a"));
    assert_eq!(activity.model, "current/active");
    assert_eq!(activity.thinking_level, "low");
    controller.state_mut().events.push(agent_event(
        Some(5),
        "agent.failed",
        json(r#"{"message":"failed"}"#),
        "a",
    ));
    let activity = controller.activity();
    assert!(!activity.working);
    assert_eq!(activity.model, "next/future");
}

#[test]
fn t30_names_the_chat_agent_in_the_idle_composer_not_the_last_background_task_agent() {
    let (controller, _) = setup();
    controller.state_mut().events = vec![
        agent_event(
            Some(1),
            "chat.user_message",
            json(r#"{"message":"hello","selection":{}}"#),
            "a",
        ),
        agent_event(Some(2), "agent.completed", json(r#"{"message":"hi"}"#), "a"),
        agent_event(Some(3), "agent.started", empty(), "reviewer"),
        agent_event(
            Some(4),
            "agent.completed",
            json(r#"{"message":"PASS"}"#),
            "reviewer",
        ),
    ];
    let activity = controller.activity();
    assert!(!activity.working);
    assert_eq!(activity.agent_id.as_deref(), Some("a"));
    assert_eq!(activity.role, "nosh");
}

#[test]
fn t31_uses_read_only_catalog_loads_and_detached_pending_snapshots() {
    local(async {
        let (controller, api) = setup();
        {
            let mut state = controller.state_mut();
            state.view = "help".into();
            state.detail = "unchanged".into();
        }
        api.once(json(
            r#"{"projects":[{"projectId":"p","repositoryRoot":"/p"}]}"#,
        ))
        .once(json(
            r#"{"models":[{"provider":"x","id":"m","name":"Model","thinkingLevels":["off"]}]}"#,
        ));
        controller.load_projects().await.unwrap();
        let mut models = controller.load_models().await.unwrap();
        models[0].thinking_levels.push("fake".into());
        assert_eq!(controller.state().view, "help");
        assert_eq!(controller.state().detail, "unchanged");
        assert_eq!(controller.state().models[0].thinking_levels, ["off"]);
        controller.execute("/approve proposal 7").await.unwrap();
        let mut action = controller.pending_action().unwrap();
        action.body.set("expectedProposalVersion", 99.0.into());
        assert_eq!(
            controller
                .pending_action()
                .unwrap()
                .body
                .get("expectedProposalVersion"),
            Some(&7.0.into())
        );
    });
}

#[test]
fn t32_lists_every_mission_direction_and_autoresearch_in_the_sidebar_not_only_those_in_the_recent_event_window()
 {
    local(async {
        let (controller, api) = setup();
        api.implement(|path| {
            if path.starts_with("/missions") {
                json(
                    r#"{"missions":[{"entityId":"mis_old","state":"completed","version":3,"value":{"title":"Old mission"}}]}"#,
                )
            } else if path.starts_with("/events") {
                json(r#"{"events":[]}"#)
            } else {
                empty()
            }
        });
        controller.poll().await;
        controller.sidebar_rows();
        settle().await;
        let rows: Vec<String> = controller
            .sidebar_rows()
            .iter()
            .map(|row| {
                format!(
                    "{}:{}:{}",
                    row.section,
                    row.id,
                    row.state.as_deref().unwrap_or("undefined")
                )
            })
            .collect();
        assert!(
            rows.contains(&"missions:mis_old:completed".to_owned()),
            "{rows:?}"
        );
        assert!(api.called("/directions?projectId=p"));
        assert!(api.called("/autoresearch?projectId=p"));
    });
}

#[test]
fn t33_keeps_a_live_entity_state_in_the_sidebar_after_the_event_window_rolls_past_it() {
    local(async {
        let (controller, api) = setup();
        api.implement(|path| {
            if path.starts_with("/autoresearch") {
                json(
                    r#"{"executions":[{"entityId":"aut","state":"running","version":2,"value":{"decisionQuestion":"Can we improve?"}}]}"#,
                )
            } else if path.starts_with("/events") {
                json(r#"{"events":[]}"#)
            } else {
                empty()
            }
        });
        controller.poll().await;
        controller.sidebar_rows();
        settle().await;
        let event = |sequence: i64, kind: &str, payload: JsValue| {
            agent_event(Some(sequence), kind, payload, "a")
        };
        controller.ingest_live_event(&event(
            1,
            "autoresearch.blocked",
            json(r#"{"entityId":"aut","state":"blocked","title":"Can we improve?"}"#),
        ));
        // Busy research: hundreds of tool events push the state change out of the 400-event
        // window within minutes.
        for sequence in 2..452 {
            controller.ingest_live_event(&event(
                sequence,
                "agent.tool_started",
                json(r#"{"toolName":"nosh_workspace_read"}"#),
            ));
        }
        let rows = controller.sidebar_rows();
        let row = rows.iter().find(|row| row.id == "aut").unwrap();
        assert_eq!(row.state.as_deref(), Some("blocked"));
    });
}

#[test]
fn t34_stages_a_paper_export_for_the_current_project() {
    local(async {
        let (controller, _) = setup();
        controller.execute("/export-paper").await.unwrap();
        let action = controller.pending_action().unwrap();
        assert_eq!(action.path, "/paper/export");
        assert_eq!(action.body.get("projectId"), Some(&"p".into()));
    });
}

#[test]
fn t35_renders_structured_status_and_approval_rows_with_inspected_versions_instead_of_json_blobs() {
    local(async {
        let (controller, api) = setup();
        api.implement(|path| match path {
            "/missions?projectId=p" => json(
                r#"{"missions":[{"entityId":"mis","version":4,"state":"running","value":{"title":"A study","objective":"Measure quality"}}]}"#,
            ),
            "/directions?projectId=p" => json(r#"{"directions":[]}"#),
            "/autoresearch?projectId=p" => json(
                r#"{"executions":[{"entityId":"aut","version":2,"state":"blocked","value":{"decisionQuestion":"Can we improve?"}}]}"#,
            ),
            "/agents?projectId=p" => json(
                r#"{"agents":[{"agentId":"a","role":"research_director","modelProvider":"x","modelId":"m","thinkingLevel":"low","status":"running"}]}"#,
            ),
            "/projects/p/contract" => json(r#"{"contract":null}"#),
            "/graph-proposals?projectId=p" => json(
                r#"{"proposals":[{"entityId":"prop","version":7,"state":"pending","value":{"record":{"rationale":"Add comparison","contractImpact":"scope"}}}]}"#,
            ),
            _ => empty(),
        });
        controller.execute("/status").await.unwrap();
        let rows = controller.inspector_rows();
        assert!(row_matches(&rows, |row| row.id == "mis"
            && row.section == "missions"
            && row.title == "A study"
            && row.version == Some(4.0)
            && row.state.as_deref() == Some("running")));
        assert!(row_matches(&rows, |row| row.id == "aut"
            && row.section == "autoresearch"
            && row.title == "Can we improve?"));
        controller.execute("/approvals").await.unwrap();
        let rows = controller.inspector_rows();
        assert!(row_matches(&rows, |row| row.id == "prop"
            && row.version == Some(7.0)
            && row.title == "Add comparison"));
        assert!(row_matches(&rows, |row| row.id == "contract"
            && row.state.as_deref() == Some("not approved")));
        assert!(controller.sidebar_rows().iter().any(|row| row.id == "mis"));
    });
}

#[test]
fn t36_derives_sidebar_state_from_durable_events_without_inventing_inspected_versions() {
    local(async {
        let (controller, _) = setup();
        controller.state_mut().events = vec![
            agent_event(
                Some(1),
                "mission.created",
                json(r#"{"entityId":"mis","state":"running"}"#),
                "a",
            ),
            agent_event(
                Some(2),
                "job.state_changed",
                json(
                    r#"{"jobId":"j","state":"failed","command":["python","run.py"],"failureReason":"timeout"}"#,
                ),
                "a",
            ),
            agent_event(
                Some(3),
                "graph.proposal_pending",
                json(r#"{"proposalId":"prop","contractImpact":"scope"}"#),
                "a",
            ),
        ];
        let rows = controller.sidebar_rows();
        for (id, state) in [("mis", "running"), ("j", "failed"), ("prop", "pending")] {
            assert!(
                row_matches(&rows, |row| row.id == id
                    && row.state.as_deref() == Some(state)),
                "{id}"
            );
        }
        let mission = rows.iter().find(|row| row.id == "mis").unwrap();
        assert_eq!(mission.version, None);
    });
}

#[test]
fn t37_keeps_the_current_server_tail_endpoint_and_shows_literal_bounded_output_plus_job_resources()
{
    local(async {
        let (controller, api) = setup();
        let output = format!("{}\nrecent\x1b[31m text", "old".repeat(10000));
        api.once(obj(vec![("text", output.into())])).once(json(
            r#"{"job":{"jobId":"j","state":"running","command":["python","train.py"]},"resources":{"elapsedSeconds":12,"residentBytes":1024,"outputStalledSeconds":1}}"#,
        ));
        controller.execute("/tail j stderr").await.unwrap();
        assert!(api.called("/jobs/j/tail?projectId=p&stream=stderr"));
        assert_eq!(api.last().path, "/jobs/j/tail?projectId=p&stream=stderr");
        let value = controller.inspector_rows()[0].fields[0].value.clone();
        assert!(value.contains("recent text"));
        assert!(length(&value) <= 16000);
        controller.execute("/job j").await.unwrap();
        assert!(controller.inspector_rows()[0].fields.contains(&ViewField {
            label: "Elapsed seconds".into(),
            value: "12".into(),
        }));
    });
}

#[test]
fn t38_bounds_rendered_history_strips_complete_osc_ansi_and_ignores_other_project_events() {
    let (controller, _) = setup();
    let message = obj(vec![("message", "x".repeat(16000).into())]);
    controller.state_mut().events = (1..=400)
        .map(|sequence| agent_event(Some(sequence), "agent.completed", message.clone(), "a"))
        .collect();
    let entries = controller.transcript_entries();
    assert!(
        entries
            .iter()
            .map(|entry| length(&entry.text))
            .sum::<usize>()
            <= 64000
    );
    assert!(entries.len() <= 120);
    assert_eq!(
        safe_text_str("before\x1b]52;c;secret\x07\x1b[31mred\x1b[0m\rafter\u{202e}"),
        "beforeredafter"
    );
    controller.state_mut().events = vec![event(1, "other")];
    assert_eq!(controller.transcript_entries(), vec![]);
}

// ---------------------------------------------------------------------------------------------
// project-scoped concurrency
// ---------------------------------------------------------------------------------------------

/// `it.each([...])('ignores late %s response after project switch')`.
async fn ignores_late_response_after_project_switch(command: &str) {
    let (controller, api) = setup();
    if command == "/confirm" {
        controller.execute("/cancel j").await.unwrap();
    }
    let resolve: Resolver = api.always_pending();
    let pending = start_execute(&controller, command).await;
    controller.select_project("q", false).unwrap();
    resolve.resolve(json(
        r#"{"jobs":[{"jobId":"stale","state":"running"}],"missions":[],"directions":[],"executions":[],"agents":[],"proposals":[],"contract":null,"project":{"projectId":"new","repositoryRoot":"/new"}}"#,
    ));
    pending.await.unwrap().unwrap();
    assert_eq!(controller.state().project_id, "q");
    assert_eq!(controller.state().view, "chat");
    assert_eq!(controller.state().detail, "");
    assert_eq!(controller.inspector_rows(), vec![]);
}

#[test]
fn t39_ignores_late_jobs_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("/jobs"));
}

#[test]
fn t40_ignores_late_status_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("/status"));
}

#[test]
fn t41_ignores_late_approvals_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("/approvals"));
}

#[test]
fn t42_ignores_late_job_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("/job j"));
}

#[test]
fn t43_ignores_late_tail_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("/tail j"));
}

#[test]
fn t44_ignores_late_chat_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("hello"));
}

#[test]
fn t45_ignores_late_confirm_response_after_project_switch() {
    local(ignores_late_response_after_project_switch("/confirm"));
}

#[test]
fn t46_ignores_late_open_response_after_project_switch() {
    local(ignores_late_response_after_project_switch(
        r#"/open {"path":"/new","workingTitle":"New","createRepository":false}"#,
    ));
}

#[test]
fn t47_does_not_restore_the_old_view_or_error_after_a_stale_command_failure() {
    local(async {
        let (controller, api) = setup();
        let reject = api.always_pending();
        {
            let mut state = controller.state_mut();
            state.view = "help".into();
            state.detail = "old help".into();
        }
        let pending = start_execute(&controller, "/jobs").await;
        controller.select_project("q", false).unwrap();
        reject.reject("old error");
        assert!(error_of(pending.await.unwrap()).contains("old error"));
        assert_eq!(controller.state().view, "chat");
        assert_eq!(controller.state().detail, "");
        assert_eq!(controller.state().error, "");
    });
}

#[test]
fn t48_stages_a_project_contract_amendment_from_a_json_file() {
    local(async {
        let dir = temp_dir("nosh-amend-");
        let file = dir.join("contract.json");
        std::fs::write(&file, r#"{"contractVersion":3}"#).unwrap();
        let (controller, _) = setup();
        controller
            .execute(&format!("/amend-contract {}", file.display()))
            .await
            .unwrap();
        let action = controller.pending_action().unwrap();
        assert_eq!(action.path, "/projects/p/contract/amend");
        assert_eq!(
            action.body.get("contract"),
            Some(&json(r#"{"contractVersion":3}"#))
        );
        assert!(
            error_of(controller.execute("/amend-contract").await).contains("Use /amend-contract")
        );
        std::fs::remove_dir_all(dir).unwrap();
    });
}

#[test]
fn t49_stages_a_mission_steer_with_the_full_message_text() {
    local(async {
        let (controller, _) = setup();
        controller
            .execute("/steer m 4 Keep  scope narrow; report blockers.")
            .await
            .unwrap();
        let action = controller.pending_action().unwrap();
        assert_eq!(action.path, "/missions/m/steer");
        assert_eq!(action.body.get("expectedVersion"), Some(&4.0.into()));
        assert_eq!(
            action.body.get("message"),
            Some(&"Keep scope narrow; report blockers.".into())
        );
        assert!(error_of(controller.execute("/steer m 4").await).contains("Use /steer"));
    });
}

#[test]
fn t50_stages_a_blocked_node_retry_as_a_versioned_node_transition_to_ready() {
    local(async {
        let (controller, _) = setup();
        controller
            .execute("/retry directions d 7 dnode_x")
            .await
            .unwrap();
        let action = controller.pending_action().unwrap();
        assert_eq!(action.path, "/directions/d/nodes/dnode_x/transition");
        assert_eq!(action.body.get("expectedVersion"), Some(&7.0.into()));
        assert_eq!(action.body.get("next"), Some(&"ready".into()));
        assert!(
            error_of(controller.execute("/retry autoresearch a 1 n").await).contains("Use /retry")
        );
    });
}

#[test]
fn t51_returns_to_the_view_an_action_was_staged_from_when_it_is_discarded() {
    local(async {
        let (controller, _) = setup();
        controller.state_mut().view = "map".into();
        controller
            .execute("/retry missions m 7 mnode_x")
            .await
            .unwrap();
        assert_eq!(controller.state().view, "action");
        controller.execute("/discard").await.unwrap();
        assert_eq!(controller.state().view, "map");
        assert_eq!(controller.pending_action(), None);
    });
}

#[test]
fn t52_never_retries_a_failed_confirmation_and_keeps_its_original_project_version_idempotency() {
    local(async {
        let (controller, api) = setup();
        controller
            .execute("/transition missions m 3 paused")
            .await
            .unwrap();
        let action = controller.pending_action().unwrap();
        api.once_error("lost response");
        assert!(error_of(controller.execute("/confirm").await).contains("lost response"));
        assert_eq!(controller.pending_action(), None);
        assert!(error_of(controller.execute("/confirm").await).contains("No staged action"));
        assert_eq!(api.calls().len(), 1);
        let call = api.last();
        assert_eq!(call.path, action.path);
        assert_eq!(call.body, Some(JsValue::Object(action.body)));
        assert_eq!(call.timeout, None);
    });
}

#[test]
fn t53_subscribes_only_after_http_bootstrap_and_closes_stale_project_callbacks_disposes() {
    local(async {
        let (controller, api) = setup();
        api.once(json(
            r#"{"projects":[{"projectId":"p","repositoryRoot":"/p"},{"projectId":"q","repositoryRoot":"/q"}]}"#,
        ))
        .once(json(r#"{"models":[]}"#))
        .once(obj(vec![("events", JsValue::Array(vec![event(10, "p")]))]));
        controller.initialize().await.unwrap();
        assert!(controller.state().models_checked);
        assert_eq!(controller.state().models, vec![]);
        assert_eq!(api.subscriptions(), vec![]);
        controller.poll().await;
        assert_eq!(api.subscriptions(), vec![("p".to_owned(), 10)]);
        let streaming = |controller: &Controller| {
            controller
                .transcript_entries()
                .iter()
                .any(|entry| entry.status.as_deref() == Some("streaming"))
        };
        api.emit(&json(
            r#"{"eventId":"delta","sequence":null,"type":"agent.text_delta","payload":{"delta":"draft"},"scope":{"projectId":"p","agentId":"a"}}"#,
        ));
        assert!(streaming(&controller));
        api.emit_state(StreamState::Disconnected);
        assert!(!streaming(&controller));
        controller.select_project("q", false).unwrap();
        assert_eq!(api.closes(), 1);
        api.emit(&event(11, "p"));
        assert_eq!(controller.state().events, vec![]);
        controller.dispose();
    });
}
