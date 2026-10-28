//! nosh-agent's pure policy and text against the built TypeScript pi-adapter
//! (`scripts/pi-adapter-ts.mjs`): every op runs through both, and the results must be identical.

use nosh_agent::policy::{self, Role, SessionOptions};
use nosh_agent::prompt::{SessionFacts, host_state_guidance, nosh_system_prompt};
use nosh_agent::terminal::{self, TerminalContext, TerminalError};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }
}

const ROLES: [&str; 6] = [
    "nosh",
    "mission_director",
    "research_director",
    "librarian_researcher",
    "general_worker",
    "reviewer",
];
const OUTPUT: &str = "https://nosh.dev/schemas/terminal-output/v1";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn options(extra: Value) -> Value {
    let mut base = json!({
        "projectId": "prj_0123456789abcdef0123456789abcdef", "missionId": null, "directionId": null,
        "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "taskId": null,
        "agentId": "agt_0123456789abcdef0123456789abcdef", "role": "general_worker", "cwd": ".", "packagePath": "pi-package",
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn permissions(tools: &[&str], extra: Value) -> Value {
    let mut base = json!({ "network": "disabled", "subprocess": "disabled", "gitCommit": false, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": tools });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn workspace() -> Value {
    json!({ "worktreeId": "wt_1", "branch": "nosh/task", "startingCommit": COMMIT, "writeScopes": ["src/**", "./docs/", "notes.md"], "protectedScopes": ["src/secret/**"] })
}

fn option_variants() -> Vec<Value> {
    let task = json!("tsk_0123456789abcdef0123456789abcdef");
    let all = [
        "tool_pi.read",
        "tool_pi.edit",
        "tool_pi.write",
        "tool_nosh.run",
        "tool_nosh.network.read",
        "tool_nosh.git.commit",
        "tool_nosh.response.submit",
        "tool_nosh.review.submit",
        "tool_nosh.episode.submit",
        "tool_nosh.evidence.submit",
        "tool_nosh.delegation.request",
        "tool_nosh.unknown",
    ];
    vec![
        options(json!({})),
        options(json!({ "tools": ["read", "nosh_response_submit"] })),
        options(json!({ "role": "reviewer", "taskId": task })),
        options(json!({ "role": "general_worker", "taskId": task })),
        options(json!({ "role": "nosh", "runtimeScoped": true })),
        options(
            json!({ "taskId": task, "taskPermissions": permissions(&all, json!({ "network": "allowlisted", "networkAllowlist": ["arxiv.org"], "subprocess": "allowlisted", "gitCommit": true })), "taskWorkspace": workspace() }),
        ),
        options(
            json!({ "taskId": task, "taskPermissions": permissions(&all, json!({ "network": "allowlisted" })), "taskWorkspace": workspace() }),
        ),
        options(json!({ "taskId": task, "taskPermissions": permissions(&all, json!({})) })),
        options(
            json!({ "role": "librarian_researcher", "taskId": task, "taskPermissions": permissions(&["tool_pi.read", "tool_nosh.network.read", "tool_nosh.response.submit"], json!({ "network": "allowlisted", "networkAllowlist": ["example.org"] })), "taskWorkspace": workspace(), "tools": ["nosh_workspace_read"] }),
        ),
        options(
            json!({ "taskPermissions": permissions(&["tool_pi.read", "tool_nosh.run"], json!({ "subprocess": "allowlisted" })), "taskWorkspace": workspace() }),
        ),
    ]
}

fn context(task: bool, tools: &[&str], episode: Option<&str>) -> Value {
    let mut context = json!({ "projectId": "prj_1", "agentId": "agt_1", "taskId": if task { json!("tsk_1") } else { Value::Null }, "instructionId": null, "threadId": null, "expectedVersion": null, "turnId": "turn", "allowedTools": tools });
    if let Some(episode) = episode {
        context["expectedEpisodeType"] = json!(episode);
    }
    context
}

fn envelope(records: Value) -> String {
    serde_json::to_string(&json!({ "$schema": OUTPUT, "schemaVersion": 1, "records": records }))
        .unwrap()
}

fn terminal_cases() -> Vec<(String, Value)> {
    let failure =
        json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1 });
    let nested = json!({ "$schema": "https://nosh.dev/schemas/task-failure/v1", "schemaVersion": 1, "expectedEffect": { "primaryMetric": "lower" }, "guardrailRisks": ["a", "b"], "novelty": { "difference": "new" } });
    let episode = json!({ "$schema": "https://nosh.dev/schemas/episode-draft/v1", "episodeType": "episode_general-worker", "facts": ["é 😀"] });
    let task = context(true, &["nosh_response_submit"], None);
    let both = context(
        true,
        &["nosh_response_submit", "nosh_episode_submit"],
        Some("episode_general-worker"),
    );
    let runtime = context(
        false,
        &["nosh_episode_submit"],
        Some("episode_general-worker"),
    );
    let text = envelope(json!([failure]));
    let valid = envelope(json!([nested]));
    let slipped = valid.replace(r#""b"],"novelty""#, r#""b"]},"novelty""#);
    let mut cases: Vec<(String, Value)> = Vec::new();
    for invalid in [
        format!("```json\n{text}\n```"),
        format!("{text}{text}"),
        text.replace(
            r#""schemaVersion":1"#,
            r#""schemaVersion":1,"schemaVersion":1"#,
        ),
        text.replace("task-failure", "runtime-instruction"),
        format!("{}{text}", " ".repeat(131_072)),
        text.clone(),
    ] {
        cases.push((invalid, task.clone()));
    }
    for trailing in [
        "]}",
        "}]}\n",
        " ] } ",
        "</|im_end|>",
        "<|im_end|>\n",
        "]}<|endoftext|>",
        " done",
        "]} ok",
        "{}",
        "[1]",
        "<|im_end|> more text",
        "\u{a0}]",
        "\u{feff}",
    ] {
        cases.push((format!("{text}{trailing}"), task.clone()));
    }
    cases.push((format!("{text}{text}"), task.clone()));
    for repair in [
        slipped.clone(),
        valid.clone(),
        slipped.replace(r#""b"]},"novelty""#, r#""b"]}},"novelty""#),
        valid.replace(r#""a","b""#, r#""a" "b""#),
        valid.replace(
            r#"{"primaryMetric":"lower"}"#,
            r#"{"primaryMetric":"lower"}]"#,
        ),
        format!("  \n{slipped}\n"),
        r#"{"$schema":"position 3", x}"#.to_owned(),
        r#"{"a":["x","y"]},"b":1}"#.to_owned(),
    ] {
        cases.push((repair, task.clone()));
    }
    let records = [
        json!([failure, episode]),
        json!([episode, failure]),
        json!([episode]),
        json!([failure, failure]),
        json!([episode, episode]),
        json!([]),
        json!([failure, episode, failure]),
        json!([1]),
        json!([[]]),
        json!([{ "$schema": "https://nosh.dev/schemas/episode-draft/v1", "episodeType": "other" }]),
        json!([{ "$schema": "https://nosh.dev/schemas/episode-draft/v1" }]),
        json!([{ "$schema": "https://nosh.dev/schemas/review-verdict/v1" }]),
    ];
    for records in &records {
        for context in [&task, &both, &runtime] {
            cases.push((envelope(records.clone()), context.clone()));
        }
    }
    let no_episode_context = context(false, &["nosh_episode_submit"], None);
    cases.push((
        envelope(json!([{ "$schema": "https://nosh.dev/schemas/episode-draft/v1" }])),
        no_episode_context,
    ));
    for odd in [
        r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":"1","records":[{}]}"#,
        r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1.0,"records":[{"$schema":"https://nosh.dev/schemas/task-failure/v1"}]}"#,
        r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1,"records":[{"$schema":"https://nosh.dev/schemas/task-failure/v1"}],"extra":1}"#,
        r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1,"records":{}}"#,
        r#"[{"$schema":"https://nosh.dev/schemas/terminal-output/v1"}]"#,
        r#"{}"#,
        "   ",
        "\u{feff}{\"a\":1}",
        r#"{"a":{"b":1,"b":2}}"#,
        r#"{"a":[{"k":1},{"k":2}],"k":3}"#,
    ] {
        cases.push((odd.to_owned(), task.clone()));
    }
    // Model-style slips: stray, missing and doubled closers, cut tails, prose.
    let mut random = Random(0x5EED_1234_ABCD_0001);
    let bases = [
        valid.clone(),
        envelope(json!([failure, episode])),
        envelope(json!([nested, episode])),
    ];
    let pieces = [
        "}",
        "]",
        "]}",
        "}}",
        ",",
        "\"",
        "\n",
        " x",
        "<|im_end|>",
        "😀",
        "\\",
    ];
    for _ in 0..3_000 {
        let base = &bases[random.below(bases.len())];
        let mut units: Vec<u16> = base.encode_utf16().collect();
        for _ in 0..1 + random.below(2) {
            let at = random.below(units.len() + 1);
            match random.below(3) {
                0 if at < units.len() && units[at] < 0x80 => {
                    units.remove(at);
                }
                1 => {
                    let piece: Vec<u16> =
                        pieces[random.below(pieces.len())].encode_utf16().collect();
                    units.splice(at..at, piece);
                }
                _ => units.truncate(at.max(1)),
            }
        }
        let Ok(text) = String::from_utf16(&units) else {
            continue;
        };
        cases.push((text, both.clone()));
    }
    cases
}

fn progress_texts() -> Vec<&'static str> {
    vec![
        "PROGRESS: wrote the loader -> testing",
        "intro\n  PROGRESS:\tdone A -> B  \nmore\r\nPROGRESS: second\r\n\tPROGRESS: third\nPROGRESS: fourth",
        "PROGRESS:",
        "PROGRESS:   ",
        "PROGRESS: \u{a0}nbsp first",
        "PROGRESS: trailing nbsp\u{a0}",
        "xPROGRESS: not at start",
        "progress: lower case",
        "PROGRESS: a\u{2028}PROGRESS: b\u{2029}PROGRESS: c",
        "",
    ]
}

fn run_typescript(ops: &[Value]) -> Vec<Value> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pi-adapter-ts.mjs");
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(ops).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "the TypeScript side ran");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn role(value: &Value) -> Role {
    serde_json::from_value(value.clone()).unwrap()
}

fn rust(op: &Value) -> Value {
    if let Some(case) = op.get("terminal") {
        let text = case[0].as_str().unwrap();
        let context: TerminalContext = serde_json::from_value(case[1].clone()).unwrap();
        return match terminal::parse_terminal_output(text, &context) {
            Ok(output) => json!({ "ok": output.to_value() }),
            Err(error) => {
                json!({ "error": error.message(), "syntax": matches!(error, TerminalError::Syntax(_)), "excerpt": terminal::with_json_excerpt(&error, text) })
            }
        };
    }
    if let Some(message) = op.get("piError") {
        return json!({ "ok": policy::user_facing_pi_error(message.as_str().unwrap()) });
    }
    if let Some(name) = op.get("value") {
        return json!({ "ok": match name.as_str().unwrap() {
            "terminalSchemaTools" => Value::Object(terminal::TERMINAL_SCHEMA_TOOLS.iter().map(|(schema, tool)| (schema.clone(), json!(tool))).collect()),
            "terminalToolNames" => { let mut names: Vec<&str> = terminal::TERMINAL_SCHEMA_TOOLS.iter().map(|(_, tool)| *tool).collect(); names.sort_unstable(); names.dedup(); json!(names) }
            "NO_MODEL_ACCOUNT" => json!(policy::NO_MODEL_ACCOUNT),
            other => panic!("no value {other}"),
        } });
    }
    let args = &op["args"];
    let code_mode = op["env"]["NOSH_CODE_MODE"] != "off";
    let options = || -> SessionOptions { serde_json::from_value(args[0].clone()).unwrap() };
    let ok = match op["fn"].as_str().unwrap() {
        "noshSystemPrompt" => {
            let session = &args[1];
            let facts = session.is_object().then(|| SessionFacts {
                project_id: session["projectId"].as_str().unwrap(),
                agent_id: session["agentId"].as_str().unwrap(),
                task_id: session["taskId"].as_str(),
            });
            json!(nosh_system_prompt(
                role(&args[0]),
                facts.as_ref(),
                code_mode
            ))
        }
        "hostStateGuidance" => json!(host_state_guidance(role(&args[0]), code_mode)),
        "noshPromptCacheKey" => json!(policy::nosh_prompt_cache_key(&options())),
        "sessionToolIds" => json!(policy::session_tool_ids(&options())),
        "terminalPrompt" => json!(terminal::terminal_prompt(
            &serde_json::from_value(args[0].clone()).unwrap()
        )),
        "progressNotes" => json!(policy::progress_notes(args[0].as_str().unwrap())),
        "resolveWorkspacePath" => {
            let workspace: policy::TaskWorkspace =
                serde_json::from_value(args[0]["taskWorkspace"].clone()).unwrap();
            match policy::resolve_workspace_path(
                Path::new(args[0]["cwd"].as_str().unwrap()),
                Some(&workspace),
                args[1].as_str().unwrap(),
                args[2].as_bool().unwrap(),
            ) {
                Ok(path) => json!(path.display().to_string()),
                Err(error) => return json!({ "error": error, "syntax": false }),
            }
        }
        other => panic!("no fn {other}"),
    };
    json!({ "ok": ok })
}

#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J works without elevation");
}

fn workspace_ops(root: &Path) -> Vec<Value> {
    let real = root.join("real");
    let workspace_root = real.join("ws");
    std::fs::create_dir_all(workspace_root.join("src").join("secret")).unwrap();
    std::fs::create_dir_all(workspace_root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("outside")).unwrap();
    let mut roots = vec![workspace_root.clone()];
    #[cfg(windows)]
    {
        junction(&workspace_root.join("linked"), &root.join("outside"));
        junction(&root.join("alias"), &real);
        roots.push(root.join("alias").join("ws"));
    }
    let cwd = workspace_root.display().to_string();
    let mut paths: Vec<String> = [
        "src/a.rs",
        "src\\b.rs",
        "docs/x.md",
        "notes.md",
        "Notes.MD",
        "other.txt",
        "src/secret/k",
        "SRC/Secret/k",
        ".git",
        ".git/config",
        ".GIT/HEAD",
        "a/../src/c.rs",
        "../ws/src/d.rs",
        "..",
        ".",
        "",
        "src/../../x",
        "linked/file",
        "linked",
        "C:x",
        "C:src/f.rs",
        "c:",
        "\\x",
        "/x",
        "src/\0x",
        "./src/e.rs",
    ]
    .map(str::to_owned)
    .to_vec();
    paths.push(format!("{cwd}\\src\\abs.rs"));
    paths.push(format!("{}/src/fwd.rs", cwd.replace('\\', "/")));
    paths.push(format!("..\\{}\\src\\case.rs", "WS"));
    let mut ops = Vec::new();
    for root in &roots {
        for path in &paths {
            for writing in [false, true] {
                ops.push(json!({ "fn": "resolveWorkspacePath", "args": [{ "cwd": root.display().to_string(), "taskWorkspace": workspace() }, path, writing] }));
            }
        }
    }
    ops
}

#[test]
fn policy_and_text_match_typescript() {
    let temp = tempfile::tempdir().unwrap();
    let mut ops: Vec<Value> = Vec::new();
    for env in [
        json!({ "NOSH_CODE_MODE": null }),
        json!({ "NOSH_CODE_MODE": "off" }),
    ] {
        for role in ROLES {
            ops.push(json!({ "fn": "hostStateGuidance", "args": [role], "env": env }));
            for session in [
                Value::Null,
                json!({ "projectId": "prj_a", "agentId": "agt_b" }),
                json!({ "projectId": "prj_a", "agentId": "agt_b", "taskId": "tsk_c" }),
                json!({ "projectId": "prj_a", "agentId": "agt_b", "taskId": null }),
            ] {
                let args = if session.is_null() {
                    json!([role])
                } else {
                    json!([role, session])
                };
                ops.push(json!({ "fn": "noshSystemPrompt", "args": args, "env": env }));
            }
        }
    }
    for options in option_variants() {
        for role in ROLES {
            let mut options = options.clone();
            options["role"] = json!(role);
            ops.push(json!({ "fn": "sessionToolIds", "args": [options] }));
            ops.push(json!({ "fn": "noshPromptCacheKey", "args": [options] }));
        }
    }
    for context in [
        context(true, &["nosh_response_submit"], None),
        context(
            false,
            &["nosh_episode_submit", "nosh_review_submit"],
            Some("episode_reviewer"),
        ),
        context(true, &[], Some("")),
        context(
            true,
            &[
                "nosh_experiment_propose",
                "nosh_review_submit",
                "nosh_response_submit",
            ],
            None,
        ),
    ] {
        ops.push(json!({ "fn": "terminalPrompt", "args": [context] }));
    }
    for (text, context) in terminal_cases() {
        ops.push(json!({ "terminal": [text, context] }));
    }
    for text in progress_texts() {
        ops.push(json!({ "fn": "progressNotes", "args": [text] }));
    }
    for message in [
        "No API key found for anthropic. Run pi /login",
        "No model selected",
        "no models available",
        "User NOT AUTHENTICATED",
        "rate limited",
    ] {
        ops.push(json!({ "piError": message }));
    }
    for name in [
        "terminalSchemaTools",
        "terminalToolNames",
        "NO_MODEL_ACCOUNT",
    ] {
        ops.push(json!({ "value": name }));
    }
    ops.extend(workspace_ops(temp.path()));
    let expected = run_typescript(&ops);
    assert_eq!(expected.len(), ops.len());
    let mut mismatches = Vec::new();
    let mut per_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (op, expected) in ops.iter().zip(&expected) {
        let actual = rust(op);
        if &actual == expected {
            continue;
        }
        let kind = op.get("fn").map_or_else(
            || op.as_object().unwrap().keys().next().unwrap().clone(),
            ToString::to_string,
        );
        let seen = per_kind.entry(kind).or_default();
        *seen += 1;
        if *seen <= 3 {
            mismatches.push(format!(
                "{}\n  ts:   {}\n  rust: {}",
                nosh_wire::js::slice_to(&op.to_string(), 400),
                nosh_wire::js::slice_to(&expected.to_string(), 600),
                nosh_wire::js::slice_to(&actual.to_string(), 600)
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "ops differ by kind {per_kind:?} (of {}):\n{}",
        ops.len(),
        mismatches
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Deliberate divergence: TypeScript resolves `D:x` against drive D's current directory and
/// returns `D:\x` (a read outside the workspace); the port refuses it as an escape.
#[cfg(windows)]
#[test]
fn refuses_drive_relative_paths_on_another_drive() {
    let temp = tempfile::tempdir().unwrap();
    let workspace: policy::TaskWorkspace = serde_json::from_value(workspace()).unwrap();
    let drive = temp.path().to_string_lossy().chars().next().unwrap();
    let other = if drive.eq_ignore_ascii_case(&'D') {
        'E'
    } else {
        'D'
    };
    for writing in [false, true] {
        assert_eq!(
            policy::resolve_workspace_path(
                temp.path(),
                Some(&workspace),
                &format!("{other}:x"),
                writing
            ),
            Err("Workspace path escapes the assigned root".to_owned())
        );
    }
    assert_eq!(
        policy::resolve_workspace_path(
            temp.path(),
            Some(&workspace),
            &format!("{drive}:src/a.rs"),
            true
        ),
        Ok(temp.path().join("src").join("a.rs"))
    );
}
