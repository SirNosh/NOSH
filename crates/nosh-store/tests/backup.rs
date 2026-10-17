//! Backup tests: the TypeScript backup tests ported, then backups made by one implementation and
//! verified, listed, and restored by the other (`scripts/ts-call.mjs`, needs a built TypeScript tree).

use nosh_store::backup::{
    RegisteredProject, create_project_backup, list_project_backups, verify_backup,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROJECT: &str = "prj_backup_contract";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", message]);
}

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    project: RegisteredProject,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@nosh.invalid"]);
    git(&repo, &["config", "user.name", "NOSH test"]);
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let database = root.join("project.sqlite");
    drop(nosh_store::open_database(&database).unwrap());
    let project = RegisteredProject {
        project_id: PROJECT.into(),
        repository_root: repo.to_string_lossy().into(),
        database_path: database.to_string_lossy().into(),
        registered_at: "2026-01-01T00:00:00.000Z".into(),
    };
    Workspace {
        _dir: dir,
        root,
        repo,
        data,
        project,
    }
}

fn add_units(repo: &Path) {
    for unit in ["contracts", "events", "sessions"] {
        std::fs::create_dir_all(repo.join(".nosh").join(unit)).unwrap();
    }
    std::fs::create_dir_all(repo.join(".nosh/artifacts/aa")).unwrap();
    std::fs::create_dir_all(repo.join("docs/figures")).unwrap();
    std::fs::write(repo.join(".nosh/project.json"), "{\"marker\":\"backup\"}\n").unwrap();
    std::fs::write(repo.join(".nosh/schema-lock.json"), "{}\n").unwrap();
    let bytes = b"artifact bytes\n";
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(
        repo.join(".nosh/artifacts/aa").join(format!("{hash}.json")),
        bytes,
    )
    .unwrap();
    let manifest = json!([{ "artifactId": "art_fixture", "projectId": PROJECT, "kind": "result", "mediaType": "application/json", "contentHash": format!("sha256:{hash}"), "sizeBytes": bytes.len(), "version": 1, "sourcePath": "source.json", "storedPath": format!("aa/{hash}.json"), "retentionClass": "accepted_evidence", "createdAt": "2026-01-01T00:00:00.000Z" }]);
    std::fs::write(
        repo.join(".nosh/artifacts/manifest.json"),
        format!("{manifest}\n"),
    )
    .unwrap();
    std::fs::write(repo.join("docs/paper.md"), "# Backup paper\n").unwrap();
    std::fs::write(repo.join("docs/paper.bib"), "@article{backup}\n").unwrap();
    std::fs::write(repo.join("docs/figures/figure.txt"), "figure\n").unwrap();
}

fn add_job(data: &Path, name: &str, project_id: &str) {
    std::fs::create_dir_all(data.join("jobs").join(name)).unwrap();
    std::fs::write(
        data.join("jobs").join(name).join("job.json"),
        json!({ "projectId": project_id }).to_string(),
    )
    .unwrap();
}

#[test]
fn refuses_tracked_dirt_and_then_untracked_files() {
    let w = workspace();
    std::fs::write(w.repo.join("tracked.txt"), "clean\n").unwrap();
    commit_all(&w.repo, "fixture");
    std::fs::write(w.repo.join("tracked.txt"), "dirty\n").unwrap();
    let error = create_project_backup(&w.data, &w.project)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "Project repository must be exactly clean, including untracked files"
    );
    git(&w.repo, &["checkout", "--", "tracked.txt"]);
    std::fs::write(w.repo.join("untracked.txt"), "untracked\n").unwrap();
    assert!(
        create_project_backup(&w.data, &w.project)
            .unwrap_err()
            .to_string()
            .contains("clean")
    );
    assert!(
        !w.data.join("backups").exists(),
        "preflight runs before anything is created"
    );
}

#[test]
fn backs_up_units_artifacts_and_matching_jobs() {
    let w = workspace();
    add_units(&w.repo);
    commit_all(&w.repo, "fixture");
    add_job(&w.data, "job-match", PROJECT);
    add_job(&w.data, "job-other", "prj_other");
    let summary = create_project_backup(&w.data, &w.project).unwrap();
    let path = Path::new(&summary.path);
    assert!(verify_backup(path).unwrap().includes_large_artifacts);
    assert!(
        std::fs::read_to_string(path.join("docs/paper.md"))
            .unwrap()
            .contains("Backup paper")
    );
    assert!(path.join("jobs/job-match").exists() && !path.join("jobs/job-other").exists());
    assert!(
        summary.backup_id.starts_with("prj_backup_contract-") && !summary.backup_id.contains(':')
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !path.join(format!("nosh.sqlite{suffix}")).exists(),
            "no database sidecar in the backup"
        );
    }
    assert_eq!(
        list_project_backups(&w.data, PROJECT).unwrap(),
        vec![summary]
    );
    assert!(
        list_project_backups(&w.data, "prj_other")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn records_detached_and_unborn_heads_exactly() {
    let w = workspace();
    let unborn = create_project_backup(&w.data, &w.project).unwrap();
    let head: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(&unborn.path).join("git-head.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        head,
        json!({ "version": 1, "state": "unborn", "ref": "refs/heads/main", "commit": null })
    );
    assert!(!Path::new(&unborn.path).join("git.bundle").exists());

    let w = workspace();
    std::fs::write(w.repo.join("tracked.txt"), "one\n").unwrap();
    commit_all(&w.repo, "fixture");
    let commit = git(&w.repo, &["rev-parse", "HEAD"]);
    git(&w.repo, &["checkout", "-q", "--detach", &commit]);
    let detached = create_project_backup(&w.data, &w.project).unwrap();
    let head: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(&detached.path).join("git-head.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        head,
        json!({ "version": 1, "state": "detached", "ref": null, "commit": commit })
    );
}

#[test]
fn rejects_corrupted_backups() {
    let w = workspace();
    std::fs::write(w.repo.join("tracked.txt"), "one\n").unwrap();
    commit_all(&w.repo, "fixture");
    let summary = create_project_backup(&w.data, &w.project).unwrap();
    let path = Path::new(&summary.path);
    std::fs::write(path.join("required-commits.txt"), "").unwrap();
    assert!(
        verify_backup(path)
            .unwrap_err()
            .to_string()
            .starts_with("Backup integrity check failed")
    );
    std::fs::write(path.join("manifest.json"), "{}\n").unwrap();
    assert_eq!(
        verify_backup(path).unwrap_err().to_string(),
        "Unsupported or malformed backup manifest"
    );
    assert!(
        list_project_backups(&w.data, PROJECT).unwrap().is_empty(),
        "invalid backups are not listed"
    );
}

/// The hardened `git status` passes through only the user's *global* checkout settings, while Git
/// for Windows sets `core.autocrlf` at *system* level; a CRLF file committed in the same instant
/// then looks modified. Whatever the verdict on such a checkout, both implementations must agree.
#[test]
fn agrees_with_typescript_on_line_ending_sensitive_checkouts() {
    let w = workspace();
    std::fs::write(
        w.repo.join("windows.txt"),
        "line one
line two
",
    )
    .unwrap();
    commit_all(&w.repo, "crlf");
    let theirs = typescript(
        json!([{ "method": "createProjectBackup", "args": [w.data.join("ts"), w.project] }]),
    );
    let ours = create_project_backup(&w.data, &w.project).map_err(|e| e.to_string());
    match (&ours, theirs[0].get("error").and_then(Value::as_str)) {
        (Ok(_), None) => {}
        (Err(rust), Some(typescript)) => assert_eq!(rust, typescript),
        (rust, _) => panic!("verdicts differ: Rust {rust:?}, TypeScript {}", theirs[0]),
    }
}

// ---- Across implementations ----

fn typescript(calls: Value) -> Vec<Value> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let request = json!({ "module": "packages/persistence/dist/index.js", "calls": calls });
    let mut child = Command::new("node")
        .arg(repo_root.join("scripts/ts-call.mjs"))
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

fn populated() -> Workspace {
    let w = workspace();
    add_units(&w.repo);
    std::fs::write(w.repo.join("tracked.txt"), "before\n").unwrap();
    commit_all(&w.repo, "fixture");
    git(&w.repo, &["branch", "feature"]);
    add_job(&w.data, "job-match", PROJECT);
    add_job(&w.data, "job-other", "prj_other");
    w
}

#[test]
fn typescript_verifies_and_lists_a_rust_backup() {
    let w = populated();
    let ours = create_project_backup(&w.data, &w.project).unwrap();
    let manifest = verify_backup(Path::new(&ours.path)).unwrap();
    let theirs = typescript(
        json!([{ "method": "verifyBackup", "args": [ours.path] }, { "method": "listProjectBackups", "args": [w.data, PROJECT] }]),
    );
    assert_eq!(theirs[0]["ok"], serde_json::to_value(&manifest).unwrap());
    assert_eq!(theirs[1]["ok"], json!([ours]));
}

#[test]
fn rust_verifies_and_lists_a_typescript_backup() {
    let w = populated();
    let theirs =
        typescript(json!([{ "method": "createProjectBackup", "args": [w.data, w.project] }]));
    let summary = &theirs[0]["ok"];
    let path = PathBuf::from(summary["path"].as_str().unwrap());
    let manifest = verify_backup(&path).unwrap();
    assert_eq!(manifest.project.project_id, PROJECT);
    assert!(manifest.includes_large_artifacts);
    assert_eq!(
        serde_json::to_value(list_project_backups(&w.data, PROJECT).unwrap()).unwrap(),
        json!([summary])
    );
}

#[test]
fn both_reject_the_same_tampered_byte() {
    let w = populated();
    let ours = create_project_backup(&w.data, &w.project).unwrap();
    let path = Path::new(&ours.path);
    std::fs::write(path.join("docs/paper.md"), "# Tampered\n").unwrap();
    let expected = "Backup integrity check failed: docs/paper.md";
    assert_eq!(verify_backup(path).unwrap_err().to_string(), expected);
    let theirs = typescript(json!([{ "method": "verifyBackup", "args": [ours.path] }]));
    assert_eq!(theirs[0]["error"], expected);
    let _ = &w.root;
}
