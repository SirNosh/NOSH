//! The CLI tests of `apps/cli/src` ported (`model-command`, `stop-update` and `backup-doctor`),
//! then routing, `logs` and `config.json` checks. Each run gets its own state root, and configs name
//! port 1, which is never a NOSH daemon.

use nosh_store::EventStore;
use nosh_store::backup::{
    FileEntry, RegisteredProject, copy_project_jobs, create_project_backup, hash_files,
    verify_backup,
};
use nosh_store::restore::{RestoreOptions, restore_backup};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod common;

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

/// `nosh <args>` with `state` as its whole environment of state: LOCALAPPDATA and APPDATA, and the
/// home Pi would use.
fn command(state: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .args(args)
        .env("LOCALAPPDATA", state)
        .env("APPDATA", state)
        .env("USERPROFILE", state)
        .env("HOME", state)
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> Run {
    let output = common::output(command).unwrap_or_else(|error| panic!("{error}"));
    Run {
        code: output.status.code(),
        out: String::from_utf8(output.stdout).unwrap(),
        err: String::from_utf8(output.stderr).unwrap(),
    }
}

fn nosh(state: &Path, args: &[&str]) -> Run {
    run(&mut command(state, args))
}

/// Writes `<state>/NOSH/config.json` for port 1 and returns the NOSH state root.
fn configured(state: &Path) -> PathBuf {
    let root = state.join("NOSH");
    std::fs::create_dir_all(&root).unwrap();
    let config = json!({ "dataDirectory": root.join("data"), "port": 1, "bootstrapToken": "test", "currentProjectId": null });
    std::fs::write(root.join("config.json"), config.to_string()).unwrap();
    root
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn git(project: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const HELP: &str = "nosh setup [--model=provider/id:level] [--yes]\nnosh login [--list]   connect a ChatGPT/Claude subscription or an API key\nnosh logout <provider>\nnosh start|stop [--if-idle]|status|open|tui|doctor\nnosh update\nnosh model [show|list|set <provider/id[:level]> [--restart]]\nnosh project list|open <path>\nnosh mission list|status <id>\nnosh job list\nnosh backup <project>\nnosh backup restore <project> <backup-id>\nnosh logs\n";

// ---- model-command.test.ts ----

#[test]
fn shows_validates_and_saves_the_default_model_without_a_running_daemon() {
    let state = tempfile::tempdir().unwrap();
    assert!(
        nosh(state.path(), &["model"])
            .err
            .contains("not initialized")
    );
    // Port 1 is never a NOSH daemon, so validation against Pi's model list is skipped.
    let root = configured(state.path());
    assert!(nosh(state.path(), &["model"]).out.contains("none"));
    assert!(
        nosh(state.path(), &["model", "set", "nope"])
            .err
            .contains("provider/id")
    );
    assert!(
        nosh(
            state.path(),
            &["model", "set", "openai-codex/gpt-6-luna:low"]
        )
        .out
        .contains("Default model set to openai-codex/gpt-6-luna:low")
    );
    let config: Value =
        serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["defaultModel"], "openai-codex/gpt-6-luna:low");
    assert_eq!(
        nosh(state.path(), &["model"]).out.trim(),
        "openai-codex/gpt-6-luna:low"
    );
}

// ---- stop-update.test.ts ----

// The npm launcher relies on these exit codes: 0 stopped, 2 active work, 3 not running.
#[test]
fn reports_3_when_no_daemon_runs_before_and_after_setup_and_explains_how_to_update_this_copy() {
    let state = tempfile::tempdir().unwrap();
    assert_eq!(nosh(state.path(), &["stop", "--if-idle"]).code, Some(3));
    configured(state.path());
    assert_eq!(nosh(state.path(), &["stop", "--if-idle"]).code, Some(3));
    assert_eq!(nosh(state.path(), &["stop"]).code, Some(0));
    let update = nosh(state.path(), &["update"]);
    assert_eq!(update.code, Some(0));
    assert!(update.out.contains("nosh-harness@latest"));
    // The installation root (this source tree), with a trailing separator, and a cargo checkout's
    // steps.
    let root = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .unwrap()
        .display()
        .to_string();
    let root = root.strip_prefix(r"\\?\").unwrap_or(&root);
    let lines: Vec<&str> = update.out.lines().collect();
    assert_eq!(
        lines,
        [
            format!("This NOSH runs from {root}{}.", std::path::MAIN_SEPARATOR).as_str(),
            "Source checkout: git pull && cargo build --release, then nosh stop and nosh start.",
            "npm install: npm install -g nosh-harness@latest, then nosh update.",
        ]
    );
}

// ---- backup-doctor.test.ts ----

#[test]
fn copies_only_selected_daemon_jobs_and_hashes_their_records_and_logs() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let destination = root.path().join("backup");
    for (job, project) in [("job_selected", "prj_selected"), ("job_other", "prj_other")] {
        let directory = data.join("jobs").join(job);
        std::fs::create_dir_all(&directory).unwrap();
        let record = json!({ "jobId": job, "projectId": project, "state": "completed" });
        std::fs::write(directory.join("job.json"), record.to_string()).unwrap();
        let name = job.trim_start_matches("job_");
        std::fs::write(directory.join("stdout.log"), format!("{name} stdout\n")).unwrap();
        std::fs::write(directory.join("stderr.log"), format!("{name} stderr\n")).unwrap();
    }

    copy_project_jobs(&data, &destination, "prj_selected").unwrap();

    let copied = destination.join("jobs").join("job_selected");
    for name in ["job.json", "stdout.log", "stderr.log"] {
        assert!(copied.join(name).exists());
    }
    assert!(!destination.join("jobs").join("job_other").exists());
    let hashes = hash_files(&destination).unwrap();
    for name in ["job.json", "stdout.log", "stderr.log"] {
        let bytes = std::fs::read(copied.join(name)).unwrap();
        assert!(hashes.contains(&FileEntry {
            path: format!("jobs/job_selected/{name}"),
            sha256: sha256(&bytes),
            bytes: bytes.len() as u64,
        }));
    }
}

#[test]
fn fails_loudly_for_malformed_job_data() {
    let root = tempfile::tempdir().unwrap();
    let job = root.path().join("data").join("jobs").join("job_invalid");
    std::fs::create_dir_all(&job).unwrap();
    std::fs::write(job.join("job.json"), "{").unwrap();
    let error = copy_project_jobs(
        &root.path().join("data"),
        &root.path().join("backup"),
        "prj_selected",
    )
    .unwrap_err();
    assert!(error.to_string().contains("malformed job.json"), "{error}");
}

/// One call of a stubbed `run`, as `[command, args]`.
fn call(command: &str, args: &[&str]) -> (String, Vec<String>) {
    (
        command.to_owned(),
        args.iter().map(|arg| (*arg).to_owned()).collect(),
    )
}

#[test]
fn checks_git_first_and_never_uses_the_caller_cwd_as_a_worktree_root() {
    let mut calls = Vec::new();
    let checks = nosh::cli::git_doctor_checks(None, &mut |command, args| {
        calls.push(call(command, args));
        nosh::cli::CommandResult {
            ok: true,
            stdout: "git version 2.50.0\n".into(),
        }
    });
    assert_eq!(
        checks[1],
        (
            "Git worktrees".to_owned(),
            true,
            "not checked; no current Project".to_owned()
        )
    );
    assert_eq!(calls, [call("git", &["--version"])]);

    let repository_root = std::env::temp_dir().join("registered-project-root");
    let repository_root = repository_root.to_str().unwrap();
    calls.clear();
    let checks = nosh::cli::git_doctor_checks(Some(repository_root), &mut |command, args| {
        calls.push(call(command, args));
        let stdout = if command == "git" && args[0] == "--version" {
            "git version 2.50.0\n".to_owned()
        } else {
            format!("{repository_root} abc\n")
        };
        nosh::cli::CommandResult { ok: true, stdout }
    });
    assert_eq!(
        calls,
        [
            call("git", &["--version"]),
            call("git", &["-C", repository_root, "worktree", "list"]),
        ]
    );
    assert_eq!(
        checks[1],
        ("Git worktrees".to_owned(), true, "1 available".to_owned())
    );
}

#[test]
fn verifies_content_addressed_artifacts_and_restores_project_data_state() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let data = root.path().join("data");
    let database = data.join("registered.sqlite");
    let objects = project.join(".nosh").join("artifacts").join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    git(&project, &["init", "-b", "main"]);
    git(&project, &["config", "user.email", "test@nosh.invalid"]);
    git(&project, &["config", "user.name", "NOSH test"]);
    std::fs::write(project.join(".gitignore"), ".nosh/\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-m", "fixture"]);
    let artifact = b"artifact payload";
    let digest = sha256(artifact);
    std::fs::write(objects.join(&digest), artifact).unwrap();
    let manifest = json!([{ "artifactId": "art_fixture", "version": 1, "storedPath": format!("objects/{digest}"), "contentHash": format!("sha256:{digest}"), "sizeBytes": artifact.len() }]);
    std::fs::write(
        project
            .join(".nosh")
            .join("artifacts")
            .join("manifest.json"),
        manifest.to_string(),
    )
    .unwrap();
    std::fs::write(project.join(".nosh").join("project.json"), "restored").unwrap();
    drop(EventStore::open(&database).unwrap());
    let job = |name: &str, record: Value| {
        let directory = data.join("jobs").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("job.json"), record.to_string()).unwrap();
    };
    job(
        "selected",
        json!({ "projectId": "prj", "state": "completed" }),
    );
    job("other", json!({ "projectId": "other" }));
    let registered = RegisteredProject {
        project_id: "prj".into(),
        repository_root: project.to_string_lossy().into_owned(),
        database_path: database.to_string_lossy().into_owned(),
        registered_at: "2026-01-01T00:00:00.000Z".into(),
    };
    let backup = PathBuf::from(create_project_backup(&data, &registered).unwrap().path);
    assert_eq!(verify_backup(&backup).unwrap().project.project_id, "prj");
    let copy = backup
        .join(".nosh")
        .join("artifacts")
        .join("objects")
        .join(&digest);
    std::fs::write(&copy, "tampered").unwrap();
    let error = verify_backup(&backup).unwrap_err();
    assert!(
        error.to_string().contains("integrity check failed"),
        "{error}"
    );
    std::fs::write(&copy, artifact).unwrap();
    let extra = objects.join("a".repeat(64));
    std::fs::write(&extra, "extra").unwrap();
    std::fs::write(project.join(".nosh").join("project.json"), "mutated").unwrap();
    job("old", json!({ "projectId": "prj" }));
    let restore = |daemon_stopped| {
        restore_backup(
            &project,
            &data,
            &backup,
            "prj",
            &database,
            daemon_stopped,
            RestoreOptions::default(),
        )
    };
    let error = restore(false).unwrap_err();
    assert!(
        error.to_string().contains("while noshd is running"),
        "{error}"
    );
    let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
    assert_eq!(read(project.join(".nosh").join("project.json")), "mutated");
    restore(true).unwrap();
    assert!(
        read(data.join("jobs").join("selected").join("job.json"))
            .contains(r#""state":"completed""#)
    );
    assert!(!data.join("jobs").join("old").exists());
    assert!(data.join("jobs").join("other").exists());
    assert!(!extra.exists());
    assert_eq!(read(project.join(".nosh").join("project.json")), "restored");
}

// ---- Without a TypeScript counterpart (cli.md §12.4) ----

#[test]
fn routes_words_and_flags_as_typescript_does() {
    let state = tempfile::tempdir().unwrap();
    for args in [&[][..], &["help"], &["status", "--help"]] {
        let help = nosh(state.path(), args);
        assert_eq!((help.code, help.out.as_str()), (Some(0), HELP), "{args:?}");
    }
    let logout = nosh(state.path(), &["logout"]);
    assert_eq!(logout.code, Some(1));
    assert_eq!(
        logout.err,
        "nosh: NOSH is not initialized; run `nosh setup`\n"
    );
    // Flags count at any position, whatever their value.
    assert_eq!(nosh(state.path(), &["--if-idle", "stop"]).code, Some(3));
    assert_eq!(nosh(state.path(), &["stop", "--if-idle=0"]).code, Some(3));

    configured(state.path());
    let logout = nosh(state.path(), &["logout"]);
    assert_eq!(logout.err, "nosh: Unknown command: logout; run nosh help\n");
    // `backup restore X` without a backup id backs up a Project named `restore`.
    let restore = nosh(state.path(), &["backup", "restore", "X"]);
    assert_eq!(
        (restore.code, restore.err.as_str()),
        (Some(1), "nosh: Project is not registered\n")
    );
    let open = nosh(state.path(), &["open"]);
    assert_eq!(
        open.err,
        "nosh: nosh tui requires an interactive terminal; use nosh help for scripting commands\n"
    );
    let login = nosh(state.path(), &["login"]);
    assert_eq!(
        login.err,
        "nosh: nosh login needs an interactive terminal\n"
    );
}

#[test]
fn logs_show_the_last_200_lines_split_at_line_breaks() {
    let state = tempfile::tempdir().unwrap();
    let root = configured(state.path());
    assert_eq!(nosh(state.path(), &["logs"]).out, "No daemon log\n");
    let logs = root.join("data").join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("noshd.log"), "a\r\n").unwrap();
    assert_eq!(nosh(state.path(), &["logs"]).out, "a\n\n");
    let text: String = (1..=250).map(|line| format!("line {line}\n")).collect();
    std::fs::write(logs.join("noshd.log"), text).unwrap();
    let expected: String = (52..=250).map(|line| format!("line {line}\n")).collect();
    assert_eq!(nosh(state.path(), &["logs"]).out, format!("{expected}\n"));
}

#[test]
fn config_rewrites_are_canonical_and_keep_unknown_keys() {
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("NOSH");
    std::fs::create_dir_all(&root).unwrap();
    let data = serde_json::to_string(&root.join("data")).unwrap();
    std::fs::write(
        root.join("config.json"),
        format!("\u{feff}{{\"port\":1,\"zeta\":{{\"b\":1,\"a\":[2]}},\"dataDirectory\":{data},\n\"bootstrapToken\":\"test\"}}"),
    )
    .unwrap();
    let set = nosh(state.path(), &["model", "set", "p/m"]);
    assert_eq!(set.out, "Default model set to p/m\n", "{}", set.err);
    assert_eq!(
        std::fs::read_to_string(root.join("config.json")).unwrap(),
        format!(
            "{{\"bootstrapToken\":\"test\",\"dataDirectory\":{data},\"defaultModel\":\"p/m\",\"port\":1,\"zeta\":{{\"a\":[2],\"b\":1}}}}\n"
        )
    );
}

#[test]
fn setup_never_rewrites_an_existing_config() {
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("NOSH");
    std::fs::create_dir_all(&root).unwrap();
    // Not canonical, so any rewrite shows.
    let config = "{ \"port\": 1, \"zeta\": true }";
    std::fs::write(root.join("config.json"), config).unwrap();
    // Without Node, setup stops at the prerequisites: after the config step, before Pi or a daemon.
    let setup = run(command(state.path(), &["setup", "--yes"])
        .env("NOSH_NODE", state.path().join("missing-node.exe")));
    assert_eq!(
        (setup.code, setup.err.as_str()),
        (
            Some(1),
            "nosh: fix the prerequisites above, then run `nosh setup` again\n"
        )
    );
    assert_eq!(
        std::fs::read_to_string(root.join("config.json")).unwrap(),
        config
    );
}

#[test]
fn daemon_replies_may_start_with_a_bom() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (status, body) in [
            ("200 OK", r#"{"jobs":[]}"#),
            ("500 Internal Server Error", r#"{"error":"boom"}"#),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            // The request head; `job list` sends no body.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let body = format!("\u{feff}{body}");
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("NOSH");
    std::fs::create_dir_all(&root).unwrap();
    let config = json!({ "dataDirectory": root.join("data"), "port": port, "bootstrapToken": "test", "currentProjectId": null });
    std::fs::write(root.join("config.json"), config.to_string()).unwrap();
    let jobs = nosh(state.path(), &["job", "list"]);
    assert_eq!(
        (jobs.code, jobs.out.as_str()),
        (Some(0), "No jobs\n"),
        "{}",
        jobs.err
    );
    let refused = nosh(state.path(), &["job", "list"]);
    assert_eq!(
        (refused.code, refused.err.as_str()),
        (Some(1), "nosh: boom\n")
    );
}
