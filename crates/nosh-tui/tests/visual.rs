//! Visual component tests, ported one for one from `apps/tui/src/visual.test.ts` ("transcript
//! reconciliation" cases 1-4, "research sidebar" case 5).
//!
//! Specified by tui-dialogs-visual.md §8.1 (the cases and the Rust harness: `UpdateStats`, block and
//! body serials, `is_live`, the sidebar as `(id, Line)` items with chunk colors). The TS `add` spy
//! count is `created + moved`; object identity is the block serial.

use nosh_tui::theme::THEME;
use nosh_tui::view_model::{EntryKind, MapLine, MapTone, TranscriptEntry, ViewRow};
use nosh_tui::visual::{AttentionItem, ResearchSidebar, Transcript};

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

fn entries(ids: &[&str]) -> Vec<TranscriptEntry> {
    ids.iter().map(|id| entry(id, id)).collect()
}

fn adds(transcript: &mut Transcript, entries: &[TranscriptEntry]) -> usize {
    let stats = transcript.update(entries);
    stats.created + stats.moved
}

/// The block serials in display order (`root.getChildren()`).
fn children(transcript: &Transcript) -> Vec<u64> {
    transcript
        .ids()
        .iter()
        .map(|id| {
            transcript
                .block_serial(id.strip_prefix("message-").expect("message id"))
                .expect("a block per id")
        })
        .collect()
}

#[test]
fn does_not_reinsert_correct_children_on_initial_render_append_or_replay() {
    let mut transcript = Transcript::new();
    assert_eq!(adds(&mut transcript, &entries(&["a", "b"])), 2);
    let original = children(&transcript);
    assert_eq!(adds(&mut transcript, &entries(&["a", "b", "c"])), 1);
    assert_eq!(adds(&mut transcript, &entries(&["a", "b", "c"])), 0);
    assert_eq!(children(&transcript)[..2], original[..]);
    assert_eq!(transcript.ids(), ["message-a", "message-b", "message-c"]);
}

#[test]
fn updates_streaming_text_in_place_and_reorders_only_misplaced_nodes() {
    let mut transcript = Transcript::new();
    transcript.update(&entries(&["a", "b", "c"]));
    let [a, b, c] = children(&transcript)[..] else {
        panic!("three blocks");
    };
    let body = transcript.body_serial("b");
    let update = [entry("a", "a"), entry("b", "stream delta"), entry("c", "c")];
    assert_eq!(adds(&mut transcript, &update), 0);
    assert_eq!(transcript.body_serial("b"), body);
    assert_eq!(
        transcript.body_content("b").as_deref(),
        Some("stream delta")
    );
    assert_eq!(transcript.body_streaming("b"), Some(true));
    let mut complete = entry("b", "complete");
    complete.status = Some("complete".into());
    assert_eq!(
        adds(
            &mut transcript,
            &[entry("c", "c"), entry("a", "a"), complete]
        ),
        1
    );
    assert_eq!(children(&transcript), [c, a, b]);
    assert_eq!(transcript.body_serial("b"), body);
    assert_eq!(transcript.body_content("b").as_deref(), Some("complete"));
    assert_eq!(transcript.body_streaming("b"), Some(false));
    transcript.update(&entries(&["b", "c", "a"]));
    assert_eq!(children(&transcript), [b, c, a]);
}

#[test]
fn settles_ephemeral_messages_and_inserts_replay_entries_without_rebuilding_survivors() {
    let mut transcript = Transcript::new();
    transcript.update(&entries(&["a", "live"]));
    let [a, live] = children(&transcript)[..] else {
        panic!("two blocks");
    };
    transcript.update(&entries(&["earlier", "a", "persisted"]));
    assert_eq!(
        transcript.ids(),
        ["message-earlier", "message-a", "message-persisted"]
    );
    assert_eq!(children(&transcript)[1], a);
    assert!(transcript.is_live(a));
    assert!(!transcript.is_live(live));
}

#[test]
fn evicts_the_bounded_replay_window_and_clears_without_self_insertion() {
    let mut transcript = Transcript::new();
    let mut window: Vec<TranscriptEntry> = (0..100)
        .map(|i| entry(&i.to_string(), &i.to_string()))
        .collect();
    transcript.update(&window);
    let [first, second] = children(&transcript)[..2] else {
        panic!("a hundred blocks");
    };
    window.push(entry("100", "100"));
    transcript.update(&window);
    assert_eq!(children(&transcript).len(), 100);
    assert_eq!(children(&transcript)[0], second);
    assert!(!transcript.is_live(first));
    assert_eq!(
        transcript.ids().last().map(String::as_str),
        Some("message-100")
    );
    transcript.update(&[]);
    assert!(transcript.ids().is_empty());
    assert!(!transcript.is_live(second));
    transcript.update(&entries(&["new-project"]));
    assert_eq!(transcript.ids(), ["message-new-project"]);
}

#[test]
fn shows_the_climb_graph_under_needs_you_and_above_the_lists_one_coloured_row_per_line() {
    let climb = [
        MapLine {
            prefix: String::new(),
            glyph: String::new(),
            tone: MapTone::Success,
            label: "CLIMB".into(),
            title: "completed".into(),
            state: String::new(),
            detail: String::new(),
            heading: Some(true),
        },
        MapLine {
            prefix: "│ ".into(),
            glyph: "✗".into(),
            tone: MapTone::Error,
            label: String::new(),
            title: "L2 1e-4 ".into(),
            state: "0.01597".into(),
            detail: String::new(),
            heading: None,
        },
    ];
    let row = ViewRow {
        id: "m".into(),
        section: "missions".into(),
        title: "Spam".into(),
        state: Some("blocked".into()),
        version: None,
        subtitle: None,
        fields: Vec::new(),
    };
    let attention = [AttentionItem {
        title: "Staged".into(),
        hint: "/confirm".into(),
    }];
    let mut sidebar = ResearchSidebar::new();
    sidebar.update(&[row], &attention, &climb);
    let items = sidebar.items(28);
    let order: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
    let index = |id: &str| order.iter().position(|item| *item == id).expect(id);
    assert!(index("side-climb-0") > index("side-attention-0"));
    assert!(index("side-climb-1") < index("side-section-missions"));
    let (_, line) = &items[index("side-climb-1")];
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(text, "│ ✗ L2 1e-4 0.01597");
    let colors: Vec<_> = line
        .spans
        .iter()
        .filter(|span| !span.content.is_empty())
        .map(|span| (span.content.as_ref(), span.style.fg))
        .collect();
    assert_eq!(
        colors,
        [
            ("│ ", Some(THEME.muted)),
            ("✗ ", Some(THEME.error)),
            ("L2 1e-4 ", Some(THEME.text)),
            ("0.01597", Some(THEME.error)),
        ]
    );
}
