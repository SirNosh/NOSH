//! `apps/tui/src/map-graph.test.ts` one for one (tui-viewmodel.md §5.1), then the byte-exact
//! goldens of §6 probed from the TS code (Node 22.23.1, beautiful-mermaid 1.1.3) in `goldens/`.

use std::rc::Rc;

use nosh_tui::js::{JsValue, js_json_parse, json_stringify};
use nosh_tui::map_graph::{
    GraphBox, GraphDetail, GraphDirection, GraphLayout, box_at, layout_graph, mermaid_source,
    move_selection, shrink_graph, suggested_command,
};

fn json(text: &str) -> JsValue {
    js_json_parse(text).expect("fixture JSON")
}

/// The shared fixture `root` of map-graph.test.ts.
fn root() -> JsValue {
    json(
        r#"{"kind":"mission","id":"mis_x","title":"Trustworthy spam probabilities","state":"blocked","detail":"v113","version":113,"progress":{"done":4,"total":6},
        "children":[
          {"kind":"node","id":"mnode_dir","title":"Research Direction 1: answer one question","state":"accepted","detail":"research direction","children":[
            {"kind":"direction","id":"dir_y","title":"Which calibrator has the \"lowest\" ECE? 🧪","state":"blocked","detail":"v25","version":25,"children":[
              {"kind":"node","id":"dnode_eval","title":"Evaluate bounded hypothesis families","state":"failed","detail":"general worker · attempt 3/3","children":[
                {"kind":"autoresearch","id":"ar_z","title":"How low can calibration push ECE","state":"completed","detail":"round 2/4 · best 0.005037866","children":[
                  {"kind":"experiment","id":"exp_a","title":"Histogram binning","state":"promoted","detail":"round 1 · score 0.005037866372912155 (+0.0136)"},
                  {"kind":"experiment","id":"exp_b","title":"Platt scaling","state":"rejected","detail":"round 1 · score 0.0115 (-0.0065)"}]}]}]}]},
          {"kind":"node","id":"mnode_impl","title":"Produce the bounded deliverable","state":"failed","detail":"implementation · attempt 8/8","children":[]},
          {"kind":"node","id":"mnode_done","title":"Collect and cite the sources","state":"accepted","detail":"librarian","children":[]},
          {"kind":"job","id":"job_q","title":"node --test","state":"running","detail":"native"}]}"#,
    )
}

fn field<'a>(graph_box: &'a GraphBox, key: &str) -> Option<&'a str> {
    graph_box.node.get(key).and_then(JsValue::as_str)
}

/// `node.id ?? node.kind`.
fn name(graph_box: &GraphBox) -> &str {
    field(graph_box, "id")
        .or_else(|| field(graph_box, "kind"))
        .unwrap_or_default()
}

fn position(layout: &GraphLayout, id: &str) -> usize {
    layout
        .boxes
        .iter()
        .position(|graph_box| field(graph_box, "id") == Some(id))
        .unwrap_or_else(|| panic!("a box for {id}"))
}

/// `box(boxes, id)`.
fn find<'a>(layout: &'a GraphLayout, id: &str) -> &'a GraphBox {
    &layout.boxes[position(layout, id)]
}

/// Rows `rows` of the picture, each sliced to `columns`, joined with a space.
fn slice(
    layout: &GraphLayout,
    rows: std::ops::Range<usize>,
    columns: std::ops::Range<usize>,
) -> String {
    let rows: Vec<String> = layout.lines[rows]
        .iter()
        .map(|line| {
            line.chars()
                .skip(columns.start)
                .take(columns.len())
                .collect()
        })
        .collect();
    rows.join(" ")
}

/// `inBox(id)`: the box's rows without its bottom border, border columns included.
fn in_box(layout: &GraphLayout, id: &str) -> String {
    let graph_box = find(layout, id);
    slice(
        layout,
        graph_box.top..graph_box.bottom,
        graph_box.left..graph_box.right + 1,
    )
}

/// `words(layout, id)`: `inBox` without box characters, whitespace collapsed.
fn words(layout: &GraphLayout, id: &str) -> String {
    let plain: String = in_box(layout, id)
        .chars()
        .map(|c| {
            if "│┌┐└┘─┬┴├┤".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The label lines of a box, trimmed, blank padding rows left out.
fn label(layout: &GraphLayout, id: &str) -> Vec<String> {
    let graph_box = find(layout, id);
    (graph_box.top + 1..graph_box.bottom)
        .map(|row| slice(layout, row..row + 1, graph_box.left + 1..graph_box.right))
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

fn at(layout: &GraphLayout, row: usize, column: usize) -> char {
    layout.lines[row].chars().nth(column).expect("a cell")
}

#[test]
fn lays_every_map_node_out_in_exactly_one_box_that_holds_its_title_with_no_overlapping_boxes() {
    let layout = layout_graph(Some(&root()), GraphDetail::Normal);
    let mut ids: Vec<&str> = layout
        .boxes
        .iter()
        .filter_map(|graph_box| field(graph_box, "id"))
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        [
            "ar_z",
            "dir_y",
            "dnode_eval",
            "job_q",
            "mis_x",
            "mnode_dir",
            "mnode_done",
            "mnode_impl"
        ]
    );
    let research = find(&layout, "ar_z");
    assert!(research.opens);
    assert_eq!(
        layout
            .boxes
            .iter()
            .filter(|graph_box| graph_box.opens)
            .count(),
        1
    );
    let opened = layout_graph(Some(&research.node), GraphDetail::Normal);
    let names: Vec<&str> = opened.boxes.iter().map(name).collect();
    assert_eq!(names, ["ar_z", "baseline", "exp_a", "exp_b"]);
    assert!(opened.boxes.iter().all(|graph_box| !graph_box.opens));
    for graph_box in &layout.boxes {
        assert_eq!(at(&layout, graph_box.top, graph_box.left), '┌');
        assert_eq!(at(&layout, graph_box.bottom, graph_box.right), '┘');
        let inside = slice(
            &layout,
            graph_box.top + 1..graph_box.bottom,
            graph_box.left + 1..graph_box.right,
        );
        assert!(
            inside.contains(&format!("#{}", graph_box.index)),
            "{inside}"
        );
    }
    let produce = in_box(&layout, "mnode_impl");
    assert!(produce.contains("Produce the bounded"), "{produce}");
    assert!(produce.contains("✗ failed · attempt 8/8"), "{produce}");
    let direction = in_box(&layout, "mnode_dir");
    assert!(direction.contains("Research Direction 1:"), "{direction}");
    assert!(direction.contains("answer one question"), "{direction}");
    assert_eq!(
        find(&layout, "dir_y").parent,
        Some(find(&layout, "mnode_dir").index)
    );
    assert_eq!(find(&opened, "exp_a").parent, Some(1));
    let mission = in_box(&layout, "mis_x");
    assert!(mission.contains("MISSION"), "{mission}");
    assert!(mission.contains("▰▰▰▰▱▱ 4/6"), "{mission}");
    for (i, a) in layout.boxes.iter().enumerate() {
        for b in &layout.boxes[i + 1..] {
            assert!(
                a.right < b.left || b.right < a.left || a.bottom < b.top || b.bottom < a.top,
                "{} overlaps {}",
                name(a),
                name(b)
            );
        }
    }
    let width = layout.lines[0].chars().count();
    assert!(
        layout
            .lines
            .iter()
            .all(|line| line.chars().count() == width)
    );
    assert!(
        !layout
            .lines
            .concat()
            .chars()
            .any(|c| ('\u{1F300}'..='\u{1FAFF}').contains(&c))
    );
}

#[test]
fn moves_the_selection_to_the_nearest_box_in_a_direction_and_stays_put_at_an_edge() {
    let layout = layout_graph(Some(&root()), GraphDetail::Normal);
    let boxes = &layout.boxes;
    let mission = position(&layout, "mis_x");
    assert_eq!(move_selection(boxes, mission, GraphDirection::Up), mission);
    let below = &boxes[move_selection(boxes, mission, GraphDirection::Down)];
    assert_eq!(field(below, "id"), Some("mnode_dir"));
    let direction = position(&layout, "mnode_dir");
    let right = &boxes[move_selection(boxes, direction, GraphDirection::Right)];
    assert!(right.left > boxes[direction].left);
    assert_eq!(field(right, "id"), Some("mnode_impl"));
    // The probed table of viewmodel §3.6: (up, down, left, right) from each box.
    let table = [
        ("mis_x", [0, 1, 0, 5]),
        ("mnode_dir", [0, 2, 1, 5]),
        ("dir_y", [1, 3, 2, 5]),
        ("dnode_eval", [2, 4, 3, 5]),
        ("ar_z", [3, 4, 4, 5]),
        ("mnode_impl", [0, 2, 1, 6]),
        ("mnode_done", [0, 2, 5, 7]),
        ("job_q", [0, 2, 6, 7]),
    ];
    for (id, expected) in table {
        let from = position(&layout, id);
        let moved = [
            GraphDirection::Up,
            GraphDirection::Down,
            GraphDirection::Left,
            GraphDirection::Right,
        ]
        .map(|direction| move_selection(boxes, from, direction));
        assert_eq!(moved, expected, "from {id}");
    }
    assert_eq!(move_selection(boxes, 99, GraphDirection::Down), 99);
}

#[test]
fn suggests_the_exact_staging_command_for_a_failed_node_or_a_blocked_root_and_nothing_for_finished_work()
 {
    let root = root();
    let layout = layout_graph(Some(&root), GraphDetail::Normal);
    let command = |layout: &GraphLayout, id: &str| suggested_command(find(layout, id));
    assert_eq!(
        command(&layout, "mnode_impl"),
        "/retry missions mis_x 113 mnode_impl"
    );
    assert_eq!(
        command(&layout, "dnode_eval"),
        "/retry directions dir_y 25 dnode_eval"
    );
    assert_eq!(
        command(&layout, "mis_x"),
        "/retry missions mis_x 113 mnode_impl"
    );
    assert_eq!(
        command(&layout, "dir_y"),
        "/retry directions dir_y 25 dnode_eval"
    );
    let mut retried = root.clone();
    if let JsValue::Object(object) = &mut retried
        && let Some(JsValue::Array(children)) = object.get_mut("children")
    {
        children.retain(|child| child.get("state").and_then(JsValue::as_str) != Some("failed"));
    }
    let retried = layout_graph(Some(&retried), GraphDetail::Normal);
    assert_eq!(
        command(&retried, "mis_x"),
        "/transition missions mis_x 113 running"
    );
    let direction = json(
        r#"{"kind":"direction","id":"dir_y","title":"Which calibrator","state":"blocked","detail":"v26","version":26,
        "children":[{"kind":"node","id":"dnode_eval","title":"Evaluate","state":"ready","detail":"general worker · attempt 3/4","children":[]}]}"#,
    );
    let direction = layout_graph(Some(&direction), GraphDetail::Normal);
    assert_eq!(
        command(&direction, "dir_y"),
        "/transition directions dir_y 26 active"
    );
    assert_eq!(command(&layout, "job_q"), "/job job_q");
    for id in ["mnode_done", "ar_z", "mnode_dir"] {
        assert_eq!(command(&layout, id), "", "{id}");
    }
    let opened = layout_graph(Some(&find(&layout, "ar_z").node), GraphDetail::Normal);
    assert_eq!(command(&opened, "exp_a"), "");
}

#[test]
fn zooms_in_to_whole_titles_and_the_full_detail_line() {
    let root = root();
    let full = layout_graph(Some(&root), GraphDetail::Full);
    assert!(words(&full, "dnode_eval").contains("general worker · attempt 3/3"));
    let mut long = root.clone();
    let title =
        "Compare temperature, Platt and histogram calibration at natural and shifted prevalence";
    if let JsValue::Object(object) = &mut long {
        object.set(
            "children",
            json(&format!(
                r#"[{{"kind":"node","id":"mnode_long","title":"{title}","state":"ready","detail":"implementation","children":[]}}]"#
            )),
        );
    }
    let normal = layout_graph(Some(&long), GraphDetail::Normal);
    assert!(words(&normal, "mnode_long").contains('…'));
    assert_eq!(
        label(&normal, "mnode_long"),
        [
            "#1",
            "Compare temperature, Platt",
            "and histogram calibration…",
            "○ ready"
        ]
    );
    let zoomed = layout_graph(Some(&long), GraphDetail::Full);
    assert!(words(&zoomed, "mnode_long").contains(title));
    assert_eq!(
        label(&zoomed, "mnode_long"),
        [
            "#1",
            "Compare temperature, Platt and histogram",
            "calibration at natural and shifted",
            "prevalence",
            "○ ready",
            "implementation"
        ]
    );
    let width = |layout: &GraphLayout| {
        let graph_box = find(layout, "mnode_dir");
        graph_box.right - graph_box.left
    };
    let normal = layout_graph(Some(&root), GraphDetail::Normal);
    assert_eq!((width(&full), width(&normal)), (39, 33));
}

#[test]
fn shrinks_the_graph_like_an_image() {
    let layout = layout_graph(Some(&root()), GraphDetail::Normal);
    for (factor, rows, columns) in [(2, 24, 51), (4, 12, 26), (8, 6, 13)] {
        let shrunk = shrink_graph(&layout, factor);
        assert_eq!(shrunk.lines.len(), rows);
        assert!(
            shrunk
                .lines
                .iter()
                .all(|line| line.chars().count() == columns)
        );
        assert_eq!(shrunk.owners.len(), rows);
        assert!(Rc::ptr_eq(&shrunk, &shrink_graph(&layout, factor)));
    }
    let shrunk = shrink_graph(&layout, 2);
    let index = position(&layout, "mis_x");
    let mission = &layout.boxes[index];
    let corner = shrunk.lines[mission.top / 2]
        .chars()
        .nth(mission.left / 2)
        .expect("a glyph");
    assert!(('\u{2801}'..='\u{28FF}').contains(&corner), "{corner}");
    assert_eq!(corner, '⡟');
    assert_eq!(shrunk.owners[0][0], index as i32);
    let blank = (0..layout.lines.len()).step_by(2).find(|&row| {
        layout.lines.get(row + 1).is_some()
            && slice(&layout, row..row + 2, 0..2).chars().all(|c| c == ' ')
    });
    assert_eq!(blank, Some(8));
    let blank = blank.expect("probed") / 2;
    assert_eq!(shrunk.lines[blank].chars().next(), Some(' '));
    assert_eq!(shrunk.owners[blank][0], -1);
    assert!(shrunk.lines.join("\n").contains("#0"));
    let leftover: String = shrink_graph(&layout, 8)
        .lines
        .concat()
        .chars()
        .filter(|&c| {
            !(('\u{2800}'..='\u{28FF}').contains(&c) || c == ' ' || c == '#' || c.is_ascii_digit())
        })
        .collect();
    assert_eq!(leftover, "");
}

#[test]
fn hit_tests_a_canvas_cell_to_the_box_drawn_there() {
    let layout = layout_graph(Some(&root()), GraphDetail::Normal);
    let target = position(&layout, "mnode_impl");
    let graph_box = &layout.boxes[target];
    let (left, top) = (graph_box.left as i64, graph_box.top as i64);
    let (right, bottom) = (graph_box.right as i64, graph_box.bottom as i64);
    assert_eq!(box_at(&layout.boxes, left + 1, top + 1), target as i32);
    assert_eq!(target, 5);
    assert_eq!(box_at(&layout.boxes, right, bottom), target as i32);
    assert_eq!(box_at(&layout.boxes, -5, -5), -1);
}

#[test]
fn is_empty_without_a_root() {
    let layout = layout_graph(None, GraphDetail::Normal);
    assert!(layout.lines.is_empty());
    assert!(layout.boxes.is_empty());
}

/// The opened Autoresearch of the fixture: `layoutGraph(box('ar_z').node)`.
fn research() -> JsValue {
    let layout = layout_graph(Some(&root()), GraphDetail::Normal);
    find(&layout, "ar_z").node.clone()
}

/// §6.1: the generated Mermaid sources, which are also the layout cache keys.
#[test]
fn generates_the_mermaid_sources_of_the_typescript_code() {
    let golden: Vec<&str> = include_str!("goldens/map-sources.txt")
        .split("\n=====\n")
        .collect();
    let (root, research) = (root(), research());
    let sources = [
        mermaid_source(Some(&root), GraphDetail::Normal),
        mermaid_source(Some(&root), GraphDetail::Full),
        mermaid_source(Some(&research), GraphDetail::Normal),
        mermaid_source(Some(&research), GraphDetail::Full),
    ];
    assert_eq!(sources, golden.as_slice());
}

/// `[index, id ?? kind, top, left, bottom, right, tone, opens, parent]` of every box.
fn box_list(layout: &GraphLayout) -> String {
    let rows: Vec<String> = layout
        .boxes
        .iter()
        .map(|b| {
            let parent = b
                .parent
                .map_or("null".to_owned(), |parent| parent.to_string());
            format!(
                "{} {} {},{},{},{} {} {} {parent}",
                b.index,
                name(b),
                b.top,
                b.left,
                b.bottom,
                b.right,
                b.tone.as_str(),
                b.opens
            )
        })
        .collect();
    rows.join("\n")
}

/// §6.2: the layouts of the fixture and of its opened Autoresearch, normal and full.
#[test]
fn draws_the_layouts_of_the_typescript_code() {
    let (root, research) = (root(), research());
    let cases = [
        (
            layout_graph(Some(&root), GraphDetail::Normal),
            include_str!("goldens/map-normal.txt"),
            "0 mis_x 0,0,6,33 error false null
1 mnode_dir 10,0,16,33 success false 0
2 dir_y 20,0,26,33 error false 1
3 dnode_eval 30,0,36,33 error false 2
4 ar_z 40,0,46,33 success true 3
5 mnode_impl 10,37,16,60 error false 0
6 mnode_done 10,64,16,85 success false 0
7 job_q 10,89,16,101 active false 0",
        ),
        (
            layout_graph(Some(&root), GraphDetail::Full),
            include_str!("goldens/map-full.txt"),
            "0 mis_x 0,0,6,39 error false null
1 mnode_dir 10,0,16,39 success false 0
2 dir_y 20,0,26,39 error false 1
3 dnode_eval 30,0,36,39 error false 2
4 ar_z 40,0,46,39 success true 3
5 mnode_impl 10,43,16,75 error false 0
6 mnode_done 10,79,16,108 success false 0
7 job_q 10,112,16,124 active false 0",
        ),
        (
            layout_graph(Some(&research), GraphDetail::Normal),
            include_str!("goldens/map-normal-opened.txt"),
            "0 ar_z 0,0,6,24 success false null
1 baseline 10,0,14,24 idle false 0
2 exp_a 18,0,22,24 success false 1
3 exp_b 18,28,22,48 error false 1",
        ),
        (
            layout_graph(Some(&research), GraphDetail::Full),
            include_str!("goldens/map-full-opened.txt"),
            "0 ar_z 0,0,6,41 success false null
1 baseline 10,0,14,41 idle false 0
2 exp_a 18,0,24,41 success false 1
3 exp_b 18,45,24,78 error false 1",
        ),
    ];
    for (layout, picture, boxes) in cases {
        assert_eq!(layout.lines.join("\n"), picture);
        assert_eq!(box_list(&layout), boxes);
    }
}

/// §6.3: the edge-case transcript of the TS probe, byte for byte.
#[test]
fn draws_the_edge_cases_of_the_typescript_code() {
    let mut out = String::new();
    let show = |out: &mut String, name: &str, value: Vec<JsValue>| {
        out.push_str(&format!(
            "{name} => {}\n",
            json_stringify(&JsValue::from(value))
        ));
    };
    let number = |value: usize| JsValue::from(value as f64);
    let nullable = |value: Option<usize>| value.map_or(JsValue::Null, number);
    let cases: [(&str, &str, GraphDetail); 15] = [
        (r#""title":"Short""#, "", GraphDetail::Normal),
        (
            r#""title":"A *very* important **bold** ~~gone~~ title""#,
            "",
            GraphDetail::Normal,
        ),
        (
            r#""title":"Literal \\n newline here""#,
            "",
            GraphDetail::Normal,
        ),
        (r#""title":"Bracket [x] in title""#, "", GraphDetail::Normal),
        (
            "\"title\":\"Emoji \u{1F9EA} and CJK \u{6F22}\u{5B57} and combining e\u{301} and zero\u{200B}width\"",
            "",
            GraphDetail::Normal,
        ),
        (r#""title":"""#, "", GraphDetail::Normal),
        ("", r#""id":"""#, GraphDetail::Normal),
        (
            &format!(r#""title":"{}""#, "word ".repeat(20)),
            "",
            GraphDetail::Full,
        ),
        (
            r#""title":"Supercalifragilisticexpialidocious-and-more-words""#,
            "",
            GraphDetail::Normal,
        ),
        (
            r#""title":"x""#,
            r#""kind":"mission","state":"running","progress":{"done":2,"total":5},"detail":"v3""#,
            GraphDetail::Normal,
        ),
        (
            r#""title":"x""#,
            r#""kind":"mission","state":"running","progress":{"total":5},"detail":"v3""#,
            GraphDetail::Normal,
        ),
        (
            r#""title":"x""#,
            r#""kind":"job","state":"running","detail":"score 1e-3 (best)""#,
            GraphDetail::Normal,
        ),
        (
            r#""title":"x""#,
            r#""kind":"job","state":"","detail":"""#,
            GraphDetail::Normal,
        ),
        (r#""title":"Box ┌─┐ chars │ └─┘""#, "", GraphDetail::Normal),
        (
            r#""title":"AR""#,
            r#""kind":"autoresearch","state":"running","detail":"round 1/3 · baseline 0.5","children":[]"#,
            GraphDetail::Normal,
        ),
    ];
    for (title, extra, detail) in cases {
        // `{kind:'node', id:'n_1', title, state:'ready', detail:'implementation', ...extra}`.
        let mut node = json(&format!(
            r#"{{"kind":"node","id":"n_1",{title}{}"state":"ready","detail":"implementation"}}"#,
            if title.is_empty() { "" } else { "," }
        ));
        if let (JsValue::Object(object), JsValue::Object(extra)) =
            (&mut node, json(&format!("{{{extra}}}")))
        {
            object.spread(&extra);
        }
        let layout = layout_graph(Some(&node), detail);
        out.push_str(&format!("{}\n", layout.lines.join("\n")));
        let boxes = layout.boxes.iter().map(|b| {
            JsValue::from(vec![
                number(b.index),
                number(b.top),
                number(b.left),
                number(b.bottom),
                number(b.right),
                JsValue::from(b.tone.as_str()),
            ])
        });
        show(&mut out, "boxes", boxes.collect());
    }
    let direction = json(
        r#"{"kind":"direction","id":"d","title":"D","state":"blocked","version":2,"detail":"v2","children":[
        {"kind":"autoresearch","id":"a1","title":"Nested","state":"running","detail":"round 1/3 · baseline 0.5","children":[{"kind":"experiment","id":"e1","title":"E","state":"running","detail":""},{"kind":"job","id":"j","title":"J","state":"running","detail":""}]},
        {"kind":"autoresearch","id":"a2","title":"Nested no exp","state":"running","detail":"","children":[{"kind":"job","id":"j2","title":"J2","state":"running","detail":""}]},
        {"kind":"node","id":"n","title":"N","state":"blocked","detail":"","children":[]}]}"#,
    );
    let layout = layout_graph(Some(&direction), GraphDetail::Normal);
    out.push_str(&format!("{}\n", layout.lines.join("\n")));
    let boxes = layout.boxes.iter().map(|b| {
        let owner = b.owner.as_ref().and_then(|owner| owner.get("id")).cloned();
        JsValue::from(vec![
            number(b.index),
            JsValue::from(name(b)),
            nullable(b.parent),
            JsValue::from(b.opens),
            owner.unwrap_or(JsValue::Null),
        ])
    });
    show(&mut out, "boxes", boxes.collect());
    for b in &layout.boxes {
        out.push_str(&format!(
            "cmd {} => {}\n",
            name(b),
            json_stringify(&JsValue::from(suggested_command(b)))
        ));
    }
    let research = json(
        r#"{"kind":"autoresearch","id":"a1","title":"Root AR","state":"running","detail":"round 1/3 · baseline 0.123456789","children":[
        {"kind":"job","id":"j","title":"J","state":"running","detail":""},
        {"kind":"experiment","id":"e1","title":"E","state":"promoted","detail":"score 0.5","children":[{"kind":"experiment","id":"e2","title":"E2","state":"rejected","detail":""}]}]}"#,
    );
    let layout = layout_graph(Some(&research), GraphDetail::Normal);
    out.push_str(&format!("{}\n", layout.lines.join("\n")));
    let boxes = layout.boxes.iter().map(|b| {
        JsValue::from(vec![
            number(b.index),
            JsValue::from(name(b)),
            nullable(b.parent),
            JsValue::from(b.opens),
        ])
    });
    show(&mut out, "boxes", boxes.collect());
    assert_eq!(out, include_str!("goldens/map-edge-cases.txt"));
}

/// §6.4: the fixture's layout shrunk by 2, 4 and 8, and the factor-8 owners.
#[test]
fn shrinks_like_the_typescript_code() {
    let layout = layout_graph(Some(&root()), GraphDetail::Normal);
    let goldens = [
        (2, include_str!("goldens/map-shrink-2.txt")),
        (4, include_str!("goldens/map-shrink-4.txt")),
        (8, include_str!("goldens/map-shrink-8.txt")),
    ];
    for (factor, picture) in goldens {
        assert_eq!(shrink_graph(&layout, factor).lines.join("\n"), picture);
    }
    let none = [-1; 8];
    let row = |head: &[i32], tail: &[i32]| [head, tail].concat();
    assert_eq!(
        shrink_graph(&layout, 8).owners,
        [
            row(&[0, 0, 0, 0, 0], &none),
            row(&[1, 1, 1, 1, 5, 5, 5, 5, 6, 6, 6, 7, 7], &[]),
            row(&[2, 2, 2, 2, 2, 5, 5, 5, 6, 6, 6, 7, 7], &[]),
            row(&[2, 2, 2, 3, 2], &none),
            row(&[3, 3, 3, 3, 3], &none),
            row(&[4, 4, 4, 4, 4], &none),
        ]
    );
}
