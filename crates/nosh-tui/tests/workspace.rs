//! The workspace's deliberate deviation from TS, tested as tui-workspace.md CA-1 requires: only the
//! exact texts `/new`, `/open` and `/thinking` open a form, so Object.prototype names such as
//! `constructor` or `valueOf` are ordinary input (TS cleared the draft and sent nothing).
//!
//! Uses the scripted `DaemonApi` of `tests/common`; "wait" yields to the `LocalSet` and applies the
//! workspace completions.

mod common;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use nosh_tui::controller::{Controller, Project};
use nosh_tui::workspace::{Completion, Workspace};
use tokio::sync::mpsc::UnboundedReceiver;

use common::{MockApi, json, local, settle};

fn type_and_submit(workspace: &mut Workspace, text: &str) {
    for character in text.chars() {
        let key = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
        workspace.handle_event(&Event::Key(key));
    }
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    workspace.handle_event(&Event::Key(enter));
}

async fn wait(workspace: &mut Workspace, completions: &mut UnboundedReceiver<Completion>) {
    settle().await;
    while let Ok(completion) = completions.try_recv() {
        workspace.handle_completion(completion);
    }
}

#[test]
fn an_object_prototype_name_is_sent_as_chat_and_cleared_only_after_it_resolves() {
    local(async {
        let api = MockApi::new();
        let reply = api.once_pending();
        let controller = Controller::new(api.clone(), None);
        {
            let mut state = controller.state_mut();
            state.projects = vec![Project {
                project_id: "prj_1".into(),
                repository_root: "C:/repo".into(),
            }];
            state.project_id = "prj_1".into();
        }
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        let mut workspace = Workspace::new(controller, sender);
        type_and_submit(&mut workspace, "constructor");
        wait(&mut workspace, &mut completions).await;
        let call = api.last();
        assert_eq!(call.path, "/chat");
        let body = call.body.expect("a chat body");
        assert_eq!(
            body.get("message").and_then(|message| message.as_str()),
            Some("constructor")
        );
        assert_eq!(workspace.composer_value(), "constructor");
        reply.resolve(json("{}"));
        wait(&mut workspace, &mut completions).await;
        assert_eq!(workspace.composer_value(), "");
    });
}

#[test]
fn an_object_prototype_name_without_a_project_opens_the_picker_and_keeps_the_draft() {
    local(async {
        let api = MockApi::new();
        let controller = Controller::new(api.clone(), None);
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        let mut workspace = Workspace::new(controller, sender);
        type_and_submit(&mut workspace, "valueOf");
        assert!(workspace.dialogs().is_open());
        assert_eq!(workspace.composer_value(), "valueOf");
        wait(&mut workspace, &mut completions).await;
        assert!(api.calls().iter().all(|call| call.path != "/chat"));
        assert_eq!(workspace.composer_value(), "valueOf");
    });
}
