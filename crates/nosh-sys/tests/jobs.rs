//! Job supervisor tests: the lifecycle in Rust, then compatibility with the TypeScript
//! `JobSupervisor` (`scripts/ts-call.mjs`, which needs a built TypeScript tree). Either daemon must
//! recover, cancel, and reconcile Jobs the other started, through the shared `job.json`,
//! `exit.json`, and process fingerprint.

use nosh_sys::jobs::{JobRecord, JobSpec, JobState, JobSupervisor, Runner, Stream};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn supervisor(data: &Path) -> JobSupervisor {
    JobSupervisor::new(
        data,
        vec![env!("CARGO_BIN_EXE_nosh-job-wrapper").into()],
        |_| (),
    )
    .unwrap()
}

fn spec(id: &str, cwd: &Path, command: &[&str]) -> JobSpec {
    JobSpec {
        job_id: id.into(),
        project_id: "prj_1".into(),
        mission_id: None,
        direction_id: None,
        autoresearch_id: None,
        run_id: "run_1".into(),
        experiment_id: None,
        commit_sha: "a".repeat(40),
        working_directory: cwd.to_string_lossy().into_owned(),
        runner: Runner::Native,
        distribution: None,
        command: command.iter().map(|s| (*s).to_owned()).collect(),
        checkpoint_command: None,
        environment_lock_hash: "sha256:lock".into(),
        evaluation_contract_hash: "sha256:contract".into(),
        timeout_seconds: 0.0,
        uses_gpu: false,
    }
}

fn wait_terminal(supervisor: &JobSupervisor, id: &str) -> JobRecord {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let record = supervisor.get(id).unwrap();
        if record.state.is_terminal() {
            return record;
        }
        assert!(Instant::now() < deadline, "{id} still {:?}", record.state);
        std::thread::sleep(Duration::from_millis(50));
    }
}

const LONG: &[&str] = &["node", "-e", "setTimeout(() => {}, 60000)"];

#[test]
fn runs_jobs_to_completion_failure_and_spawn_errors() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = supervisor(dir.path());
    let ok = jobs
        .start(&spec(
            "job_ok",
            dir.path(),
            &["node", "-e", "console.log('out'); console.error('err')"],
        ))
        .unwrap();
    assert_eq!(ok.state, JobState::Running);
    // A Job this short can exit before its fingerprint is read; TypeScript then records null too.
    assert!(ok.windows_pid.is_some());
    let ok = wait_terminal(&jobs, "job_ok");
    assert_eq!(
        (ok.state, ok.exit_code, ok.failure_reason.as_deref()),
        (JobState::Completed, Some(0), None)
    );
    assert_eq!(
        jobs.tail("job_ok", Stream::Stdout, 1024).unwrap().trim(),
        "out"
    );
    assert_eq!(jobs.tail("job_ok", Stream::Stderr, 2).unwrap(), "r\n");
    assert_eq!(
        jobs.tail("job_ok", Stream::Stdout, 2 * 1024 * 1024)
            .unwrap_err()
            .to_string(),
        "Tail size must be between 0 and 1048576 bytes"
    );

    jobs.start(&spec(
        "job_three",
        dir.path(),
        &["node", "-e", "process.exit(3)"],
    ))
    .unwrap();
    let three = wait_terminal(&jobs, "job_three");
    assert_eq!(
        (
            three.state,
            three.exit_code,
            three.failure_reason.as_deref()
        ),
        (JobState::Failed, Some(3), Some("exit_3"))
    );

    jobs.start(&spec(
        "job_missing",
        dir.path(),
        &["nosh-definitely-missing-program"],
    ))
    .unwrap();
    let missing = wait_terminal(&jobs, "job_missing");
    assert_eq!(
        (missing.state, missing.failure_reason.as_deref()),
        (
            JobState::Failed,
            Some("spawn nosh-definitely-missing-program ENOENT")
        )
    );

    let ids: Vec<String> = jobs
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.spec.job_id)
        .collect();
    assert_eq!(ids.len(), 3);
}

#[test]
fn validates_specs_and_starts_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = supervisor(dir.path());
    let error = |s: JobSpec| jobs.start(&s).unwrap_err().to_string();
    assert_eq!(
        error(spec("JOB_1", dir.path(), &["node"])),
        "Invalid Job ID"
    );
    assert_eq!(
        error(spec("job_1", dir.path(), &[])),
        "Job command must be a non-empty argument array"
    );
    assert_eq!(
        error(spec("job_1", &dir.path().join("missing"), &["node"])),
        "Job working directory does not exist"
    );
    assert_eq!(
        error(JobSpec {
            runner: Runner::Wsl2,
            ..spec("job_1", dir.path(), &["node"])
        }),
        "WSL2 jobs require a distribution"
    );

    let first = jobs.start(&spec("job_twice", dir.path(), LONG)).unwrap();
    assert_eq!(
        jobs.start(&spec("job_twice", dir.path(), LONG)).unwrap(),
        first
    );
    assert_eq!(
        error(spec("job_twice", dir.path(), &["node", "-v"])),
        "Job job_twice already exists with a different specification"
    );
    jobs.cancel("job_twice", "cancelled_by_user").unwrap();
}

#[test]
fn cancels_times_out_and_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = supervisor(dir.path());
    let running = jobs.start(&spec("job_cancel", dir.path(), LONG)).unwrap();
    let cancelled = jobs.cancel("job_cancel", "cancelled_by_user").unwrap();
    assert_eq!(
        (cancelled.state, cancelled.failure_reason.as_deref()),
        (JobState::Cancelled, Some("cancelled_by_user"))
    );
    assert!(
        !nosh_sys::process::alive(running.windows_pid, running.process_fingerprint.as_deref()),
        "cancel kills the process"
    );
    assert_eq!(
        jobs.cancel("job_cancel", "again")
            .unwrap()
            .failure_reason
            .as_deref(),
        Some("cancelled_by_user"),
        "terminal Jobs stay as they are"
    );

    let timed = jobs
        .start(&JobSpec {
            timeout_seconds: 1.0,
            ..spec("job_timeout", dir.path(), LONG)
        })
        .unwrap();
    let timed_out = wait_terminal(&jobs, "job_timeout");
    assert_eq!(
        (timed_out.state, timed_out.failure_reason.as_deref()),
        (JobState::Cancelled, Some("timeout"))
    );
    assert!(!nosh_sys::process::alive(
        timed.windows_pid,
        timed.process_fingerprint.as_deref()
    ));

    jobs.start(&spec("job_nockpt", dir.path(), LONG)).unwrap();
    assert_eq!(
        jobs.checkpoint("job_nockpt").unwrap_err().to_string(),
        "Running job does not declare a checkpoint command"
    );
    jobs.cancel("job_nockpt", "cancelled_by_user").unwrap();

    jobs.start(&JobSpec {
        checkpoint_command: Some(vec!["cmd".into(), "/c".into(), "exit 0".into()]),
        ..spec("job_ckpt", dir.path(), LONG)
    })
    .unwrap();
    assert_eq!(
        jobs.checkpoint("job_ckpt").unwrap().state,
        JobState::Running
    );
    jobs.cancel("job_ckpt", "cancelled_by_user").unwrap();

    jobs.start(&JobSpec {
        checkpoint_command: Some(vec!["cmd".into(), "/c".into(), "exit 1".into()]),
        ..spec("job_badckpt", dir.path(), LONG)
    })
    .unwrap();
    let failed = jobs.checkpoint("job_badckpt").unwrap();
    assert_eq!(
        (failed.state, failed.failure_reason.as_deref()),
        (JobState::Failed, Some("checkpoint_failed"))
    );
}

#[test]
fn keeps_fields_a_newer_nosh_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = supervisor(dir.path());
    jobs.start(&spec("job_extra", dir.path(), LONG)).unwrap();
    let path = dir.path().join("jobs/job_extra/job.json");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    record["futureField"] = json!({ "kept": true });
    std::fs::write(&path, record.to_string()).unwrap();
    jobs.cancel("job_extra", "cancelled_by_user").unwrap();
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["futureField"], json!({ "kept": true }));
    assert_eq!(saved["state"], "cancelled");
    let snapshot = jobs.resource_snapshot("job_extra").unwrap();
    assert!(
        snapshot.resident_bytes.is_none(),
        "finished Jobs report no memory"
    );
}

// ---- Compatibility with the TypeScript JobSupervisor ----

fn rust_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Calls methods of a TypeScript `JobSupervisor` on `data` in one Node process.
fn typescript(data: &Path, calls: Value) -> Vec<Value> {
    let request = json!({ "module": "packages/jobs/dist/index.js", "construct": { "name": "JobSupervisor", "args": [data] }, "calls": calls });
    let mut child = Command::new("node")
        .arg(rust_root().join("scripts/ts-call.mjs"))
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
    let results: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    for result in &results {
        assert!(
            result.get("ok").is_some(),
            "TypeScript call failed: {result}"
        );
    }
    results.into_iter().map(|r| r["ok"].clone()).collect()
}

const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];

/// A record's canonical JSON with the fields that legitimately differ between runs (time,
/// process, paths) masked.
fn comparable(record: &Value) -> String {
    let mut record = record.clone();
    for key in [
        "startedAt",
        "finishedAt",
        "windowsPid",
        "processFingerprint",
        "stdoutPath",
        "stderrPath",
    ] {
        if !record[key].is_null() {
            record[key] = json!("<masked>");
        }
    }
    nosh_wire::canonical_json(&record)
}

#[test]
fn records_match_the_typescript_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let (ts_data, rust_data) = (dir.path().join("ts"), dir.path().join("rust"));
    let cases: [(&str, &[&str]); 3] = [
        ("job_zero", &["node", "-e", "process.exit(0)"]),
        ("job_seven", &["node", "-e", "process.exit(7)"]),
        ("job_absent", &["nosh-definitely-missing-program", "--flag"]),
    ];
    let calls: Vec<Value> = cases
        .iter()
        .flat_map(|(id, command)| [json!({ "method": "start", "args": [spec(id, dir.path(), command)] }), json!({ "method": "get", "args": [id], "waitUntil": { "path": ["state"], "in": TERMINAL } })])
        .collect();
    let theirs = typescript(&ts_data, Value::Array(calls));
    let jobs = supervisor(&rust_data);
    for (index, (id, command)) in cases.iter().enumerate() {
        jobs.start(&spec(id, dir.path(), command)).unwrap();
        let ours = serde_json::to_value(wait_terminal(&jobs, id)).unwrap();
        assert_eq!(
            comparable(&ours),
            comparable(&theirs[index * 2 + 1]),
            "{id}"
        );
        let ts_log = ts_data.join("jobs").join(id).join("stdout.log");
        assert_eq!(
            ours["stdoutPath"],
            json!(rust_data.join("jobs").join(id).join("stdout.log"))
        );
        assert_eq!(theirs[index * 2 + 1]["stdoutPath"], json!(ts_log));
    }
}

#[test]
fn rust_recovers_and_cancels_a_typescript_job() {
    let dir = tempfile::tempdir().unwrap();
    let started = typescript(
        dir.path(),
        json!([{ "method": "start", "args": [spec("job_fromts", dir.path(), LONG)] }]),
    );
    assert_eq!(started[0]["state"], "running");
    // The TypeScript daemon is gone; its detached Job keeps running.
    let jobs = supervisor(dir.path());
    let recovered = jobs.recover().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(
        (recovered[0].state, recovered[0].recovered_after_restart),
        (JobState::Running, true),
        "the TypeScript fingerprint matches Rust's"
    );
    let cancelled = jobs.cancel("job_fromts", "cancelled_by_user").unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert!(!nosh_sys::process::alive(
        cancelled.windows_pid,
        cancelled.process_fingerprint.as_deref()
    ));
    let seen = typescript(
        dir.path(),
        json!([{ "method": "get", "args": ["job_fromts"] }]),
    );
    assert_eq!(
        (seen[0]["state"].as_str(), seen[0]["failureReason"].as_str()),
        (Some("cancelled"), Some("cancelled_by_user"))
    );
}

#[test]
fn typescript_recovers_a_rust_job_and_reads_its_exit() {
    let dir = tempfile::tempdir().unwrap();
    {
        let jobs = supervisor(dir.path());
        jobs.start(&spec(
            "job_fromrust",
            dir.path(),
            &["node", "-e", "setTimeout(() => process.exit(5), 2500)"],
        ))
        .unwrap();
        // The Rust daemon stops while the Job runs.
    }
    let theirs = typescript(
        dir.path(),
        json!([{ "method": "recover" }, { "method": "get", "args": ["job_fromrust"], "waitUntil": { "path": ["state"], "in": TERMINAL } }]),
    );
    assert_eq!(
        (
            theirs[0][0]["state"].as_str(),
            theirs[0][0]["recoveredAfterRestart"].as_bool()
        ),
        (Some("running"), Some(true))
    );
    assert_eq!(
        (
            theirs[1]["state"].as_str(),
            theirs[1]["exitCode"].as_i64(),
            theirs[1]["failureReason"].as_str()
        ),
        (Some("failed"), Some(5), Some("exit_5")),
        "TypeScript reads the Rust wrapper's exit.json"
    );
}

#[test]
fn rust_reconciles_a_typescript_job_that_ended_while_no_daemon_ran() {
    let dir = tempfile::tempdir().unwrap();
    typescript(
        dir.path(),
        json!([{ "method": "start", "args": [spec("job_ended", dir.path(), &["node", "-e", "setTimeout(() => process.exit(9), 500)"])] }]),
    );
    std::thread::sleep(Duration::from_secs(3));
    let recovered = supervisor(dir.path()).recover().unwrap();
    assert_eq!(
        (
            recovered[0].state,
            recovered[0].exit_code,
            recovered[0].failure_reason.as_deref()
        ),
        (JobState::Failed, Some(9), Some("exit_9")),
        "Rust reads the TypeScript wrapper's exit.json"
    );
}
