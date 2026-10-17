//! Projects and papers: the TypeScript project and paper tests ported, then the same operations
//! compared with the TypeScript evidence package (`scripts/ts-call.mjs`, needs a built tree).

use nosh_research::paper::{initialize_paper_workspace, markdown_to_latex};
use nosh_research::project::{
    ProjectInitialization, amend_project_contract, approve_project_contract,
    commit_project_contract, detect_runnable_commands, initialize_research_project,
    read_project_contract,
};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn init(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    initialize_research_project(&ProjectInitialization {
        path: &repo,
        data_directory: &dir.join("data"),
        create_repository: true,
        working_title: None,
        github_repository_url: None,
    })
    .unwrap()
    .repository_root
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn with(contract: &Value, changes: Value) -> Value {
    let mut contract = contract.clone();
    for (key, value) in changes.as_object().unwrap() {
        contract[key] = value.clone();
    }
    contract
}

#[test]
fn rejects_contract_paths_outside_nosh_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path(), "repo");
    assert_eq!(read_project_contract(&repo).unwrap()["contractVersion"], 1);
    let path = repo.join(".nosh/project.json");
    let mut metadata: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    metadata["activeProjectContractPath"] = json!("../outside.json");
    std::fs::write(&path, metadata.to_string()).unwrap();
    assert!(
        read_project_contract(&repo)
            .unwrap_err()
            .to_string()
            .contains("activeProjectContractPath must name .nosh/contracts/project.v<N>.json")
    );
}

#[test]
fn approval_stamps_approved_at_and_amendment_needs_approval() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path(), "repo");
    let draft = read_project_contract(&repo).unwrap();
    let execution = json!({ "runner": "native", "commands": [{ "commandId": "command_test", "description": "Unit tests", "argv": ["node", "--test"], "timeoutSeconds": 120 }] });
    assert!(
        amend_project_contract(
            &repo,
            &with(
                &draft,
                json!({ "contractVersion": 2, "execution": execution })
            )
        )
        .unwrap_err()
        .to_string()
        .contains("Complete Project discovery")
    );
    let approved = approve_project_contract(
        &repo,
        &with(
            &draft,
            json!({ "contractVersion": 2, "workingTitle": "Approved", "approvedAt": null }),
        ),
    )
    .unwrap();
    assert!(approved["approvedAt"].is_string());
    assert_eq!(
        read_project_contract(&repo).unwrap()["workingTitle"],
        "Approved"
    );
    assert!(
        approve_project_contract(&repo, &with(&approved, json!({ "contractVersion": 3 })))
            .unwrap_err()
            .to_string()
            .contains("already complete")
    );
    assert!(
        amend_project_contract(
            &repo,
            &with(
                &approved,
                json!({ "contractVersion": 4, "execution": execution })
            )
        )
        .unwrap_err()
        .to_string()
        .contains("identity or version")
    );
    let amended = amend_project_contract(
        &repo,
        &with(
            &approved,
            json!({ "contractVersion": 3, "execution": execution }),
        ),
    )
    .unwrap();
    assert_eq!(
        read_project_contract(&repo).unwrap()["execution"],
        execution
    );
    assert_eq!(
        read_project_contract(&repo).unwrap()["approvedAt"],
        amended["approvedAt"]
    );
    let malformed = with(&draft, json!({ "contractVersion": 2 }));
    let mut malformed = malformed;
    malformed["northStar"]["contributionType"] = json!("contribution_method_measurement_fixture");
    let dir2 = tempfile::tempdir().unwrap();
    let repo2 = init(dir2.path(), "repo");
    malformed["projectId"] = read_project_contract(&repo2).unwrap()["projectId"].clone();
    malformed["createdAt"] = read_project_contract(&repo2).unwrap()["createdAt"].clone();
    assert!(
        approve_project_contract(&repo2, &malformed)
            .unwrap_err()
            .to_string()
            .contains("exactly one underscore")
    );
}

#[test]
fn commits_contract_scaffolding_but_not_other_staged_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path(), "repo");
    std::fs::write(repo.join("notes.txt"), "work in progress").unwrap();
    git(&repo, &["add", "notes.txt"]);
    approve_project_contract(
        &repo,
        &with(
            &read_project_contract(&repo).unwrap(),
            json!({ "contractVersion": 2 }),
        ),
    )
    .unwrap();
    assert_eq!(commit_project_contract(&repo).unwrap()["committed"], true);
    let mut files: Vec<String> = git(&repo, &["show", "--name-only", "--format=", "HEAD"])
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            ".nosh/contracts/project.v1.json",
            ".nosh/contracts/project.v2.json",
            ".nosh/project.json",
            ".nosh/schema-lock.json",
            "docs/paper.bib",
            "docs/paper.md"
        ]
    );
    assert_eq!(
        git(&repo, &["diff", "--cached", "--name-only"]),
        "notes.txt"
    );
    assert_eq!(
        commit_project_contract(&repo).unwrap(),
        json!({ "committed": false, "detail": "already committed" })
    );
    for (folder, file) in [
        ("artifacts/ab", "ab12.json"),
        ("mission", "summary.json"),
        ("pi", "session.jsonl"),
    ] {
        std::fs::create_dir_all(repo.join(".nosh").join(folder)).unwrap();
        std::fs::write(repo.join(".nosh").join(folder).join(file), "{}").unwrap();
    }
    assert_eq!(
        git(&repo, &["status", "--porcelain"]),
        "A  notes.txt",
        "runtime state never dirties the checkout"
    );
}

#[test]
fn accepts_a_repository_reached_through_a_short_path() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("Long Folder Name").join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .unwrap();
    let query = format!(
        "(New-Object -ComObject Scripting.FileSystemObject).GetFolder('{}').ShortPath",
        repo.display()
    );
    let short = PathBuf::from(
        String::from_utf8_lossy(
            &Command::new("powershell.exe")
                .args(["-NoProfile", "-Command", &query])
                .output()
                .unwrap()
                .stdout,
        )
        .trim(),
    );
    if short == repo {
        return; // 8.3 names are disabled on this volume.
    }
    let initialized = initialize_research_project(&ProjectInitialization {
        path: &short,
        data_directory: &dir.path().join("data"),
        create_repository: false,
        working_title: None,
        github_repository_url: None,
    })
    .unwrap();
    assert_eq!(
        initialized.repository_root,
        nosh_sys::paths::realpath_native(&repo).unwrap()
    );
}

#[test]
fn hides_file_content_from_parse_errors() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path(), "repo");
    std::fs::write(
        repo.join(".nosh/contracts/project.v1.json"),
        "{ secret-token-value",
    )
    .unwrap();
    let message = read_project_contract(&repo).unwrap_err().to_string();
    assert!(message.contains("file is not valid JSON") && !message.contains("secret-token-value"));
}

// ---- Across implementations ----

fn typescript(calls: Value) -> Vec<Value> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    typescript_with(calls, |command| {
        command.arg(root.join("scripts/ts-call.mjs"));
    })
}

fn typescript_with(calls: Value, setup: impl FnOnce(&mut Command)) -> Vec<Value> {
    let request = json!({ "module": "packages/evidence/dist/index.js", "calls": calls });
    let mut command = Command::new("node");
    setup(&mut command);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node is on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "ts-call.mjs failed (is the TypeScript tree built?):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn detects_the_same_runnable_commands_in_package_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Out of alphabetical order on purpose: JavaScript keeps file order.
    std::fs::write(root.join("package.json"), r#"{"scripts":{"test":"node --test test/a.test.mjs","evaluate":"node evaluate.mjs","test:unit":"vitest run && echo ok","bench":"npx bench","build":"node build.mjs","reproduce:Paper Fig":"python   repro.py  --all","eval":"  cargo  test ","benchmark":"deno run b.ts"}}"#).unwrap();
    std::fs::create_dir(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/test_model.py"),
        "def test_ok():\n    pass\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Makefile"),
        "eval:\n\tpython eval.py\nreproduce: all\n",
    )
    .unwrap();
    let theirs = typescript(
        json!([{ "method": "detectRunnableCommands", "args": [root] }, { "method": "detectRunnableCommands", "args": [root.join("missing")] }]),
    );
    assert_eq!(theirs[0]["ok"], json!(detect_runnable_commands(root)));
    assert_eq!(
        theirs[1]["ok"],
        json!(detect_runnable_commands(&root.join("missing")))
    );
}

fn masked(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if matches!(k.as_str(), "createdAt" | "updatedAt" | "approvedAt")
                            && v.is_string()
                        {
                            json!("<t>")
                        } else {
                            masked(v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(masked).collect()),
        other => other.clone(),
    }
}

#[test]
fn initializes_identical_projects_and_shares_contracts() {
    let dir = tempfile::tempdir().unwrap();
    // The same Project ID in both, through an existing project.json, so the files compare.
    for name in ["rust", "ts"] {
        let repo = dir.path().join(name);
        std::fs::create_dir_all(repo.join(".nosh")).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        std::fs::write(
            repo.join(".nosh/project.json"),
            r#"{"projectId":"prj_0123456789abcdef0123456789abcdef"}"#,
        )
        .unwrap();
    }
    let rust = initialize_research_project(&ProjectInitialization {
        path: &dir.path().join("rust"),
        data_directory: &dir.path().join("data"),
        create_repository: false,
        working_title: Some("  Shared  "),
        github_repository_url: Some("https://github.com/SirNosh/NOSH"),
    })
    .unwrap();
    let theirs = typescript(
        json!([{ "method": "initializeResearchProject", "args": [{ "path": dir.path().join("ts"), "dataDirectory": dir.path().join("data"), "createRepository": false, "workingTitle": "  Shared  ", "githubRepositoryUrl": "https://github.com/SirNosh/NOSH" }] }]),
    );
    let ts_root = PathBuf::from(theirs[0]["ok"]["repositoryRoot"].as_str().unwrap());
    assert_eq!(theirs[0]["ok"]["projectId"], rust.project_id);
    assert_eq!(theirs[0]["ok"]["databasePath"], json!(rust.database_path));
    for file in [
        ".nosh/project.json",
        ".nosh/contracts/project.v1.json",
        ".nosh/schema-lock.json",
    ] {
        let read = |root: &Path| -> Value {
            serde_json::from_str(&std::fs::read_to_string(root.join(file)).unwrap()).unwrap()
        };
        assert_eq!(
            masked(&read(&rust.repository_root)),
            masked(&read(&ts_root)),
            "{file}"
        );
    }
    for file in [".nosh/.gitignore", "docs/paper.md", "docs/paper.bib"] {
        assert_eq!(
            std::fs::read_to_string(rust.repository_root.join(file)).unwrap(),
            std::fs::read_to_string(ts_root.join(file)).unwrap(),
            "{file}"
        );
    }
    assert_eq!(
        git(&rust.repository_root, &["remote", "get-url", "origin"]),
        "https://github.com/SirNosh/NOSH"
    );

    // TypeScript approves and commits what Rust created; Rust reads and amends it.
    let draft = read_project_contract(&rust.repository_root).unwrap();
    let approved = typescript(json!([
        { "method": "readProjectContract", "args": [rust.repository_root] },
        { "method": "approveProjectContract", "args": [rust.repository_root, with(&draft, json!({ "contractVersion": 2 }))] },
        { "method": "commitProjectContract", "args": [rust.repository_root] },
    ]));
    assert_eq!(approved[0]["ok"], draft);
    assert_eq!(approved[2]["ok"]["committed"], true);
    let read_back = read_project_contract(&rust.repository_root).unwrap();
    assert_eq!(read_back, approved[1]["ok"]);
    let amended = amend_project_contract(
        &rust.repository_root,
        &with(&read_back, json!({ "contractVersion": 3 })),
    )
    .unwrap();
    let theirs = typescript(
        json!([{ "method": "readProjectContract", "args": [rust.repository_root] }, { "method": "commitProjectContract", "args": [rust.repository_root] }]),
    );
    assert_eq!(theirs[0]["ok"], amended);
    assert_eq!(theirs[1]["ok"]["committed"], true);
    assert_eq!(
        commit_project_contract(&rust.repository_root).unwrap(),
        json!({ "committed": false, "detail": "already committed" })
    );

    // Read errors read the same.
    std::fs::write(
        rust.repository_root.join(".nosh/contracts/project.v3.json"),
        "{ not json",
    )
    .unwrap();
    let theirs =
        typescript(json!([{ "method": "readProjectContract", "args": [rust.repository_root] }]));
    assert_eq!(
        theirs[0]["error"],
        json!(
            read_project_contract(&rust.repository_root)
                .unwrap_err()
                .to_string()
        )
    );
}

/// The LaTeX for a varied paper must match TypeScript's byte for byte (TypeScript runs without
/// latexmk on its PATH, so the comparison covers the conversion only).
#[test]
fn converts_markdown_to_the_same_latex() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    initialize_paper_workspace(&repo, "Paper").unwrap();
    std::fs::write(repo.join("docs/figures/plot.png"), [0x89, b'P', b'N', b'G']).unwrap();
    std::fs::write(repo.join("docs/paper.bib"), "@article{key, title={T}}\n").unwrap();
    let markdown = [
        "# A $100 & 50% Title_with {braces} \\ slash",
        "",
        "## Method",
        "### Sub section",
        "Plain **bold** and `code_0123456789abcdef0123456789abcdef01234567` with [a link](https://example.com/a_b) and [local](notes.md) and [@key] and #hash.",
        "- item **one**",
        "-not an item",
        "![A figure](figures/plot.png)",
        "```",
        "verbatim $ & # _ {}",
        "```",
        "| `threshold` | score | note / detail |",
        "| ---: | :---: | --- |",
        "| 0.6 | 0.87 | pass_case / ok |",
        "| no separator | here |",
        "| a | b |",
        "",
        "Unicode: é 😀 and an unclosed fence below",
        "```",
        "never closed",
    ]
    .join("\r\n");
    std::fs::write(repo.join("docs/paper.md"), &markdown).unwrap();
    let no_tex: String = std::env::var("PATH")
        .unwrap()
        .split(';')
        .filter(|entry| {
            !entry.to_lowercase().contains("miktex") && !entry.to_lowercase().contains("texlive")
        })
        .collect::<Vec<_>>()
        .join(";");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let theirs = typescript_with(
        json!([{ "method": "exportPaper", "args": [repo.join("docs/paper.md"), repo.join("docs/paper.bib"), repo.join("build/paper")] }]),
        |command| {
            command
                .arg(root.join("scripts/ts-call.mjs"))
                .env("PATH", &no_tex);
        },
    );
    let manifest = &theirs[0]["ok"];
    assert!(
        manifest.is_object(),
        "TypeScript export failed: {}",
        theirs[0]
    );
    let their_tex = std::fs::read_to_string(manifest["latexPath"].as_str().unwrap()).unwrap();
    let staging = repo.join("rust-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut warnings = Vec::new();
    let ours = markdown_to_latex(
        &markdown,
        &mut warnings,
        &repo.join("docs/paper.bib"),
        &repo.join("docs/paper.md"),
        &staging,
    )
    .unwrap();
    assert_eq!(ours, their_tex);
    let their_warnings: Vec<String> = manifest["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_owned())
        .filter(|w| !w.starts_with("PDF was not produced"))
        .collect();
    assert_eq!(warnings, their_warnings);
    assert!(
        staging.join("figures").read_dir().unwrap().count() == 1,
        "the figure is copied"
    );
}

/// Unsafe or unsupported figures refuse the export with the same message as TypeScript.
#[test]
fn refuses_the_same_figures() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    initialize_paper_workspace(&repo, "Paper").unwrap();
    let figures = repo.join("docs").join("figures");
    std::fs::write(repo.join("docs/outside.png"), "png").unwrap();
    std::fs::write(figures.join("notes.txt"), "text").unwrap();
    std::fs::create_dir(figures.join("folder.png")).unwrap();
    std::fs::create_dir(dir.path().join("elsewhere")).unwrap();
    std::fs::write(dir.path().join("elsewhere/plot.png"), "png").unwrap();
    let junction = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(figures.join("linked"))
        .arg(dir.path().join("elsewhere"))
        .output()
        .unwrap();
    assert!(junction.status.success());
    let references = [
        "figures/nope.png",
        "../outside.png",
        "figures/../../outside.png",
        "C:/Windows/win.ini",
        "/rooted.png",
        "https://example.com/plot.png",
        "figures/notes.txt",
        "figures/folder.png",
        "figures/linked/plot.png",
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for reference in references {
        let markdown = format!("# Paper\n\n![figure]({reference})\n");
        std::fs::write(repo.join("docs/paper.md"), &markdown).unwrap();
        let theirs = typescript_with(
            json!([{ "method": "exportPaper", "args": [repo.join("docs/paper.md"), repo.join("docs/paper.bib"), repo.join("build/paper")] }]),
            |command| {
                command.arg(root.join("scripts/ts-call.mjs"));
            },
        );
        let staging = repo.join("rust-staging");
        std::fs::create_dir_all(&staging).unwrap();
        let ours = markdown_to_latex(
            &markdown,
            &mut Vec::new(),
            &repo.join("docs/paper.bib"),
            &repo.join("docs/paper.md"),
            &staging,
        );
        assert!(
            theirs[0]["error"].is_string(),
            "{reference}: TypeScript accepted it"
        );
        assert_eq!(
            theirs[0]["error"],
            json!(ours.unwrap_err().to_string()),
            "{reference}"
        );
    }
}
