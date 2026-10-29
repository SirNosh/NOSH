//! The workspace smoke test, ported one for one from `apps/tui/src/smoke.ts` on a ratatui
//! `TestBackend` of 100x30 (then 60x20).
//!
//! Specified by tui-workspace.md §9.1 (steps 0-19, `checkMapMouse` a-g, the map fixture, the
//! verbatim failure messages; CA-6 and DV-4: this is the only smoke test) and §9.3 (harness:
//! `render::frame_text`, crossterm events fed to `Workspace::handle_event`, "wait 50 ms" = yield to
//! the `LocalSet` and drain `Completion`s), with the fixture and input-helper notes of
//! tui-dialogs-visual.md §8.2. The client is a real `DaemonClient` for `http://127.0.0.1` with
//! session token `test`; no request is made. `checkNativeTranscript` draws a standalone transcript;
//! its "no renderer warnings" step has no Rust counterpart.

use std::rc::Rc;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use nosh_tui::TuiConfig;
use nosh_tui::controller::Controller;
use nosh_tui::js::js_json_parse;
use nosh_tui::render::frame_text;
use nosh_tui::transport::{DaemonApi, DaemonClient, install_crypto_provider};
use nosh_tui::view_model::{EntryKind, TranscriptEntry};
use nosh_tui::visual::Transcript;
use nosh_tui::workspace::{Completion, Workspace, nosh_version};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use regex::Regex;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const MAP: &str = r#"{ "generatedAt": "2026-10-03T12:00:00.000Z",
  "workers": [{ "kind": "worker", "id": "agt_1", "title": "general worker", "state": "running nosh_run", "detail": "gpt-6-luna:low", "children": [] }],
  "roots": [{ "kind": "mission", "id": "mis_1", "title": "Boundary tests", "state": "running", "detail": "v4", "version": 4, "progress": { "done": 1, "total": 2 }, "children": [
    { "kind": "node", "id": "n1", "title": "Write the boundary tests for every calibrator and both prevalences", "state": "failed", "detail": "implementation · attempt 3/3", "children": [] },
    { "kind": "node", "id": "n2", "title": "Final review", "state": "ready", "detail": "final review", "children": [] },
    { "kind": "node", "id": "n3", "title": "Collect sources", "state": "accepted", "detail": "librarian", "children": [] },
    { "kind": "node", "id": "n4", "title": "Draft the paper", "state": "accepted", "detail": "implementation", "children": [] } ] }] }"#;

struct Smoke {
    terminal: Terminal<TestBackend>,
    workspace: Workspace,
    controller: Rc<Controller>,
    completions: UnboundedReceiver<Completion>,
}

impl Smoke {
    /// `test.renderOnce()` + `captureCharFrame()`.
    fn render(&mut self) -> String {
        let workspace = &mut self.workspace;
        self.terminal
            .draw(|frame| workspace.draw(frame))
            .expect("draw");
        frame_text(self.terminal.backend().buffer())
    }

    /// `setTimeout(50)`: lets spawned commands finish and applies their completions.
    async fn wait(&mut self) {
        for _ in 0..32 {
            tokio::task::yield_now().await;
            while let Ok(completion) = self.completions.try_recv() {
                self.workspace.handle_completion(completion);
            }
        }
    }

    fn event(&mut self, event: Event) {
        self.workspace.handle_event(&event);
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.event(Event::Key(KeyEvent::new(code, modifiers)));
    }

    fn press(&mut self, code: KeyCode) {
        self.key(code, KeyModifiers::NONE);
    }

    fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.press(KeyCode::Char(character));
        }
    }

    fn mouse(&mut self, kind: MouseEventKind, x: i64, y: i64) {
        self.event(Event::Mouse(MouseEvent {
            kind,
            column: x as u16,
            row: y as u16,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn view(&self) -> String {
        self.controller.state().view.clone()
    }

    fn composer(&self) -> String {
        self.workspace.composer_value()
    }
}

/// `at(frame, needle)`: the column and row of the first row containing `needle` (`y = -1` if none).
fn at(frame: &str, needle: &str) -> (i64, i64) {
    frame
        .split('\n')
        .enumerate()
        .find_map(|(y, row)| {
            row.find(needle)
                .map(|index| (row[..index].chars().count() as i64, y as i64))
        })
        .unwrap_or((-1, -1))
}

#[test]
fn workspace_smoke() {
    let version = nosh_version();
    assert!(
        Regex::new(r"^\d+\.\d+\.\d+$").unwrap().is_match(version),
        "TUI could not read the NOSH version (got \"{version}\")"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    tokio::task::LocalSet::new().block_on(&runtime, smoke(version));
}

async fn smoke(version: &str) {
    install_crypto_provider();
    let client = Rc::new(
        DaemonClient::new(&TuiConfig {
            base_url: "http://127.0.0.1".into(),
            session_token: Some("test".into()),
            ..TuiConfig::default()
        })
        .expect("client"),
    );
    let controller = Controller::new(client.clone(), None);
    let (sender, completions) = unbounded_channel();
    let mut test = Smoke {
        terminal: Terminal::new(TestBackend::new(100, 30)).expect("terminal"),
        workspace: Workspace::new(controller.clone(), sender),
        controller: controller.clone(),
        completions,
    };
    let footer = format!("nosh {version}");

    let frame = test.render();
    assert!(
        frame.contains("███╗   ██╗")
            && frame.contains(&footer)
            && frame.contains("Choose a project first"),
        "OpenTUI workspace did not render"
    );
    test.type_text("research draft");
    test.key(KeyCode::Enter, KeyModifiers::SHIFT);
    test.render();
    assert!(
        test.composer().contains('\n'),
        "Shift+Enter must insert a newline"
    );
    let draft = test.composer();
    test.key(KeyCode::Char('p'), KeyModifiers::CONTROL);
    let frame = test.render();
    assert!(
        test.workspace.dialogs().is_open() && frame.contains("Commands"),
        "Ctrl+P must open palette"
    );
    test.type_text("model");
    let frame = test.render();
    assert!(
        frame.contains("Select model"),
        "Palette must filter command choices"
    );
    assert!(
        test.composer() == draft,
        "Dialog typing leaked into composer"
    );
    test.press(KeyCode::Esc);
    test.render();
    assert!(
        !test.workspace.dialogs().is_open() && test.composer() == draft,
        "Escape must restore unchanged composer"
    );
    test.workspace.edit_composer("");
    test.type_text("/");
    test.render();
    assert!(
        test.workspace.dialogs().is_open(),
        "Slash must open command search"
    );
    test.press(KeyCode::Esc);
    test.render();
    test.type_text("model example example low");
    test.render();
    assert!(
        !test.workspace.dialogs().is_open() && test.composer().starts_with("/model "),
        "Dismissed slash menu must permit raw commands"
    );
    test.workspace.edit_composer("");
    test.type_text("/help");
    test.render();
    test.press(KeyCode::Enter);
    test.wait().await;
    test.render();
    assert!(
        test.view() == "help" && test.composer().is_empty(),
        "A command chosen from the slash menu must consume its composer trigger"
    );
    test.press(KeyCode::Esc);
    test.wait().await;
    test.render();
    assert!(test.view() == "chat", "Esc must return to the conversation");
    test.press(KeyCode::Up);
    test.wait().await;
    test.render();
    assert!(
        test.composer() == "/help" && !test.workspace.dialogs().is_open(),
        "History recall of a slash command must not reopen slash search (and Esc navigation is not history)"
    );
    test.press(KeyCode::Esc);
    test.press(KeyCode::Esc);
    test.render();
    assert!(
        test.composer().is_empty(),
        "Esc Esc must clear a recalled draft"
    );
    test.workspace.dialogs_mut().open_project_form(false);
    let frame = test.render();
    assert!(frame.contains("Repository path"), "Project form missing");
    test.press(KeyCode::Esc);
    test.render();
    {
        let mut state = controller.state_mut();
        state.view = "help".into();
        state.detail = "Native help check".into();
    }
    let frame = test.render();
    assert!(
        frame.contains("ctrl+b hide"),
        "Session sidebar missing at wide size"
    );
    controller.state_mut().research_map = Some(js_json_parse(MAP).expect("fixture"));
    controller.state_mut().view = "map".into();
    test.workspace.edit_composer("");
    let frame = test.render();
    assert!(
        [
            "RESEARCH MAP",
            "MISSION  1/1  running  Boundary tests",
            "#0 MISSION",
            "Boundary tests",
            "┌",
            "▼",
            "▰▰▰▱▱▱ 1/2",
            "workers",
        ]
        .iter()
        .all(|needle| frame.contains(needle)),
        "Research map did not render as a graph:\n{frame}"
    );
    // Down selects the failed node under the Mission; Enter fills the composer with its retry,
    // applied only after confirmation.
    test.press(KeyCode::Down);
    test.render();
    let frame = test.render();
    // In a 30-row terminal the canvas pans to the selected box, bringing its state line into view.
    assert!(
        frame.contains("✗ failed · attempt 3/3")
            && frame.contains("enter  /retry missions mis_1 4 n1"),
        "Selecting a failed map node must offer its retry:\n{frame}"
    );
    test.press(KeyCode::Enter);
    test.render();
    assert!(
        test.composer() == "/retry missions mis_1 4 n1"
            && test.view() == "map"
            && controller.pending_action().is_none(),
        "Enter on a failed map node must only fill the composer"
    );
    test.workspace.edit_composer("");
    check_map_mouse(&mut test);
    test.terminal.backend_mut().resize(60, 20);
    let frame = test.render();
    assert!(
        frame.contains(&footer) && !frame.contains("ctrl+b hide"),
        "Narrow layout must hide sidebar"
    );
    test.workspace.dispose();
    test.terminal.backend_mut().resize(100, 30);
    check_native_transcript(&mut test.terminal);
    test.workspace.dispose();
    controller.dispose();
    client.close();
    println!(
        "OpenTUI native smoke passed: home/session, multiline input, searchable palette, modal focus, form, research map, map mouse zoom/drag/click, resize, transcript reconciliation."
    );
}

/// The map behaves like an image: the wheel zooms at the pointer, a drag moves it, a click selects
/// a box. `leaked` counts wheel events that reached the conversation scroll area.
fn check_map_mouse(test: &mut Smoke) {
    let leaked = |test: &Smoke| test.workspace.conversation_wheel_events();
    let full_title = Regex::new(r"tests for every\s*│").unwrap();
    let frame = test.render();
    let tag = at(&frame, "#1");
    assert!(
        tag.1 >= 0,
        "The selected box's tag must be on screen:\n{frame}"
    );
    test.mouse(MouseEventKind::ScrollDown, tag.0, tag.1);
    let frame = test.render();
    assert!(
        frame
            .chars()
            .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
            && frame.contains("50%")
            && leaked(test) == 0,
        "Wheel down must zoom the map out like an image, not scroll the conversation:\n{frame}"
    );
    let small = at(&frame, "#1");
    test.mouse(MouseEventKind::ScrollUp, small.0, small.1);
    test.mouse(MouseEventKind::ScrollUp, small.0, small.1);
    let frame = test.render();
    // Zoomed in, the box itself holds the long title's first 40-column line (up to its right
    // border), which the normal box cuts at 26.
    assert!(
        frame.contains("150%") && full_title.is_match(&frame),
        "Wheel up must zoom in to whole titles:\n{frame}"
    );
    let full = at(&frame, "#1");
    test.mouse(MouseEventKind::ScrollDown, full.0, full.1);
    let frame = test.render();
    assert!(
        frame.contains("100%") && !full_title.is_match(&frame),
        "Wheel down must return to normal boxes:\n{frame}"
    );
    let before = at(&frame, "#2");
    let left = MouseButton::Left;
    test.mouse(MouseEventKind::Down(left), before.0, before.1);
    for step in 1..=5 {
        let x = (before.0 as f64 - 4.0 * f64::from(step) / 5.0).round() as i64;
        test.mouse(MouseEventKind::Drag(left), x, before.1);
    }
    test.mouse(MouseEventKind::Up(left), before.0 - 4, before.1);
    let frame = test.render();
    let after = at(&frame, "#2");
    assert!(
        after.1 == before.1 && after.0 == before.0 - 4,
        "A drag must move the map with the pointer (#2 from {} to {}):\n{frame}",
        before.0,
        after.0
    );
    test.mouse(MouseEventKind::Down(left), after.0, after.1);
    test.mouse(MouseEventKind::Up(left), after.0, after.1);
    let frame = test.render();
    // The Rust port has no in-app text selection, so "no text selection exists" always holds.
    assert!(
        frame.contains("graph node · ready · final review · n2"),
        "A click must select the box under the pointer without selecting text:\n{frame}"
    );
    test.type_text("x");
    test.render();
    assert!(
        test.composer() == "x" && leaked(test) == 0,
        "The composer must keep focus after map clicks"
    );
    test.workspace.edit_composer("");
}

fn entry(id: &str, text: &str) -> TranscriptEntry {
    TranscriptEntry {
        id: id.into(),
        kind: EntryKind::Assistant,
        title: "Assistant".into(),
        text: text.into(),
        sequence: None,
        agent_id: None,
        model: None,
        thinking_level: None,
        status: Some("streaming".into()),
    }
}

/// A standalone transcript drawn from the top of the screen.
fn draw(terminal: &mut Terminal<TestBackend>, transcript: &mut Transcript) -> String {
    terminal
        .draw(|frame| {
            let area = frame.area();
            for (row, line) in transcript
                .lines(area.width)
                .iter()
                .take(usize::from(area.height))
                .enumerate()
            {
                frame.render_widget(line, Rect::new(0, row as u16, area.width, 1));
            }
        })
        .expect("draw");
    frame_text(terminal.backend().buffer())
}

fn check_native_transcript(terminal: &mut Terminal<TestBackend>) {
    let mut transcript = Transcript::new();
    let check_order = |transcript: &Transcript, ids: &[&str]| {
        let expected: Vec<String> = ids.iter().map(|id| format!("message-{id}")).collect();
        assert!(
            transcript.ids() == expected,
            "Native transcript order differs from replay order"
        );
    };
    transcript.update(&[entry("a", "Alpha"), entry("live", "Draft")]);
    draw(terminal, &mut transcript);
    let (a, live) = (
        transcript.block_serial("a").unwrap(),
        transcript.block_serial("live").unwrap(),
    );
    let body = transcript.body_serial("live");
    transcript.update(&[
        entry("a", "Alpha"),
        entry("live", "Streaming delta"),
        entry("b", "Beta"),
    ]);
    let frame = draw(terminal, &mut transcript);
    check_order(&transcript, &["a", "live", "b"]);
    assert!(
        transcript.body_serial("live") == body,
        "Streaming must preserve native body identity"
    );
    assert!(
        frame.contains("Streaming delta"),
        "Stream delta did not render"
    );
    transcript.update(&[
        entry("b", "Beta"),
        entry("a", "Alpha"),
        entry("live", "Streaming delta"),
    ]);
    draw(terminal, &mut transcript);
    check_order(&transcript, &["b", "a", "live"]);
    assert!(
        transcript.block_serial("a") == Some(a),
        "Reorder rebuilt an existing message"
    );
    let mut saved = entry("saved", "Final answer");
    saved.status = Some("complete".into());
    transcript.update(&[entry("b", "Beta"), entry("a", "Alpha"), saved]);
    let frame = draw(terminal, &mut transcript);
    check_order(&transcript, &["b", "a", "saved"]);
    assert!(
        !transcript.is_live(live),
        "Settled ephemeral block was not released"
    );
    let row = |needle: &str| at(&frame, needle).1;
    assert!(
        frame.contains("Beta")
            && frame.find("Beta") < frame.find("Alpha")
            && row("Beta") < row("Alpha")
            && row("Alpha") < row("Final answer"),
        "Native rendered rows do not match transcript order"
    );
    let entries: Vec<TranscriptEntry> = (0..=100)
        .map(|i| entry(&i.to_string(), &format!("Message {i}")))
        .collect();
    transcript.update(&entries[..100]);
    let (first, second) = (
        transcript.block_serial("0").unwrap(),
        transcript.block_serial("1").unwrap(),
    );
    transcript.update(&entries);
    draw(terminal, &mut transcript);
    let ids: Vec<String> = (1..=100).map(|i| i.to_string()).collect();
    check_order(
        &transcript,
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(
        transcript.ids().len() == 100
            && transcript.block_serial("1") == Some(second)
            && !transcript.is_live(first),
        "Native transcript must evict old rows and retain surviving nodes"
    );
    transcript.update(&[]);
    draw(terminal, &mut transcript);
    check_order(&transcript, &[]);
}
